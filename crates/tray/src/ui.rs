//! Drawing: header, server cards, device rows, dialogs.

use eframe::egui::{
    self, Align, Align2, CentralPanel, Color32, CornerRadius, Frame, Id, Layout, Margin, Modal, Order, Panel, RichText,
    ScrollArea, Sense, Stroke, TextEdit, Vec2,
};
use usbip_client::{ServerState, ViewDevice, ViewServer};

use crate::app::{AddDialog, App, InstallPrompt};
use crate::settings::ThemeChoice;
use crate::theme::{bold, palette, Palette};

impl App {
    pub fn draw(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let p = palette(ui.visuals().dark_mode);

        // The X button hides the window to the tray; "Quit" in the tray menu really quits.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting && self.has_tray() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        let view = self.view();

        Panel::top("menubar")
            .frame(Frame::new().fill(p.window).inner_margin(Margin { left: 12, right: 12, top: 4, bottom: 0 }))
            .show_separator_line(false)
            .show(ui, |ui| self.menu_bar(ui, &view));

        Panel::top("header")
            .frame(Frame::new().fill(p.window).inner_margin(Margin { left: 20, right: 20, top: 8, bottom: 10 }))
            .show_separator_line(false)
            .show(ui, |ui| self.header(ui, &p));

        Panel::bottom("footer")
            .frame(Frame::new().fill(p.window).inner_margin(Margin { left: 20, right: 20, top: 8, bottom: 10 }))
            .show_separator_line(false)
            .show(ui, |ui| self.footer(ui, &p, &view));

        CentralPanel::default().frame(Frame::new().fill(p.window).inner_margin(Margin::symmetric(20, 4))).show(ui, |ui| {
            self.error_banner(ui, &p);
            ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                if view.is_empty() {
                    self.empty_state(ui, &p);
                }
                for server in &view {
                    self.server_card(ui, &p, server);
                    ui.add_space(12.0);
                }
            });
        });

        self.dialogs(&ctx, &p);
        self.show_toast(&ctx, &p);
    }

    // ---------------------------------------------------------------- menu bar

    fn menu_bar(&mut self, ui: &mut egui::Ui, view: &[ViewServer]) {
        let ctx = ui.ctx().clone();
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("Servers", |ui| {
                if ui.button("Add server…").clicked() {
                    self.add = Some(AddDialog { focus: true, ..Default::default() });
                }
                if ui.button("Search network now").clicked() {
                    self.search_now();
                }
                if ui.button("Refresh now").clicked() {
                    self.refresh();
                }
                ui.menu_button("Remove server", |ui| {
                    let saved: Vec<String> = view.iter().filter(|s| s.saved).map(|s| s.addr.clone()).collect();
                    if saved.is_empty() {
                        ui.add_enabled(false, egui::Button::new("No saved servers"));
                    }
                    for addr in saved {
                        if ui.button(&addr).clicked() {
                            self.remove_server(&addr);
                        }
                    }
                });
                ui.separator();
                if self.has_tray() && ui.button("Hide to tray").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                }
                if ui.button("Quit").clicked() {
                    self.quit(&ctx);
                }
            });

            ui.menu_button("Options", |ui| {
                ui.menu_button("Theme", |ui| {
                    for choice in ThemeChoice::ALL {
                        if ui.radio(self.theme == choice, choice.label()).clicked() && self.theme != choice {
                            self.set_theme(choice);
                        }
                    }
                });
                let mut minimized = self.start_minimized;
                let check = ui
                    .checkbox(&mut minimized, "Start minimized")
                    .on_hover_text("Open hidden in the system tray instead of showing this window");
                if check.changed() {
                    self.set_start_minimized(minimized);
                }
                let mut on = self.autostart;
                let check = ui.add_enabled(cfg!(windows), egui::Checkbox::new(&mut on, "Start with Windows"));
                if check.changed() {
                    self.set_autostart(on);
                }
            });

            ui.menu_button("Device", |ui| {
                let devices = |in_use: bool| -> Vec<(String, ViewDevice, String)> {
                    view.iter()
                        .flat_map(|s| {
                            s.devices
                                .iter()
                                .filter(move |d| d.in_use == in_use)
                                .map(move |d| (s.addr.clone(), d.clone(), format!("{}  {}  ·  {}", d.title(), d.ids, s.name)))
                        })
                        .collect()
                };

                ui.menu_button("Use", |ui| {
                    let list = devices(false);
                    if list.is_empty() {
                        ui.add_enabled(false, egui::Button::new("No devices available"));
                    }
                    for (addr, dev, label) in list {
                        if ui.button(label).clicked() {
                            self.request_use(addr, dev);
                        }
                    }
                });
                ui.menu_button("Stop using", |ui| {
                    let list = devices(true);
                    if list.is_empty() {
                        ui.add_enabled(false, egui::Button::new("No devices in use"));
                    }
                    for (addr, dev, label) in list {
                        if ui.button(label).clicked() {
                            self.stop_using(addr, dev);
                        }
                    }
                });
                ui.separator();
                let idle = !self.installing && !self.removing;
                if ui.add_enabled(idle && !self.driver_ok, egui::Button::new("Install USB driver…")).clicked() {
                    self.request_install();
                }
                if ui.add_enabled(idle && self.driver_ok, egui::Button::new("Remove USB driver…")).clicked() {
                    self.request_remove();
                }
            });
        });
    }

    // ---------------------------------------------------------------- header / footer

    fn header(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.horizontal(|ui| {
            if let Some(logo) = &self.logo {
                ui.add(egui::Image::new(logo).fit_to_exact_size(Vec2::splat(36.0)));
            }
            ui.label(bold("USB over Net").size(22.0));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if primary_button(ui, p, "+  Add server").clicked() {
                    self.add = Some(AddDialog { focus: true, ..Default::default() });
                }
                if ui.button("Rescan").on_hover_text("Search the network for servers again").clicked() {
                    self.search_now();
                }
            });
        });
    }

    fn footer(&mut self, ui: &mut egui::Ui, p: &Palette, view: &[ViewServer]) {
        let devices: usize = view.iter().map(|s| s.devices.len()).sum();
        let servers = view.len();
        let plural = |n: usize| if n == 1 { "" } else { "s" };
        let mut text = format!("{servers} server{} · {devices} device{}", plural(servers), plural(devices));
        if !self.attached.is_empty() {
            text.push_str(&format!(" · {} in use", self.attached.len()));
        }

        ui.horizontal(|ui| {
            ui.label(RichText::new(text).small().color(p.muted));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if self.installing {
                    ui.label(RichText::new("Installing USB driver…").small().color(p.muted));
                    ui.spinner();
                } else if self.removing {
                    ui.label(RichText::new("Removing USB driver…").small().color(p.muted));
                    ui.spinner();
                } else if self.driver_ok {
                    ui.label(RichText::new("USB driver ready").small().color(p.muted));
                    dot(ui, p.good);
                } else {
                    if ui.small_button("Install").clicked() {
                        self.request_install();
                    }
                    ui.label(RichText::new("USB driver not installed").small().color(p.muted));
                    dot(ui, p.warn);
                }
            });
        });
    }

    fn error_banner(&mut self, ui: &mut egui::Ui, p: &Palette) {
        if self.errors.is_empty() {
            return;
        }
        let mut dismiss = false;
        Frame::new()
            .fill(p.bad_soft)
            .stroke(Stroke::new(1.0, p.bad))
            .corner_radius(10)
            .inner_margin(Margin::same(12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                for e in &self.errors {
                    ui.label(RichText::new(e).color(p.bad));
                }
                if ui.small_button("Dismiss").clicked() {
                    dismiss = true;
                }
            });
        ui.add_space(10.0);
        if dismiss {
            self.errors.clear();
        }
    }

    // ---------------------------------------------------------------- body

    fn empty_state(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let searching = self.discovery.scans_done() == 0;
        ui.add_space(56.0);
        ui.vertical_centered(|ui| {
            if searching {
                ui.spinner();
                ui.add_space(12.0);
                ui.label(bold("Looking for servers…").size(18.0));
                ui.label(RichText::new("Servers on your network show up here by themselves.").color(p.muted));
            } else {
                ui.label(bold("No servers found").size(18.0));
                ui.add_space(2.0);
                ui.label(RichText::new("Check that usbip-server is running on your Raspberry Pi").color(p.muted));
                ui.label(RichText::new("and that this PC is on the same network.").color(p.muted));
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    // Centre the two buttons.
                    let w = 250.0;
                    ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
                    if ui.button("Search again").clicked() {
                        self.search_now();
                    }
                    if primary_button(ui, p, "Add server manually").clicked() {
                        self.add = Some(AddDialog { focus: true, ..Default::default() });
                    }
                });
            }
        });
    }

    fn server_card(&mut self, ui: &mut egui::Ui, p: &Palette, server: &ViewServer) {
        Frame::new()
            .fill(p.card)
            .stroke(Stroke::new(1.0, p.card_stroke))
            .corner_radius(12)
            .inner_margin(Margin::same(14))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());

                ui.horizontal(|ui| {
                    let colour = match server.state {
                        ServerState::Online => p.good,
                        ServerState::Searching | ServerState::NoDevices => p.warn,
                        ServerState::Unreachable(_) => p.bad,
                    };
                    dot(ui, colour);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.label(bold(&server.name).size(16.0));
                        if server.name != server.addr {
                            ui.label(RichText::new(&server.addr).small().color(p.muted));
                        }
                    });
                    if server.saved {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.small_button("Remove").on_hover_text("Forget this server").clicked() {
                                self.remove_server(&server.addr);
                            }
                        });
                    }
                });

                match &server.state {
                    ServerState::Searching => {
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(RichText::new("Looking for devices…").color(p.muted));
                        });
                    }
                    ServerState::Unreachable(why) => {
                        ui.add_space(4.0);
                        ui.label(RichText::new("Can't reach this server").color(p.bad));
                        ui.label(RichText::new(why).small().color(p.muted));
                    }
                    ServerState::NoDevices => {
                        ui.add_space(4.0);
                        ui.label(RichText::new("This server isn't sharing any USB devices.").color(p.muted));
                    }
                    ServerState::Online => {
                        ui.add_space(6.0);
                        for (i, dev) in server.devices.iter().enumerate() {
                            if i > 0 {
                                ui.add_space(6.0);
                            }
                            self.device_row(ui, p, server, dev);
                        }
                    }
                }
            });
    }

    fn device_row(&mut self, ui: &mut egui::Ui, p: &Palette, server: &ViewServer, dev: &ViewDevice) {
        let working = self.working.contains(&(server.addr.clone(), dev.busid.clone()));

        Frame::new().fill(p.row).corner_radius(10).inner_margin(Margin::symmetric(10, 9)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                badge(ui, p, &dev.kind, dev.in_use);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.label(bold(dev.title()).size(14.5));
                    // With a real name as the title, the kind moves down here so nothing is lost.
                    let details = if dev.name.is_some() {
                        format!("{}  ·  {}  ·  {}", dev.kind, dev.ids, dev.busid)
                    } else {
                        format!("{}  ·  {}", dev.ids, dev.busid)
                    };
                    ui.label(RichText::new(details).small().color(p.muted));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if working {
                        ui.spinner();
                    } else if dev.in_use {
                        if ui.button("Stop using").clicked() {
                            self.stop_using(server.addr.clone(), dev.clone());
                        }
                        pill(ui, p, "In use");
                    } else if primary_button(ui, p, "Use").clicked() {
                        self.request_use(server.addr.clone(), dev.clone());
                    }
                });
            });
        });
    }

    // ---------------------------------------------------------------- dialogs / toast

    fn dialogs(&mut self, ctx: &egui::Context, p: &Palette) {
        // "Add server"
        if let Some(mut d) = self.add.take() {
            let mut submit = false;
            let mut cancel = false;
            let modal = Modal::new(Id::unique("add_server")).show(ctx, |ui| {
                ui.set_width(340.0);
                ui.label(bold("Add a server").size(18.0));
                ui.label(
                    RichText::new("Servers on your network appear by themselves. Use this for one that doesn't, such as a Pi over a VPN.")
                        .small()
                        .color(p.muted),
                );
                ui.add_space(4.0);
                let edit = ui.add(
                    TextEdit::singleline(&mut d.text)
                        .hint_text("192.168.1.50 or raspberrypi.local")
                        .desired_width(f32::INFINITY)
                        .margin(Margin::symmetric(10, 8)),
                );
                if std::mem::take(&mut d.focus) {
                    edit.request_focus();
                }
                if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    submit = true;
                }
                if let Some(e) = &d.error {
                    ui.label(RichText::new(e).small().color(p.bad));
                }
                ui.add_space(4.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if primary_button(ui, p, "Add").clicked() {
                        submit = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
            if modal.should_close() {
                cancel = true;
            }
            if submit {
                match self.add_server(&d.text) {
                    Ok(()) => {}
                    Err(e) => {
                        d.error = Some(e);
                        self.add = Some(d);
                    }
                }
            } else if !cancel {
                self.add = Some(d);
            }
        }

        // "Remove the USB driver?"
        if self.remove_prompt {
            let mut answer: Option<bool> = None;
            let modal = Modal::new(Id::unique("remove_driver")).show(ctx, |ui| {
                ui.set_width(360.0);
                ui.label(bold("Remove the USB driver?").size(18.0));
                ui.label(
                    "Devices you are using will be released first. You won't be able to use network USB \
                     devices again until the driver is installed.",
                );
                ui.label(
                    RichText::new("Windows will ask for permission, and your USB 3 ports may restart for a moment.")
                        .small()
                        .color(p.muted),
                );
                ui.add_space(6.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(egui::Button::new(bold("Remove driver").color(Color32::WHITE)).fill(Color32::from_rgb(0xdc, 0x26, 0x26))).clicked() {
                        answer = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        answer = Some(false);
                    }
                });
            });
            if answer == Some(true) {
                self.remove_prompt = false;
                self.confirm_remove();
            } else if answer == Some(false) || modal.should_close() {
                self.remove_prompt = false;
            }
        }

        // "Install the USB driver?"
        if let Some(prompt) = self.install_prompt.take() {
            let mut answer: Option<bool> = None;
            let modal = Modal::new(Id::unique("install_driver")).show(ctx, |ui| {
                ui.set_width(360.0);
                ui.label(bold("Install the USB driver?").size(18.0));
                ui.label(
                    "Windows needs a small driver to treat a network USB device as if it were plugged into this PC. \
                     It's the signed usbip-win2 driver with just its two small helper tools: no extra apps. You only need to install it once.",
                );
                let how = if crate::embedded::available() {
                    "The installer is built into this app. Its Microsoft signature is checked before it runs, and Windows will ask for permission once."
                } else {
                    "The installer is downloaded and its Microsoft signature checked before it runs. Windows will ask for permission once."
                };
                ui.label(RichText::new(how).small().color(p.muted));
                ui.add_space(6.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if primary_button(ui, p, "Install driver").clicked() {
                        answer = Some(true);
                    }
                    if ui.button("Not now").clicked() {
                        answer = Some(false);
                    }
                });
            });
            match answer {
                Some(true) => self.confirm_install(prompt.then),
                Some(false) => {}
                None if modal.should_close() => {}
                None => self.install_prompt = Some(InstallPrompt { then: prompt.then }),
            }
        }
    }

    fn show_toast(&mut self, ctx: &egui::Context, p: &Palette) {
        let Some(toast) = &self.toast else { return };
        let now = std::time::Instant::now();
        if now >= toast.until {
            self.toast = None;
            return;
        }
        ctx.request_repaint_after(toast.until - now);
        egui::Area::new(Id::unique("toast"))
            .anchor(Align2::CENTER_BOTTOM, [0.0, -46.0])
            .order(Order::Foreground)
            .interactable(false)
            .show(ctx, |ui| {
                Frame::new()
                    .fill(p.text)
                    .corner_radius(20)
                    .inner_margin(Margin::symmetric(16, 9))
                    .shadow(egui::epaint::Shadow { offset: [0, 4], blur: 16, spread: 0, color: Color32::from_black_alpha(70) })
                    .show(ui, |ui| {
                        ui.label(bold(&toast.text).color(p.window));
                    });
            });
    }
}

