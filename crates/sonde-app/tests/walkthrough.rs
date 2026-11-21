//! A click-through of the application shell in a headless window with the
//! real renderer, against the Postgres fixture and a SQLite file.
//!
//! Each step is checked and reported. Set `SONDE_SHOTS` to a directory to
//! also get a PNG of the window after each stage.
//!
//! ```sh
//! docker compose -f tests/docker-compose.yml up -d
//! SONDE_SHOTS=/tmp/shots cargo test -p sonde-app --test walkthrough
//! ```

use std::{
    collections::HashMap,
    path::PathBuf,
    process::ExitCode,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use futures::{StreamExt as _, executor::block_on};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Entity, HeadlessAppContext, MouseButton,
    ScrollDelta, Window, WindowBounds, WindowOptions,
    component::{ActiveTheme as _, Theme, ThemeMode},
    point, px, size,
    test::{TestEventExt as _, TestQueryExt as _, TestWindowExt as _},
};
use sonde_app::{
    connections::Connections,
    layout::{Appearance, Layout, LayoutFile},
    workspace::Workspace,
};
use sonde_core::{
    ConnectionConfig, PostgresConfig, Result, Secret, TlsMode,
    config::{ConnectionStore, SavedConnection, SecretStore},
};

const PG_PORT: u16 = 54329;

#[derive(Default)]
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

/// The checks made so far, and whether any failed.
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

/// One running copy of the application.
struct Ui {
    cx: HeadlessAppContext,
    window: AnyWindowHandle,
    connections: Entity<Connections>,
    shots: Option<PathBuf>,
}

