//! One statement's result: a grid fed by the statement's page stream, pulled
//! as the user scrolls.

use std::{
    ops::RangeInclusive,
    path::PathBuf,
    time::{Duration, Instant},
};

use futures::{
    StreamExt as _,
    channel::{mpsc, oneshot},
};
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Editor, EditorState},
    table::{Column, DataTable, TableDelegate, TableEvent, TableState},
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use sonde_core::{ColumnMeta, Error, QueryHandle, QuerySummary, ResultPage, Value, export::Format};

use super::{Copy, SelectAll, cell};
use crate::design::{GRID_CELL_INSET, GRID_HEADER_HEIGHT, ROW_HEIGHT};

/// Key context of the grid, for its copy and select-all bindings.
pub const GRID_CONTEXT: &str = "ResultGrid";

/// How the first batch of a result ended, reported to whoever ran it.
pub enum Settled {
    /// Every row is loaded.
    Finished,
    /// The first pages are loaded and more are waiting.
    Paused,
    Failed(Error),
}

enum State {
    Streaming,
    /// The stream is parked with rows still to come.
    Paused,
    Finished(QuerySummary),
    /// Cut short so the next statement of the script could run.
    Truncated,
    /// Stopped by the user; the rows that arrived are kept.
    Cancelled,
    /// Stopped at the row cap of `settings.toml`.
    Capped,
    Failed(SharedString),
}

struct Stream {
    pages: mpsc::Receiver<sonde_core::Result<ResultPage>>,
    summary: oneshot::Receiver<QuerySummary>,
}

/// A rectangle of cells, both corners included.
#[derive(Clone, PartialEq, Eq)]
struct CellRange {
    rows: RangeInclusive<usize>,
    columns: RangeInclusive<usize>,
}

impl CellRange {
    fn between(a: (usize, usize), b: (usize, usize)) -> Self {
        Self {
            rows: a.0.min(b.0)..=a.0.max(b.0),
            columns: a.1.min(b.1)..=a.1.max(b.1),
        }
    }

    fn contains(&self, row: usize, column: usize) -> bool {
        self.rows.contains(&row) && self.columns.contains(&column)
    }

    fn is_single(&self) -> bool {
        self.rows.start() == self.rows.end() && self.columns.start() == self.columns.end()
    }
}

pub struct ResultView {
    title: SharedString,
    columns: Vec<ColumnMeta>,
    table: Entity<TableState<Grid>>,
    state: State,
    /// The parked stream. `None` while a pull owns it and once it has ended.
    stream: Option<Stream>,
    /// The running pull. Dropping it drops the stream, which ends the
    /// statement.
    pull: Option<Task<()>>,
    settled: Option<oneshot::Sender<Settled>>,
    started: Instant,
    elapsed: Option<Duration>,
    /// Where a shift-click range starts from.
    anchor: Option<(usize, usize)>,
    /// Pages pulled before the grid waits for the user to scroll.
    eager_pages: usize,
    /// Most rows this result keeps.
    row_cap: usize,
    /// An export waiting for the rest of the rows.
    pending_export: Option<(Format, PathBuf)>,
    /// What the last export did.
    export_note: Option<SharedString>,
    _export: Option<Task<()>>,
    focus_handle: FocusHandle,
    _table_events: Subscription,
}

