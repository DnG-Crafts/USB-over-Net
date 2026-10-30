//! "Start with Windows": a per-user entry in the registry's `Run` key, managed with `reg.exe`
//! (no extra dependencies, no admin rights needed).
//!
//! Whether the window or only the tray icon appears at login is the app's own "Start minimized"
//! setting, so the entry is just the program path.

use std::path::Path;

use crate::attach::run_with;

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "USB over Net";
/// Forces a hidden start. Older versions put this in the login entry; it is still understood.
pub const MINIMIZED_FLAG: &str = "--minimized";

/// The command Windows runs at login for `exe`.
pub fn run_command(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

fn add_args(exe: &Path) -> Vec<String> {
    ["add", RUN_KEY, "/v", VALUE_NAME, "/t", "REG_SZ", "/d", &run_command(exe), "/f"].map(String::from).to_vec()
}

fn delete_args() -> Vec<String> {
    ["delete", RUN_KEY, "/v", VALUE_NAME, "/f"].map(String::from).to_vec()
}

fn query_args() -> Vec<String> {
    ["query", RUN_KEY, "/v", VALUE_NAME].map(String::from).to_vec()
}

/// The data of our value in `reg query` output, e.g.
/// `    USB over Net    REG_SZ    "C:\Apps\usbip-tray.exe"`.
pub fn parse_query(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let rest = line.trim().strip_prefix(VALUE_NAME)?.trim_start();
        let data = rest.strip_prefix("REG_SZ")?.trim();
        (!data.is_empty()).then(|| data.to_string())
    })
}

fn reg_exe() -> &'static Path {
    Path::new("reg.exe")
}

/// What is registered to run at login, if anything.
pub fn registered() -> Option<String> {
    registered_with(reg_exe())
}

fn registered_with(reg: &Path) -> Option<String> {
    // A missing value makes `reg query` exit non-zero, which is simply "not enabled".
    run_with(reg, &query_args(), "").ok().and_then(|out| parse_query(&out))
}

/// Turn "start with Windows" on or off for the current user.
pub fn set_enabled(enabled: bool) -> Result<(), String> {
    if !cfg!(windows) {
        return Err("Start with Windows is only available on Windows.".into());
    }
    let exe = std::env::current_exe().map_err(|e| format!("Could not find this program: {e}"))?;
    set_enabled_with(reg_exe(), &exe, enabled)
}

fn set_enabled_with(reg: &Path, exe: &Path, enabled: bool) -> Result<(), String> {
    if enabled {
        run_with(reg, &add_args(exe), "reg.exe was not found.")
            .map(|_| ())
            .map_err(|e| format!("Could not turn on Start with Windows: {e}"))
    } else if registered_with(reg).is_none() {
        Ok(()) // already off
    } else {
        run_with(reg, &delete_args(), "reg.exe was not found.")
            .map(|_| ())
            .map_err(|e| format!("Could not turn off Start with Windows: {e}"))
    }
}

/// If it is enabled but points at an old location (the app was moved or updated), point it here.
pub fn repair() {
    if !cfg!(windows) {
        return;
    }
    if let (Some(current), Ok(exe)) = (registered(), std::env::current_exe()) {
        if current != run_command(&exe) {
            let _ = set_enabled_with(reg_exe(), &exe, true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_the_command_and_arguments() {
        let exe = Path::new(r"C:\Program Files\USB over Net\usbip-tray.exe");
        assert_eq!(run_command(exe), r#""C:\Program Files\USB over Net\usbip-tray.exe""#);
        let add = add_args(exe);
        assert_eq!(&add[..4], ["add", RUN_KEY, "/v", "USB over Net"]);
        assert_eq!(add[7], run_command(exe));
        assert_eq!(add.last().unwrap(), "/f");
        assert_eq!(delete_args(), ["delete", RUN_KEY, "/v", "USB over Net", "/f"]);
    }

    #[test]
    fn parses_reg_query_output() {
        let out = "\r\nHKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Run\r\n    USB over Net    REG_SZ    \"C:\\Apps\\usbip-tray.exe\"\r\n\r\n";
        assert_eq!(parse_query(out).as_deref(), Some(r#""C:\Apps\usbip-tray.exe""#));
        assert_eq!(parse_query("nothing here"), None);
        assert_eq!(parse_query("    Other App    REG_SZ    x"), None);
    }

    /// A stand-in for reg.exe that keeps one value in a file.
    #[cfg(unix)]
    fn fake_reg(dir: &Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let reg = dir.join("reg");
        let store = dir.join("value");
        std::fs::write(
            &reg,
            format!(
                "#!/bin/sh\nS='{}'\ncase \"$1\" in\n\
                 add) printf '%s' \"$8\" > \"$S\"; exit 0;;\n\
                 delete) rm -f \"$S\"; exit 0;;\n\
                 query) [ -f \"$S\" ] || exit 1; printf '    USB over Net    REG_SZ    %s\\n' \"$(cat \"$S\")\"; exit 0;;\n\
                 esac\nexit 2\n",
                store.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&reg, std::fs::Permissions::from_mode(0o755)).unwrap();
        reg
    }

    #[cfg(unix)]
    #[test]
    fn enable_disable_roundtrip_with_a_fake_reg() {
        let dir = std::env::temp_dir().join(format!("uon-autostart-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = fake_reg(&dir);
        let exe = Path::new("/apps/usbip-tray.exe");

        assert_eq!(registered_with(&reg), None);
        assert_eq!(set_enabled_with(&reg, exe, false), Ok(())); // off -> off is fine
        assert_eq!(set_enabled_with(&reg, exe, true), Ok(()));
        assert_eq!(registered_with(&reg).as_deref(), Some(run_command(exe).as_str()));
        assert_eq!(set_enabled_with(&reg, exe, false), Ok(()));
        assert_eq!(registered_with(&reg), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn refuses_off_windows() {
        assert!(set_enabled(true).is_err());
    }
}