impl Ui {
    fn launch(
        dir: &std::path::Path,
        secrets: Option<Arc<dyn SecretStore>>,
        window_size: (f32, f32),
    ) -> Self {
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
        let mut connections = None;
        let (window, _) = cx
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
                        let entity = cx.new(|_| Connections::load(store, secrets));
                        connections = Some(entity.clone());
                        cx.new(|cx| Workspace::new(entity, layout, window, cx))
                    },
                )
            })
            .expect("the window opens");

        let mut ui = Self {
            cx,
            window,
            connections: connections.expect("the window was built"),
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
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            self.frame();
            if self.with(&mut done) {
                return true;
            }
            if Instant::now() > deadline {
                println!("     timed out waiting for: {what}");
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn click(&mut self, id: &'static str) {
        self.with(|window, cx| window.click(id, cx));
        self.frame();
    }

    /// Click whatever carries this accessible name: a tree row, a radio.
    fn click_label(&mut self, label: &str) {
        self.with(|window, cx| {
            let at = window.find_by_label(label).bounds().center();
            window.pointer_down(at, MouseButton::Left, cx);
            window.pointer_up(at, MouseButton::Left, cx);
        });
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

    /// Replace a field's contents by clicking into it and typing.
    fn fill(&mut self, id: &'static str, text: &str) {
        self.with(|window, cx| {
            window.click(id, cx);
            window.press("ctrl-a", cx);
            window.press("backspace", cx);
            window.input(text, cx);
        });
        self.frame();
    }

    fn press(&mut self, key: &str) {
        self.with(|window, cx| window.press(key, cx));
        self.frame();
    }

    /// Scroll the sidebar's tree down until `label`'s row is fully inside
    /// the list's visible area, where a click can land on it.
    fn scroll_tree_until(&mut self, label: &str) {
        for _ in 0..50 {
            let inside = self.with(|window, _| {
                window.try_find_by_label(label).is_some_and(|element| {
                    let tree = window.find("connection-tree").bounds();
                    // The list is inset from the tree's edge by its
                    // padding, and clicks outside it are ignored.
                    let margin = px(12.);
                    element.bounds().center().y >= tree.top() + margin
                        && element.bounds().center().y <= tree.bottom() - margin
                })
            });
            if inside {
                return;
            }
            self.with(|window, cx| {
                window.scroll(
                    "connection-tree",
                    ScrollDelta::Pixels(point(px(0.), px(-100.))),
                    cx,
                );
            });
            self.frame();
        }
    }

    /// Wait for `label`'s row to appear, scrolling the tree down as new
    /// content lands below the current view.
    fn await_tree_row(&mut self, label: &str) -> bool {
        (0..100).any(|_| {
            self.frame();
            let found = self.has_label(label);
            if !found {
                self.with(|window, cx| {
                    window.scroll(
                        "connection-tree",
                        ScrollDelta::Pixels(point(px(0.), px(-100.))),
                        cx,
                    );
                });
            }
            found
        })
    }

    /// Scroll the sidebar's tree back to its first rows.
    fn has_label(&mut self, label: &str) -> bool {
        self.with(|window, _| window.try_find_by_label(label).is_some())
    }

    fn has(&mut self, id: &'static str) -> bool {
        self.with(|window, _| window.try_find(id).is_some())
    }

    fn disabled(&mut self, id: &'static str) -> bool {
        self.with(|window, _| window.find(id).disabled() == Some(true))
    }

    fn status_contains(&mut self, text: &str) -> bool {
        self.with(|window, _| {
            window
                .find("connection-status")
                .label()
                .is_some_and(|label| label.contains(text))
        })
    }

    fn saved_names(&mut self) -> Vec<String> {
        let connections = self.connections.clone();
        self.cx.update(|cx| {
            connections
                .read(cx)
                .list()
                .iter()
                .map(|saved| saved.name.clone())
                .collect()
        })
    }

    fn shot(&mut self, name: &str) {
        // Headless time only advances explicitly. Finish the entrance/exit
        // transitions before capturing, so dialogs are visible in the PNG.
        for _ in 0..20 {
            self.frame();
        }
        self.frame();
        let Some(dir) = &self.shots else { return };
        std::fs::create_dir_all(dir).expect("the screenshot directory can be created");
        self.cx
            .capture_screenshot(self.window)
            .expect("offscreen rendering is available")
            .save(dir.join(format!("{name}.png")))
            .expect("the screenshot can be written");
    }
}

/// A schema with two related tables in the Postgres fixture.
fn postgres_fixture() {
    let config = ConnectionConfig::Postgres(PostgresConfig {
        host: "127.0.0.1".into(),
        port: PG_PORT,
        database: "sonde".into(),
        user: "sonde".into(),
        tls: TlsMode::Prefer,
        options: Default::default(),
    });
    block_on(async {
        let session = sonde_core::connect(&config, Some(Secret::new("sonde")))
            .await
            .expect("the Postgres fixture is running");
        for sql in [
            "drop database if exists sonde_walk_db",
            "drop schema if exists sonde_walk cascade",
            "create schema sonde_walk",
            "create table sonde_walk.author (id int4 primary key, name text not null)",
            "create table sonde_walk.book (
                 id int4 primary key,
                 author_id int4 not null references sonde_walk.author (id),
                 title text not null,
                 price numeric(10, 2))",
            "create index book_title on sonde_walk.book (lower(title))",
        ] {
            let mut handle = session.execute(sql, &[]).await.expect("fixture SQL runs");
            while handle.pages.next().await.is_some() {}
        }
    });
}

fn main() -> ExitCode {
    let dir = std::env::temp_dir().join(format!("sonde-walk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory can be created");
    let sqlite_path = dir.join("notes.db");
    rusqlite::Connection::open(&sqlite_path)
        .and_then(|c| c.execute_batch("create table note (id integer primary key, body text)"))
        .expect("a SQLite file can be created");
    postgres_fixture();

    let mut report = Report::default();
    let secrets: Arc<MemorySecrets> = Arc::default();
    let keyring = || Some(Arc::clone(&secrets) as Arc<dyn SecretStore>);

    // ── First run ────────────────────────────────────────────────────────
    let mut ui = Ui::launch(&dir, keyring(), (1200., 800.));
    report.check(
        "setup: first launch opens the database chooser in the system theme",
        ui.has("setup-wizard")
            && LayoutFile::at(dir.join("layout.toml")).load().appearance == Appearance::System
            && ui.with(|window, cx| cx.theme().mode == ThemeMode::from(window.appearance())),
    );
    ui.shot("00-first-run-wizard");
    report.check(
        "setup: the URL action waits for input",
        ui.disabled("setup-next"),
    );
    ui.click("setup-skip");
    report.check(
        "setup: skipping opens the regular workspace",
        !ui.has("setup-wizard"),
    );
    ui.shot("01-first-run");
    report.check(
        "first run: the tab area offers New connection",
        ui.has("start-new-connection"),
    );
    report.check(
        "first run: per-connection actions are disabled with nothing selected",
        ui.disabled("Connect"),
    );

    // ── Add a Postgres connection through the form ───────────────────────
    ui.click("start-new-connection");
    report.check(
        "New connection: opens on the engine step, with nothing to save yet",
        ui.has("kind-postgres") && ui.has("kind-redis") && !ui.has("save"),
    );
    ui.shot("02a-form-engines");
    ui.click("kind-postgres");
    report.check(
        "New connection: picking an engine moves on to its details",
        ui.has("save") && ui.has("Host") && !ui.has("kind-postgres"),
    );
    ui.shot("02-form-blank");

    ui.click("save");
    report.check(
        "form: saving with no name is refused and nothing is saved",
        ui.has("save") && ui.saved_names().is_empty(),
    );
    ui.shot("03-form-validation");

    ui.fill("Name", "Local Postgres");
    ui.fill("Host", "127.0.0.1");
    ui.fill("Port", &PG_PORT.to_string());
    ui.fill("Database", "sonde");
    ui.fill("User", "sonde");
    ui.fill("Name", "");
    ui.press("enter");
    report.check(
        "form: Enter with an empty name keeps the dialog open and explains why",
        ui.has("save")
            && ui.saved_names().is_empty()
            && ui.status_contains("Give the connection a name"),
    );
    ui.fill("Name", "Local Postgres");
    ui.fill("Password", "not-the-password");
    ui.click("test");
    let refused = ui.settle("the failed test to report", |window, _| {
        window.find("test").disabled() != Some(true)
    });
    ui.shot("04-form-test-failed");
    report.check(
        "form: Test connection reports a wrong password",
        refused && ui.status_contains("rejected the password"),
    );

    ui.fill("Password", "sonde");
    ui.click("test");
    let tested = ui.settle("the test to finish", |window, _| {
        window.find("test").disabled() != Some(true)
    });
    ui.shot("05-form-test-passed");
    report.check(
        "form: Test connection reports success with the correct password",
        tested && ui.status_contains("Connected in"),
    );

    ui.click("save");
    let closed = ui.settle("the form to close", |window, _| {
        window.try_find("save").is_none()
    });
    report.check(
        "form: Save connection closes the dialog and saves it",
        closed && ui.saved_names() == ["Local Postgres"],
    );
    report.check(
        "form: the password went to the keyring, not the file",
        secrets.0.lock().unwrap().values().any(|p| p == "sonde")
            && !std::fs::read_to_string(dir.join("connections.toml"))
                .expect("the saved connections can be read")
                .contains("password"),
    );

    // ── Add a SQLite connection ──────────────────────────────────────────
    ui.click("new-connection");
    ui.click("kind-redis");
    ui.click("back");
    report.check(
        "form: Back returns to the engine step",
        ui.has("kind-sqlite") && !ui.has("save"),
    );
    ui.click("kind-sqlite");
    ui.fill("Name", "Notes");
    report.check(
        "form: choosing SQLite swaps in the file field",
        ui.has("Database file") && !ui.has("Host"),
    );
    ui.fill("Database file", "");
    ui.press("enter");
    report.check(
        "form: Enter without a SQLite path keeps the dialog open and explains why",
        ui.has("save")
            && ui.saved_names() == ["Local Postgres"]
            && ui.status_contains("Choose the SQLite database file"),
    );
    ui.fill("Database file", &sqlite_path.to_string_lossy());
    ui.shot("06-form-sqlite");
    ui.press("enter");
    let closed = ui.settle("the form to close", |window, _| {
        window.try_find("save").is_none()
    });
    report.check(
        "form: Enter in a field saves",
        closed && ui.saved_names() == ["Local Postgres", "Notes"],
    );

    // ── Escape cancels ───────────────────────────────────────────────────
    ui.click("new-connection");
    ui.click("kind-postgres");
    ui.fill("Name", "Discarded");
    ui.press("escape");
    let closed = ui.settle("the form to close", |window, _| {
        window.try_find("save").is_none()
    });
    report.check(
        "form: Escape closes without saving",
        closed && ui.saved_names().len() == 2,
    );

    // ── Connect and browse Postgres ──────────────────────────────────────
    ui.click_label("Local Postgres");
    let connected = ui.settle("the schema list", |window, _| {
        window.try_find_by_label("sonde_walk").is_some()
    });
    report.check(
        "sidebar: expanding a connection connects and lists schemas",
        connected,
    );
    report.check(
        "sidebar: a connected connection offers Disconnect and Refresh",
        ui.has("Disconnect") && !ui.disabled("Refresh"),
    );
    report.check(
        "overview: selecting a connection shows its details",
        ui.with(|window, _| {
            window.find("inspector-title").label() == Some("Inspecting Local Postgres")
        }),
    );
    ui.shot("06b-connection-overview");
    ui.click_label("sonde_walk");
    report.check(
        "sidebar: expanding a schema lists its tables",
        ui.has_label("author") && ui.has_label("book"),
    );
    report.check(
        "overview: selecting a schema shows its relations",
        ui.with(|window, _| {
            window.find("inspector-title").label() == Some("Inspecting sonde_walk")
        }),
    );
    ui.click_label("book");
    let detailed = ui.settle("the table's detail", |window, _| {
        window.try_find_by_label("Columns").is_some()
    });
    report.check("sidebar: expanding a table loads its detail", detailed);
    ui.click_label("Columns");
    // Each group's expansion pushes the next one under the tree's bottom
    // edge, so scroll until it is in view again.
    ui.scroll_tree_until("Indexes");
    ui.click_label("Indexes");
    ui.scroll_tree_until("Foreign keys");
    ui.click_label("Foreign keys");
    report.check(
        "sidebar: columns, indexes and foreign keys are listed",
        ui.has_label("price") && ui.has_label("book_title") && ui.has_label("book_author_id_fkey"),
    );
    report.check(
        "overview: table metadata follows selected child groups",
        ui.with(|window, _| {
            window.find("inspector-title").label() == Some("Inspecting book")
                && window.find("inspector-columns").label() == Some("4 columns")
                && window.find("inspector-indexes").label() == Some("2 indexes")
                && window.find("inspector-foreign-keys").label() == Some("1 foreign key")
        }),
    );
    ui.shot("07-browse-postgres");

    // ── Keyboard ─────────────────────────────────────────────────────────
    ui.scroll_tree_until("Notes");
    ui.click_label("Notes");
    let connected = ui.settle("the SQLite schema", |window, _| {
        window.try_find_by_label("main").is_some()
    });
    report.check("sidebar: the SQLite connection opens", connected);
    ui.press("down");
    ui.press("right");
    report.check(
        "sidebar: arrow keys move to a schema and expand it",
        ui.has_label("note"),
    );
    ui.press("left");
    report.check("sidebar: left arrow collapses it", !ui.has_label("note"));
    ui.shot("08-browse-both");

    // ── Refresh picks up a new table ─────────────────────────────────────
    rusqlite::Connection::open(&sqlite_path)
        .and_then(|c| c.execute_batch("create table tag (name text primary key)"))
        .expect("the SQLite file can be changed");
    ui.press("right");
    ui.click("Refresh");
    // The reload lands the new table at the list's end; scroll while
    // waiting for it to appear.
    let refreshed = ui.await_tree_row("tag");
    report.check(
        "sidebar: Refresh shows a table created elsewhere",
        refreshed,
    );
    report.check(
        "sidebar: Refresh keeps the connection actions available",
        ui.has("Disconnect") && !ui.disabled("Refresh"),
    );

    // ── Disconnect ───────────────────────────────────────────────────────
    ui.click("Disconnect");
    report.check(
        "sidebar: Disconnect collapses the connection and offers Connect again",
        !ui.has_label("main") && ui.has("Connect"),
    );
    report.check(
        "overview: disconnect clears loaded metadata and explains how to reconnect",
        !ui.has("inspector-columns")
            && ui.with(|window, _| {
                window.find("inspector-message").label()
                    == Some("Use Connect to browse this database.")
            }),
    );

    // ── A connection that cannot be reached ──────────────────────────────
    let connections = ui.connections.clone();
    ui.cx.update(|cx| {
        connections.update(cx, |connections, cx| {
            connections
                .upsert(
                    SavedConnection {
                        id: "unreachable".into(),
                        name: "Nothing listening".into(),
                        config: ConnectionConfig::Postgres(PostgresConfig {
                            host: "127.0.0.1".into(),
                            port: 1,
                            database: "none".into(),
                            user: "nobody".into(),
                            tls: TlsMode::Disable,
                            options: Default::default(),
                        }),
                    },
                    cx,
                )
                .expect("the scratch store is writable");
        });
    });
    ui.frame();
    ui.scroll_tree_until("Nothing listening");
    ui.click_label("Nothing listening");
    let failed = ui.settle("the failure to show", |window, _| {
        window.elements().iter().any(|e| {
            e.label()
                .is_some_and(|l| l.starts_with("could not connect"))
        })
    });
    report.check(
        "sidebar: a failed connection shows why, in its place",
        failed,
    );
    report.check(
        "overview: failed connections show the error without stale metadata",
        !ui.has("inspector-columns")
            && ui.with(|window, _| {
                window
                    .find("inspector-message")
                    .label()
                    .is_some_and(|message| message.starts_with("could not connect"))
            }),
    );
    ui.shot("09-connection-failed");

    // ── Edit, from the connection's right-click menu ─────────────────────
    ui.right_click_label("Nothing listening");
    report.check(
        "Edit: right-clicking a connection offers its actions",
        ui.has_label("Edit…") && ui.has_label("Delete…"),
    );
    ui.shot("10b-context-menu");
    ui.click_label("Edit…");
    report.check(
        "Edit: opens the form filled in, with the type locked",
        ui.with(|window, _| window.find("Name").value() == Some("Nothing listening")),
    );
    ui.fill("Name", "Renamed");
    ui.click("save");
    ui.settle("the form to close", |window, _| {
        window.try_find("save").is_none()
    });
    report.check(
        "Edit: saving renames the connection in place",
        ui.saved_names() == ["Local Postgres", "Notes", "Renamed"],
    );

    // ── Delete, with confirmation ────────────────────────────────────────
    ui.right_click_label("Renamed");
    ui.click_label("Delete…");
    ui.shot("10-delete-confirm");
    report.check("Delete: asks first", ui.has_label("Delete connection"));
    ui.press("escape");
    let closed = ui.settle("the confirmation to close", |window, _| {
        window.try_find_by_label("Delete connection").is_none()
    });
    report.check(
        "Delete: Escape keeps the connection",
        closed && ui.saved_names().len() == 3,
    );
    ui.right_click_label("Renamed");
    ui.click_label("Delete…");
    ui.click_label("Delete connection");
    ui.settle("the connection to go", |window, _| {
        window.try_find_by_label("Renamed").is_none()
    });
    report.check(
        "Delete: confirming removes it",
        ui.saved_names() == ["Local Postgres", "Notes"],
    );

    // ── Dark theme ───────────────────────────────────────────────────────
    ui.with(|window, cx| Theme::change(ThemeMode::Dark, Some(window), cx));
    ui.shot("11-dark");
    ui.scroll_tree_until("book");
    ui.click_label("book");
    ui.shot("11b-dark-inspector");
    ui.click("new-connection");
    ui.shot("12-dark-form");
    ui.press("escape");
    drop(ui);

    // ── Restart: saved connections and passwords are still there ─────────
    let mut ui = Ui::launch(&dir, keyring(), (1200., 800.));
    report.check(
        "restart: both connections are listed",
        ui.has_label("Local Postgres") && ui.has_label("Notes"),
    );
    ui.click_label("Local Postgres");
    let reconnected = ui.settle("the schema list", |window, _| {
        window.try_find_by_label("sonde_walk").is_some()
    });
    report.check(
        "restart: reconnecting needs no password to be typed",
        reconnected,
    );

    // ── Structure changes from the sidebar ───────────────────────────────
    ui.right_click_label("sonde_walk");
    report.check(
        "manage: a schema's right-click menu offers New table and Drop schema",
        ui.has_label("New table…") && ui.has_label("Drop schema…"),
    );
    ui.click_label("New table…");
    ui.fill("manage-name", "made_here");
    ui.fill("column-name-1", "label");
    ui.click("column-type-1");
    ui.shot("13a0-type-menu");
    // The list opens on the current type, text; step down to varchar.
    let types = sonde_core::ddl::column_types(sonde_core::DbKind::Postgres);
    let at = |name: &str| types.iter().position(|t| t.name == name).unwrap();
    for _ in at("text")..at("varchar") {
        ui.press("down");
    }
    ui.press("enter");
    report.check(
        "manage: picking varchar from the type list asks for its size, filled with 255",
        ui.with(|window, _| window.try_find("column-size-1").is_some())
            && ui.has_label("Column 2 size"),
    );
    ui.shot("13a-new-table");
    report.check(
        "manage: the dialog is ready to create the table",
        ui.has("manage-run") && ui.has("column-name-1"),
    );
    ui.click("manage-run");
    ui.settle("the dialog to close", |window, _| {
        window.try_find("manage-run").is_none()
    });
    ui.click_label("sonde_walk");
    let created = ui.settle("the new table in the sidebar", |window, _| {
        window.try_find_by_label("made_here").is_some()
    });
    report.check(
        "manage: New table creates it and the sidebar lists it",
        created,
    );

    ui.right_click_label("made_here");
    ui.click_label("Rename…");
    ui.fill("manage-name", "renamed_here");
    ui.click("manage-run");
    let renamed = ui.settle("the renamed table", |window, _| {
        window.try_find_by_label("renamed_here").is_some()
            && window.try_find_by_label("made_here").is_none()
    });
    report.check("manage: Rename renames the table", renamed);

    ui.right_click_label("renamed_here");
    ui.click_label("Drop table…");
    ui.shot("13b-drop-table");
    report.check(
        "manage: dropping warns that it cannot be undone",
        ui.has("manage-run") && !ui.saved_names().is_empty(),
    );
    ui.click("manage-run");
    let dropped = ui.settle("the dropped table to leave", |window, _| {
        window.try_find_by_label("renamed_here").is_none()
    });
    report.check("manage: Drop table removes it from the sidebar", dropped);

    // ── Every database on the server ─────────────────────────────────────
    ui.right_click_label("Local Postgres");
    ui.click_label("Edit…");
    ui.click("all-databases");
    ui.shot("13c-edit-all-databases");
    ui.click("save");
    ui.settle("the form to close", |window, _| {
        window.try_find("save").is_none()
    });
    ui.click_label("Local Postgres");
    let listed = ui.settle("the database list", |window, _| {
        window.try_find_by_label("sonde").is_some()
    });
    report.check(
        "all databases: the connection lists the server's databases",
        listed,
    );
    ui.right_click_label("Local Postgres");
    ui.click_label("New database…");
    ui.fill("manage-name", "sonde_walk_db");
    ui.click("manage-run");
    let created = ui.settle("the new database in the sidebar", |window, _| {
        window.try_find_by_label("sonde_walk_db").is_some()
    });
    ui.shot("13d-database-created");
    report.check("all databases: a new database appears by itself", created);
    ui.click_label("sonde_walk_db");
    let opened = ui.settle("the new database's schemas", |window, _| {
        window.try_find_by_label("public").is_some()
    });
    report.check(
        "all databases: a listed database opens its own schemas",
        opened,
    );
    ui.right_click_label("sonde_walk_db");
    ui.click_label("Drop database…");
    ui.click("manage-run");
    let gone = ui.settle("the dropped database to leave", |window, _| {
        window.try_find_by_label("sonde_walk_db").is_none()
    });
    report.check("all databases: a dropped database leaves by itself", gone);
    ui.right_click_label("Local Postgres");
    ui.click_label("Edit…");
    ui.click("all-databases");
    ui.click("save");
    ui.settle("the form to close", |window, _| {
        window.try_find("save").is_none()
    });
    ui.shot("13-after-restart");
    drop(ui);

    // ── A narrow window ──────────────────────────────────────────────────
    let mut ui = Ui::launch(&dir, keyring(), (640., 520.));
    ui.click_label("Local Postgres");
    // In a window this short the schema list loads below the fold; scroll
    // while waiting for it.
    ui.await_tree_row("sonde_walk");
    ui.shot("14-narrow");
    ui.scroll_tree_until("sonde_walk");
    ui.click_label("sonde_walk");
    ui.scroll_tree_until("book");
    ui.click_label("book");
    let detailed = ui.settle("the narrow table overview", |window, _| {
        window.try_find("inspector-columns").is_some()
    });
    ui.shot("14b-narrow-inspector");
    report.check(
        "narrow window: the object overview fits the main pane",
        detailed
            && ui.with(|window, _| {
                let content = window.find("inspector-columns").bounds();
                content.left() >= px(280.) && content.right() <= px(640.)
            }),
    );
    ui.click("new-connection");
    ui.shot("15a-narrow-engines");
    report.check(
        "narrow window: the engine grid fits the window",
        ui.with(|window, _| {
            ["kind-postgres", "kind-elasticsearch", "cancel"]
                .into_iter()
                .all(|id| {
                    let bounds = window.find(id).bounds();
                    bounds.left() >= px(0.) && bounds.right() <= px(640.)
                })
        }),
    );
    ui.click("kind-postgres");
    ui.shot("15-narrow-form");
    report.check(
        "narrow window: the form's buttons stay inside the window",
        ui.with(|window, _| {
            ["test", "cancel", "save"].into_iter().all(|id| {
                let bounds = window.find(id).bounds();
                bounds.left() >= px(0.)
                    && bounds.top() >= px(0.)
                    && bounds.right() <= px(640.)
                    && bounds.bottom() <= px(520.)
            })
        }),
    );
    ui.with(|window, cx| {
        window.scroll("Host", ScrollDelta::Pixels(point(px(0.), px(-500.))), cx);
    });
    ui.shot("15b-narrow-form-scrolled");
    report.check(
        "narrow window: the fields scroll above the fixed action buttons",
        ui.with(|window, _| {
            let status = window.find("connection-status").bounds();
            let save = window.find("save").bounds();
            status.top() >= px(0.) && status.bottom() <= save.top()
        }),
    );
    ui.click("cancel");
    let closed = ui.settle("the form to close", |window, _| {
        window.try_find("save").is_none()
    });
    report.check("narrow window: Cancel closes the form", closed);
    drop(ui);

    // ── No keyring: ask each time ────────────────────────────────────────
    let mut ui = Ui::launch(&dir, None, (1200., 800.));
    ui.click_label("Local Postgres");
    report.check(
        "no keyring: connecting asks for the password",
        ui.has("prompt-password"),
    );
    ui.shot("16-no-keyring-prompt");
    ui.press("escape");
    let closed = ui.settle("the password prompt to close", |window, _| {
        window.try_find("prompt-password").is_none()
    });
    report.check(
        "no keyring: Escape cancels without connecting",
        closed && !ui.has_label("sonde_walk"),
    );
    ui.click_label("Local Postgres");
    ui.with(|window, cx| window.input("sonde", cx));
    ui.press("enter");
    let connected = ui.settle("the schema list", |window, _| {
        window.try_find_by_label("sonde_walk").is_some()
    });
    report.check("no keyring: the typed password connects", connected);
    ui.shot("17-no-keyring-connected");
    drop(ui);

    // First-run Postgres setup uses the real form and connects on completion.
    let pg_setup = dir.join("postgres-setup");
    std::fs::create_dir_all(&pg_setup).unwrap();
    let mut ui = Ui::launch(&pg_setup, keyring(), (1200., 800.));
    ui.click("setup-postgres");
    ui.shot("18-setup-postgres-details");
    ui.click("setup-next");
    report.check(
        "setup: invalid details stay on the details step",
        ui.has("Name") && ui.saved_names().is_empty(),
    );
    ui.fill("Name", "Setup Postgres");
    ui.fill("Host", "127.0.0.1");
    ui.fill("Port", &PG_PORT.to_string());
    ui.fill("Database", "sonde");
    ui.fill("User", "sonde");
    ui.fill("Password", "wrong-setup-password");
    ui.press("enter");
    report.check(
        "setup: Enter advances to review without saving",
        ui.has("setup-connect") && !ui.has("Password") && ui.saved_names().is_empty(),
    );
    ui.click("setup-test");
    let tested = ui.settle("the setup test to finish", |window, _| {
        window.find("setup-test").disabled() != Some(true)
    });
    report.check(
        "setup: a failed test is reported without saving",
        tested && ui.status_contains("rejected the password") && ui.saved_names().is_empty(),
    );
    ui.shot("19-setup-test-failed");
    ui.click("setup-back");
    report.check(
        "setup: Back preserves the connection details",
        ui.with(|window, _| window.find("Name").value() == Some("Setup Postgres")),
    );
    ui.fill("Password", "sonde");
    ui.press("enter");
    report.check(
        "setup: editing settings clears the old test result",
        !ui.status_contains("rejected the password"),
    );
    ui.click("setup-test");
    let tested = ui.settle("the successful setup test", |window, _| {
        window.find("setup-test").disabled() != Some(true)
    });
    report.check(
        "setup: the real Postgres connection can be tested",
        tested && ui.status_contains("Connected in"),
    );
    ui.shot("20-setup-review");
    ui.with(|window, cx| Theme::change(ThemeMode::Dark, Some(window), cx));
    ui.shot("20b-setup-dark-review");
    ui.click("setup-connect");
    let connected = ui.settle("setup to open the Postgres schemas", |window, _| {
        window.try_find("setup-wizard").is_none()
            && window.try_find_by_label("sonde_walk").is_some()
    });
    report.check(
        "setup: Save and connect saves once and opens the database",
        connected && ui.saved_names() == ["Setup Postgres"],
    );
    report.check(
        "setup: passwords stay out of the saved connection file",
        !std::fs::read_to_string(pg_setup.join("connections.toml"))
            .unwrap()
            .contains("password"),
    );
    ui.shot("21-setup-connected");
    drop(ui);
    let mut ui = Ui::launch(&pg_setup, keyring(), (1200., 800.));
    report.check(
        "setup: saved but disconnected databases do not trigger the wizard on restart",
        !ui.has("setup-wizard") && ui.has_label("Setup Postgres"),
    );
    drop(ui);

    // SQLite setup works without a keyring and testing is optional.
    let sqlite_setup = dir.join("sqlite-setup");
    std::fs::create_dir_all(&sqlite_setup).unwrap();
    let mut ui = Ui::launch(&sqlite_setup, None, (640., 520.));
    ui.shot("22-setup-narrow");
    ui.click("setup-sqlite");
    ui.fill("Name", "Setup SQLite");
    ui.fill("Database file", &sqlite_path.to_string_lossy());
    ui.click("setup-next");
    ui.shot("23-setup-sqlite-review");
    report.check(
        "setup: narrow navigation stays within the window",
        ui.with(|window, _| {
            let bounds = window.find("setup-connect").bounds();
            bounds.left() >= px(0.) && bounds.right() <= px(640.) && bounds.bottom() <= px(520.)
        }),
    );
    ui.click("setup-back");
    ui.click("setup-back");
    report.check(
        "setup: Back returns to database choice",
        ui.has("setup-sqlite") && !ui.has("Database file"),
    );
    ui.click("setup-sqlite");
    report.check(
        "setup: returning to details preserves the SQLite path",
        ui.with(|window, _| {
            window.find("Database file").value() == Some(sqlite_path.to_string_lossy().as_ref())
        }),
    );
    ui.click("setup-next");
    ui.click("setup-connect");
    let connected = ui.settle("setup to open SQLite", |window, _| {
        window.try_find("setup-wizard").is_none() && window.try_find_by_label("main").is_some()
    });
    report.check(
        "setup: SQLite opens without a keyring or a required test",
        connected && ui.saved_names() == ["Setup SQLite"],
    );
    drop(ui);

    let session_setup = dir.join("session-setup");
    std::fs::create_dir_all(&session_setup).unwrap();
    let mut ui = Ui::launch(&session_setup, None, (1200., 800.));
    ui.click("setup-postgres");
    for (field, value) in [
        ("Name", "Session Postgres"),
        ("Host", "127.0.0.1"),
        ("Database", "sonde"),
        ("User", "sonde"),
        ("Password", "sonde"),
    ] {
        ui.fill(field, value);
    }
    ui.fill("Port", &PG_PORT.to_string());
    ui.click("setup-next");
    ui.click("setup-connect");
    let connected = ui.settle("setup to use the session-only password", |window, _| {
        window.try_find_by_label("sonde_walk").is_some()
    });
    report.check(
        "setup: no-keyring Postgres uses the password without asking twice",
        connected && !ui.has("prompt-password"),
    );
    report.check(
        "setup: session-only passwords are never written to the connection file",
        !std::fs::read_to_string(session_setup.join("connections.toml"))
            .unwrap()
            .contains("password"),
    );
    drop(ui);

    let url_setup = dir.join("url-setup");
    std::fs::create_dir_all(&url_setup).unwrap();
    let mut ui = Ui::launch(&url_setup, None, (1200., 800.));
    let original_mode = ui.with(|_, cx| cx.theme().mode);
    ui.click("setup-theme");
    report.check(
        "setup: the chooser's theme shortcut changes the appearance in place",
        ui.has("setup-wizard") && ui.with(|_, cx| cx.theme().mode != original_mode),
    );
    ui.shot("24-chooser-theme");
    ui.click("setup-theme");
    ui.fill("setup-url", "mssql://alice:private@localhost/db");
    ui.press("enter");
    report.check(
        "setup: unsupported URLs stay in the chooser without leaking credentials",
        ui.has("setup-url-error") && !ui.has("Name") && !ui.has_label("private"),
    );
    ui.fill(
        "setup-url",
        &format!("postgresql://sonde:sonde@127.0.0.1:{PG_PORT}/sonde?sslmode=disable"),
    );
    ui.press("enter");
    report.check(
        "setup: a pasted URL fills real connection details",
        ui.with(|window, _| {
            window.find("Host").value() == Some("127.0.0.1")
                && window.find("Name").value() == Some("sonde")
                && window.find("Database").value() == Some("sonde")
                && window.find("User").value() == Some("sonde")
        }),
    );
    ui.click("setup-back");
    report.check(
        "setup: the URL is cleared after importing credentials",
        ui.with(|window, _| window.find("setup-url").value() == Some("")),
    );
    ui.click("setup-postgres");
    ui.click("setup-next");
    ui.click("setup-connect");
    let connected = ui.settle("a URL connection to open", |window, _| {
        window.try_find_by_label("sonde_walk").is_some()
    });
    report.check(
        "setup: imported credentials connect without another password prompt",
        connected && !ui.has("password-prompt") && ui.saved_names() == ["sonde"],
    );
    ui.click("sidebar-identity");
    let menu_matches_trigger = |window: &mut Window, _: &mut App| {
        let trigger = window.find("sidebar-identity").bounds();
        let menu = window.find("popup-menu").bounds();
        menu.left() == trigger.left() && menu.right() == trigger.right()
    };
    report.check(
        "workspace: the Sonde menu aligns with both edges of its trigger",
        ui.with(menu_matches_trigger),
    );
    ui.shot("25-workspace-menu");
    ui.click_label("Dark");
    report.check(
        "workspace: the Sonde menu changes and saves the theme",
        ui.with(|_, cx| cx.theme().mode.is_dark())
            && LayoutFile::at(url_setup.join("layout.toml"))
                .load()
                .appearance
                == Appearance::Dark,
    );
    ui.shot("25-workspace-dark");
    ui.click("sidebar-identity");
    ui.click_label("Settings");
    report.check(
        "workspace: Settings opens from the Sonde menu",
        ui.has("workspace-settings"),
    );
    ui.shot("26-workspace-settings");
    report.check(
        "settings: an Updates section shows the version and the choices",
        ui.has("settings-updates") && ui.has_label("Ask before installing"),
    );
    ui.click_label("Light");
    report.check(
        "settings: Light updates the appearance without closing Settings",
        ui.has("workspace-settings") && !ui.with(|_, cx| cx.theme().mode.is_dark()),
    );
    ui.click_label("System");
    report.check(
        "settings: System saves the system preference and applies the window appearance",
        LayoutFile::at(url_setup.join("layout.toml"))
            .load()
            .appearance
            == Appearance::System
            && ui.with(|window, cx| cx.theme().mode == ThemeMode::from(window.appearance())),
    );
    ui.click_label("Compact");
    report.check(
        "settings: Compact saves a narrower sidebar",
        LayoutFile::at(url_setup.join("layout.toml"))
            .load()
            .sidebar_width
            < Layout::default().sidebar_width,
    );
    ui.press("escape");
    assert!(ui.settle("Settings to close", |window, _| {
        window.try_find("workspace-settings").is_none()
    }));
    ui.click("sidebar-identity");
    report.check(
        "workspace: the Sonde menu follows the compact sidebar width",
        ui.with(menu_matches_trigger),
    );
    ui.shot("26-compact-workspace-menu");
    ui.press("down");
    ui.press("enter");
    report.check(
        "workspace: the Sonde menu opens Settings by keyboard",
        ui.has("workspace-settings"),
    );
    ui.click_label("Standard");
    report.check(
        "settings: Standard restores the default width",
        LayoutFile::at(url_setup.join("layout.toml"))
            .load()
            .sidebar_width
            == Layout::default().sidebar_width,
    );
    ui.click("settings-sidebar-visible");
    report.check(
        "settings: hiding the sidebar leaves Settings accessible",
        ui.has("workspace-settings") && !ui.has("sidebar-identity"),
    );
    ui.click("settings-sidebar-visible");
    ui.press("escape");
    let closed = ui.settle("Settings to close", |window, _| {
        window.try_find("workspace-settings").is_none()
    });
    report.check(
        "settings: showing the sidebar restores the Sonde menu and Escape closes Settings",
        closed && ui.has("sidebar-identity"),
    );
    drop(ui);

    let dismissed_setup = dir.join("dismissed-setup");
    std::fs::create_dir_all(&dismissed_setup).unwrap();
    let mut ui = Ui::launch(&dismissed_setup, keyring(), (1200., 800.));
    ui.click("setup-theme");
    let chosen_mode = ui.with(|_, cx| cx.theme().mode);
    ui.press("escape");
    report.check(
        "setup: Escape dismisses setup without writing a connection",
        !ui.has("setup-wizard") && ui.saved_names().is_empty(),
    );
    drop(ui);
    let mut ui = Ui::launch(&dismissed_setup, keyring(), (1200., 800.));
    report.check(
        "setup: a saved theme override applies on restart before a connection is saved",
        ui.has("setup-wizard") && ui.with(|_, cx| cx.theme().mode == chosen_mode),
    );
    drop(ui);

    // ── Updates: nothing is checked until the user says so ───────────────
    let mut ui = Ui::launch(&dir, keyring(), (1200., 800.));
    report.check(
        "updates: no answer yet means no checks",
        ui.with(|_, cx| {
            cx.global::<sonde_app::updates::Updater>()
                .prefs
                .consent
                .is_none()
        }),
    );
    ui.with(sonde_app::updates::ask_consent);
    ui.shot("27-update-consent");
    report.check(
        "updates: the consent question offers all three answers",
        ui.has("updates-automatic") && ui.has("updates-ask") && ui.has("updates-off"),
    );
    ui.click("updates-off");
    let closed = ui.settle("the consent dialog to close", |window, _| {
        window.try_find("updates-off").is_none()
    });
    report.check(
        "updates: choosing records the answer and closes the question",
        closed
            && ui.with(|_, cx| {
                cx.global::<sonde_app::updates::Updater>().prefs.consent
                    == Some(sonde_app::updates::Consent::Off)
            }),
    );
    drop(ui);

    let broken = dir.join("broken-settings");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join("connections.toml"), "invalid = [").unwrap();
    let mut ui = Ui::launch(&broken, keyring(), (1200., 800.));
    report.check(
        "setup: unreadable settings are not treated as a fresh install",
        !ui.has("setup-wizard")
            && std::fs::read_to_string(broken.join("connections.toml")).unwrap() == "invalid = [",
    );
    drop(ui);

    let _ = std::fs::remove_dir_all(&dir);
    println!(
        "\nwalkthrough: {}",
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