impl ResultView {
    /// Show `handle`'s rows, starting to load them at once. The receiver
    /// reports how the first batch ended.
    pub fn new(
        title: SharedString,
        handle: QueryHandle,
        started: Instant,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Self, oneshot::Receiver<Settled>) {
        let numeric: Vec<bool> = handle
            .columns
            .iter()
            .map(|column| cell::is_numeric(&column.type_name))
            .collect();
        let grid = Grid {
            view: cx.weak_entity(),
            columns: handle
                .columns
                .iter()
                .zip(&numeric)
                .enumerate()
                .map(|(ix, (meta, &numeric))| {
                    // Cells draw their own padding so a selected range can
                    // fill the whole cell.
                    let column = Column::new(format!("c{ix}"), meta.name.to_string())
                        .width(px(cell::column_width(&meta.name, &meta.type_name)))
                        .p_0();
                    if numeric { column.text_right() } else { column }
                })
                .collect(),
            numeric,
            rows: Vec::new(),
            has_more: false,
            range: None,
        };
        let settings = crate::settings::Settings::get(cx);
        let (eager_pages, row_cap) = (settings.eager_pages(), settings.row_cap);
        let table = cx.new(|cx| {
            TableState::new(grid, window, cx)
                .cell_selectable(true)
                // The row header only selects whole rows, which the grid has
                // no use for; a range covers it.
                .row_header(false)
                .col_movable(false)
                .sortable(false)
        });
        let _table_events = cx.subscribe_in(&table, window, Self::on_table_event);

        let (settled, settled_rx) = oneshot::channel();
        let mut this = Self {
            title,
            columns: handle.columns,
            table,
            state: State::Streaming,
            stream: Some(Stream {
                pages: handle.pages,
                summary: handle.summary,
            }),
            pull: None,
            settled: Some(settled),
            started,
            elapsed: None,
            anchor: None,
            eager_pages,
            row_cap,
            pending_export: None,
            export_note: None,
            _export: None,
            focus_handle: cx.focus_handle(),
            _table_events,
        };
        this.pull(eager_pages, cx);
        (this, settled_rx)
    }

    pub fn title(&self) -> SharedString {
        self.title.clone()
    }

    /// Rows loaded so far.
    pub fn rows_loaded(&self, cx: &App) -> usize {
        self.table.read(cx).delegate().rows.len()
    }

    /// Whether rows are waiting that scrolling or "Load all rows" would fetch.
    pub fn has_more(&self) -> bool {
        matches!(self.state, State::Paused)
    }

    /// Whether rows are arriving right now.
    pub fn is_streaming(&self) -> bool {
        matches!(self.state, State::Streaming)
    }

    /// The loaded cell at a position, for tests and the detail view.
    pub fn value(&self, row: usize, column: usize, cx: &App) -> Option<Value> {
        self.table
            .read(cx)
            .delegate()
            .rows
            .get(row)
            .and_then(|cells| cells.get(column))
            .cloned()
    }

    /// One line describing where the result stands.
    pub fn status(&self, cx: &App) -> String {
        let rows = self.rows_loaded(cx);
        let rows_text = |count: u64| format!("{count} {}", if count == 1 { "row" } else { "rows" });
        let loaded = rows_text(rows as u64);
        let seconds = cell::elapsed;
        match &self.state {
            State::Streaming => format!("{loaded} so far"),
            State::Paused => format!("{loaded} loaded, more available"),
            State::Finished(summary) => match summary.rows_affected {
                Some(affected) => {
                    format!(
                        "{} affected in {}",
                        rows_text(affected),
                        seconds(summary.elapsed)
                    )
                }
                None => format!("{loaded} in {}", seconds(summary.elapsed)),
            },
            State::Truncated => {
                format!("First {loaded} only. Run this statement on its own to see the rest.")
            }
            State::Cancelled => format!("Cancelled after {loaded}"),
            State::Capped => {
                format!("First {loaded} only: the row cap in settings.toml stopped the rest")
            }
            State::Failed(message) => format!("Failed after {loaded}: {message}"),
        }
    }

    /// Rows the statement returned or changed, once it has finished.
    pub fn finished_rows(&self, cx: &App) -> Option<u64> {
        match &self.state {
            State::Finished(summary) => {
                Some(summary.rows_affected.unwrap_or(self.rows_loaded(cx) as u64))
            }
            State::Paused | State::Truncated | State::Capped => Some(self.rows_loaded(cx) as u64),
            _ => None,
        }
    }

    /// Wall time until the statement finished or paused.
    pub fn elapsed(&self) -> Duration {
        match &self.state {
            State::Finished(summary) => summary.elapsed,
            _ => self.elapsed.unwrap_or_else(|| self.started.elapsed()),
        }
    }

    /// The columns of the result.
    pub fn columns(&self) -> &[ColumnMeta] {
        &self.columns
    }

    /// The pages to pull when the grid is scrolled to the end.
    fn eager_pages(&self) -> usize {
        self.eager_pages
    }

    /// What the last export did, if one has run.
    pub fn export_note(&self) -> Option<SharedString> {
        self.export_note.clone()
    }

