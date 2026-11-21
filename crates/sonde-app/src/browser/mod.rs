//! The sidebar: saved connections, and under each connected one its schemas,
//! tables, columns, indexes and foreign keys.

mod manage;
mod rows;

use sonde_core::schema::Schema;

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::Arc,
};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    list::ListItem,
    menu::PopupMenu,
    tree::{TreeEvent, TreeItem, TreeState, tree},
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use sonde_core::{
    Catalog, Result, Secret, Session, TableDetail, TableRef, config::SavedConnection,
};

use self::rows::{Kind, Row, Target, Tone, id};
use crate::{
    connections::{Connections, ConnectionsChanged, open_connection_form, open_password_prompt},
    design,
    query::QueryTarget,
    ui::{self, Glyph},
};

/// The sidebar, for code that changes structure elsewhere (a query tab) to
/// tell it.
struct Sidebar(WeakEntity<BrowserPanel>);

impl Global for Sidebar {}

/// Tell the sidebar a statement on `connection` changed its structure, so
/// it reloads that connection, and the database list it sits in.
pub fn structure_changed(connection: &str, cx: &mut App) {
    let Some(sidebar) = cx.try_global::<Sidebar>().map(|s| s.0.clone()) else {
        return;
    };
    let connection = connection.to_owned();
    cx.defer(move |cx| {
        let _ = sidebar.update(cx, |panel, cx| panel.structure_changed(&connection, cx));
    });
}

/// Something the sidebar asks the workspace to do.
pub enum BrowserEvent {
    /// Open a query tab on this connection.
    OpenQuery(QueryTarget),
    /// Open a table's rows on this connection.
    OpenTable(QueryTarget, TableRef),
}

/// Something fetched from the database, in each of its states.
enum Load<T> {
    Loading,
    Ready(T),
    Failed(SharedString),
}

enum Status {
    Idle,
    Connecting,
    Connected {
        session: Arc<dyn Session>,
        catalog: Load<Catalog>,
    },
    Failed(SharedString),
}

struct Entry {
    saved: SavedConnection,
    status: Status,
    /// A password typed for this connection in this run of the program. Kept
    /// in memory only, so further sessions (query tabs) can open without
    /// asking again when there is no keyring.
    password: Option<Secret>,
    /// Bumped whenever the connection is (re)started or dropped, so results
    /// from an earlier attempt can be recognised and ignored.
    generation: u64,
    /// For a database listed under a connection, that connection's id.
    parent: Option<String>,
    /// The databases a connection that lists them found, once connected.
    databases: Option<Load<Vec<String>>>,
}

impl Entry {
    fn new(saved: SavedConnection, generation: u64) -> Self {
        Self {
            saved,
            status: Status::Idle,
            password: None,
            generation,
            parent: None,
            databases: None,
        }
    }

    /// The one database to show, for a connection whose schemas are its
    /// server's databases and that does not show them all. `None` shows
    /// every schema.
    fn only_database(&self) -> Option<&str> {
        let kind = self.saved.config.kind();
        let server = self.saved.config.server()?;
        let named = server.database.trim();
        (sonde_core::ddl::schemas_are_databases(kind)
            && !server.all_databases()
            && !named.is_empty())
        .then_some(named)
    }

    /// Whether this connection shows the server's databases rather than one
    /// database's schemas.
    fn lists_databases(&self) -> bool {
        self.parent.is_none()
            && self.saved.config.kind().session_per_database()
            && self
                .saved
                .config
                .server()
                .is_some_and(sonde_core::ServerConfig::all_databases)
    }
}

pub struct BrowserPanel {
    connections: Entity<Connections>,
    entries: Vec<Entry>,
    details: HashMap<(String, TableRef), Load<TableDetail>>,
    expanded: HashSet<SharedString>,
    /// What each tree row is, shared with the row renderer.
    rows: Rc<RefCell<HashMap<SharedString, Row>>>,
    tree: Entity<TreeState>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

/// Read-only presentation of the selected object's already-loaded metadata.
pub(crate) struct Inspection<'a> {
    pub connection: &'a SavedConnection,
    pub status: &'static str,
    pub catalog: Option<&'a Catalog>,
    pub schema: Option<&'a Schema>,
    pub table: Option<TableRef>,
    pub detail: Option<&'a TableDetail>,
    pub message: Option<&'a str>,
    pub error: bool,
}

impl BrowserPanel {
    pub fn new(
        connections: Entity<Connections>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let tree = cx.new(|cx| TreeState::new(cx));
        let _subscriptions = vec![
            cx.subscribe_in(&tree, window, Self::on_tree_event),
            // Selection decides which toolbar actions are offered.
            cx.observe(&tree, |_, _, cx| cx.notify()),
            cx.subscribe(&connections, |this, _, _: &ConnectionsChanged, cx| {
                this.sync_saved(cx);
            }),
        ];
        let mut this = Self {
            connections,
            entries: Vec::new(),
            details: HashMap::new(),
            expanded: HashSet::new(),
            rows: Rc::default(),
            tree,
            focus_handle: cx.focus_handle(),
            _subscriptions,
        };
        this.sync_saved(cx);
        cx.set_global(Sidebar(cx.weak_entity()));
        this
    }

