//! Application state and background work. Drawing lives in `ui.rs`.
//!
//! All network I/O and child processes run on worker threads; they report back through an mpsc
//! channel and wake the UI with `request_repaint`.

use std::collections::HashSet;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use usbip_client::attach::{self, exe_available, find_exe};
use usbip_client::autostart;

use crate::settings::{
    load_theme, save_start_minimized, save_theme, start_minimized_file, theme_file, ThemeChoice,
};
use usbip_client::{
    build_view, driver, is_valid_addr, load_servers, merge_servers, poll_servers, save_servers, servers_file,
    Discovery, ServerEntry, ServerStatus, ViewDevice, ViewServer,
};

const REFRESH_EVERY: Duration = Duration::from_secs(15);
const POLL_TIMEOUT: Duration = Duration::from_secs(3);
const TOAST_FOR: Duration = Duration::from_secs(4);

/// Results from worker threads.
enum Msg {
    Polled(Vec<ServerStatus>),
    Installed(Result<(), String>),
    Removed(Result<(), String>),
    /// Another launch of the program asked us to show our window.
    ShowWindow,
    Attached { addr: String, busid: String, kind: String, outcome: Result<u32, String> },
    Detached { addr: String, busid: String, kind: String, outcome: Result<(), String> },
}

/// A "Use" click that is waiting for the driver to be installed.
pub struct PendingUse {
    pub addr: String,
    pub dev: ViewDevice,
}

/// The "install the USB driver?" question.
pub struct InstallPrompt {
    pub then: Option<PendingUse>,
}

/// The "add a server" dialog.
#[derive(Default)]
pub struct AddDialog {
    pub text: String,
    pub error: Option<String>,
    pub focus: bool,
}

pub struct Toast {
    pub text: String,
    pub until: Instant,
}

pub struct App {
    pub ctx: egui::Context,
    pub logo: Option<egui::TextureHandle>,

    pub servers: Vec<String>,
    pub discovery: Discovery,
    pub latest: Vec<ServerStatus>,
    polling: bool,
    rerun: bool,
    seen_generation: u64,
    last_refresh: Instant,
    last_tick: Instant,

    tx: Sender<Msg>,
    rx: Receiver<Msg>,

    /// (server addr, busid, virtual port) for devices we attached.
    pub attached: Vec<(String, String, u32)>,
    /// Devices with an attach/detach running, to ignore repeated clicks.
    pub working: HashSet<(String, String)>,
    pub installing: bool,
    pub removing: bool,
    pub remove_prompt: bool,
    pub driver_ok: bool,
    /// "Start with Windows" is on.
    pub autostart: bool,
    pub theme: ThemeChoice,
    /// "Start minimized" is on: launch hidden in the tray.
    pub start_minimized: bool,
    start_hidden: bool,
    logic_calls: u32,

    pub errors: Vec<String>,
    pub toast: Option<Toast>,
    pub add: Option<AddDialog>,
    pub install_prompt: Option<InstallPrompt>,

