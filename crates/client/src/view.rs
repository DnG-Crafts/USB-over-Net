//! GUI-independent model of "what the tree should show".
//!
//! `poll_servers` queries every saved server in parallel; `build_view` turns the
//! latest results into display rows. Keeping this out of the GUI makes it testable
//! on any platform and lets the GUI skip redraws when nothing changed.

use std::collections::HashMap;
use std::thread;
use std::time::Duration;

use usbip_proto::ExportedDevice;

use crate::{device_ids, device_kind, device_label, list_devices, list_names, normalize_addr, DiscoveredServer};

/// Result of one poll of one server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerStatus {
    pub addr: String,
    pub result: Result<Vec<ExportedDevice>, String>,
    /// busid -> device name, when the server provides them.
    pub names: HashMap<String, String>,
}

/// Query all servers concurrently. Output order matches `addrs`.
pub fn poll_servers(addrs: &[String], timeout: Duration) -> Vec<ServerStatus> {
    thread::scope(|scope| {
        let handles: Vec<_> = addrs
            .iter()
            .map(|addr| {
                scope.spawn(move || {
                    let result = list_devices(addr, timeout).map_err(|e| e.to_string());
                    // Only ask for names from a server that has devices to name.
                    let names = match &result {
                        Ok(devs) if !devs.is_empty() => list_names(addr, timeout),
                        _ => HashMap::new(),
                    };
                    ServerStatus { addr: addr.clone(), result, names }
                })
            })
            .collect();
        handles
            .into_iter()
            .zip(addrs)
            .map(|(h, addr)| {
                h.join().unwrap_or_else(|_| ServerStatus {
                    addr: addr.clone(),
                    result: Err("internal error".into()),
                    names: HashMap::new(),
                })
            })
            .collect()
    })
}

/// A server the GUI should show: saved by the user, found on the LAN, or both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerEntry {
    pub addr: String,
    /// Friendly name from mDNS, if the server was discovered.
    pub name: Option<String>,
    /// Whether it is in the user's saved list (only those can be removed).
    pub saved: bool,
}

/// Saved servers first (in saved order), then newly discovered ones. A discovered server whose
/// address matches a saved one just lends it its name.
pub fn merge_servers(saved: &[String], found: &[DiscoveredServer]) -> Vec<ServerEntry> {
    let same = |a: &str, b: &str| normalize_addr(a).eq_ignore_ascii_case(&normalize_addr(b));
    let mut out: Vec<ServerEntry> =
        saved.iter().map(|a| ServerEntry { addr: a.clone(), name: None, saved: true }).collect();
    for f in found {
        match out.iter_mut().find(|e| same(&e.addr, &f.addr)) {
            Some(e) => e.name = Some(f.name.clone()),
            None => out.push(ServerEntry { addr: f.addr.clone(), name: Some(f.name.clone()), saved: false }),
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewDevice {
    pub busid: String,
    pub label: String,
    pub in_use: bool,
    /// The device's own name, e.g. `Logitech HD Webcam C505`, if the server told us.
    pub name: Option<String>,
    /// e.g. `Mass Storage`
    pub kind: String,
    /// `vvvv:pppp`
    pub ids: String,
}

impl ViewDevice {
    /// What to call the device: its own name when known, otherwise its kind.
    pub fn title(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.kind)
    }
}

/// How a server is doing, for drawing its status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerState {
    Searching,
    Unreachable(String),
    NoDevices,
    Online,
}

/// What a tree row stands for. Stored in the row as an integer so the GUI can map a
/// selection back to a server or device without parsing text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeRef {
    Server(usize),
    Device { server: usize, device: usize },
}

const DEVICES_PER_SERVER: usize = 100_000;

impl NodeRef {
    pub fn to_param(self) -> isize {
        match self {
            NodeRef::Server(s) => s as isize,
            NodeRef::Device { server, device } => -((server * DEVICES_PER_SERVER + device) as isize) - 1,
        }
    }

