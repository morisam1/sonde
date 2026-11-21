//! SQL Server in the real window: connect from the sidebar, browse, run SQL with
//! completion, and edit a table through the review dialog.
//!
//! ```sh
//! docker compose -f tests/docker-compose.yml up -d
//! cargo test -p sonde-app --test sqlserver
//! ```

#![allow(dead_code)]

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
    MouseButton, MouseDownEvent, MouseUpEvent, Window, WindowBounds, WindowOptions, px, size,
    test::{TestEventExt as _, TestQueryExt as _, TestWindowExt as _},
};
use sonde_app::{
    connections::Connections, layout::LayoutFile, query::QueryPanel, table_view::TableView,
    workspace::Workspace,
};
use sonde_core::{
    ConnectionConfig, Result, Secret, ServerConfig, TlsMode, Value,
    config::{ConnectionStore, SavedConnection, SecretStore},
};

const MSSQL_PORT: u16 = 14339;

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

impl Ui {
    fn queries(&mut self) -> Vec<Entity<QueryPanel>> {
        let workspace = self.workspace.clone();
        self.cx.update(|cx| workspace.read(cx).queries())
    }

    /// Open a query tab from a connection's right-click menu.
    fn new_query(&mut self, connection: &str) -> Entity<QueryPanel> {
        self.right_click_label(connection);
        self.click_label("New query");
        let panel = self.queries().pop().expect("a query tab opened");
        let ready = panel.clone();
        self.settle("the query tab's session", move |_, cx| {
            ready.read(cx).is_ready()
        });
        panel
    }

    fn run_sql(&mut self, panel: &Entity<QueryPanel>, sql: &str) {
        let target = panel.clone();
        self.with(|window, cx| target.update(cx, |p, cx| p.set_text(sql, window, cx)));
        self.click("run-all");
        let target = panel.clone();
        self.settle("the run to finish", move |_, cx| {
            let p = target.read(cx);
            !p.is_running() && p.results().iter().all(|r| !r.read(cx).is_streaming())
        });
    }

    fn first_cell(&mut self, panel: &Entity<QueryPanel>) -> Option<Value> {
        let panel = panel.clone();
        self.cx.update(|cx| {
            let last = panel.read(cx).results().last().cloned()?;
            last.read(cx).value(0, 0, cx)
        })
    }

    fn completion_labels(&mut self, panel: &Entity<QueryPanel>, marked: &str) -> Vec<String> {
        let cursor = marked.find('|').expect("a cursor mark");
        let target = panel.clone();
        self.with(|window, cx| {
            target.update(cx, |p, cx| {
                p.set_text(&marked.replace('|', ""), window, cx);
                p.set_cursor(cursor, window, cx);
            });
        });
        let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        let fill = slot.clone();
        let target = panel.clone();
        self.cx.update(|cx| {
            let task = target.update(cx, |p, cx| p.completions(false, cx));
            cx.spawn(async move |_| *fill.borrow_mut() = Some(task.await))
                .detach();
        });
        let done = slot.clone();
        self.settle("completions", move |_, _| done.borrow().is_some());
        let found = slot.borrow_mut().take().unwrap_or_default();
        found.into_iter().map(|c| c.insert).collect()
    }
}

fn server_config() -> ConnectionConfig {
    ConnectionConfig::SqlServer(ServerConfig {
        host: "127.0.0.1".into(),
        port: MSSQL_PORT,
        database: "sonde_app".into(),
        user: "sa".into(),
        tls: TlsMode::Prefer,
        options: Default::default(),
    })
}

fn fixture() {
    let config = ConnectionConfig::SqlServer(ServerConfig {
        database: "master".into(),
        ..match server_config() {
            ConnectionConfig::SqlServer(server) => server,
            _ => unreachable!(),
        }
    });
    block_on(async {
        let session = sonde_core::connect(&config, Some(Secret::new("Sonde-pass1")))
            .await
            .expect(
                "the SQL Server fixture is running (docker compose -f tests/docker-compose.yml up -d)",
            );
        for sql in [
            "IF DB_ID('sonde_app') IS NOT NULL BEGIN \
             ALTER DATABASE sonde_app SET SINGLE_USER WITH ROLLBACK IMMEDIATE; \
             DROP DATABASE sonde_app; END",
            "CREATE DATABASE sonde_app",
            "CREATE TABLE sonde_app.dbo.[order] (id INT IDENTITY PRIMARY KEY, \
             customer NVARCHAR(40) NOT NULL, total DECIMAL(10,2), placed DATETIME2(0))",
            "INSERT INTO sonde_app.dbo.[order] (customer, total, placed) VALUES \
             (N'Ada', 12.50, '2024-01-02 03:04:05'), (N'Lin', NULL, NULL)",
            "CREATE TABLE sonde_app.dbo.log (line NVARCHAR(MAX))",
        ] {
            let mut handle = session.execute(sql, &[]).await.expect("fixture SQL runs");
            while handle.pages.next().await.is_some() {}
        }
    });
}

fn check_rows(statements: &[&str]) -> Vec<Vec<Value>> {
    block_on(async {
        let session = sonde_core::connect(&server_config(), Some(Secret::new("Sonde-pass1")))
            .await
            .expect("MySQL");
        let mut rows = Vec::new();
        for sql in statements {
            rows.clear();
            let mut handle = session.execute(sql, &[]).await.expect("check SQL runs");
            while let Some(page) = handle.pages.next().await {
                rows.extend(page.expect("rows").rows);
            }
        }
        rows
    })
}

