//! A table tab: one table's rows through a paged `SELECT`, sorted by the
//! server and narrowed by a `WHERE` fragment, with edits staged until they
//! are previewed and applied in one transaction.

mod changes;
pub mod columns;
pub mod form;
pub mod rules;
pub mod summary;

use std::{sync::Arc, time::Instant};

use futures::{
    StreamExt as _,
    channel::{mpsc, oneshot},
};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Selectable as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Editor, EditorState, Input, InputEvent, InputState},
    table::{Column, ColumnSort, DataTable, TableDelegate, TableEvent, TableState},
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use serde::{Deserialize, Serialize};
use sonde_core::{
    ConnectionConfig, DbKind, Error, QuerySummary, ResultPage, Session, SqliteConfig, Value,
    config::SecretStore,
    edit::EditStatement,
    filter::{self, Filter, Op, Rule},
    schema::{ForeignKey, RelationKind, TableDetail, TableRef},
};

use self::{
    changes::{Changes, RowKey, row_key},
    columns::{ColumnLayout, ColumnsDialog},
    rules::{BuilderEvent, FilterBuilder},
    summary::{SummaryEvent, SummaryPanel},
};
use crate::{
    design::{GRID_CELL_INSET, GRID_HEADER_HEIGHT, ROW_HEIGHT},
    query::{QueryTarget, cell},
};

actions!(
    table_view,
    [
        /// Edit the selected cell.
        EditCell,
        /// Stage the selected row for deletion, or take that back.
        ToggleDelete
    ]
);

/// Pages pulled before the grid waits for the user to scroll,
/// from `settings.toml`.
fn eager_pages(cx: &App) -> usize {
    crate::settings::Settings::get(cx).eager_pages()
}
/// Key context of the grid, for its editing bindings.
const GRID_CONTEXT: &str = "TableGrid";

/// Register the table tab's key bindings. Call once per application.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("enter", EditCell, Some(GRID_CONTEXT)),
        KeyBinding::new("delete", ToggleDelete, Some(GRID_CONTEXT)),
    ]);
}

enum Link {
    Connecting,
    Ready(Arc<dyn Session>),
    Failed(SharedString),
}

/// Where the rows of the current `SELECT` stand.
enum Load {
    /// The table is being described; there is no `SELECT` yet.
    Describing,
    Streaming,
    /// The stream is parked with rows still to come.
    Paused,
    Finished,
    /// Stopped by the user, or given up to free the session for an apply.
    Stopped,
    Failed,
}

struct Stream {
    pages: mpsc::Receiver<sonde_core::Result<ResultPage>>,
    summary: oneshot::Receiver<QuerySummary>,
}

/// A grid column, from the table's description.
struct GridColumn {
    name: String,
    /// The type as the database writes it, used to parse edited text.
    type_name: String,
}

/// The cell an edit is for. A loaded row is held by its position, which is
/// stable until the next reload closes the editor; a new row by its index
/// among the new rows, because loading more rows moves it down the grid.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    Loaded(usize),
    New(usize),
}

struct Editing {
    place: Place,
    column: usize,
    input: Entity<InputState>,
    _events: Subscription,
}

/// What a table tab asks of the workspace.
pub enum TableViewEvent {
    /// Open `table` on the same connection, narrowed by `filter`.
    OpenTable { table: TableRef, filter: Filter },
    /// Open a query tab holding `sql`.
    OpenQuery(String),
    /// Save the tab's filter, sort and columns as a view.
    SaveView(ViewState),
}

/// A tab's filter, sort, columns and mode: what a saved view keeps.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewState {
    /// Rules from the builder.
    #[serde(default)]
    pub filter: Filter,
    /// A typed filter, used instead of the rules when `typed` is set.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default)]
    pub typed: bool,
    /// The column sorted on, ascending when the flag is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<(String, bool)>,
    #[serde(default)]
    pub columns: ColumnLayout,
    /// Open in Summary mode.
    #[serde(default)]
    pub summary: bool,
}

/// Rows or the Summary of them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Rows,
    Summary,
}

impl EventEmitter<TableViewEvent> for TableView {}

/// A foreign key on another table that points at this one.
#[derive(Clone)]
struct Referencing {
    table: TableRef,
    key: ForeignKey,
}

/// Why the last action did not work.
struct Problem {
    headline: SharedString,
    notes: Vec<SharedString>,
}

impl Problem {
    fn new(headline: impl Into<SharedString>) -> Self {
        Self {
            headline: headline.into(),
            notes: Vec::new(),
        }
    }

    fn from_error(prefix: &str, error: &Error) -> Self {
        match error {
            Error::Query {
                message,
                detail,
                hint,
                ..
            } => Self {
                headline: format!("{prefix}{message}").into(),
                notes: detail
                    .iter()
                    .chain(hint)
                    .map(|line| SharedString::from(line.clone()))
                    .collect(),
            },
            other => Self::new(format!("{prefix}{other}")),
        }
    }
}

pub struct TableView {
    weak: WeakEntity<Self>,
    target: QueryTarget,
    table_ref: TableRef,
    link: Link,
    columns: Vec<GridColumn>,
    /// The table's description, once read.
    detail: Option<TableDetail>,
    /// The rule builder, once the columns are known. `None` for Redis,
    /// whose keys are filtered by pattern.
    builder: Option<Entity<FilterBuilder>>,
    /// Whether the rule list is open.
    builder_open: bool,
    /// Filter with typed text instead of rules.
    typed: bool,
    layout: ColumnLayout,
    /// Keys on other tables that point here, found after the table loads.
    referencing: Vec<Referencing>,
    mode: Mode,
    summary: Option<Entity<SummaryPanel>>,
    /// A view to apply once the table is described.
    pending: Option<ViewState>,
    /// Seconds a Redis key has left; `-1` for none.
    ttl: Option<i64>,
    /// The row form, while one is open.
    form: Option<Entity<form::RowForm>>,
    /// The columns a row is keyed on: the primary key, or SQLite's `rowid`.
    /// Empty when the table is read-only.
    key_columns: Vec<String>,
    /// Why rows cannot be edited, when they cannot.
    read_only: Option<SharedString>,
    filter: Entity<InputState>,
    /// The filter the current rows were loaded with.
    filtered: String,
    sort: Option<(usize, ColumnSort)>,
    table: Entity<TableState<Grid>>,
    load: Load,
    /// The parked stream. `None` while a pull owns it and once it has ended.
    stream: Option<Stream>,
    /// The running `SELECT` or pull. Dropping it ends the statement.
    pull: Option<Task<()>>,
    editing: Option<Editing>,
    applying: Option<Task<()>>,
    /// What the last apply wrote, until the next action.
    applied: Option<String>,
    problem: Option<Problem>,
    _subscriptions: Vec<Subscription>,
}

