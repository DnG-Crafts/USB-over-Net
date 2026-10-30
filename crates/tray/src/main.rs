// No console window for the GUI build on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod embedded;
mod settings;
mod single;
mod theme;
#[cfg(windows)]
mod tray;
mod ui;

use eframe::egui;
use image::imageops::{resize, FilterType};

const ICON_ICO: &[u8] = include_bytes!("../assets/icon.ico");

/// The app icon in the sizes we need.
pub struct Icon {
    /// Window / taskbar icon.
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Square image shown in the window header (drawn at half this size for sharpness on HiDPI).
    pub logo_rgba: Vec<u8>,
    pub logo_size: u32,
    /// Small square image for the system tray.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub tray_rgba: Vec<u8>,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub tray_size: u32,
}

fn load_icon() -> Option<Icon> {
    let img = image::load_from_memory_with_format(ICON_ICO, image::ImageFormat::Ico).ok()?.into_rgba8();
    let (width, height) = img.dimensions();
    let scaled = |size: u32| resize(&img, size, size, FilterType::Lanczos3).into_raw();
    Some(Icon {
        logo_rgba: scaled(80),
        logo_size: 80,
        tray_rgba: scaled(32),
        tray_size: 32,
        rgba: img.into_raw(),
        width,
        height,
    })
}

/// How the program was started.
pub struct Launch {
    /// Start hidden in the tray (the "Start minimized" setting, or `--minimized`).
    pub minimized: bool,
    /// The "Start minimized" setting itself, for the Options menu.
    pub start_minimized_setting: bool,
    /// Our claim on being the only running copy.
    pub guard: Option<std::net::TcpListener>,
}

fn main() -> eframe::Result {
    let minimized = std::env::args().any(|a| a == usbip_client::autostart::MINIMIZED_FLAG);
    let guard = match single::acquire() {
        single::Instance::First(listener) => Some(listener),
        single::Instance::Unguarded => None,
        single::Instance::Other => return Ok(()), // the running copy was asked to show itself
    };

    // "Start minimized" is on by default, except the very first run on a PC, which shows the window.
    let saved = settings::load_start_minimized(&settings::start_minimized_file());
    if saved.is_none() {
        let _ = settings::save_start_minimized(&settings::start_minimized_file(), true);
    }
    let launch = Launch {
        minimized: settings::should_start_hidden(minimized, saved),
        start_minimized_setting: saved.unwrap_or(true),
        guard,
    };
    let icon = load_icon();

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("USB over Net")
        .with_app_id("usb-over-net")
        .with_inner_size([500.0, 620.0])
        .with_min_inner_size([420.0, 380.0]);
    if let Some(i) = &icon {
        viewport = viewport.with_icon(egui::IconData { rgba: i.rgba.clone(), width: i.width, height: i.height });
    }

    eframe::run_native(
        "USB over Net",
        eframe::NativeOptions { viewport, ..Default::default() },
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, icon, launch)))),
    )
}