    /// Whether an export is waiting or writing.
    pub fn is_exporting(&self) -> bool {
        self.pending_export.is_some() || self._export.is_some()
    }

    /// Save every row of the result to `path`. Rows not loaded yet are
    /// loaded first, up to the row cap.
    pub fn export(&mut self, format: Format, path: PathBuf, cx: &mut Context<Self>) {
        self.pending_export = Some((format, path));
        self.export_note = Some("Loading the rest of the rows to export…".into());
        if self.stream.is_some() {
            self.pull(usize::MAX, cx);
        } else if self.pull.is_none() {
            self.write_pending_export(cx);
        }
        cx.notify();
    }

    fn write_pending_export(&mut self, cx: &mut Context<Self>) {
        let Some((format, path)) = self.pending_export.take() else {
            return;
        };
        let columns = self.columns.clone();
        let rows = self.table.read(cx).delegate().rows.clone();
        self.export_note = Some(format!("Saving {} rows…", rows.len()).into());
        let write = cx.background_spawn(async move {
            let file = std::fs::File::create(&path)?;
            let count = sonde_core::export::write(
                format,
                &columns,
                rows.iter().map(Vec::as_slice),
                std::io::BufWriter::new(file),
            )?;
            Ok::<_, std::io::Error>((count, path))
        });
        self._export = Some(cx.spawn(async move |this, cx| {
            let outcome = write.await;
            let _ = this.update(cx, |this, cx| {
                this._export = None;
                this.export_note = Some(
                    match outcome {
                        Ok((count, path)) => format!(
                            "Saved {count} {} to {}",
                            if count == 1 { "row" } else { "rows" },
                            path.display()
                        ),
                        Err(error) => format!("Could not save the export: {error}"),
                    }
                    .into(),
                );
                cx.notify();
            });
        }));
    }

    /// Notices the database raised, once the statement has finished.
    fn notices(&self) -> &[String] {
        match &self.state {
            State::Finished(summary) => &summary.notices,
            _ => &[],
        }
    }

