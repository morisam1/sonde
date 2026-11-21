//! Table tabs' requests (open a linked table, open a query, save a view)
//! and the sidebar's saved views.

use std::path::PathBuf;

use gpui_kit::component::{
    ActiveTheme as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::ContextMenuExt as _,
    v_flex,
};
use gpui_kit::*;

use super::{TabKind, Workspace, queries::NameFor};
use crate::{
    saved_views::{self, SavedView},
    table_view::{TableView, TableViewEvent, ViewState},
    ui::{self, Glyph},
};

impl Workspace {
    pub(super) fn on_table_event(
        &mut self,
        panel: &Entity<TableView>,
        event: &TableViewEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = panel.read(cx).target();
        match event {
            TableViewEvent::OpenTable { table, filter } => {
                let opened = self.open_table(target, table.clone(), window, cx);
                let state = ViewState {
                    filter: filter.clone(),
                    ..ViewState::default()
                };
                opened.update(cx, |view, cx| view.apply_view(state, window, cx));
            }
            TableViewEvent::OpenQuery(sql) => {
                self.open_query(target, window, cx);
                if let Some(TabKind::Query(query)) = self.tabs.last().map(|tab| tab.kind.clone()) {
                    query.update(cx, |query, cx| query.set_text(sql, window, cx));
                }
            }
            TableViewEvent::SaveView(state) => {
                let view = panel.read(cx);
                let table = view.table_ref();
                let saved = SavedView {
                    connection_id: crate::connections::keyring_id(&target.id).to_owned(),
                    schema: table.schema.clone(),
                    table: table.name.clone(),
                    state: state.clone(),
                };
                let name = table.name.clone();
                self.ask_name(NameFor::SaveView(Box::new(saved)), name, window, cx);
            }
        }
    }

    pub(super) fn reload_saved_views(&mut self, cx: &mut Context<Self>) {
        self.saved_views = saved_views::list(&self.views_dir);
        cx.notify();
    }

    /// Act on a name the dialog took for a view.
    pub(super) fn named_view(
        &mut self,
        purpose: NameFor,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let path = saved_views::path_for(&self.views_dir, name)?;
        match purpose {
            NameFor::SaveView(view) => {
                if path.exists() {
                    return Err(format!(
                        "A saved view is already called {}.",
                        saved_views::name_of(&path)
                    ));
                }
                saved_views::write(&path, &view)
                    .map_err(|error| format!("Could not save it: {error}"))?;
            }
            NameFor::RenameView(from) => {
                if path == from {
                    return Ok(());
                }
                if path.exists() {
                    return Err(format!(
                        "A saved view is already called {}.",
                        saved_views::name_of(&path)
                    ));
                }
                std::fs::rename(&from, &path)
                    .map_err(|error| format!("Could not rename it: {error}"))?;
            }
            _ => {}
        }
        self.reload_saved_views(cx);
        Ok(())
    }

    /// Open a saved view: its table, with its filter, sort and columns.
    pub fn open_view(
        &mut self,
        path: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = match saved_views::read(path) {
            Ok(view) => view,
            Err(error) => {
                window.push_notification(format!("Could not open the view: {error}"), cx);
                return;
            }
        };
        let Some(target) = self.target_for(&view.connection_id, cx) else {
            window.push_notification(
                "The connection this view was saved on no longer exists.".to_owned(),
                cx,
            );
            return;
        };
        let panel = self.open_table(target, view.table_ref(), window, cx);
        panel.update(cx, |panel, cx| panel.apply_view(view.state, window, cx));
    }

    /// The saved views in the folder.
    pub fn saved_views(&self) -> &[saved_views::ViewFile] {
        &self.saved_views
    }

    fn delete_view(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let name = saved_views::name_of(&path);
        let workspace = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            let path = path.clone();
            let workspace = workspace.clone();
            dialog
                .title("Delete saved view")
                .w(px(420.))
                .p_6()
                .child(
                    v_flex()
                        .gap_4()
                        .child(div().text_sm().child(format!(
                            "This deletes the saved view {name}. The table and its rows are not touched."
                        )))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(div().flex_1())
                                .child(
                                    Button::new("delete-view-cancel")
                                        .ghost()
                                        .label("Cancel")
                                        .on_click(|_, window, cx| window.close_dialog(cx)),
                                )
                                .child(
                                    Button::new("delete-view-confirm")
                                        .danger()
                                        .label("Delete")
                                        .on_click(move |_, window, cx| {
                                            let _ = workspace.update(cx, |this, cx| {
                                                if let Err(error) = std::fs::remove_file(&path) {
                                                    window.push_notification(
                                                        format!("Could not delete it: {error}"),
                                                        cx,
                                                    );
                                                }
                                                this.reload_saved_views(cx);
                                            });
                                            window.close_dialog(cx);
                                        }),
                                ),
                        ),
                )
                .text_color(cx.theme().foreground)
        });
    }

    pub(super) fn render_saved_views(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = v_flex().gap_1();
        for (ix, view) in self.saved_views.iter().enumerate() {
            let (path, menu_path) = (view.path.clone(), view.path.clone());
            let owner = cx.weak_entity();
            list = list.child(
                ui::menu_button(("saved-view", ix), false, cx)
                    .test_support()
                    .aria_label(view.name.clone())
                    .child(ui::icon(Glyph::Bookmark).flex_shrink_0())
                    .child(div().flex_1().min_w_0().truncate().child(view.name.clone()))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.open_view(&path, window, cx)),
                    )
                    .context_menu(move |menu, _, _| {
                        let (open, rename, delete) =
                            (menu_path.clone(), menu_path.clone(), menu_path.clone());
                        let (o1, o2, o3) = (owner.clone(), owner.clone(), owner.clone());
                        menu.min_w(px(ui::MENU_WIDTH))
                            .item(ui::menu_item("Open", Glyph::Bookmark, false).on_click(
                                move |_, window, cx| {
                                    let _ =
                                        o1.update(cx, |this, cx| this.open_view(&open, window, cx));
                                },
                            ))
                            .item(ui::menu_item("Rename…", Glyph::PencilLine, false).on_click(
                                move |_, window, cx| {
                                    let _ = o2.update(cx, |this, cx| {
                                        let name = saved_views::name_of(&rename);
                                        this.ask_name(
                                            NameFor::RenameView(rename.clone()),
                                            name,
                                            window,
                                            cx,
                                        );
                                    });
                                },
                            ))
                            .separator()
                            .item(ui::menu_item("Delete…", Glyph::Trash, true).on_click(
                                move |_, window, cx| {
                                    let _ = o3.update(cx, |this, cx| {
                                        this.delete_view(delete.clone(), window, cx);
                                    });
                                },
                            ))
                    }),
            );
        }
        div()
            .id("saved-views")
            .test_support()
            .max_h(px(crate::design::ROW_HEIGHT * 5. + 16.))
            .overflow_y_scroll()
            .child(list)
    }
}