    /// Bring the entries in line with the saved list. A connection whose
    /// settings changed is dropped back to idle: its session was opened with
    /// the old ones.
    fn sync_saved(&mut self, cx: &mut Context<Self>) {
        let saved = self.connections.read(cx).list().to_vec();
        let (children, parents): (Vec<Entry>, Vec<Entry>) = self
            .entries
            .drain(..)
            .partition(|entry| entry.parent.is_some());
        let mut previous: HashMap<String, Entry> = parents
            .into_iter()
            .map(|entry| (entry.saved.id.clone(), entry))
            .collect();
        let mut kept = HashSet::new();

        for saved in saved {
            let entry = match previous.remove(&saved.id) {
                Some(entry) if entry.saved.config == saved.config => {
                    kept.insert(saved.id.clone());
                    Entry { saved, ..entry }
                }
                Some(entry) => {
                    self.forget(&saved.id);
                    Entry::new(saved, entry.generation + 1)
                }
                None => Entry::new(saved, 0),
            };
            self.entries.push(entry);
        }
        for removed in previous.into_keys() {
            self.forget(&removed);
        }
        // A listed database stays while its connection is unchanged.
        for child in children {
            if child
                .parent
                .as_ref()
                .is_some_and(|parent| kept.contains(parent))
            {
                self.entries.push(child);
            } else {
                self.forget(&child.saved.id);
            }
        }
        self.rebuild(cx);
    }

    /// Drop everything remembered about a connection's contents.
    fn forget(&mut self, connection: &str) {
        let prefix = id::connection(connection);
        self.expanded.retain(|row| !id::is_within(row, &prefix));
        self.details.retain(|(owner, _), _| owner != connection);
    }

    fn entry_mut(&mut self, connection: &str) -> Option<&mut Entry> {
        self.entries
            .iter_mut()
            .find(|entry| entry.saved.id == connection)
    }

    fn session(&self, connection: &str) -> Option<(Arc<dyn Session>, u64)> {
        self.entries
            .iter()
            .find(|entry| entry.saved.id == connection)
            .and_then(|entry| match &entry.status {
                Status::Connected { session, .. } => Some((Arc::clone(session), entry.generation)),
                _ => None,
            })
    }

