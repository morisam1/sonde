//! A query tab: a SQL editor bound to one connection, with a result per
//! statement below it.

pub(crate) mod cell;
mod complete;
mod result;
mod script;

use std::{path::PathBuf, rc::Rc, sync::Arc, time::Instant};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, RopeExt as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    highlighter::{Diagnostic, DiagnosticSeverity},
    input::{Editor, EditorState, ShowCompletions},
    menu::{DropdownMenu as _, PopupMenuItem},
    resizable::{resizable_panel, v_resizable},
    tab::{Tab, TabBar},
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use sonde_core::{
    ConnectionConfig, Error, QueryHandle, Secret, Session,
    config::SecretStore,
    export::Format,
    history::{Entry, History},
};

pub use self::complete::SchemaCache;
pub use self::result::ResultView;
use self::{
    result::{GRID_CONTEXT, Settled},
    script::Planned,
};

actions!(
    query,
    [
        /// Run the selection, or the statement the cursor is in.
        RunStatement,
        /// Run every statement in the editor.
        RunAll,
        /// Stop the running statement.
        CancelRun,
        /// Copy the selected cells as tab-separated text.
        Copy,
        /// Select every loaded cell of the result.
        SelectAll
    ]
);

const EDITOR_CONTEXT: &str = "QueryEditor";

/// Register the query tab's key bindings. Call once per application.
pub fn init(cx: &mut App) {
    // The editor binds ctrl-enter itself (to a newline). These are registered
    // later and scoped to the query editor, so they win there.
    let in_editor = Some("QueryEditor > Input");
    cx.bind_keys([
        KeyBinding::new("ctrl-enter", RunStatement, in_editor),
        KeyBinding::new("ctrl-shift-enter", RunAll, in_editor),
        KeyBinding::new("ctrl-space", ShowCompletions, in_editor),
        KeyBinding::new("ctrl-enter", RunStatement, Some(EDITOR_CONTEXT)),
        KeyBinding::new("ctrl-shift-enter", RunAll, Some(EDITOR_CONTEXT)),
        KeyBinding::new("ctrl-c", Copy, Some(GRID_CONTEXT)),
        KeyBinding::new("ctrl-a", SelectAll, Some(GRID_CONTEXT)),
    ]);
}

/// Where query history is kept. Without one, nothing is recorded.
pub struct HistoryStore(pub Option<History>);

impl Global for HistoryStore {}

fn history(cx: &App) -> Option<History> {
    cx.try_global::<HistoryStore>()
        .and_then(|store| store.0.clone())
}

/// Whether `sql` may change what the schema cache knows.
fn changes_schema(sql: &str) -> bool {
    let first = sql
        .trim_start()
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    // SQL DDL; Redis commands that add or remove keys; REST requests that
    // add or remove indices; MongoDB calls that add or remove collections.
    matches!(
        first.as_str(),
        "create"
            | "alter"
            | "drop"
            | "rename"
            | "comment"
            | "attach"
            | "detach"
            | "use"
            | "del"
            | "unlink"
            | "set"
            | "mset"
            | "hset"
            | "lpush"
            | "rpush"
            | "sadd"
            | "zadd"
            | "xadd"
            | "flushdb"
            | "flushall"
            | "select"
            | "put"
            | "delete"
    ) || [
        ".insert",
        ".drop",
        "createCollection",
        "createIndex",
        "dropDatabase",
    ]
    .iter()
    .any(|call| sql.contains(call))
}

