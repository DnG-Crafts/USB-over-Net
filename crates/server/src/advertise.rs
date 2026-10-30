//! Announce this server on the LAN with mDNS / DNS-SD so clients can find it automatically.

use std::collections::HashMap;

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use usbip_proto::MDNS_SERVICE_TYPE;

/// Keeps the announcement alive; dropping it withdraws the service.
pub struct Advertisement {
    daemon: ServiceDaemon,
}

impl Advertisement {
    pub fn start(instance: &str, port: u16) -> Result<Self> {
        let daemon = ServiceDaemon::new().context("starting mDNS responder")?;
        let host = format!("{}.local.", host_label(instance));
        let props = HashMap::from([("version".to_string(), env!("CARGO_PKG_VERSION").to_string())]);
        // An empty address list plus `enable_addr_auto` makes the responder use whatever
        // addresses the machine's network interfaces currently have.
        let info = ServiceInfo::new(MDNS_SERVICE_TYPE, instance, &host, "", port, props)
            .context("building mDNS service record")?
            .enable_addr_auto();
        daemon.register(info).context("registering mDNS service")?;
        Ok(Self { daemon })
    }
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown(); // sends the goodbye packets
    }
}

/// This machine's name, for the default instance name.
pub fn local_hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "usbip-server".to_string())
}

/// A valid DNS host label: ASCII letters, digits and hyphens only.
fn host_label(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() { "usbip-server".into() } else { trimmed.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_labels_are_sanitised() {
        assert_eq!(host_label("raspberrypi"), "raspberrypi");
        assert_eq!(host_label("Lab Pi #2"), "Lab-Pi--2");
        assert_eq!(host_label("***"), "usbip-server");
    }
}