    fn on_tree_event(
        &mut self,
        _: &Entity<TreeState>,
        event: &TreeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            TreeEvent::Collapsed(row) => {
                self.expanded.remove(row);
            }
            TreeEvent::Expanded(row) => {
                self.expanded.insert(row.clone());
                let target = self.rows.borrow().get(row).map(|row| row.target.clone());
                match target {
                    Some(rows::Target::Connection(connection)) => {
                        let startable = self.entries.iter().any(|entry| {
                            entry.saved.id == connection
                                && matches!(entry.status, Status::Idle | Status::Failed(_))
                        });
                        if startable {
                            self.connect(&connection, None, window, cx);
                        }
                    }
                    Some(rows::Target::Table { connection, table })
                        if !self
                            .details
                            .contains_key(&(connection.clone(), table.clone())) =>
                    {
                        self.load_detail(connection, table, cx);
                    }
                    _ => {}
                }
                // A row at the bottom of the view would leave its children
                // just below it; bring the first one into view.
                if let Some(ix) = self.tree.read(cx).index_of(row) {
                    self.keep_row_children_in_view(ix, cx);
                }
            }
        }
    }

    /// When the user expands a row near the tree's bottom edge, its children
    /// start out just below the visible area. `Nearest` scrolls just enough
    /// that the first one is in view; already-visible children are left
    /// alone.
    fn keep_row_children_in_view(&self, row: usize, cx: &mut Context<Self>) {
        let first = {
            let tree = self.tree.read(cx);
            let Some(entry) = tree.entry(row) else {
                return;
            };
            if !entry.is_folder() || !entry.is_expanded() {
                return;
            }
            let row_id = entry.item().id.clone();
            let mut first = None;
            for ix in row + 1.. {
                let Some(desc) = tree.entry(ix) else {
                    break;
                };
                if id::is_within(&desc.item().id, &row_id) {
                    first = Some(ix);
                    break;
                }
            }
            first
        };

        if let Some(first) = first {
            self.tree.update(cx, |tree, cx| {
                tree.scroll_to_item(first, ScrollStrategy::Nearest);
                cx.notify();
            });
        }
    }

    /// Open a session for a saved connection.
    ///
    /// `password` is what the user just typed, when they were asked. Without
    /// it the keyring is consulted; with no keyring, a Postgres connection
    /// asks first and comes back here with the answer.
    pub fn connect(
        &mut self,
        connection: &str,
        password: Option<Secret>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let secrets = self.connections.read(cx).secrets();
        let row = id::connection(connection);
        let Some(entry) = self.entry_mut(connection) else {
            return;
        };
        let wants_password = entry.saved.config.kind().is_server();
        // A listed database signs in the way its connection did.
        let password = password.or_else(|| {
            entry
                .parent
                .is_some()
                .then(|| entry.password.clone())
                .flatten()
        });

        if wants_password && secrets.is_none() && password.is_none() {
            let name = entry.saved.name.clone();
            // Nothing is in flight yet, so the row should not sit open on a
            // "Connecting" line while the prompt is up.
            self.expanded.remove(&row);
            self.rebuild(cx);
            open_password_prompt(cx.entity(), connection.to_owned(), name, window, cx);
            return;
        }

        entry.generation += 1;
        entry.status = Status::Connecting;
        if password.is_some() {
            entry.password.clone_from(&password);
        }
        let generation = entry.generation;
        let config = entry.saved.config.clone();
        let connection = connection.to_owned();
        self.expanded.insert(row);
        self.rebuild(cx);

        cx.spawn(async move |this, cx| {
            let secret = match (password, secrets) {
                (Some(password), _) => Ok(Some(password)),
                (None, Some(secrets)) if wants_password => {
                    let key = crate::connections::keyring_id(&connection).to_owned();
                    // The keyring speaks D-Bus and can block on an unlock prompt.
                    cx.background_spawn(async move { secrets.get(&key) }).await
                }
                _ => Ok(None),
            };
            let outcome = match secret {
                Ok(secret) => sonde_core::connect(&config, secret).await,
                Err(error) => Err(error),
            };
            // The panel may have been closed while this ran.
            let _ = this.update(cx, |this, cx| {
                this.on_connected(&connection, generation, outcome, cx);
            });
        })
        .detach();
    }

    fn on_connected(
        &mut self,
        connection: &str,
        generation: u64,
        outcome: Result<Box<dyn Session>>,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self
            .entry_mut(connection)
            .filter(|entry| entry.generation == generation)
        else {
            // Superseded: dropping the session closes it.
            return;
        };
        match outcome {
            Ok(session) => {
                entry.status = Status::Connected {
                    session: Arc::from(session),
                    catalog: Load::Loading,
                };
                if entry.lists_databases() {
                    self.load_databases(connection, cx);
                } else {
                    self.load_catalog(connection, cx);
                }
            }
            Err(error) => {
                entry.status = Status::Failed(error.to_string().into());
                self.rebuild(cx);
            }
        }
    }

    /// List a connection's databases and keep one entry per database under
    /// it: new ones appear, dropped ones go.
    fn load_databases(&mut self, connection: &str, cx: &mut Context<Self>) {
        let Some((session, generation)) = self.session(connection) else {
            return;
        };
        if let Some(entry) = self.entry_mut(connection) {
            entry.databases = Some(Load::Loading);
        }
        self.rebuild(cx);

        let connection = connection.to_owned();
        cx.spawn(async move |this, cx| {
            let listed = session.databases().await;
            let _ =
                this.update(cx, |this, cx| {
                    let Some(parent) = this.entries.iter().find(|entry| {
                        entry.saved.id == connection && entry.generation == generation
                    }) else {
                        return;
                    };
                    let mut names = match listed {
                        Ok(names) => names,
                        Err(error) => {
                            if let Some(entry) = this.entry_mut(&connection) {
                                entry.databases = Some(Load::Failed(error.to_string().into()));
                            }
                            this.rebuild(cx);
                            return;
                        }
                    };
                    // The database the connection signs in to is always there;
                    // an empty Redis database is not in INFO's list.
                    if let Some(server) = parent.saved.config.server() {
                        let own = if parent.saved.config.kind() == sonde_core::DbKind::Redis {
                            format!(
                                "db{}",
                                if server.database.is_empty() {
                                    "0"
                                } else {
                                    &server.database
                                }
                            )
                        } else {
                            server.database.clone()
                        };
                        if !own.is_empty() && !names.contains(&own) {
                            names.push(own);
                        }
                    }
                    if parent.saved.config.kind() == sonde_core::DbKind::Redis {
                        names.sort_by_key(|name| {
                            name.trim_start_matches("db")
                                .parse::<u32>()
                                .unwrap_or(u32::MAX)
                        });
                    } else {
                        names.sort();
                    }
                    let config = parent.saved.config.clone();
                    let password = parent.password.clone();

                    let gone: Vec<String> = this
                        .entries
                        .iter()
                        .filter(|entry| entry.parent.as_deref() == Some(connection.as_str()))
                        .filter(|entry| {
                            !names.iter().any(|name| {
                                crate::connections::database_id(&connection, name) == entry.saved.id
                            })
                        })
                        .map(|entry| entry.saved.id.clone())
                        .collect();
                    for id in &gone {
                        this.forget(id);
                    }
                    this.entries.retain(|entry| !gone.contains(&entry.saved.id));
                    for name in &names {
                        let id = crate::connections::database_id(&connection, name);
                        if this.entries.iter().any(|entry| entry.saved.id == id) {
                            continue;
                        }
                        let mut entry = Entry::new(
                            SavedConnection {
                                id,
                                name: name.clone(),
                                config: config.with_database(name),
                            },
                            0,
                        );
                        entry.parent = Some(connection.clone());
                        entry.password.clone_from(&password);
                        this.entries.push(entry);
                    }
                    if let Some(entry) = this.entry_mut(&connection) {
                        entry.databases = Some(Load::Ready(names));
                    }
                    this.rebuild(cx);
                });
        })
        .detach();
    }

    fn load_catalog(&mut self, connection: &str, cx: &mut Context<Self>) {
        let Some((session, generation)) = self.session(connection) else {
            return;
        };
        if let Some(Status::Connected { catalog, .. }) =
            self.entry_mut(connection).map(|entry| &mut entry.status)
        {
            *catalog = Load::Loading;
        }
        self.rebuild(cx);

        let connection = connection.to_owned();
        cx.spawn(async move |this, cx| {
            let loaded = session.catalog().await;
            let _ = this.update(cx, |this, cx| {
                let Some(entry) = this
                    .entry_mut(&connection)
                    .filter(|entry| entry.generation == generation)
                else {
                    return;
                };
                let only = entry.only_database().map(str::to_owned);
                if let Status::Connected { catalog, .. } = &mut entry.status {
                    *catalog = match loaded {
                        Ok(mut loaded) => {
                            // Databases shown as schemas: just the named one,
                            // unless the connection shows them all.
                            if let Some(only) = only {
                                loaded.schemas.retain(|schema| schema.name == only);
                            }
                            Load::Ready(loaded)
                        }
                        Err(error) => Load::Failed(error.to_string().into()),
                    };
                }
                this.rebuild(cx);
            });
        })
        .detach();
    }

    fn load_detail(&mut self, connection: String, table: TableRef, cx: &mut Context<Self>) {
        let Some((session, generation)) = self.session(&connection) else {
            return;
        };
        let key = (connection, table);
        self.details.insert(key.clone(), Load::Loading);
        self.rebuild(cx);

        cx.spawn(async move |this, cx| {
            let loaded = session.table_detail(&key.1).await;
            let _ = this.update(cx, |this, cx| {
                // Only record it if the same session is still the current one
                // and nothing cleared the request (a refresh, a disconnect).
                let current = this
                    .session(&key.0)
                    .is_some_and(|(_, current)| current == generation);
                if current && this.details.contains_key(&key) {
                    let detail = match loaded {
                        Ok(detail) => Load::Ready(detail),
                        Err(error) => Load::Failed(error.to_string().into()),
                    };
                    this.details.insert(key, detail);
                    this.rebuild(cx);
                }
            });
        })
        .detach();
    }

    /// Reload a connected database's schemas, discarding cached table details.
    /// A connection that lists its databases lists them again.
    fn refresh(&mut self, connection: &str, cx: &mut Context<Self>) {
        if self
            .entries
            .iter()
            .any(|entry| entry.saved.id == connection && entry.lists_databases())
        {
            self.load_databases(connection, cx);
            return;
        }
        self.details.retain(|(owner, _), _| owner != connection);
        // Tables left open would show "Loading" forever with their details
        // gone, so fetch those again.
        let open: Vec<TableRef> = self
            .rows
            .borrow()
            .iter()
            .filter(|(row, _)| self.expanded.contains(*row))
            .filter_map(|(_, row)| match &row.target {
                rows::Target::Table {
                    connection: owner,
                    table,
                } if owner == connection => Some(table.clone()),
                _ => None,
            })
            .collect();
        self.load_catalog(connection, cx);
        for table in open {
            self.load_detail(connection.to_owned(), table, cx);
        }
    }

    fn disconnect(&mut self, connection: &str, cx: &mut Context<Self>) {
        self.forget(connection);
        // Its listed databases close with it.
        let children: Vec<String> = self
            .entries
            .iter()
            .filter(|entry| entry.parent.as_deref() == Some(connection))
            .map(|entry| entry.saved.id.clone())
            .collect();
        for child in &children {
            self.forget(child);
        }
        self.entries
            .retain(|entry| !children.contains(&entry.saved.id));
        if let Some(entry) = self.entry_mut(connection) {
            entry.databases = None;
        }
        if let Some(entry) = self.entry_mut(connection) {
            entry.generation += 1;
            // Dropping the last handle to the session closes it.
            entry.status = Status::Idle;
            entry.password = None;
        }
        self.rebuild(cx);
    }

    fn delete(&mut self, connection: &str, window: &mut Window, cx: &mut Context<Self>) {
        let secrets = self.connections.read(cx).secrets();
        let removed = self
            .connections
            .update(cx, |connections, cx| connections.remove(connection, cx));
        match removed {
            Ok(()) => {
                if let Some(secrets) = secrets {
                    let key = connection.to_owned();
                    // A password left behind is harmless clutter, and there is
                    // no connection left to report the failure against.
                    cx.background_spawn(async move { secrets.delete(&key) })
                        .detach();
                }
            }
            Err(error) => window.push_notification(error.to_string(), cx),
        }
    }

    /// Open the connection's form with its saved settings.
    fn edit(&mut self, connection: &str, window: &mut Window, cx: &mut Context<Self>) {
        let saved = self.connections.read(cx).get(connection).cloned();
        if saved.is_some() {
            open_connection_form(self.connections.clone(), saved, window, cx);
        }
    }

    fn confirm_delete(&mut self, connection: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self
            .connections
            .read(cx)
            .get(connection)
            .map(|saved| saved.name.clone())
        else {
            return;
        };
        let panel = cx.weak_entity();
        let connection = connection.to_owned();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let panel = panel.clone();
            let connection = connection.clone();
            alert
                .title(format!("Delete \"{name}\"?"))
                .description(
                    "The saved connection and its stored password are removed. \
                     The database itself is not touched.",
                )
                .show_cancel(true)
                .ok_text("Delete connection")
                .on_ok(move |_, window, cx| {
                    // The panel going away while its own dialog is open would
                    // be a bug elsewhere; closing the dialog is still right.
                    let _ = panel.update(cx, |panel, cx| panel.delete(&connection, window, cx));
                    true
                })
        });
    }

    /// Follow selected child rows back to their table, including columns and
    /// key groups. This never opens a session or fetches additional metadata.
    pub(crate) fn inspection(&self, cx: &App) -> Option<Inspection<'_>> {
        let selected = self.tree.read(cx).selected_item()?.id.clone();
        let owner = id::connection_of(&selected)?;
        let entry = self.entries.iter().find(|entry| entry.saved.id == owner)?;
        let (status, catalog, message, error) = match (&entry.status, &entry.databases) {
            // A connection that lists its databases has no schemas of its own.
            (Status::Connected { .. }, Some(Load::Loading)) => (
                "Connected",
                None,
                Some("Listing the server's databases."),
                false,
            ),
            (Status::Connected { .. }, Some(Load::Failed(message))) => {
                ("Connected", None, Some(message.as_ref()), true)
            }
            (Status::Connected { .. }, Some(Load::Ready(_))) => (
                "Connected",
                None,
                Some("Open a database under this connection to browse its schemas."),
                false,
            ),
            (status, _) => match status {
                Status::Idle => (
                    "Disconnected",
                    None,
                    Some("Use Connect to browse this database."),
                    false,
                ),
                Status::Connecting => (
                    "Connecting",
                    None,
                    Some("Opening the database connection."),
                    false,
                ),
                Status::Failed(message) => {
                    ("Connection failed", None, Some(message.as_ref()), true)
                }
                Status::Connected {
                    catalog: Load::Loading,
                    ..
                } => ("Connected", None, Some("Loading schemas."), false),
                Status::Connected {
                    catalog: Load::Failed(message),
                    ..
                } => ("Connected", None, Some(message.as_ref()), true),
                Status::Connected {
                    catalog: Load::Ready(catalog),
                    ..
                } => ("Connected", Some(catalog), None, false),
            },
        };
        let root = id::connection(owner);
        let schema = catalog.and_then(|catalog| {
            catalog.schemas.iter().find(|schema| {
                id::is_within(&selected, &id::child(&root, &format!("s:{}", schema.name)))
            })
        });
        let table = self.rows.borrow().iter().find_map(|(row_id, row)| {
            if !id::is_within(&selected, row_id) {
                return None;
            }
            match &row.target {
                rows::Target::Table { table, .. } => Some(table.clone()),
                _ => None,
            }
        });
        let (detail, message, error) = match table
            .as_ref()
            .map(|table| self.details.get(&(owner.to_owned(), table.clone())))
        {
            Some(Some(Load::Ready(detail))) => (Some(detail), None, false),
            Some(Some(Load::Failed(message))) => (None, Some(message.as_ref()), true),
            Some(Some(Load::Loading)) => (
                None,
                Some("Loading columns, indexes and foreign keys."),
                false,
            ),
            Some(None) => (
                None,
                Some("Expand this table to load its columns and indexes."),
                false,
            ),
            None => (None, message, error),
        };
        Some(Inspection {
            connection: &entry.saved,
            status,
            catalog,
            schema,
            table,
            detail,
            message,
            error,
        })
    }

    /// The connection the selected row belongs to.
    pub(crate) fn select_connection(
        &mut self,
        connection: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.tree.update(cx, |tree, cx| {
            tree.set_selected_index(tree.index_of(&id::connection(connection)), cx);
            tree.focus(window, cx);
        });
        cx.notify();
    }

    /// What a tab on this entry is called: a listed database names its
    /// connection too.
    fn tab_name(&self, entry: &Entry) -> String {
        let parent = entry.parent.as_deref().and_then(|parent| {
            self.entries
                .iter()
                .find(|other| other.saved.id == parent)
                .map(|other| other.saved.name.clone())
        });
        match parent {
            Some(parent) => format!("{parent} · {}", entry.saved.name),
            None => entry.saved.name.clone(),
        }
    }

    /// The connection a listed database sits under.
    fn parent_of(&self, connection: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|entry| entry.saved.id == connection)
            .and_then(|entry| entry.parent.as_deref())
    }

    /// Drop a database listed under a connection. Its own session closes
    /// first, since a database in use cannot be dropped; the statement runs
    /// on the connection's session, which then lists its databases again.
    fn drop_listed_database(
        &mut self,
        connection: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(parent) = self.parent_of(connection).map(str::to_owned) else {
            return;
        };
        let Some(name) = crate::connections::database_of(connection).map(str::to_owned) else {
            return;
        };
        self.disconnect(connection, cx);
        self.manage(&parent, manage::Task::DropDatabase(name), window, cx);
    }

    /// Reload what a statement run elsewhere may have changed: a listed
    /// database reloads, and so does the list it sits in.
    pub fn structure_changed(&mut self, connection: &str, cx: &mut Context<Self>) {
        if !self.is_connected(connection) {
            return;
        }
        self.refresh(connection, cx);
        if let Some(parent) = self.parent_of(connection).map(str::to_owned) {
            self.refresh(&parent, cx);
        }
    }

    /// The kind of database a saved connection is.
    fn kind_of(&self, connection: &str) -> Option<sonde_core::DbKind> {
        self.entries
            .iter()
            .find(|entry| entry.saved.id == connection)
            .map(|entry| entry.saved.config.kind())
    }

    /// Open the dialog for a structure change on a connected connection; the
    /// connection reloads once it has run.
    fn manage(
        &mut self,
        connection: &str,
        task: manage::Task,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.saved.id == connection)
        else {
            return;
        };
        let Status::Connected { session, .. } = &entry.status else {
            return;
        };
        let kind = entry.saved.config.kind();
        let session = Arc::clone(session);
        let owner = cx.weak_entity();
        let connection = connection.to_owned();
        let on_done: Rc<dyn Fn(&mut App)> = Rc::new(move |cx| {
            let _ = owner.update(cx, |panel, cx| panel.refresh(&connection, cx));
        });
        manage::open(kind, task, session, on_done, window, cx);
    }

    /// Whether the connection with this id is open.
    fn is_connected(&self, connection: &str) -> bool {
        self.entries.iter().any(|entry| {
            entry.saved.id == connection && matches!(entry.status, Status::Connected { .. })
        })
    }

    /// Ask the workspace for a query tab on a connected connection.
    fn open_query(&mut self, connection: &str, cx: &mut Context<Self>) {
        let target = self
            .entries
            .iter()
            .find(|entry| entry.saved.id == connection)
            .filter(|entry| matches!(entry.status, Status::Connected { .. }))
            .map(|entry| QueryTarget {
                id: entry.saved.id.clone(),
                name: self.tab_name(entry),
                config: entry.saved.config.clone(),
                password: entry.password.clone(),
            });
        if let Some(target) = target {
            cx.emit(BrowserEvent::OpenQuery(target));
        }
    }

    /// Ask the workspace for a tab with a table's rows.
    fn open_table(&mut self, connection: &str, table: TableRef, cx: &mut Context<Self>) {
        let target = self
            .entries
            .iter()
            .find(|entry| entry.saved.id == connection)
            .filter(|entry| matches!(entry.status, Status::Connected { .. }))
            .map(|entry| QueryTarget {
                id: entry.saved.id.clone(),
                name: self.tab_name(entry),
                config: entry.saved.config.clone(),
                password: entry.password.clone(),
            });
        if let Some(target) = target {
            cx.emit(BrowserEvent::OpenTable(target, table));
        }
    }

    fn selected_connection(&self, cx: &App) -> Option<String> {
        self.tree
            .read(cx)
            .selected_item()
            .and_then(|item| id::connection_of(&item.id))
            .map(str::to_owned)
    }

    /// Keep the selected row, or its connection if the row disappears.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let mut rows = HashMap::new();
        let items: Vec<TreeItem> = self
            .entries
            .iter()
            .filter(|entry| entry.parent.is_none())
            .map(|entry| {
                let children: Vec<&Entry> = self
                    .entries
                    .iter()
                    .filter(|child| child.parent.as_deref() == Some(entry.saved.id.as_str()))
                    .collect();
                rows::connection(entry, &children, &self.details, &self.expanded, &mut rows)
            })
            .collect();
        *self.rows.borrow_mut() = rows;

        self.tree.update(cx, |tree, cx| {
            let selected = tree.selected_item().map(|item| item.id.clone());
            tree.set_items(items, cx);
            let index = selected.and_then(|selected| {
                tree.index_of(&selected).or_else(|| {
                    // A catalog reload temporarily removes schemas and tables.
                    // Keep their connection selected so its actions stay usable.
                    let connection = id::connection_of(&selected)?;
                    tree.index_of(&id::connection(connection))
                })
            });
            tree.set_selected_index(index, cx);
        });
        cx.notify();
    }
}