/// One line of SQL for a menu: whitespace collapsed, cut at `max` characters.
fn one_line(sql: &str, max: usize) -> String {
    let flat = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// What a query tab needs to open its own session.
#[derive(Clone)]
pub struct QueryTarget {
    /// The saved connection's id, which is also its keyring entry.
    pub id: String,
    /// The saved connection's name.
    pub name: String,
    pub config: ConnectionConfig,
    /// A password typed this session, for when there is no keyring to ask.
    pub password: Option<Secret>,
}

enum Link {
    Connecting,
    Ready(Arc<dyn Session>),
    Failed(SharedString),
}

/// Where a multi-statement run has got to.
struct Progress {
    statement: usize,
    of: usize,
}

/// Why the last run stopped early.
struct Problem {
    headline: SharedString,
    /// The database's detail and hint lines, if it gave any.
    notes: Vec<SharedString>,
}

pub struct QueryPanel {
    title: SharedString,
    target: QueryTarget,
    secrets: Option<Arc<dyn SecretStore>>,
    editor: Entity<EditorState>,
    link: Link,
    results: Vec<Entity<ResultView>>,
    active: usize,
    progress: Option<Progress>,
    /// The running script. Dropping it abandons the statements not yet run.
    run: Option<Task<()>>,
    problem: Option<Problem>,
}

impl QueryPanel {
    pub fn new(
        title: SharedString,
        target: QueryTarget,
        secrets: Option<Arc<dyn SecretStore>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
                .placeholder("Write SQL here. Ctrl+Enter runs the statement under the cursor.")
        });
        let provider = Rc::new(complete::Provider {
            panel: cx.weak_entity(),
        });
        editor.update(cx, |editor, _| {
            editor.lsp_mut().completion_provider = Some(provider);
        });
        let mut this = Self {
            title,
            target,
            secrets: secrets.clone(),
            editor,
            link: Link::Connecting,
            results: Vec::new(),
            active: 0,
            progress: None,
            run: None,
            problem: None,
        };
        this.connect(secrets, cx);
        this
    }

    /// Open this tab's own session. The sidebar's session is not shared: a
    /// session runs one statement at a time, and browsing should not wait
    /// behind a long query.
    fn connect(&mut self, secrets: Option<Arc<dyn SecretStore>>, cx: &mut Context<Self>) {
        self.link = Link::Connecting;
        let config = self.target.config.clone();
        let typed = self.target.password.clone();
        let id = crate::connections::keyring_id(&self.target.id).to_owned();
        let wants_password = config.kind().is_server();

        cx.spawn(async move |this, cx| {
            let secret = match (typed, secrets) {
                (Some(typed), _) => Ok(Some(typed)),
                (None, Some(secrets)) if wants_password => {
                    cx.background_spawn(async move { secrets.get(&id) }).await
                }
                _ => Ok(None),
            };
            let outcome = match secret {
                Ok(secret) => sonde_core::connect(&config, secret).await,
                Err(error) => Err(error),
            };
            // The tab may have been closed while this ran.
            let _ = this.update(cx, |this, cx| {
                this.link = match outcome {
                    Ok(session) => Link::Ready(Arc::from(session)),
                    Err(error) => Link::Failed(error.to_string().into()),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Open a new session after the last one was lost or failed.
    pub fn reconnect(&mut self, cx: &mut Context<Self>) {
        if matches!(self.link, Link::Connecting) {
            return;
        }
        self.connect(self.secrets.clone(), cx);
        cx.notify();
    }

    /// The session, when it is open and not running anything.
    fn idle_session(&self) -> Option<Arc<dyn Session>> {
        match &self.link {
            Link::Ready(session) if !self.is_running() => Some(Arc::clone(session)),
            _ => None,
        }
    }

    /// The saved connection this tab runs on.
    pub fn connection_id(&self) -> &str {
        &self.target.id
    }

    /// Completions at the editor's cursor, as the menu would offer them.
    pub fn completions(
        &self,
        explicit: bool,
        cx: &mut App,
    ) -> Task<Vec<sonde_core::complete::Candidate>> {
        let (text, cursor) = {
            let editor = self.editor.read(cx);
            (editor.value().to_string(), editor.cursor())
        };
        complete::candidates(
            &self.target.id,
            self.target.config.kind(),
            self.idle_session(),
            text,
            cursor,
            explicit,
            cx,
        )
    }

    /// Put the cursor at byte `offset` of the editor's text.
    pub fn set_cursor(&mut self, offset: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            let position = editor.text().offset_to_position(offset);
            editor.set_cursor_position(position, window, cx);
        });
    }

    /// This connection's history, newest first.
    pub fn history(&self, cx: &App) -> Vec<Entry> {
        history(cx)
            .and_then(|history| history.load(&self.target.id).ok())
            .unwrap_or_default()
    }

    /// Save the shown result to `path`.
    pub fn export(&mut self, format: Format, path: PathBuf, cx: &mut Context<Self>) {
        if let Some(result) = self.active_result() {
            result.update(cx, |result, cx| result.export(format, path, cx));
        }
    }

    /// Ask where to save the shown result, then save it.
    fn choose_export(&mut self, format: Format, cx: &mut Context<Self>) {
        let dir = std::env::current_dir()
            .ok()
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."));
        let name = format!("result.{}", format.extension());
        let chosen = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = chosen.await {
                let _ = this.update(cx, |this, cx| this.export(format, path, cx));
            }
        })
        .detach();
    }

    /// Record a statement that has run in this connection's history.
    fn record(
        &self,
        sql: &str,
        started: Instant,
        view: Option<&Entity<ResultView>>,
        error: Option<&Error>,
        cx: &mut App,
    ) {
        if error.is_none() && changes_schema(sql) {
            SchemaCache::forget(&self.target.id, cx);
            crate::browser::structure_changed(&self.target.id, cx);
        }
        let Some(history) = history(cx) else {
            return;
        };
        let entry = Entry::now(sql.trim());
        let entry = match error {
            Some(error) => entry.failed(error.to_string()),
            None => {
                let (elapsed, rows) = view.map_or((started.elapsed(), None), |view| {
                    let view = view.read(cx);
                    (view.elapsed(), view.finished_rows(cx))
                });
                entry.finished(elapsed, rows)
            }
        };
        let entry = Entry {
            at_ms: entry
                .at_ms
                .saturating_sub(started.elapsed().as_millis() as u64),
            ..entry
        };
        let id = self.target.id.clone();
        cx.background_spawn(async move {
            if let Err(error) = history.append(&id, &entry) {
                eprintln!("sonde: {error}");
            }
        })
        .detach();
    }

    /// The editor's text.
    pub fn text(&self, cx: &App) -> String {
        self.editor.read(cx).value().to_string()
    }

    /// Replace the editor's text.
    pub fn set_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            editor.set_value(text.to_owned(), window, cx)
        });
    }

    /// Whether the tab's session is open.
    pub fn is_ready(&self) -> bool {
        matches!(self.link, Link::Ready(_))
    }

    /// Whether a run is in progress.
    pub fn is_running(&self) -> bool {
        self.progress.is_some()
    }

    /// The results of the last run, one per statement that got to run.
    pub fn results(&self) -> &[Entity<ResultView>] {
        &self.results
    }

    /// Why the last run stopped early, if it did.
    pub fn problem(&self) -> Option<SharedString> {
        self.problem
            .as_ref()
            .map(|problem| problem.headline.clone())
    }

    /// Where the editor's cursor is, as a line and column counted from 1.
    pub fn cursor(&self, cx: &App) -> (usize, usize) {
        let editor = self.editor.read(cx);
        script::line_column(&editor.value(), editor.cursor())
    }

    fn run_statement(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let kind = self.target.config.kind();
        let (text, cursor, selection) = {
            let editor = self.editor.read(cx);
            (editor.value(), editor.cursor(), editor.selected_range())
        };
        let planned = if selection.is_empty() {
            script::at_cursor(&text, cursor, kind).into_iter().collect()
        } else {
            script::within(&text, selection, kind)
        };
        self.run(planned, window, cx);
    }

    fn run_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let planned = script::all(&self.text(cx), self.target.config.kind());
        self.run(planned, window, cx);
    }

    fn run(&mut self, planned: Vec<Planned>, window: &mut Window, cx: &mut Context<Self>) {
        let Link::Ready(session) = &self.link else {
            return;
        };
        if self.is_running() {
            return;
        }
        let session = Arc::clone(session);

        // Dropping the old results releases the session if one of them was
        // still parked on unread rows.
        self.results.clear();
        self.active = 0;
        self.clear_marks(cx);
        if planned.is_empty() {
            self.problem = Some(Problem {
                headline: "There is no statement to run.".into(),
                notes: Vec::new(),
            });
            cx.notify();
            return;
        }
        self.problem = None;
        let total = planned.len();
        self.progress = Some(Progress {
            statement: 1,
            of: total,
        });
        cx.notify();

        self.run = Some(cx.spawn_in(window, async move |this, cx| {
            for (ix, statement) in planned.into_iter().enumerate() {
                let last = ix + 1 == total;
                let started = Instant::now();
                let advanced = this.update(cx, |this, cx| {
                    this.progress = Some(Progress {
                        statement: ix + 1,
                        of: total,
                    });
                    cx.notify();
                });
                if advanced.is_err() {
                    // The tab was closed.
                    return;
                }

                let mut shown = None;
                let failure = match session.execute(&statement.sql, &[]).await {
                    Err(error) => Some(error),
                    Ok(handle) => {
                        let Ok((view, settled)) = this.update_in(cx, |this, window, cx| {
                            this.add_result(ix, handle, started, window, cx)
                        }) else {
                            return;
                        };
                        shown = Some(view.clone());
                        match settled.await {
                            Ok(Settled::Failed(error)) => Some(error),
                            Ok(Settled::Paused) if !last => {
                                // The next statement needs the session.
                                view.update(cx, |view, cx| view.truncate(cx));
                                None
                            }
                            // Finished, parked on the final statement, or the
                            // view was closed: nothing stands in the way.
                            _ => None,
                        }
                    }
                };
                let _ = this.update(cx, |this, cx| {
                    if failure.as_ref() != Some(&Error::Cancelled) || shown.is_some() {
                        this.record(
                            &statement.sql,
                            started,
                            shown.as_ref(),
                            failure.as_ref(),
                            cx,
                        );
                    }
                });
                if let Some(error) = failure {
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.fail(&error, &statement, window, cx);
                    });
                    return;
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.progress = None;
                cx.notify();
            });
        }));
    }

    fn add_result(
        &mut self,
        ix: usize,
        handle: QueryHandle,
        started: Instant,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (
        Entity<ResultView>,
        futures::channel::oneshot::Receiver<Settled>,
    ) {
        let title: SharedString = format!("Result {}", ix + 1).into();
        let mut settled = None;
        let view = cx.new(|cx| {
            let (view, receiver) = ResultView::new(title, handle, started, window, cx);
            settled = Some(receiver);
            view
        });
        // Scrolling a grid loads more rows, which changes the status line.
        cx.observe(&view, |_, _, cx| cx.notify()).detach();
        self.results.push(view.clone());
        self.active = self.results.len() - 1;
        cx.notify();
        let settled = settled.unwrap_or_else(|| futures::channel::oneshot::channel().1);
        (view, settled)
    }

    /// Record why the run stopped and, when the database said where, mark the
    /// place in the editor and move the cursor to it.
    fn fail(
        &mut self,
        error: &Error,
        statement: &Planned,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.progress = None;
        if *error == Error::Cancelled {
            self.problem = Some(Problem {
                headline: "Cancelled.".into(),
                notes: Vec::new(),
            });
            cx.notify();
            return;
        }
        if error.is_connection_lost() {
            self.problem = Some(Problem {
                headline: "The connection to the database was lost. Sonde is reconnecting; \
                           run the statement again once it is back."
                    .into(),
                notes: Vec::new(),
            });
            self.connect(self.secrets.clone(), cx);
            cx.notify();
            return;
        }

        let mut notes = Vec::new();
        if let Error::Query { detail, hint, .. } = error {
            notes.extend(detail.iter().map(|detail| detail.clone().into()));
            notes.extend(hint.iter().map(|hint| format!("Hint: {hint}").into()));
        }
        let text = self.text(cx);
        // Without a position the statement's first line is still better than
        // nothing in a long script.
        let offset = statement.offset + error.position().unwrap_or(0);
        let (line, column) = script::line_column(&text, offset);
        let headline = match error.position() {
            Some(_) => format!("Line {line}, column {column}: {error}"),
            None => format!("Line {line}: {error}"),
        };

        if error.position().is_some() {
            // Underline to the end of the word the database pointed at.
            let word = text[offset.min(text.len())..]
                .find(|c: char| c.is_whitespace() || c == ';' || c == ',')
                .map_or(text.len(), |length| offset + length.max(1));
            let message = error.to_string();
            self.editor.update(cx, |editor, cx| {
                let start = editor.text().offset_to_position(offset);
                let end = editor.text().offset_to_position(word.min(text.len()));
                if let Some(marks) = editor.diagnostics_mut() {
                    marks.push(
                        Diagnostic::new(start..end, message)
                            .with_severity(DiagnosticSeverity::Error),
                    );
                }
                editor.set_cursor_position(start, window, cx);
                cx.notify();
            });
        }
        self.problem = Some(Problem {
            headline: headline.into(),
            notes,
        });
        cx.notify();
    }

    fn clear_marks(&mut self, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            if let Some(marks) = editor.diagnostics_mut() {
                marks.clear();
            }
            cx.notify();
        });
    }

    /// Stop the running statement. The run then ends with "Cancelled".
    fn cancel(&mut self, cx: &mut Context<Self>) {
        if !self.is_running() {
            return;
        }
        if let Link::Ready(session) = &self.link {
            session.canceller().cancel();
        }
        cx.notify();
    }

    fn active_result(&self) -> Option<&Entity<ResultView>> {
        self.results.get(self.active)
    }

    /// One line for the status bar.
    pub fn status(&self, cx: &App) -> String {
        match (&self.link, &self.progress, self.active_result()) {
            (Link::Connecting, ..) => format!("Connecting to {}", self.target.name),
            (Link::Failed(message), ..) => format!("Could not connect: {message}"),
            (_, Some(progress), result) => {
                let rows = result.map_or(0, |result| result.read(cx).rows_loaded(cx));
                if progress.of > 1 {
                    format!(
                        "Running statement {} of {}, {rows} rows so far",
                        progress.statement, progress.of
                    )
                } else {
                    format!("Running, {rows} rows so far")
                }
            }
            (_, None, Some(result)) => {
                let result = result.read(cx);
                result
                    .export_note()
                    .map_or_else(|| result.status(cx), |note| note.to_string())
            }
            (_, None, None) => "Ready".to_owned(),
        }
    }
}

