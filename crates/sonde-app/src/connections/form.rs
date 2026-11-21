//! The dialog for adding or editing a saved connection.

use std::{collections::BTreeMap, time::Instant};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputEvent, InputState},
    radio::RadioGroup,
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use sonde_core::{
    ConnectionConfig, DbKind, Secret, ServerConfig, SqliteConfig, TlsMode, config::SavedConnection,
};

use super::Connections;

const TLS_MODES: [(TlsMode, &str, &str); 4] = [
    (TlsMode::Disable, "Off", "Never encrypt the connection."),
    (
        TlsMode::Prefer,
        "Prefer",
        "Encrypt when the server offers it. The server's certificate is not checked.",
    ),
    (
        TlsMode::Require,
        "Require",
        "Refuse to connect without encryption. The server's certificate is not checked.",
    ),
    (
        TlsMode::VerifyFull,
        "Verify",
        "Require encryption and check the certificate against this system's trusted roots.",
    ),
];

enum Test {
    Idle,
    Running,
    Passed(SharedString),
    Failed(SharedString),
}

pub(super) struct ConnectionForm {
    connections: Entity<Connections>,
    /// The connection being edited, or `None` for a new one.
    editing: Option<SavedConnection>,
    kind: DbKind,
    tls: TlsMode,
    read_only: bool,
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    database: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    path: Entity<InputState>,
    /// The settings particular to some kinds, by option key.
    options: Vec<(&'static str, Entity<InputState>)>,
    /// Saved options the form has no field for, such as `srv` from a
    /// `mongodb+srv://` URL. They are kept as they were.
    kept_options: BTreeMap<String, String>,
    /// List every database on the server in the sidebar.
    all_databases: bool,
    /// Why the form cannot be saved as it stands.
    problem: Option<SharedString>,
    test: Test,
    saving: bool,
    setup: bool,
    /// On the first step of a new connection: picking the engine.
    choosing: bool,
    test_revision: u64,
    _input_changes: Vec<Subscription>,
}

pub(super) struct ConnectionSaved {
    pub id: String,
    pub password: Option<Secret>,
}

impl EventEmitter<ConnectionSaved> for ConnectionForm {}

/// Open the connection dialog, blank or filled from `editing`.
pub fn open_connection_form(
    connections: Entity<Connections>,
    editing: Option<SavedConnection>,
    window: &mut Window,
    cx: &mut App,
) {
    let title = if editing.is_some() {
        "Edit connection"
    } else {
        "New connection"
    };
    let form = cx.new(|cx| ConnectionForm::new(connections, editing, window, cx));
    let name = form.read(cx).name.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let submit = form.clone();
        let footer = form.update(cx, |form, cx| form.footer(cx));
        dialog
            .title(title)
            .w(px(560.))
            .p_6()
            .on_ok(move |_, window, cx| {
                submit.update(cx, |form, cx| form.save(window, cx));
                // Save closes only after validation and persistence succeed.
                false
            })
            .footer(footer)
            .child(form.clone())
    });
    name.update(cx, |name, cx| name.focus(window, cx));
}