// -------------------------------------------------------------------- small widgets

fn primary_button(ui: &mut egui::Ui, p: &Palette, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(bold(text).color(p.accent_text))
            .fill(p.accent)
            .stroke(Stroke::NONE)
            .corner_radius(CornerRadius::same(9)),
    )
}

/// A small filled circle used as a status light.
fn dot(ui: &mut egui::Ui, colour: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 5.0, colour);
}

/// A rounded label such as "In use".
fn pill(ui: &mut egui::Ui, p: &Palette, text: &str) {
    Frame::new().fill(p.good_soft).corner_radius(20).inner_margin(Margin::symmetric(10, 3)).show(ui, |ui| {
        ui.label(bold(text).size(12.0).color(p.good));
    });
}

/// The square icon at the left of a device row, with a short code for the device type.
fn badge(ui: &mut egui::Ui, p: &Palette, kind: &str, in_use: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
    let (fill, ink) = if in_use { (p.good_soft, p.good) } else { (p.accent_soft, p.accent) };
    ui.painter().rect_filled(rect, CornerRadius::same(10), fill);
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, abbreviation(kind), egui::FontId::proportional(11.5), ink);
}

fn abbreviation(kind: &str) -> &'static str {
    let k = kind.to_ascii_lowercase();
    let has = |s: &str| k.contains(s);
    if has("hid") {
        "HID"
    } else if has("mass") {
        "DISK"
    } else if has("video") || has("imaging") {
        "CAM" // before audio: webcams report video + audio
    } else if has("audio") {
        "AUDIO"
    } else if has("printer") {
        "PRINT"
    } else if has("communications") || has("cdc") {
        "COM"
    } else if has("hub") {
        "HUB"
    } else if has("wireless") {
        "RF"
    } else {
        "USB"
    }
}

#[cfg(test)]
mod tests {
    use super::abbreviation;

    #[test]
    fn picks_a_badge_for_each_kind() {
        assert_eq!(abbreviation("HID"), "HID");
        assert_eq!(abbreviation("HID + Audio"), "HID");
        assert_eq!(abbreviation("Mass Storage"), "DISK");
        assert_eq!(abbreviation("Video + Audio"), "CAM");
        assert_eq!(abbreviation("Audio"), "AUDIO");
        assert_eq!(abbreviation("Vendor Specific"), "USB");
        assert_eq!(abbreviation("Device"), "USB");
    }
}