impl QueryPanel {
    fn render_history_menu(&self, cx: &Context<Self>) -> impl IntoElement {
        let panel = cx.weak_entity();
        Button::new("query-history")
            .ghost()
            .h_10()
            .icon(IconName::Inbox)
            .label("History")
            .tooltip("Statements run on this connection")
            .dropdown_menu(move |menu, _, cx| {
                let Some(this) = panel.upgrade() else {
                    return menu;
                };
                let entries = this.read(cx).history(cx);
                let mut menu = menu.max_h(px(420.)).scrollable(true).min_w(px(360.));
                if entries.is_empty() {
                    return menu.item(
                        PopupMenuItem::new("Nothing run on this connection yet").disabled(true),
                    );
                }
                for entry in entries.into_iter().take(50) {
                    let label = format!(
                        "{}{}",
                        if entry.error.is_some() { "✕ " } else { "" },
                        one_line(&entry.sql, 70)
                    );
                    let panel = panel.clone();
                    menu = menu.item(PopupMenuItem::new(label).on_click(move |_, window, cx| {
                        let _ = panel.update(cx, |this, cx| {
                            this.set_text(&entry.sql, window, cx);
                        });
                    }));
                }
                menu
            })
    }
}

impl Focusable for QueryPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        // Opening a query tab should put the cursor in the editor.
        self.editor.read(cx).focus_handle(cx)
    }
}

