//! Which columns a table tab shows, in what order, and which stay pinned
//! at the left while the rest scroll. Remembered per table in
//! `tables.toml` beside Sonde's other files.

use std::{collections::BTreeMap, path::PathBuf};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex, v_flex,
};
use gpui_kit::*;
use serde::{Deserialize, Serialize};

use super::TableView;
use crate::ui::Glyph;

/// The columns' arrangement. Names only: a column added to the table later
/// shows at the end, and one dropped is ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnLayout {
    /// Columns in the order they were arranged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub order: Vec<String>,
    /// Columns not shown.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden: Vec<String>,
    /// Columns kept at the left.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pinned: Vec<String>,
}

impl ColumnLayout {
    pub fn is_default(&self) -> bool {
        self.order.is_empty() && self.hidden.is_empty() && self.pinned.is_empty()
    }

    /// The columns to show, as indices into `names` in display order, and
    /// how many at the front are pinned.
    pub fn arrange(&self, names: &[String]) -> (Vec<usize>, usize) {
        let position = |name: &String| names.iter().position(|n| n == name);
        let mut order: Vec<usize> = self.order.iter().filter_map(position).collect();
        for ix in 0..names.len() {
            if !order.contains(&ix) {
                order.push(ix);
            }
        }
        order.retain(|ix| !self.hidden.contains(&names[*ix]));
        if order.is_empty() {
            // Never an empty grid: hiding every column shows the first.
            order.push(0);
        }
        let (mut pinned, rest): (Vec<usize>, Vec<usize>) = order
            .into_iter()
            .partition(|ix| self.pinned.contains(&names[*ix]));
        let count = pinned.len();
        pinned.extend(rest);
        (pinned, count)
    }

    /// Every column in display order, hidden ones in their place.
    fn all(&self, names: &[String]) -> Vec<String> {
        let mut all: Vec<String> = self
            .order
            .iter()
            .filter(|name| names.contains(name))
            .cloned()
            .collect();
        for name in names {
            if !all.contains(name) {
                all.push(name.clone());
            }
        }
        all
    }

    /// Move a column one place up (`-1`) or down (`1`) among all columns.
    pub fn shift(&mut self, names: &[String], name: &str, by: isize) {
        let mut all = self.all(names);
        let Some(from) = all.iter().position(|n| n == name) else {
            return;
        };
        let to = from.saturating_add_signed(by).min(all.len() - 1);
        let column = all.remove(from);
        all.insert(to, column);
        self.order = all;
    }

    /// Move the column shown at `from` to `to`, as a header drag does.
    pub fn moved(&mut self, names: &[String], shown: &[usize], from: usize, to: usize) {
        let (Some(&from), Some(&to)) = (shown.get(from), shown.get(to)) else {
            return;
        };
        let mut all = self.all(names);
        let (Some(a), Some(b)) = (
            all.iter().position(|n| *n == names[from]),
            all.iter().position(|n| *n == names[to]),
        ) else {
            return;
        };
        let column = all.remove(a);
        all.insert(b, column);
        self.order = all;
    }

    fn toggle(list: &mut Vec<String>, name: &str) {
        match list.iter().position(|n| n == name) {
            Some(ix) => {
                list.remove(ix);
            }
            None => list.push(name.to_owned()),
        }
    }

    pub fn toggle_hidden(&mut self, name: &str) {
        Self::toggle(&mut self.hidden, name);
    }

    pub fn toggle_pinned(&mut self, name: &str) {
        Self::toggle(&mut self.pinned, name);
    }
}

/// Where `tables.toml` lives. The workspace sets it; without it nothing is
/// remembered.
pub struct TablePrefsFile(pub PathBuf);

impl Global for TablePrefsFile {}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PrefsFile {
    #[serde(default)]
    tables: BTreeMap<String, ColumnLayout>,
}

