//! The saved connections, as application state.

mod engines;
mod form;
mod password;
pub(crate) mod setup;
mod url;

use std::sync::Arc;

use gpui_kit::{Context, EventEmitter, SharedString};
use sonde_core::{
    Result,
    config::{ConnectionStore, SavedConnection, SecretStore},
};

pub use form::open_connection_form;

/// Separates a connection's id from one of its databases' names in the id
/// of a database listed under it. Not a character a saved id contains.
const DATABASE_SEP: char = '\u{1e}';

/// The id of database `name` listed under saved connection `parent`.
pub fn database_id(parent: &str, name: &str) -> String {
    format!("{parent}{DATABASE_SEP}{name}")
}

/// The saved connection an id belongs to, which also names its keyring
/// entry: the id itself, or the parent of a listed database.
pub fn keyring_id(id: &str) -> &str {
    id.split(DATABASE_SEP).next().unwrap_or(id)
}

/// The database a listed-database id names, if it is one.
pub fn database_of(id: &str) -> Option<&str> {
    id.split_once(DATABASE_SEP).map(|(_, name)| name)
}
pub use password::open_password_prompt;

/// Emitted whenever the list of saved connections changes.
pub struct ConnectionsChanged;

/// The saved connections and the stores behind them.
pub struct Connections {
    store: ConnectionStore,
    secrets: Option<Arc<dyn SecretStore>>,
    list: Vec<SavedConnection>,
    load_error: Option<SharedString>,
}

impl EventEmitter<ConnectionsChanged> for Connections {}

impl Connections {
    /// Read the saved connections. A file that cannot be read leaves the list
    /// empty and the reason in [`Self::load_error`], so the user sees why
    /// rather than an apparently fresh install.
    pub fn load(store: ConnectionStore, secrets: Option<Arc<dyn SecretStore>>) -> Self {
        let (list, load_error) = match store.load() {
            Ok(list) => (list, None),
            Err(error) => (Vec::new(), Some(error.to_string().into())),
        };
        Self {
            store,
            secrets,
            list,
            load_error,
        }
    }

    pub fn list(&self) -> &[SavedConnection] {
        &self.list
    }

    pub fn get(&self, id: &str) -> Option<&SavedConnection> {
        self.list.iter().find(|connection| connection.id == id)
    }

    /// Why the connections file could not be read at startup, if it could not.
    pub fn load_error(&self) -> Option<&SharedString> {
        self.load_error.as_ref()
    }

    /// The keyring, or `None` when this session has none and passwords must
    /// be asked for each time.
    pub fn secrets(&self) -> Option<Arc<dyn SecretStore>> {
        self.secrets.clone()
    }

    /// Add a connection, or replace the one with the same id, and write the
    /// file. The list is left as it was if the write fails.
    pub fn upsert(&mut self, connection: SavedConnection, cx: &mut Context<Self>) -> Result<()> {
        let mut next = self.list.clone();
        match next.iter_mut().find(|saved| saved.id == connection.id) {
            Some(saved) => *saved = connection,
            None => next.push(connection),
        }
        self.replace(next, cx)
    }

    /// Remove a connection and write the file.
    pub fn remove(&mut self, id: &str, cx: &mut Context<Self>) -> Result<()> {
        let next = self
            .list
            .iter()
            .filter(|saved| saved.id != id)
            .cloned()
            .collect();
        self.replace(next, cx)
    }

    fn replace(&mut self, next: Vec<SavedConnection>, cx: &mut Context<Self>) -> Result<()> {
        self.store.save(&next)?;
        self.list = next;
        // A successful save supersedes whatever was wrong with the old file.
        self.load_error = None;
        cx.emit(ConnectionsChanged);
        cx.notify();
        Ok(())
    }
}
