//! The system-tray icon (Windows).
//!
//! Click events arrive on the tray library's own callback; we queue them and wake the UI, whose
//! `logic()` hook drains the queue. That hook still runs while the window is hidden.

use std::sync::mpsc::{self, Receiver};

use eframe::egui;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

pub enum TrayCmd {
    Show,
    Search,
    Quit,
}

pub struct Tray {
    icon: TrayIcon,
    rx: Receiver<TrayCmd>,
}

impl Tray {
    /// Must be called on the thread that runs the window's event loop (eframe's app creator does).
    pub fn new(ctx: &egui::Context, icon: &crate::Icon) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel();

        let show = MenuItem::with_id("show", "Open USB over Net", true, None);
        let search = MenuItem::with_id("search", "Search network now", true, None);
        let quit = MenuItem::with_id("quit", "Quit", true, None);
        let menu = Menu::new();
        menu.append_items(&[&show, &search, &PredefinedMenuItem::separator(), &quit]).map_err(|e| e.to_string())?;

        let image = Icon::from_rgba(icon.tray_rgba.clone(), icon.tray_size, icon.tray_size).map_err(|e| e.to_string())?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("USB over Net")
            .with_icon(image)
            .build()
            .map_err(|e| e.to_string())?;

        {
            let (tx, ctx) = (tx.clone(), ctx.clone());
            MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
                let id: &str = event.id.as_ref();
                let cmd = match id {
                    "show" => TrayCmd::Show,
                    "search" => TrayCmd::Search,
                    "quit" => TrayCmd::Quit,
                    _ => return,
                };
                let _ = tx.send(cmd);
                ctx.request_repaint();
            }));
        }
        {
            let ctx = ctx.clone();
            TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
                let open = matches!(
                    event,
                    TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
                        | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. }
                );
                if open {
                    let _ = tx.send(TrayCmd::Show);
                    ctx.request_repaint();
                }
            }));
        }

        Ok(Self { icon: tray, rx })
    }

    pub fn poll(&self) -> Vec<TrayCmd> {
        self.rx.try_iter().collect()
    }

    pub fn set_tooltip(&self, text: &str) {
        let _ = self.icon.set_tooltip(Some(text));
    }
}
