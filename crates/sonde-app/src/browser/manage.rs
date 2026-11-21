//! The dialog behind the sidebar's create, rename, empty and drop commands.
//!
//! It asks only for what the change needs, shows the statements it will
//! run as they take shape, and runs them on the connection's session. The
//! sidebar reloads the connection afterwards.

use std::sync::Arc;

use futures::StreamExt as _;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputEvent, InputState},
    radio::RadioGroup,
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use sonde_core::{
    DbKind, Language, Session, TableRef,
    ddl::{self, Change, KeyType, NewColumn, Op},
};

/// What the dialog changes, with what is already known about it.
#[derive(Clone)]
pub(super) enum Task {
    CreateDatabase,
    DropDatabase(String),
    CreateSchema,
    DropSchema(String),
    CreateTable { schema: String },
    RenameTable(TableRef),
    EmptyTable(TableRef),
    DropTable(TableRef),
    AddColumn(TableRef),
    RenameColumn { table: TableRef, column: String },
    DropColumn { table: TableRef, column: String },
    CreateKey,
    FlushDatabase(String),
}

/// The word a database uses for its tables.
pub(super) fn table_noun(kind: DbKind) -> &'static str {
    match kind {
        DbKind::Mongodb => "collection",
        DbKind::Elasticsearch => "index",
        DbKind::Redis => "key",
        _ => "table",
    }
}

/// The word a database uses for its columns.
pub(super) fn column_noun(kind: DbKind) -> &'static str {
    match kind {
        DbKind::Mongodb | DbKind::Elasticsearch => "field",
        _ => "column",
    }
}

/// The word a database uses for a database.
pub(super) fn database_noun(kind: DbKind) -> &'static str {
    match kind {
        DbKind::Cassandra => "keyspace",
        _ => "database",
    }
}

