//! Saved connections and where their passwords live.
//!
//! Connection details go in a TOML file in the user's config directory.
//! Passwords never do: they are kept in the operating system's keyring, keyed
//! by the connection's id.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::{
    driver::{ConnectionConfig, Secret},
    error::{Error, Result},
};

/// The keyring service name every Sonde password is filed under.
const KEYRING_SERVICE: &str = "sonde";

/// A connection the user has saved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedConnection {
    /// Stable identifier. It names the keyring entry, so renaming a
    /// connection does not lose its password.
    pub id: String,
    /// The name shown in the connection list.
    pub name: String,
    /// What to connect to.
    #[serde(flatten)]
    pub config: ConnectionConfig,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ConnectionsFile {
    #[serde(default, rename = "connection")]
    connections: Vec<SavedConnection>,
}

/// The directory Sonde keeps its files in: `~/.config/sonde` on Linux.
///
/// # Errors
///
/// [`Error::Config`] when the system reports no home directory.
pub fn config_dir() -> Result<PathBuf> {
    ProjectDirs::from("", "", "sonde")
        .map(|dirs| dirs.config_dir().to_path_buf())
        .ok_or_else(|| Error::Config("no home directory to keep settings in".into()))
}

/// The file of saved connections.
#[derive(Debug, Clone)]
pub struct ConnectionStore {
    path: PathBuf,
}

impl ConnectionStore {
    /// The store at its standard place, `connections.toml` in
    /// [`config_dir`].
    ///
    /// # Errors
    ///
    /// See [`config_dir`].
    pub fn standard() -> Result<Self> {
        Ok(Self::at(config_dir()?.join("connections.toml")))
    }

    /// A store backed by the file at `path`.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The file this store reads and writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the saved connections. A file that does not exist yet is an empty
    /// list.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the file cannot be read or is not valid.
    pub fn load(&self) -> Result<Vec<SavedConnection>> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(self.failure("read", &error)),
        };
        toml::from_str::<ConnectionsFile>(&text)
            .map(|file| file.connections)
            .map_err(|error| self.failure("parse", &error))
    }

    /// Replace the saved connections.
    ///
    /// The new contents are written beside the file and moved over it, so an
    /// interrupted save leaves the previous list intact.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the file cannot be written.
    pub fn save(&self, connections: &[SavedConnection]) -> Result<()> {
        let file = ConnectionsFile {
            connections: connections.to_vec(),
        };
        let text = toml::to_string_pretty(&file).map_err(|error| self.failure("encode", &error))?;
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir).map_err(|error| self.failure("create a folder for", &error))?;
        }
        let staged = self.path.with_extension("toml.tmp");
        fs::write(&staged, text)
            .and_then(|()| fs::rename(&staged, &self.path))
            .map_err(|error| self.failure("write", &error))
    }

    fn failure(&self, action: &str, error: &dyn std::fmt::Display) -> Error {
        Error::Config(format!(
            "could not {action} {}: {error}",
            self.path.display()
        ))
    }
}

/// Somewhere passwords can be kept.
pub trait SecretStore: Send + Sync {
    /// The password saved for a connection, if there is one.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the store cannot be reached.
    fn get(&self, connection_id: &str) -> Result<Option<Secret>>;

    /// Save a connection's password, replacing any earlier one.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the store cannot be reached.
    fn set(&self, connection_id: &str, secret: &Secret) -> Result<()>;

    /// Forget a connection's password. Forgetting one that was never saved
    /// succeeds.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the store cannot be reached.
    fn delete(&self, connection_id: &str) -> Result<()>;
}

/// The operating system's keyring: the Secret Service on Linux.
#[derive(Debug, Clone, Copy)]
pub struct Keyring;

impl Keyring {
    /// The keyring, if this session has one.
    ///
    /// Returns `None` when no Secret Service is running. The caller should
    /// then ask for passwords each time rather than write them anywhere else.
    pub fn open() -> Option<Self> {
        keyring::Entry::store_status().is_ok().then_some(Self)
    }

    fn entry(connection_id: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(KEYRING_SERVICE, connection_id).map_err(keyring_failure)
    }
}

fn keyring_failure(error: keyring::Error) -> Error {
    Error::Config(format!("keyring: {error}"))
}

impl SecretStore for Keyring {
    fn get(&self, connection_id: &str) -> Result<Option<Secret>> {
        match Self::entry(connection_id)?.get_password() {
            Ok(password) => Ok(Some(Secret::new(password))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(keyring_failure(error)),
        }
    }

    fn set(&self, connection_id: &str, secret: &Secret) -> Result<()> {
        Self::entry(connection_id)?
            .set_password(secret.expose())
            .map_err(keyring_failure)
    }

    fn delete(&self, connection_id: &str) -> Result<()> {
        match Self::entry(connection_id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(keyring_failure(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{PostgresConfig, SqliteConfig, TlsMode};

    fn scratch(name: &str) -> ConnectionStore {
        let dir = std::env::temp_dir().join(format!("sonde-config-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        ConnectionStore::at(dir.join("nested").join("connections.toml"))
    }

    fn samples() -> Vec<SavedConnection> {
        vec![
            SavedConnection {
                id: "a1".into(),
                name: "Local Postgres".into(),
                config: ConnectionConfig::Postgres(PostgresConfig {
                    host: "127.0.0.1".into(),
                    port: 5432,
                    database: "app".into(),
                    user: "me".into(),
                    tls: TlsMode::VerifyFull,
                    options: Default::default(),
                }),
            },
            SavedConnection {
                id: "b2".into(),
                name: "Notes".into(),
                config: ConnectionConfig::Sqlite(SqliteConfig {
                    path: "/tmp/notes.db".into(),
                    read_only: true,
                }),
            },
        ]
    }

    #[test]
    fn load_should_return_nothing_when_the_file_is_missing() {
        assert_eq!(scratch("missing").load(), Ok(Vec::new()));
    }

    #[test]
    fn save_should_round_trip_both_kinds_of_connection() {
        let store = scratch("roundtrip");

        store.save(&samples()).unwrap();

        assert_eq!(store.load(), Ok(samples()));
    }

    #[test]
    fn save_should_write_readable_toml() {
        let store = scratch("format");

        store.save(&samples()[..1]).unwrap();

        let text = fs::read_to_string(store.path()).unwrap();
        assert_eq!(
            text,
            "[[connection]]\nid = \"a1\"\nname = \"Local Postgres\"\nkind = \"postgres\"\n\
             host = \"127.0.0.1\"\nport = 5432\ndatabase = \"app\"\nuser = \"me\"\n\
             tls = \"verify-full\"\n"
        );
    }

    #[test]
    fn save_should_never_write_a_password_field() {
        let store = scratch("nopassword");

        store.save(&samples()).unwrap();

        let text = fs::read_to_string(store.path()).unwrap();
        assert!(!text.contains("password") && !text.contains("secret"));
    }

    #[test]
    fn load_should_report_a_malformed_file() {
        let store = scratch("malformed");
        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        fs::write(store.path(), "[[connection]]\nid = 3\n").unwrap();

        assert!(matches!(store.load(), Err(Error::Config(_))));
    }
}
