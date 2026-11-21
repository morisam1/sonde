//! A click-through of browsing without writing queries: the filter
//! builder, foreign-key links, the row form, column controls, Summary mode,
//! saved views, and editing a Redis key. Runs against the Postgres and
//! Redis fixtures.
//!
//! ```sh
//! docker compose -f tests/docker-compose.yml up -d
//! SONDE_SHOTS=/tmp/shots cargo test -p sonde-app --test browse
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
    connections::Connections, layout::LayoutFile, table_view::TableView, workspace::Workspace,
};
use sonde_core::{
    ConnectionConfig, PostgresConfig, Result, Secret, Session, TlsMode, Value,
    config::{ConnectionStore, SavedConnection, SecretStore},
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

fn redis_config() -> ConnectionConfig {
    ConnectionConfig::Redis(PostgresConfig {
        host: "127.0.0.1".into(),
        port: 63799,
        database: "13".into(),
        user: String::new(),
        tls: TlsMode::Disable,
        options: Default::default(),
    })
}

fn open(config: &ConnectionConfig, password: Option<&str>) -> Box<dyn Session> {
    block_on(sonde_core::connect(config, password.map(Secret::new)))
        .expect("the fixture is running")
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
                .unwrap_or_else(|error| panic!("fixture `{statement}` runs: {error}"));
            while let Some(page) = handle.pages.next().await {
                rows.extend(page.expect("fixture SQL runs").rows);
            }
        }
        rows
    })
}

struct DropFixture;

impl Drop for DropFixture {
    fn drop(&mut self) {
        sql(
            &*open(&postgres_config(), Some("sonde")),
            &["drop schema if exists sonde_browse cascade"],
        );
        sql(&*open(&redis_config(), None), &["FLUSHDB"]);
    }
}

