use gpui_kit::{
    component::{
        ActiveTheme as _, Side, WindowExt as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        menu::{DropdownMenu as _, PopupMenuItem},
        radio::{Radio, RadioGroup},
        switch::Switch,
        v_flex,
    },
    prelude::FluentBuilder as _,
    *,
};

use super::{SIDEBAR_BORDER, SIDEBAR_HEADER_INSET, Workspace, identity};
use crate::{
    design::ROW_HEIGHT,
    layout::{Appearance, Layout},
    ui::{self, Glyph},
};

const THEMES: [(Appearance, &str, &str, Glyph); 3] = [
    (Appearance::System, "theme-system", "System", Glyph::Monitor),
    (Appearance::Light, "theme-light", "Light", Glyph::Sun),
    (Appearance::Dark, "theme-dark", "Dark", Glyph::Moon),
];
const COMPACT_WIDTH: f32 = 220.;

fn app_menu_item(label: &'static str, icon: Glyph) -> PopupMenuItem {
    PopupMenuItem::element(move |_, _| {
        h_flex()
            .id(label)
            .test_support()
            .aria_label(label)
            .flex_1()
            .h(px(ROW_HEIGHT))
            .px_1()
            .gap_2()
            .child(ui::icon(icon))
            .child(label)
    })
}

impl Workspace {
    pub(super) fn set_appearance(
        &mut self,
        appearance: Appearance,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.appearance = appearance;
        appearance.apply(window, cx);
        self.layout_file.save(self.layout(window));
        cx.notify();
    }

    pub(super) fn render_app_menu(
        &self,
        live: usize,
        count: usize,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let workspace = cx.weak_entity();
        let current = self.appearance;
        let menu_width = px(self.sidebar_width) - SIDEBAR_BORDER - SIDEBAR_HEADER_INSET * 2.;
        Button::new("sidebar-identity")
            .ghost()
            .h(px(56.))
            .w_full()
            .px_2()
            .tab_stop(true)
            .accessibility_label("Sonde menu")
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(identity(cx))
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{live}/{count}")),
                    )
                    .child(ui::icon(Glyph::ChevronDown).text_color(cx.theme().muted_foreground)),
            )
            .dropdown_menu(move |menu, _, cx| {
                let settings = workspace.clone();
                let mut menu = menu
                    .min_w(menu_width)
                    .max_w(menu_width)
                    .check_side(Side::Right)
                    .item(app_menu_item("Settings", Glyph::Settings).on_click(
                        move |_, window, cx| {
                            if let Some(workspace) = settings.upgrade() {
                                window.open_dialog(cx, move |dialog, _, cx| {
                                    let body = workspace.update(cx, |this, cx| this.settings(cx));
                                    dialog.title("Settings").w(px(440.)).child(body)
                                });
                            }
                        },
                    ))
                    .item({
                        let (label, enabled) = crate::updates::menu_entry(cx);
                        if enabled {
                            ui::menu_item(label, Glyph::RefreshCw, false)
                                .on_click(|_, _, cx| crate::updates::menu_action(cx))
                        } else {
                            ui::disabled_menu_item(label, Glyph::RefreshCw)
                        }
                    })
                    .separator()
                    .label("Theme");
                for (appearance, _, label, icon) in THEMES {
                    let workspace = workspace.clone();
                    menu = menu.item(
                        app_menu_item(label, icon)
                            .checked(current == appearance)
                            .on_click(move |_, window, cx| {
                                let _ = workspace.update(cx, |this, cx| {
                                    this.set_appearance(appearance, window, cx);
                                });
                            }),
                    );
                }
                menu
            })
    }

    fn settings(&self, cx: &Context<Self>) -> AnyElement {
        let default_width = Layout::default().sidebar_width;
        let widths = [COMPACT_WIDTH, default_width];
        let heading = |label: &'static str| {
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(label)
        };

        v_flex()
            .id("workspace-settings")
            .test_support()
            .gap_5()
            .child(
                v_flex().gap_3().child(heading("Appearance")).child(
                    RadioGroup::horizontal("settings-appearance")
                        .children(THEMES.map(|(_, id, label, _)| Radio::new(id).label(label)))
                        .selected_index(
                            THEMES
                                .iter()
                                .position(|(mode, ..)| *mode == self.appearance),
                        )
                        .on_change(cx.listener(|this, index: &usize, window, cx| {
                            this.set_appearance(THEMES[*index].0, window, cx);
                        })),
                ),
            )
            .child(
                v_flex()
                    .gap_3()
                    .pt_4()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(heading("Workspace"))
                    .child(
                        Switch::new("settings-sidebar-visible")
                            .label("Show sidebar")
                            .checked(self.sidebar_open)
                            .on_change(cx.listener(|this, &visible, window, cx| {
                                this.sidebar_open = visible;
                                this.layout_file.save(this.layout(window));
                                cx.notify();
                            })),
                    )
                    .child(ui::muted("Sidebar width", cx))
                    .child(
                        RadioGroup::horizontal("settings-sidebar-width")
                            .child(Radio::new("compact").label("Compact"))
                            .child(Radio::new("standard").label("Standard"))
                            .selected_index(
                                widths.iter().position(|width| *width == self.sidebar_width),
                            )
                            .on_change(cx.listener(move |this, index: &usize, window, cx| {
                                this.sidebar_width = widths[*index];
                                this.layout_file.save(this.layout(window));
                                cx.notify();
                            })),
                    ),
            )
            .child({
                let (path, problem) = self.settings_file();
                v_flex()
                    .id("settings-file")
                    .test_support()
                    .gap_2()
                    .pt_4()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(heading("Editor and results"))
                    .child(ui::muted(
                        "Font, page size and row cap are set in this file and apply on the next start:",
                        cx,
                    ))
                    .child(
                        div()
                            .text_sm()
                            .font_family(crate::design::MONO_FONT)
                            .child(path.display().to_string()),
                    )
                    .when_some(problem.cloned(), |this, problem| {
                        this.child(div().text_sm().text_color(cx.theme().danger).child(problem))
                    })
            })
            .child(crate::updates::settings_section(cx))
            .into_any_element()
    }
}
