//! Shared client logic: talking to a server, the saved server list and
//! human-readable device labels. Used by both the CLI and the tray GUI.
//! Pure `std`, so it builds on every platform.

use std::fs;
use std::io::{self, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub mod attach;
pub mod autostart;
pub mod discovery;
pub mod driver;
pub mod view;
pub use discovery::{DiscoveredServer, Discovery};
pub use view::{
    build_view, merge_servers, poll_servers, summary, NodeRef, ServerEntry, ServerState, ServerStatus, ViewDevice, ViewServer,
};

use usbip_proto::{
    encode_devlist_request, encode_import_request, encode_names_request, read_devlist_reply, read_import_reply,
    read_names_reply, ExportedDevice, ProtoError, UsbDevice, DEFAULT_PORT,
};

// ---------------------------------------------------------------- addresses

/// Turns `host`, `host:port`, `1.2.3.4`, `::1` or `[::1]:3240` into something
/// `ToSocketAddrs` accepts, adding the default port when none was given.
pub fn normalize_addr(input: &str) -> String {
    let s = input.trim();
    if let Some(rest) = s.strip_prefix('[') {
        // Bracketed IPv6: port present only if "]:" follows.
        return if rest.contains("]:") { s.to_string() } else { format!("{s}:{DEFAULT_PORT}") };
    }
    match s.matches(':').count() {
        0 => format!("{s}:{DEFAULT_PORT}"),
        1 => s.to_string(),
        _ => format!("[{s}]:{DEFAULT_PORT}"), // bare IPv6 literal
    }
}

/// `true` if the string looks like something worth trying to connect to.
pub fn is_valid_addr(input: &str) -> bool {
    let s = input.trim();
    if s.is_empty() || s.chars().any(char::is_whitespace) {
        return false;
    }
    let n = normalize_addr(s);
    // The port is whatever follows the last colon.
    n.rsplit_once(':').is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok_and(|p| p != 0))
}

