//! One-click installation of the Windows USB/IP driver (usbip-win2).
//!
//! Windows can only present a remote USB device as a local one through a kernel driver, and a
//! driver needs a Microsoft-signed package. usbip-win2 ships exactly that, so instead of asking
//! the user to find and run an installer, the app does it: it uses an installer placed next to
//! the program if there is one, otherwise it downloads the newest release from GitHub, checks the
//! Authenticode signature, and runs it silently (Windows shows a single UAC prompt).

use std::path::{Path, PathBuf};

use crate::attach::run_with;

/// Name of the file (in the config folder) that remembers where `usbip.exe` ended up.
const SAVED_EXE_FILE: &str = "usbip-exe.txt";

/// Does `name` look like a usbip-win2 installer, e.g. `USBip-0.9.8.0-x64.exe`?
pub fn is_installer_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with("usbip-") && n.ends_with("-x64.exe")
}

/// First installer found in `dirs` (newest name wins if several are present).
pub fn find_local_installer(dirs: &[PathBuf]) -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_file() && path.file_name().and_then(|n| n.to_str()).is_some_and(is_installer_name) {
                found.push(path);
            }
        }
        if !found.is_empty() {
            break;
        }
    }
    found.sort();
    found.pop()
}

/// Folders worth checking for a bundled installer: next to our own executable, then the CWD.
pub fn default_installer_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        dirs.push(d);
    }
    if let Ok(d) = std::env::current_dir() {
        dirs.push(d);
    }
    dirs
}

/// `usbip.exe` location remembered from a previous install, if it still exists.
pub fn saved_exe() -> Option<PathBuf> {
    let text = std::fs::read_to_string(crate::config_dir().join(SAVED_EXE_FILE)).ok()?;
    let p = PathBuf::from(text.trim());
    p.is_file().then_some(p)
}

fn remember_exe(exe: &Path) {
    let dir = crate::config_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join(SAVED_EXE_FILE), exe.to_string_lossy().as_bytes());
}

/// Arguments for the usbip-win2 installer: silent, and only its two required components.
///
/// Its Inno Setup script (`userspace/innosetup/setup.iss`) offers `main` (usbip.exe, devnode.exe
/// and their DLLs) and `client` (the filter and UDE drivers, the virtual host controller and a task
/// that detaches devices at shutdown), both mandatory, plus optional `gui`, `sdk` and `pdb`.
/// A silent install with no selection takes all of them, so the optional ones are left out here,
/// as are the Start-menu shortcuts.
pub const INSTALLER_ARGS: [&str; 5] =
    ["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/COMPONENTS=main,client", "/NOICONS"];

/// PowerShell that installs the driver. `installer` is an optional local file; without it the
/// newest release is downloaded. Prints `USBIP_EXE=<path>` on success; any failure throws.
pub fn install_script(installer: Option<&Path>) -> String {
    let local = match installer {
        // Single-quoted PowerShell string: double any embedded quote.
        Some(p) => format!("'{}'", p.display().to_string().replace('\'', "''")),
        None => "$null".to_string(),
    };
    let args = INSTALLER_ARGS.map(|a| format!("'{a}'")).join(",");
    format!(
        r#"$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$installer = {local}
$tmp = $null
if (-not $installer) {{
    $rel = Invoke-RestMethod -UseBasicParsing -Headers @{{ 'User-Agent' = 'usb-over-net' }} 'https://api.github.com/repos/vadimgrn/usbip-win2/releases/latest'
    $asset = $rel.assets | Where-Object {{ $_.name -match '^USBip-.*-x64\.exe$' }} | Select-Object -First 1
    if (-not $asset) {{ throw 'The latest usbip-win2 release has no x64 installer.' }}
    $tmp = Join-Path $env:TEMP $asset.name
    Invoke-WebRequest -UseBasicParsing -Uri $asset.browser_download_url -OutFile $tmp
    $installer = $tmp
}}
$sig = Get-AuthenticodeSignature -FilePath $installer
if ($sig.Status -ne 'Valid') {{ throw ('The installer is not validly signed (' + $sig.Status + '); refusing to run it.') }}
$p = Start-Process -FilePath $installer -ArgumentList {args} -Verb RunAs -Wait -PassThru
if ($p.ExitCode -ne 0) {{ throw ('The installer exited with code ' + $p.ExitCode) }}
if ($tmp) {{ Remove-Item -Force -ErrorAction SilentlyContinue $tmp }}
$exe = Get-ChildItem -Path "$env:ProgramFiles\USBip*","${{env:ProgramFiles(x86)}}\USBip*" -Filter usbip.exe -Recurse -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $exe) {{ throw 'The driver installed, but usbip.exe could not be found afterwards.' }}
Write-Output ('USBIP_EXE=' + $exe.FullName)
"#
    )
}