    /// Pull up to `pages` more pages, then park the stream again.
    pub fn pull(&mut self, pages: usize, cx: &mut Context<Self>) {
        let Some(mut stream) = self.stream.take() else {
            return;
        };
        self.state = State::Streaming;
        self.set_has_more(false, cx);
        cx.notify();

        let row_cap = self.row_cap;
        let mut loaded = self.rows_loaded(cx);
        self.pull = Some(cx.spawn(async move |this, cx| {
            let mut pulled = 0;
            let mut failure = None;
            let end = loop {
                if loaded >= row_cap {
                    break Some(State::Capped);
                }
                if pulled == pages {
                    break None;
                }
                let page = match stream.pages.next().await {
                    Some(Ok(page)) => page,
                    Some(Err(Error::Cancelled)) => break Some(State::Cancelled),
                    Some(Err(error)) => {
                        let message = error.to_string().into();
                        failure = Some(error);
                        break Some(State::Failed(message));
                    }
                    None => {
                        // The summary follows the last page. It is missing
                        // only if the driver went away, and then there is
                        // nothing to add to the rows already shown.
                        let summary = (&mut stream.summary).await.unwrap_or_default();
                        break Some(State::Finished(summary));
                    }
                };
                pulled += 1;
                let mut rows = page.rows;
                rows.truncate(row_cap.saturating_sub(loaded));
                loaded += rows.len();
                let appended = this.update(cx, |this, cx| {
                    this.table.update(cx, |table, cx| {
                        table.delegate_mut().rows.extend(rows);
                        cx.notify();
                    });
                    cx.notify();
                });
                if appended.is_err() {
                    // The view was closed; dropping the stream ends the
                    // statement.
                    return;
                }
            };

            // Nothing useful to do if the view is already gone.
            let _ = this.update(cx, |this, cx| {
                let report = match &end {
                    None => Settled::Paused,
                    Some(State::Failed(message)) => {
                        Settled::Failed(failure.take().unwrap_or_else(|| Error::Query {
                            message: message.to_string(),
                            position: None,
                            code: None,
                            detail: None,
                            hint: None,
                        }))
                    }
                    Some(State::Cancelled) => Settled::Failed(Error::Cancelled),
                    Some(_) => Settled::Finished,
                };
                match end {
                    Some(state) => {
                        this.state = state;
                        this.elapsed = Some(this.started.elapsed());
                    }
                    None => {
                        this.state = State::Paused;
                        this.stream = Some(stream);
                        this.set_has_more(true, cx);
                    }
                }
                this.pull = None;
                if let Some(settled) = this.settled.take() {
                    // The run that started this may itself have been dropped.
                    let _ = settled.send(report);
                }
                if this.pending_export.is_some() {
                    if this.stream.is_some() {
                        this.pull(usize::MAX, cx);
                    } else {
                        this.write_pending_export(cx);
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Give up the rest of the rows so the session is free for the next
    /// statement of a script.
    pub fn truncate(&mut self, cx: &mut Context<Self>) {
        self.release(State::Truncated, cx);
    }

    /// Stop loading at the user's request, keeping what has arrived.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if self.stream.is_some() || self.pull.is_some() {
            self.release(State::Cancelled, cx);
        }
    }

    fn release(&mut self, state: State, cx: &mut Context<Self>) {
        self.pull = None;
        self.stream = None;
        self.state = state;
        self.elapsed = Some(self.started.elapsed());
        self.set_has_more(false, cx);
        cx.notify();
    }

    fn set_has_more(&mut self, has_more: bool, cx: &mut Context<Self>) {
        self.table.update(cx, |table, _| {
            table.delegate_mut().has_more = has_more;
        });
    }

    fn on_table_event(
        &mut self,
        _: &Entity<TableState<Grid>>,
        event: &TableEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match *event {
            TableEvent::SelectCell(row, column) => {
                // Shift extends from where the selection started; anything
                // else starts a new one.
                let anchor = match self.anchor {
                    Some(anchor) if window.modifiers().shift => anchor,
                    _ => (row, column),
                };
                self.anchor = Some(anchor);
                self.set_range(Some(CellRange::between(anchor, (row, column))), cx);
            }
            TableEvent::DoubleClickedCell(row, column) => self.open_detail(row, column, window, cx),
            TableEvent::ClearSelection => {
                self.anchor = None;
                self.set_range(None, cx);
            }
            _ => {}
        }
    }

    fn set_range(&mut self, range: Option<CellRange>, cx: &mut Context<Self>) {
        self.table.update(cx, |table, cx| {
            table.delegate_mut().range = range;
            cx.notify();
        });
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        let rows = self.rows_loaded(cx);
        if rows == 0 || self.columns.is_empty() {
            return;
        }
        self.anchor = Some((0, 0));
        self.set_range(
            Some(CellRange {
                rows: 0..=rows - 1,
                columns: 0..=self.columns.len() - 1,
            }),
            cx,
        );
    }

    /// The selected cells as tab-separated text, or `None` with no selection.
    pub fn selection_tsv(&self, cx: &App) -> Option<String> {
        let grid = self.table.read(cx).delegate();
        let range = grid.range.as_ref()?;
        let rows = grid
            .rows
            .get(range.rows.clone())?
            .iter()
            .filter_map(|row| row.get(range.columns.clone()));
        Some(cell::tsv(rows))
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.selection_tsv(cx) {
            // A single cell is copied as the bare value, without the line
            // ending a one-row table would carry.
            let text = text.strip_suffix('\n').map_or(text.clone(), str::to_owned);
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// Show a cell's whole value, for text the grid has to cut short.
    fn open_detail(
        &mut self,
        row: usize,
        column: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(value) = self.value(row, column, cx).filter(|value| !value.is_null()) else {
            return;
        };
        let Some(meta) = self.columns.get(column) else {
            return;
        };
        let (text, json) = cell::detail(&value);
        let title = if meta.type_name.is_empty() {
            meta.name.to_string()
        } else {
            format!("{} ({})", meta.name, meta.type_name)
        };
        let copied = text.clone();
        let editor = cx.new(|cx| {
            let state = EditorState::new(window, cx)
                .line_number(false)
                .default_value(text);
            if json { state.language("json") } else { state }
        });
        window.open_dialog(cx, move |dialog, window, cx| {
            let copied = copied.clone();
            let preview_height = (window.viewport_size().height * 0.45).clamp(px(140.), px(360.));
            dialog
                .title(title.clone())
                .w(px(640.))
                .p_6()
                .child(
                    div()
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
                                .aria_label("Cell value"),
                        ),
                )
                .footer(
                    h_flex()
                        .flex_wrap()
                        .gap_3()
                        .justify_end()
                        .child(
                            Button::new("copy-value")
                                .outline()
                                .h_10()
                                .label("Copy value")
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        copied.clone(),
                                    ));
                                }),
                        )
                        .child(
                            Button::new("close-value")
                                .primary()
                                .h_10()
                                .label("Close")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        ),
                )
        });
    }
}

impl Focusable for ResultView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ResultView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let quiet = theme.muted_foreground;
        let notices: Vec<SharedString> = self.notices().iter().map(Into::into).collect();

        let body = if self.columns.is_empty() {
            // A statement that returns no rows has nothing to put in a grid;
            // its outcome is the status line.
            v_flex()
                .p_5()
                .gap_1()
                .text_sm()
                .child(self.status(cx))
                .children(
                    notices
                        .into_iter()
                        .map(|notice| div().text_color(quiet).child(notice)),
                )
                .into_any_element()
        } else {
            v_flex()
                .size_full()
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .font_family(theme.mono_font_family.clone())
                        .child(
                            DataTable::new(&self.table)
                                .with_size(px(ROW_HEIGHT))
                                .header_row_height(px(GRID_HEADER_HEIGHT))
                                .bordered(false)
                                .accessibility_label(format!("{} rows", self.title)),
                        ),
                )
                .when(!notices.is_empty(), |this| {
                    this.child(
                        v_flex()
                            .px_5()
                            .py_3()
                            .text_sm()
                            .text_color(quiet)
                            .border_t_1()
                            .border_color(theme.border)
                            .children(notices),
                    )
                })
                .into_any_element()
        };

        div()
            .key_context(GRID_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &Copy, _, cx| this.copy(cx)))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| this.select_all(cx)))
            .size_full()
            .child(body)
    }
}

