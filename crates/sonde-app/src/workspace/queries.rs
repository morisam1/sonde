//! Query tabs as files: new tabs, saving, opening, renaming and deleting
//! saved queries, and asking before a tab with unsaved changes closes.

use std::path::{Path, PathBuf};

use gpui_kit::component::{
    ActiveTheme as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex,
};
use gpui_kit::*;

use super::{TabKind, Workspace};
use crate::{query::QueryTarget, saved_queries};

/// What the name dialog does with the name.
#[derive(Clone)]
enum NameFor {
    /// Save the query tab at this index under the name.
    Save(usize),
    /// Rename the saved query at this path.
    Rename(PathBuf),
}

/// A dialog that asks for a query's name.
struct NameDialog {
    workspace: WeakEntity<Workspace>,
    purpose: NameFor,
    name: Entity<InputState>,
    problem: Option<SharedString>,
    _changes: Subscription,
}

impl NameDialog {
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name.read(cx).value().to_string();
        let purpose = self.purpose.clone();
        let outcome = self
            .workspace
            .update(cx, |workspace, cx| {
                workspace.named(purpose, &name, window, cx)
            })
            .unwrap_or_else(|_| Err("The window closed.".into()));
        match outcome {
            Ok(()) => window.close_dialog(cx),
            Err(problem) => {
                self.problem = Some(problem.into());
                cx.notify();
            }
        }
    }
}

impl Render for NameDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let action = match self.purpose {
            NameFor::Save(_) => "Save",
            NameFor::Rename(_) => "Rename",
        };
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child("Name"),
            )
            .child(
                Input::new(&self.name)
                    .id("query-name")
                    .aria_label("Query name"),
            )
            .child(
                div()
                    .id("query-name-status")
                    .min_h_5()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .children(self.problem.clone()),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1())
                    .child(
                        Button::new("query-name-cancel")
                            .ghost()
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("query-name-save")
                            .primary()
                            .label(action)
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}

impl Workspace {
    /// Read the saved-queries folder again.
    pub(super) fn reload_saved_queries(&mut self, cx: &mut Context<Self>) {
        self.saved_queries = saved_queries::list(&self.queries_dir);
        cx.notify();
    }

    /// Where a tab on `connection_id` connects: through the sidebar's open
    /// connection when there is one, so a typed password carries over.
    pub(super) fn target_for(&self, connection_id: &str, cx: &App) -> Option<QueryTarget> {
        if let Some(target) = self.browser.read(cx).query_target(connection_id) {
            return Some(target);
        }
        // A tab on a listed database opens on that database.
        let parent = crate::connections::keyring_id(connection_id);
        let connection = self.connections.read(cx).get(parent).cloned()?;
        Some(match crate::connections::database_of(connection_id) {
            Some(database) => QueryTarget {
                id: connection_id.to_owned(),
                name: format!("{} · {database}", connection.name),
                config: connection.config.with_database(database),
                password: None,
            },
            None => QueryTarget {
                id: connection.id,
                name: connection.name,
                config: connection.config,
                password: None,
            },
        })
    }

    /// The connection a new tab uses: the open tab's, else the sidebar's
    /// selected connection when it is connected.
    fn current_target(&self, cx: &App) -> Option<QueryTarget> {
        let from_tab = self
            .active
            .and_then(|ix| self.tabs.get(ix))
            .map(|tab| match &tab.kind {
                TabKind::Query(panel) => panel.read(cx).target(),
                TabKind::Table(panel) => panel.read(cx).target(),
            });
        from_tab.or_else(|| self.browser.read(cx).selected_target(cx))
    }

