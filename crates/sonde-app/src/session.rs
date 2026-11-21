//! `session.toml`: the query tabs open when Sonde last closed, so the next
//! start can reopen them with their text.

use std::{fs, path::PathBuf};

use serde::{Deserialize, Serialize};

/// One query tab as it was left.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedTab {
    /// The saved connection the tab ran on.
    pub connection_id: String,
    /// The editor's text.
    pub sql: String,
}

/// The tabs of one session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSession {
    #[serde(default, rename = "tab")]
    pub tabs: Vec<SavedTab>,
    /// Which of `tabs` was on screen.
    #[serde(default)]
    pub active: Option<usize>,
}

/// The file a session is kept in.
#[derive(Debug, Clone)]
pub struct SessionFile(PathBuf);

impl SessionFile {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    /// The last session, or an empty one when there is none or it cannot be
    /// read. Like the layout, it is a convenience and never worth an error.
    pub fn load(&self) -> SavedSession {
        fs::read_to_string(&self.0)
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Save `session`, replacing the last one.
    pub fn save(&self, session: &SavedSession) {
        let Ok(text) = toml::to_string_pretty(session) else {
            return;
        };
        if let Some(dir) = self.0.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let staged = self.0.with_extension("toml.tmp");
        if let Err(error) = fs::write(&staged, text).and_then(|()| fs::rename(&staged, &self.0)) {
            let _ = fs::remove_file(staged);
            eprintln!("sonde: could not save {}: {error}", self.0.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_should_round_trip_multiline_sql() {
        let dir = std::env::temp_dir().join(format!("sonde-session-{}", std::process::id()));
        let file = SessionFile::at(dir.join("session.toml"));
        let session = SavedSession {
            tabs: vec![
                SavedTab {
                    connection_id: "a".into(),
                    sql: "select 1;\n-- \"quoted\" 'text'\nselect 2".into(),
                },
                SavedTab {
                    connection_id: "b".into(),
                    sql: String::new(),
                },
            ],
            active: Some(1),
        };

        file.save(&session);

        assert_eq!(file.load(), session);
    }

    #[test]
    fn load_should_ignore_a_broken_file() {
        let dir = std::env::temp_dir().join(format!("sonde-session-bad-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("session.toml"), "tab = 3").unwrap();

        assert_eq!(
            SessionFile::at(dir.join("session.toml")).load(),
            SavedSession::default()
        );
    }
}