impl TableView {
    pub fn new(
        target: QueryTarget,
        table_ref: TableRef,
        secrets: Option<Arc<dyn SecretStore>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let kind = target.config.kind();
        let dialect = sonde_core::dialect(kind);
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder(dialect.filter_hint()));
        let grid = Grid {
            view: cx.weak_entity(),
            columns: Vec::new(),
            order: Vec::new(),
            pinned: 0,
            sort: None,
            links: Vec::new(),
            numeric: Vec::new(),
            rows: Vec::new(),
            keys: Vec::new(),
            key_positions: Vec::new(),
            changes: Changes::default(),
            has_more: false,
            loading: true,
            filtered: false,
            editable: false,
        };
        let table = cx.new(|cx| {
            TableState::new(grid, window, cx)
                .cell_selectable(true)
                .row_header(false)
                .col_movable(true)
                .sortable(dialect.sorts())
        });
        let _subscriptions = vec![
            cx.subscribe_in(&table, window, Self::on_table_event),
            cx.subscribe_in(&filter, window, |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.reload(cx);
                }
            }),
        ];
        let mut this = Self {
            weak: cx.weak_entity(),
            target,
            table_ref,
            link: Link::Connecting,
            columns: Vec::new(),
            detail: None,
            builder: None,
            builder_open: false,
            typed: !filter::supported(kind),
            layout: ColumnLayout::default(),
            referencing: Vec::new(),
            mode: Mode::Rows,
            summary: None,
            pending: None,
            ttl: None,
            form: None,
            key_columns: Vec::new(),
            read_only: None,
            filter,
            filtered: String::new(),
            sort: None,
            table,
            load: Load::Describing,
            stream: None,
            pull: None,
            editing: None,
            applying: None,
            applied: None,
            problem: None,
            _subscriptions,
        };
        this.connect(secrets, window, cx);
        this
    }

    /// Open this tab's own session, like a query tab, then describe the
    /// table and load its rows.
    fn connect(
        &mut self,
        secrets: Option<Arc<dyn SecretStore>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let config = self.target.config.clone();
        let typed = self.target.password.clone();
        let id = crate::connections::keyring_id(&self.target.id).to_owned();
        let wants_password = config.kind().is_server();
        let table = self.table_ref.clone();

        cx.spawn_in(window, async move |this, cx| {
            let secret = match (typed, secrets) {
                (Some(typed), _) => Ok(Some(typed)),
                (None, Some(secrets)) if wants_password => {
                    cx.background_spawn(async move { secrets.get(&id) }).await
                }
                _ => Ok(None),
            };
            let session = match secret {
                Ok(secret) => sonde_core::connect(&config, secret).await,
                Err(error) => Err(error),
            };
            let session: Arc<dyn Session> = match session {
                Ok(session) => Arc::from(session),
                Err(error) => {
                    let _ = this.update(cx, |this, cx| {
                        this.link = Link::Failed(error.to_string().into());
                        cx.notify();
                    });
                    return;
                }
            };
            let detail = session.table_detail(&table).await;
            // The tab may have been closed while this ran.
            let _ = this.update_in(cx, |this, window, cx| {
                this.link = Link::Ready(session);
                match detail {
                    Ok(detail) => {
                        this.describe(&detail, cx);
                        this.prepare(window, cx);
                        this.reload(cx);
                        this.find_referencing(cx);
                        this.read_ttl(cx);
                    }
                    Err(error) => {
                        this.load = Load::Failed;
                        this.problem =
                            Some(Problem::from_error("Could not read the table: ", &error));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Set up the columns and decide whether rows can be edited.
    fn describe(&mut self, detail: &TableDetail, cx: &mut Context<Self>) {
        let read_only_database = matches!(
            self.target.config,
            ConnectionConfig::Sqlite(SqliteConfig {
                read_only: true,
                ..
            })
        );
        let mut columns: Vec<GridColumn> = detail
            .columns
            .iter()
            .map(|column| GridColumn {
                name: column.name.clone(),
                type_name: column.type_name.clone(),
            })
            .collect();
        self.key_columns = if !detail.primary_key.is_empty() {
            detail.primary_key.clone()
        } else if detail.has_rowid {
            // The rowid addresses the row but is not one of its columns, so
            // it is read alongside them, first.
            columns.insert(
                0,
                GridColumn {
                    name: "rowid".into(),
                    type_name: "integer".into(),
                },
            );
            vec!["rowid".into()]
        } else {
            Vec::new()
        };
        let kind = self.target.config.kind();
        let reason = sonde_core::dialect(kind).read_only_reason();
        self.read_only = if let Some(reason) = reason {
            Some(reason.into())
        } else if read_only_database {
            Some("This database is open read-only, so its rows cannot be changed here.".into())
        } else if self.key_columns.is_empty() && kind == DbKind::Redis {
            Some("Streams and JSON keys are changed with commands in a query tab, such as XADD or JSON.SET.".into())
        } else if self.key_columns.is_empty() {
            Some(
                format!(
                    "{} has no primary key, so Sonde cannot tell its rows apart \
                     safely. It is shown read-only.",
                    self.table_ref.name
                )
                .into(),
            )
        } else {
            None
        };
        if self.read_only.is_some() {
            self.key_columns.clear();
        }

        let key_positions: Vec<usize> = self
            .key_columns
            .iter()
            .filter_map(|key| columns.iter().position(|column| &column.name == key))
            .collect();
        let numeric: Vec<bool> = columns
            .iter()
            .map(|column| cell::is_numeric(&column.type_name))
            .collect();
        let sorts = sonde_core::dialect(self.target.config.kind()).sorts();
        let grid_columns: Vec<Column> = columns
            .iter()
            .zip(&numeric)
            .enumerate()
            .map(|(ix, (column, &numeric))| {
                // Cells draw their own padding so a staged edit can fill the
                // whole cell.
                let grid_column = Column::new(format!("c{ix}"), column.name.clone())
                    .width(px(cell::column_width(&column.name, &column.type_name)))
                    .p_0();
                let grid_column = if sorts {
                    grid_column.sortable()
                } else {
                    grid_column
                };
                if numeric {
                    grid_column.text_right()
                } else {
                    grid_column
                }
            })
            .collect();
        // Columns a single-column foreign key leaves from link to its row.
        let links: Vec<bool> = columns
            .iter()
            .map(|column| {
                detail
                    .foreign_keys
                    .iter()
                    .any(|fk| fk.columns.len() == 1 && fk.columns[0] == column.name)
            })
            .collect();
        self.columns = columns;
        self.detail = Some(detail.clone());
        let editable = self.read_only.is_none();
        self.table.update(cx, |table, cx| {
            let grid = table.delegate_mut();
            grid.columns = grid_columns;
            grid.numeric = numeric;
            grid.key_positions = key_positions;
            grid.editable = editable;
            grid.links = links;
            grid.order = (0..grid.columns.len()).collect();
            table.refresh(cx);
        });
    }

    /// Set up what needs the columns and a window: the rule builder, the
    /// remembered column layout, and a view waiting to be applied.
    fn prepare(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let kind = self.target.config.kind();
        if filter::supported(kind) {
            let columns: Vec<(String, String)> = self
                .columns
                .iter()
                .filter(|c| !(c.name == "rowid" && self.key_columns == ["rowid"]))
                .map(|c| (c.name.clone(), c.type_name.clone()))
                .collect();
            let builder = cx.new(|_| FilterBuilder::new(kind, columns));
            self._subscriptions.push(cx.subscribe(
                &builder,
                |this, _, event: &BuilderEvent, cx| match event {
                    BuilderEvent::Apply => this.reload(cx),
                },
            ));
            self._subscriptions
                .push(cx.observe(&builder, |_, _, cx| cx.notify()));
            self.builder = Some(builder);
        }
        let layout = columns::remembered(&self.prefs_key(), cx);
        self.set_layout(layout, cx);
        if let Some(state) = self.pending.take() {
            self.apply_state(state, window, cx);
        }
    }

    /// The table's key in `tables.toml`.
    fn prefs_key(&self) -> String {
        let connection = crate::connections::keyring_id(&self.target.id);
        match &self.table_ref.schema {
            Some(schema) => format!("{connection}/{schema}/{}", self.table_ref.name),
            None => format!("{connection}//{}", self.table_ref.name),
        }
    }

    fn column_names(&self) -> Vec<String> {
        self.columns.iter().map(|c| c.name.clone()).collect()
    }

    /// Show the columns as `layout` arranges them, and remember it.
    pub fn set_layout(&mut self, layout: ColumnLayout, cx: &mut Context<Self>) {
        let names = self.column_names();
        if names.is_empty() {
            return;
        }
        let (order, pinned) = layout.arrange(&names);
        if layout != self.layout {
            columns::remember(&self.prefs_key(), &layout, cx);
        }
        self.layout = layout;
        let sort = self.sort;
        self.table.update(cx, |table, cx| {
            let grid = table.delegate_mut();
            grid.order = order;
            grid.pinned = pinned;
            grid.sort = sort;
            table.refresh(cx);
        });
        cx.notify();
    }

    /// The columns as shown: data indices in display order.
    pub fn shown_columns(&self, cx: &App) -> Vec<usize> {
        self.table.read(cx).delegate().order.clone()
    }

    /// A header drag moved a shown column.
    fn moved_column(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        let names = self.column_names();
        let shown = self.shown_columns(cx);
        let mut layout = self.layout.clone();
        layout.moved(&names, &shown, from, to);
        self.set_layout(layout, cx);
    }

    /// Open the Columns dialog.
    pub fn open_columns(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.columns.is_empty() {
            return;
        }
        let view = cx.weak_entity();
        let dialog = cx.new(|_| ColumnsDialog { view });
        window.open_dialog(cx, move |d, _, _| {
            d.title("Columns").w(px(520.)).p_6().child(dialog.clone())
        });
    }

    /// The filter, sort, columns and mode, for a saved view.
    pub fn state(&self, cx: &App) -> ViewState {
        ViewState {
            filter: self
                .builder
                .as_ref()
                .map(|b| b.read(cx).filter(cx))
                .unwrap_or_default(),
            text: self.filter.read(cx).value().to_string(),
            typed: self.typed,
            sort: self.sort.and_then(|(column, sort)| {
                let ascending = match sort {
                    ColumnSort::Ascending => true,
                    ColumnSort::Descending => false,
                    ColumnSort::Default => return None,
                };
                Some((self.columns.get(column)?.name.clone(), ascending))
            }),
            columns: self.layout.clone(),
            summary: self.mode == Mode::Summary,
        }
    }

    /// Apply a saved view or a followed key's filter, now or once the table
    /// is described.
    pub fn apply_view(&mut self, state: ViewState, window: &mut Window, cx: &mut Context<Self>) {
        if self.columns.is_empty() {
            self.pending = Some(state);
            return;
        }
        self.apply_state(state, window, cx);
        self.reload(cx);
    }

    fn apply_state(&mut self, mut state: ViewState, window: &mut Window, cx: &mut Context<Self>) {
        // A rule made elsewhere may not know its column's type.
        for rule in &mut state.filter.rules {
            if rule.type_name.is_empty()
                && let Some(column) = self.columns.iter().find(|c| c.name == rule.column)
            {
                rule.type_name = column.type_name.clone();
            }
        }
        if let Some(builder) = &self.builder {
            builder.update(cx, |builder, cx| {
                builder.set_filter(&state.filter, window, cx)
            });
            self.builder_open = !state.filter.is_empty();
        }
        self.typed = state.typed || self.builder.is_none();
        let text = state.text.clone();
        self.filter
            .update(cx, |input, cx| input.set_value(text, window, cx));
        self.sort = state.sort.as_ref().and_then(|(name, ascending)| {
            let column = self.columns.iter().position(|c| &c.name == name)?;
            Some((
                column,
                if *ascending {
                    ColumnSort::Ascending
                } else {
                    ColumnSort::Descending
                },
            ))
        });
        if !state.columns.is_default() || !self.layout.is_default() {
            self.set_layout(state.columns.clone(), cx);
        } else {
            self.set_layout(self.layout.clone(), cx);
        }
        if state.summary {
            self.mode = Mode::Summary;
        }
    }

    /// The filter to load rows with: the rules as the database's condition,
    /// or the typed text.
    fn current_filter(&self, cx: &App) -> Result<String, String> {
        match (&self.builder, self.typed) {
            (Some(builder), false) => {
                filter::condition(self.target.config.kind(), &builder.read(cx).filter(cx))
            }
            _ => Ok(self.filter.read(cx).value().to_string()),
        }
    }

    /// The rule builder, once the table is described.
    pub fn builder(&self) -> Option<&Entity<FilterBuilder>> {
        self.builder.as_ref()
    }

    /// Filter with typed text, or go back to rules.
    pub fn set_typed(&mut self, typed: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.builder.is_none() {
            return;
        }
        if typed && !self.typed {
            // The rules carry over as text, to edit further.
            if let Ok(text) = self.current_filter(cx) {
                self.filter
                    .update(cx, |input, cx| input.set_value(text, window, cx));
            }
        }
        self.typed = typed;
        if typed {
            self.focus_filter(window, cx);
        }
        cx.notify();
    }

    /// Add a rule and open the rule list.
    pub fn add_rule(&mut self, column: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(builder) = &self.builder {
            self.typed = false;
            self.builder_open = true;
            builder.update(cx, |builder, cx| builder.add_rule(column, window, cx));
            cx.notify();
        }
    }

    /// Drop every rule and the typed filter, and load every row.
    pub fn clear_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(builder) = &self.builder {
            builder.update(cx, |builder, cx| builder.clear(cx));
        }
        self.filter
            .update(cx, |input, cx| input.set_value(String::new(), window, cx));
        self.builder_open = false;
        self.reload(cx);
    }

    /// Show rows or their Summary.
    pub fn set_mode(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        if mode == self.mode && (mode == Mode::Rows || self.summary.is_some()) {
            return;
        }
        self.mode = mode;
        match mode {
            Mode::Rows => self.reload(cx),
            Mode::Summary => self.show_summary(window, cx),
        }
        cx.notify();
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn summary_panel(&self) -> Option<&Entity<SummaryPanel>> {
        self.summary.as_ref()
    }

    fn show_summary(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session().cloned() else {
            return;
        };
        if !filter::summarizes(self.target.config.kind()) || self.columns.is_empty() {
            return;
        }
        // The summary runs on this tab's session, which a parked row stream
        // would hold.
        self.stop(cx);
        let condition = match self.current_filter(cx) {
            Ok(condition) => condition,
            Err(message) => {
                self.problem = Some(Problem::new(message));
                String::new()
            }
        };
        let panel = match &self.summary {
            Some(panel) => panel.clone(),
            None => {
                let columns = self
                    .columns
                    .iter()
                    .filter(|c| !(c.name == "rowid" && self.key_columns == ["rowid"]))
                    .map(|c| (c.name.clone(), c.type_name.clone()))
                    .collect();
                let (kind, table) = (self.target.config.kind(), self.table_ref.clone());
                let panel = cx.new(|cx| SummaryPanel::new(kind, table, columns, window, cx));
                self._subscriptions
                    .push(
                        cx.subscribe(&panel, |_, _, event: &SummaryEvent, cx| match event {
                            SummaryEvent::OpenQuery(sql) => {
                                cx.emit(TableViewEvent::OpenQuery(sql.clone()))
                            }
                        }),
                    );
                self._subscriptions
                    .push(cx.observe(&panel, |_, _, cx| cx.notify()));
                self.summary = Some(panel.clone());
                panel
            }
        };
        panel.update(cx, |panel, cx| panel.show(session, condition, cx));
    }

    /// Find the foreign keys on other tables that point at this one, for
    /// "Referenced by" in a row's menu. Only the same schema is searched.
    fn find_referencing(&mut self, cx: &mut Context<Self>) {
        let kind = self.target.config.kind();
        if !matches!(
            kind,
            DbKind::Postgres
                | DbKind::Mysql
                | DbKind::Mariadb
                | DbKind::SqlServer
                | DbKind::Sqlite
                | DbKind::Snowflake
        ) {
            return;
        }
        let Some(session) = self.session().cloned() else {
            return;
        };
        let me = self.table_ref.clone();
        cx.spawn(async move |this, cx| {
            let Ok(catalog) = session.catalog().await else {
                return;
            };
            let mut found = Vec::new();
            for schema in &catalog.schemas {
                if me.schema.as_ref().is_some_and(|s| *s != schema.name) {
                    continue;
                }
                // A big schema is not worth describing table by table.
                for relation in schema
                    .relations
                    .iter()
                    .filter(|r| r.kind == RelationKind::Table)
                    .take(300)
                {
                    let table = TableRef::new(schema.name.clone(), relation.name.clone());
                    let Ok(detail) = session.table_detail(&table).await else {
                        continue;
                    };
                    for key in detail.foreign_keys {
                        let same_schema = key.references.schema.is_none()
                            || key.references.schema == me.schema
                            || me.schema.is_none();
                        if key.references.name == me.name && same_schema {
                            found.push(Referencing {
                                table: table.clone(),
                                key,
                            });
                        }
                    }
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.referencing = found;
                cx.notify();
            });
        })
        .detach();
    }

    /// Tables whose foreign keys point here, once found.
    pub fn referenced_by(&self) -> Vec<String> {
        self.referencing
            .iter()
            .map(|r| r.table.name.clone())
            .collect()
    }

    /// Read how long a Redis key has left.
    fn read_ttl(&mut self, cx: &mut Context<Self>) {
        if self.target.config.kind() != DbKind::Redis {
            return;
        }
        let Some(session) = self.session().cloned() else {
            return;
        };
        let command = format!(
            "TTL {}",
            sonde_core::dialect(DbKind::Redis).quote_table(&self.table_ref)
        );
        cx.spawn(async move |this, cx| {
            let ttl = async {
                let mut handle = session.execute(&command, &[]).await.ok()?;
                let page = handle.pages.next().await?.ok()?;
                match page.rows.first()?.first()? {
                    Value::Int(n) => Some(*n),
                    _ => None,
                }
            }
            .await;
            let _ = this.update(cx, |this, cx| {
                this.ttl = ttl;
                cx.notify();
            });
        })
        .detach();
    }

    /// Give a Redis key an expiry in seconds, or none.
    pub fn set_expiry(&mut self, seconds: Option<u64>, cx: &mut Context<Self>) {
        let Some(session) = self.session().cloned() else {
            return;
        };
        let key = sonde_core::dialect(DbKind::Redis).quote_table(&self.table_ref);
        let command = match seconds {
            Some(seconds) => format!("EXPIRE {key} {seconds}"),
            None => format!("PERSIST {key}"),
        };
        cx.spawn(async move |this, cx| {
            let result = async {
                let mut handle = session.execute(&command, &[]).await?;
                while let Some(page) = handle.pages.next().await {
                    page?;
                }
                Ok::<_, Error>(())
            }
            .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.problem = Some(Problem::from_error("Could not set the expiry: ", &error));
                }
                this.read_ttl(cx);
            });
        })
        .detach();
    }

    /// Seconds the key has left, `-1` for no expiry. Redis only.
    pub fn ttl(&self) -> Option<i64> {
        self.ttl
    }

    fn open_expiry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.weak_entity();
        let seconds = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("seconds, such as 3600")
                .default_value(
                    self.ttl
                        .filter(|t| *t > 0)
                        .map(|t| t.to_string())
                        .unwrap_or_default(),
                )
        });
        window.open_dialog(cx, move |d, _, cx| {
            let (v1, v2, input) = (view.clone(), view.clone(), seconds.clone());
            d.title("Key expiry")
                .w(px(420.))
                .p_6()
                .child(
                    v_flex()
                        .gap_3()
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child("Redis deletes the key once this many seconds have passed."),
                        )
                        .child(Input::new(&seconds).aria_label("Expiry in seconds")),
                )
                .footer(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("expiry-remove")
                                .ghost()
                                .label("Never expire")
                                .on_click(move |_, window, cx| {
                                    let _ = v1.update(cx, |view, cx| view.set_expiry(None, cx));
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(div().flex_1())
                        .child(
                            Button::new("expiry-set")
                                .primary()
                                .label("Set expiry")
                                .on_click(move |_, window, cx| {
                                    let text = input.read(cx).value().trim().to_owned();
                                    if let Ok(seconds) = text.parse::<u64>() {
                                        let _ = v2.update(cx, |view, cx| {
                                            view.set_expiry(Some(seconds), cx)
                                        });
                                        window.close_dialog(cx);
                                    }
                                }),
                        ),
                )
        });
    }

    /// Open a row, or a new one, as a form.
    /// Open a loaded row as a form.
    pub fn open_row_form(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.open_form(Place::Loaded(row), window, cx);
    }

    fn open_form(&mut self, place: Place, window: &mut Window, cx: &mut Context<Self>) {
        if self.columns.is_empty() {
            return;
        }
        self.editing = None;
        let form = cx.new(|cx| form::RowForm::new(self, place, window, cx));
        let title = match place {
            Place::New(_) => format!("New row in {}", self.table_ref.name),
            Place::Loaded(_) if self.is_editable() => {
                format!("Edit row in {}", self.table_ref.name)
            }
            Place::Loaded(_) => format!("Row in {}", self.table_ref.name),
        };
        let shown = form.clone();
        window.open_dialog(cx, move |d, _, _| {
            d.title(title.clone())
                .w(px(640.))
                .p_6()
                .child(shown.clone())
        });
        self.form = Some(form);
    }

    /// Open the selected row as a form, or a new row when none is selected.
    pub fn open_selected_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let place = match self.selected_cell(cx) {
            Some((place, _)) => place,
            None if self.is_editable() => {
                let ix = self.table.update(cx, |table, cx| {
                    let ix = table.delegate_mut().changes.add_insert();
                    table.refresh(cx);
                    ix
                });
                Place::New(ix)
            }
            None => return,
        };
        self.open_form(place, window, cx);
    }

    /// The form last opened, while it is open. For tests.
    pub fn form(&self) -> Option<&Entity<form::RowForm>> {
        self.form.as_ref()
    }

    /// Stage several cells of one row at once, as the form does.
    fn stage_cells(
        &mut self,
        place: Place,
        cells: Vec<(usize, Option<Value>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.form = None;
        for (column, value) in cells {
            self.stage(place, column, value, window, cx);
        }
        cx.notify();
    }

    /// A loaded row's value in each of `columns`, or `None` if any is NULL.
    fn row_values(&self, place: Place, columns: &[String], cx: &App) -> Option<Vec<Value>> {
        let grid = self.table.read(cx).delegate();
        columns
            .iter()
            .map(|name| {
                let ix = self.column(name)?;
                grid.current(place, ix).filter(|v| !v.is_null()).cloned()
            })
            .collect()
    }

    /// Rules that match `values` on `columns`.
    fn rules_for(columns: &[String], values: &[Value]) -> Filter {
        Filter {
            rules: columns
                .iter()
                .zip(values)
                .map(|(column, value)| Rule::new(column, "", Op::Is, &value.to_string()))
                .collect(),
            any: false,
        }
    }

    /// Open the row a foreign key on `column` points at.
    fn follow_key(&mut self, place: Place, column: usize, cx: &mut Context<Self>) {
        let Some(name) = self.columns.get(column).map(|c| c.name.clone()) else {
            return;
        };
        let Some(key) = self.detail.as_ref().and_then(|d| {
            d.foreign_keys
                .iter()
                .find(|fk| fk.columns.contains(&name))
                .cloned()
        }) else {
            return;
        };
        self.follow(place, &key, cx);
    }

    fn follow(&mut self, place: Place, key: &ForeignKey, cx: &mut Context<Self>) {
        let Some(values) = self.row_values(place, &key.columns, cx) else {
            return;
        };
        let mut table = key.references.clone();
        if table.schema.is_none() {
            table.schema = self.table_ref.schema.clone();
        }
        cx.emit(TableViewEvent::OpenTable {
            table,
            filter: Self::rules_for(&key.referenced_columns, &values),
        });
    }

    fn show_referencing(
        &mut self,
        place: Place,
        referencing: &Referencing,
        cx: &mut Context<Self>,
    ) {
        let Some(values) = self.row_values(place, &referencing.key.referenced_columns, cx) else {
            return;
        };
        cx.emit(TableViewEvent::OpenTable {
            table: referencing.table.clone(),
            filter: Self::rules_for(&referencing.key.columns, &values),
        });
    }

    /// Follow the foreign key on a loaded row's column. For tests and the
    /// grid's links.
    pub fn follow_cell(&mut self, row: usize, column: usize, cx: &mut Context<Self>) {
        self.follow_key(Place::Loaded(row), column, cx);
    }

    /// Show the rows of `table` that point at a loaded row. For tests.
    pub fn show_referencing_rows(&mut self, row: usize, table: &str, cx: &mut Context<Self>) {
        if let Some(referencing) = self
            .referencing
            .iter()
            .find(|r| r.table.name == table)
            .cloned()
        {
            self.show_referencing(Place::Loaded(row), &referencing, cx);
        }
    }

    /// Narrow the rows to those where `column` holds this row's value.
    fn filter_by_value(
        &mut self,
        place: Place,
        column: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(builder) = self.builder.clone() else {
            return;
        };
        let Some(info) = self.columns.get(column) else {
            return;
        };
        let value = self
            .table
            .read(cx)
            .delegate()
            .current(place, column)
            .cloned();
        let mut filter = builder.read(cx).filter(cx);
        filter.rules.push(match value {
            Some(Value::Null) | None => Rule::new(&info.name, &info.type_name, Op::IsEmpty, ""),
            Some(value) => Rule::new(&info.name, &info.type_name, Op::Is, &value.to_string()),
        });
        builder.update(cx, |builder, cx| builder.set_filter(&filter, window, cx));
        self.typed = false;
        self.builder_open = true;
        self.reload(cx);
    }

    /// Ask the workspace to save this tab as a view.
    pub fn save_view(&mut self, cx: &mut Context<Self>) {
        let state = self.state(cx);
        cx.emit(TableViewEvent::SaveView(state));
    }

    /// The `SELECT` for the current filter and sort. It has no `LIMIT`: the
    /// driver pages the result and the bounded channel holds the rest back.
    fn select_sql(&self, session: &dyn Session) -> String {
        let names: Vec<String> = self.columns.iter().map(|c| c.name.clone()).collect();
        let sort = self.sort.and_then(|(column, sort)| {
            let ascending = match sort {
                ColumnSort::Ascending => true,
                ColumnSort::Descending => false,
                ColumnSort::Default => return None,
            };
            Some((names[column].as_str(), ascending))
        });
        session
            .dialect()
            .browse(&self.table_ref, &names, &self.filtered, sort)
    }

    /// Run the filter box's text and load the rows from the top. Staged
    /// changes are kept: they are keyed on the rows, not on positions.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session().cloned() else {
            return;
        };
        if self.columns.is_empty() || self.applying.is_some() {
            return;
        }
        if self.mode == Mode::Summary {
            if let (Some(panel), Ok(condition)) = (self.summary.clone(), self.current_filter(cx)) {
                self.filtered = condition.clone();
                panel.update(cx, |panel, cx| panel.show(session, condition, cx));
            }
            return;
        }
        self.filtered = match self.current_filter(cx) {
            Ok(filter) => filter,
            Err(message) => {
                self.problem = Some(Problem::new(message));
                cx.notify();
                return;
            }
        };
        let sql = self.select_sql(session.as_ref());
        let filtered = !self.filtered.trim().is_empty();

        // Dropping the old stream ends its statement.
        self.stream = None;
        self.editing = None;
        self.problem = None;
        self.applied = None;
        self.load = Load::Streaming;
        self.table.update(cx, |table, cx| {
            let grid = table.delegate_mut();
            grid.rows.clear();
            grid.keys.clear();
            grid.has_more = false;
            grid.loading = true;
            grid.filtered = filtered;
            cx.notify();
        });
        cx.notify();

        self.pull = Some(cx.spawn(async move |this, cx| {
            let handle = session.execute(&sql, &[]).await;
            let _ = this.update(cx, |this, cx| match handle {
                Ok(handle) => {
                    this.stream = Some(Stream {
                        pages: handle.pages,
                        summary: handle.summary,
                    });
                    this.pull = None;
                    this.pull(eager_pages(cx), cx);
                }
                Err(error) => {
                    this.pull = None;
                    this.fail(&error, cx);
                }
            });
        }));
    }

    fn fail(&mut self, error: &Error, cx: &mut Context<Self>) {
        self.load = Load::Failed;
        let prefix = if self.filtered.trim().is_empty() {
            "Could not load the rows: "
        } else {
            "Could not load the rows with this filter: "
        };
        self.problem = Some(Problem::from_error(prefix, error));
        self.table.update(cx, |table, cx| {
            table.delegate_mut().loading = false;
            cx.notify();
        });
        cx.notify();
    }

    /// Pull up to `pages` more pages, then park the stream again.
    fn pull(&mut self, pages: usize, cx: &mut Context<Self>) {
        let Some(mut stream) = self.stream.take() else {
            return;
        };
        self.load = Load::Streaming;
        self.set_has_more(false, cx);
        cx.notify();

        self.pull = Some(cx.spawn(async move |this, cx| {
            let mut pulled = 0;
            let end = loop {
                if pulled == pages {
                    break None;
                }
                let page = match stream.pages.next().await {
                    Some(Ok(page)) => page,
                    Some(Err(error)) => break Some(Err(error)),
                    None => {
                        let _ = (&mut stream.summary).await;
                        break Some(Ok(()));
                    }
                };
                pulled += 1;
                let appended = this.update(cx, |this, cx| {
                    this.table.update(cx, |table, cx| {
                        table.delegate_mut().append(page.rows);
                        cx.notify();
                    });
                    cx.notify();
                });
                if appended.is_err() {
                    // The tab was closed; dropping the stream ends the
                    // statement.
                    return;
                }
            };

            let _ = this.update(cx, |this, cx| {
                this.pull = None;
                this.table
                    .update(cx, |table, _| table.delegate_mut().loading = false);
                match end {
                    None => {
                        this.load = Load::Paused;
                        this.stream = Some(stream);
                        this.set_has_more(true, cx);
                    }
                    Some(Ok(())) => this.load = Load::Finished,
                    Some(Err(Error::Cancelled)) => this.load = Load::Stopped,
                    Some(Err(error)) => this.fail(&error, cx),
                }
                cx.notify();
            });
        }));
    }

    /// Keep loading until every row is in.
    pub fn load_all(&mut self, cx: &mut Context<Self>) {
        self.pull(usize::MAX, cx);
    }

    /// Stop loading, keeping the rows that arrived.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if self.stream.is_some() || self.pull.is_some() {
            self.pull = None;
            self.stream = None;
            self.load = Load::Stopped;
            self.set_has_more(false, cx);
            self.table
                .update(cx, |table, _| table.delegate_mut().loading = false);
            cx.notify();
        }
    }

    fn set_has_more(&mut self, has_more: bool, cx: &mut Context<Self>) {
        self.table
            .update(cx, |table, _| table.delegate_mut().has_more = has_more);
    }

    fn session(&self) -> Option<&Arc<dyn Session>> {
        match &self.link {
            Link::Ready(session) => Some(session),
            _ => None,
        }
    }

    /// The table this tab shows.
    pub fn table_ref(&self) -> &TableRef {
        &self.table_ref
    }

    pub fn name(&self) -> SharedString {
        self.table_ref.name.clone().into()
    }

    /// The connection and table this tab shows.
    /// Where the tab connects.
    pub fn target(&self) -> QueryTarget {
        self.target.clone()
    }

    pub fn shows(&self, connection: &str, table: &TableRef) -> bool {
        self.target.id == connection && &self.table_ref == table
    }

    /// Whether rows can be edited, which also means the table has loaded.
    pub fn is_editable(&self) -> bool {
        self.read_only.is_none() && !self.columns.is_empty()
    }

    /// Why rows cannot be edited, when the table says so.
    pub fn read_only_reason(&self) -> Option<SharedString> {
        self.read_only.clone()
    }

    /// Whether rows are arriving right now.
    pub fn is_loading(&self) -> bool {
        let describing = matches!(self.load, Load::Describing)
            && !matches!(self.link, Link::Failed(_))
            && self.problem.is_none();
        describing || self.pull.is_some() || self.applying.is_some()
    }

    /// Whether rows are waiting that scrolling or "Load all rows" would fetch.
    pub fn has_more(&self) -> bool {
        matches!(self.load, Load::Paused)
    }

    pub fn rows_loaded(&self, cx: &App) -> usize {
        self.table.read(cx).delegate().rows.len()
    }

    /// Rows staged to be added, shown below the loaded ones.
    pub fn new_rows(&self, cx: &App) -> usize {
        self.table.read(cx).delegate().changes.inserts()
    }

    /// How many rows the staged changes touch.
    pub fn staged(&self, cx: &App) -> usize {
        self.table.read(cx).delegate().changes.count()
    }

    /// A loaded cell as it was read.
    pub fn value(&self, row: usize, column: usize, cx: &App) -> Option<Value> {
        self.table
            .read(cx)
            .delegate()
            .rows
            .get(row)?
            .get(column)
            .cloned()
    }

    /// Index of a column by name.
    pub fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column.name == name)
    }

    /// The headline of the last problem, if there is one.
    pub fn problem(&self) -> Option<SharedString> {
        self.problem
            .as_ref()
            .map(|problem| problem.headline.clone())
    }

    /// Whether the cell editor is open.
    pub fn is_editing(&self) -> bool {
        self.editing.is_some()
    }

    /// Put `text` in the filter box, and filter by typed text.
    pub fn set_filter(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.typed = true;
        self.filter
            .update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
    }

    /// Put the keyboard in the filter box.
    pub fn focus_filter(&self, window: &mut Window, cx: &mut App) {
        let handle = self.filter.read(cx).focus_handle(cx);
        handle.focus(window, cx);
    }

    /// Replace the open editor's text, as typing would.
    pub fn set_edit_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editing) = &self.editing {
            editing
                .input
                .update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
        }
    }

    /// Select a grid cell and put the keyboard on the grid.
    pub fn select_cell(
        &mut self,
        row: usize,
        column: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shown = self.shown_at(column, cx).unwrap_or(0);
        self.table
            .update(cx, |table, cx| table.set_selected_cell(row, shown, cx));
        self.focus_grid(window, cx);
    }

    /// Sort as a header click would, or with `Default` drop the sort.
    /// `column` is the table's own column index.
    pub fn sort_by(&mut self, column: usize, sort: ColumnSort, cx: &mut Context<Self>) {
        self.sort = match sort {
            ColumnSort::Default => None,
            sort => Some((column, sort)),
        };
        let current = self.sort;
        self.table
            .update(cx, |table, _| table.delegate_mut().sort = current);
        self.reload(cx);
    }

    /// The selected cell as a row place and the table's own column index.
    fn selected_cell(&self, cx: &App) -> Option<(Place, usize)> {
        let table = self.table.read(cx);
        let (row, shown) = table.selected_cell()?;
        let column = *table.delegate().order.get(shown)?;
        Some((self.place(row, cx)?, column))
    }

    /// Where the table's column `column` is shown, if it is.
    fn shown_at(&self, column: usize, cx: &App) -> Option<usize> {
        self.table
            .read(cx)
            .delegate()
            .order
            .iter()
            .position(|ix| *ix == column)
    }

    fn place(&self, row: usize, cx: &App) -> Option<Place> {
        let grid = self.table.read(cx).delegate();
        if row < grid.rows.len() {
            Some(Place::Loaded(row))
        } else {
            let ix = row - grid.rows.len();
            (ix < grid.changes.inserts()).then_some(Place::New(ix))
        }
    }

    /// Stage a new row and open the editor on its first column.
    pub fn add_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_editable() {
            return;
        }
        let (row, ix) = self.table.update(cx, |table, cx| {
            let grid = table.delegate_mut();
            let ix = grid.changes.add_insert();
            let row = grid.rows.len() + ix;
            table.refresh(cx);
            (row, ix)
        });
        self.applied = None;
        let column = self.first_writable_column();
        let shown = self.shown_at(column, cx).unwrap_or(0);
        self.table.update(cx, |table, cx| {
            table.set_selected_cell(row, shown, cx);
            table.scroll_to_row(row, cx);
        });
        self.open_editor(Place::New(ix), column, window, cx);
    }

    /// The first column a new row can be given a value for: not `rowid`,
    /// which SQLite assigns.
    fn first_writable_column(&self) -> usize {
        usize::from(self.key_columns == ["rowid"])
    }

    /// Stage the selected row for deletion, take that back, or drop a new
    /// row that was not written yet.
    pub fn toggle_delete(&mut self, cx: &mut Context<Self>) {
        if !self.is_editable() {
            return;
        }
        let Some((place, _)) = self.selected_cell(cx) else {
            return;
        };
        self.editing = None;
        self.applied = None;
        self.table.update(cx, |table, cx| {
            let grid = table.delegate_mut();
            match place {
                Place::Loaded(row) => {
                    if let Some(key) = grid.key_values(row) {
                        grid.changes.toggle_delete(&key);
                    }
                }
                Place::New(ix) => grid.changes.remove_insert(ix),
            }
            table.refresh(cx);
        });
        cx.notify();
    }

    /// Drop every staged change.
    pub fn discard(&mut self, cx: &mut Context<Self>) {
        self.editing = None;
        self.table.update(cx, |table, cx| {
            table.delegate_mut().changes.clear();
            table.refresh(cx);
        });
        cx.notify();
    }

    /// Open the editor on a grid cell, as a double-click would.
    pub fn edit_cell(
        &mut self,
        row: usize,
        column: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(place) = self.place(row, cx) {
            self.open_editor(place, column, window, cx);
        }
    }

    /// Open the editor on a cell, holding its current text.
    fn open_editor(
        &mut self,
        place: Place,
        column: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.is_editable() || column >= self.columns.len() {
            return;
        }
        if self.columns[column].name == "rowid" && self.key_columns == ["rowid"] {
            self.problem = Some(Problem::new(
                "SQLite assigns the rowid, so it cannot be edited.",
            ));
            cx.notify();
            return;
        }
        let (text, placeholder) = {
            let grid = self.table.read(cx).delegate();
            if let Place::Loaded(row) = place
                && grid
                    .row_key(row)
                    .is_some_and(|key| grid.changes.is_deleted(key))
            {
                // A row staged for deletion takes no edits until restored.
                return;
            }
            match grid.current(place, column) {
                Some(value) if !value.is_null() => (value.to_string(), ""),
                Some(_) => (String::new(), "NULL"),
                None => (String::new(), "default"),
            }
        };
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(text)
                .placeholder(placeholder)
        });
        let _events = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.save_edit(window, cx);
            }
        });
        input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        self.problem = None;
        self.editing = Some(Editing {
            place,
            column,
            input,
            _events,
        });
        cx.notify();
    }

    /// Stage the editor's text for its cell.
    pub fn save_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = &self.editing else {
            return;
        };
        let text = editing.input.read(cx).value().to_string();
        let (place, column) = (editing.place, editing.column);
        let original = match place {
            Place::Loaded(row) => self.value(row, column, cx),
            Place::New(_) => None,
        };
        let current = self
            .table
            .read(cx)
            .delegate()
            .current(place, column)
            .cloned();
        if text.is_empty() && current.as_ref().is_none_or(Value::is_null) {
            // An empty editor on a NULL or default cell leaves it as it was,
            // rather than turning a text column's NULL into ''.
            self.stage(place, column, current, window, cx);
            return;
        }
        match parse_cell(&self.columns[column].type_name, original.as_ref(), &text) {
            Ok(value) => self.stage(place, column, Some(value), window, cx),
            Err(message) => {
                self.problem = Some(Problem::new(format!(
                    "{} was not staged: {message}",
                    self.columns[column].name
                )));
                cx.notify();
            }
        }
    }

    /// Stage `NULL` for the editor's cell.
    pub fn set_null(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editing) = &self.editing {
            let (place, column) = (editing.place, editing.column);
            self.stage(place, column, Some(Value::Null), window, cx);
        }
    }

    /// Leave a new row's cell to the column default.
    pub fn use_default(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editing) = &self.editing {
            let (place, column) = (editing.place, editing.column);
            self.stage(place, column, None, window, cx);
        }
    }

    pub fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = None;
        self.focus_grid(window, cx);
        cx.notify();
    }

    /// Stage a cell's value. `None` is the column default, for new rows.
    fn stage(
        &mut self,
        place: Place,
        column: usize,
        value: Option<Value>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.table.update(cx, |table, cx| {
            let grid = table.delegate_mut();
            match place {
                Place::Loaded(row) => {
                    let (Some(key), Some(original)) = (
                        grid.key_values(row),
                        grid.rows
                            .get(row)
                            .and_then(|cells| cells.get(column))
                            .cloned(),
                    ) else {
                        return;
                    };
                    let value = value.unwrap_or(Value::Null);
                    grid.changes.set_cell(&key, column, &original, value);
                }
                Place::New(ix) => grid.changes.set_insert_cell(ix, column, value),
            }
            cx.notify();
        });
        self.editing = None;
        self.problem = None;
        self.applied = None;
        self.focus_grid(window, cx);
        cx.notify();
    }

    fn focus_grid(&self, window: &mut Window, cx: &mut App) {
        let handle = self.table.read(cx).focus_handle(cx);
        handle.focus(window, cx);
    }

    /// The staged changes as statements, in the order apply runs them.
    pub fn statements(&self, cx: &App) -> Vec<EditStatement> {
        let Some(session) = self.session() else {
            return Vec::new();
        };
        let key_columns: Vec<&str> = self.key_columns.iter().map(String::as_str).collect();
        let columns: Vec<&str> = self
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect();
        self.table.read(cx).delegate().changes.statements(
            session.dialect(),
            &self.table_ref,
            &key_columns,
            &columns,
        )
    }

    /// Show the statements the staged changes become, to apply or not.
    pub fn review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let statements = self.statements(cx);
        if statements.is_empty() || self.applying.is_some() {
            return;
        }
        self.editing = None;
        let count = statements.len();
        let preview = statements
            .iter()
            .map(|statement| format!("{};", statement.rendered()))
            .collect::<Vec<_>>()
            .join("\n");
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
                .default_value(preview.clone())
        });
        let view = cx.weak_entity();
        let summary = format!(
            "{count} {} will run in one transaction. If any of them does not \
             change exactly one row, none of them is kept.",
            if count == 1 {
                "statement"
            } else {
                "statements"
            }
        );
        window.open_dialog(cx, move |dialog, window, cx| {
            let view = view.clone();
            let button_label = if count == 1 { "change" } else { "changes" };
            let title = format!("Apply {count} {button_label}");
            let preview_height = (window.viewport_size().height * 0.4).clamp(px(140.), px(320.));
            dialog
                .title(title.clone())
                .w(px(720.))
                .p_6()
                .child(
                    v_flex()
                        .gap_4()
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(summary.clone()),
                        )
                        .child(
                            div()
                                .id("apply-preview")
                                .test_support()
                                .aria_label(SharedString::from(preview.clone()))
                                .h(preview_height)
                                .p_2()
                                .border_1()
                                .border_color(cx.theme().border)
                                .rounded(cx.theme().radius)
                                .child(
                                    Editor::new(&editor)
                                        .h(relative(1.))
                                        .readonly(true)
                                        .appearance(false)
                                        .aria_label("Statements to apply"),
                                ),
                        ),
                )
                .footer(
                    h_flex()
                        .flex_wrap()
                        .gap_3()
                        .justify_end()
                        .child(
                            Button::new("keep-editing")
                                .outline()
                                .h_10()
                                .label("Keep editing")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("apply-changes")
                                .primary()
                                .h_10()
                                .label(title)
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    // A closed tab has nothing to apply.
                                    let _ = view.update(cx, |view, cx| view.apply(cx));
                                }),
                        ),
                )
        });
    }

    /// Write the staged changes in one transaction.
    pub fn apply(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session().cloned() else {
            return;
        };
        let statements = self.statements(cx);
        if statements.is_empty() || self.applying.is_some() {
            return;
        }
        // A parked stream holds the session; the rows reload afterwards.
        self.stop(cx);
        self.editing = None;
        self.problem = None;
        let started = Instant::now();
        let count = self.staged(cx);
        self.applying = Some(cx.spawn(async move |this, cx| {
            let result = session.apply(&statements).await;
            let _ = this.update(cx, |this, cx| {
                this.applying = None;
                match result {
                    Ok(()) => {
                        this.discard(cx);
                        this.reload(cx);
                        this.applied = Some(format!(
                            "Applied {count} {} in {}",
                            if count == 1 { "change" } else { "changes" },
                            cell::elapsed(started.elapsed())
                        ));
                    }
                    Err(Error::Stale) => {
                        this.discard(cx);
                        this.reload(cx);
                        this.problem = Some(Problem {
                            headline: "Nothing was written. A row you changed was changed \
                                       or deleted elsewhere after it was loaded."
                                .into(),
                            notes: vec![
                                "Every statement was rolled back and the staged changes \
                                 were dropped. The rows have been reloaded; make the \
                                 changes again on what is there now."
                                    .into(),
                            ],
                        });
                    }
                    Err(error) => {
                        // The changes stay staged, so one bad value can be
                        // fixed without redoing the rest.
                        let mut problem = Problem::from_error("Nothing was written: ", &error);
                        problem.notes.push(
                            "Every statement was rolled back. The changes are still \
                             staged; fix them and apply again, or discard them."
                                .into(),
                        );
                        this.problem = Some(problem);
                        this.table.update(cx, |table, cx| table.refresh(cx));
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// One line describing where the tab stands.
    pub fn status(&self, cx: &App) -> String {
        match &self.link {
            Link::Connecting => return format!("Connecting to {}", self.target.name),
            Link::Failed(message) => return format!("Could not connect: {message}"),
            Link::Ready(_) => {}
        }
        if self.applying.is_some() {
            return "Applying changes".into();
        }
        let rows = self.rows_loaded(cx);
        let rows_text = format!("{rows} {}", if rows == 1 { "row" } else { "rows" });
        let mut line = match self.load {
            Load::Describing => "Reading the table's columns".into(),
            Load::Streaming if rows == 0 => "Loading rows".into(),
            Load::Streaming => format!("{rows_text} so far"),
            Load::Paused => format!("{rows_text} loaded, more available"),
            Load::Finished if !self.filtered.trim().is_empty() => {
                format!("{rows_text} match the filter")
            }
            Load::Finished => rows_text,
            Load::Stopped => format!("{rows_text} loaded, stopped"),
            Load::Failed => format!("{rows_text} loaded"),
        };
        if let Some(applied) = &self.applied {
            line = format!("{applied}. {line}");
        }
        let staged = self.staged(cx);
        if staged > 0 {
            line.push_str(&format!(
                ", {staged} {} staged",
                if staged == 1 { "change" } else { "changes" }
            ));
        }
        line
    }

    fn on_table_event(
        &mut self,
        _: &Entity<TableState<Grid>>,
        event: &TableEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match *event {
            TableEvent::DoubleClickedCell(row, shown) => {
                let column = self.table.read(cx).delegate().order.get(shown).copied();
                if let (Some(place), Some(column)) = (self.place(row, cx), column) {
                    self.open_editor(place, column, window, cx);
                }
            }
            TableEvent::MoveColumn(from, to) => self.moved_column(from, to, cx),
            _ => {}
        }
    }

    fn edit_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((place, column)) = self.selected_cell(cx) {
            self.open_editor(place, column, window, cx);
        }
    }
}

/// Parse an edited cell's text for its column.
///
/// Text columns keep the text exactly as typed, spaces included, and an
/// empty text is an empty string: `NULL` has its own button. Other columns
/// go through [`sonde_core::value::parse`], where an empty text is `NULL`.
/// A column with no declared type (SQLite allows that) is parsed as the kind
/// of value it held.
fn parse_cell(type_name: &str, original: Option<&Value>, text: &str) -> Result<Value, String> {
    let inferred;
    let type_name = if type_name.trim().is_empty() {
        inferred = match original {
            Some(Value::Int(_)) => "integer",
            Some(Value::Float(_)) => "real",
            Some(Value::Bytes(_)) => "blob",
            _ => "text",
        };
        inferred
    } else {
        type_name
    };
    // `parse` keeps anything it has no rule for as text; asking it about a
    // sample tells whether this column is one of those.
    let texty = matches!(sonde_core::value::parse(type_name, "x"), Ok(Value::Text(_)));
    if texty {
        Ok(Value::text(text.to_owned()))
    } else {
        sonde_core::value::parse(type_name, text)
    }
}

impl Focusable for TableView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        // Opening the tab puts the keyboard in the grid.
        self.table.read(cx).focus_handle(cx)
    }
}

impl TableView {
    /// The saved connection's name.
    pub fn connection_name(&self) -> &str {
        &self.target.name
    }

    /// The table's schema, when it has one.
    pub fn schema_name(&self) -> Option<&str> {
        self.table_ref.schema.as_deref()
    }
}

impl Render for TableView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A narrow window gets icons where there is room for words.
        let narrow = window.viewport_size().width < px(1000.);
        let (border, quiet, danger, panel) = {
            let theme = cx.theme();
            (
                theme.border,
                theme.muted_foreground,
                theme.danger,
                crate::design::tokens(cx).well,
            )
        };
        let ready = matches!(self.link, Link::Ready(_)) && !self.columns.is_empty();
        let editable = ready && self.is_editable();
        let applying = self.applying.is_some();
        let staged = self.staged(cx);
        let status = self.status(cx);
        let failed_link = matches!(self.link, Link::Failed(_));
        let loading = matches!(self.load, Load::Streaming) && self.pull.is_some();
        let has_more = self.has_more();

        let kind = self.target.config.kind();
        let rule_count = self.builder.as_ref().map_or(0, |b| b.read(cx).rule_count());
        let builder_mode = self.builder.is_some() && !self.typed;
        let summary_mode = self.mode == Mode::Summary;
        let filter_controls =
            if builder_mode {
                h_flex()
                    .flex_basis(px(256.))
                    .flex_grow_1()
                    .min_w_0()
                    .gap_2()
                    .child(
                        Button::new("filter-rules")
                            .outline()
                            .h_10()
                            .icon(crate::ui::icon(crate::ui::Glyph::ListFilter))
                            .label(if rule_count == 0 {
                                "Filter".to_owned()
                            } else {
                                format!("Filter · {rule_count}")
                            })
                            .selected(self.builder_open && rule_count > 0)
                            .tooltip("Narrow the rows with rules")
                            .disabled(!ready)
                            .on_click(cx.listener(|this, _, window, cx| {
                                let empty = this
                                    .builder
                                    .as_ref()
                                    .is_none_or(|b| b.read(cx).rule_count() == 0);
                                if empty || !this.builder_open {
                                    if empty {
                                        this.add_rule(None, window, cx);
                                    }
                                    this.builder_open = true;
                                } else {
                                    this.builder_open = false;
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("filter-as-text")
                            .ghost()
                            .h_10()
                            .map(|this| {
                                if narrow {
                                    this.icon(crate::ui::icon(crate::ui::Glyph::PencilLine))
                                } else {
                                    this.label("Type a filter")
                                }
                            })
                            .tooltip(sonde_core::dialect(kind).filter_hint())
                            .disabled(!ready)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.set_typed(true, window, cx)),
                            ),
                    )
                    .child(div().flex_1())
            } else {
                h_flex()
                    .flex_basis(px(256.))
                    .flex_grow_1()
                    .min_w_0()
                    .gap_3()
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&self.filter)
                                .h_10()
                                .prefix(crate::ui::icon(IconName::Search))
                                .aria_label("Filter rows"),
                        ),
                    )
                    .when(self.builder.is_some(), |this| {
                        this.child(
                            Button::new("filter-use-rules")
                                .ghost()
                                .h_10()
                                .flex_shrink_0()
                                .label("Use rules")
                                .tooltip("Build the filter from lists instead of typing it")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.set_typed(false, window, cx)
                                })),
                        )
                    })
            };
        let filter_controls = filter_controls.child(
            Button::new("reload-rows")
                .outline()
                .h_10()
                .flex_shrink_0()
                .label("Reload")
                .tooltip("Run the filter and load the rows again (Enter in the filter)")
                .disabled(!ready || applying)
                .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
        );
        let summarizes = filter::summarizes(kind);
        let view_actions = h_flex()
            .flex_wrap()
            .min_w_0()
            .gap_2()
            .when(summarizes, |this| {
                this.child(
                    gpui_kit::component::button::ButtonGroup::new("table-mode")
                        .outline()
                        .child(
                            Button::new("mode-rows")
                                .h_10()
                                .map(|this| {
                                    if narrow {
                                        this.icon(crate::ui::icon(crate::ui::Glyph::Rows3))
                                    } else {
                                        this.label("Rows")
                                    }
                                })
                                .tooltip("Rows")
                                .selected(!summary_mode),
                        )
                        .child(
                            Button::new("mode-summary")
                                .h_10()
                                .icon(crate::ui::icon(crate::ui::Glyph::ChartColumn))
                                .when(!narrow, |this| this.label("Summary"))
                                .tooltip("Summary: group the rows and count or add them up")
                                .selected(summary_mode),
                        )
                        .disabled(!ready || applying)
                        .on_click(cx.listener(|this, picked: &Vec<usize>, window, cx| {
                            let mode = if picked.first() == Some(&1) {
                                Mode::Summary
                            } else {
                                Mode::Rows
                            };
                            this.set_mode(mode, window, cx);
                        })),
                )
            })
            .child(
                Button::new("table-columns")
                    .ghost()
                    .h_10()
                    .icon(crate::ui::icon(crate::ui::Glyph::Columns3))
                    .tooltip("Show, hide, order and pin columns")
                    .disabled(!ready || summary_mode)
                    .on_click(cx.listener(|this, _, window, cx| this.open_columns(window, cx))),
            )
            .child(
                Button::new("row-form")
                    .ghost()
                    .h_10()
                    .icon(crate::ui::icon(crate::ui::Glyph::SquarePen))
                    .tooltip("Open the selected row as a form")
                    .disabled(!ready || summary_mode)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.open_selected_form(window, cx)),
                    ),
            )
            .child(
                Button::new("save-view")
                    .ghost()
                    .h_10()
                    .icon(crate::ui::icon(crate::ui::Glyph::Bookmark))
                    .tooltip("Save this filter, sort and columns as a view in the sidebar")
                    .disabled(!ready)
                    .on_click(cx.listener(|this, _, _, cx| this.save_view(cx))),
            );

        let toolbar = h_flex()
            .flex_shrink_0()
            .flex_wrap()
            .gap_3()
            .pb_5()
            .items_center()
            .child(filter_controls)
            .child(view_actions)
            .when(editable && !summary_mode, |this| {
                let row_actions = h_flex()
                    .flex_wrap()
                    .min_w_0()
                    .gap_2()
                    .child(
                        Button::new("add-row")
                            .ghost()
                            .h_10()
                            .icon(IconName::Plus)
                            .when(!narrow, |this| this.label("Add row"))
                            .tooltip("Stage a new row below the loaded ones")
                            .disabled(applying)
                            .on_click(cx.listener(|this, _, window, cx| this.add_row(window, cx))),
                    )
                    .child(
                        Button::new("delete-row")
                            .ghost()
                            .h_10()
                            .map(|this| {
                                if narrow {
                                    this.icon(crate::ui::icon(crate::ui::Glyph::Trash))
                                } else {
                                    this.label("Delete row")
                                }
                            })
                            .tooltip("Stage the selected row for deletion, or restore it (Delete)")
                            .disabled(applying)
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_delete(cx))),
                    );
                let change_actions = h_flex()
                    .flex_wrap()
                    .min_w_0()
                    .gap_2()
                    .child(
                        Button::new("discard-changes")
                            .ghost()
                            .h_10()
                            .label("Discard")
                            .tooltip("Drop every staged change")
                            .disabled(staged == 0 || applying)
                            .on_click(cx.listener(|this, _, _, cx| this.discard(cx))),
                    )
                    .child(
                        Button::new("review-changes")
                            .primary()
                            .h_10()
                            .label(if staged == 0 {
                                "Apply".to_owned()
                            } else {
                                format!("Apply {staged}")
                            })
                            .tooltip("Review the statements, then write them in one transaction")
                            .disabled(staged == 0 || applying)
                            .on_click(cx.listener(|this, _, window, cx| this.review(window, cx))),
                    );
                this.child(
                    h_flex()
                        .flex_wrap()
                        .min_w_0()
                        .gap_3()
                        .child(row_actions)
                        .child(change_actions),
                )
            });

        let edit_bar = self.editing.as_ref().map(|editing| {
            let column = &self.columns[editing.column];
            let label = if column.type_name.is_empty() {
                column.name.clone()
            } else {
                format!("{} ({})", column.name, column.type_name)
            };
            let new_row = matches!(editing.place, Place::New(_));
            let field = h_flex()
                .flex_basis(px(184.))
                .flex_grow_1()
                .min_w_0()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .font_family(cx.theme().mono_font_family.clone())
                        .child(Input::new(&editing.input).h_10().aria_label("Cell value")),
                )
                .when(new_row, |this| {
                    this.child(
                        Button::new("use-default")
                            .ghost()
                            .h_10()
                            .flex_shrink_0()
                            .label("Use default")
                            .tooltip("Leave this column to its default value")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.use_default(window, cx)),
                            ),
                    )
                });
            let actions = h_flex()
                .flex_wrap()
                .min_w_0()
                .gap_2()
                .child(
                    Button::new("set-null")
                        .ghost()
                        .h_10()
                        .label("Set NULL")
                        .on_click(cx.listener(|this, _, window, cx| this.set_null(window, cx))),
                )
                .child(
                    Button::new("cancel-edit")
                        .ghost()
                        .h_10()
                        .label("Cancel")
                        .tooltip("Close without staging (Escape)")
                        .on_click(cx.listener(|this, _, window, cx| this.cancel_edit(window, cx))),
                )
                .child(
                    Button::new("stage-edit")
                        .outline()
                        .h_10()
                        .label("Stage")
                        .tooltip("Stage this value; nothing is written until Apply (Enter)")
                        .on_click(cx.listener(|this, _, window, cx| this.save_edit(window, cx))),
                );
            v_flex()
                .id("cell-editor")
                .test_support()
                .flex_shrink_0()
                .gap_3()
                .px_5()
                .py_4()
                .border_b_1()
                .border_color(border)
                .bg(panel)
                .child(
                    div()
                        .truncate()
                        .font_family(crate::design::MONO_FONT)
                        .text_sm()
                        .text_color(quiet)
                        .child(label),
                )
                .child(h_flex().flex_wrap().gap_3().child(field).child(actions))
        });

        let banner = self.read_only.clone().map(|reason| {
            h_flex()
                .id("table-read-only")
                .test_support()
                .aria_label(reason.clone())
                .flex_shrink_0()
                .gap_3()
                .px_5()
                .py_3()
                .text_sm()
                .text_color(quiet)
                .border_b_1()
                .border_color(border)
                .child(gpui_kit::component::Icon::new(IconName::Info).small())
                .child(reason)
        });

        let problem = self.problem.as_ref().map(|problem| {
            v_flex()
                .id("table-problem")
                .test_support()
                .aria_label(problem.headline.clone())
                .flex_shrink_0()
                .gap_2()
                .px_5()
                .py_3()
                .text_sm()
                .border_b_1()
                .border_color(border)
                .child(div().text_color(danger).child(problem.headline.clone()))
                .children(
                    problem
                        .notes
                        .iter()
                        .map(|note| div().text_color(quiet).child(note.clone())),
                )
        });

        let rules_panel = (builder_mode && self.builder_open && rule_count > 0)
            .then(|| self.builder.clone())
            .flatten()
            .map(|builder| {
                let preview = self.current_filter(cx);
                v_flex()
                    .id("filter-builder")
                    .test_support()
                    .flex_shrink_0()
                    .gap_3()
                    .px_5()
                    .py_4()
                    .border_b_1()
                    .border_color(border)
                    .bg(panel)
                    .child(builder)
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_2()
                            .items_center()
                            .child(
                                Button::new("filter-apply")
                                    .primary()
                                    .small()
                                    .label("Apply filter")
                                    .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                            )
                            .child(
                                Button::new("filter-clear")
                                    .ghost()
                                    .small()
                                    .label("Clear")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.clear_filter(window, cx)
                                    })),
                            )
                            .child(
                                div()
                                    .id("filter-preview")
                                    .test_support()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .font_family(crate::design::MONO_FONT)
                                    .map(|this| match &preview {
                                        Ok(text) => this
                                            .aria_label(SharedString::from(text.clone()))
                                            .text_color(quiet)
                                            .child(text.clone()),
                                        Err(message) => this
                                            .aria_label(SharedString::from(message.clone()))
                                            .text_color(danger)
                                            .child(message.clone()),
                                    }),
                            ),
                    )
            });

        let expiry = (kind == DbKind::Redis && ready).then(|| {
            let text = match self.ttl {
                Some(seconds) if seconds >= 0 => format!(
                    "Expires in {}",
                    cell::elapsed(std::time::Duration::from_secs(seconds as u64))
                ),
                Some(_) => "Never expires".to_owned(),
                None => "Reading the expiry".to_owned(),
            };
            h_flex()
                .id("key-expiry")
                .test_support()
                .aria_label(SharedString::from(text.clone()))
                .flex_shrink_0()
                .gap_3()
                .px_5()
                .py_2()
                .items_center()
                .text_sm()
                .text_color(quiet)
                .border_b_1()
                .border_color(border)
                .child(gpui_kit::component::Icon::new(crate::ui::Glyph::Clock).small())
                .child(div().flex_1().child(text))
                .child(
                    Button::new("set-expiry")
                        .ghost()
                        .small()
                        .label("Set expiry…")
                        .on_click(cx.listener(|this, _, window, cx| this.open_expiry(window, cx))),
                )
        });

        let body = if summary_mode && let Some(panel) = self.summary.clone() {
            div().flex_1().min_h_0().child(panel).into_any_element()
        } else if ready {
            div()
                .key_context(GRID_CONTEXT)
                .on_action(cx.listener(|this, _: &EditCell, window, cx| {
                    this.edit_selected(window, cx);
                }))
                .on_action(cx.listener(|this, _: &ToggleDelete, _, cx| this.toggle_delete(cx)))
                .flex_1()
                .min_h_0()
                .font_family(cx.theme().mono_font_family.clone())
                .child(
                    DataTable::new(&self.table)
                        .with_size(px(ROW_HEIGHT))
                        .header_row_height(px(GRID_HEADER_HEIGHT))
                        .bordered(false)
                        .accessibility_label(format!("{} rows", self.table_ref.name)),
                )
                .into_any_element()
        } else {
            div()
                .flex_1()
                .p_5()
                .text_sm()
                .text_color(quiet)
                .child(if failed_link {
                    "This tab has no connection. Close it and open the table again once \
                     the connection works."
                } else if self.problem.is_some() {
                    ""
                } else {
                    "Reading the table."
                })
                .into_any_element()
        };

        let status_bar = h_flex()
            .flex_shrink_0()
            .gap_3()
            .px_5()
            .py_2()
            .min_h_12()
            .items_center()
            .border_t_1()
            .border_color(border)
            .bg(panel)
            .text_sm()
            .child(
                div()
                    .id("table-status")
                    .test_support()
                    .flex_1()
                    .truncate()
                    .text_color(if failed_link { danger } else { quiet })
                    .child(status),
            )
            .when(has_more, |this| {
                this.child(
                    Button::new("load-all")
                        .ghost()
                        .small()
                        .label("Load all rows")
                        .on_click(cx.listener(|this, _, _, cx| this.load_all(cx))),
                )
            })
            .when(loading || has_more, |this| {
                this.child(
                    Button::new("stop-loading")
                        .ghost()
                        .small()
                        .label("Stop")
                        .tooltip("Keep the rows loaded so far and stop reading")
                        .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                )
            });

        let card = crate::ui::card(cx)
            .flex_1()
            .min_h_0()
            .when_some(banner.filter(|_| !summary_mode), |this, banner| {
                this.child(banner)
            })
            .when_some(expiry, |this, expiry| this.child(expiry))
            .when_some(rules_panel, |this, panel| this.child(panel))
            .when_some(problem, |this, problem| this.child(problem))
            .when_some(edit_bar, |this, bar| {
                this.child(
                    div()
                        .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                            if event.keystroke.key == "escape" {
                                this.cancel_edit(window, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .child(bar),
                )
            })
            .child(body)
            .when(!summary_mode, |this| this.child(status_bar));

        v_flex().size_full().child(toolbar).child(card)
    }
}