    /// A new, empty query tab on the current connection.
    pub(super) fn new_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.current_target(cx) {
            Some(target) => self.open_query(target, window, cx),
            None => window.push_notification(
                "Connect to a database in the sidebar first, then open a new query.",
                cx,
            ),
        }
    }

    /// Save a query tab: to its file when it has one, else under a name the
    /// user gives. `ask` always asks for a name (Save as).
    pub(super) fn save_query(
        &mut self,
        ix: usize,
        ask: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(TabKind::Query(panel)) = self.tabs.get(ix).map(|tab| tab.kind.clone()) else {
            return;
        };
        if !ask && let Some(path) = panel.read(cx).path().map(Path::to_path_buf) {
            if let Err(problem) = self.write_query(ix, &path, cx) {
                window.push_notification(problem, cx);
            }
            return;
        }
        let name = panel.read(cx).title().to_string();
        self.ask_name(NameFor::Save(ix), name, window, cx);
    }

    fn write_query(
        &mut self,
        ix: usize,
        path: &Path,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(TabKind::Query(panel)) = self.tabs.get(ix).map(|tab| tab.kind.clone()) else {
            return Err("That tab is not a query.".into());
        };
        let (connection, text) = {
            let panel = panel.read(cx);
            (panel.connection_id().to_owned(), panel.text(cx))
        };
        saved_queries::write(path, Some(&connection), &text)
            .map_err(|error| format!("Could not save {}: {error}", path.display()))?;
        panel.update(cx, |panel, cx| {
            panel.mark_saved(path.to_path_buf(), text, cx)
        });
        self.reload_saved_queries(cx);
        Ok(())
    }

    fn ask_name(
        &mut self,
        purpose: NameFor,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = match purpose {
            NameFor::Save(_) => "Save query",
            NameFor::Rename(_) => "Rename query",
        };
        let workspace = cx.weak_entity();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(name));
        let dialog = cx.new(|cx| {
            let changes = cx.subscribe_in(
                &input,
                window,
                |this: &mut NameDialog, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => this.submit(window, cx),
                    InputEvent::Change => {
                        this.problem = None;
                        cx.notify();
                    }
                    _ => {}
                },
            );
            NameDialog {
                workspace,
                purpose,
                name: input.clone(),
                problem: None,
                _changes: changes,
            }
        });
        window.open_dialog(cx, move |d, _, _| {
            d.title(title).w(px(420.)).p_6().child(dialog.clone())
        });
        input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });
    }

    /// Act on a name the dialog took.
    fn named(
        &mut self,
        purpose: NameFor,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let path = saved_queries::path_for(&self.queries_dir, name)?;
        match purpose {
            NameFor::Save(ix) => {
                let current = match self.tabs.get(ix).map(|tab| &tab.kind) {
                    Some(TabKind::Query(panel)) => panel.read(cx).path().map(Path::to_path_buf),
                    _ => None,
                };
                if path.exists() && current.as_deref() != Some(path.as_path()) {
                    return Err(format!(
                        "A saved query is already called {}.",
                        saved_queries::name_of(&path)
                    ));
                }
                self.write_query(ix, &path, cx)?;
                self.activate(Some(ix), window, cx);
                Ok(())
            }
            NameFor::Rename(from) => {
                if path == from {
                    return Ok(());
                }
                if path.exists() {
                    return Err(format!(
                        "A saved query is already called {}.",
                        saved_queries::name_of(&path)
                    ));
                }
                std::fs::rename(&from, &path)
                    .map_err(|error| format!("Could not rename it: {error}"))?;
                for tab in &self.tabs {
                    if let TabKind::Query(panel) = &tab.kind
                        && panel.read(cx).path() == Some(from.as_path())
                    {
                        panel.update(cx, |panel, cx| panel.set_path(Some(path.clone()), cx));
                    }
                }
                self.reload_saved_queries(cx);
                Ok(())
            }
        }
    }

    /// Open a query file in a tab, or go to the tab that has it open.
    pub(super) fn open_query_file(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let open = self.tabs.iter().position(|tab| match &tab.kind {
            TabKind::Query(panel) => panel.read(cx).path() == Some(path),
            TabKind::Table(_) => false,
        });
        if let Some(ix) = open {
            self.activate(Some(ix), window, cx);
            return;
        }
        let file = match saved_queries::read(path) {
            Ok(file) => file,
            Err(error) => {
                window.push_notification(format!("Could not open {}: {error}", path.display()), cx);
                return;
            }
        };
        let target = file
            .connection_id
            .as_deref()
            .and_then(|id| self.target_for(id, cx))
            .or_else(|| self.current_target(cx));
        let Some(target) = target else {
            window.push_notification(
                "This query names no saved connection. Connect to a database in the sidebar, then open it again.",
                cx,
            );
            return;
        };
        self.open_query(target, window, cx);
        if let Some(TabKind::Query(panel)) = self.tabs.last().map(|tab| tab.kind.clone()) {
            panel.update(cx, |panel, cx| {
                panel.set_text(&file.sql, window, cx);
                panel.mark_saved(path.to_path_buf(), file.sql.clone(), cx);
            });
        }
    }

    /// Ask for `.sql` files anywhere on disk and open each.
    pub(super) fn choose_query_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Open".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = chosen.await {
                let _ = this.update_in(cx, |this, window, cx| {
                    for path in paths {
                        this.open_query_file(&path, window, cx);
                    }
                });
            }
        })
        .detach();
    }

    pub(super) fn rename_saved_query(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = saved_queries::name_of(&path);
        self.ask_name(NameFor::Rename(path), name, window, cx);
    }

    /// Delete a saved query, after asking. Tabs that had it open keep their
    /// text, as unsaved.
    pub(super) fn delete_saved_query(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = saved_queries::name_of(&path);
        let workspace = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            let path = path.clone();
            let workspace = workspace.clone();
            dialog
                .title("Delete saved query")
                .w(px(420.))
                .p_6()
                .child(
                    v_flex()
                        .gap_4()
                        .child(div().text_sm().child(format!(
                            "This deletes the saved query {name} from disk. Open tabs keep its text."
                        )))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(div().flex_1())
                                .child(
                                    Button::new("delete-query-cancel")
                                        .ghost()
                                        .label("Cancel")
                                        .on_click(|_, window, cx| window.close_dialog(cx)),
                                )
                                .child(
                                    Button::new("delete-query-confirm")
                                        .danger()
                                        .label("Delete")
                                        .on_click(move |_, window, cx| {
                                            let path = path.clone();
                                            let _ = workspace.update(cx, |this, cx| {
                                                if let Err(error) = std::fs::remove_file(&path) {
                                                    window.push_notification(
                                                        format!("Could not delete it: {error}"),
                                                        cx,
                                                    );
                                                }
                                                for tab in &this.tabs {
                                                    if let TabKind::Query(panel) = &tab.kind
                                                        && panel.read(cx).path() == Some(path.as_path())
                                                    {
                                                        panel.update(cx, |panel, cx| panel.set_path(None, cx));
                                                    }
                                                }
                                                this.reload_saved_queries(cx);
                                            });
                                            window.close_dialog(cx);
                                        }),
                                ),
                        ),
                )
                .text_color(cx.theme().foreground)
        });
    }

    /// Close a tab, first asking to save a query with unsaved changes.
    pub(super) fn request_close(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let dirty = match self.tabs.get(ix).map(|tab| &tab.kind) {
            Some(TabKind::Query(panel)) => {
                panel.read(cx).is_dirty(cx).then(|| panel.read(cx).title())
            }
            _ => None,
        };
        let Some(title) = dirty else {
            self.close_tab(ix, window, cx);
            return;
        };
        self.activate(Some(ix), window, cx);
        let workspace = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let save = workspace.clone();
            let discard = workspace.clone();
            dialog
                .title("Save changes?")
                .w(px(440.))
                .p_6()
                .child(
                    v_flex()
                        .gap_4()
                        .child(div().text_sm().child(format!(
                            "{title} has changes that are not saved. If you close it without saving, the changes are lost."
                        )))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new("close-discard")
                                        .ghost()
                                        .label("Don't save")
                                        .on_click(move |_, window, cx| {
                                            window.close_dialog(cx);
                                            let _ = discard.update(cx, |this, cx| {
                                                this.close_tab(ix, window, cx);
                                            });
                                        }),
                                )
                                .child(div().flex_1())
                                .child(
                                    Button::new("close-cancel")
                                        .outline()
                                        .label("Cancel")
                                        .on_click(|_, window, cx| window.close_dialog(cx)),
                                )
                                .child(
                                    Button::new("close-save")
                                        .primary()
                                        .label("Save")
                                        .on_click(move |_, window, cx| {
                                            window.close_dialog(cx);
                                            let _ = save.update(cx, |this, cx| {
                                                this.save_then_close(ix, window, cx);
                                            });
                                        }),
                                ),
                        ),
                )
        });
    }

    /// Save a tab, then close it once it is saved. A new query asks for its
    /// name first and stays open if the user cancels.
    fn save_then_close(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let path = match self.tabs.get(ix).map(|tab| &tab.kind) {
            Some(TabKind::Query(panel)) => panel.read(cx).path().map(Path::to_path_buf),
            _ => None,
        };
        match path {
            Some(path) => match self.write_query(ix, &path, cx) {
                Ok(()) => self.close_tab(ix, window, cx),
                Err(problem) => window.push_notification(problem, cx),
            },
            // Saved under a name, it stays open; the user closes it then.
            None => self.save_query(ix, true, window, cx),
        }
    }
}
