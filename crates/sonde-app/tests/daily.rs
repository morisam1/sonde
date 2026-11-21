//! M5's daily-driver features in a headless window with the real renderer:
//! settings, export, history, completion, reconnecting and reopening the
//! last session's tabs. Runs against the Postgres fixture and a SQLite file.
//!
//! Each step is checked and reported. Set `SONDE_SHOTS` to a directory to
//! also get a PNG of the window after each stage.
//!
//! ```sh
//! docker compose -f tests/docker-compose.yml up -d
//! SONDE_SHOTS=/tmp/shots cargo test -p sonde-app --test daily
//! ```

#![allow(dead_code)]

use std::{
    cell::RefCell,
    collections::HashMap,
    path::{Path, PathBuf},
    process::ExitCode,
    rc::Rc,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Entity, Focusable as _, HeadlessAppContext,
    InputEvent as _, MouseButton, MouseDownEvent, MouseUpEvent, Window, WindowBounds,
    WindowOptions, px, size,
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
    complete::Candidate,
    config::{ConnectionStore, SavedConnection, SecretStore},
    export::Format,
};

const PG_PORT: u16 = 54329;

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

    fn disabled(&mut self, id: &'static str) -> bool {
        self.with(|window, _| window.find(id).disabled() == Some(true))
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

impl Ui {
    /// Ask a tab for completions at its cursor and wait for them.
    fn completions(&mut self, query: &Query, explicit: bool) -> Vec<Candidate> {
        let slot: Rc<RefCell<Option<Vec<Candidate>>>> = Rc::default();
        let panel = query.0.clone();
        let fill = Rc::clone(&slot);
        self.cx.update(|cx| {
            let task = panel.update(cx, |panel, cx| panel.completions(explicit, cx));
            cx.spawn(async move |_| *fill.borrow_mut() = Some(task.await))
                .detach();
        });
        let done = Rc::clone(&slot);
        self.settle("completions", move |_, _| done.borrow().is_some());
        slot.borrow_mut().take().unwrap_or_default()
    }

    fn labels(&mut self, query: &Query, explicit: bool) -> Vec<String> {
        self.completions(query, explicit)
            .into_iter()
            .map(|candidate| candidate.label)
            .collect()
    }

    /// Write `sql` with `|` marking where the cursor goes.
    fn write_at(&mut self, query: &Query, marked: &str) {
        let cursor = marked.find('|').expect("a cursor mark");
        self.write(query, &marked.replace('|', ""));
        let panel = query.0.clone();
        self.with(|window, cx| panel.update(cx, |panel, cx| panel.set_cursor(cursor, window, cx)));
        self.frame();
    }

    fn open_connection(&mut self, name: &str) {
        self.click_label(name);
        self.settle("the connection", |window, _| {
            window.try_find("Disconnect").is_some()
        });
    }

    fn status(&mut self, query: &Query) -> String {
        self.read(query, |panel, cx| panel.status(cx))
    }

    fn problem(&mut self, query: &Query) -> String {
        self.read(query, |panel, _| {
            panel.problem().map(|p| p.to_string()).unwrap_or_default()
        })
    }
}

fn postgres_config() -> ConnectionConfig {
    ConnectionConfig::Postgres(PostgresConfig {
        host: "127.0.0.1".into(),
        port: PG_PORT,
        database: "sonde".into(),
        user: "sonde".into(),
        tls: TlsMode::Prefer,
        options: Default::default(),
    })
}

fn main() -> ExitCode {
    let dir = std::env::temp_dir().join(format!("sonde-daily-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory can be created");
    let sqlite_path = dir.join("notes.db");
    rusqlite::Connection::open(&sqlite_path)
        .and_then(|c| {
            c.execute_batch(
                "create table note (id integer primary key, body text, score real);
                 insert into note (body, score) values ('first', 1.5), ('second', 2);",
            )
        })
        .expect("a SQLite file can be created");
    ConnectionStore::at(dir.join("connections.toml"))
        .save(&[
            SavedConnection {
                id: "pg".into(),
                name: "Local Postgres".into(),
                config: postgres_config(),
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
    std::fs::write(
        dir.join("settings.toml"),
        "page_size = 2000\nrow_cap = 5000\neditor_font_size = 16\n",
    )
    .expect("settings can be written");
    futures::executor::block_on(async {
        let session = sonde_core::connect(&postgres_config(), Some(Secret::new("sonde")))
            .await
            .expect("the Postgres fixture is running");
        for sql in [
            "drop schema if exists sonde_daily cascade",
            "create schema sonde_daily",
            "create table sonde_daily.invoice (id int4 primary key, customer text, \
             total numeric(10,2), \"Paid\" bool)",
            "insert into sonde_daily.invoice values (1, 'Ada', 12.50, true), (2, 'Lin, \"B\"', null, false)",
        ] {
            let handle = session.execute(sql, &[]).await.expect("fixture SQL runs");
            let _ = futures::StreamExt::collect::<Vec<_>>(handle.pages).await;
        }
    });
    let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecrets(Mutex::new(HashMap::from([(
        "pg".to_owned(),
        "sonde".to_owned(),
    )]))));

    let mut report = Report::default();
    let mut ui = Ui::launch(&dir, Arc::clone(&secrets), (1280., 820.));

    // ── Settings ─────────────────────────────────────────────────────────
    report.check(
        "settings: the editor font size from settings.toml applies",
        ui.cx.update(|cx| cx.theme_mono_size()) == 16.,
    );
    ui.open_connection("Local Postgres");
    let query = ui.new_query("Local Postgres");
    ui.write(&query, "select g from generate_series(1, 20000) g");
    ui.run(&query, "run-statement");
    report.check(
        "settings: page_size decides how many rows load first",
        ui.rows(&query, 0) == 2000 && ui.result(&query, 0, |r, _| r.has_more()) == Some(true),
    );
    ui.click("load-all");
    ui.finish(&query);
    report.check(
        "settings: row_cap stops a result and says why",
        ui.rows(&query, 0) == 5000 && ui.status(&query).contains("row cap"),
    );
    ui.shot("d01-row-cap");

    // ── Export ───────────────────────────────────────────────────────────
    let csv_path = dir.join("big.csv");
    ui.write(&query, "select g from generate_series(1, 4500) g");
    ui.run(&query, "run-statement");
    let panel = query.0.clone();
    let path = csv_path.clone();
    ui.cx
        .update(|cx| panel.update(cx, |panel, cx| panel.export(Format::Csv, path, cx)));
    let exported = ui.settle("the CSV export", |_, cx| {
        panel.read(cx).status(cx).starts_with("Saved")
    });
    let csv = std::fs::read_to_string(&csv_path).unwrap_or_default();
    report.check(
        "export: CSV loads the rows not yet fetched and writes them all",
        exported
            && csv.lines().count() == 4501
            && csv.starts_with("g\n1\n")
            && csv.ends_with("4500\n"),
    );
    report.check(
        "export: the status says where the file went",
        ui.status(&query).contains("big.csv"),
    );
    ui.write(&query, "select * from sonde_daily.invoice order by id");
    ui.run(&query, "run-statement");
    let json_path = dir.join("invoice.json");
    let path = json_path.clone();
    let panel = query.0.clone();
    ui.cx
        .update(|cx| panel.update(cx, |panel, cx| panel.export(Format::Json, path, cx)));
    ui.settle("the JSON export", |_, cx| {
        panel.read(cx).status(cx).starts_with("Saved")
    });
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&json_path).unwrap_or_default())
            .unwrap_or_default();
    report.check(
        "export: JSON keeps types, decimals as text, NULL as null",
        json == serde_json::json!([
            {"id": 1, "customer": "Ada", "total": "12.50", "Paid": true},
            {"id": 2, "customer": "Lin, \"B\"", "total": null, "Paid": false}
        ]),
    );
    report.check(
        "export: the Export button is offered for a result",
        ui.has("export-result"),
    );
    ui.shot("d02-export");

    // ── History ──────────────────────────────────────────────────────────
    ui.write(&query, "select nope_column");
    ui.run(&query, "run-statement");
    let history = ui.read(&query, |panel, cx| panel.history(cx));
    report.check(
        "history: every statement run is recorded, newest first",
        history.len() == 4
            && history[0].sql == "select nope_column"
            && history[1].sql == "select * from sonde_daily.invoice order by id",
    );
    report.check(
        "history: a failed statement keeps its error, a good one its row count",
        history[0]
            .error
            .as_deref()
            .is_some_and(|e| e.contains("nope_column"))
            && history[1].rows == Some(2),
    );
    ui.click("query-history");
    ui.shot("d03-history-menu");
    ui.click_label("select * from sonde_daily.invoice order by id");
    report.check(
        "history: picking an entry puts its SQL back in the editor",
        ui.read(&query, |panel, cx| panel.text(cx))
            == "select * from sonde_daily.invoice order by id",
    );

    // ── Completion ───────────────────────────────────────────────────────
    ui.write_at(&query, "select * from sonde_daily.inv|");
    report.check(
        "completion: a schema's tables after `schema.`",
        ui.labels(&query, false) == ["invoice"],
    );
    ui.write_at(&query, "select cu| from sonde_daily.invoice");
    report.check(
        "completion: columns of the table the statement names",
        ui.labels(&query, false).first().map(String::as_str) == Some("customer"),
    );
    ui.write_at(&query, "select i.| from sonde_daily.invoice i");
    let candidates = ui.completions(&query, false);
    report.check(
        "completion: columns through an alias, quoted where Postgres needs it",
        candidates
            .iter()
            .map(|c| c.label.as_str())
            .collect::<Vec<_>>()
            == ["id", "customer", "total", "Paid"]
            && candidates.iter().any(|c| c.insert == "\"Paid\""),
    );
    ui.write_at(&query, "SEL|");
    report.check(
        "completion: keywords in the case being typed",
        ui.labels(&query, false).contains(&"SELECT".to_owned()),
    );
    ui.write_at(&query, "select * from |");
    report.check(
        "completion: nothing typed offers nothing until asked (Ctrl+Space)",
        ui.labels(&query, false).is_empty()
            && ui.labels(&query, true).contains(&"sonde_daily".to_owned()),
    );
    ui.write(&query, "create table sonde_daily.fresh_one (x int)");
    ui.run(&query, "run-statement");
    ui.write_at(&query, "select * from sonde_daily.fre|");
    report.check(
        "completion: a table created in the tab is offered at once",
        ui.labels(&query, false) == ["fresh_one"],
    );
    ui.write_at(&query, "select 'fr|");
    report.check(
        "completion: quiet inside a string",
        ui.labels(&query, true).is_empty(),
    );

    // ── Reconnecting ─────────────────────────────────────────────────────
    ui.write(&query, "select pg_backend_pid()");
    ui.run(&query, "run-statement");
    let pid = ui.result(&query, 0, |r, cx| r.value(0, 0, cx)).flatten();
    futures::executor::block_on(async {
        let killer = sonde_core::connect(&postgres_config(), Some(Secret::new("sonde")))
            .await
            .expect("a second session");
        let sql = format!(
            "select pg_terminate_backend({})",
            pid.map(|p| p.to_string()).unwrap_or_default()
        );
        let handle = killer.execute(&sql, &[]).await.expect("terminate runs");
        let _ = futures::StreamExt::collect::<Vec<_>>(handle.pages).await;
    });
    thread::sleep(Duration::from_millis(200));
    ui.write(&query, "select 'after' as back");
    ui.run(&query, "run-statement");
    report.check(
        "reconnect: a dropped connection says so",
        ui.problem(&query)
            .contains("connection to the database was lost"),
    );
    let panel = query.0.clone();
    let back = ui.settle("the reconnect", move |_, cx| panel.read(cx).is_ready());
    ui.run(&query, "run-statement");
    report.check(
        "reconnect: the tab reconnects by itself and the statement runs again",
        back && ui.result(&query, 0, |r, cx| r.value(0, 0, cx)) == Some(Some(Value::text("after"))),
    );
    ui.shot("d04-reconnected");

    // ── SQLite ───────────────────────────────────────────────────────────
    ui.open_connection("Notes");
    let notes = ui.new_query("Notes");
    ui.write_at(&notes, "select bo| from note");
    report.check(
        "completion: works on SQLite too",
        ui.labels(&notes, false).first().map(String::as_str) == Some("body"),
    );
    ui.write(&notes, "select body from note order by id");

    // ── Reopening the last session ──────────────────────────────────────
    ui.write(&query, "select 1;\nselect 'two'");
    let workspace = ui.workspace.clone();
    ui.cx.update(|cx| workspace.read(cx).save_session(cx));
    drop(ui);
    let mut ui = Ui::launch(&dir, Arc::clone(&secrets), (1280., 820.));
    ui.frame();
    let reopened: Vec<String> = {
        let queries = ui.queries();
        ui.cx
            .update(|cx| queries.iter().map(|q| q.read(cx).text(cx)).collect())
    };
    report.check(
        "session: last session's query tabs reopen with their text",
        reopened
            == [
                "select 1;\nselect 'two'",
                "select body from note order by id",
            ],
    );
    let queries = ui.queries();
    let restored = Query(queries[1].clone());
    let panel = restored.0.clone();
    ui.settle("the restored tab's session", move |_, cx| {
        panel.read(cx).is_ready()
    });
    ui.run(&restored, "run-statement");
    report.check(
        "session: a reopened tab runs on its own connection",
        ui.rows(&restored, 0) == 2,
    );
    ui.shot("d05-restored");

    println!();
    if report.failures == 0 {
        println!("daily walkthrough: all checks passed");
        ExitCode::SUCCESS
    } else {
        println!("daily walkthrough: {} check(s) FAILED", report.failures);
        ExitCode::FAILURE
    }
}

trait MonoSize {
    fn theme_mono_size(&self) -> f32;
}

impl MonoSize for App {
    fn theme_mono_size(&self) -> f32 {
        use gpui_kit::component::ActiveTheme as _;
        self.theme().mono_font_size.into()
    }
}