impl ConnectionForm {
    fn new(
        connections: Entity<Connections>,
        editing: Option<SavedConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let postgres = editing
            .as_ref()
            .and_then(|saved| saved.config.server().cloned());
        let kind = editing
            .as_ref()
            .map_or(DbKind::Postgres, |saved| saved.config.kind());
        let sqlite = match editing.as_ref().map(|saved| &saved.config) {
            Some(ConnectionConfig::Sqlite(config)) => Some(config.clone()),
            _ => None,
        };
        let has_keyring = connections.read(cx).secrets().is_some();
        let editing_none = editing.is_none();

        let mut text = |value: String, placeholder: &'static str| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(value)
                    .placeholder(placeholder)
            })
        };
        let name = text(
            editing
                .as_ref()
                .map(|saved| saved.name.clone())
                .unwrap_or_default(),
            "Shown in the sidebar",
        );
        let host = text(
            postgres
                .as_ref()
                .map_or_else(|| "localhost".into(), |c| c.host.clone()),
            "db.example.com",
        );
        let port = text(
            postgres
                .as_ref()
                .map_or_else(|| kind.default_port().unwrap_or(5432), |c| c.port)
                .to_string(),
            "Port",
        );
        let database = text(
            postgres
                .as_ref()
                .map(|c| c.database.clone())
                .unwrap_or_default(),
            "Database name",
        );
        let user = text(
            postgres
                .as_ref()
                .map(|c| c.user.clone())
                .unwrap_or_default(),
            "User name",
        );
        let path = text(
            sqlite
                .as_ref()
                .map(|c| c.path.to_string_lossy().into_owned())
                .unwrap_or_default(),
            "/path/to/database.db",
        );
        let mut option_keys: Vec<(&'static str, &'static str)> = Vec::new();
        for kind in DbKind::ALL {
            for (key, _, placeholder) in kind.option_fields() {
                if !option_keys.iter().any(|(k, _)| k == key) {
                    option_keys.push((key, placeholder));
                }
            }
        }
        let saved_options = postgres
            .as_ref()
            .map(|c| c.options.clone())
            .unwrap_or_default();
        let options: Vec<(&'static str, Entity<InputState>)> = option_keys
            .into_iter()
            .map(|(key, placeholder)| {
                let value = saved_options.get(key).cloned().unwrap_or_default();
                (key, text(value, placeholder))
            })
            .collect();
        let all_databases = saved_options
            .get(ServerConfig::ALL_DATABASES)
            .map(String::as_str)
            == Some("true");
        let kept_options = saved_options
            .into_iter()
            .filter(|(key, _)| key != ServerConfig::ALL_DATABASES)
            .filter(|(key, _)| !options.iter().any(|(k, _)| k == key))
            .collect();

        let password = cx.new(|cx| {
            InputState::new(window, cx).masked(true).placeholder(
                if editing.is_some() && has_keyring {
                    "Leave empty to keep the stored password"
                } else {
                    "Password"
                },
            )
        });

        let input_changes = [&name, &host, &port, &database, &user, &password, &path]
            .into_iter()
            .chain(options.iter().map(|(_, input)| input))
            .map(|input| {
                cx.subscribe(input, |this: &mut Self, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.changed(cx);
                    }
                })
            })
            .collect();

        Self {
            connections,
            kind,
            tls: postgres.as_ref().map_or_else(TlsMode::default, |c| c.tls),
            read_only: sqlite.as_ref().is_some_and(|c| c.read_only),
            editing,
            name,
            host,
            port,
            database,
            user,
            password,
            path,
            options,
            kept_options,
            all_databases,
            problem: None,
            test: Test::Idle,
            saving: false,
            setup: false,
            choosing: editing_none,
            test_revision: 0,
            _input_changes: input_changes,
        }
    }

    pub(super) fn for_setup(
        connections: Entity<Connections>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut form = Self::new(connections, None, window, cx);
        form.setup = true;
        form.choosing = false;
        form
    }

    pub(super) fn choose(&mut self, kind: DbKind, window: &mut Window, cx: &mut Context<Self>) {
        if self.kind == kind {
            return;
        }
        // A port still at the old engine's default follows the new engine.
        let port = Self::value(&self.port, cx).parse::<u16>().ok();
        if let Some(new) = kind.default_port()
            && (port.is_none() || port == self.kind.default_port())
        {
            self.port
                .update(cx, |input, cx| input.set_value(new.to_string(), window, cx));
        }
        self.kind = kind;
        self.changed(cx);
    }

    pub(super) fn import_url(
        &mut self,
        imported: super::url::ImportedConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.kind = imported.config.kind();
        let mut values = vec![(self.name.clone(), imported.name)];
        match imported.config {
            ConnectionConfig::Sqlite(config) => {
                self.read_only = config.read_only;
                values.push((
                    self.path.clone(),
                    config.path.to_string_lossy().into_owned(),
                ));
                values.push((self.password.clone(), String::new()));
            }
            server => {
                let config = server
                    .server()
                    .cloned()
                    .expect("every other kind is a server");
                self.tls = config.tls;
                for (key, input) in &self.options {
                    values.push((
                        input.clone(),
                        config.options.get(*key).cloned().unwrap_or_default(),
                    ));
                }
                self.all_databases = config.all_databases();
                self.kept_options = config
                    .options
                    .iter()
                    .filter(|(key, _)| *key != ServerConfig::ALL_DATABASES)
                    .filter(|(key, _)| !self.options.iter().any(|(k, _)| k == key))
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect();
                values.extend([
                    (self.host.clone(), config.host),
                    (self.port.clone(), config.port.to_string()),
                    (self.database.clone(), config.database),
                    (self.user.clone(), config.user),
                    (
                        self.password.clone(),
                        imported
                            .password
                            .as_ref()
                            .map_or_else(String::new, |password| password.expose().to_owned()),
                    ),
                ]);
            }
        }
        for (input, value) in values {
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        }
        self.changed(cx);
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        self.test_revision += 1;
        self.test = Test::Idle;
        self.problem = None;
        cx.notify();
    }

    /// Step one done: the engine is chosen, on to its details.
    fn pick(&mut self, kind: DbKind, window: &mut Window, cx: &mut Context<Self>) {
        self.choose(kind, window, cx);
        self.choosing = false;
        self.focus_name(window, cx);
        // The dialog's footer is built with the dialog, not with this view.
        window.refresh();
        cx.notify();
    }

    /// Back to step one, keeping what was typed.
    fn change_engine(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.choosing = true;
        window.refresh();
        cx.notify();
    }

    pub(super) fn focus_name(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.name.update(cx, |name, cx| name.focus(window, cx));
    }

    pub(super) fn review(&self, cx: &App) -> Result<(String, ConnectionConfig), SharedString> {
        let config = self.config(cx)?;
        let name = Self::value(&self.name, cx);
        if name.is_empty() {
            return Err("Give the connection a name.".into());
        }
        Ok((name, config))
    }

    pub(super) fn validate(&mut self, cx: &mut Context<Self>) -> bool {
        self.problem = self.review(cx).err();
        cx.notify();
        self.problem.is_none()
    }

    pub(super) fn busy(&self) -> bool {
        self.saving || self.testing()
    }

    pub(super) fn testing(&self) -> bool {
        matches!(self.test, Test::Running)
    }

    pub(super) fn saving(&self) -> bool {
        self.saving
    }

    pub(super) fn feedback(&self, cx: &App) -> impl IntoElement + use<> {
        let (status, color) = match (&self.problem, &self.test) {
            (Some(problem), _) => (Some(problem.clone()), cx.theme().danger),
            (None, _) if self.saving => (
                Some("Saving connection".into()),
                cx.theme().muted_foreground,
            ),
            (None, Test::Running) => (Some("Connecting".into()), cx.theme().muted_foreground),
            (None, Test::Passed(message)) => (Some(message.clone()), cx.theme().success),
            (None, Test::Failed(message)) => (Some(message.clone()), cx.theme().danger),
            (None, Test::Idle) => (None, cx.theme().muted_foreground),
        };
        div()
            .id("connection-status")
            .test_support()
            .role(Role::Status)
            .min_h_5()
            .text_sm()
            .text_color(color)
            .when_some(status, |this, status| {
                this.aria_label(status.clone()).child(status)
            })
    }

    fn value(input: &Entity<InputState>, cx: &App) -> String {
        input.read(cx).value().trim().to_owned()
    }

    /// The configuration the fields describe, or what is wrong with them.
    fn config(&self, cx: &App) -> Result<ConnectionConfig, SharedString> {
        match self.kind {
            DbKind::Sqlite => {
                let path = Self::value(&self.path, cx);
                if path.is_empty() {
                    return Err("Choose the SQLite database file.".into());
                }
                Ok(ConnectionConfig::Sqlite(SqliteConfig {
                    path: path.into(),
                    read_only: self.read_only,
                }))
            }
            kind => {
                let host = Self::value(&self.host, cx);
                let database = Self::value(&self.database, cx);
                let user = Self::value(&self.user, cx);
                if host.is_empty() {
                    return Err(if kind == DbKind::Snowflake {
                        "Enter the account identifier, such as myorg-myaccount.".into()
                    } else {
                        "Enter the server's host name or address.".into()
                    });
                }
                let port = Self::value(&self.port, cx)
                    .parse::<u16>()
                    .ok()
                    .filter(|&port| port != 0)
                    .ok_or("The port must be a number from 1 to 65535.")?;
                if user.is_empty() && !kind.user_optional() {
                    return Err("Enter the user to log in as.".into());
                }
                let mut options = self.kept_options.clone();
                if self.all_databases && kind.lists_databases() {
                    options.insert(ServerConfig::ALL_DATABASES.to_owned(), "true".to_owned());
                }
                for (key, ..) in kind.option_fields() {
                    if let Some((_, input)) = self.options.iter().find(|(k, _)| k == key) {
                        let value = Self::value(input, cx);
                        if !value.is_empty() {
                            options.insert((*key).to_owned(), value);
                        }
                    }
                }
                let server = ServerConfig {
                    host,
                    port,
                    // Postgres itself falls back to a database named after
                    // the user; saying so explicitly keeps the saved file
                    // honest. MySQL can connect to no database at all.
                    database: if database.is_empty() && kind == DbKind::Postgres {
                        user.clone()
                    } else {
                        database
                    },
                    user,
                    tls: if kind == DbKind::Snowflake {
                        TlsMode::VerifyFull
                    } else {
                        self.tls
                    },
                    options,
                };
                ConnectionConfig::for_server(kind, server)
                    .ok_or_else(|| "That database is not a server.".into())
            }
        }
    }

    /// The password typed into the form, if any.
    fn typed_password(&self, cx: &App) -> Option<Secret> {
        // Not trimmed: a password may legitimately start or end with a space.
        let typed = self.password.read(cx).value();
        (self.kind.is_server() && !typed.is_empty()).then(|| Secret::new(typed.to_string()))
    }

    pub(super) fn test(&mut self, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        let config = match self.config(cx) {
            Ok(config) => config,
            Err(problem) => {
                self.problem = Some(problem);
                cx.notify();
                return;
            }
        };
        self.problem = None;
        self.test = Test::Running;
        self.test_revision += 1;
        let revision = self.test_revision;
        cx.notify();

        let typed = self.typed_password(cx);
        // With nothing typed while editing, test with the stored password.
        let stored = self
            .editing
            .as_ref()
            .map(|saved| saved.id.clone())
            .zip(self.connections.read(cx).secrets())
            .filter(|_| typed.is_none() && self.kind.is_server());

        cx.spawn(async move |this, cx| {
            let started = Instant::now();
            let secret = match stored {
                Some((id, secrets)) => cx.background_spawn(async move { secrets.get(&id) }).await,
                None => Ok(typed),
            };
            let outcome = match secret {
                Ok(secret) => sonde_core::connect(&config, secret).await,
                Err(error) => Err(error),
            };
            let test = match outcome {
                Ok(session) => {
                    // Only reachability was in question; a failed close
                    // changes nothing about that.
                    let _ = session.close().await;
                    Test::Passed(
                        format!("Connected in {} ms.", started.elapsed().as_millis()).into(),
                    )
                }
                Err(error) => Test::Failed(error.to_string().into()),
            };
            // The dialog may have been closed meanwhile.
            let _ = this.update(cx, |this, cx| {
                if this.test_revision != revision {
                    return;
                }
                this.test = test;
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // On the engine step there is nothing to save yet.
        if self.saving || self.choosing {
            return;
        }
        let (name, config) = match self.review(cx) {
            Ok(draft) => draft,
            Err(problem) => {
                self.problem = Some(problem);
                cx.notify();
                return;
            }
        };
        let saved = SavedConnection {
            id: self.editing.as_ref().map_or_else(
                || uuid::Uuid::new_v4().to_string(),
                |saved| saved.id.clone(),
            ),
            name,
            config,
        };
        let typed = self.typed_password(cx);
        let password = typed.clone().zip(self.connections.read(cx).secrets());
        let id = saved.id.clone();
        self.saving = true;
        self.problem = None;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            // Password first: a connection saved without the password the
            // user just typed would fail later with no hint as to why.
            let stored = match password {
                Some((password, secrets)) => {
                    let id = saved.id.clone();
                    cx.background_spawn(async move { secrets.set(&id, &password) })
                        .await
                }
                None => Ok(()),
            };
            // The dialog may have been closed meanwhile.
            let _ = this.update_in(cx, |this, window, cx| {
                let outcome = stored.and_then(|()| {
                    this.connections
                        .update(cx, |connections, cx| connections.upsert(saved, cx))
                });
                match outcome {
                    Ok(()) if this.setup => cx.emit(ConnectionSaved {
                        id,
                        password: typed,
                    }),
                    Ok(()) => window.close_dialog(cx),
                    Err(error) => {
                        this.saving = false;
                        this.problem = Some(error.to_string().into());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose database".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            // Cancelled, or the platform has no file dialog: leave the field.
            let Ok(Ok(Some(paths))) = chosen.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                let text = path.to_string_lossy().into_owned();
                this.path
                    .update(cx, |input, cx| input.set_value(text, window, cx));
            });
        })
        .detach();
    }

    fn footer(&self, cx: &Context<Self>) -> impl IntoElement + use<> {
        if self.choosing {
            return h_flex().gap_2().child(div().flex_1()).child(
                Button::new("cancel")
                    .ghost()
                    .h_9()
                    .label("Cancel")
                    .on_click(|_, window, cx| window.close_dialog(cx)),
            );
        }
        // A new connection goes back to the engine step.
        let back = self.editing.is_none() && !self.setup;
        h_flex()
            .gap_2()
            .when(back, |this| {
                this.child(
                    Button::new("back")
                        .ghost()
                        .h_9()
                        .icon(IconName::ChevronLeft)
                        .label("Back")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.change_engine(window, cx)),
                        ),
                )
            })
            .child(
                Button::new("test")
                    .outline()
                    .h_9()
                    .label("Test connection")
                    .loading(matches!(self.test, Test::Running))
                    .disabled(matches!(self.test, Test::Running))
                    .on_click(cx.listener(|this, _, _, cx| this.test(cx))),
            )
            .child(div().flex_1())
            .child(
                Button::new("cancel")
                    .ghost()
                    .h_9()
                    .label("Cancel")
                    .on_click(|_, window, cx| window.close_dialog(cx)),
            )
            .child(
                Button::new("save")
                    .primary()
                    .h_9()
                    .label(if self.editing.is_some() {
                        "Save changes"
                    } else {
                        "Save connection"
                    })
                    .loading(self.saving)
                    .disabled(self.saving)
                    .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
            )
    }
}

/// "1 Database  —  2 Details", with the current step filled in.
fn stepper(current: usize, cx: &App) -> Div {
    let theme = cx.theme();
    let step = |n: usize, label: &'static str| {
        let active = n == current;
        let done = n < current;
        h_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .size(px(20.))
                    .flex_none()
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .when(active || done, |this| {
                        this.bg(theme.primary).text_color(theme.primary_foreground)
                    })
                    .when(!active && !done, |this| {
                        this.border_1()
                            .border_color(theme.border)
                            .text_color(theme.muted_foreground)
                    })
                    .child(n.to_string()),
            )
            .child(
                div()
                    .text_sm()
                    .when(active, |this| this.font_weight(FontWeight::MEDIUM))
                    .when(!active, |this| this.text_color(theme.muted_foreground))
                    .child(label),
            )
    };
    h_flex()
        .gap_3()
        .items_center()
        .child(step(1, "Database"))
        .child(div().h(px(1.)).w_8().bg(theme.border))
        .child(step(2, "Details"))
}