pub fn connect(addr: &str, timeout: Duration) -> io::Result<TcpStream> {
    let target = normalize_addr(addr);
    let mut last_err = io::Error::new(io::ErrorKind::NotFound, format!("could not resolve `{addr}`"));
    for sa in target.to_socket_addrs()? {
        match TcpStream::connect_timeout(&sa, timeout) {
            Ok(s) => {
                s.set_read_timeout(Some(timeout))?;
                s.set_write_timeout(Some(timeout))?;
                s.set_nodelay(true)?;
                return Ok(s);
            }
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

pub fn list_devices(addr: &str, timeout: Duration) -> Result<Vec<ExportedDevice>, ProtoError> {
    list_devices_on(&mut connect(addr, timeout)?)
}

/// The `DEVLIST` exchange on a connection the caller already opened.
pub fn list_devices_on(stream: &mut TcpStream) -> Result<Vec<ExportedDevice>, ProtoError> {
    stream.write_all(&encode_devlist_request())?;
    read_devlist_reply(stream)
}

/// Human-readable device names (busid -> name) from servers that run this project's server.
/// Standard USB/IP servers do not know the request and just hang up: that is "no names", not an error.
pub fn list_names(addr: &str, timeout: Duration) -> std::collections::HashMap<String, String> {
    let fetch = || -> Result<_, ProtoError> {
        let mut s = connect(addr, timeout)?;
        s.write_all(&encode_names_request())?;
        read_names_reply(&mut s)
    };
    fetch().map(|v| v.into_iter().collect()).unwrap_or_default()
}

pub fn import_device(addr: &str, busid: &str, timeout: Duration) -> Result<UsbDevice, ProtoError> {
    let mut s = connect(addr, timeout)?;
    s.write_all(&encode_import_request(busid))?;
    read_import_reply(&mut s)
}

// ---------------------------------------------------------------- saved servers

/// `%APPDATA%\usb-over-net` on Windows, `$XDG_CONFIG_HOME` or `~/.config` elsewhere.
pub fn config_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("usb-over-net")
}

pub fn servers_file() -> PathBuf {
    config_dir().join("servers.txt")
}

/// One server per line; blank lines and `#` comments are ignored. A missing file is an empty list.
pub fn load_servers(path: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(path) else { return Vec::new() };
    let mut out: Vec<String> = Vec::new();
    for line in text.lines().map(str::trim) {
        if !line.is_empty() && !line.starts_with('#') && !out.iter().any(|s| s == line) {
            out.push(line.to_string());
        }
    }
    out
}

pub fn save_servers(path: &Path, servers: &[String]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut text = String::from("# usb-over-net servers, one host[:port] per line\n");
    for s in servers {
        text.push_str(s);
        text.push('\n');
    }
    fs::write(path, text)
}

// ---------------------------------------------------------------- labels

/// Name for a USB class code (device or interface).
pub fn class_name(class: u8) -> &'static str {
    match class {
        0x01 => "Audio",
        0x02 => "Communications",
        0x03 => "HID",
        0x05 => "Physical",
        0x06 => "Imaging",
        0x07 => "Printer",
        0x08 => "Mass Storage",
        0x09 => "Hub",
        0x0a => "CDC Data",
        0x0b => "Smart Card",
        0x0d => "Content Security",
        0x0e => "Video",
        0x0f => "Healthcare",
        0x10 => "Audio/Video",
        0xdc => "Diagnostic",
        0xe0 => "Wireless",
        0xef => "Miscellaneous",
        0xfe => "Application Specific",
        0xff => "Vendor Specific",
        _ => "Device",
    }
}

/// What kind of device this is, e.g. `Mass Storage` or `HID + Audio`.
pub fn device_kind(d: &ExportedDevice) -> String {
    let dev = &d.device;
    let mut kinds: Vec<&'static str> = Vec::new();
    // 0x00 means "see the interfaces"; 0xEF (Miscellaneous) is what composite devices with
    // interface associations (webcams, headsets) report, which says nothing useful on its own.
    if dev.device_class != 0 && dev.device_class != 0xef {
        kinds.push(class_name(dev.device_class));
    } else {
        for i in &d.interfaces {
            let n = class_name(i.class);
            if !kinds.contains(&n) {
                kinds.push(n);
            }
        }
    }
    if kinds.is_empty() && dev.device_class == 0xef {
        class_name(0xef).to_string()
    } else if kinds.is_empty() {
        "Device".to_string()
    } else {
        kinds.join(" + ")
    }
}

/// `vvvv:pppp` in hex.
pub fn device_ids(d: &ExportedDevice) -> String {
    format!("{:04x}:{:04x}", d.device.id_vendor, d.device.id_product)
}

/// e.g. `Mass Storage  0951:1666  [1-1.2]`
pub fn device_label(d: &ExportedDevice) -> String {
    format!("{}  {}  [{}]", device_kind(d), device_ids(d), d.device.busid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use usbip_proto::{Speed, UsbInterface};

    #[test]
    fn normalizes_addresses() {
        assert_eq!(normalize_addr("pi"), "pi:3240");
        assert_eq!(normalize_addr(" 10.0.0.5 "), "10.0.0.5:3240");
        assert_eq!(normalize_addr("pi:4000"), "pi:4000");
        assert_eq!(normalize_addr("::1"), "[::1]:3240");
        assert_eq!(normalize_addr("[::1]"), "[::1]:3240");
        assert_eq!(normalize_addr("[::1]:99"), "[::1]:99");
    }

    #[test]
    fn validates_addresses() {
        assert!(is_valid_addr("raspberrypi.local"));
        assert!(is_valid_addr("10.0.0.5:3240"));
        assert!(is_valid_addr("[::1]:3240"));
        assert!(!is_valid_addr(""));
        assert!(!is_valid_addr("two words"));
        assert!(!is_valid_addr("host:0"));
        assert!(!is_valid_addr("host:99999"));
        assert!(!is_valid_addr("host:abc"));
    }

    #[test]
    fn server_file_roundtrip() {
        let dir = std::env::temp_dir().join(format!("uon-test-{}", std::process::id()));
        let path = dir.join("servers.txt");
        assert!(load_servers(&path).is_empty()); // missing file
        save_servers(&path, &["a:1".into(), "b".into()]).unwrap();
        fs::write(&path, format!("{}\n\n  a:1  \n", fs::read_to_string(&path).unwrap())).unwrap();
        assert_eq!(load_servers(&path), vec!["a:1".to_string(), "b".to_string()]); // trimmed + deduped
        let _ = fs::remove_dir_all(dir);
    }

    fn dev(class: u8, busid: &str, ifs: &[u8]) -> ExportedDevice {
        let device = UsbDevice {
            path: String::new(),
            busid: busid.into(),
            busnum: 1,
            devnum: 2,
            speed: Speed::High,
            id_vendor: 0x0951,
            id_product: 0x1666,
            bcd_device: 0,
            device_class: class,
            device_subclass: 0,
            device_protocol: 0,
            configuration_value: 1,
            num_configurations: 1,
            num_interfaces: 0,
        };
        ExportedDevice::new(
            device,
            ifs.iter().map(|&c| UsbInterface { class: c, subclass: 0, protocol: 0 }).collect(),
        )
    }

    #[test]
    fn labels() {
        assert_eq!(device_label(&dev(0, "1-1.2", &[8])), "Mass Storage  0951:1666  [1-1.2]");
        assert_eq!(device_label(&dev(0, "1-2", &[3, 3, 1])), "HID + Audio  0951:1666  [1-2]");
        assert_eq!(device_label(&dev(0xff, "1-3", &[])), "Vendor Specific  0951:1666  [1-3]");
        assert_eq!(device_label(&dev(0, "1-4", &[])), "Device  0951:1666  [1-4]");
        // A webcam: class 0xEF with video + audio interfaces reads as what it is, not "Miscellaneous".
        assert_eq!(device_label(&dev(0xef, "1-5", &[0x0e, 0x0e, 0x01, 0x01])), "Video + Audio  0951:1666  [1-5]");
        assert_eq!(device_label(&dev(0xef, "1-6", &[])), "Miscellaneous  0951:1666  [1-6]");
    }
}
