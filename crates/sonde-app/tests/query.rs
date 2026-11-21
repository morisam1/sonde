//! A click-through of the query tab in a headless window with the real
//! renderer, against the Postgres fixture and a SQLite file.
//!
//! Each step is checked and reported. Set `SONDE_SHOTS` to a directory to
//! also get a PNG of the window after each stage.
//!
//! ```sh
//! docker compose -f tests/docker-compose.yml up -d
//! SONDE_SHOTS=/tmp/shots cargo test -p sonde-app --test query
//! ```

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Entity, Focusable as _, HeadlessAppContext,
    InputEvent as _, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Window, WindowBounds,
    WindowOptions,
    component::{Theme, ThemeMode},
    px, size,
    test::{TestEventExt as _, TestQueryExt as _, TestWindowExt as _},
};
use sonde_app::{
    connections::Connections,
    layout::LayoutFile,
    query::{QueryPanel, ResultView},
    workspace::Workspace,
};
use sonde_core::{
    ConnectionConfig, PostgresConfig, Result, Secret, SqliteConfig, TlsMode, Value,
    config::{ConnectionStore, SavedConnection, SecretStore},
};

const PG_PORT: u16 = 54329;
const BIG_ROWS: usize = 500_000;

struct MemorySecrets(Mutex<HashMap<String, String>>);

impl SecretStore for MemorySecrets {
    fn get(&self, id: &str) -> Result<Option<Secret>> {
        Ok(self.0.lock().unwrap().get(id).map(Secret::new))
    }

    fn set(&self, id: &str, secret: &Secret) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .insert(id.to_owned(), secret.expose().to_owned());
        Ok(())
    }

    fn delete(&self, id: &str) -> Result<()> {
        self.0.lock().unwrap().remove(id);
        Ok(())
    }
}

#[derive(Default)]
struct Report {
    failures: usize,
}

impl Report {
    fn check(&mut self, what: &str, passed: bool) {
        println!("{} {what}", if passed { "PASS" } else { "FAIL" });
        if !passed {
            self.failures += 1;
        }
    }
}

struct Ui {
    cx: HeadlessAppContext,
    window: AnyWindowHandle,
    workspace: Entity<Workspace>,
    shots: Option<PathBuf>,
}