    pub fn from_param(p: isize) -> Option<Self> {
        match p {
            isize::MIN => None, // the "no servers yet" hint row
            p if p >= 0 => Some(NodeRef::Server(p as usize)),
            p => {
                let n = (-(p + 1)) as usize;
                Some(NodeRef::Device { server: n / DEVICES_PER_SERVER, device: n % DEVICES_PER_SERVER })
            }
        }
    }

    pub fn server(self) -> usize {
        match self {
            NodeRef::Server(s) | NodeRef::Device { server: s, .. } => s,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewServer {
    pub addr: String,
    pub label: String,
    /// Friendly name from mDNS (or the address if there is none).
    pub name: String,
    pub state: ServerState,
    pub saved: bool,
    pub devices: Vec<ViewDevice>,
}

/// One row per server, in the given order. Servers with no result yet show as "searching";
/// results for servers that are no longer listed are dropped.
/// `in_use` lists `(server addr, busid)` pairs currently attached.
pub fn build_view(servers: &[ServerEntry], latest: &[ServerStatus], in_use: &[(String, String)]) -> Vec<ViewServer> {
    servers
        .iter()
        .map(|entry| {
            let name = match &entry.name {
                Some(n) => format!("{n}  ({})", entry.addr),
                None => entry.addr.clone(),
            };
            let (label, state, devices) = match latest.iter().find(|s| s.addr == entry.addr) {
                None => (format!("{name}  (searching...)"), ServerState::Searching, Vec::new()),
                Some(s) => match &s.result {
                    Err(e) => (format!("{name}  (unreachable: {e})"), ServerState::Unreachable(e.clone()), Vec::new()),
                    Ok(devs) if devs.is_empty() => (format!("{name}  (no devices)"), ServerState::NoDevices, Vec::new()),
                    Ok(devs) => (
                        name,
                        ServerState::Online,
                        devs.iter()
                            .map(|d| {
                                let used = in_use.iter().any(|(a, b)| *a == entry.addr && *b == d.device.busid);
                                let label = if used {
                                    format!("{}  - in use", device_label(d))
                                } else {
                                    device_label(d)
                                };
                                ViewDevice {
                                    busid: d.device.busid.clone(),
                                    label,
                                    in_use: used,
                                    name: s.names.get(&d.device.busid).cloned(),
                                    kind: device_kind(d),
                                    ids: device_ids(d),
                                }
                            })
                            .collect(),
                    ),
                },
            };
            ViewServer {
                addr: entry.addr.clone(),
                label,
                name: entry.name.clone().unwrap_or_else(|| entry.addr.clone()),
                state,
                saved: entry.saved,
                devices,
            }
        })
        .collect()
}

/// Text for the tray tooltip.
pub fn summary(view: &[ViewServer]) -> String {
    let devices: usize = view.iter().map(|s| s.devices.len()).sum();
    let s = |n: usize| if n == 1 { "" } else { "s" };
    format!("USB over Net - {} server{}, {} device{}", view.len(), s(view.len()), devices, s(devices))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use usbip_proto::{encode_devlist_reply, OpHeader, Speed, UsbDevice, UsbInterface, OP_HEADER_LEN};

    fn exported(busid: &str) -> ExportedDevice {
        let device = UsbDevice {
            path: String::new(),
            busid: busid.into(),
            busnum: 1,
            devnum: 3,
            speed: Speed::Full,
            id_vendor: 0x0403,
            id_product: 0x6001,
            bcd_device: 0x0600,
            device_class: 0,
            device_subclass: 0,
            device_protocol: 0,
            configuration_value: 1,
            num_configurations: 1,
            num_interfaces: 0,
        };
        ExportedDevice::new(device, vec![UsbInterface { class: 0xff, subclass: 0xff, protocol: 0xff }])
    }

    /// One-shot fake server: answers a single DEVLIST request with `devs`.
    fn fake_server(devs: Vec<ExportedDevice>) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut hdr = [0u8; OP_HEADER_LEN];
            s.read_exact(&mut hdr).unwrap();
            assert_eq!(OpHeader::decode(&hdr).code, usbip_proto::OP_REQ_DEVLIST);
            s.write_all(&encode_devlist_reply(&devs)).unwrap();
        });
        addr
    }

