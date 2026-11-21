//! The parts of the window layout that survive a restart.

use std::{fs, path::PathBuf};

use sonde_core::Result;

use serde::{Deserialize, Serialize};

/// The user's choice of app appearance.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

/// Window geometry, in logical pixels, and workspace preferences.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Layout {
    pub window_width: f32,
    pub window_height: f32,
    pub sidebar_width: f32,
    pub sidebar_open: bool,
    pub appearance: Appearance,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            window_width: 1200.,
            window_height: 800.,
            sidebar_width: 300.,
            sidebar_open: true,
            appearance: Appearance::default(),
        }
    }
}

impl Layout {
    /// Smallest sizes worth restoring; anything below is a corrupt file or a
    /// window that was mid-collapse when it was saved.
    const MIN_WINDOW: f32 = 480.;
    const MIN_SIDEBAR: f32 = 160.;

    fn sanitized(self) -> Self {
        let default = Self::default();
        let usable = |value: f32, min: f32, fallback: f32| {
            if value.is_finite() && value >= min {
                value
            } else {
                fallback
            }
        };
        Self {
            window_width: usable(self.window_width, Self::MIN_WINDOW, default.window_width),
            window_height: usable(self.window_height, Self::MIN_WINDOW, default.window_height),
            sidebar_width: usable(self.sidebar_width, Self::MIN_SIDEBAR, default.sidebar_width),
            ..self
        }
    }
}

/// The file the layout is kept in.
#[derive(Debug, Clone)]
pub struct LayoutFile(PathBuf);

impl LayoutFile {
    /// `layout.toml` in Sonde's config directory.
    ///
    /// # Errors
    ///
    /// When the system reports no home directory.
    pub fn standard() -> Result<Self> {
        Ok(Self(sonde_core::config::config_dir()?.join("layout.toml")))
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    /// The folder the layout file is in, where Sonde's other files go too.
    pub fn dir(&self) -> PathBuf {
        self.0
            .parent()
            .map_or_else(|| PathBuf::from("."), std::path::Path::to_path_buf)
    }

    /// The saved layout, or the default when there is none or it is unusable.
    /// Layout is a convenience, so a bad file is ignored rather than reported.
    pub fn load(&self) -> Layout {
        fs::read_to_string(&self.0)
            .ok()
            .and_then(|text| toml::from_str::<Layout>(&text).ok())
            .map(Layout::sanitized)
            .unwrap_or_default()
    }

    /// Save the layout. Failure is ignored: losing a window size is not worth
    /// interrupting anyone for.
    pub fn save(&self, layout: Layout) {
        let Ok(text) = toml::to_string(&layout.sanitized()) else {
            return;
        };
        if let Some(dir) = self.0.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let staged = self
            .0
            .with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        if let Err(error) = fs::write(&staged, text).and_then(|()| fs::rename(&staged, &self.0)) {
            let _ = fs::remove_file(staged);
            eprintln!("sonde: could not save {}: {error}", self.0.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use core::prelude::v1::test;

    use super::*;

    #[test]
    fn sanitized_should_replace_a_collapsed_sidebar_width() {
        let layout = Layout {
            sidebar_width: 3.,
            ..Layout::default()
        };

        assert_eq!(
            layout.sanitized().sidebar_width,
            Layout::default().sidebar_width
        );
    }

    #[test]
    fn sanitized_should_replace_a_non_finite_window_size() {
        let layout = Layout {
            window_width: f32::NAN,
            ..Layout::default()
        };

        assert_eq!(
            layout.sanitized().window_width,
            Layout::default().window_width
        );
    }

    #[test]
    fn sanitized_should_keep_a_reasonable_layout() {
        let layout = Layout {
            window_width: 1600.,
            window_height: 900.,
            sidebar_width: 320.,
            sidebar_open: false,
            appearance: Appearance::Dark,
        };

        assert_eq!(layout.sanitized(), layout);
    }
}