struct Grid {
    view: WeakEntity<ResultView>,
    columns: Vec<Column>,
    numeric: Vec<bool>,
    rows: Vec<Vec<Value>>,
    has_more: bool,
    range: Option<CellRange>,
}

impl TableDelegate for Grid {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        // Columns carry no padding of their own (see `ResultView::new`), so
        // the header supplies the same inset the cells do.
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
        let value = self.rows.get(row_ix).and_then(|row| row.get(col_ix));
        let numeric = self.numeric.get(col_ix).copied().unwrap_or(false);
        // A lone selected cell already has the table's own marker.
        let in_range = self
            .range
            .as_ref()
            .is_some_and(|range| !range.is_single() && range.contains(row_ix, col_ix));

        h_flex()
            .size_full()
            .px(px(GRID_CELL_INSET))
            .when(numeric, |this| this.justify_end())
            .when(in_range, |this| this.bg(theme.selection))
            .when_some(value, |this, value| {
                this.text_color(crate::design::value_ink(value, cx))
            })
            .map(|this| match value.map(cell::display) {
                Some(Some(text)) => this.child(div().truncate().child(text)),
                Some(None) => this.child(div().italic().child("NULL")),
                None => this,
            })
    }

    fn cell_label(&self, row_ix: usize, col_ix: usize, _: &App) -> Option<SharedString> {
        let value = self.rows.get(row_ix)?.get(col_ix)?;
        Some(cell::display(value).unwrap_or_else(|| "NULL".into()))
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _: &App) -> String {
        self.rows
            .get(row_ix)
            .and_then(|row| row.get(col_ix))
            .map(ToString::to_string)
            .unwrap_or_default()
    }

    fn has_more(&self, _: &App) -> bool {
        self.has_more
    }

    fn load_more_threshold(&self) -> usize {
        200
    }

    fn load_more(&mut self, _: &mut Window, cx: &mut Context<TableState<Self>>) {
        // The table is mid-update here, and pulling updates the table, so the
        // pull has to wait until this update has unwound.
        let view = self.view.clone();
        cx.defer(move |cx| {
            // A closed view has nothing left to load.
            let _ = view.update(cx, |view, cx| {
                let pages = view.eager_pages();
                view.pull(pages, cx);
            });
        });
    }
}