fn capital(word: &str) -> String {
    let mut chars = word.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

impl Task {
    pub(super) fn op(&self) -> Op {
        match self {
            Self::CreateDatabase => Op::CreateDatabase,
            Self::DropDatabase(_) => Op::DropDatabase,
            Self::CreateSchema => Op::CreateSchema,
            Self::DropSchema(_) => Op::DropSchema,
            Self::CreateTable { .. } => Op::CreateTable,
            Self::RenameTable(_) => Op::RenameTable,
            Self::EmptyTable(_) => Op::EmptyTable,
            Self::DropTable(_) => Op::DropTable,
            Self::AddColumn(_) => Op::AddColumn,
            Self::RenameColumn { .. } => Op::RenameColumn,
            Self::DropColumn { .. } => Op::DropColumn,
            Self::CreateKey => Op::CreateKey,
            Self::FlushDatabase(_) => Op::FlushDatabase,
        }
    }

    /// The menu entry and dialog title.
    pub(super) fn title(&self, kind: DbKind) -> String {
        let table = table_noun(kind);
        let column = column_noun(kind);
        let database = database_noun(kind);
        match self {
            Self::CreateDatabase => format!("New {database}…"),
            Self::DropDatabase(_) => format!("Drop {database}…"),
            Self::CreateSchema => "New schema…".into(),
            Self::DropSchema(_) => "Drop schema…".into(),
            Self::CreateTable { .. } => format!("New {table}…"),
            Self::RenameTable(_) => "Rename…".into(),
            Self::EmptyTable(_) => format!("Empty {table}…"),
            Self::DropTable(_) if kind == DbKind::Redis => "Delete key…".into(),
            Self::DropTable(_) => format!("Drop {table}…"),
            Self::AddColumn(_) => format!("Add {column}…"),
            Self::RenameColumn { .. } => format!("Rename {column}…"),
            Self::DropColumn { .. } => format!("Drop {column}…"),
            Self::CreateKey => "New key…".into(),
            Self::FlushDatabase(_) => "Delete every key…".into(),
        }
    }

    /// The menu entry's icon.
    pub(super) fn glyph(&self) -> crate::ui::Glyph {
        use crate::ui::Glyph;
        match self {
            Self::CreateDatabase => Glyph::DatabasePlus,
            Self::CreateSchema => Glyph::FolderPlus,
            Self::CreateTable { .. } | Self::CreateKey | Self::AddColumn(_) => Glyph::Plus,
            Self::RenameTable(_) | Self::RenameColumn { .. } => Glyph::PencilLine,
            Self::EmptyTable(_) => Glyph::Eraser,
            _ => Glyph::Trash,
        }
    }

    /// Whether the change destroys data, which makes its button red.
    pub(super) fn destructive(&self) -> bool {
        matches!(
            self,
            Self::DropDatabase(_)
                | Self::DropSchema(_)
                | Self::EmptyTable(_)
                | Self::DropTable(_)
                | Self::DropColumn { .. }
                | Self::FlushDatabase(_)
        )
    }

    /// What a destructive change takes with it, in words.
    fn warning(&self, kind: DbKind) -> Option<String> {
        let table = table_noun(kind);
        Some(match self {
            Self::DropDatabase(name) => format!(
                "This deletes the {} {name} and everything in it. It cannot be undone.",
                database_noun(kind)
            ),
            Self::DropSchema(name) => {
                format!(
                    "This deletes the schema {name}. The database refuses if it still holds tables."
                )
            }
            Self::EmptyTable(t) => format!(
                "This deletes every row of the {table} {}. It cannot be undone.",
                t.name
            ),
            Self::DropTable(t) => format!(
                "This deletes the {table} {} and its data. It cannot be undone.",
                t.name
            ),
            Self::DropColumn { table: t, column } => format!(
                "This deletes the {} {column} from {} and its values. It cannot be undone.",
                column_noun(kind),
                t.name
            ),
            Self::FlushDatabase(name) => {
                format!("This deletes every key in {name}. It cannot be undone.")
            }
            _ => return None,
        })
    }
}

struct ColumnRow {
    name: Entity<InputState>,
    type_name: Entity<InputState>,
    key: bool,
    required: bool,
}

pub(super) struct ManageDialog {
    kind: DbKind,
    task: Task,
    session: Arc<dyn Session>,
    name: Entity<InputState>,
    /// MongoDB's first collection, or a Redis key's first value.
    second: Entity<InputState>,
    columns: Vec<ColumnRow>,
    key_type: KeyType,
    running: bool,
    problem: Option<SharedString>,
    on_done: Rc<dyn Fn(&mut App)>,
    _subscriptions: Vec<Subscription>,
}

use std::rc::Rc;

impl ManageDialog {
    fn new(
        kind: DbKind,
        task: Task,
        session: Arc<dyn Session>,
        on_done: Rc<dyn Fn(&mut App)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let current = match &task {
            Task::RenameTable(table) => table.name.clone(),
            Task::RenameColumn { column, .. } => column.clone(),
            _ => String::new(),
        };
        let placeholder = match &task {
            Task::CreateKey => "user:1",
            _ => "Name",
        };
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(current)
                .placeholder(placeholder)
        });
        let second = cx.new(|cx| InputState::new(window, cx));
        let mut dialog = Self {
            kind,
            task,
            session,
            name,
            second,
            columns: Vec::new(),
            key_type: KeyType::String,
            running: false,
            problem: None,
            on_done,
            _subscriptions: Vec::new(),
        };
        match &dialog.task {
            Task::CreateTable { .. } if !matches!(kind, DbKind::Mongodb) => {
                let id = if kind == DbKind::Elasticsearch {
                    None
                } else {
                    Some("id")
                };
                if let Some(id) = id {
                    let int = match kind {
                        DbKind::Sqlite => "INTEGER",
                        DbKind::Snowflake => "NUMBER",
                        DbKind::Cassandra | DbKind::Mysql | DbKind::Mariadb | DbKind::SqlServer => {
                            "int"
                        }
                        _ => "bigint",
                    };
                    dialog.add_column(id, int, true, window, cx);
                }
                dialog.add_column("", ddl::default_type(kind), false, window, cx);
            }
            Task::AddColumn(_) if kind != DbKind::Mongodb => {
                dialog.add_column("", ddl::default_type(kind), false, window, cx);
            }
            _ => {}
        }
        for input in [dialog.name.clone(), dialog.second.clone()] {
            dialog
                ._subscriptions
                .push(cx.subscribe(&input, Self::on_input));
        }
        dialog
    }

    fn on_input(&mut self, _: Entity<InputState>, event: &InputEvent, cx: &mut Context<Self>) {
        if matches!(event, InputEvent::Change) {
            self.problem = None;
            cx.notify();
        }
    }

    fn add_column(
        &mut self,
        name: &str,
        type_name: &str,
        key: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(name.to_owned())
                .placeholder("column")
        });
        let type_name = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(type_name.to_owned())
                .placeholder("type")
        });
        self._subscriptions
            .push(cx.subscribe(&name, Self::on_input));
        self._subscriptions
            .push(cx.subscribe(&type_name, Self::on_input));
        self.columns.push(ColumnRow {
            name,
            type_name,
            key,
            required: key,
        });
        cx.notify();
    }

    fn text(input: &Entity<InputState>, cx: &App) -> String {
        input.read(cx).value().trim().to_owned()
    }

    fn new_columns(&self, cx: &App) -> Vec<NewColumn> {
        self.columns
            .iter()
            .map(|row| NewColumn {
                name: Self::text(&row.name, cx),
                type_name: Self::text(&row.type_name, cx),
                nullable: !row.required,
                primary_key: row.key,
            })
            .collect()
    }

    /// The change the dialog describes, or what it is missing.
    fn change(&self, cx: &App) -> Result<Change, String> {
        let name = Self::text(&self.name, cx);
        Ok(match &self.task {
            Task::CreateDatabase => Change::CreateDatabase {
                name,
                first: (self.kind == DbKind::Mongodb).then(|| Self::text(&self.second, cx)),
            },
            Task::DropDatabase(db) => Change::DropDatabase(db.clone()),
            Task::CreateSchema => Change::CreateSchema(name),
            Task::DropSchema(schema) => Change::DropSchema(schema.clone()),
            Task::CreateTable { schema } => Change::CreateTable {
                table: TableRef::new(schema.clone(), name),
                columns: self.new_columns(cx),
            },
            Task::RenameTable(table) => Change::RenameTable {
                table: table.clone(),
                to: name,
            },
            Task::EmptyTable(table) => Change::EmptyTable(table.clone()),
            Task::DropTable(table) => Change::DropTable(table.clone()),
            Task::AddColumn(table) => Change::AddColumn {
                table: table.clone(),
                column: self
                    .new_columns(cx)
                    .into_iter()
                    .next()
                    .unwrap_or(NewColumn {
                        name,
                        type_name: String::new(),
                        nullable: true,
                        primary_key: false,
                    }),
            },
            Task::RenameColumn { table, column } => Change::RenameColumn {
                table: table.clone(),
                column: column.clone(),
                to: name,
            },
            Task::DropColumn { table, column } => Change::DropColumn {
                table: table.clone(),
                column: column.clone(),
            },
            Task::CreateKey => Change::CreateKey {
                key: name,
                kind: self.key_type,
                value: Self::text(&self.second, cx),
            },
            Task::FlushDatabase(_) => Change::FlushDatabase,
        })
    }

    fn statements(&self, cx: &App) -> Result<Vec<String>, String> {
        ddl::statements(self.kind, &self.change(cx)?)
    }

    fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        let statements = match self.statements(cx) {
            Ok(statements) => statements,
            Err(problem) => {
                self.problem = Some(problem.into());
                cx.notify();
                return;
            }
        };
        self.running = true;
        self.problem = None;
        cx.notify();
        let session = Arc::clone(&self.session);
        let window_handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let mut outcome = Ok(());
            for sql in &statements {
                let result = async {
                    let mut handle = session.execute(sql, &[]).await?;
                    while let Some(page) = handle.pages.next().await {
                        page?;
                    }
                    Ok::<_, sonde_core::Error>(())
                }
                .await;
                if let Err(error) = result {
                    outcome = Err(error.to_string());
                    break;
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.running = false;
                match outcome {
                    Ok(()) => {
                        (this.on_done)(cx);
                        let _ = window_handle.update(cx, |_, window, cx| window.close_dialog(cx));
                    }
                    Err(problem) => this.problem = Some(problem.into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn label(text: impl Into<SharedString>) -> Div {
        div()
            .text_sm()
            .font_weight(FontWeight::MEDIUM)
            .child(text.into())
    }
}

impl Render for ManageDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let quiet = theme.muted_foreground;
        let kind = self.kind;
        let table = table_noun(kind);
        let column = column_noun(kind);

        let mut body = v_flex().gap_4();
        if let Some(warning) = self.task.warning(kind) {
            body = body.child(div().text_sm().text_color(theme.danger).child(warning));
        }
        let name_label = match &self.task {
            Task::CreateDatabase => Some(format!("{} name", capital(database_noun(kind)))),
            Task::CreateSchema => Some("Schema name".into()),
            Task::CreateTable { .. } => Some(format!("{} name", capital(table))),
            Task::RenameTable(_) => Some("New name".into()),
            Task::RenameColumn { .. } => Some("New name".into()),
            Task::CreateKey => Some("Key".into()),
            Task::AddColumn(_) if kind == DbKind::Mongodb => Some("Field name".into()),
            _ => None,
        };
        if let Some(label) = name_label {
            body = body.child(
                v_flex()
                    .gap_2()
                    .child(Self::label(label.clone()))
                    .child(Input::new(&self.name).id("manage-name").aria_label(label)),
            );
        }
        if matches!(self.task, Task::CreateDatabase) && kind == DbKind::Mongodb {
            body = body.child(
                v_flex()
                    .gap_2()
                    .child(Self::label("First collection"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(quiet)
                            .child("MongoDB keeps a database only once it holds a collection."),
                    )
                    .child(
                        Input::new(&self.second)
                            .id("manage-second")
                            .aria_label("First collection"),
                    ),
            );
        }
        if matches!(self.task, Task::CreateKey) {
            let selected = KeyType::ALL.iter().position(|k| *k == self.key_type);
            let hint = match self.key_type {
                KeyType::String => "The value. Quote it if it has spaces.",
                KeyType::Hash => "Field and value pairs, such as: name ada lang en",
                KeyType::List | KeyType::Set => "Elements, separated by spaces.",
                KeyType::SortedSet => "Score and member pairs, such as: 1 ada 2 alan",
            };
            body = body
                .child(
                    v_flex().gap_2().child(Self::label("Type")).child(
                        RadioGroup::horizontal("key-type")
                            .children(KeyType::ALL.map(KeyType::name))
                            .selected_index(selected)
                            .on_click(cx.listener(|this, ix: &usize, _, cx| {
                                if let Some(kind) = KeyType::ALL.get(*ix) {
                                    this.key_type = *kind;
                                    cx.notify();
                                }
                            })),
                    ),
                )
                .child(
                    v_flex()
                        .gap_2()
                        .child(Self::label("Value"))
                        .child(div().text_xs().text_color(quiet).child(hint))
                        .child(
                            Input::new(&self.second)
                                .id("manage-second")
                                .aria_label("Value"),
                        ),
                );
        }
        if !self.columns.is_empty() {
            let keyed = matches!(self.task, Task::CreateTable { .. })
                && kind.language() != Language::Elastic;
            let mut list = v_flex()
                .gap_2()
                .child(Self::label(format!("{}s", capital(column))));
            for (ix, row) in self.columns.iter().enumerate() {
                let mut line = h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&row.name)
                                .id(SharedString::from(format!("column-name-{ix}")))
                                .aria_label(format!("{} {} name", capital(column), ix + 1)),
                        ),
                    )
                    .child(
                        div().w(px(150.)).flex_none().child(
                            Input::new(&row.type_name)
                                .id(SharedString::from(format!("column-type-{ix}")))
                                .aria_label(format!("{} {} type", capital(column), ix + 1)),
                        ),
                    );
                if keyed {
                    line = line.child(
                        Checkbox::new(SharedString::from(format!("column-key-{ix}")))
                            .label("Key")
                            .checked(row.key)
                            .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                if let Some(row) = this.columns.get_mut(ix) {
                                    row.key = *checked;
                                    row.required |= *checked;
                                }
                                cx.notify();
                            })),
                    );
                }
                if kind.language() != Language::Elastic && kind != DbKind::Cassandra {
                    line = line.child(
                        Checkbox::new(SharedString::from(format!("column-required-{ix}")))
                            .label("Required")
                            .checked(row.required)
                            .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                if let Some(row) = this.columns.get_mut(ix) {
                                    row.required = *checked || row.key;
                                }
                                cx.notify();
                            })),
                    );
                }
                if keyed && self.columns.len() > 1 {
                    line = line.child(
                        Button::new(SharedString::from(format!("column-remove-{ix}")))
                            .ghost()
                            .small()
                            .icon(IconName::Close)
                            .accessibility_label("Remove column")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if ix < this.columns.len() {
                                    this.columns.remove(ix);
                                }
                                cx.notify();
                            })),
                    );
                }
                list = list.child(line);
            }
            if matches!(self.task, Task::CreateTable { .. }) {
                let default = ddl::default_type(kind);
                list = list.child(
                    h_flex().child(
                        Button::new("add-column-row")
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .label(format!("Add {column}"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.add_column("", default, false, window, cx);
                            })),
                    ),
                );
            }
            body = body.child(list);
        }

        let preview = self.statements(cx);
        body = body.child(
            v_flex().gap_2().child(Self::label("Runs")).child(
                div()
                    .id("manage-preview")
                    .p_3()
                    .rounded(px(crate::design::RADIUS_ITEM))
                    .bg(theme.muted)
                    .font_family(theme.mono_font_family.clone())
                    .text_sm()
                    .whitespace_normal()
                    .map(|this| match &preview {
                        Ok(statements) => this.child(statements.join(";\n")),
                        Err(problem) => this.text_color(quiet).child(problem.clone()),
                    }),
            ),
        );
        body = body.child(
            div()
                .id("manage-status")
                .min_h_5()
                .text_sm()
                .text_color(theme.danger)
                .when_some(self.problem.clone(), |this, problem| this.child(problem)),
        );

        let action = match &self.task {
            Task::DropDatabase(_)
            | Task::DropSchema(_)
            | Task::DropTable(_)
            | Task::DropColumn { .. } => {
                if kind == DbKind::Redis {
                    "Delete"
                } else {
                    "Drop"
                }
            }
            Task::FlushDatabase(_) => "Delete every key",
            Task::EmptyTable(_) => "Empty",
            Task::RenameTable(_) | Task::RenameColumn { .. } => "Rename",
            Task::AddColumn(_) => "Add",
            _ => "Create",
        };
        let run = Button::new("manage-run")
            .h_9()
            .label(action)
            .loading(self.running)
            .disabled(self.running || preview.is_err())
            .on_click(cx.listener(|this, _, window, cx| this.run(window, cx)));
        let run = if self.task.destructive() {
            run.danger()
        } else {
            run.primary()
        };
        body.child(
            h_flex()
                .gap_2()
                .child(div().flex_1())
                .child(
                    Button::new("manage-cancel")
                        .ghost()
                        .h_9()
                        .label("Cancel")
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                )
                .child(run),
        )
    }
}

/// Open the dialog for `task` on a connection's session.
pub(super) fn open(
    kind: DbKind,
    task: Task,
    session: Arc<dyn Session>,
    on_done: Rc<dyn Fn(&mut App)>,
    window: &mut Window,
    cx: &mut App,
) {
    let title: SharedString = task.title(kind).trim_end_matches('…').to_owned().into();
    let wants_name = !matches!(
        task,
        Task::DropDatabase(_)
            | Task::DropSchema(_)
            | Task::EmptyTable(_)
            | Task::DropTable(_)
            | Task::DropColumn { .. }
            | Task::FlushDatabase(_)
    ) && !(matches!(task, Task::AddColumn(_)) && kind != DbKind::Mongodb);
    let view = cx.new(|cx| ManageDialog::new(kind, task, session, on_done, window, cx));
    let name = view.read(cx).name.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title(title.clone())
            .w(px(600.))
            .p_6()
            .child(view.clone())
    });
    if wants_name {
        name.update(cx, |name, cx| name.focus(window, cx));
    }
}