fn main() -> ExitCode {
    let dir = std::env::temp_dir().join(format!("sonde-browse-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory can be created");

    let pg = open(&postgres_config(), Some("sonde"));
    sql(
        &*pg,
        &[
            "drop schema if exists sonde_browse cascade",
            "create schema sonde_browse",
            "create table sonde_browse.customers (id int primary key, name text not null, email text, country char(2), joined date)",
            "insert into sonde_browse.customers
             select g, 'Customer ' || g, 'c' || g || '@example.com',
                    (array['PT','DE','JP'])[1 + g % 3], date '2024-01-01' + 2 * g
             from generate_series(1, 30) g",
            "create table sonde_browse.orders (id int primary key, customer_id int references sonde_browse.customers(id), total numeric(10,2), placed date)",
            "insert into sonde_browse.orders
             select g, 1 + g % 10, g * 1.5, date '2024-01-01' + g * 7
             from generate_series(1, 60) g",
        ],
    );
    let redis = open(&redis_config(), None);
    sql(
        &*redis,
        &[
            "FLUSHDB",
            "HSET profile name ada lang en",
            "SET greeting hello",
        ],
    );
    let _fixture = DropFixture;

    ConnectionStore::at(dir.join("connections.toml"))
        .save(&[
            SavedConnection {
                id: "pg".into(),
                name: "Local Postgres".into(),
                config: postgres_config(),
            },
            SavedConnection {
                id: "kv".into(),
                name: "Local Redis".into(),
                config: redis_config(),
            },
        ])
        .expect("the scratch store is writable");
    let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecrets(Mutex::new(HashMap::from([(
        "pg".to_owned(),
        "sonde".to_owned(),
    )]))));

    let mut report = Report::default();
    let mut ui = Ui::launch(&dir, Arc::clone(&secrets), (1400., 900.));

    ui.click_label("Local Postgres");
    ui.settle("the schema list", |window, _| {
        window.try_find_by_label("sonde_browse").is_some()
    });
    ui.scroll_tree_until("sonde_browse");
    ui.click_label("sonde_browse");
    ui.settle("the table list", |window, _| {
        window.try_find_by_label("customers").is_some()
    });
    ui.scroll_tree_until("customers");
    ui.click_label_times("customers", 2);
    let Some(customers) = ui.tables().pop() else {
        report.check("customers opens", false);
        return ExitCode::FAILURE;
    };
    ui.idle(&customers);
    report.check(
        "rows: all 30 customers load",
        ui.read(&customers, |t, cx| t.rows_loaded(cx)) == 30,
    );

    // ── The filter builder ──────────────────────────────────────────────
    report.check(
        "builder: the toolbar offers rules, not a WHERE box",
        ui.has("filter-rules"),
    );
    ui.act(&customers, |t, window, cx| {
        t.add_rule(Some("country"), window, cx)
    });
    let builder = ui
        .read(&customers, |t, _| t.builder().cloned())
        .expect("a builder");
    {
        let builder = builder.clone();
        ui.with(|window, cx| builder.update(cx, |b, cx| b.set_value(0, "PT", window, cx)));
    }
    ui.frame();
    ui.click("filter-apply");
    ui.idle(&customers);
    let countries: Vec<Option<Value>> = (0..ui.read(&customers, |t, cx| t.rows_loaded(cx)))
        .map(|row| ui.cell(&customers, row, "country"))
        .collect();
    report.check(
        "builder: country is PT keeps only the 10 Portuguese customers",
        countries.len() == 10
            && countries
                .iter()
                .all(|c| c.as_ref().is_some_and(|v| v.to_string() == "PT")),
    );
    report.check(
        "builder: the condition it ran is shown",
        ui.label_of("filter-preview")
            .is_some_and(|p| p.contains("\"country\" = 'PT'")),
    );
    ui.act(&customers, |t, window, cx| {
        t.add_rule(Some("joined"), window, cx)
    });
    {
        let builder = builder.clone();
        ui.with(|window, cx| {
            builder.update(cx, |b, cx| {
                b.set_op(1, sonde_core::filter::Op::Before, window, cx);
                b.set_value(1, "2024-01-15", window, cx);
            })
        });
    }
    ui.click("filter-apply");
    ui.idle(&customers);
    report.check(
        "builder: a second rule on a date narrows further (PT and joined before 15 Jan)",
        ui.read(&customers, |t, cx| t.rows_loaded(cx)) == 2,
    );
    ui.shot("b01-builder");
    {
        let builder = builder.clone();
        ui.with(|window, cx| builder.update(cx, |b, cx| b.set_value(1, "soon", window, cx)));
    }
    ui.frame();
    report.check(
        "builder: a value that does not fit is caught before it runs",
        ui.label_of("filter-preview")
            .is_some_and(|p| p.contains("needs a date")),
    );
    ui.click("filter-clear");
    ui.idle(&customers);
    report.check(
        "builder: Clear brings every row back",
        ui.read(&customers, |t, cx| t.rows_loaded(cx)) == 30,
    );

    // ── Foreign keys, both ways ─────────────────────────────────────────
    ui.settle("the keys pointing at customers", {
        let customers = customers.clone();
        move |_, cx| {
            customers
                .read(cx)
                .referenced_by()
                .contains(&"orders".to_owned())
        }
    });
    report.check(
        "links: orders is found pointing at customers",
        ui.read(&customers, |t, _| t.referenced_by()) == ["orders"],
    );
    // Row 2 is customer 3.
    ui.act(&customers, |t, _, cx| {
        t.show_referencing_rows(2, "orders", cx)
    });
    let orders = ui
        .tables()
        .into_iter()
        .find(|t| ui.read(t, |t, _| t.name()) == "orders");
    let Some(orders) = orders else {
        report.check("links: Referenced by opens orders", false);
        return ExitCode::FAILURE;
    };
    ui.idle(&orders);
    let owners: Vec<Option<Value>> = (0..ui.read(&orders, |t, cx| t.rows_loaded(cx)))
        .map(|row| ui.cell(&orders, row, "customer_id"))
        .collect();
    report.check(
        "links: \"orders rows pointing here\" opens the 6 orders of customer 3",
        owners.len() == 6 && owners.iter().all(|o| o == &Some(Value::Int(3))),
    );
    ui.shot("b02-referencing");
    let fk = ui
        .read(&orders, |t, _| t.column("customer_id"))
        .expect("customer_id");
    ui.act(&orders, |t, _, cx| t.follow_cell(0, fk, cx));
    ui.idle(&customers);
    report.check(
        "links: following customer_id goes back to customer 3 alone",
        ui.read(&customers, |t, cx| t.rows_loaded(cx)) == 1
            && ui.cell(&customers, 0, "id") == Some(Value::Int(3)),
    );
    ui.act(&customers, |t, window, cx| t.clear_filter(window, cx));
    ui.idle(&customers);

    // ── Columns ─────────────────────────────────────────────────────────
    ui.act(&customers, |t, _, cx| {
        let mut layout = sonde_app::table_view::columns::ColumnLayout::default();
        layout.toggle_hidden("email");
        layout.toggle_pinned("name");
        t.set_layout(layout, cx);
    });
    let shown = ui.read(&customers, |t, cx| t.shown_columns(cx));
    report.check(
        "columns: email hidden and name pinned first",
        shown == [1, 0, 3, 4],
    );
    let remembered = std::fs::read_to_string(dir.join("tables.toml")).unwrap_or_default();
    report.check(
        "columns: the layout is remembered in tables.toml",
        remembered.contains("customers") && remembered.contains("email"),
    );
    ui.act(&customers, |t, window, cx| t.open_columns(window, cx));
    report.check(
        "columns: the Columns dialog lists every column",
        ui.has("columns-list"),
    );
    ui.shot("b03-columns");
    ui.click("columns-done");
    ui.act(&customers, |t, window, cx| t.select_cell(0, 1, window, cx));
    report.check(
        "columns: cells keep their own column when the order changes",
        ui.read(&customers, |t, cx| t.value(0, 1, cx)) == Some(Value::text("Customer 1")),
    );

    // ── The row form ────────────────────────────────────────────────────
    ui.act(&customers, |t, window, cx| t.open_row_form(0, window, cx));
    let form = ui.read(&customers, |t, _| t.form().cloned());
    report.check(
        "form: a row opens as a form",
        form.is_some() && ui.has("row-form-fields"),
    );
    ui.shot("b04-form");
    if let Some(form) = form {
        ui.with(|window, cx| {
            form.update(cx, |f, cx| f.set_text("name", "Ada Lovelace", window, cx))
        });
        ui.with(|window, cx| form.update(cx, |f, cx| f.stage(window, cx)));
        ui.frame();
    }
    report.check(
        "form: Stage changes stages one change",
        ui.read(&customers, |t, cx| t.staged(cx)) == 1,
    );
    ui.act(&customers, |t, _, cx| t.apply(cx));
    ui.idle(&customers);
    report.check(
        "form: the change is written",
        sql(
            &*pg,
            &["select name from sonde_browse.customers where id = 1"],
        ) == [vec![Value::text("Ada Lovelace")]],
    );

    // ── Summary ─────────────────────────────────────────────────────────
    ui.idle(&customers);
    // The toolbar is disabled for a frame or two while the applied rows
    // reload, so the click is repeated until it lands.
    for _ in 0..20 {
        ui.click("mode-summary");
        if ui.read(&customers, |t, _| t.summary_panel().is_some()) {
            break;
        }
        ui.cx.advance_clock(Duration::from_millis(50));
        thread::sleep(Duration::from_millis(20));
    }
    let summary = ui.read(&customers, |t, _| t.summary_panel().cloned());
    report.check("summary: Summary mode opens", summary.is_some());
    if let Some(summary) = summary {
        {
            let summary = summary.clone();
            ui.with(|window, cx| summary.update(cx, |s, cx| s.group_by("country", window, cx)));
        }
        ui.settle("the summary", {
            let summary = summary.clone();
            move |_, cx| {
                summary.read(cx).results().is_some() || summary.read(cx).problem().is_some()
            }
        });
        let results = ui
            .cx
            .update(|cx| summary.read(cx).results())
            .unwrap_or_default();
        report.check(
            "summary: counting by country gives three groups of ten",
            results.len() == 3 && results.iter().all(|(_, count)| count == "10"),
        );
        ui.shot("b05-summary");
        {
            let summary = summary.clone();
            ui.with(|window, cx| summary.update(cx, |s, cx| s.group_by("joined", window, cx)));
        }
        ui.settle("the monthly summary", {
            let summary = summary.clone();
            move |_, cx| {
                summary.read(cx).results().is_some_and(|r| r.len() == 3)
                    || summary.read(cx).problem().is_some()
            }
        });
        report.check(
            "summary: a date column groups by month (January to March)",
            ui.cx
                .update(|cx| summary.read(cx).results())
                .is_some_and(|r| r.len() == 3),
        );
        ui.click("summary-open-query");
        let workspace = ui.workspace.clone();
        let queries = ui.cx.update(|cx| workspace.read(cx).queries().len());
        report.check("summary: Open as query opens a query tab", queries == 1);
    }
    // Back to the customers tab, in Rows mode.
    ui.click_label("customers tab");
    ui.click("mode-rows");
    ui.idle(&customers);

    // ── Saved views ─────────────────────────────────────────────────────
    ui.act(&customers, |t, window, cx| {
        t.add_rule(Some("country"), window, cx)
    });
    {
        let builder = builder.clone();
        ui.with(|window, cx| builder.update(cx, |b, cx| b.set_value(0, "JP", window, cx)));
    }
    ui.click("filter-apply");
    ui.idle(&customers);
    ui.click("save-view");
    report.check("views: Save view asks for a name", ui.has("query-name"));
    ui.with(|window, cx| {
        window.click("query-name", cx);
        window.press("ctrl-a", cx);
        window.press("backspace", cx);
        window.input("Japanese customers", cx);
    });
    ui.frame();
    ui.click("query-name-save");
    let workspace = ui.workspace.clone();
    let views = ui.cx.update(|cx| workspace.read(cx).saved_views().to_vec());
    report.check(
        "views: the view is saved and listed in the sidebar",
        views.len() == 1 && views[0].name == "Japanese customers" && ui.has("saved-views"),
    );
    ui.act(&customers, |t, window, cx| t.clear_filter(window, cx));
    ui.idle(&customers);
    if let Some(view) = views.first() {
        let path = view.path.clone();
        let workspace = ui.workspace.clone();
        ui.with(|window, cx| workspace.update(cx, |w, cx| w.open_view(&path, window, cx)));
        ui.idle(&customers);
        report.check(
            "views: opening it brings back the filter (10 Japanese customers) and the columns",
            ui.read(&customers, |t, cx| t.rows_loaded(cx)) == 10
                && ui.read(&customers, |t, cx| t.shown_columns(cx)) == [1, 0, 3, 4],
        );
        ui.shot("b06-saved-view");
    }

    // ── Redis: edit a hash, and its expiry ──────────────────────────────
    ui.scroll_tree_to_top();
    ui.click_label("Local Redis");
    ui.settle("the Redis database", |window, _| {
        window.try_find_by_label("db13").is_some() || window.try_find_by_label("profile").is_some()
    });
    if ui.with(|window, _| window.try_find_by_label("profile").is_none()) {
        ui.click_label("db13");
    }
    ui.settle("the Redis keys", |window, _| {
        window.try_find_by_label("greeting").is_some()
    });
    ui.scroll_tree_until("profile");
    // The overview lists the key too; the sidebar's row is the last match.
    let last = ui.with(|window, _| window.find_all_by_label("profile").len().saturating_sub(1));
    ui.click_nth_label("profile", last, 2);
    let profile = ui
        .tables()
        .into_iter()
        .find(|t| ui.read(t, |t, _| t.name()) == "profile");
    if let Some(profile) = profile {
        ui.idle(&profile);
        report.check(
            "redis: a hash opens editable",
            ui.read(&profile, |t, _| t.is_editable()),
        );
        ui.settle("the expiry", |window, _| {
            window
                .try_find("key-expiry")
                .and_then(|n| n.label().map(str::to_owned))
                .is_some_and(|l| l == "Never expires")
        });
        report.check(
            "redis: the key's expiry is shown",
            ui.label_of("key-expiry").as_deref() == Some("Never expires"),
        );
        let row = (0..2)
            .find(|row| ui.cell(&profile, *row, "field") == Some(Value::text("lang")))
            .unwrap_or(0);
        ui.edit(&profile, row, "value", "pt");
        ui.act(&profile, |t, _, cx| t.apply(cx));
        ui.idle(&profile);
        report.check(
            "redis: an edited field is written with HSET",
            sql(&*redis, &["HGET profile lang"]) == [vec![Value::text("pt")]],
        );
        ui.act(&profile, |t, _, cx| t.set_expiry(Some(600), cx));
        ui.settle("the new expiry", |window, _| {
            window
                .try_find("key-expiry")
                .and_then(|n| n.label().map(str::to_owned))
                .is_some_and(|l| l.starts_with("Expires in"))
        });
        report.check(
            "redis: Set expiry gives the key a TTL",
            ui.label_of("key-expiry")
                .is_some_and(|l| l.starts_with("Expires in")),
        );
        ui.shot("b07-redis");
    } else {
        report.check("redis: the key opens", false);
    }

    if report.failures == 0 {
        println!("\nPASS every check");
        ExitCode::SUCCESS
    } else {
        println!("\nFAIL {} checks failed", report.failures);
        ExitCode::FAILURE
    }
}
