//! Finding servers on the LAN without being told where they are.
//!
//! Two independent methods run side by side and their results are merged:
//!
//! * **mDNS / DNS-SD** (`_usbip._tcp`): instant and gives the server a friendly name, but needs
//!   multicast to work in both directions (Windows Firewall, Wi-Fi client isolation and some
//!   routers get in the way).
//! * **Subnet scan**: probes every host on the machine's local subnets on the USB/IP port and
//!   keeps only those that answer a real USB/IP `DEVLIST` request. Slower and unnamed, but it
//!   needs nothing except ordinary outgoing TCP, so it still works when mDNS does not.
//!
//! `Discovery::start()` runs both in the background; `servers()` returns the current merged
//! list and `generation()` changes whenever that list (or the "first scan done" state) changes.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use if_addrs::{IfAddr, IfOperStatus};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use usbip_proto::{ProtoError, DEFAULT_PORT, MDNS_SERVICE_TYPE};

use crate::list_devices_on;

/// How often the subnet scan repeats on its own.
const SCAN_EVERY: Duration = Duration::from_secs(60);
/// Per-host connect timeout during a scan (LAN round trips are a few milliseconds).
const SCAN_CONNECT_TIMEOUT: Duration = Duration::from_millis(400);
/// Time allowed for a connected host to answer the USB/IP handshake.
const SCAN_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const SCAN_WORKERS: usize = 64;
/// Never scan more than this many hosts per pass (a few /24s).
const MAX_SCAN_HOSTS: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredServer {
    /// Friendly name: the mDNS instance name, or a generic label for scan results.
    pub name: String,
    /// `ip:port`, ready for `connect`.
    pub addr: String,
}

type ServerMap = Arc<Mutex<BTreeMap<String, DiscoveredServer>>>;

pub struct Discovery {
    mdns: Option<ServiceDaemon>,
    mdns_error: Option<String>,
    mdns_found: ServerMap,
    scan_found: ServerMap,
    generation: Arc<AtomicU64>,
    scans_done: Arc<AtomicU64>,
    /// Dropping this stops the scan thread; sending on it triggers an immediate rescan.
    rescan: Sender<()>,
}

impl Discovery {
    /// Start both methods. Never fails: if mDNS cannot start (see `mdns_error`) the scan still runs.
    pub fn start() -> Self {
        let mdns_found: ServerMap = Arc::default();
        let scan_found: ServerMap = Arc::default();
        let generation = Arc::new(AtomicU64::new(0));
        let scans_done = Arc::new(AtomicU64::new(0));

        let (mdns, mdns_error) = match start_mdns(&mdns_found, &generation) {
            Ok(d) => (Some(d), None),
            Err(e) => (None, Some(e)),
        };

        let (rescan, wake) = channel::<()>();
        {
            let (found, gen, done) = (Arc::clone(&scan_found), Arc::clone(&generation), Arc::clone(&scans_done));
            thread::spawn(move || loop {
                let hits = scan_hosts(&local_scan_targets(), DEFAULT_PORT, SCAN_CONNECT_TIMEOUT);
                let fresh: BTreeMap<String, DiscoveredServer> = hits
                    .into_iter()
                    .map(|addr| (addr.clone(), DiscoveredServer { name: "USB/IP server".into(), addr }))
                    .collect();

                let first_scan = done.fetch_add(1, Ordering::SeqCst) == 0;
                let changed = std::mem::replace(&mut *lock(&found), fresh.clone()) != fresh;
                if changed || first_scan {
                    gen.fetch_add(1, Ordering::SeqCst); // first_scan: lets a GUI swap "searching" for "none found"
                }

                match wake.recv_timeout(SCAN_EVERY) {
                    Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break, // Discovery was dropped
                }
            });
        }

        Self { mdns, mdns_error, mdns_found, scan_found, generation, scans_done, rescan }
    }

    /// Everything currently visible, mDNS names preferred, sorted by address.
    pub fn servers(&self) -> Vec<DiscoveredServer> {
        let mut by_addr: BTreeMap<String, DiscoveredServer> = BTreeMap::new();
        for s in lock(&self.scan_found).values() {
            by_addr.insert(s.addr.clone(), s.clone());
        }
        for s in lock(&self.mdns_found).values() {
            by_addr.insert(s.addr.clone(), s.clone()); // named entry wins over the generic one
        }
        by_addr.into_values().collect()
    }

    /// Changes whenever the visible set changes, and once when the first scan finishes.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Number of completed subnet scans (0 means the first one is still running).
    pub fn scans_done(&self) -> u64 {
        self.scans_done.load(Ordering::SeqCst)
    }

    /// Why mDNS is unavailable, if it is.
    pub fn mdns_error(&self) -> Option<&str> {
        self.mdns_error.as_deref()
    }