impl Ui {
    fn launch(dir: &Path, secrets: Arc<dyn SecretStore>, window_size: (f32, f32)) -> Self {
        let mut cx = HeadlessAppContext::with_platform(
            gpui_kit::platform::current_platform(true).text_system(),
            Arc::new(sonde_app::AppAssets),
            gpui_kit::platform::current_headless_renderer,
        );
        // Query results are woken from the driver runtime's threads.
        cx.allow_parking();
        cx.update(gpui_kit::init);

        let store = ConnectionStore::at(dir.join("connections.toml"));
        let layout = LayoutFile::at(dir.join("layout.toml"));
        let (window, workspace) = cx
            .update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: Default::default(),
                            size: size(px(window_size.0), px(window_size.1)),
                        })),
                        focus: true,
                        show: false,
                        ..Default::default()
                    },
                    cx,
                    |window, cx| {
                        let connections = cx.new(|_| Connections::load(store, Some(secrets)));
                        cx.new(|cx| Workspace::new(connections, layout, window, cx))
                    },
                )
            })
            .expect("the window opens");

        let mut ui = Self {
            cx,
            window,
            workspace,
            shots: std::env::var_os("SONDE_SHOTS").map(PathBuf::from),
        };
        ui.frame();
        ui
    }

    fn with<R>(&mut self, f: impl FnOnce(&mut Window, &mut App) -> R) -> R {
        self.cx
            .update_window(self.window, |_, window, cx| f(window, cx))
            .expect("the window is open")
    }

    fn frame(&mut self) {
        self.cx.advance_clock(Duration::from_millis(16));
        self.cx.run_until_parked();
        self.with(|window, cx| window.render_frame(cx));
    }

    /// Keep the app running until `done`, or give up after a while.
    fn settle(&mut self, what: &str, mut done: impl FnMut(&mut Window, &mut App) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            self.frame();
            if self.with(&mut done) {
                return true;
            }
            if Instant::now() > deadline {
                println!("     timed out waiting for: {what}");
                return false;
            }
            // Transitions run on the headless clock, which only moves when
            // told to.
            self.cx.advance_clock(Duration::from_millis(10));
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn click(&mut self, id: &'static str) {
        self.with(|window, cx| window.click(id, cx));
        self.frame();
    }

    /// Right-click whatever carries this accessible name, opening its
    /// context menu.
    fn right_click_label(&mut self, label: &str) {
        self.with(|window, cx| {
            let at = window.find_by_label(label).bounds().center();
            window.pointer_down(at, MouseButton::Right, cx);
            window.pointer_up(at, MouseButton::Right, cx);
        });
        self.frame();
    }

    fn click_label(&mut self, label: &str) {
        self.click_label_times(label, 1);
    }

    /// Click whatever carries this accessible name, `times` in quick
    /// succession.
    fn click_label_times(&mut self, label: &str, times: usize) {
        self.with(|window, cx| {
            let position = window.find_by_label(label).bounds().center();
            for click_count in 1..=times {
                let modifiers = window.modifiers();
                window.dispatch_event(
                    MouseDownEvent {
                        button: MouseButton::Left,
                        position,
                        modifiers,
                        click_count,
                        first_mouse: false,
                    }
                    .to_platform_input(),
                    cx,
                );
                window.dispatch_event(
                    MouseUpEvent {
                        button: MouseButton::Left,
                        position,
                        modifiers,
                        click_count,
                    }
                    .to_platform_input(),
                    cx,
                );
            }
        });
        self.frame();
    }

    fn press(&mut self, key: &str) {
        self.with(|window, cx| window.press(key, cx));
        self.frame();
    }

    fn has(&mut self, id: &'static str) -> bool {
        self.with(|window, _| window.try_find(id).is_some())
    }

    fn queries(&mut self) -> Vec<Entity<QueryPanel>> {
        let workspace = self.workspace.clone();
        self.cx.update(|cx| workspace.read(cx).queries())
    }

    /// Open a query tab from a connection's right-click menu and wait for
    /// its session.
    fn new_query(&mut self, connection: &str) -> Query {
        self.right_click_label(connection);
        self.click_label("New query");
        let panel = self.queries().pop().expect("a query tab opened");
        let ready = panel.clone();
        self.settle("the query tab's session", move |_, cx| {
            ready.read(cx).is_ready()
        });
        Query(panel)
    }

    /// Put text in the editor and the keyboard focus with it.
    fn write(&mut self, query: &Query, sql: &str) {
        let panel = query.0.clone();
        self.with(|window, cx| {
            panel.update(cx, |panel, cx| panel.set_text(sql, window, cx));
            panel.focus_handle(cx).focus(window, cx);
        });
        self.frame();
    }

    /// Run with a real click on a toolbar button and wait for the run to end.
    fn run(&mut self, query: &Query, button: &'static str) {
        self.click(button);
        self.finish(query);
    }

    fn finish(&mut self, query: &Query) {
        let panel = query.0.clone();
        self.settle("the run to finish", move |_, cx| {
            let panel = panel.read(cx);
            !panel.is_running()
                && panel
                    .results()
                    .iter()
                    .all(|result| !result.read(cx).is_streaming())
        });
    }

    fn read<R>(&mut self, query: &Query, f: impl FnOnce(&QueryPanel, &App) -> R) -> R {
        let panel = query.0.clone();
        self.cx.update(|cx| f(panel.read(cx), cx))
    }

    fn result<R>(
        &mut self,
        query: &Query,
        ix: usize,
        f: impl FnOnce(&ResultView, &App) -> R,
    ) -> Option<R> {
        self.read(query, |panel, cx| {
            panel.results().get(ix).map(|result| f(result.read(cx), cx))
        })
    }

    fn rows(&mut self, query: &Query, ix: usize) -> usize {
        self.result(query, ix, ResultView::rows_loaded).unwrap_or(0)
    }

    fn clipboard(&mut self) -> Option<String> {
        self.cx
            .update(|cx| cx.read_from_clipboard().and_then(|item| item.text()))
    }

    fn shot(&mut self, name: &str) {
        self.frame();
        if self.shots.is_none() {
            return;
        }
        // Headless time only advances with frames; run enough of them for
        // a dialog's entrance to finish.
        for _ in 0..20 {
            self.cx.advance_clock(Duration::from_millis(30));
            thread::sleep(Duration::from_millis(15));
            self.frame();
        }
        let Some(dir) = &self.shots else { return };
        std::fs::create_dir_all(dir).expect("the screenshot directory can be created");
        self.cx
            .capture_screenshot(self.window)
            .expect("offscreen rendering is available")
            .save(dir.join(format!("{name}.png")))
            .expect("the screenshot can be written");
    }
}

