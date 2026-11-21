//! Query history: every statement run, per saved connection.
//!
//! Each connection has its own file of JSON lines, newest last, in Sonde's
//! data directory (`~/.local/share/sonde/history` on Linux). Appending a line
//! is cheap and a torn last line, from a crash mid-write, costs only that
//! entry.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write as _},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Entries kept per connection. Older ones are dropped when the file is
/// compacted.
pub const KEEP: usize = 1_000;

/// One statement that was run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The statement's text.
    pub sql: String,
    /// When it started, in milliseconds since the Unix epoch.
    pub at_ms: u64,
    /// How long it ran, in milliseconds. `None` when it never finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    /// Rows returned or affected, when it succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u64>,
    /// The database's error, when it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Entry {
    /// An entry for `sql` starting now.
    pub fn now(sql: impl Into<String>) -> Self {
        let at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            });
        Self {
            sql: sql.into(),
            at_ms,
            elapsed_ms: None,
            rows: None,
            error: None,
        }
    }

    /// Record that it finished after `elapsed` with `rows` rows.
    #[must_use]
    pub fn finished(mut self, elapsed: Duration, rows: Option<u64>) -> Self {
        self.elapsed_ms = Some(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX));
        self.rows = rows;
        self
    }

    /// Record that it failed with `error`.
    #[must_use]
    pub fn failed(mut self, error: impl Into<String>) -> Self {
        self.error = Some(error.into());
        self
    }
}

/// Whether the file at `path` exists and its last byte is not a newline.
fn ends_mid_line(path: &Path) -> io::Result<bool> {
    use std::io::{Read as _, Seek as _, SeekFrom};

    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if file.metadata()?.len() == 0 {
        return Ok(false);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0];
    file.read_exact(&mut last)?;
    Ok(last[0] != b'\n')
}

/// Where history files are kept.
#[derive(Debug, Clone)]
pub struct History {
    dir: PathBuf,
}

impl History {
    /// History in Sonde's data directory.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the system reports no home directory.
    pub fn standard() -> Result<Self> {
        ProjectDirs::from("", "", "sonde")
            .map(|dirs| Self::at(dirs.data_dir().join("history")))
            .ok_or_else(|| Error::Config("no home directory to keep history in".into()))
    }

    /// History kept in `dir`.
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The folder history files live in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn file(&self, connection_id: &str) -> PathBuf {
        // Ids are UUIDs in practice; anything else is made safe for a file
        // name rather than trusted.
        let name: String = connection_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.dir.join(format!("{name}.jsonl"))
    }

    /// Add an entry to a connection's history.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the file cannot be written.
    pub fn append(&self, connection_id: &str, entry: &Entry) -> Result<()> {
        let path = self.file(connection_id);
        let failure = |error: &dyn std::fmt::Display| {
            Error::Config(format!("could not write {}: {error}", path.display()))
        };
        fs::create_dir_all(&self.dir).map_err(|error| failure(&error))?;
        let mut line = serde_json::to_string(entry).map_err(|error| failure(&error))?;
        line.push('\n');
        // A line torn by a crash must not swallow this one.
        if ends_mid_line(&path).map_err(|error| failure(&error))? {
            line.insert(0, '\n');
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut file| file.write_all(line.as_bytes()))
            .map_err(|error| failure(&error))?;
        Ok(())
    }

    /// A connection's history, newest first, at most [`KEEP`] entries. Lines
    /// that cannot be read are skipped. When the file has grown well past
    /// [`KEEP`], it is rewritten with only the entries returned.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the file exists but cannot be read.
    pub fn load(&self, connection_id: &str) -> Result<Vec<Entry>> {
        let path = self.file(connection_id);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(Error::Config(format!(
                    "could not read {}: {error}",
                    path.display()
                )));
            }
        };
        let lines = text.lines().count();
        let mut entries: Vec<Entry> = text
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let excess = entries.len().saturating_sub(KEEP);
        entries.drain(..excess);
        if lines > KEEP * 2 {
            // Compacting is housekeeping; failing at it loses nothing.
            let _ = self.rewrite(&path, &entries);
        }
        entries.reverse();
        Ok(entries)
    }

    fn rewrite(&self, path: &Path, oldest_first: &[Entry]) -> io::Result<()> {
        let mut text = String::new();
        for entry in oldest_first {
            text.push_str(&serde_json::to_string(entry)?);
            text.push('\n');
        }
        let staged = path.with_extension("jsonl.tmp");
        fs::write(&staged, text)?;
        fs::rename(staged, path)
    }

    /// Forget a connection's history.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the file exists but cannot be removed.
    pub fn clear(&self, connection_id: &str) -> Result<()> {
        let path = self.file(connection_id);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::Config(format!(
                "could not remove {}: {error}",
                path.display()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> History {
        let dir = std::env::temp_dir().join(format!("sonde-history-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        History::at(dir)
    }

    #[test]
    fn load_should_return_newest_first() {
        let history = scratch("order");
        history.append("c1", &Entry::now("select 1")).unwrap();
        history
            .append(
                "c1",
                &Entry::now("select 2").finished(Duration::from_millis(12), Some(1)),
            )
            .unwrap();

        let entries = history.load("c1").unwrap();

        assert_eq!(
            entries.iter().map(|e| e.sql.as_str()).collect::<Vec<_>>(),
            ["select 2", "select 1"]
        );
        assert_eq!(entries[0].elapsed_ms, Some(12));
        assert_eq!(entries[0].rows, Some(1));
    }

    #[test]
    fn load_should_keep_connections_apart() {
        let history = scratch("apart");
        history.append("c1", &Entry::now("one")).unwrap();
        history.append("c2", &Entry::now("two")).unwrap();

        assert_eq!(history.load("c2").unwrap()[0].sql, "two");
        assert_eq!(history.load("c1").unwrap().len(), 1);
    }

    #[test]
    fn load_should_skip_a_torn_line() {
        let history = scratch("torn");
        history.append("c1", &Entry::now("kept")).unwrap();
        let path = history.file("c1");
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str("{\"sql\":\"cut off");
        fs::write(&path, text).unwrap();
        history.append("c1", &Entry::now("after")).unwrap();

        let sqls: Vec<String> = history
            .load("c1")
            .unwrap()
            .into_iter()
            .map(|entry| entry.sql)
            .collect();

        assert_eq!(sqls, ["after", "kept"]);
    }

    #[test]
    fn load_should_cap_and_compact() {
        let history = scratch("compact");
        for n in 0..(KEEP * 2 + 5) {
            history
                .append("c1", &Entry::now(format!("select {n}")))
                .unwrap();
        }

        let entries = history.load("c1").unwrap();

        assert_eq!(entries.len(), KEEP);
        assert_eq!(entries[0].sql, format!("select {}", KEEP * 2 + 4));
        let lines = fs::read_to_string(history.file("c1"))
            .unwrap()
            .lines()
            .count();
        assert_eq!(lines, KEEP);
    }

    #[test]
    fn file_should_not_escape_the_history_folder() {
        let history = scratch("escape");

        let path = history.file("../../etc/passwd");

        assert_eq!(path.parent(), Some(history.dir()));
    }

    #[test]
    fn clear_should_forget_and_tolerate_a_missing_file() {
        let history = scratch("clear");
        history.append("c1", &Entry::now("x")).unwrap();

        history.clear("c1").unwrap();
        history.clear("c1").unwrap();

        assert!(history.load("c1").unwrap().is_empty());
    }
}
