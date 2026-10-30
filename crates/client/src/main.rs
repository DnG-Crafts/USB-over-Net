//! Command-line client for testing the server.
//!
//!   usbip-client list   <host[:port]>
//!   usbip-client import <host[:port]> <busid>
//!   usbip-client discover [seconds]
//!   usbip-client install-driver [installer.exe]
//!   usbip-client uninstall-driver

use std::process::ExitCode;
use std::time::Duration;

use usbip_client::{import_device, list_devices, list_names, Discovery};

const TIMEOUT: Duration = Duration::from_secs(10);

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();

    let result = match args.as_slice() {
        ["list", addr] => list(addr),
        ["import", addr, busid] => import(addr, busid),
        ["uninstall-driver"] => uninstall_driver(),
        ["install-driver"] => install_driver(None),
        ["install-driver", file] => install_driver(Some(std::path::Path::new(file))),
        ["discover"] => discover(5),
        ["discover", secs] => match secs.parse() {
            Ok(n) => discover(n),
            Err(_) => Err("seconds must be a number".into()),
        },
        _ => {
            eprintln!("usage:\n  usbip-client list <host[:port]>\n  usbip-client import <host[:port]> <busid>\n  usbip-client discover [seconds]\n  usbip-client install-driver [installer.exe]\n  usbip-client uninstall-driver");
            return ExitCode::from(2);
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn list(addr: &str) -> Result<(), Box<dyn std::error::Error>> {
    let devs = list_devices(addr, TIMEOUT)?;
    let names = list_names(addr, TIMEOUT);
    if devs.is_empty() {
        println!("no exported devices");
    }
    for d in devs {
        let dev = &d.device;
        println!(
            "{:<10} {:04x}:{:04x}  class {:02x}  {:?}  {} interface(s)  {}",
            dev.busid,
            dev.id_vendor,
            dev.id_product,
            dev.device_class,
            dev.speed,
            d.interfaces.len(),
            names.get(&dev.busid).map_or("", String::as_str)
        );
        for (n, i) in d.interfaces.iter().enumerate() {
            println!("             if {n}: class {:02x}/{:02x}/{:02x}", i.class, i.subclass, i.protocol);
        }
    }
    Ok(())
}

fn import(addr: &str, busid: &str) -> Result<(), Box<dyn std::error::Error>> {
    let dev = import_device(addr, busid, TIMEOUT)?;
    println!("import ok: {} {:04x}:{:04x}", dev.busid, dev.id_vendor, dev.id_product);
    println!("(this only tests the handshake; use usbip-win2's `usbip attach` or the tray app to really use the device)");
    Ok(())
}

fn discover(secs: u64) -> Result<(), Box<dyn std::error::Error>> {
    let d = Discovery::start();
    if let Some(e) = d.mdns_error() {
        println!("(mDNS unavailable: {e}; using the subnet scan only)");
    }
    println!("searching for {secs}s...");
    std::thread::sleep(Duration::from_secs(secs));
    let found = d.servers();
    if found.is_empty() {
        println!("no servers found");
    }
    for s in found {
        println!("{:<24} {}", s.name, s.addr);
    }
    Ok(())
}

fn install_driver(file: Option<&std::path::Path>) -> Result<(), Box<dyn std::error::Error>> {
    use usbip_client::{attach, driver};
    if attach::exe_available(&attach::find_exe()) {
        println!("the USB driver is already installed ({})", attach::find_exe().display());
        return Ok(());
    }
    let local = file.map(std::path::Path::to_path_buf).or_else(|| driver::find_local_installer(&driver::default_installer_dirs()));
    match &local {
        Some(p) => println!("installing from {} (Windows will ask for permission)...", p.display()),
        None => println!("downloading and installing the latest usbip-win2 (Windows will ask for permission)..."),
    }
    let exe = driver::install(local.as_deref())?;
    println!("installed: {}", exe.display());
    Ok(())
}

fn uninstall_driver() -> Result<(), Box<dyn std::error::Error>> {
    println!("removing the USB driver (Windows will ask for permission)...");
    usbip_client::driver::uninstall()?;
    println!("removed");
    Ok(())
}