struct Query(Entity<QueryPanel>);

fn main() -> ExitCode {
    let dir = std::env::temp_dir().join(format!("sonde-query-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory can be created");
    let sqlite_path = dir.join("notes.db");
    rusqlite::Connection::open(&sqlite_path)
        .and_then(|c| {
            c.execute_batch(
                "create table note (id integer primary key, body text, score real);
                 insert into note (body, score) values ('first', 1.5), (null, 2), ('third', null);",
            )
        })
        .expect("a SQLite file can be created");

    ConnectionStore::at(dir.join("connections.toml"))
        .save(&[
            SavedConnection {
                id: "pg".into(),
                name: "Local Postgres".into(),
                config: ConnectionConfig::Postgres(PostgresConfig {
                    host: "127.0.0.1".into(),
                    port: PG_PORT,
                    database: "sonde".into(),
                    user: "sonde".into(),
                    tls: TlsMode::Prefer,
                    options: Default::default(),
                }),
            },
            SavedConnection {
                id: "notes".into(),
                name: "Notes".into(),
                config: ConnectionConfig::Sqlite(SqliteConfig {
                    path: sqlite_path,
                    read_only: false,
                }),
            },
        ])
        .expect("the scratch store is writable");
    let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecrets(Mutex::new(HashMap::from([(
        "pg".to_owned(),
        "sonde".to_owned(),
    )]))));

    let mut report = Report::default();
    let mut ui = Ui::launch(&dir, Arc::clone(&secrets), (1280., 820.));

    // ── Opening a query tab ──────────────────────────────────────────────
    ui.click_label("Local Postgres");
    report.check(
        "New query: no tab opens before the connection is open",
        ui.queries().is_empty(),
    );
    ui.settle("the connection", |window, _| {
        window.try_find("Disconnect").is_some()
    });
    let query = ui.new_query("Local Postgres");
    report.check(
        "New query: the connection's right-click menu opens a tab with its own session",
        ui.read(&query, |panel, _| panel.is_ready()) && ui.has("run-statement"),
    );
    ui.shot("q01-empty-tab");

    // ── Typing and running with the keyboard ─────────────────────────────
    ui.with(|window, cx| window.input("select 40 + 2 as answer", cx));
    report.check(
        "editor: a new tab takes the keyboard, so typing lands in it",
        ui.read(&query, |panel, cx| panel.text(cx)) == "select 40 + 2 as answer",
    );
    ui.press("ctrl-enter");
    ui.finish(&query);
    report.check(
        "run: Ctrl+Enter runs the statement and shows its row",
        ui.result(&query, 0, |result, cx| result.value(0, 0, cx)) == Some(Some(Value::Int(42))),
    );
    ui.shot("q02-one-row");

    // ── A large result: first pages, then everything ─────────────────────
    ui.write(
        &query,
        &format!(
            "select g as id,\n       md5(g::text) as hash,\n       (g % 7)::numeric / 3 as ratio,\n       \
             case when g % 5 = 0 then null else 'note ' || g end as note,\n       \
             now() - make_interval(secs => g) as at\nfrom generate_series(1, {BIG_ROWS}) g"
        ),
    );
    let started = Instant::now();
    ui.run(&query, "run-statement");
    let first_batch = started.elapsed();
    report.check(
        "large result: the first 10,000 rows load and the rest wait",
        ui.rows(&query, 0) == 10_000
            && ui.result(&query, 0, |result, _| result.has_more()) == Some(true),
    );
    println!("     first 10,000 of {BIG_ROWS} rows on screen after {first_batch:.2?}");
    ui.shot("q03-first-pages");

    let started = Instant::now();
    ui.click("load-all");
    ui.finish(&query);
    let everything = started.elapsed();
    report.check(
        "large result: Load all rows brings in every row",
        ui.rows(&query, 0) == BIG_ROWS,
    );
    println!("     remaining rows loaded in {everything:.2?}");
    let started = Instant::now();
    for _ in 0..60 {
        ui.with(|window, cx| window.render_frame(cx));
    }
    let per_frame = started.elapsed() / 60;
    report.check(
        "large result: a frame with 500,000 rows loaded still renders within 16 ms",
        per_frame < Duration::from_millis(16),
    );
    println!("     {per_frame:.2?} per frame");
    ui.shot("q04-all-rows");

    // ── Cancel ───────────────────────────────────────────────────────────
    ui.write(&query, "select pg_sleep(30)");
    ui.click("run-statement");
    let panel = query.0.clone();
    ui.settle("the statement to start", move |_, cx| {
        panel.read(cx).is_running()
    });
    report.check(
        "cancel: a running statement offers Cancel",
        ui.has("cancel-run"),
    );
    ui.shot("q05-running");
    // Give the statement time to be genuinely under way on the server.
    thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    ui.click("cancel-run");
    ui.finish(&query);
    let cancelled_in = started.elapsed();
    report.check(
        "cancel: control returns in under a second",
        cancelled_in < Duration::from_secs(1)
            && ui.read(&query, |panel, _| panel.problem()).as_deref() == Some("Cancelled."),
    );
    println!("     cancelled in {cancelled_in:.2?}");
    ui.write(&query, "select 7");
    ui.run(&query, "run-statement");
    report.check(
        "cancel: the tab runs the next statement normally",
        ui.result(&query, 0, |result, cx| result.value(0, 0, cx)) == Some(Some(Value::Int(7))),
    );

    // ── A failure in the middle of a script ──────────────────────────────
    ui.write(
        &query,
        "select 1;\n\nselect * from sonde_no_such_table;\nselect 3;",
    );
    ui.run(&query, "run-all");
    let problem = ui
        .read(&query, |panel, _| panel.problem())
        .unwrap_or_default();
    report.check(
        "script error: the run stops at the failing statement",
        ui.read(&query, |panel, _| panel.results().len()) == 1,
    );
    report.check(
        "script error: the message names the right line and column",
        problem.starts_with("Line 3, column 15:") && problem.contains("sonde_no_such_table"),
    );
    report.check(
        "script error: the cursor moves to the failing position",
        ui.read(&query, |panel, cx| panel.cursor(cx)) == (3, 15),
    );
    ui.shot("q06-script-error");

    // ── One result per statement ─────────────────────────────────────────
    ui.write(
        &query,
        "create temp table walk (n int, label text);\n\
         insert into walk select g, 'row ' || g from generate_series(1, 5) g;\n\
         select * from walk order by n;",
    );
    ui.press("ctrl-shift-enter");
    ui.finish(&query);
    report.check(
        "script: Ctrl+Shift+Enter runs every statement, one result each",
        ui.read(&query, |panel, _| panel.results().len()) == 3 && ui.rows(&query, 2) == 5,
    );
    report.check(
        "script: a statement that changes rows reports how many",
        ui.result(&query, 1, |result, cx| result.status(cx))
            .is_some_and(|status| status.starts_with("5 rows affected")),
    );
    ui.shot("q07-script-results");
    ui.click_label("Result 2");
    report.check(
        "script: clicking a result tab shows that statement's outcome",
        ui.read(&query, |panel, cx| panel.status(cx))
            .starts_with("5 rows affected"),
    );
    ui.shot("q08-rows-affected");
    ui.click_label("Result 3");

    // ── Selecting and copying cells ──────────────────────────────────────
    ui.click_label("row 2");
    ui.press("ctrl-c");
    report.check(
        "grid: Ctrl+C copies the selected cell",
        ui.clipboard().as_deref() == Some("row 2"),
    );
    ui.with(|window, cx| {
        window.change_modifiers(
            Modifiers {
                shift: true,
                ..Modifiers::default()
            },
            cx,
        );
    });
    ui.click_label("4");
    ui.with(|window, cx| window.change_modifiers(Modifiers::default(), cx));
    ui.press("ctrl-c");
    report.check(
        "grid: Shift+click extends to a range, copied as tab-separated rows",
        ui.clipboard().as_deref() == Some("2\trow 2\n3\trow 3\n4\trow 4"),
    );
    ui.shot("q09-range-selected");
    ui.press("ctrl-a");
    ui.press("ctrl-c");
    report.check(
        "grid: Ctrl+A selects every loaded cell",
        ui.clipboard().is_some_and(|text| text.lines().count() == 5),
    );

    // ── Earlier statements give way to later ones ────────────────────────
    ui.write(
        &query,
        "select generate_series(1, 50000);\nselect 'after' as marker;",
    );
    ui.run(&query, "run-all");
    report.check(
        "script: a large earlier result is cut at 10,000 rows so the script can go on",
        ui.rows(&query, 0) == 10_000
            && ui
                .result(&query, 0, |result, cx| result.status(cx))
                .is_some_and(|status| status.starts_with("First 10000 rows only"))
            && ui.result(&query, 1, |result, cx| result.value(0, 0, cx))
                == Some(Some(Value::text("after"))),
    );

    // ── Statement under the cursor, and the selection ────────────────────
    ui.write(&query, "select 'one';\nselect 'two';");
    ui.press("ctrl-end");
    ui.press("ctrl-enter");
    ui.finish(&query);
    report.check(
        "run: Ctrl+Enter runs only the statement the cursor is in",
        ui.read(&query, |panel, _| panel.results().len()) == 1
            && ui.result(&query, 0, |result, cx| result.value(0, 0, cx))
                == Some(Some(Value::text("two"))),
    );
    ui.press("ctrl-a");
    ui.press("ctrl-enter");
    ui.finish(&query);
    report.check(
        "run: with text selected, Ctrl+Enter runs the selection",
        ui.read(&query, |panel, _| panel.results().len()) == 2,
    );

    // ── Typed cells, NULL, and the detail view ───────────────────────────
    ui.write(
        &query,
        "select 1234.50::numeric as amount, null::text as nothing, true as flag,\n       \
         '{\"name\": \"sonde\", \"tags\": [\"a\", \"b\"], \"depth\": {\"m\": 4200}}'::jsonb as doc,\n       \
         repeat('long text ', 60) as essay, '\\xdeadbeef'::bytea as bytes",
    );
    ui.run(&query, "run-statement");
    report.check(
        "grid: values keep their types",
        ui.result(&query, 0, |result, cx| result.value(0, 2, cx)) == Some(Some(Value::Bool(true)))
            && ui.result(&query, 0, |result, cx| result.value(0, 1, cx)) == Some(Some(Value::Null)),
    );
    ui.shot("q10-typed-cells");
    ui.click_label_times(
        "{\"name\": \"sonde\", \"tags\": [\"a\", \"b\"], \"depth\": {\"m\": 4200}}",
        2,
    );
    report.check(
        "grid: double-clicking a cell opens its whole value",
        ui.has("copy-value"),
    );
    ui.shot("q11-cell-detail");
    ui.click("copy-value");
    report.check(
        "detail: Copy value copies the indented JSON",
        ui.clipboard()
            .is_some_and(|text| text.starts_with("{\n  \"name\": \"sonde\"")),
    );
    ui.press("escape");
    ui.settle("the detail to close", |window, _| {
        window.try_find("copy-value").is_none()
    });
    report.check("detail: Escape closes it", !ui.has("copy-value"));

    // ── Nothing to run ───────────────────────────────────────────────────
    ui.write(&query, "  -- only a comment\n");
    ui.click("run-statement");
    ui.frame();
    report.check(
        "run: an empty editor says there is nothing to run",
        ui.read(&query, |panel, _| panel.problem()).as_deref()
            == Some("There is no statement to run."),
    );

    // ── Dark theme ───────────────────────────────────────────────────────
    ui.write(
        &query,
        "select g as id, md5(g::text) as hash, case when g % 3 = 0 then null else g * 1.5 end as score\n\
         from generate_series(1, 200) g;\nselect * from nowhere_at_all;",
    );
    ui.run(&query, "run-all");
    ui.with(|window, cx| Theme::change(ThemeMode::Dark, Some(window), cx));
    ui.shot("q12-dark-error");
    ui.with(|window, cx| Theme::change(ThemeMode::Light, Some(window), cx));

    // ── SQLite ───────────────────────────────────────────────────────────
    ui.click_label("Notes");
    ui.settle("the SQLite connection", |window, _| {
        window.try_find("Disconnect").is_some()
    });
    let notes = ui.new_query("Notes");
    ui.write(&notes, "select * from note order by id");
    ui.run(&notes, "run-statement");
    report.check(
        "SQLite: a query tab runs against the file",
        ui.rows(&notes, 0) == 3
            && ui.result(&notes, 0, |result, cx| result.value(1, 1, cx)) == Some(Some(Value::Null)),
    );
    ui.write(&notes, "select * from note where id > 100");
    ui.run(&notes, "run-statement");
    report.check(
        "SQLite: an empty result still shows its columns",
        ui.rows(&notes, 0) == 0
            && ui.with(|window, _| {
                window.try_find_by_label("body").is_some()
                    || !window.find_all_by_label("Result 1 rows").is_empty()
            }),
    );
    ui.shot("q13-sqlite-empty");
    ui.write(&notes, "select body frm note");
    ui.run(&notes, "run-statement");
    report.check(
        "SQLite: a syntax error is placed in the editor",
        ui.read(&notes, |panel, _| panel.problem())
            .is_some_and(|problem| problem.starts_with("Line 1, column 17:")),
    );
    report.check(
        "tabs: both query tabs stay open with their own sessions",
        ui.queries().len() == 2,
    );
    drop(ui);

    // ── A narrow window ──────────────────────────────────────────────────
    let mut ui = Ui::launch(&dir, secrets, (680., 520.));
    ui.click_label("Local Postgres");
    ui.settle("the connection", |window, _| {
        window.try_find("Disconnect").is_some()
    });
    let query = ui.new_query("Local Postgres");
    ui.write(
        &query,
        "select g, md5(g::text) from generate_series(1, 50) g",
    );
    ui.run(&query, "run-statement");
    ui.shot("q14-narrow");
    report.check(
        "narrow window: the toolbar and status bar stay inside the window",
        ui.with(|window, _| {
            let run = window.find("run-all").bounds();
            let status = window.find("query-status").bounds();
            run.right() <= px(680.) && status.bottom() <= px(520.)
        }),
    );
    drop(ui);

    let _ = std::fs::remove_dir_all(&dir);
    println!(
        "\nquery walkthrough: {}",
        if report.failures == 0 {
            "all checks passed".to_owned()
        } else {
            format!("{} check(s) FAILED", report.failures)
        }
    );
    if report.failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
