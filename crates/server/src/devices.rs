//! Enumerates local USB devices with libusb and maps them to USB/IP records.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use rusb::{Device, UsbContext};
use usbip_proto::{ExportedDevice, Speed, UsbDevice, UsbInterface};

/// A `VID:PID` pair in hex, e.g. `0403:6001`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VidPid(pub u16, pub u16);

impl FromStr for VidPid {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (v, p) = s
            .split_once(':')
            .ok_or_else(|| format!("expected VID:PID (hex), got `{s}`"))?;
        let parse = |x: &str| u16::from_str_radix(x.trim(), 16).map_err(|e| format!("`{x}`: {e}"));
        Ok(VidPid(parse(v)?, parse(p)?))
    }
}

/// List exportable devices. If `allow` is non-empty, only matching VID:PIDs are returned.
/// Hubs and root hubs are never exported (the Linux USB/IP stack refuses them too).
pub fn enumerate(allow: &[VidPid]) -> rusb::Result<Vec<ExportedDevice>> {
    let mut out = Vec::new();
    // An explicit context returns an error if libusb cannot initialise; the
    // global one (`rusb::devices()`) panics instead.
    let ctx = rusb::Context::new()?;
    for dev in ctx.devices()?.iter() {
        match describe(&dev) {
            Ok(Some(d)) => {
                let id = VidPid(d.device.id_vendor, d.device.id_product);
                if allow.is_empty() || allow.contains(&id) {
                    out.push(d);
                }
            }
            Ok(None) => {}
            Err(e) => tracing::debug!("skipping device {}:{}: {e}", dev.bus_number(), dev.address()),
        }
    }
    out.sort_by(|a, b| a.device.busid.cmp(&b.device.busid));
    Ok(out)
}

/// `(busid, name)` for every exportable device that has a readable name, e.g.
/// `("1-1.3", "Logitech HD Webcam C505")`. Names come from the device's own USB string
/// descriptors, so they need permission to open the device; devices that cannot be read are left out.
pub fn names(allow: &[VidPid]) -> rusb::Result<Vec<(String, String)>> {
    let ctx = rusb::Context::new()?;
    let mut out = Vec::new();
    for dev in ctx.devices()?.iter() {
        let Ok(Some(d)) = describe(&dev) else { continue };
        let id = VidPid(d.device.id_vendor, d.device.id_product);
        if !(allow.is_empty() || allow.contains(&id)) {
            continue;
        }
        if let Some(name) = cached_name(&dev, d.device.busnum as u8, d.device.devnum as u8, id) {
            out.push((d.device.busid, name));
        }
    }
    Ok(out)
}

/// Opening a device to read strings is cheap but not free, and the answer never changes while the
/// same device stays plugged in, so remember it (keyed so a different device on the same port
/// gets read again).
fn cached_name<C: UsbContext>(dev: &Device<C>, bus: u8, addr: u8, id: VidPid) -> Option<String> {
    type Cache = Mutex<HashMap<(u8, u8, u16, u16), Option<String>>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let key = (bus, addr, id.0, id.1);
    if let Some(hit) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return hit.clone();
    }
    let name = read_name(dev);
    cache.lock().unwrap_or_else(|e| e.into_inner()).insert(key, name.clone());
    name
}

fn read_name<C: UsbContext>(dev: &Device<C>) -> Option<String> {
    let desc = dev.device_descriptor().ok()?;
    let handle = dev.open().ok()?;
    let timeout = Duration::from_millis(500);
    let language = handle.read_languages(timeout).ok()?.into_iter().next()?;
    let manufacturer = handle.read_manufacturer_string(language, &desc, timeout).ok();
    let product = handle.read_product_string(language, &desc, timeout).ok();
    compose_name(manufacturer.as_deref(), product.as_deref())
}

/// Join manufacturer and product into one readable name, without repeating the manufacturer when
/// the product string already starts with it ("Logitech" + "Logitech Webcam" -> "Logitech Webcam").
pub fn compose_name(manufacturer: Option<&str>, product: Option<&str>) -> Option<String> {
    let clean = |s: Option<&str>| {
        let s: String = s?.chars().filter(|c| !c.is_control()).collect();
        let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
        (!s.is_empty()).then_some(s)
    };
    let (manufacturer, product) = (clean(manufacturer), clean(product));
    match (manufacturer, product) {
        (Some(m), Some(p)) if p.to_lowercase().contains(&m.to_lowercase()) => Some(p),
        (Some(m), Some(p)) => Some(format!("{m} {p}")),
        (None, Some(p)) => Some(p),
        (Some(m), None) => Some(m),
        (None, None) => None,
    }
}