/// The grid's rows and how to draw them, staged changes included.
struct Grid {
    view: WeakEntity<TableView>,
    /// The table's columns, in table order.
    columns: Vec<Column>,
    /// The columns shown, as indices into `columns`, in display order.
    order: Vec<usize>,
    /// How many of `order` are pinned at the left.
    pinned: usize,
    /// The sorted column, as a table column index.
    sort: Option<(usize, ColumnSort)>,
    /// Which columns link to another table's row.
    links: Vec<bool>,
    numeric: Vec<bool>,
    rows: Vec<Vec<Value>>,
    /// Each loaded row's key, when rows can be edited.
    keys: Vec<Option<RowKey>>,
    key_positions: Vec<usize>,
    changes: Changes,
    has_more: bool,
    loading: bool,
    filtered: bool,
    editable: bool,
}

impl Grid {
    fn append(&mut self, rows: Vec<Vec<Value>>) {
        for row in rows {
            let key = self.key_of(&row).map(|key| row_key(&key));
            self.keys.push(key);
            self.rows.push(row);
        }
    }

    fn key_of(&self, row: &[Value]) -> Option<Vec<Value>> {
        if self.key_positions.is_empty() {
            return None;
        }
        self.key_positions
            .iter()
            .map(|ix| row.get(*ix).cloned())
            .collect()
    }