impl EventEmitter<BrowserEvent> for BrowserPanel {}

impl Focusable for BrowserPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl BrowserPanel {
    /// The number of saved connections.
    pub(crate) fn connection_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.parent.is_none())
            .count()
    }

    /// The tree row that is selected, by id.
    pub(crate) fn selected_row(&self, cx: &App) -> Option<SharedString> {
        self.tree
            .read(cx)
            .selected_item()
            .map(|item| item.id.clone())
    }

    /// How many saved connections have a live session.
    pub(crate) fn connected_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.parent.is_none())
            .filter(|entry| matches!(entry.status, Status::Connected { .. }))
            .count()
    }
}

impl Render for BrowserPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected_connection(cx);
        let status = selected.as_ref().and_then(|selected| {
            self.entries
                .iter()
                .find(|entry| &entry.saved.id == selected)
                .map(|entry| &entry.status)
        });
        let connected = matches!(status, Some(Status::Connected { .. }));
        let connecting = matches!(status, Some(Status::Connecting));
        let load_error = self.connections.read(cx).load_error().cloned();
        let keyring_missing = self.connections.read(cx).secrets().is_none();
        let (danger, quiet) = {
            let theme = cx.theme();
            (theme.danger, theme.muted_foreground)
        };

        // Icon-only `SidebarGroupAction`s, each acting on the selected
        // connection or table. Their ids are their names.
        let action =
            |label: &'static str,
             glyph: Glyph,
             enabled: bool,
             cx: &Context<Self>,
             run: fn(&mut Self, &str, &mut Window, &mut Context<Self>)| {
                let target = selected.clone();
                Button::new(label)
                    .ghost()
                    .small()
                    .icon(ui::icon(glyph))
                    .accessibility_label(label)
                    .tooltip(label)
                    .disabled(!enabled || target.is_none())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(target) = &target {
                            run(this, target, window, cx);
                        }
                    }))
            };

        let label_row = h_flex()
            .flex_shrink_0()
            .pr_1()
            .items_center()
            .child(ui::group_label("Connections", cx))
            .child(div().flex_1())
            .child(if connected {
                action(
                    "Disconnect",
                    Glyph::Unplug,
                    true,
                    cx,
                    |this, target, _, cx| this.disconnect(target, cx),
                )
            } else {
                action(
                    "Connect",
                    Glyph::Plug,
                    !connecting,
                    cx,
                    |this, target, window, cx| this.connect(target, None, window, cx),
                )
            })
            .child(action(
                "Refresh",
                Glyph::RefreshCw,
                connected,
                cx,
                |this, target, _, cx| this.refresh(target, cx),
            ));

        let notice = |text: SharedString, color: Hsla, cx: &App| {
            div()
                .flex_shrink_0()
                .mx_2()
                .mb_2()
                .px_3()
                .py_2()
                .rounded(px(crate::design::RADIUS_ITEM))
                .bg(crate::design::tokens(cx).sidebar_accent)
                .text_sm()
                .line_height(rems(1.4))
                .text_color(color)
                .child(text)
        };

        let rows = Rc::clone(&self.rows);
        let menu_rows = Rc::clone(&self.rows);
        let menu_owner = cx.weak_entity();
        let row_owner = cx.weak_entity();
        let body = if self.entries.is_empty() {
            v_flex()
                .px_2()
                .py_1()
                .gap_1()
                .child(div().text_sm().child("No saved connections"))
                .child(
                    div()
                        .text_sm()
                        .text_color(quiet)
                        .child("Add a database server or a SQLite file with New connection."),
                )
                .into_any_element()
        } else {
            // A connection's New query, Edit and Delete live in its
            // right-click menu, and a table's Open table in its own.
            tree(&self.tree, move |ix, entry, selected, _, cx| {
                let row = rows.borrow().get(&entry.item().id).cloned();
                let table = row.as_ref().and_then(|row| match &row.target {
                    Target::Table { connection, table } => {
                        Some((connection.clone(), table.clone()))
                    }
                    _ => None,
                });
                let item = render_row(
                    ix,
                    entry.depth(),
                    entry.is_folder(),
                    entry.is_expanded(),
                    selected,
                    row,
                    cx,
                );
                match table {
                    // A double-click opens the table's rows. Its two clicks
                    // also expand and collapse the row, which leaves it as
                    // it was.
                    Some((connection, table)) => {
                        let owner = row_owner.clone();
                        // On the press, not the click: the tree takes the
                        // click to select and expand the row.
                        item.on_mouse_down(MouseButton::Left, move |event, _, cx| {
                            if event.click_count == 2 {
                                let _ = owner.update(cx, |panel, cx| {
                                    panel.open_table(&connection, table.clone(), cx);
                                });
                            }
                        })
                    }
                    None => item,
                }
            })
            .context_menu(move |_, entry, menu, _window, cx| {
                let target = menu_rows
                    .borrow()
                    .get(&entry.item().id)
                    .map(|row| row.target.clone());
                let Some(panel) = menu_owner.upgrade() else {
                    return menu;
                };
                let menu = menu.min_w(px(ui::MENU_WIDTH));
                // Structure changes, offered where the database can make them.
                let manage = |menu: PopupMenu, connection: &str, tasks: Vec<manage::Task>| {
                    let Some(kind) = panel.read(cx).kind_of(connection) else {
                        return menu;
                    };
                    let connected = panel.read(cx).is_connected(connection);
                    let tasks: Vec<_> = tasks
                        .into_iter()
                        .filter(|task| sonde_core::ddl::supports(kind, task.op()))
                        .collect();
                    if tasks.is_empty() || !connected {
                        return menu;
                    }
                    let mut menu = menu.separator();
                    for task in tasks {
                        let owner = menu_owner.clone();
                        let connection = connection.to_owned();
                        menu = menu.item(
                            ui::menu_item(task.title(kind), task.glyph(), task.destructive())
                                .on_click(move |_, window, cx| {
                                    let _ = owner.update(cx, |panel, cx| {
                                        panel.manage(&connection, task.clone(), window, cx);
                                    });
                                }),
                        );
                    }
                    menu
                };
                match target {
                    Some(Target::Connection(connection))
                        if panel.read(cx).parent_of(&connection).is_some() =>
                    {
                        // A database listed under a connection: queries and
                        // schemas here; dropping it runs on the connection.
                        let query_target = connection.clone();
                        let connected = panel.read(cx).is_connected(&connection);
                        let menu = menu.item(
                            if connected {
                                ui::menu_item("New query", Glyph::SquareTerminal, false)
                            } else {
                                ui::disabled_menu_item("New query", Glyph::SquareTerminal)
                            }
                            .on_click({
                                let owner = menu_owner.clone();
                                move |_, _, cx| {
                                    let _ = owner.update(cx, |panel, cx| {
                                        panel.open_query(&query_target, cx);
                                    });
                                }
                            }),
                        );
                        let menu = manage(menu, &connection, vec![manage::Task::CreateSchema]);
                        let kind = panel.read(cx).kind_of(&connection);
                        let drops = kind.is_some_and(|kind| {
                            kind != sonde_core::DbKind::Redis
                                && sonde_core::ddl::supports(
                                    kind,
                                    sonde_core::ddl::Op::DropDatabase,
                                )
                        });
                        if !drops {
                            return menu;
                        }
                        let noun = kind.map_or("database", manage::database_noun);
                        menu.separator().item(
                            ui::menu_item(format!("Drop {noun}…"), Glyph::Trash, true).on_click({
                                let owner = menu_owner.clone();
                                move |_, window, cx| {
                                    let _ = owner.update(cx, |panel, cx| {
                                        panel.drop_listed_database(&connection, window, cx);
                                    });
                                }
                            }),
                        )
                    }
                    Some(Target::Connection(connection)) => {
                        let edit_target = connection.clone();
                        let query_target = connection.clone();
                        let connected = panel.read(cx).is_connected(&connection);
                        let menu = menu.item(
                            if connected {
                                ui::menu_item("New query", Glyph::SquareTerminal, false)
                            } else {
                                ui::disabled_menu_item("New query", Glyph::SquareTerminal)
                            }
                            .on_click({
                                let owner = menu_owner.clone();
                                move |_, _, cx| {
                                    let _ = owner.update(cx, |panel, cx| {
                                        panel.open_query(&query_target, cx);
                                    });
                                }
                            }),
                        );
                        manage(
                            menu,
                            &connection,
                            vec![manage::Task::CreateDatabase, manage::Task::CreateSchema],
                        )
                        .separator()
                        .item(ui::menu_item("Edit…", Glyph::SquarePen, false).on_click({
                            let owner = menu_owner.clone();
                            move |_, window, cx| {
                                if let Some(panel) = owner.upgrade() {
                                    panel.update(cx, |panel, cx| {
                                        panel.edit(&edit_target, window, cx)
                                    });
                                }
                            }
                        }))
                        .item(
                            ui::menu_item("Delete…", Glyph::Trash, true).on_click({
                                let owner = menu_owner.clone();
                                move |_, window, cx| {
                                    if let Some(panel) = owner.upgrade() {
                                        panel.update(cx, |panel, cx| {
                                            panel.confirm_delete(&connection, window, cx)
                                        });
                                    }
                                }
                            }),
                        )
                    }
                    Some(Target::Schema { connection, schema }) => {
                        let kind = panel.read(cx).kind_of(&connection);
                        let drop = if kind.is_some_and(sonde_core::ddl::schemas_are_databases) {
                            manage::Task::DropDatabase(schema.clone())
                        } else {
                            manage::Task::DropSchema(schema.clone())
                        };
                        manage(
                            menu,
                            &connection,
                            vec![
                                manage::Task::CreateTable {
                                    schema: schema.clone(),
                                },
                                manage::Task::CreateKey,
                                drop,
                                manage::Task::FlushDatabase(schema),
                            ],
                        )
                    }
                    Some(Target::Table { connection, table }) => {
                        let owner = menu_owner.clone();
                        let open_target = connection.clone();
                        let open_table = table.clone();
                        let menu =
                            menu.item(ui::menu_item("Open table", Glyph::Table, false).on_click(
                                move |_, _, cx| {
                                    let _ = owner.update(cx, |panel, cx| {
                                        panel.open_table(&open_target, open_table.clone(), cx);
                                    });
                                },
                            ));
                        manage(
                            menu,
                            &connection,
                            vec![
                                manage::Task::RenameTable(table.clone()),
                                manage::Task::AddColumn(table.clone()),
                                manage::Task::EmptyTable(table.clone()),
                                manage::Task::DropTable(table),
                            ],
                        )
                    }
                    Some(Target::Column {
                        connection,
                        table,
                        column,
                    }) => manage(
                        menu,
                        &connection,
                        vec![
                            manage::Task::RenameColumn {
                                table: table.clone(),
                                column: column.clone(),
                            },
                            manage::Task::DropColumn { table, column },
                        ],
                    ),
                    _ => menu,
                }
            })
            .into_any_element()
        };

        v_flex()
            .track_focus(&self.focus_handle)
            .size_full()
            .min_h_0()
            .child(label_row)
            .when_some(load_error, |this, error| {
                this.child(notice(error, danger, cx))
            })
            .when(keyring_missing, |this| {
                this.child(notice(
                    "No keyring is running, so passwords are not saved. \
                     You will be asked for one each time you connect."
                        .into(),
                    quiet,
                    cx,
                ))
            })
            .child(
                div()
                    .id("connection-tree")
                    .test_support()
                    .flex_1()
                    .min_h_0()
                    .child(body),
            )
    }
}

