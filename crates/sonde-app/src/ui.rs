//! shadcn/ui building blocks on top of GPUI: cards, sidebar groups, menu
//! buttons and badges. Colour comes from the theme and `design.rs`.

use gpui_kit::{
    component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex},
    prelude::FluentBuilder as _,
    *,
};

use crate::design::{RADIUS_CARD, RADIUS_ITEM};

const LOGO_PATH: &str = "folded-s.svg";

gpui_kit::assets::icon_assets!(
    pub ExtraIcons,
    [
        Database, Layers, Table, Eye, Columns3, KeyRound, Key, Link, Plug, Unplug, ListTree,
        Hash, Rows3, X, Moon, Sun, Monitor, SquareTerminal, DatabasePlus, FolderPlus, Plus,
        PencilLine, Eraser, Trash, SquarePen, FileCode, Save, FolderOpen, ListFilter, ChartColumn,
        Bookmark, ArrowUpRight, Pin, Clock, ArrowUp, ArrowDown, Search
    ]
);

/// The component icons and Sonde's own artwork.
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        if path == LOGO_PATH {
            return Ok(Some(std::borrow::Cow::Borrowed(include_bytes!(
                "../assets/folded-s.svg"
            ))));
        }
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        if LOGO_PATH.starts_with(path) {
            paths.push(LOGO_PATH.into());
        }
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

pub use gpui_kit::assets::IconName as Glyph;

/// A 16px icon, the size shadcn uses inside menu buttons.
pub fn icon(name: impl Into<Icon>) -> Icon {
    name.into().with_size(px(crate::design::ICON))
}

/// The width of a right-click menu, so its rows line up with the sidebar.
pub const MENU_WIDTH: f32 = 220.;

/// A row of a right-click or dropdown menu, at the sidebar's row height,
/// with its icon. `danger` colors a destructive action.
pub fn menu_item(
    label: impl Into<SharedString>,
    glyph: Glyph,
    danger: bool,
) -> gpui_kit::component::menu::PopupMenuItem {
    styled_menu_item(label.into(), glyph, danger, true)
}

/// A menu row that cannot be chosen now: drawn quiet, and inert.
pub fn disabled_menu_item(
    label: impl Into<SharedString>,
    glyph: Glyph,
) -> gpui_kit::component::menu::PopupMenuItem {
    styled_menu_item(label.into(), glyph, false, false).disabled(true)
}

fn styled_menu_item(
    label: SharedString,
    glyph: Glyph,
    danger: bool,
    enabled: bool,
) -> gpui_kit::component::menu::PopupMenuItem {
    gpui_kit::component::menu::PopupMenuItem::element(move |_, cx| {
        let color = if !enabled {
            cx.theme().muted_foreground.opacity(0.6)
        } else if danger {
            cx.theme().danger
        } else {
            cx.theme().popover_foreground
        };
        h_flex()
            .id(label.clone())
            .test_support()
            .aria_label(label.clone())
            .flex_1()
            .h(px(crate::design::ROW_HEIGHT))
            .px_1()
            .gap_2()
            .text_color(color)
            .child(icon(glyph).text_color(if danger || !enabled {
                color
            } else {
                cx.theme().muted_foreground
            }))
            .child(label.clone())
    })
}

pub(crate) fn logo(size: Pixels, cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .size(size)
        .flex_shrink_0()
        .rounded(size * 0.25)
        .bg(theme.primary)
        .child(
            svg()
                .size_full()
                .path(LOGO_PATH)
                .text_color(theme.primary_foreground),
        )
}

/// `Card`: rounded-xl, hairline border, card surface.
pub fn card(cx: &App) -> Div {
    v_flex()
        .rounded(px(RADIUS_CARD))
        .border_1()
        .border_color(cx.theme().border)
        .bg(crate::design::tokens(cx).card)
        .overflow_hidden()
}

/// `bg-muted/50 rounded-xl`: the placeholder wells of the content area.
pub fn well(cx: &App) -> Div {
    v_flex()
        .rounded(px(RADIUS_CARD))
        .bg(crate::design::tokens(cx).well)
        .overflow_hidden()
}

/// `SidebarGroupLabel`: h-8, px-2, text-xs, 70% foreground.
pub fn group_label(label: impl Into<SharedString>, cx: &App) -> Div {
    h_flex()
        .h_9()
        .px_3()
        .flex_shrink_0()
        .items_center()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .text_color(cx.theme().sidebar_foreground.opacity(0.7))
        .child(label.into())
}

/// `SidebarMenuBadge`: text-xs, medium, tabular figures, no fill.
pub fn menu_badge(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .flex_shrink_0()
        .min_w_5()
        .px_1()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .text_color(cx.theme().sidebar_foreground)
        .child(text.into())
}

/// `Badge variant="secondary"`.
#[allow(dead_code)]
pub fn badge(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .flex_shrink_0()
        .px_2()
        .py(px(1.))
        .rounded(px(RADIUS_ITEM))
        .bg(cx.theme().secondary)
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .text_color(cx.theme().secondary_foreground)
        .child(text.into())
}

/// `SidebarMenuButton`: h-8, rounded-md, px-2, gap-2, text-sm, accent on
/// hover and when active.
pub fn menu_button(id: impl Into<ElementId>, active: bool, cx: &App) -> Stateful<Div> {
    let colors = crate::design::tokens(cx);
    h_flex()
        .id(id)
        .h(px(crate::design::ROW_HEIGHT))
        .w_full()
        .min_w_0()
        .px_3()
        .gap_3()
        .items_center()
        .rounded(px(RADIUS_ITEM))
        .text_sm()
        .cursor_pointer()
        .text_color(cx.theme().sidebar_foreground)
        .hover(move |this| this.bg(colors.sidebar_accent))
        .when(active, |this| {
            this.bg(colors.sidebar_accent)
                .text_color(colors.accent)
                .font_weight(FontWeight::MEDIUM)
        })
}

/// A thin vertical rule, as between the sidebar trigger and the breadcrumb.
pub fn vertical_separator(cx: &App) -> Div {
    div().flex_shrink_0().w_px().h_5().bg(cx.theme().border)
}

/// Muted supporting text.
pub fn muted(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}