fn describe<C: UsbContext>(dev: &Device<C>) -> rusb::Result<Option<ExportedDevice>> {
    let ports = dev.port_numbers()?;
    if ports.is_empty() {
        return Ok(None); // root hub
    }
    let desc = dev.device_descriptor()?;
    if desc.class_code() == 9 {
        return Ok(None); // hub
    }

    let bus = dev.bus_number();
    let port_path = ports.iter().map(u8::to_string).collect::<Vec<_>>().join(".");
    let busid = format!("{bus}-{port_path}");

    // The active config is normally served from sysfs without opening the device.
    // Unconfigured devices report an error; export them with no interfaces.
    let (configuration_value, interfaces) = match dev.active_config_descriptor() {
        Ok(cfg) => {
            let ifs = cfg
                .interfaces()
                .filter_map(|i| i.descriptors().next()) // alt setting 0
                .map(|d| UsbInterface {
                    class: d.class_code(),
                    subclass: d.sub_class_code(),
                    protocol: d.protocol_code(),
                })
                .collect();
            (cfg.number(), ifs)
        }
        Err(_) => (0, Vec::new()),
    };

    let speed = match dev.speed() {
        rusb::Speed::Low => Speed::Low,
        rusb::Speed::Full => Speed::Full,
        rusb::Speed::High => Speed::High,
        rusb::Speed::Super => Speed::Super,
        rusb::Speed::SuperPlus => Speed::SuperPlus,
        _ => Speed::Unknown,
    };

    let device = UsbDevice {
        path: format!("/sys/bus/usb/devices/{busid}"),
        busid,
        busnum: u32::from(bus),
        devnum: u32::from(dev.address()),
        speed,
        id_vendor: desc.vendor_id(),
        id_product: desc.product_id(),
        bcd_device: version_to_bcd(desc.device_version()),
        device_class: desc.class_code(),
        device_subclass: desc.sub_class_code(),
        device_protocol: desc.protocol_code(),
        configuration_value,
        num_configurations: desc.num_configurations(),
        num_interfaces: 0, // set by ExportedDevice::new
    };
    Ok(Some(ExportedDevice::new(device, interfaces)))
}

/// rusb decodes bcdDevice into (major, minor, sub_minor); turn it back into raw BCD.
fn version_to_bcd(v: rusb::Version) -> u16 {
    let (major, minor, sub) = (u16::from(v.major()), u16::from(v.minor()), u16::from(v.sub_minor()));
    ((major / 10) << 12) | ((major % 10) << 8) | (minor << 4) | sub
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composes_readable_names() {
        assert_eq!(compose_name(Some("Logitech"), Some("HD Webcam C505")).as_deref(), Some("Logitech HD Webcam C505"));
        assert_eq!(compose_name(Some("Logitech"), Some("Logitech HD Pro")).as_deref(), Some("Logitech HD Pro"));
        assert_eq!(compose_name(None, Some("USB Receiver")).as_deref(), Some("USB Receiver"));
        assert_eq!(compose_name(Some("SanDisk"), None).as_deref(), Some("SanDisk"));
        assert_eq!(compose_name(Some("  "), Some("\u{0}\n")), None);
        assert_eq!(compose_name(None, None), None);
        // Stray control characters and runs of spaces from sloppy firmware are tidied.
        assert_eq!(compose_name(Some("ACME\0"), Some("Gizmo   2000\r\n")).as_deref(), Some("ACME Gizmo 2000"));
    }

    #[test]
    fn parses_vid_pid() {
        assert_eq!("0403:6001".parse(), Ok(VidPid(0x0403, 0x6001)));
        assert_eq!("1d6b:2".parse(), Ok(VidPid(0x1d6b, 0x2)));
        assert!("0403".parse::<VidPid>().is_err());
        assert!("zz:01".parse::<VidPid>().is_err());
    }

    #[test]
    fn bcd_roundtrip() {
        assert_eq!(version_to_bcd(rusb::Version::from_bcd(0x0600)), 0x0600);
        assert_eq!(version_to_bcd(rusb::Version::from_bcd(0x1234)), 0x1234);
    }
}
