//! A click-through of the table tab (M4) in a headless window with the real
//! renderer, against the Postgres fixture and a SQLite file.
//!
//! Each step is checked and reported. Set `SONDE_SHOTS` to a directory to
//! also get a PNG of the window after each stage.
//!
//! ```sh
//! docker compose -f tests/docker-compose.yml up -d
//! SONDE_SHOTS=/tmp/shots cargo test -p sonde-app --test table
//! ```

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use futures::{StreamExt as _, executor::block_on};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Entity, HeadlessAppContext, InputEvent as _,
    MouseButton, MouseDownEvent, MouseUpEvent, Window, WindowBounds, WindowOptions,
    component::{Theme, ThemeMode, table::ColumnSort},
    px, size,
    test::{TestEventExt as _, TestQueryExt as _, TestWindowExt as _},
};
use sonde_app::{
    connections::Connections, layout::LayoutFile, table_view::TableView, workspace::Workspace,
};
use sonde_core::{
    ConnectionConfig, PostgresConfig, Result, Secret, Session, SqliteConfig, TlsMode, Value,
    config::{ConnectionStore, SavedConnection, SecretStore},
};

const PG_PORT: u16 = 54329;
const BIG_ROWS: usize = 30_000;

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
        // Rows are woken from the driver runtime's threads.
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

    fn click_label_times(&mut self, label: &str, times: usize) {
        self.click_nth_label(label, 0, times);
    }

    /// Click the `nth` element carrying this accessible name, `times` in
    /// quick succession.
    fn click_nth_label(&mut self, label: &str, nth: usize, times: usize) {
        self.with(|window, cx| {
            let position = window.find_all_by_label(label)[nth].bounds().center();
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

    /// Open a table from its right-click menu in the sidebar.
    fn open_from_menu(&mut self, label: &str) {
        self.right_click_label(label);
        self.click_label("Open table");
    }

    /// Scroll the sidebar's tree down until `label`'s row is inside the
    /// list's visible area, where a click can land on it.
    fn scroll_tree_until(&mut self, label: &str) {
        for _ in 0..50 {
            let inside = self.with(|window, _| {
                let tree = window.find("connection-tree").bounds();
                window
                    .find_all_by_label(label)
                    .first()
                    .is_some_and(|element| {
                        let y = element.bounds().center().y;
                        y >= tree.top() + px(12.) && y <= tree.bottom() - px(12.)
                    })
            });
            if inside {
                return;
            }
            self.with(|window, cx| {
                let position = window.find("connection-tree").bounds().center();
                window.dispatch_event(
                    gpui_kit::ScrollEvent {
                        position,
                        delta: gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), px(-100.))),
                        ..Default::default()
                    }
                    .to_platform_input(),
                    cx,
                );
            });
            self.frame();
        }
    }

    /// Scroll the sidebar's tree back to its first row.
    fn scroll_tree_to_top(&mut self) {
        self.with(|window, cx| {
            let position = window.find("connection-tree").bounds().center();
            window.dispatch_event(
                gpui_kit::ScrollEvent {
                    position,
                    delta: gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), px(100_000.))),
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
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

    fn label_of(&mut self, id: &'static str) -> Option<String> {
        self.with(|window, _| {
            window
                .try_find(id)
                .and_then(|node| node.label().map(str::to_owned))
        })
    }

    fn tables(&mut self) -> Vec<Entity<TableView>> {
        let workspace = self.workspace.clone();
        self.cx.update(|cx| workspace.read(cx).tables())
    }

    fn read<R>(&mut self, tab: &Entity<TableView>, f: impl FnOnce(&TableView, &App) -> R) -> R {
        let tab = tab.clone();
        self.cx.update(|cx| f(tab.read(cx), cx))
    }

    fn act(
        &mut self,
        tab: &Entity<TableView>,
        f: impl FnOnce(&mut TableView, &mut Window, &mut gpui_kit::Context<TableView>),
    ) {
        let tab = tab.clone();
        self.with(|window, cx| tab.update(cx, |tab, cx| f(tab, window, cx)));
        self.frame();
    }

    /// Wait until the tab is neither connecting, loading nor applying.
    fn idle(&mut self, tab: &Entity<TableView>) -> bool {
        let tab = tab.clone();
        self.settle("the table tab to finish loading", move |_, cx| {
            let tab = tab.read(cx);
            !tab.is_loading()
                && (tab.rows_loaded(cx) > 0
                    || tab.problem().is_some()
                    || !tab.status(cx).starts_with("Connecting"))
        })
    }

    fn cell(&mut self, tab: &Entity<TableView>, row: usize, column: &str) -> Option<Value> {
        self.read(tab, |tab, cx| tab.value(row, tab.column(column)?, cx))
    }

    /// Edit one cell through the editor: open it with the keyboard, type,
    /// and stage with Enter.
    fn edit(&mut self, tab: &Entity<TableView>, row: usize, column: &str, text: &str) {
        let column = self
            .read(tab, |tab, _| tab.column(column))
            .expect("the column exists");
        self.act(tab, |tab, window, cx| {
            tab.select_cell(row, column, window, cx)
        });
        self.press("enter");
        self.act(tab, |tab, window, cx| tab.set_edit_text(text, window, cx));
        self.press("enter");
    }

    fn shot(&mut self, name: &str) {
        self.frame();
        if self.shots.is_none() {
            return;
        }
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

fn postgres() -> Box<dyn Session> {
    block_on(sonde_core::connect(
        &postgres_config(),
        Some(Secret::new("sonde")),
    ))
    .expect("the Postgres fixture is running")
}

/// Run statements and return the rows of the last one.
fn sql(session: &dyn Session, statements: &[&str]) -> Vec<Vec<Value>> {
    block_on(async {
        let mut rows = Vec::new();
        for statement in statements {
            rows.clear();
            let mut handle = session
                .execute(statement, &[])
                .await
                .expect("fixture SQL runs");
            while let Some(page) = handle.pages.next().await {
                rows.extend(page.expect("fixture SQL runs").rows);
            }
        }
        rows
    })
}

/// Drops the fixture schema when the test ends, panics included, so the
/// other click-throughs see the database as they expect it.
struct DropFixture;

impl Drop for DropFixture {
    fn drop(&mut self) {
        sql(&*postgres(), &["drop schema if exists sonde_tab cascade"]);
    }
}

fn postgres_fixture(session: &dyn Session) {
    sql(
        session,
        &[
            "drop schema if exists sonde_tab cascade",
            "create schema sonde_tab",
            &format!(
                "create table sonde_tab.big as
                 select g as id, g % 7 as bucket from generate_series(1, {BIG_ROWS}) g"
            ),
            "alter table sonde_tab.big add primary key (id)",
            "create table sonde_tab.every_type (
                id int4 primary key,
                flag bool, i2 int2, i4 int4, i8 int8, f4 float4, f8 float8,
                num numeric(12, 3), txt text, ch varchar(9), bin bytea,
                d date, t time, ts timestamp, tstz timestamptz, iv interval,
                uid uuid, js json, jsb jsonb, tags int4[])",
            "insert into sonde_tab.every_type values (
                1, true, -7, 42, -9223372036854775808, 1.5, -2.25,
                12345.670, 'héllo', 'abc', '\\xdeadbeef',
                '2024-02-29', '13:14:15.250', '2024-02-29 13:14:15', '2024-02-29 13:14:15+02',
                '1 year 2 mons 3 days 04:05:06',
                '6f1c9e0a-58a5-4c7e-9a43-0c3f1d2b4e5f', '{\"a\": 1}', '{\"a\": [1, 2]}', '{1, 2, 3}'),
                (2, null, null, null, null, null, null, null, '  padded ', null, null,
                 null, null, null, null, null, null, null, null, null)",
            "create table sonde_tab.conflict (id int4 primary key, name text check (name <> 'bad'))",
            "insert into sonde_tab.conflict values (1, 'a'), (2, 'b')",
            "create table sonde_tab.no_key (name text)",
            "insert into sonde_tab.no_key values ('only')",
        ],
    );
}

/// What the editor is given for each column, and the text the database must
/// then hold for it (as Postgres prints it back through `::text`).
const EDITS: [(&str, &str); 19] = [
    ("flag", "false"),
    ("i2", "8"),
    ("i4", "-42"),
    ("i8", "9223372036854775807"),
    ("f4", "-1.25"),
    ("f8", "3.5"),
    ("num", "0.001"),
    ("txt", "edited 'quote'"),
    ("ch", "xyz"),
    ("bin", "\\xcafe"),
    ("d", "1969-07-20"),
    ("t", "01:02:03"),
    ("ts", "1999-12-31 23:59:59"),
    ("tstz", "2000-01-01 00:00:00+00"),
    ("iv", "2 days 05:00:00"),
    ("uid", "01234567-89ab-4cde-9f01-23456789abcd"),
    ("js", "{\"b\": 2}"),
    ("jsb", "{\"b\": [3]}"),
    ("tags", "{4,5}"),
];

fn main() -> ExitCode {
    let dir = std::env::temp_dir().join(format!("sonde-table-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory can be created");
    let sqlite_path = dir.join("notes.db");
    rusqlite::Connection::open(&sqlite_path)
        .and_then(|c| {
            c.execute_batch(
                "create table note (id integer primary key, body text, score real default 1);
                 insert into note (body, score) values ('first', 1.5), (null, 2);
                 create table log (msg text);
                 insert into log values ('started');
                 create view note_bodies as select body from note;",
            )
        })
        .expect("a SQLite file can be created");

    let outside = postgres();
    postgres_fixture(&*outside);
    let _fixture = DropFixture;

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
                    path: sqlite_path.clone(),
                    read_only: false,
                }),
            },
            SavedConnection {
                id: "notes-ro".into(),
                name: "Notes read-only".into(),
                config: ConnectionConfig::Sqlite(SqliteConfig {
                    path: sqlite_path.clone(),
                    read_only: true,
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

    // ── Opening a table from the sidebar ─────────────────────────────────
    ui.click_label("Local Postgres");
    ui.settle("the schema list", |window, _| {
        window.try_find_by_label("sonde_tab").is_some()
    });
    ui.click_label("sonde_tab");
    ui.settle("the table list", |window, _| {
        window.try_find_by_label("big").is_some()
    });
    ui.click_label_times("big", 2);
    let big = ui.tables().pop();
    report.check(
        "sidebar: double-clicking a table opens its rows in a tab",
        big.is_some(),
    );
    let Some(big) = big else {
        return ExitCode::FAILURE;
    };
    ui.idle(&big);
    report.check(
        "paging: the first 10,000 rows load and the rest wait",
        ui.read(&big, |tab, cx| tab.rows_loaded(cx)) == 10_000
            && ui.read(&big, |tab, _| tab.has_more()),
    );
    report.check(
        "status: says how many rows are loaded and that more are available",
        ui.label_of("table-status")
            .is_some_and(|status| status.contains("10000 rows loaded, more available"))
            || ui.read(&big, |tab, cx| tab.status(cx)) == "10000 rows loaded, more available",
    );
    ui.shot("t01-big-table");

    ui.click("load-all");
    ui.idle(&big);
    report.check(
        "paging: Load all rows brings in every row",
        ui.read(&big, |tab, cx| tab.rows_loaded(cx)) == BIG_ROWS
            && !ui.read(&big, |tab, _| tab.has_more()),
    );

    ui.click_label_times("big", 2);
    report.check(
        "sidebar: opening an open table goes to its tab instead of a second one",
        ui.tables().len() == 1,
    );

    // ── Sort and filter run on the server ────────────────────────────────
    ui.act(&big, |tab, _, cx| {
        tab.sort_by(0, ColumnSort::Descending, cx)
    });
    ui.idle(&big);
    report.check(
        "sort: descending on id puts the last row first",
        ui.cell(&big, 0, "id") == Some(Value::Int(BIG_ROWS as i64)),
    );
    ui.act(&big, |tab, window, cx| {
        tab.set_filter("id <= 3 or bucket = -1 -- a trailing comment", window, cx);
        tab.focus_filter(window, cx);
    });
    ui.press("enter");
    ui.idle(&big);
    report.check(
        "filter: Enter runs the WHERE fragment, OR and trailing comment included",
        ui.read(&big, |tab, cx| tab.rows_loaded(cx)) == 3
            && ui.cell(&big, 0, "id") == Some(Value::Int(3)),
    );
    report.check(
        "filter: the status says the rows match the filter",
        ui.read(&big, |tab, cx| tab.status(cx)) == "3 rows match the filter",
    );
    ui.shot("t02-sorted-filtered");
    ui.act(&big, |tab, window, cx| {
        tab.set_filter("no_such_column = 1", window, cx);
        tab.focus_filter(window, cx);
    });
    ui.press("enter");
    ui.idle(&big);
    report.check(
        "filter: a bad filter shows the database's error",
        ui.has("table-problem")
            && ui
                .read(&big, |tab, _| tab.problem())
                .is_some_and(|problem| problem.contains("no_such_column")),
    );
    ui.shot("t03-bad-filter");
    ui.act(&big, |tab, window, cx| {
        tab.set_filter("", window, cx);
        tab.focus_filter(window, cx);
    });
    ui.press("enter");
    ui.idle(&big);
    report.check(
        "filter: clearing it and pressing Enter loads the table again",
        ui.read(&big, |tab, _| tab.problem()).is_none()
            && ui.read(&big, |tab, cx| tab.rows_loaded(cx)) == 10_000,
    );

    // ── Every type round-trips through the editor ───────────────────────
    ui.open_from_menu("every_type");
    let types = ui
        .tables()
        .pop()
        .filter(|tab| ui.read(tab, |tab, _| tab.name() == "every_type"));
    report.check(
        "menu: Open table in a table's right-click menu opens it",
        types.is_some(),
    );
    let Some(types) = types else {
        return ExitCode::FAILURE;
    };
    ui.idle(&types);
    ui.act(&types, |tab, _, cx| {
        tab.sort_by(0, ColumnSort::Ascending, cx)
    });
    ui.idle(&types);
    report.check(
        "editing: a table with a primary key is editable",
        ui.read(&types, |tab, _| tab.is_editable())
            && ui.has("add-row")
            && !ui.has("table-read-only"),
    );

    let column_count = EDITS.len() + 1;
    ui.act(&types, |tab, window, cx| tab.select_cell(0, 1, window, cx));
    ui.press("enter");
    report.check(
        "editing: Enter on a selected cell opens the editor",
        ui.read(&types, |tab, _| tab.is_editing()) && ui.has("cell-editor"),
    );
    ui.shot("t04-editor-open");
    ui.press("escape");
    report.check(
        "editing: Escape closes the editor without staging",
        !ui.read(&types, |tab, _| tab.is_editing())
            && ui.read(&types, |tab, cx| tab.staged(cx)) == 0,
    );
    for row in 0..2 {
        for column in 1..column_count {
            ui.act(&types, |tab, window, cx| {
                tab.edit_cell(row, column, window, cx)
            });
            ui.act(&types, |tab, window, cx| tab.save_edit(window, cx));
        }
    }
    report.check(
        "round trip: staging every cell's own text, NULLs and padded text included, changes nothing",
        ui.read(&types, |tab, cx| tab.staged(cx)) == 0 && ui.read(&types, |tab, _| tab.problem()).is_none(),
    );
    if let Some(problem) = ui.read(&types, |tab, _| tab.problem()) {
        println!("     {problem}");
    }
    for statement in ui.read(&types, |tab, cx| tab.statements(cx)) {
        println!("     staged by the round trip: {}", statement.rendered());
    }
    ui.act(&types, |tab, _, cx| tab.discard(cx));

    for (column, text) in EDITS {
        let before = ui.read(&types, |tab, cx| tab.staged(cx));
        ui.edit(&types, 0, column, text);
        if let Some(problem) = ui.read(&types, |tab, _| tab.problem()) {
            println!("     {column}: {problem} (staged before: {before})");
        }
    }
    report.check(
        "editing: edits to one row stage as one change",
        ui.read(&types, |tab, cx| tab.staged(cx)) == 1
            && ui.read(&types, |tab, _| tab.problem()).is_none(),
    );
    if let Some(problem) = ui.read(&types, |tab, _| tab.problem()) {
        println!("     {problem}");
    }
    ui.edit(&types, 1, "i4", "forty");
    report.check(
        "editing: text the column cannot hold is refused with a reason",
        ui.read(&types, |tab, _| tab.problem())
            .is_some_and(|problem| problem.contains("i4"))
            && ui.read(&types, |tab, cx| tab.staged(cx)) == 1,
    );
    ui.press("escape");
    ui.act(&types, |tab, window, cx| tab.edit_cell(1, 8, window, cx));
    ui.click("set-null");
    report.check(
        "editing: Set NULL stages a NULL",
        ui.read(&types, |tab, cx| tab.staged(cx)) == 2,
    );
    report.check(
        "staging: nothing reaches the database before Apply",
        sql(
            &*outside,
            &["select flag::text from sonde_tab.every_type where id = 1"],
        ) == vec![vec![Value::text("true")]],
    );
    ui.shot("t05-staged-edits");

    ui.click("review-changes");
    let preview = ui.label_of("apply-preview").unwrap_or_default();
    report.check(
        "apply: the dialog previews the exact statements",
        preview.contains("UPDATE \"public\"")
            || preview.contains("UPDATE \"sonde_tab\".\"every_type\" SET \"flag\" = false"),
    );
    println!("     {}", preview.lines().next().unwrap_or_default());
    ui.shot("t06-apply-preview");
    ui.click("apply-changes");
    ui.idle(&types);
    let held = sql(
        &*outside,
        &[&format!(
            "select {} from sonde_tab.every_type where id = 1",
            EDITS
                .iter()
                .map(|(column, _)| format!("{column}::text"))
                .collect::<Vec<_>>()
                .join(", ")
        )],
    );
    let expected: Vec<Value> = EDITS.iter().map(|(_, text)| Value::text(*text)).collect();
    let mismatches: Vec<String> = held
        .first()
        .map(|row| {
            row.iter()
                .zip(&expected)
                .zip(EDITS)
                .filter(|((got, want), _)| got != want)
                .map(|((got, want), (column, _))| format!("{column}: got {got}, want {want}"))
                .collect()
        })
        .unwrap_or_else(|| vec!["the row is gone".into()]);
    report.check(
        "apply: every type in PLAN 4.4 is written as edited",
        mismatches.is_empty(),
    );
    for mismatch in &mismatches {
        println!("     {mismatch}");
    }
    report.check(
        "apply: the status reports what was written and the rows reload",
        ui.read(&types, |tab, cx| tab.status(cx))
            .starts_with("Applied 2 changes")
            && ui.read(&types, |tab, cx| tab.staged(cx)) == 0,
    );
    report.check(
        "apply: Set NULL wrote a NULL",
        sql(
            &*outside,
            &["select txt is null from sonde_tab.every_type where id = 2"],
        ) == vec![vec![Value::Bool(true)]],
    );
    ui.shot("t07-applied");

    // ── New rows and deletes ─────────────────────────────────────────────
    ui.click("add-row");
    report.check(
        "add row: stages a new row and opens the editor on it",
        ui.read(&types, |tab, cx| tab.new_rows(cx)) == 1
            && ui.read(&types, |tab, _| tab.is_editing()),
    );
    ui.act(&types, |tab, window, cx| tab.set_edit_text("3", window, cx));
    ui.press("enter");
    ui.edit(&types, 2, "txt", "brand new");
    ui.act(&types, |tab, window, cx| tab.select_cell(1, 0, window, cx));
    ui.press("delete");
    report.check(
        "delete row: the Delete key stages the selected row",
        ui.read(&types, |tab, cx| tab.staged(cx)) == 2,
    );
    ui.shot("t08-insert-and-delete");
    ui.click("review-changes");
    let preview = ui.label_of("apply-preview").unwrap_or_default();
    report.check(
        "apply: deletes come before inserts, and unset columns take their defaults",
        preview.find("DELETE") < preview.find("INSERT")
            && preview.contains("INSERT INTO \"sonde_tab\".\"every_type\" (\"id\", \"txt\") VALUES (3, 'brand new')"),
    );
    ui.click("apply-changes");
    ui.idle(&types);
    report.check(
        "apply: the new row is written and the deleted one is gone",
        sql(
            &*outside,
            &["select id from sonde_tab.every_type order by id"],
        ) == vec![vec![Value::Int(1)], vec![Value::Int(3)]],
    );

    // ── A row changed elsewhere rolls the whole change set back ──────────
    ui.open_from_menu("conflict");
    let conflict = ui.tables().pop().expect("the conflict table opened");
    ui.idle(&conflict);
    ui.act(&conflict, |tab, _, cx| {
        tab.sort_by(0, ColumnSort::Ascending, cx)
    });
    ui.idle(&conflict);
    ui.edit(&conflict, 0, "name", "x");
    ui.edit(&conflict, 1, "name", "y");
    sql(&*outside, &["delete from sonde_tab.conflict where id = 1"]);
    ui.click("review-changes");
    ui.click("apply-changes");
    ui.idle(&conflict);
    report.check(
        "conflict: a row deleted elsewhere makes apply write nothing",
        sql(&*outside, &["select name from sonde_tab.conflict"]) == vec![vec![Value::text("b")]],
    );
    report.check(
        "conflict: the message says nothing was written and why",
        ui.read(&conflict, |tab, _| tab.problem())
            .is_some_and(|problem| problem.starts_with("Nothing was written. A row you changed")),
    );
    report.check(
        "conflict: the staged changes are dropped and the rows reloaded",
        ui.read(&conflict, |tab, cx| tab.staged(cx)) == 0
            && ui.read(&conflict, |tab, cx| tab.rows_loaded(cx)) == 1,
    );
    ui.shot("t09-conflict");

    ui.edit(&conflict, 0, "name", "bad");
    ui.click("review-changes");
    ui.click("apply-changes");
    ui.idle(&conflict);
    report.check(
        "database error: apply writes nothing and keeps the changes to fix",
        ui.read(&conflict, |tab, _| tab.problem())
            .is_some_and(|problem| problem.contains("check constraint"))
            && ui.read(&conflict, |tab, cx| tab.staged(cx)) == 1
            && sql(&*outside, &["select name from sonde_tab.conflict"])
                == vec![vec![Value::text("b")]],
    );
    ui.click("discard-changes");
    report.check(
        "discard: drops every staged change",
        ui.read(&conflict, |tab, cx| tab.staged(cx)) == 0,
    );

    // ── Read-only tables ─────────────────────────────────────────────────
    ui.open_from_menu("no_key");
    let no_key = ui.tables().pop().expect("the no_key table opened");
    ui.idle(&no_key);
    report.check(
        "read-only: a table without a primary key shows why and offers no editing",
        ui.label_of("table-read-only")
            .is_some_and(|reason| reason.contains("no primary key"))
            && !ui.has("add-row")
            && ui.read(&no_key, |tab, cx| tab.rows_loaded(cx)) == 1,
    );
    ui.act(&no_key, |tab, window, cx| tab.edit_cell(0, 0, window, cx));
    report.check(
        "read-only: double-click does not open an editor",
        !ui.read(&no_key, |tab, _| tab.is_editing()),
    );
    ui.shot("t10-read-only");

    // ── SQLite: rowid tables, views, read-only files ────────────────────
    // Fold Postgres away so the SQLite connections are in view.
    ui.click_label("sonde_tab");
    ui.scroll_tree_to_top();
    ui.click_label("Local Postgres");
    ui.click_label("Notes");
    ui.settle("the SQLite schema", |window, _| {
        window.try_find_by_label("main").is_some()
    });
    ui.scroll_tree_until("main");
    ui.click_label("main");
    ui.settle("the SQLite tables", |window, _| {
        window.try_find_by_label("log").is_some()
    });
    ui.scroll_tree_until("log");
    ui.click_label_times("log", 2);
    let log = ui
        .tables()
        .pop()
        .filter(|tab| ui.read(tab, |tab, _| tab.name() == "log"));
    report.check("SQLite: double-clicking a table opens it", log.is_some());
    let Some(log) = log else {
        return ExitCode::FAILURE;
    };
    ui.idle(&log);
    report.check(
        "SQLite: a table without a primary key is keyed on its rowid",
        ui.read(&log, |tab, _| tab.is_editable())
            && ui.cell(&log, 0, "rowid") == Some(Value::Int(1)),
    );
    ui.act(&log, |tab, window, cx| tab.edit_cell(0, 0, window, cx));
    report.check(
        "SQLite: the rowid itself cannot be edited",
        !ui.read(&log, |tab, _| tab.is_editing())
            && ui
                .read(&log, |tab, _| tab.problem())
                .is_some_and(|problem| problem.contains("rowid")),
    );
    ui.edit(&log, 0, "msg", "  restarted ");
    ui.click("add-row");
    ui.act(&log, |tab, window, cx| {
        tab.set_edit_text("second", window, cx)
    });
    ui.press("enter");
    ui.click("review-changes");
    ui.click("apply-changes");
    ui.idle(&log);
    let file = rusqlite::Connection::open(&sqlite_path).expect("the SQLite file opens");
    let messages: Vec<(i64, String)> = file
        .prepare("select rowid, msg from log order by rowid")
        .and_then(|mut statement| {
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect()
        })
        .unwrap_or_default();
    report.check(
        "SQLite: edits keyed on rowid apply, text keeps its spaces, and new rows get a rowid",
        messages == vec![(1, "  restarted ".to_owned()), (2, "second".to_owned())],
    );
    println!("     {messages:?}");

    ui.open_from_menu("note_bodies");
    let view = ui.tables().pop().expect("the view opened");
    ui.idle(&view);
    report.check(
        "SQLite: a view is read-only",
        !ui.read(&view, |tab, _| tab.is_editable()) && ui.has("table-read-only"),
    );

    ui.click_label("Notes read-only");
    ui.settle("the read-only file's schema", |window, _| {
        window.find_all_by_label("main").len() > 1
    });
    ui.click_nth_label("main", 1, 1);
    ui.settle("the read-only file's tables", |window, _| {
        window.find_all_by_label("note").len() > 1
    });
    // The second "note" row belongs to the read-only connection, listed
    // below the first.
    ui.click_nth_label("note", 1, 2);
    let read_only = ui.tables().pop().expect("the read-only table opened");
    ui.idle(&read_only);
    report.check(
        "SQLite: a file opened read-only says so and offers no editing",
        ui.read(&read_only, |tab, _| tab.read_only_reason())
            .is_some_and(|reason| reason.contains("read-only"))
            && ui.read(&read_only, |tab, cx| tab.rows_loaded(cx)) == 2,
    );

    // ── Both themes ──────────────────────────────────────────────────────
    ui.with(|window, cx| Theme::change(ThemeMode::Dark, Some(window), cx));
    ui.click_label("Local Postgres");
    ui.settle("the schema list", |window, _| {
        window.try_find_by_label("sonde_tab").is_some()
    });
    ui.click_label("sonde_tab");
    ui.settle("the table list", |window, _| {
        window.try_find_by_label("every_type").is_some()
    });
    ui.scroll_tree_until("every_type");
    ui.click_label_times("every_type", 2);
    ui.edit(&types, 0, "txt", "dark mode");
    ui.click("add-row");
    ui.press("escape");
    ui.act(&types, |tab, window, cx| tab.select_cell(0, 0, window, cx));
    ui.press("delete");
    ui.shot("t11-dark-staged");
    ui.with(|window, cx| Theme::change(ThemeMode::Light, Some(window), cx));
    ui.shot("t12-light-staged");

    drop(ui);
    let mut ui = Ui::launch(&dir, secrets, (640., 520.));
    ui.click_label("Notes");
    ui.settle("the SQLite schema", |window, _| {
        window.try_find_by_label("main").is_some()
    });
    ui.click_label("main");
    ui.settle("the SQLite tables", |window, _| {
        window.try_find_by_label("note").is_some()
    });
    ui.scroll_tree_until("note");
    ui.click_label_times("note", 2);
    let note = ui.tables().pop().expect("the narrow table opened");
    ui.idle(&note);
    ui.click("add-row");
    let controls_fit = |window: &mut Window, ids: &[&'static str]| {
        let viewport = window.viewport_size();
        ids.iter().all(|&id| {
            window.try_find(id).is_some_and(|element| {
                let bounds = element.bounds();
                bounds.left() >= px(0.)
                    && bounds.right() <= viewport.width
                    && bounds.top() >= px(0.)
                    && bounds.bottom() <= viewport.height
            })
        })
    };
    report.check(
        "narrow window: editing keeps its actions and at least one grid row visible",
        ui.with(|window, _| {
            controls_fit(
                window,
                &[
                    "reload-rows",
                    "review-changes",
                    "use-default",
                    "set-null",
                    "cancel-edit",
                    "stage-edit",
                ],
            ) && window.find("table").bounds().size.height >= px(76.)
        }),
    );
    ui.shot("t13-narrow-editor");
    ui.press("escape");
    ui.click("review-changes");
    report.check(
        "narrow window: review keeps both actions visible",
        ui.with(|window, _| controls_fit(window, &["keep-editing", "apply-changes"])),
    );
    ui.shot("t14-narrow-review");
    ui.click("keep-editing");
    drop(ui);

    let _ = std::fs::remove_dir_all(&dir);
    println!(
        "\n{} {}",
        if report.failures == 0 { "PASS" } else { "FAIL" },
        if report.failures == 0 {
            "every check".to_owned()
        } else {
            format!("{} checks failed", report.failures)
        }
    );
    if report.failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