    fn key_values(&self, row: usize) -> Option<Vec<Value>> {
        self.key_of(self.rows.get(row)?)
    }

    fn row_key(&self, row: usize) -> Option<&RowKey> {
        self.keys.get(row)?.as_ref()
    }

    /// A cell's value with its staged edit applied. `None` for a new row's
    /// cell left to its default.
    fn current(&self, place: Place, column: usize) -> Option<&Value> {
        match place {
            Place::Loaded(row) => self
                .row_key(row)
                .and_then(|key| self.changes.cell(key, column))
                .or_else(|| self.rows.get(row)?.get(column)),
            Place::New(ix) => self.changes.insert_cell(ix, column),
        }
    }

    /// The table column shown at `shown`.
    fn data(&self, shown: usize) -> usize {
        self.order.get(shown).copied().unwrap_or(shown)
    }

    fn place(&self, row: usize) -> Place {
        if row < self.rows.len() {
            Place::Loaded(row)
        } else {
            Place::New(row - self.rows.len())
        }
    }
}

impl TableDelegate for Grid {
    fn columns_count(&self, _: &App) -> usize {
        self.order.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len() + self.changes.inserts()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let data = self.data(col_ix);
        let mut column = self.columns[data].clone();
        if col_ix < self.pinned {
            column = column.fixed_left();
        }
        if let Some((sorted, sort)) = self.sort
            && sorted == data
            && column.sort.is_some()
        {
            column = column.sort(sort);
        }
        column
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        let col_ix = self.data(col_ix);
        // The table is mid-update here, and sorting reloads it, so the
        // reload waits until this update has unwound.
        let view = self.view.clone();
        cx.defer(move |cx| {
            let _ = view.update(cx, |view, cx| view.sort_by(col_ix, sort, cx));
        });
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let col_ix = self.data(col_ix);
        let numeric = self.numeric.get(col_ix).copied().unwrap_or(false);
        let link = self.links.get(col_ix).copied().unwrap_or(false);
        // shadcn's table head: muted, medium, on a hairline.
        h_flex()
            .w_full()
            .h_full()
            .px(px(GRID_CELL_INSET))
            .border_b_1()
            .border_color(cx.theme().border)
            .font_family(crate::design::UI_FONT)
            .font_weight(FontWeight::MEDIUM)
            .text_color(cx.theme().muted_foreground)
            .when(numeric, |this| this.justify_end())
            .gap_1()
            .child(div().truncate().child(self.columns[col_ix].name.clone()))
            .when(link, |this| {
                this.child(
                    gpui_kit::component::Icon::new(crate::ui::Glyph::ArrowUpRight)
                        .xsmall()
                        .text_color(cx.theme().muted_foreground),
                )
            })
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let col_ix = self.data(col_ix);
        let theme = cx.theme();
        let numeric = self.numeric.get(col_ix).copied().unwrap_or(false);
        let link = self.links.get(col_ix).copied().unwrap_or(false);
        let place = self.place(row_ix);
        let key = match place {
            Place::Loaded(row) => self.row_key(row),
            Place::New(_) => None,
        };
        let deleted = key.is_some_and(|key| self.changes.is_deleted(key));
        let edited = key.is_some_and(|key| self.changes.cell(key, col_ix).is_some());
        let is_new = matches!(place, Place::New(_));
        let value = self.current(place, col_ix);

        if value.is_none() && !is_new {
            return h_flex().size_full();
        }
        h_flex()
            .size_full()
            .px(px(GRID_CELL_INSET))
            .when(numeric, |this| this.justify_end())
            // A new row is not on the page yet: it sits in a well.
            .when(is_new, |this| this.bg(theme.secondary_hover))
            .child(match value {
                Some(value) => {
                    // Change state takes priority over the value's normal color.
                    let ink = if deleted {
                        theme.danger
                    } else if edited || is_new {
                        crate::design::tokens(cx).staged
                    } else {
                        crate::design::value_ink(value, cx)
                    };
                    let linked = link && !value.is_null() && !deleted && !edited && !is_new;
                    div()
                        .text_color(ink)
                        .when(linked, |this| this.underline())
                        .when(edited, |this| this.font_weight(FontWeight::SEMIBOLD))
                        .when(deleted, |this| this.line_through())
                        .map(|this| match cell::display(value) {
                            Some(text) => this.truncate().child(text),
                            None => this.italic().child("NULL"),
                        })
                }
                None => div()
                    .italic()
                    .text_color(theme.muted_foreground)
                    .child("default"),
            })
    }