impl QueryPanel {
    /// The tab's title, "Query 1" and so on.
    pub fn title(&self) -> SharedString {
        self.title.clone()
    }

    /// The saved connection's name.
    pub fn connection_name(&self) -> &str {
        &self.target.name
    }
}

impl Render for QueryPanel {
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
        let ready = self.is_ready();
        let running = self.is_running();
        let failed_link = matches!(self.link, Link::Failed(_));
        let status = self.status(cx);
        let has_more = self
            .active_result()
            .is_some_and(|result| result.read(cx).has_more());
        let can_export = !running
            && self.active_result().is_some_and(|result| {
                let result = result.read(cx);
                !result.columns().is_empty() && !result.is_exporting()
            });

        let toolbar = h_flex()
            .flex_shrink_0()
            .flex_wrap()
            .gap_3()
            .justify_between()
            .pb_5()
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        Button::new("run-statement")
                            .primary()
                            .h_10()
                            .icon(IconName::Play)
                            .label("Run")
                            .tooltip(
                                "Run the selection, or the statement under the cursor (Ctrl+Enter)",
                            )
                            .disabled(!ready || running)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.run_statement(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("run-all")
                            .outline()
                            .h_10()
                            .label("Run all")
                            .tooltip("Run every statement (Ctrl+Shift+Enter)")
                            .disabled(!ready || running)
                            .on_click(cx.listener(|this, _, window, cx| this.run_all(window, cx))),
                    )
                    .child(self.render_history_menu(cx)),
            )
            .child(
                div()
                    .min_w_0()
                    .text_sm()
                    .text_color(quiet)
                    .child("Ctrl+Enter runs the statement under the cursor"),
            );

        let editor = crate::ui::card(cx).size_full().p_3().child(
            Editor::new(&self.editor)
                .h(relative(1.))
                .appearance(false)
                .bordered(false)
                .aria_label("SQL"),
        );

        let problem = self.problem.as_ref().map(|problem| {
            v_flex()
                .id("query-problem")
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

        let tabs = (self.results.len() > 1).then(|| {
            TabBar::new("result-tabs")
                .underline()
                .small()
                .selected_index(self.active)
                .on_click(cx.listener(|this, ix: &usize, _, cx| {
                    this.active = *ix;
                    cx.notify();
                }))
                .children(
                    self.results
                        .iter()
                        .map(|result| Tab::new().label(result.read(cx).title())),
                )
        });

        let results = v_flex()
            .size_full()
            .when_some(problem, |this, problem| this.child(problem))
            .when_some(tabs, |this, tabs| {
                this.child(div().flex_shrink_0().px_2().child(tabs))
            })
            .child(div().flex_1().min_h_0().map(|this| {
                match self.active_result() {
                    Some(result) => this.child(result.clone()),
                    None if self.problem.is_some() || running => this,
                    None => this.child(div().p_5().text_sm().text_color(quiet).child(
                        if failed_link {
                            "This tab has no connection. Close it and open a new query \
                             once the connection works."
                        } else {
                            "Results appear here, one tab per statement."
                        },
                    )),
                }
            }));

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
                    .id("query-status")
                    .test_support()
                    .flex_1()
                    .truncate()
                    .text_color(if failed_link { danger } else { quiet })
                    .child(status),
            )
            .when(has_more && !running, |this| {
                this.child(
                    Button::new("load-all")
                        .ghost()
                        .small()
                        .label("Load all rows")
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(result) = this.active_result() {
                                result.update(cx, |result, cx| result.pull(usize::MAX, cx));
                            }
                        })),
                )
                .child(
                    Button::new("stop-loading")
                        .ghost()
                        .small()
                        .label("Stop")
                        .tooltip("Discard the rows not yet loaded")
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(result) = this.active_result() {
                                result.update(cx, |result, cx| result.stop(cx));
                            }
                        })),
                )
            })
            .when(failed_link, |this| {
                this.child(
                    Button::new("reconnect")
                        .outline()
                        .small()
                        .label("Try again")
                        .on_click(cx.listener(|this, _, _, cx| this.reconnect(cx))),
                )
            })
            .when(can_export, |this| {
                let panel = cx.weak_entity();
                this.child(
                    Button::new("export-result")
                        .ghost()
                        .small()
                        .label("Export")
                        .dropdown_menu(move |menu, _, _| {
                            let mut menu = menu;
                            for (label, format) in [("CSV", Format::Csv), ("JSON", Format::Json)] {
                                let panel = panel.clone();
                                menu = menu.item(PopupMenuItem::new(label).on_click(
                                    move |_, _, cx| {
                                        let _ = panel.update(cx, |this, cx| {
                                            this.choose_export(format, cx);
                                        });
                                    },
                                ));
                            }
                            menu
                        }),
                )
            })
            .when(running, |this| {
                this.child(
                    Button::new("cancel-run")
                        .outline()
                        .small()
                        .label("Cancel")
                        .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                )
            });

        v_flex()
            .key_context(EDITOR_CONTEXT)
            .on_action(cx.listener(|this, _: &RunStatement, window, cx| {
                this.run_statement(window, cx);
            }))
            .on_action(cx.listener(|this, _: &RunAll, window, cx| this.run_all(window, cx)))
            .on_action(cx.listener(|this, _: &CancelRun, _, cx| this.cancel(cx)))
            .size_full()
            .child(toolbar)
            .child(
                div().flex_1().min_h_0().child(
                    v_resizable("query-split")
                        .child(
                            resizable_panel()
                                .size(px(220.))
                                .child(div().size_full().pb_2().child(editor)),
                        )
                        .child(
                            resizable_panel().child(
                                div().size_full().pt_2().child(
                                    crate::ui::card(cx)
                                        .size_full()
                                        .child(div().flex_1().min_h_0().child(results))
                                        .child(status_bar),
                                ),
                            ),
                        ),
                ),
            )
    }
}
