//! Small user settings kept in the config folder (next to the saved server list).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use eframe::egui::ThemePreference;

/// Which colour scheme to use.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ThemeChoice {
    /// Follow Windows' light/dark setting.
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    pub const ALL: [ThemeChoice; 3] = [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark];

    pub fn label(self) -> &'static str {
        match self {
            ThemeChoice::System => "System default",
            ThemeChoice::Light => "Light",
            ThemeChoice::Dark => "Dark",
        }
    }

    fn key(self) -> &'static str {
        match self {
            ThemeChoice::System => "system",
            ThemeChoice::Light => "light",
            ThemeChoice::Dark => "dark",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.key().eq_ignore_ascii_case(text.trim()))
    }

    pub fn preference(self) -> ThemePreference {
        match self {
            ThemeChoice::System => ThemePreference::System,
            ThemeChoice::Light => ThemePreference::Light,
            ThemeChoice::Dark => ThemePreference::Dark,
        }
    }
}

pub fn theme_file() -> PathBuf {
    usbip_client::config_dir().join("theme.txt")
}

/// The saved choice; a missing or unreadable file means "System default".
pub fn load_theme(path: &Path) -> ThemeChoice {
    fs::read_to_string(path).ok().and_then(|t| ThemeChoice::parse(&t)).unwrap_or_default()
}

pub fn save_theme(path: &Path, choice: ThemeChoice) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, choice.key())
}

// ------------------------------------------------------------------ start minimized

pub fn start_minimized_file() -> PathBuf {
    usbip_client::config_dir().join("start-minimized.txt")
}

/// The saved "Start minimized" choice. `None` means the file does not exist yet: this is the very
/// first run on this PC.
pub fn load_start_minimized(path: &Path) -> Option<bool> {
    let text = fs::read_to_string(path).ok()?;
    Some(!text.trim().eq_ignore_ascii_case("off"))
}

pub fn save_start_minimized(path: &Path, on: bool) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, if on { "on" } else { "off" })
}

/// Should the window start hidden in the tray?
/// * `forced`: started with `--minimized`;
/// * `saved`: the saved setting, `None` on the very first run (the window is shown then, so a new
///   user sees what they installed).
pub fn should_start_hidden(forced: bool, saved: Option<bool>) -> bool {
    forced || saved == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_loosely() {
        assert_eq!(ThemeChoice::parse("dark"), Some(ThemeChoice::Dark));
        assert_eq!(ThemeChoice::parse(" Light\n"), Some(ThemeChoice::Light));
        assert_eq!(ThemeChoice::parse("system"), Some(ThemeChoice::System));
        assert_eq!(ThemeChoice::parse("purple"), None);
    }

    #[test]
    fn first_run_shows_the_window_then_the_default_is_minimized() {
        let dir = std::env::temp_dir().join(format!("uon-minimized-{}", std::process::id()));
        let file = dir.join("start-minimized.txt");
        let _ = fs::remove_dir_all(&dir);

        // Very first run: no file yet -> window shown, and the default ("on") is saved for next time.
        let saved = load_start_minimized(&file);
        assert_eq!(saved, None);
        assert!(!should_start_hidden(false, saved));
        save_start_minimized(&file, true).unwrap();

        // Every later run: hidden by default.
        assert_eq!(load_start_minimized(&file), Some(true));
        assert!(should_start_hidden(false, load_start_minimized(&file)));

        // The user turned it off: window shown. The flag still forces a hidden start.
        save_start_minimized(&file, false).unwrap();
        assert_eq!(load_start_minimized(&file), Some(false));
        assert!(!should_start_hidden(false, Some(false)));
        assert!(should_start_hidden(true, Some(false)));
        assert!(should_start_hidden(true, None));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn saves_and_loads() {
        let dir = std::env::temp_dir().join(format!("uon-theme-{}", std::process::id()));
        let file = dir.join("theme.txt");
        assert_eq!(load_theme(&file), ThemeChoice::System); // missing file
        save_theme(&file, ThemeChoice::Dark).unwrap();
        assert_eq!(load_theme(&file), ThemeChoice::Dark);
        fs::write(&file, "garbage").unwrap();
        assert_eq!(load_theme(&file), ThemeChoice::System);
        let _ = fs::remove_dir_all(dir);
    }
}