    fn cell_label(&self, row_ix: usize, col_ix: usize, _: &App) -> Option<SharedString> {
        let col_ix = self.data(col_ix);
        let place = self.place(row_ix);
        let text = match self.current(place, col_ix) {
            Some(value) => cell::display(value).unwrap_or_else(|| "NULL".into()),
            None => "default".into(),
        };
        let key = match place {
            Place::Loaded(row) => self.row_key(row),
            Place::New(_) => None,
        };
        Some(if key.is_some_and(|key| self.changes.is_deleted(key)) {
            format!("{text}, staged for deletion").into()
        } else if key.is_some_and(|key| self.changes.cell(key, col_ix).is_some()) {
            format!("{text}, changed").into()
        } else if matches!(place, Place::New(_)) {
            format!("{text}, new row").into()
        } else {
            text
        })
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _: &App) -> String {
        let col_ix = self.data(col_ix);
        self.current(self.place(row_ix), col_ix)
            .map(ToString::to_string)
            .unwrap_or_default()
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let text = match (self.filtered, self.editable) {
            (true, _) => "No rows match the filter.",
            (false, true) => "This table has no rows. Add row stages one.",
            (false, false) => "This table has no rows.",
        };
        h_flex()
            .size_full()
            .justify_center()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(text)
    }

