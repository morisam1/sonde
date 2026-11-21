//! Sonde's application: the window, the sidebar and the dialogs.

pub mod browser;
pub mod connections;
mod design;
mod inspector;
pub mod layout;
pub mod query;
pub mod session;
pub mod settings;
pub mod table_view;
mod ui;
pub mod updates;
pub mod workspace;

pub use ui::AppAssets;

use std::sync::Arc;

#[cfg(target_os = "linux")]
use anyhow::Context as _;
use gpui_kit::component::TitleBar;
use gpui_kit::*;
use sonde_core::config::{ConnectionStore, Keyring, SecretStore};

use crate::{connections::Connections, layout::LayoutFile, workspace::Workspace};

const APP_ID: &str = "sonde";
const APP_NAME: &str = "Sonde";

/// Start the application and run until its window is closed.
///
/// # Errors
///
/// When there is no home directory for settings or the bundled icon cannot be decoded.
pub fn run() -> anyhow::Result<()> {
    let store = ConnectionStore::standard()?;
    let layout_file = LayoutFile::standard()?;
    #[cfg(target_os = "linux")]
    let icon = Arc::new(
        image::load_from_memory_with_format(
            include_bytes!("../assets/sonde.png"),
            image::ImageFormat::Png,
        )
        .context("could not decode the Sonde app icon")?
        .into_rgba8(),
    );

    gpui_kit::application()
        .with_assets(ui::AppAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            if let Ok(history) = sonde_core::history::History::standard() {
                cx.set_global(workspace::HistoryLocation(history.dir().to_path_buf()));
            }
            cx.set_app_identity(APP_ID, APP_NAME);
            // There is one window; closing it ends the program.
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let layout = layout_file.load();
            // Sonde draws its own title bar and window frame; the platform hands
            // over the decoration (and falls back to its own when it cannot).
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(layout.window_width), px(layout.window_height)),
                    cx,
                ))),
                app_id: Some(APP_ID.into()),
                titlebar: Some(TitlebarOptions {
                    title: Some(APP_NAME.into()),
                    ..TitleBar::title_bar_options()
                }),
                #[cfg(target_os = "linux")]
                icon: Some(icon),
                window_decorations: Some(WindowDecorations::Client),
                ..TitleBar::window_options()
            };
            let opened = gpui_kit::open_window(options, cx, |window, cx| {
                let secrets =
                    Keyring::open().map(|keyring| Arc::new(keyring) as Arc<dyn SecretStore>);
                let connections = cx.new(|_| Connections::load(store, secrets));
                cx.new(|cx| Workspace::new(connections, layout_file, window, cx))
            });
            match opened {
                Ok((window, _)) => {
                    cx.activate(true);
                    let dir = sonde_core::config::config_dir().ok();
                    updates::start(dir, window, cx);
                }
                Err(error) => {
                    eprintln!("sonde: could not open a window: {error}");
                    cx.quit();
                }
            }
        });
    Ok(())
}
