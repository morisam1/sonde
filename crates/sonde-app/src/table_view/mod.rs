//! A table tab: one table's rows through a paged `SELECT`, sorted by the
//! server and narrowed by a `WHERE` fragment, with edits staged until they
//! are previewed and applied in one transaction.

mod changes;

use std::{sync::Arc, time::Instant};

use futures::{
    StreamExt as _,
    channel::{mpsc, oneshot},
};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Editor, EditorState, Input, InputEvent, InputState},
    table::{Column, ColumnSort, DataTable, TableDelegate, TableEvent, TableState},
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use sonde_core::{
    ConnectionConfig, Error, QuerySummary, ResultPage, Session, SqliteConfig, Value,
    config::SecretStore,
    edit::EditStatement,
    schema::{TableDetail, TableRef},
};

use self::changes::{Changes, RowKey, row_key};
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
    target: QueryTarget,
    table_ref: TableRef,
    link: Link,
    columns: Vec<GridColumn>,
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
        let dialect = sonde_core::dialect(target.config.kind());
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder(dialect.filter_hint()));
        let grid = Grid {
            view: cx.weak_entity(),
            columns: Vec::new(),
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
                .col_movable(false)
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
            target,
            table_ref,
            link: Link::Connecting,
            columns: Vec::new(),
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
        this.connect(secrets, cx);
        this
    }

    /// Open this tab's own session, like a query tab, then describe the
    /// table and load its rows.
    fn connect(&mut self, secrets: Option<Arc<dyn SecretStore>>, cx: &mut Context<Self>) {
        let config = self.target.config.clone();
        let typed = self.target.password.clone();
        let id = crate::connections::keyring_id(&self.target.id).to_owned();
        let wants_password = config.kind().is_server();
        let table = self.table_ref.clone();

        cx.spawn(async move |this, cx| {
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
            let _ = this.update(cx, |this, cx| {
                this.link = Link::Ready(session);
                match detail {
                    Ok(detail) => {
                        this.describe(&detail, cx);
                        this.reload(cx);
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
        let reason = sonde_core::dialect(self.target.config.kind()).read_only_reason();
        self.read_only = if let Some(reason) = reason {
            Some(reason.into())
        } else if read_only_database {
            Some("This database is open read-only, so its rows cannot be changed here.".into())
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
        self.columns = columns;
        let editable = self.read_only.is_none();
        self.table.update(cx, |table, cx| {
            let grid = table.delegate_mut();
            grid.columns = grid_columns;
            grid.numeric = numeric;
            grid.key_positions = key_positions;
            grid.editable = editable;
            table.refresh(cx);
        });
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
        self.filtered = self.filter.read(cx).value().to_string();
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

    /// Put `text` in the filter box.
    pub fn set_filter(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
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
        self.table
            .update(cx, |table, cx| table.set_selected_cell(row, column, cx));
        self.focus_grid(window, cx);
    }

    /// Sort as a header click would, or with `Default` drop the sort.
    pub fn sort_by(&mut self, column: usize, sort: ColumnSort, cx: &mut Context<Self>) {
        self.sort = match sort {
            ColumnSort::Default => None,
            sort => Some((column, sort)),
        };
        self.reload(cx);
    }

    fn selected_cell(&self, cx: &App) -> Option<(Place, usize)> {
        let (row, column) = self.table.read(cx).selected_cell()?;
        Some((self.place(row, cx)?, column))
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
        self.table.update(cx, |table, cx| {
            table.set_selected_cell(row, column, cx);
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
        if let TableEvent::DoubleClickedCell(row, column) = *event
            && let Some(place) = self.place(row, cx)
        {
            self.open_editor(place, column, window, cx);
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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

        let toolbar = h_flex()
            .flex_shrink_0()
            .flex_wrap()
            .gap_3()
            .pb_5()
            .items_center()
            .child(
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
                    .child(
                        Button::new("reload-rows")
                            .outline()
                            .h_10()
                            .flex_shrink_0()
                            .label("Reload")
                            .tooltip("Run the filter and load the rows again (Enter in the filter)")
                            .disabled(!ready || applying)
                            .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                    ),
            )
            .when(editable, |this| {
                let row_actions = h_flex()
                    .flex_wrap()
                    .min_w_0()
                    .gap_2()
                    .child(
                        Button::new("add-row")
                            .ghost()
                            .h_10()
                            .icon(IconName::Plus)
                            .label("Add row")
                            .tooltip("Stage a new row below the loaded ones")
                            .disabled(applying)
                            .on_click(cx.listener(|this, _, window, cx| this.add_row(window, cx))),
                    )
                    .child(
                        Button::new("delete-row")
                            .ghost()
                            .h_10()
                            .label("Delete row")
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

        let body = if ready {
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
            .when_some(banner, |this, banner| this.child(banner))
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
            .child(status_bar);

        v_flex().size_full().child(toolbar).child(card)
    }
}

/// The grid's rows and how to draw them, staged changes included.
struct Grid {
    view: WeakEntity<TableView>,
    columns: Vec<Column>,
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
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len() + self.changes.inserts()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
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
        let numeric = self.numeric.get(col_ix).copied().unwrap_or(false);
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
            .child(div().truncate().child(self.columns[col_ix].name.clone()))
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let numeric = self.numeric.get(col_ix).copied().unwrap_or(false);
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
                    div()
                        .text_color(ink)
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
