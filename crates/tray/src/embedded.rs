//! The usbip-win2 installer built into this exe (see build.rs), if the build found one.

use std::path::PathBuf;

include!(concat!(env!("OUT_DIR"), "/driver_embed.rs"));

/// `true` if this build carries the driver installer, so installing needs no download.
pub fn available() -> bool {
    INSTALLER.is_some()
}

/// Write the embedded installer to a temporary folder. Returns the file and the folder to delete
/// afterwards, or `None` when this build has no installer.
pub fn extract() -> Option<Result<(PathBuf, PathBuf), String>> {
    let (name, bytes) = INSTALLER?;
    let dir = std::env::temp_dir().join(format!("usb-over-net-driver-{}", std::process::id()));
    let write = || -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(&dir)?;
        let file = dir.join(name);
        std::fs::write(&file, bytes)?;
        Ok(file)
    };
    Some(write().map(|file| (file, dir.clone())).map_err(|e| format!("Could not unpack the driver installer: {e}")))
}

#[cfg(test)]
mod tests {
    #[test]
    fn embedded_installer_has_an_installer_name_when_present() {
        if let Some((name, bytes)) = super::INSTALLER {
            assert!(usbip_client::driver::is_installer_name(name), "{name}");
            assert!(!bytes.is_empty());

            let (file, dir) = super::extract().expect("present").expect("unpacks");
            assert_eq!(std::fs::read(&file).unwrap(), bytes);
            assert!(file.ends_with(name));
            let _ = std::fs::remove_dir_all(dir);
        } else {
            assert!(super::extract().is_none());
        }
    }
}