fn read(path: &std::path::Path) -> PrefsFile {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

/// The remembered layout of the table `key` names.
pub fn remembered(key: &str, cx: &App) -> ColumnLayout {
    cx.try_global::<TablePrefsFile>()
        .map(|file| read(&file.0).tables.remove(key).unwrap_or_default())
        .unwrap_or_default()
}

/// Remember a table's layout; the default layout forgets it.
pub fn remember(key: &str, layout: &ColumnLayout, cx: &App) {
    let Some(file) = cx.try_global::<TablePrefsFile>() else {
        return;
    };
    let mut prefs = read(&file.0);
    if layout.is_default() {
        prefs.tables.remove(key);
    } else {
        prefs.tables.insert(key.to_owned(), layout.clone());
    }
    if let Ok(text) = toml::to_string_pretty(&prefs) {
        if let Some(dir) = file.0.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let staged = file.0.with_extension("toml.tmp");
        if std::fs::write(&staged, text).is_ok() {
            let _ = std::fs::rename(&staged, &file.0);
        }
    }
}

/// The "Columns" dialog: show, pin and order each column.
pub struct ColumnsDialog {
    pub(super) view: WeakEntity<TableView>,
}

impl ColumnsDialog {
    fn change(&self, cx: &mut Context<Self>, change: impl FnOnce(&mut ColumnLayout, &[String])) {
        let _ = self.view.update(cx, |view, cx| {
            let names = view.column_names();
            let mut layout = view.layout.clone();
            change(&mut layout, &names);
            view.set_layout(layout, cx);
        });
        cx.notify();
    }
}

impl Render for ColumnsDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(view) = self.view.upgrade() else {
            return div();
        };
        let (names, layout, types) = {
            let view = view.read(cx);
            (
                view.column_names(),
                view.layout.clone(),
                view.columns
                    .iter()
                    .map(|c| c.type_name.clone())
                    .collect::<Vec<_>>(),
            )
        };
        let quiet = cx.theme().muted_foreground;
        let all = layout.all(&names);
        let last = all.len().saturating_sub(1);
        let mut list = v_flex()
            .id("columns-list")
            .test_support()
            .gap_1()
            .max_h(px(420.))
            .overflow_y_scroll();
        for (ix, name) in all.iter().enumerate() {
            let shown = !layout.hidden.contains(name);
            let pinned = layout.pinned.contains(name);
            let type_name = names
                .iter()
                .position(|n| n == name)
                .and_then(|i| types.get(i).cloned())
                .unwrap_or_default();
            let (n1, n2, n3, n4) = (name.clone(), name.clone(), name.clone(), name.clone());
            list = list.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .py_1()
                    .child(
                        Checkbox::new(("column-shown", ix))
                            .checked(shown)
                            .label(name.clone())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.change(cx, |layout, _| layout.toggle_hidden(&n1));
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .text_xs()
                            .text_color(quiet)
                            .font_family(crate::design::MONO_FONT)
                            .child(type_name),
                    )
                    .child(
                        Button::new(("column-pin", ix))
                            .ghost()
                            .small()
                            .icon(crate::ui::icon(Glyph::Pin))
                            .selected(pinned)
                            .tooltip(if pinned { "Unpin" } else { "Pin at the left" })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.change(cx, |layout, _| layout.toggle_pinned(&n2));
                            })),
                    )
                    .child(
                        Button::new(("column-up", ix))
                            .ghost()
                            .small()
                            .icon(crate::ui::icon(Glyph::ArrowUp))
                            .tooltip("Move up")
                            .disabled(ix == 0)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.change(cx, |layout, names| layout.shift(names, &n3, -1));
                            })),
                    )
                    .child(
                        Button::new(("column-down", ix))
                            .ghost()
                            .small()
                            .icon(crate::ui::icon(Glyph::ArrowDown))
                            .tooltip("Move down")
                            .disabled(ix == last)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.change(cx, |layout, names| layout.shift(names, &n4, 1));
                            })),
                    ),
            );
        }
        v_flex()
            .gap_4()
            .child(
                div()
                    .text_sm()
                    .text_color(quiet)
                    .child("Changes show at once and are remembered for this table. You can also drag a column's header."),
            )
            .child(list)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("columns-reset")
                            .ghost()
                            .label("Show all, in table order")
                            .disabled(layout.is_default())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.change(cx, |layout, _| *layout = ColumnLayout::default());
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("columns-done")
                            .primary()
                            .label("Done")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::ColumnLayout;

    fn names() -> Vec<String> {
        ["id", "name", "email", "country"]
            .map(String::from)
            .to_vec()
    }

    #[test]
    fn arrange_should_hide_order_and_pin() {
        let layout = ColumnLayout {
            order: vec!["email".into(), "id".into()],
            hidden: vec!["name".into()],
            pinned: vec!["country".into()],
        };

        assert_eq!(layout.arrange(&names()), (vec![3, 2, 0], 1));
    }

    #[test]
    fn shift_and_moved_should_reorder_every_column() {
        let mut layout = ColumnLayout::default();
        layout.shift(&names(), "email", -1);
        assert_eq!(layout.arrange(&names()).0, [0, 2, 1, 3]);

        let (shown, _) = layout.arrange(&names());
        layout.moved(&names(), &shown, 3, 0);
        assert_eq!(layout.arrange(&names()).0, [3, 0, 2, 1]);
    }
}