/// Standard base64 (what PowerShell's `-EncodedCommand` expects, applied to UTF-16LE text).
fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Arguments for `powershell.exe` that run `script` without any quoting problems.
pub fn powershell_args(script: &str) -> Vec<String> {
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand"]
        .iter()
        .map(|s| s.to_string())
        .chain(std::iter::once(base64(&utf16)))
        .collect()
}

/// Find `USBIP_EXE=<path>` in the script's output.
pub fn parse_installed_exe(output: &str) -> Option<PathBuf> {
    output
        .lines()
        .find_map(|l| l.trim().strip_prefix("USBIP_EXE="))
        .map(|p| PathBuf::from(p.trim()))
        .filter(|p| !p.as_os_str().is_empty())
}

/// Install the driver and return the path of the new `usbip.exe`. Blocks until the installer
/// finishes (up to a minute or two); call it from a worker thread. Windows only.
pub fn install(installer: Option<&Path>) -> Result<PathBuf, String> {
    if !cfg!(windows) {
        return Err("The USB/IP driver can only be installed on Windows.".into());
    }
    let out = run_with(
        Path::new("powershell.exe"),
        &powershell_args(&install_script(installer)),
        "PowerShell is required to install the driver.",
    )
    .map_err(|e| format!("Driver installation failed: {e}"))?;
    let exe = parse_installed_exe(&out)
        .ok_or_else(|| format!("Driver installation finished but did not report where usbip.exe is: {out}"))?;
    remember_exe(&exe);
    Ok(exe)
}

/// PowerShell that removes the driver by running usbip-win2's own (Inno Setup) uninstaller, found
/// through the standard Windows "installed apps" registry entry. Prints `REMOVED` on success.
pub fn uninstall_script() -> String {
    r#"$ErrorActionPreference = 'Stop'
$keys = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*', 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*'
$app = Get-ItemProperty -Path $keys -ErrorAction SilentlyContinue | Where-Object { $_.DisplayName -like 'USBip*' } | Select-Object -First 1
if (-not $app -or -not $app.UninstallString) { throw 'The USB driver (usbip-win2) is not listed as installed. If it is still there, remove it in Windows Settings > Apps > Installed apps.' }
$cmd = $app.UninstallString.Trim()
if ($cmd -match '^"([^"]+)"') { $exe = $Matches[1] } else { $exe = ($cmd -split ' ')[0] }
if (-not (Test-Path -LiteralPath $exe)) { throw ('The uninstaller was not found: ' + $exe) }
$p = Start-Process -FilePath $exe -ArgumentList '/VERYSILENT','/SUPPRESSMSGBOXES','/NORESTART' -Verb RunAs -Wait -PassThru
if ($p.ExitCode -ne 0) { throw ('The uninstaller exited with code ' + $p.ExitCode) }
Write-Output 'REMOVED'
"#
    .to_string()
}