fn main() -> ExitCode {
    let dir = std::env::temp_dir().join(format!("sonde-mssql-app-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory can be created");
    fixture();
    ConnectionStore::at(dir.join("connections.toml"))
        .save(&[SavedConnection {
            id: "ms".into(),
            name: "Local SQL Server".into(),
            config: server_config(),
        }])
        .expect("the scratch store is writable");
    let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecrets(Mutex::new(HashMap::from([(
        "ms".to_owned(),
        "Sonde-pass1".to_owned(),
    )]))));
    let mut report = Report::default();
    let mut ui = Ui::launch(&dir, secrets, (1280., 820.));

    // ── Sidebar ──────────────────────────────────────────────────────────
    ui.click_label("Local SQL Server");
    let connected = ui.settle("the connection", |window, _| {
        window.try_find("Disconnect").is_some()
    });
    let listed = ui.settle("the database list", |window, _| {
        !window.find_all_by_label("dbo").is_empty()
    });
    report.check(
        "sidebar: a SQL Server connection opens and lists its schemas",
        connected && listed,
    );
    ui.shot("s01-sidebar");

    // ── Query tab ────────────────────────────────────────────────────────
    let query = ui.new_query("Local SQL Server");
    ui.run_sql(
        &query,
        "SELECT customer FROM [order] ORDER BY id\nGO\nSELECT @@VERSION",
    );
    let results = ui.cx.update(|cx| query.read(cx).results().len());
    report.check(
        "query: a script runs batch by batch on SQL Server, GO included",
        results == 2
            && ui
                .cx
                .update(|cx| query.read(cx).results()[0].read(cx).value(0, 0, cx))
                == Some(Value::text("Ada")),
    );
    ui.run_sql(&query, "SELECT 1\nSELECT * FORM [order]");
    let problem = ui
        .cx
        .update(|cx| query.read(cx).problem().map(|p| p.to_string()));
    report.check(
        "query: a syntax error points at its line",
        problem.as_deref().is_some_and(|p| p.starts_with("Line 2")),
    );
    report.check(
        "completion: SQL Server names are quoted with brackets",
        ui.completion_labels(&query, "SELECT * FROM ord|")
            .first()
            .map(String::as_str)
            == Some("[order]"),
    );
    report.check(
        "completion: columns of the named table",
        ui.completion_labels(&query, "SELECT cust| FROM [order]")
            .first()
            .map(String::as_str)
            == Some("customer"),
    );
    ui.shot("s02-query");

    // ── Table tab ────────────────────────────────────────────────────────
    ui.click_label("dbo");
    ui.open_from_menu("order");
    let tab = ui.tables().pop().expect("a table tab opened");
    ui.idle(&tab);
    report.check(
        "table: opens with its rows and is editable on its primary key",
        ui.read(&tab, |tab, cx| tab.rows_loaded(cx)) == 2
            && ui.read(&tab, |tab, _| tab.is_editable()),
    );
    ui.edit(&tab, 1, "total", "99.95");
    ui.edit(&tab, 0, "customer", "Ada L.");
    ui.click("review-changes");
    let preview = ui.read(&tab, |tab, cx| {
        tab.statements(cx)
            .iter()
            .map(|s| s.rendered())
            .collect::<Vec<_>>()
    });
    report.check(
        "table: the review shows SQL Server statements with brackets",
        preview
            .iter()
            .any(|s| s == "UPDATE [dbo].[order] SET [total] = 99.95 WHERE [id] = 2"),
    );
    ui.shot("s03-review");
    ui.click("apply-changes");
    ui.idle(&tab);
    let rows = check_rows(&["SELECT customer, total FROM [order] ORDER BY id"]);
    report.check(
        "table: apply writes the edits",
        rows == vec![
            vec![
                Value::text("Ada L."),
                Value::Decimal("12.50".parse().unwrap()),
            ],
            vec![Value::text("Lin"), Value::Decimal("99.95".parse().unwrap())],
        ],
    );
    // The review dialog animates out; wait until it has gone.
    ui.settle("the review dialog to close", |window, _| {
        window.try_find("apply-changes").is_none()
    });
    ui.act(&tab, |tab, window, cx| tab.add_row(window, cx));
    ui.press("escape");
    ui.edit(&tab, 2, "customer", "Zed");
    ui.click("review-changes");
    ui.click("apply-changes");
    ui.idle(&tab);
    let rows = check_rows(&["SELECT COUNT(*) FROM [order]"]);
    report.check(
        "table: a new row is inserted, the key left to IDENTITY",
        rows == vec![vec![Value::Int(3)]],
    );

    ui.right_click_label("log");
    ui.click_label("Open table");
    let log = ui.tables().pop().expect("the log tab opened");
    ui.idle(&log);
    report.check(
        "table: a table without a primary key is read-only and says why",
        !ui.read(&log, |tab, _| tab.is_editable())
            && ui.read(&log, |tab, _| tab.read_only_reason()).is_some(),
    );
    ui.shot("s04-read-only");

    println!();
    if report.failures == 0 {
        println!("sqlserver walkthrough: all checks passed");
        ExitCode::SUCCESS
    } else {
        println!("sqlserver walkthrough: {} check(s) FAILED", report.failures);
        ExitCode::FAILURE
    }
}