    pub quitting: bool,
    #[cfg(windows)]
    pub tray: Option<crate::tray::Tray>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, icon: Option<crate::Icon>, launch: crate::Launch) -> Self {
        crate::theme::apply(&cc.egui_ctx);
        // USB_OVER_NET_THEME=light|dark|system overrides the saved choice for this run.
        let theme = std::env::var("USB_OVER_NET_THEME")
            .ok()
            .and_then(|t| ThemeChoice::parse(&t))
            .unwrap_or_else(|| load_theme(&theme_file()));
        cc.egui_ctx.set_theme(theme.preference());
        let (tx, rx) = mpsc::channel();

        if let Some(listener) = launch.guard {
            let (tx, ctx) = (tx.clone(), cc.egui_ctx.clone());
            crate::single::serve(listener, move || {
                let _ = tx.send(Msg::ShowWindow);
                ctx.request_repaint();
            });
        }
        autostart::repair(); // keep the login entry pointing at this copy if it was moved

        let logo = icon.as_ref().map(|i| {
            cc.egui_ctx.load_texture(
                "logo",
                egui::ColorImage::from_rgba_unmultiplied([i.logo_size as usize; 2], &i.logo_rgba),
                egui::TextureOptions::LINEAR,
            )
        });

        #[cfg(windows)]
        let tray = icon.as_ref().and_then(|i| match crate::tray::Tray::new(&cc.egui_ctx, i) {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!("tray icon unavailable: {e}");
                None
            }
        });

        let mut app = Self {
            ctx: cc.egui_ctx.clone(),
            logo,
            servers: load_servers(&servers_file()),
            // mDNS plus a subnet scan; if one method is blocked the other still finds servers.
            discovery: Discovery::start(),
            latest: Vec::new(),
            polling: false,
            rerun: false,
            seen_generation: 0,
            last_refresh: Instant::now(),
            last_tick: Instant::now(),
            tx,
            rx,
            attached: Vec::new(),
            working: HashSet::new(),
            installing: false,
            removing: false,
            remove_prompt: false,
            driver_ok: exe_available(&find_exe()),
            autostart: autostart::registered().is_some(),
            theme,
            start_minimized: launch.start_minimized_setting,
            start_hidden: launch.minimized,
            logic_calls: 0,
            errors: Vec::new(),
            toast: None,
            add: None,
            install_prompt: None,
            quitting: false,
            #[cfg(windows)]
            tray,
        };
        app.refresh();
        app
    }

    /// Saved servers plus whatever discovery currently sees.
    fn entries(&self) -> Vec<ServerEntry> {
        merge_servers(&self.servers, &self.discovery.servers())
    }

    pub fn view(&self) -> Vec<ViewServer> {
        let in_use: Vec<(String, String)> = self.attached.iter().map(|(a, b, _)| (a.clone(), b.clone())).collect();
        build_view(&self.entries(), &self.latest, &in_use)
    }

    /// Show a short confirmation at the bottom of the window.
    pub fn toast(&mut self, text: impl Into<String>) {
        self.toast = Some(Toast { text: text.into(), until: Instant::now() + TOAST_FOR });
    }

    // ---------------------------------------------------------------- polling

    pub fn refresh(&mut self) {
        if self.polling {
            // A poll is already running; run again when it finishes so changes are picked up promptly.
            self.rerun = true;
            return;
        }
        self.polling = true;
        self.last_refresh = Instant::now();
        let addrs: Vec<String> = self.entries().into_iter().map(|e| e.addr).collect();
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        thread::spawn(move || {
            let _ = tx.send(Msg::Polled(poll_servers(&addrs, POLL_TIMEOUT)));
            ctx.request_repaint();
        });
    }

    /// Scan the network again now, and re-poll what we already know.
    pub fn search_now(&mut self) {
        self.discovery.rescan();
        self.refresh();
    }

    /// Runs about once a second, even while the window is hidden in the tray.
    fn tick(&mut self) {
        let generation = self.discovery.generation();
        let changed = generation != std::mem::replace(&mut self.seen_generation, generation);
        if changed || self.last_refresh.elapsed() >= REFRESH_EVERY {
            self.refresh();
        }
        self.driver_ok = exe_available(&find_exe());
        #[cfg(windows)]
        if let Some(t) = &self.tray {
            t.set_tooltip(&usbip_client::summary(&self.view()));
        }
    }

    fn drain(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Polled(results) => {
                    self.latest = results;
                    self.polling = false;
                    if std::mem::take(&mut self.rerun) {
                        self.refresh();
                    }
                }
                Msg::Installed(Ok(())) => {
                    self.installing = false;
                    self.driver_ok = true;
                    self.toast("USB driver installed");
                }
                Msg::Installed(Err(e)) => {
                    self.installing = false;
                    self.errors.push(format!("Could not install the USB driver.\n{e}"));
                }
                Msg::ShowWindow => self.show_window(&self.ctx.clone()),
                Msg::Removed(Ok(())) => {
                    self.removing = false;
                    self.driver_ok = exe_available(&find_exe());
                    self.toast("USB driver removed");
                }
                Msg::Removed(Err(e)) => {
                    self.removing = false;
                    self.errors.push(format!("Could not remove the USB driver.\n{e}"));
                }
                Msg::Attached { addr, busid, kind, outcome } => {
                    self.working.remove(&(addr.clone(), busid.clone()));
                    match outcome {
                        Ok(port) => {
                            self.attached.push((addr, busid, port));
                            self.toast(format!("Now using {kind}"));
                        }
                        Err(e) if e.is_empty() => {} // already reported (driver install failed)
                        Err(e) => self.errors.push(format!("Could not use {kind}.\n{e}")),
                    }
                }
                Msg::Detached { addr, busid, kind, outcome } => {
                    self.working.remove(&(addr.clone(), busid.clone()));
                    match outcome {
                        Ok(()) => {
                            self.attached.retain(|(a, b, _)| !(*a == addr && *b == busid));
                            self.toast(format!("Stopped using {kind}"));
                        }
                        Err(e) => self.errors.push(format!("Could not release {kind}.\n{e}")),
                    }
                }
            }
        }
    }

    // ---------------------------------------------------------------- servers

    /// Validate, remember and start polling a manually entered server.
    pub fn add_server(&mut self, text: &str) -> Result<(), String> {
        let text = text.trim().to_string();
        if !is_valid_addr(&text) {
            return Err("Enter a host name or IP address, optionally with :port (for example 192.168.1.50 or raspberrypi.local).".into());
        }
        if !self.servers.contains(&text) {
            self.servers.push(text);
            if let Err(e) = save_servers(&servers_file(), &self.servers) {
                self.errors.push(format!("The server list could not be saved: {e}"));
            }
        }
        self.refresh();
        Ok(())
    }

    pub fn remove_server(&mut self, addr: &str) {
        self.servers.retain(|s| s != addr);
        if let Err(e) = save_servers(&servers_file(), &self.servers) {
            self.errors.push(format!("The server list could not be saved: {e}"));
        }
    }

    // ---------------------------------------------------------------- use / stop using a device

    /// "Use": installs the driver first (after asking) if this PC does not have it yet.
    pub fn request_use(&mut self, addr: String, dev: ViewDevice) {
        if exe_available(&find_exe()) {
            self.start_attach(addr, dev, false);
        } else if !self.installing {
            self.install_prompt = Some(InstallPrompt { then: Some(PendingUse { addr, dev }) });
        }
    }

    /// The user wants the driver installed without attaching anything yet.
    pub fn request_install(&mut self) {
        if !self.installing && !exe_available(&find_exe()) {
            self.install_prompt = Some(InstallPrompt { then: None });
        }
    }

    /// Options > Start minimized.
    pub fn set_start_minimized(&mut self, on: bool) {
        match save_start_minimized(&start_minimized_file(), on) {
            Ok(()) => self.start_minimized = on,
            Err(e) => self.errors.push(format!("The setting could not be saved: {e}")),
        }
    }

    /// Options > Theme.
    pub fn set_theme(&mut self, choice: ThemeChoice) {
        self.theme = choice;
        self.ctx.set_theme(choice.preference());
        if let Err(e) = save_theme(&theme_file(), choice) {
            self.errors.push(format!("The theme choice could not be saved: {e}"));
        }
    }

    /// Options > Start with Windows.
    pub fn set_autostart(&mut self, on: bool) {
        match autostart::set_enabled(on) {
            Ok(()) => {
                self.autostart = on;
                self.toast(if on { "Will start with Windows" } else { "Won't start with Windows" });
            }
            Err(e) => self.errors.push(e),
        }
    }

    /// Device > Remove USB driver...
    pub fn request_remove(&mut self) {
        if !self.removing && !self.installing && exe_available(&find_exe()) {
            self.remove_prompt = true;
        }
    }

    /// The user confirmed removal: release our devices first, then run the uninstaller.
    pub fn confirm_remove(&mut self) {
        self.removing = true;
        let ports: Vec<u32> = self.attached.drain(..).map(|(_, _, port)| port).collect();
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        thread::spawn(move || {
            let exe = find_exe();
            for port in ports {
                let _ = attach::detach(&exe, port);
            }
            let _ = tx.send(Msg::Removed(driver::uninstall()));
            ctx.request_repaint();
        });
    }

    /// The user answered "Install" to the prompt.
    pub fn confirm_install(&mut self, then: Option<PendingUse>) {
        match then {
            Some(p) => self.start_attach(p.addr, p.dev, true),
            None => {
                self.installing = true;
                let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
                thread::spawn(move || {
                    let _ = tx.send(Msg::Installed(install_driver()));
                    ctx.request_repaint();
                });
            }
        }
    }

    fn start_attach(&mut self, addr: String, dev: ViewDevice, install_first: bool) {
        if !self.working.insert((addr.clone(), dev.busid.clone())) {
            return;
        }
        self.installing |= install_first;
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        thread::spawn(move || {
            let mut outcome = Ok(find_exe());
            if install_first {
                let installed = install_driver();
                outcome = match &installed {
                    Ok(()) => Ok(find_exe()),
                    Err(_) => Err(String::new()), // reported by the Installed message below
                };
                let _ = tx.send(Msg::Installed(installed));
            }
            let outcome = outcome.and_then(|exe| attach::attach(&exe, &addr, &dev.busid));
            let kind = dev.title().to_string();
            let _ = tx.send(Msg::Attached { addr, busid: dev.busid, kind, outcome });
            ctx.request_repaint();
        });
    }

    pub fn stop_using(&mut self, addr: String, dev: ViewDevice) {
        let Some(port) = self.attached.iter().find(|(a, b, _)| *a == addr && *b == dev.busid).map(|(_, _, p)| *p)
        else {
            self.errors.push(
                "This device was attached outside this app, so it cannot be released from here. \
                 Use `usbip.exe detach -p <port>`."
                    .into(),
            );
            return;
        };
        if !self.working.insert((addr.clone(), dev.busid.clone())) {
            return;
        }
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        thread::spawn(move || {
            let outcome = attach::detach(&find_exe(), port);
            let kind = dev.title().to_string();
            let _ = tx.send(Msg::Detached { addr, busid: dev.busid, kind, outcome });
            ctx.request_repaint();
        });
    }

    /// Release everything we attached, so no device stays bound to a closed app.
    fn detach_all(&mut self) {
        let exe = find_exe();
        for (_, _, port) in self.attached.drain(..) {
            let _ = attach::detach(&exe, port);
        }
    }

    // ---------------------------------------------------------------- window

    pub fn show_window(&self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn quit(&mut self, ctx: &egui::Context) {
        self.quitting = true;
        self.detach_all();
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        // A window hidden in the tray might not process the close; make sure we really go away.
        thread::spawn(|| {
            thread::sleep(Duration::from_secs(3));
            std::process::exit(0);
        });
    }

    /// `true` when closing the window should only hide it (there is a tray icon to bring it back).
    pub fn has_tray(&self) -> bool {
        #[cfg(windows)]
        {
            self.tray.is_some()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    #[cfg(windows)]
    fn handle_tray(&mut self, ctx: &egui::Context) {
        use crate::tray::TrayCmd;
        let cmds = self.tray.as_ref().map(|t| t.poll()).unwrap_or_default();
        for cmd in cmds {
            match cmd {
                TrayCmd::Show => self.show_window(ctx),
                TrayCmd::Search => self.search_now(),
                TrayCmd::Quit => self.quit(ctx),
            }
        }
    }
}

/// Install the USB driver: from the installer built into this exe, else one next to it, else
/// downloaded. In every case the installer's Microsoft signature is checked before it runs.
fn install_driver() -> Result<(), String> {
    if let Some(unpacked) = crate::embedded::extract() {
        let (file, dir) = unpacked?;
        let result = driver::install(Some(&file)).map(|_| ());
        let _ = std::fs::remove_dir_all(dir);
        return result;
    }
    driver::install(driver::find_local_installer(&driver::default_installer_dirs()).as_deref()).map(|_| ())
}

impl eframe::App for App {
    /// Also runs while the window is hidden in the tray, so discovery and tray clicks keep working.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain();
        #[cfg(windows)]
        self.handle_tray(ctx);

        // Started by "Start with Windows": go to the tray. eframe shows the window after its first
        // frame no matter what, so wait for that frame and hide it on the next pass.
        self.logic_calls = self.logic_calls.saturating_add(1);
        if self.start_hidden {
            if self.logic_calls < 2 {
                ctx.request_repaint();
            } else {
                self.start_hidden = false;
                // Without a tray icon nothing could bring a hidden window back, so leave it showing.
                if self.has_tray() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                }
            }
        }
        if self.last_tick.elapsed() >= Duration::from_millis(900) {
            self.last_tick = Instant::now();
            self.tick();
        }
        ctx.request_repaint_after(Duration::from_secs(1));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.detach_all();
    }
}
