//! "Use this device" on Windows: drive usbip-win2's `usbip.exe`.
//!
//! usbip-win2 provides the signed virtual host-controller driver; `usbip.exe attach` opens the
//! TCP connection to our server and hands it to that driver. We only run the command and parse
//! what it prints:
//!
//! ```text
//! usbip.exe attach -r <host> -b <busid>   ->  "successfully attached to port 1"
//! usbip.exe detach -p <port>
//! ```

use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

use usbip_proto::DEFAULT_PORT;

use crate::normalize_addr;

/// Where to find `usbip.exe`: `$USBIP_EXE`, the path remembered by the installer, the usbip-win2
/// install folder, then PATH.
pub fn find_exe() -> PathBuf {
    if let Some(p) = std::env::var_os("USBIP_EXE") {
        return p.into();
    }
    if let Some(p) = crate::driver::saved_exe() {
        return p;
    }
    for var in ["ProgramFiles", "ProgramW6432"] {
        if let Some(base) = std::env::var_os(var) {
            let p = PathBuf::from(base).join("USBip").join("usbip.exe");
            if p.is_file() {
                return p;
            }
        }
    }
    PathBuf::from("usbip.exe")
}

/// Is there actually a runnable `usbip.exe` at `exe` (or, for a bare name, somewhere on PATH)?
pub fn exe_available(exe: &Path) -> bool {
    exe_available_in(exe, std::env::var_os("PATH").as_deref())
}

fn exe_available_in(exe: &Path, path_var: Option<&OsStr>) -> bool {
    if exe.components().count() > 1 || exe.is_absolute() {
        return exe.is_file();
    }
    path_var.is_some_and(|paths| std::env::split_paths(paths).any(|dir| dir.join(exe).is_file()))
}

/// Split `host[:port]` (any form `normalize_addr` accepts) into host and port.
pub fn split_host_port(addr: &str) -> (String, u16) {
    let n = normalize_addr(addr);
    match n.rsplit_once(':') {
        Some((host, port)) => (
            host.trim_start_matches('[').trim_end_matches(']').to_string(),
            port.parse().unwrap_or(DEFAULT_PORT),
        ),
        None => (n, DEFAULT_PORT),
    }
}

pub fn attach_args(host: &str, busid: &str) -> Vec<String> {
    ["attach", "-r", host, "-b", busid].map(String::from).to_vec()
}

pub fn detach_args(port: u32) -> Vec<String> {
    vec!["detach".into(), "-p".into(), port.to_string()]
}