    fn loading(&self, _: &App) -> bool {
        self.loading && self.rows.is_empty()
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: gpui_kit::component::menu::PopupMenu,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> gpui_kit::component::menu::PopupMenu {
        use crate::ui::{Glyph, menu_item};
        let Some(view) = self.view.upgrade() else {
            return menu;
        };
        let place = self.place(row_ix);
        let selected = cx
            .entity()
            .read(cx)
            .selected_cell()
            .filter(|(row, _)| *row == row_ix)
            .map(|(_, shown)| self.data(shown));
        let (editable, outgoing, incoming, columns, has_builder) = {
            let view = view.read(cx);
            let outgoing: Vec<ForeignKey> = view
                .detail
                .as_ref()
                .map(|d| d.foreign_keys.clone())
                .unwrap_or_default();
            (
                view.is_editable(),
                outgoing,
                view.referencing.clone(),
                view.column_names(),
                view.builder.is_some(),
            )
        };
        let weak = self.view.clone();
        let mut menu = menu.min_w(px(crate::ui::MENU_WIDTH)).item(
            menu_item(
                if editable {
                    "Edit row in form…"
                } else {
                    "Open row as form…"
                },
                Glyph::SquarePen,
                false,
            )
            .on_click({
                let weak = weak.clone();
                move |_, window, cx| {
                    let _ = weak.update(cx, |view, cx| view.open_form(place, window, cx));
                }
            }),
        );
        if let (Some(column), true) = (selected, has_builder) {
            let name = columns.get(column).cloned().unwrap_or_default();
            menu = menu.item(
                menu_item(format!("Only rows with this {name}"), Glyph::Search, false).on_click({
                    let weak = weak.clone();
                    move |_, window, cx| {
                        let _ = weak.update(cx, |view, cx| {
                            view.filter_by_value(place, column, window, cx)
                        });
                    }
                }),
            );
        }
        let loaded = matches!(place, Place::Loaded(_));
        if loaded && (!outgoing.is_empty() || !incoming.is_empty()) {
            menu = menu.separator();
        }
        for key in outgoing.into_iter().filter(|_| loaded) {
            let label = format!("Open {} ({})", key.references.name, key.columns.join(", "));
            let weak = weak.clone();
            menu = menu.item(menu_item(label, Glyph::ArrowUpRight, false).on_click(
                move |_, _, cx| {
                    let _ = weak.update(cx, |view, cx| view.follow(place, &key, cx));
                },
            ));
        }
        for referencing in incoming.into_iter().filter(|_| loaded) {
            let label = format!("{} rows pointing here", referencing.table.name);
            let weak = weak.clone();
            menu = menu.item(
                menu_item(label, Glyph::ListTree, false).on_click(move |_, _, cx| {
                    let _ = weak.update(cx, |view, cx| {
                        view.show_referencing(place, &referencing, cx)
                    });
                }),
            );
        }
        if editable {
            let deleted = match place {
                Place::Loaded(row) => self
                    .row_key(row)
                    .is_some_and(|key| self.changes.is_deleted(key)),
                Place::New(_) => false,
            };
            menu = menu.separator().item(
                menu_item(
                    if deleted { "Restore row" } else { "Delete row" },
                    Glyph::Trash,
                    !deleted,
                )
                .on_click(move |_, window, cx| {
                    let _ = weak.update(cx, |view, cx| {
                        let shown = view.shown_at(0, cx).unwrap_or(0);
                        view.table
                            .update(cx, |table, cx| table.set_selected_cell(row_ix, shown, cx));
                        view.toggle_delete(cx);
                        view.focus_grid(window, cx);
                    });
                }),
            );
        }
        menu
    }

    fn has_more(&self, _: &App) -> bool {
        self.has_more
    }

    fn load_more_threshold(&self) -> usize {
        200
    }

    fn load_more(&mut self, _: &mut Window, cx: &mut Context<TableState<Self>>) {
        // The table is mid-update here, and pulling updates the table, so the
        // pull waits until this update has unwound.
        let view = self.view.clone();
        cx.defer(move |cx| {
            let _ = view.update(cx, |view, cx| view.pull(eager_pages(cx), cx));
        });
    }
}

#[cfg(test)]
mod tests {
    // Not a glob: gpui's `test` attribute would shadow the standard one.
    use super::parse_cell;
    use sonde_core::Value;

    #[test]
    fn text_columns_should_keep_spaces_and_empty_strings() {
        assert_eq!(parse_cell("text", None, "  a "), Ok(Value::text("  a ")));
        assert_eq!(parse_cell("varchar(20)", None, ""), Ok(Value::text("")));
    }

    #[test]
    fn typed_columns_should_parse_and_treat_empty_as_null() {
        assert_eq!(parse_cell("integer", None, " 42 "), Ok(Value::Int(42)));
        assert_eq!(parse_cell("integer", None, ""), Ok(Value::Null));
        assert!(parse_cell("integer", None, "forty").is_err());
    }

    #[test]
    fn untyped_columns_should_parse_as_the_value_they_held() {
        assert_eq!(parse_cell("", Some(&Value::Int(1)), "7"), Ok(Value::Int(7)));
        assert_eq!(
            parse_cell("", Some(&Value::text("x")), "7"),
            Ok(Value::text("7"))
        );
    }
}