/// Remove the driver. Blocks until the uninstaller finishes; call it from a worker thread. Windows only.
pub fn uninstall() -> Result<(), String> {
    if !cfg!(windows) {
        return Err("The USB driver can only be removed on Windows.".into());
    }
    let out = run_with(
        Path::new("powershell.exe"),
        &powershell_args(&uninstall_script()),
        "PowerShell is required to remove the driver.",
    )
    .map_err(|e| format!("Driver removal failed: {e}"))?;
    if !out.lines().any(|l| l.trim() == "REMOVED") {
        return Err(format!("Driver removal did not finish cleanly: {out}"));
    }
    // Forget the remembered location; the file is gone now.
    let _ = std::fs::remove_file(crate::config_dir().join(SAVED_EXE_FILE));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_installer_names() {
        assert!(is_installer_name("USBip-0.9.8.0-x64.exe"));
        assert!(is_installer_name("usbip-1.0-X64.EXE"));
        assert!(!is_installer_name("usbip.exe"));
        assert!(!is_installer_name("USBip-0.9.8.0-arm64.exe"));
        assert!(!is_installer_name("other-0.9-x64.exe"));
    }

    #[test]
    fn finds_a_local_installer_and_prefers_the_newest() {
        let dir = std::env::temp_dir().join(format!("uon-inst-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(find_local_installer(std::slice::from_ref(&dir)), None);
        std::fs::write(dir.join("readme.txt"), b"x").unwrap();
        std::fs::write(dir.join("USBip-0.9.7.5-x64.exe"), b"x").unwrap();
        std::fs::write(dir.join("USBip-0.9.8.0-x64.exe"), b"x").unwrap();
        let found = find_local_installer(&[PathBuf::from("/no/such/dir"), dir.clone()]).unwrap();
        assert!(found.ends_with("USBip-0.9.8.0-x64.exe"), "{found:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn base64_matches_the_standard_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn powershell_args_encode_utf16le() {
        let a = powershell_args("A");
        assert_eq!(a.last().unwrap(), "QQA="); // 'A' = 41 00
        assert!(a.iter().any(|x| x == "-EncodedCommand"));
    }

    #[test]
    fn script_quotes_local_paths_and_checks_the_signature() {
        let s = install_script(Some(Path::new(r"C:\Users\O'Neil\USBip-1-x64.exe")));
        assert!(s.contains(r"'C:\Users\O''Neil\USBip-1-x64.exe'"), "{s}");
        assert!(s.contains("Get-AuthenticodeSignature"));
        assert!(s.contains("'/VERYSILENT'") && s.contains("'/COMPONENTS=main,client'") && s.contains("'/NOICONS'"));
        assert!(!s.contains("gui") && !s.contains("sdk") && !s.contains("pdb"));
        assert!(!s.contains("api.github.com") || install_script(None).contains("api.github.com"));
        assert!(install_script(None).contains("$installer = $null"));
        // The format braces must all have been resolved.
        assert!(!s.contains("{{") && !s.contains("}}"), "{s}");
    }

    #[test]
    fn parses_the_reported_path() {
        let out = "some noise\r\nUSBIP_EXE=C:\\Program Files\\USBip\\usbip.exe\r\n";
        assert_eq!(parse_installed_exe(out), Some(PathBuf::from(r"C:\Program Files\USBip\usbip.exe")));
        assert_eq!(parse_installed_exe("nothing here"), None);
        assert_eq!(parse_installed_exe("USBIP_EXE="), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn refuses_off_windows() {
        assert!(install(None).unwrap_err().contains("only be installed on Windows"));
        assert!(uninstall().unwrap_err().contains("only be removed on Windows"));
    }

    #[test]
    fn uninstall_script_runs_the_registered_silent_uninstaller() {
        let s = uninstall_script();
        assert!(s.contains("Uninstall\\*") && s.contains("USBip*"));
        assert!(s.contains("/VERYSILENT") && s.contains("-Verb RunAs"));
        assert!(s.contains("Write-Output 'REMOVED'"));
        // Must travel through -EncodedCommand without trouble.
        assert!(powershell_args(&s).last().is_some_and(|a| a.len() > 100));
    }
}