/// Pull the virtual port number out of `usbip attach` output.
pub fn parse_attach_port(output: &str) -> Option<u32> {
    let lower = output.to_ascii_lowercase();
    let after = &lower[lower.find("attached to port")? + "attached to port".len()..];
    let digits: String = after.trim_start().chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Attach `busid` from the server at `addr`. Returns the virtual port (needed to detach).
pub fn attach(exe: &Path, addr: &str, busid: &str) -> Result<u32, String> {
    let (host, port) = split_host_port(addr);
    if port != DEFAULT_PORT {
        // usbip.exe connects to the standard USB/IP port; don't silently connect elsewhere.
        return Err(format!(
            "usbip-win2 can only attach through port {DEFAULT_PORT}, but this server uses {port}. \
             Run the server with --listen 0.0.0.0:{DEFAULT_PORT}."
        ));
    }
    let text = run(exe, &attach_args(&host, busid)).map_err(with_hint)?;
    parse_attach_port(&text)
        .ok_or_else(|| format!("usbip reported success but did not say which port it used: {text}"))
}

pub fn detach(exe: &Path, port: u32) -> Result<(), String> {
    run(exe, &detach_args(port)).map(|_| ()).map_err(with_hint)
}

/// Add a pointer when usbip failed for lack of privileges.
fn with_hint(err: String) -> String {
    let l = err.to_ascii_lowercase();
    if l.contains("access is denied") || l.contains("access denied") || l.contains("administrator") {
        format!("{err}\n\nThis needs administrator rights: close the app and start it with 'Run as administrator'.")
    } else {
        err
    }
}

/// Run usbip and return its combined output; non-zero exit becomes `Err(output)`.
fn run(exe: &Path, args: &[String]) -> Result<String, String> {
    run_with(
        exe,
        args,
        "Install usbip-win2 (it provides the Windows USB/IP driver) or set the USBIP_EXE environment variable to its usbip.exe.",
    )
}

/// Like `run`, with the advice to give when the program cannot be found.
pub(crate) fn run_with(exe: &Path, args: &[String], not_found_advice: &str) -> Result<String, String> {
    let mut cmd = Command::new(exe);
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console flash from the tray app
    }
    let out = cmd.output().map_err(|e| match e.kind() {
        ErrorKind::NotFound => format!("Could not run {}. {not_found_advice}", exe.display()),
        _ => format!("Could not run {}: {e}", exe.display()),
    })?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
        .trim()
        .to_string();
    if out.status.success() {
        Ok(text)
    } else if text.is_empty() {
        Err(format!("usbip exited with {}", out.status))
    } else {
        Err(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_port_from_attach_output() {
        assert_eq!(parse_attach_port("successfully attached to port 1"), Some(1));
        assert_eq!(parse_attach_port("usbip: info: Successfully attached to port 12\r\n"), Some(12));
        assert_eq!(parse_attach_port("attach failed"), None);
        assert_eq!(parse_attach_port("attached to port x"), None);
    }

    #[test]
    fn splits_addresses() {
        assert_eq!(split_host_port("pi.local"), ("pi.local".into(), 3240));
        assert_eq!(split_host_port("10.0.0.5:3240"), ("10.0.0.5".into(), 3240));
        assert_eq!(split_host_port("10.0.0.5:99"), ("10.0.0.5".into(), 99));
        assert_eq!(split_host_port("[::1]:3240"), ("::1".into(), 3240));
    }

    #[test]
    fn finds_executables_by_path_or_on_path() {
        let dir = std::env::temp_dir().join(format!("uon-exe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("usbip.exe"), b"x").unwrap();

        assert!(exe_available_in(&dir.join("usbip.exe"), None));
        assert!(!exe_available_in(&dir.join("missing.exe"), None));
        let path = std::env::join_paths([dir.clone()]).unwrap();
        assert!(exe_available_in(Path::new("usbip.exe"), Some(&path)));
        assert!(!exe_available_in(Path::new("usbip.exe"), None));
        assert!(!exe_available_in(Path::new("nope.exe"), Some(&path)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn permission_errors_get_a_hint() {
        assert!(with_hint("Access is denied.".into()).contains("Run as administrator"));
        assert_eq!(with_hint("device busy".into()), "device busy");
    }

    #[test]
    fn builds_commands() {
        assert_eq!(attach_args("pi", "1-1.2"), ["attach", "-r", "pi", "-b", "1-1.2"]);
        assert_eq!(detach_args(3), ["detach", "-p", "3"]);
    }

    #[test]
    fn refuses_nonstandard_ports_without_running_anything() {
        let err = attach(Path::new("/definitely/not/here"), "pi:4000", "1-1").unwrap_err();
        assert!(err.contains("3240"), "{err}");
    }

    #[test]
    fn missing_exe_gives_a_helpful_message() {
        let err = attach(Path::new("/definitely/not/here/usbip.exe"), "pi", "1-1").unwrap_err();
        assert!(err.contains("usbip-win2") && err.contains("USBIP_EXE"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn drives_a_fake_usbip() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("uon-fake-usbip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("usbip");
        // Echoes its arguments so the test can check what we passed.
        std::fs::write(
            &exe,
            "#!/bin/sh\nif [ \"$1\" = attach ]; then echo \"args: $*\"; echo 'successfully attached to port 7'; exit 0; fi\n\
             if [ \"$1\" = detach ]; then echo \"port $3 is successfully detached\"; exit 0; fi\n\
             echo 'boom' >&2; exit 3\n",
        )
        .unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(attach(&exe, "192.168.1.50", "1-1.2"), Ok(7));
        assert_eq!(detach(&exe, 7), Ok(()));
        assert_eq!(run(&exe, &["bogus".to_string()]), Err("boom".to_string()));
        let _ = std::fs::remove_dir_all(dir);
    }
}