/// One tree row, drawn as a `SidebarMenuButton`: indentation guides like
/// `SidebarMenuSub`'s left border, a chevron on anything that opens, the
/// object's icon, its name, and a quieter note after it.
fn render_row(
    ix: usize,
    depth: usize,
    folder: bool,
    expanded: bool,
    selected: bool,
    row: Option<Row>,
    cx: &App,
) -> ListItem {
    let theme = cx.theme();
    let row = row.unwrap_or_default();
    let colors = design::tokens(cx);
    let color = match row.tone {
        Tone::Normal | Tone::Quiet if selected => colors.accent,
        Tone::Normal => theme.sidebar_foreground,
        Tone::Quiet => theme.muted_foreground,
        Tone::Problem => theme.danger,
    };
    let guide = theme.sidebar_border;
    let guides = (0..depth).map(move |_| {
        div()
            .w(px(crate::design::INDENT))
            .h(px(crate::design::ROW_HEIGHT))
            .flex_shrink_0()
            .child(
                div()
                    .ml(px(crate::design::ICON / 2. - 0.5))
                    .w_px()
                    .h(px(crate::design::ROW_HEIGHT))
                    .bg(guide),
            )
    });
    let chevron = div()
        .w(px(crate::design::ICON))
        .flex_shrink_0()
        .when(folder, |this| {
            this.child(
                ui::icon(if expanded {
                    Glyph::ChevronDown
                } else {
                    Glyph::ChevronRight
                })
                .text_color(theme.muted_foreground),
            )
        });
    let glyph = match row.kind {
        Kind::Connection | Kind::Database => Some(Glyph::Database),
        Kind::Schema => Some(if expanded {
            Glyph::FolderOpen
        } else {
            Glyph::Folder
        }),
        Kind::Table => Some(Glyph::Table),
        Kind::View => Some(Glyph::Eye),
        Kind::Group => Some(Glyph::ListTree),
        Kind::Column { key: true } => Some(Glyph::KeyRound),
        Kind::Column { key: false } => Some(Glyph::Columns3),
        Kind::Index => Some(Glyph::Hash),
        Kind::ForeignKey => Some(Glyph::Link),
        Kind::Note => None,
    };
    let icon_color = match row.kind {
        Kind::Connection | Kind::Database => row
            .engine
            .map_or(theme.muted_foreground, |kind| design::engine_ink(kind, cx)),
        Kind::Table => colors.accent,
        Kind::View => colors.view,
        Kind::Column { key: true } | Kind::ForeignKey => colors.key,
        _ => theme.muted_foreground,
    };
    let badge = match (row.kind, row.engine) {
        (Kind::Connection, Some(kind)) => Some(kind.badge()),
        _ => None,
    };
    let dot = matches!(row.kind, Kind::Connection | Kind::Database).then(|| {
        let color = match row.secondary.as_deref() {
            Some("connected") => theme.success,
            Some("connecting") => theme.muted_foreground,
            Some("failed") => theme.danger,
            _ => theme.border,
        };
        div().size(px(8.)).flex_shrink_0().rounded_full().bg(color)
    });
    let secondary = row.secondary.filter(|_| row.kind != Kind::Connection);

    ListItem::new(ix)
        .h(px(crate::design::ROW_HEIGHT))
        .py_0()
        .px_3()
        .rounded(px(crate::design::RADIUS_ITEM))
        .child(
            h_flex()
                .w_full()
                .h_full()
                .min_w_0()
                .gap_2()
                .children(guides)
                .child(chevron)
                .when_some(glyph, |this, glyph| {
                    this.child(ui::icon(glyph).flex_shrink_0().text_color(icon_color))
                })
                .child(
                    div()
                        .min_w_0()
                        .flex_shrink_0()
                        .max_w(px(240.))
                        .truncate()
                        .text_color(color)
                        .when(row.strong || selected, |this| {
                            this.font_weight(FontWeight::MEDIUM)
                        })
                        .child(row.label),
                )
                .when_some(secondary, |this, secondary| {
                    this.child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(secondary),
                    )
                })
                .child(div().flex_1())
                .when_some(dot, |this, dot| this.child(dot))
                .when_some(badge, |this, badge| this.child(ui::menu_badge(badge, cx))),
        )
}