    #[test]
    fn polls_a_server_and_builds_rows() {
        let up = fake_server(vec![exported("1-1.2")]);
        // Port 1 on loopback: nothing listens there, connection is refused immediately.
        let down = "127.0.0.1:1".to_string();
        let servers = vec![up.clone(), down.clone()];

        let latest = poll_servers(&servers, Duration::from_secs(2));
        let servers = merge_servers(&servers, &[]);
        assert_eq!(latest.len(), 2);
        assert_eq!(latest[0].addr, up);
        assert_eq!(latest[1].addr, down);

        let view = build_view(&servers, &latest, &[(up.clone(), "1-1.2".to_string())]);
        assert_eq!(view[0].label, up);
        assert_eq!(view[0].devices.len(), 1);
        assert_eq!(view[0].devices[0].busid, "1-1.2");
        assert_eq!(view[0].devices[0].label, "Vendor Specific  0403:6001  [1-1.2]  - in use");
        assert!(view[0].devices[0].in_use);
        assert!(view[1].label.starts_with("127.0.0.1:1  (unreachable:"), "{}", view[1].label);
        assert!(view[1].devices.is_empty());
        assert_eq!(summary(&view), "USB over Net - 2 servers, 1 device");
    }

    #[test]
    fn pending_empty_and_removed_servers() {
        let servers = merge_servers(&["a:1".to_string(), "b:1".to_string()], &[]);
        let latest = vec![
            ServerStatus { addr: "b:1".into(), result: Ok(vec![]), names: HashMap::new() },
            ServerStatus { addr: "gone:1".into(), result: Ok(vec![exported("1-1")]), names: HashMap::new() },
        ];
        let view = build_view(&servers, &latest, &[]);
        assert_eq!(view.len(), 2); // "gone:1" dropped, configured order kept
        assert_eq!(view[0].label, "a:1  (searching...)");
        assert_eq!(view[1].label, "b:1  (no devices)");
        assert_eq!(summary(&[]), "USB over Net - 0 servers, 0 devices");
    }

    #[test]
    fn node_refs_roundtrip() {
        for n in [NodeRef::Server(0), NodeRef::Server(7), NodeRef::Device { server: 0, device: 0 }, NodeRef::Device { server: 3, device: 12 }] {
            assert_eq!(NodeRef::from_param(n.to_param()), Some(n));
        }
        assert_eq!(NodeRef::from_param(isize::MIN), None);
        assert_eq!(NodeRef::Device { server: 4, device: 9 }.server(), 4);
        // Server and device encodings never collide.
        assert!(NodeRef::Server(5).to_param() >= 0 && NodeRef::Device { server: 0, device: 0 }.to_param() < 0);
    }

    #[test]
    fn merges_saved_and_discovered_servers() {
        let found = vec![
            DiscoveredServer { name: "pi".into(), addr: "192.168.1.50:3240".into() },
            DiscoveredServer { name: "lab".into(), addr: "10.0.0.7:3240".into() },
        ];
        let saved = vec!["192.168.1.50".to_string(), "other.local".to_string()]; // default port elided
        let merged = merge_servers(&saved, &found);

        assert_eq!(merged.len(), 3);
        assert_eq!((merged[0].addr.as_str(), merged[0].name.as_deref(), merged[0].saved), ("192.168.1.50", Some("pi"), true));
        assert_eq!((merged[1].addr.as_str(), merged[1].name.as_deref(), merged[1].saved), ("other.local", None, true));
        assert_eq!((merged[2].addr.as_str(), merged[2].name.as_deref(), merged[2].saved), ("10.0.0.7:3240", Some("lab"), false));

        let view = build_view(&merged, &[], &[]);
        assert_eq!(view[0].label, "pi  (192.168.1.50)  (searching...)");
        assert!(view[0].saved && !view[2].saved);
    }
}
