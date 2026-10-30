// Build script:
//  1. embeds the icon (and version info) in the .exe so Explorer, the taskbar and Start show it;
//  2. embeds the usbip-win2 installer in the exe, so installing the USB driver needs no download.
//
// The installer is taken from `assets/driver/USBip-*-x64.exe`. On Windows, if it is not there the
// build downloads the latest release into that folder (set USB_OVER_NET_NO_DOWNLOAD=1 to skip).
// Nothing here may fail the build: without an embedded installer the app downloads it when needed.

use std::fs;
use std::path::{Path, PathBuf};

const DRIVER_DIR: &str = "assets/driver";

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed={DRIVER_DIR}");
    println!("cargo:rerun-if-env-changed=USB_OVER_NET_NO_DOWNLOAD");

    embed_icon();

    let dir = Path::new(DRIVER_DIR);
    #[allow(unused_mut)] // only reassigned on Windows, where it may be downloaded
    let mut installer = find_installer(dir);
    #[cfg(windows)]
    if installer.is_none() && std::env::var_os("USB_OVER_NET_NO_DOWNLOAD").is_none() {
        download_installer(dir);
        installer = find_installer(dir);
    }

    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let code = match installer.and_then(|p| Some((p.file_name()?.to_str()?.to_string(), fs::canonicalize(&p).ok()?))) {
        Some((name, abs)) => {
            // `canonicalize` gives \\?\ paths on Windows; strip the prefix so include_bytes! is happy.
            let abs = abs.to_string_lossy().trim_start_matches(r"\\?\").to_string();
            format!("pub const INSTALLER: Option<(&str, &[u8])> = Some(({name:?}, include_bytes!({abs:?})));\n")
        }
        None => {
            println!(
                "cargo:warning=no usbip-win2 installer in crates/tray/{DRIVER_DIR}: the app will download it when the driver is needed"
            );
            "pub const INSTALLER: Option<(&str, &[u8])> = None;\n".to_string()
        }
    };
    fs::write(out.join("driver_embed.rs"), code).expect("write driver_embed.rs");
}

fn find_installer(dir: &Path) -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                let n = n.to_ascii_lowercase();
                n.starts_with("usbip-") && n.ends_with("-x64.exe")
            })
        })
        .collect();
    found.sort();
    found.pop() // newest name wins
}

fn embed_icon() {
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "USB over Net");
        res.set("FileDescription", "USB over Net");
        // A missing resource compiler must not stop the build; the app just gets no file icon.
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed the icon in the exe: {e}");
        }
    }
}

/// Download the newest usbip-win2 x64 installer into `dir` and keep it only if Microsoft-signed.
#[cfg(windows)]
fn download_installer(dir: &Path) {
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    let dir = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let dir = dir.to_string_lossy().trim_start_matches(r"\\?\").replace('\'', "''");
    let script = format!(
        r#"$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$rel = Invoke-RestMethod -UseBasicParsing -Headers @{{ 'User-Agent' = 'usb-over-net' }} 'https://api.github.com/repos/vadimgrn/usbip-win2/releases/latest'
$asset = $rel.assets | Where-Object {{ $_.name -match '^USBip-.*-x64\.exe$' }} | Select-Object -First 1
if (-not $asset) {{ throw 'no x64 installer in the latest release' }}
$file = Join-Path '{dir}' $asset.name
Invoke-WebRequest -UseBasicParsing -Uri $asset.browser_download_url -OutFile $file
$sig = Get-AuthenticodeSignature -FilePath $file
if ($sig.Status -ne 'Valid') {{ Remove-Item -Force $file; throw ('installer is not validly signed: ' + $sig.Status) }}
"#
    );
    let status = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script])
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => println!("cargo:warning=could not download the usbip-win2 installer ({s})"),
        Err(e) => println!("cargo:warning=could not run PowerShell to download the installer: {e}"),
    }
}
