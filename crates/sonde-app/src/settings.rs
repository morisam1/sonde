//! `settings.toml`: the preferences that have no control in the window.
//!
//! The theme is chosen in the Settings panel and saved with the layout; this
//! file holds the rest. It is written with its defaults, commented, the first
//! time Sonde starts, so there is something to edit.

use std::{fs, io, path::PathBuf};

use gpui_kit::{App, Global};
use serde::{Deserialize, Serialize};
use sonde_core::PAGE_ROWS;

/// Everything `settings.toml` can set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Font family of the SQL editor and the result grids.
    pub editor_font: String,
    /// Font size of the SQL editor and the result grids, in pixels.
    pub editor_font_size: f32,
    /// Rows a result or table loads before waiting for the user to scroll.
    pub page_size: usize,
    /// Most rows one query result keeps. Rows past it are not fetched.
    pub row_cap: usize,
    /// Reopen the query tabs of the last session on start.
    pub restore_tabs: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            editor_font: crate::design::MONO_FONT.to_owned(),
            editor_font_size: 14.,
            page_size: 10_000,
            row_cap: 1_000_000,
            restore_tabs: true,
        }
    }
}

impl Global for Settings {}

impl Settings {
    /// The settings in effect, or the defaults when none were installed.
    pub fn get(cx: &App) -> &Self {
        static DEFAULT: std::sync::OnceLock<Settings> = std::sync::OnceLock::new();
        cx.try_global::<Self>()
            .unwrap_or_else(|| DEFAULT.get_or_init(Self::default))
    }

    /// Pages of [`PAGE_ROWS`] that make up [`Settings::page_size`], at least
    /// one.
    pub fn eager_pages(&self) -> usize {
        self.page_size.div_ceil(PAGE_ROWS).max(1)
    }

    /// Replace values no one could mean with the defaults.
    fn sanitized(self) -> Self {
        let default = Self::default();
        Self {
            editor_font: if self.editor_font.trim().is_empty() {
                default.editor_font
            } else {
                self.editor_font
            },
            editor_font_size: if self.editor_font_size.is_finite()
                && (6.0..=72.0).contains(&self.editor_font_size)
            {
                self.editor_font_size
            } else {
                default.editor_font_size
            },
            page_size: if self.page_size == 0 {
                default.page_size
            } else {
                self.page_size
            },
            row_cap: if self.row_cap == 0 {
                default.row_cap
            } else {
                self.row_cap.max(self.page_size)
            },
            restore_tabs: self.restore_tabs,
        }
    }
}

/// What reading `settings.toml` came to.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub settings: Settings,
    /// Why the file was not used, when it was there but unreadable.
    pub problem: Option<String>,
}

/// The file the settings are kept in.
#[derive(Debug, Clone)]
pub struct SettingsFile(PathBuf);

impl SettingsFile {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    pub fn path(&self) -> &std::path::Path {
        &self.0
    }

    /// The settings, writing a commented file of defaults when there is
    /// none. A file that cannot be read leaves the defaults in effect and
    /// says why.
    pub fn load(&self) -> Loaded {
        match fs::read_to_string(&self.0) {
            Ok(text) => match toml::from_str::<Settings>(&text) {
                Ok(settings) => Loaded {
                    settings: settings.sanitized(),
                    problem: None,
                },
                Err(error) => Loaded {
                    settings: Settings::default(),
                    problem: Some(format!(
                        "{} could not be read, so the defaults are in use: {}",
                        self.0.display(),
                        error.message()
                    )),
                },
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // Writing the template is a courtesy; failing at it changes
                // nothing.
                if let Some(dir) = self.0.parent() {
                    let _ = fs::create_dir_all(dir);
                }
                let _ = fs::write(&self.0, TEMPLATE);
                Loaded {
                    settings: Settings::default(),
                    problem: None,
                }
            }
            Err(error) => Loaded {
                settings: Settings::default(),
                problem: Some(format!("could not read {}: {error}", self.0.display())),
            },
        }
    }
}

const TEMPLATE: &str = "\
# Sonde settings. Changes apply the next time Sonde starts.
# The theme is chosen in the app, under Settings.

# Font of the SQL editor and the result grids.
editor_font = \"Geist Mono\"
editor_font_size = 14

# Rows a result or table loads before waiting for you to scroll.
page_size = 10000

# Most rows one query result keeps.
row_cap = 1000000

# Reopen the last session's query tabs on start.
restore_tabs = true
";

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> SettingsFile {
        let dir =
            std::env::temp_dir().join(format!("sonde-settings-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        SettingsFile::at(dir.join("settings.toml"))
    }

    #[test]
    fn load_should_write_the_template_on_first_run_and_read_it_back() {
        let file = scratch("first");

        let first = file.load();
        let second = file.load();

        assert_eq!(first.settings, Settings::default());
        assert!(file.path().exists());
        assert_eq!(second, first);
    }

    #[test]
    fn load_should_keep_defaults_for_missing_keys() {
        let file = scratch("partial");
        fs::create_dir_all(file.path().parent().unwrap()).unwrap();
        fs::write(file.path(), "page_size = 500\n").unwrap();

        let loaded = file.load().settings;

        assert_eq!(loaded.page_size, 500);
        assert_eq!(loaded.eager_pages(), 1);
        assert_eq!(loaded.row_cap, Settings::default().row_cap);
    }

    #[test]
    fn load_should_report_a_broken_file_and_use_defaults() {
        let file = scratch("broken");
        fs::create_dir_all(file.path().parent().unwrap()).unwrap();
        fs::write(file.path(), "page_size = \"lots\"\n").unwrap();

        let loaded = file.load();

        assert_eq!(loaded.settings, Settings::default());
        assert!(loaded.problem.is_some_and(|p| p.contains("defaults")));
    }

    #[test]
    fn sanitized_should_refuse_nonsense() {
        let settings = Settings {
            editor_font: " ".into(),
            editor_font_size: 0.,
            page_size: 0,
            row_cap: 0,
            restore_tabs: false,
        }
        .sanitized();

        assert_eq!(
            settings,
            Settings {
                restore_tabs: false,
                ..Settings::default()
            }
        );
    }
}