    /// Scan the network again now instead of waiting for the next periodic scan.
    pub fn rescan(&self) {
        let _ = self.rescan.send(());
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        if let Some(d) = &self.mdns {
            let _ = d.shutdown();
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------- mDNS

fn start_mdns(found: &ServerMap, generation: &Arc<AtomicU64>) -> Result<ServiceDaemon, String> {
    let daemon = ServiceDaemon::new().map_err(|e| e.to_string())?;
    let events = daemon.browse(MDNS_SERVICE_TYPE).map_err(|e| e.to_string())?;
    let (found, generation) = (Arc::clone(found), Arc::clone(generation));
    thread::spawn(move || {
        // Ends when the daemon is shut down (the channel closes).
        while let Ok(event) = events.recv() {
            let changed = match event {
                ServiceEvent::ServiceResolved(info) => {
                    let v4: Vec<Ipv4Addr> = info.get_addresses_v4().into_iter().collect();
                    match pick_v4(&v4, info.get_port()) {
                        Some(addr) => {
                            let fullname = info.get_fullname().to_string();
                            let entry = DiscoveredServer { name: instance_name(&fullname), addr };
                            lock(&found).insert(fullname, entry.clone()) != Some(entry)
                        }
                        None => false,
                    }
                }
                ServiceEvent::ServiceRemoved(_, fullname) => lock(&found).remove(&fullname).is_some(),
                _ => false,
            };
            if changed {
                generation.fetch_add(1, Ordering::SeqCst);
            }
        }
    });
    Ok(daemon)
}

/// `"pi._usbip._tcp.local."` -> `"pi"`.
fn instance_name(fullname: &str) -> String {
    let suffix = format!(".{MDNS_SERVICE_TYPE}");
    fullname.strip_suffix(&suffix).unwrap_or(fullname).to_string()
}

/// Prefer a routable address over link-local (169.254/16) or loopback; deterministic order.
fn pick_v4(addrs: &[Ipv4Addr], port: u16) -> Option<String> {
    let mut sorted = addrs.to_vec();
    sorted.sort();
    let best = sorted
        .iter()
        .find(|a| !a.is_link_local() && !a.is_loopback())
        .or_else(|| sorted.first())?;
    Some(format!("{best}:{port}"))
}

// ---------------------------------------------------------------- subnet scan

/// Hosts worth probing on this machine's networks: the other addresses of each up, non-loopback
/// IPv4 interface's subnet, with subnets larger than a /24 narrowed to the /24 we are in.
pub fn local_scan_targets() -> Vec<Ipv4Addr> {
    let mut out: Vec<Ipv4Addr> = Vec::new();
    let Ok(interfaces) = if_addrs::get_if_addrs() else { return out };
    for iface in interfaces {
        if matches!(iface.oper_status, IfOperStatus::Down | IfOperStatus::NotPresent) {
            continue;
        }
        let IfAddr::V4(v4) = iface.addr else { continue };
        if v4.ip.is_loopback() || v4.ip.is_link_local() || v4.ip.is_unspecified() {
            continue;
        }
        for host in subnet_hosts(v4.ip, v4.netmask) {
            if !out.contains(&host) {
                out.push(host);
            }
        }
        if out.len() >= MAX_SCAN_HOSTS {
            out.truncate(MAX_SCAN_HOSTS);
            break;
        }
    }
    out
}

/// All usable host addresses in `ip`'s subnet except `ip` itself, capped at a /24.
pub fn subnet_hosts(ip: Ipv4Addr, netmask: Ipv4Addr) -> Vec<Ipv4Addr> {
    let prefix = u32::from(netmask).count_ones().max(24);
    if prefix >= 31 {
        return Vec::new(); // point-to-point or host route: nothing to scan
    }
    let mask = u32::MAX << (32 - prefix);
    let network = u32::from(ip) & mask;
    let broadcast = network | !mask;
    (network + 1..broadcast)
        .map(Ipv4Addr::from)
        .filter(|h| *h != ip)
        .collect()
}

/// Probe `hosts` on `port` in parallel; return `ip:port` for each one that completes a genuine
/// USB/IP `DEVLIST` exchange. (Plain "port is open" is not enough: other software uses 3240.)
pub fn scan_hosts(hosts: &[Ipv4Addr], port: u16, connect_timeout: Duration) -> Vec<String> {
    let next = AtomicUsize::new(0);
    let hits: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let workers = SCAN_WORKERS.min(hosts.len()).max(1);

    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                let Some(host) = hosts.get(i) else { break };
                let sock = std::net::SocketAddr::from((*host, port));
                // Most addresses on a LAN have nothing listening, so this usually ends here.
                let Ok(mut stream) = std::net::TcpStream::connect_timeout(&sock, connect_timeout) else {
                    continue;
                };
                // Handshake on the same connection: one connection per host, so a server's
                // log is not filled with half-open probes.
                let _ = stream.set_read_timeout(Some(SCAN_HANDSHAKE_TIMEOUT));
                let _ = stream.set_write_timeout(Some(SCAN_HANDSHAKE_TIMEOUT));
                // A server that answers with a proper USB/IP error (e.g. it cannot read the USB bus)
                // is still a USB/IP server; list it so the user sees its problem. Garbage is not.
                if matches!(list_devices_on(&mut stream), Ok(_) | Err(ProtoError::Status(_))) {
                    lock(&hits).push(sock.to_string());
                }
            });
        }
    });

    let mut hits = hits.into_inner().unwrap_or_else(|e| e.into_inner());
    hits.sort();
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use usbip_proto::{encode_devlist_reply, OpHeader, OP_REP_DEVLIST, ST_ERROR};

    #[test]
    fn picks_a_routable_address() {
        let a = |s: &str| s.parse::<Ipv4Addr>().unwrap();
        assert_eq!(pick_v4(&[a("169.254.3.4"), a("192.168.1.50")], 3240).as_deref(), Some("192.168.1.50:3240"));
        assert_eq!(pick_v4(&[a("169.254.3.4")], 3240).as_deref(), Some("169.254.3.4:3240"));
        assert_eq!(pick_v4(&[a("10.0.0.9"), a("10.0.0.2")], 99).as_deref(), Some("10.0.0.2:99"));
        assert_eq!(pick_v4(&[], 3240), None);
    }

    #[test]
    fn strips_the_service_suffix() {
        assert_eq!(instance_name("raspberrypi._usbip._tcp.local."), "raspberrypi");
        assert_eq!(instance_name("odd"), "odd");
    }

    #[test]
    fn enumerates_subnet_hosts() {
        let a = |s: &str| s.parse::<Ipv4Addr>().unwrap();

        let h = subnet_hosts(a("192.168.1.50"), a("255.255.255.0"));
        assert_eq!(h.len(), 253); // 254 usable minus ourselves
        assert!(!h.contains(&a("192.168.1.0")) && !h.contains(&a("192.168.1.255")) && !h.contains(&a("192.168.1.50")));
        assert!(h.contains(&a("192.168.1.1")) && h.contains(&a("192.168.1.254")));

        // A /16 is narrowed to the /24 we are in rather than scanning 65k hosts.
        let big = subnet_hosts(a("10.1.7.20"), a("255.255.0.0"));
        assert_eq!(big.len(), 253);
        assert!(big.iter().all(|h| h.octets()[..3] == [10, 1, 7]));

        // Small subnets are scanned as they are.
        assert_eq!(subnet_hosts(a("192.168.1.9"), a("255.255.255.248")).len(), 5); // .8-.15: 6 usable minus self
        assert!(subnet_hosts(a("10.0.0.1"), a("255.255.255.252")).len() == 1);
        assert!(subnet_hosts(a("10.0.0.1"), a("255.255.255.255")).is_empty());
    }

    /// Accepts one connection and replies like a server with no devices.
    fn real_server() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        thread::spawn(move || {
            if let Ok((mut s, _)) = l.accept() {
                let mut hdr = [0u8; 8];
                let _ = s.read_exact(&mut hdr);
                let _ = s.write_all(&encode_devlist_reply(&[]));
            }
        });
        port
    }

    #[test]
    fn scan_accepts_real_servers_and_rejects_impostors_and_closed_ports() {
        let loopback = [Ipv4Addr::LOCALHOST];

        let port = real_server();
        assert_eq!(scan_hosts(&loopback, port, Duration::from_millis(500)), vec![format!("127.0.0.1:{port}")]);

        // Something else listening on the port: accepts, then talks nonsense.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        thread::spawn(move || {
            if let Ok((mut s, _)) = l.accept() {
                let _ = s.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
            }
        });
        assert!(scan_hosts(&loopback, port, Duration::from_millis(500)).is_empty());

        // Nothing listening at all.
        let closed = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert!(scan_hosts(&loopback, closed, Duration::from_millis(500)).is_empty());
    }

    #[test]
    fn scan_lists_a_server_that_answers_with_a_usbip_error() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        thread::spawn(move || {
            if let Ok((mut s, _)) = l.accept() {
                let mut hdr = [0u8; 8];
                let _ = s.read_exact(&mut hdr);
                let _ = s.write_all(&OpHeader::new(OP_REP_DEVLIST, ST_ERROR).encode());
            }
        });
        assert_eq!(
            scan_hosts(&[Ipv4Addr::LOCALHOST], port, Duration::from_millis(500)),
            vec![format!("127.0.0.1:{port}")]
        );
    }

    #[test]
    fn scan_handles_an_empty_target_list() {
        assert!(scan_hosts(&[], 3240, Duration::from_millis(100)).is_empty());
    }
}