/// A labelled field: the label is visible text and also names the control.
fn field(label: &'static str, input: &Entity<InputState>) -> Div {
    v_flex()
        .flex_1()
        .min_w_0()
        .gap_2()
        .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(label))
        .child(Input::new(input).id(label).aria_label(label))
}

impl Render for ConnectionForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_keyring = self.connections.read(cx).secrets().is_some();
        let theme = cx.theme();
        let quiet = theme.muted_foreground;

        let kind = self.kind;
        let option_fields: Vec<Div> = kind
            .option_fields()
            .iter()
            .filter_map(|(key, label, _)| {
                self.options
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, input)| field(label, input))
            })
            .collect();
        let fields =
            match kind {
                kind if kind.is_server() => {
                    let (tls_index, tls_help) = TLS_MODES
                        .iter()
                        .enumerate()
                        .find(|(_, (mode, ..))| *mode == self.tls)
                        .map_or((0, ""), |(ix, (_, _, help))| (ix, *help));
                    let snowflake = kind == DbKind::Snowflake;
                    let host_label = if snowflake { "Account" } else { "Host" };
                    let password_label = match kind {
                        DbKind::Snowflake => "Access token",
                        DbKind::Elasticsearch => "Password or API key",
                        _ => "Password",
                    };
                    let user_label = if kind.user_optional() {
                        "User (optional)"
                    } else {
                        "User"
                    };
                    v_flex()
                    .gap_4()
                    .child(
                        h_flex()
                            .gap_3()
                            .items_start()
                            .child(field(host_label, &self.host))
                            .child(field("Port", &self.port).flex_none().w_24()),
                    )
                    .child(
                        h_flex()
                            .gap_3()
                            .items_start()
                            .when_some(kind.database_label(), |this, label| {
                                this.child(field(label, &self.database))
                            })
                            .child(field(user_label, &self.user)),
                    )
                    .when(kind.lists_databases(), |this| {
                        let noun = if kind == DbKind::Cassandra { "keyspace" } else { "database" };
                        let (label, help) = match kind {
                            DbKind::Redis => (
                                "Show every database number in use".to_owned(),
                                "Off: only the database number above. On: each one in use \
                                 appears under the connection with its keys."
                                    .to_owned(),
                            ),
                            kind if kind.session_per_database() => (
                                "Show every database on the server".to_owned(),
                                "Off: only the database above. On: each one appears under the \
                                 connection with its own schemas; the one above is where Sonde \
                                 signs in."
                                    .to_owned(),
                            ),
                            _ => (
                                format!("Show every {noun} on the server"),
                                format!(
                                    "Off: only the {noun} above, or all of them when it is empty. \
                                     On: all of them, with the one above used by default."
                                ),
                            ),
                        };
                        this.child(
                            v_flex()
                                .gap_1()
                                .child(
                                    Checkbox::new("all-databases")
                                        .label(label)
                                        .checked(self.all_databases)
                                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                            this.all_databases = *checked;
                                            this.changed(cx);
                                        })),
                                )
                                .child(div().pl_6().text_xs().text_color(quiet).child(help)),
                        )
                    })
                    .when(!option_fields.is_empty(), |this| {
                        this.child(h_flex().gap_3().items_start().children(option_fields))
                    })
                    .child(
                        v_flex()
                            .gap_2()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(password_label),
                            )
                            .child(
                                Input::new(&self.password)
                                    .id("Password")
                                    .aria_label(password_label)
                                    .mask_toggle(),
                            )
                            .child(div().text_xs().text_color(quiet).child(if snowflake {
                                "A programmatic access token for the user, from Snowsight. \
                                 Stored in the system keyring."
                            } else if has_keyring {
                                "Stored in the system keyring, never in Sonde's files."
                            } else if self.setup {
                                "Used for this session only. Your password is not saved."
                            } else {
                                "No keyring is running, so this is used for testing only \
                                 and not saved. You will be asked for it when connecting."
                            })),
                    )
                    .when(!snowflake, |this| {
                        this.child(
                            v_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child("Encryption"),
                                )
                                .child(
                                    RadioGroup::horizontal("tls")
                                        .children(TLS_MODES.map(|(_, label, _)| label))
                                        .selected_index(Some(tls_index))
                                        .on_click(cx.listener(|this, ix: &usize, _, cx| {
                                            if let Some((mode, ..)) = TLS_MODES.get(*ix) {
                                                this.tls = *mode;
                                                this.changed(cx);
                                            }
                                        })),
                                )
                                .child(div().text_xs().text_color(quiet).child(tls_help)),
                        )
                    })
                }
                _ => v_flex()
                    .gap_3()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_end()
                            .child(field("Database file", &self.path))
                            .child(Button::new("browse").outline().label("Browse").on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.browse(window, cx);
                                }),
                            )),
                    )
                    .child(
                        Checkbox::new("read-only")
                            .label("Open read-only")
                            .checked(self.read_only)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.read_only = *checked;
                                this.changed(cx);
                            })),
                    ),
            };

        if self.choosing {
            let form = cx.weak_entity();
            return v_flex()
                .gap_5()
                .child(stepper(1, cx))
                .child(
                    div()
                        .text_sm()
                        .text_color(quiet)
                        .child("Pick the database to connect to."),
                )
                .child(super::engines::grid("kind", cx, |kind| {
                    let form = form.clone();
                    Box::new(move |_, window, cx| {
                        let _ = form.update(cx, |this, cx| this.pick(kind, window, cx));
                    })
                }));
        }

        v_flex()
            .gap_5()
            .when(!self.setup, |this| {
                let editing = self.editing.is_some();
                this.when(!editing, |this| this.child(stepper(2, cx)))
                    .child(
                        h_flex()
                            .gap_3()
                            .pb_4()
                            .border_b_1()
                            .border_color(cx.theme().border)
                            .child(
                                Icon::new(if kind.is_server() {
                                    IconName::Network
                                } else {
                                    IconName::HardDrive
                                })
                                .with_size(px(16.))
                                .text_color(crate::design::engine_ink(kind, cx)),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::MEDIUM)
                                            .child(kind.name()),
                                    )
                                    .when_some(kind.also(), |this, also| {
                                        this.child(
                                            div()
                                                .text_xs()
                                                .text_color(quiet)
                                                .child(format!("Also {also}")),
                                        )
                                    }),
                            ),
                    )
            })
            .child(field("Name", &self.name))
            .child(fields)
            // The line is always present so the buttons do not jump when a
            // message appears.
            .child(self.feedback(cx))
    }
}
