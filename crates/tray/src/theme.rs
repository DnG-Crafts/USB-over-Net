//! Look and feel: colours, spacing, rounded corners and fonts. Follows the system light/dark setting.

use eframe::egui::{
    self, epaint::Shadow, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Stroke,
    TextStyle, Theme,
};

/// Colours that are not part of egui's own `Visuals`.
#[derive(Clone, Copy)]
pub struct Palette {
    pub window: Color32,
    pub card: Color32,
    pub card_stroke: Color32,
    pub row: Color32,
    pub row_hover: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub accent_text: Color32,
    pub accent_soft: Color32,
    pub good: Color32,
    pub good_soft: Color32,
    pub bad: Color32,
    pub bad_soft: Color32,
    pub warn: Color32,
}

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            window: Color32::from_rgb(0x12, 0x15, 0x1a),
            card: Color32::from_rgb(0x1b, 0x1f, 0x26),
            card_stroke: Color32::from_rgb(0x2b, 0x31, 0x3b),
            row: Color32::from_rgb(0x22, 0x27, 0x30),
            row_hover: Color32::from_rgb(0x28, 0x2e, 0x38),
            text: Color32::from_rgb(0xe8, 0xeb, 0xf0),
            muted: Color32::from_rgb(0x8d, 0x96, 0xa5),
            accent: Color32::from_rgb(0x3b, 0x82, 0xf6),
            accent_text: Color32::WHITE,
            accent_soft: Color32::from_rgba_unmultiplied(0x3b, 0x82, 0xf6, 0x2e),
            good: Color32::from_rgb(0x34, 0xd3, 0x99),
            good_soft: Color32::from_rgba_unmultiplied(0x34, 0xd3, 0x99, 0x26),
            bad: Color32::from_rgb(0xf8, 0x71, 0x71),
            bad_soft: Color32::from_rgba_unmultiplied(0xf8, 0x71, 0x71, 0x26),
            warn: Color32::from_rgb(0xfb, 0xbf, 0x24),
        }
    } else {
        Palette {
            window: Color32::from_rgb(0xf3, 0xf5, 0xf8),
            card: Color32::WHITE,
            card_stroke: Color32::from_rgb(0xe2, 0xe6, 0xec),
            row: Color32::from_rgb(0xf5, 0xf7, 0xfa),
            row_hover: Color32::from_rgb(0xeb, 0xef, 0xf5),
            text: Color32::from_rgb(0x1a, 0x22, 0x30),
            muted: Color32::from_rgb(0x6b, 0x75, 0x85),
            accent: Color32::from_rgb(0x25, 0x63, 0xeb),
            accent_text: Color32::WHITE,
            accent_soft: Color32::from_rgba_unmultiplied(0x25, 0x63, 0xeb, 0x1a),
            good: Color32::from_rgb(0x05, 0x96, 0x69),
            good_soft: Color32::from_rgba_unmultiplied(0x05, 0x96, 0x69, 0x1a),
            bad: Color32::from_rgb(0xdc, 0x26, 0x26),
            bad_soft: Color32::from_rgba_unmultiplied(0xdc, 0x26, 0x26, 0x14),
            warn: Color32::from_rgb(0xd9, 0x77, 0x06),
        }
    }
}

/// Install fonts and styles for both themes.
pub fn apply(ctx: &egui::Context) {
    ctx.set_fonts(fonts());
    for (theme, dark) in [(Theme::Dark, true), (Theme::Light, false)] {
        let p = palette(dark);
        ctx.style_mut_of(theme, |style| {
            style.spacing.item_spacing = egui::vec2(10.0, 8.0);
            style.spacing.button_padding = egui::vec2(14.0, 7.0);
            style.spacing.window_margin = Margin::same(18);
            style.spacing.interact_size.y = 32.0;

            style.text_styles = [
                (TextStyle::Heading, FontId::new(22.0, FontFamily::Proportional)),
                (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
                (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
                (TextStyle::Small, FontId::new(12.0, FontFamily::Proportional)),
                (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
            ]
            .into();

            let v = &mut style.visuals;
            v.dark_mode = dark;
            v.panel_fill = p.window;
            v.window_fill = p.card;
            v.extreme_bg_color = if dark { Color32::from_rgb(0x0d, 0x10, 0x14) } else { Color32::WHITE };
            v.override_text_color = Some(p.text);
            v.hyperlink_color = p.accent;
            v.selection.bg_fill = p.accent_soft;
            v.selection.stroke = Stroke::new(1.0, p.accent);
            v.window_corner_radius = CornerRadius::same(14);
            v.window_stroke = Stroke::new(1.0, p.card_stroke);
            v.window_shadow = Shadow { offset: [0, 8], blur: 28, spread: 0, color: Color32::from_black_alpha(60) };
            v.popup_shadow = v.window_shadow;

            let radius = CornerRadius::same(9);
            for w in [
                &mut v.widgets.noninteractive,
                &mut v.widgets.inactive,
                &mut v.widgets.hovered,
                &mut v.widgets.active,
                &mut v.widgets.open,
            ] {
                w.corner_radius = radius;
            }
            v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.card_stroke);
            v.widgets.inactive.bg_fill = p.row;
            v.widgets.inactive.weak_bg_fill = p.row;
            v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.card_stroke);
            v.widgets.hovered.bg_fill = p.row_hover;
            v.widgets.hovered.weak_bg_fill = p.row_hover;
            v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.muted);
            v.widgets.active.bg_fill = p.row_hover;
            v.widgets.active.weak_bg_fill = p.row_hover;
        });
    }
}

/// Bold-ish text for titles and buttons. egui has no real bold, so this is a second font family:
/// Segoe UI Semibold on Windows, the regular font elsewhere.
pub fn bold(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text).family(FontFamily::Name(SEMIBOLD.into()))
}

const SEMIBOLD: &str = "semibold";

/// Use Windows' own UI font when it is there, so text looks native; otherwise egui's built-in one.
fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    let load = |fonts: &mut FontDefinitions, name: &str, path: &str| -> bool {
        match std::fs::read(path) {
            Ok(bytes) => {
                fonts.font_data.insert(name.to_owned(), std::sync::Arc::new(FontData::from_owned(bytes)));
                true
            }
            Err(_) => false,
        }
    };
    if load(&mut fonts, "segoe-ui", r"C:\Windows\Fonts\segoeui.ttf") {
        fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "segoe-ui".to_owned());
    }
    let mut semibold = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    if load(&mut fonts, "segoe-ui-semibold", r"C:\Windows\Fonts\seguisb.ttf") {
        semibold.insert(0, "segoe-ui-semibold".to_owned());
    }
    fonts.families.insert(FontFamily::Name(SEMIBOLD.into()), semibold);
    fonts
}
