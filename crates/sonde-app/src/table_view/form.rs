//! A row as a form: one field per column, for rows too wide for the grid,
//! values too long for a cell, and new rows. Saving stages the changes, as
//! cell edits do; nothing is written until Apply.

use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    input::{Editor, EditorState, Input, InputState},
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use sonde_core::{Value, filter::Class};

use super::{Place, TableView, parse_cell};

enum Widget {
    Line(Entity<InputState>),
    Block(Entity<EditorState>),
}

struct Field {
    column: usize,
    name: String,
    type_name: String,
    nullable: bool,
    key: bool,
    /// The table a foreign key on this column points at.
    references: Option<String>,
    /// The text the field started with, to tell what changed.
    original: String,
    was_null: bool,
    /// Whether a new row's field was left to its default.
    is_default: bool,
    null: bool,
    widget: Widget,
}

impl Field {
    fn text(&self, cx: &App) -> String {
        match &self.widget {
            Widget::Line(input) => input.read(cx).value().to_string(),
            Widget::Block(editor) => editor.read(cx).value().to_string(),
        }
    }
}

pub struct RowForm {
    view: WeakEntity<TableView>,
    place: Place,
    fields: Vec<Field>,
    read_only: bool,
    problem: Option<SharedString>,
}

/// The text a value is edited as. JSON is laid out on several lines.
fn edit_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Json(text) => serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .and_then(|json| serde_json::to_string_pretty(&json).ok())
            .unwrap_or_else(|| text.to_string()),
        other => other.to_string(),
    }
}

impl RowForm {
    pub(super) fn new(
        view: &TableView,
        place: Place,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let values: Vec<Option<Value>> = {
            let grid = view.table.read(cx).delegate();
            (0..view.columns.len())
                .map(|column| grid.current(place, column).cloned())
                .collect()
        };
        let detail = view.detail.as_ref();
        let mut fields = Vec::new();
        for (column, info) in view.columns.iter().enumerate() {
            if info.name == "rowid" && view.key_columns == ["rowid"] {
                continue;
            }
            let value = values[column].clone();
            let column_info = detail.and_then(|d| d.columns.iter().find(|c| c.name == info.name));
            let references = detail.and_then(|d| {
                d.foreign_keys
                    .iter()
                    .find(|fk| fk.columns.first() == Some(&info.name))
                    .map(|fk| fk.references.name.clone())
            });
            let text = value.as_ref().map(edit_text).unwrap_or_default();
            let block = Class::of(&info.type_name) == Class::Nested
                || matches!(value, Some(Value::Json(_)))
                || text.len() > 80
                || text.contains('\n');
            let placeholder = match &value {
                None => "default",
                Some(Value::Null) => "NULL",
                _ => "",
            };
            let widget = if block {
                Widget::Block(cx.new(|cx| {
                    EditorState::new(window, cx)
                        .language(if Class::of(&info.type_name) == Class::Nested {
                            "json"
                        } else {
                            "text"
                        })
                        .default_value(text.clone())
                }))
            } else {
                Widget::Line(cx.new(|cx| {
                    InputState::new(window, cx)
                        .default_value(text.clone())
                        .placeholder(placeholder)
                }))
            };
            fields.push(Field {
                column,
                name: info.name.clone(),
                type_name: info.type_name.clone(),
                nullable: column_info.is_none_or(|c| c.nullable),
                key: view.key_columns.contains(&info.name),
                references,
                original: text,
                was_null: matches!(value, Some(Value::Null)),
                is_default: value.is_none(),
                null: matches!(value, Some(Value::Null)),
                widget,
            });
        }
        Self {
            view: view.weak.clone(),
            place,
            fields,
            read_only: !view.is_editable(),
            problem: None,
        }
    }

    /// Stage what changed, then close.
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut cells = Vec::new();
        for field in &self.fields {
            let text = field.text(cx);
            if field.null {
                if !field.was_null {
                    cells.push((field.column, Some(Value::Null)));
                }
                continue;
            }
            let untouched = text == field.original && !field.was_null;
            if untouched || (field.is_default && text.is_empty()) {
                continue;
            }
            match parse_cell(&field.type_name, None, &text) {
                Ok(value) => cells.push((field.column, Some(value))),
                Err(message) => {
                    self.problem = Some(format!("{}: {message}", field.name).into());
                    cx.notify();
                    return;
                }
            }
        }
        let place = self.place;
        let _ = self
            .view
            .update(cx, |view, cx| view.stage_cells(place, cells, window, cx));
        window.close_dialog(cx);
    }

    fn follow(&self, field: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(field) = self.fields.get(field) else {
            return;
        };
        let (column, place) = (field.column, self.place);
        let _ = self
            .view
            .update(cx, |view, cx| view.follow_key(place, column, cx));
        window.close_dialog(cx);
    }
}

impl Render for RowForm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (quiet, danger, border) = (theme.muted_foreground, theme.danger, theme.border);
        let height = (window.viewport_size().height * 0.6).clamp(px(220.), px(640.));
        let mut list = v_flex()
            .id("row-form-fields")
            .test_support()
            .gap_4()
            .max_h(height)
            .overflow_y_scroll()
            .pr_2();
        for (ix, field) in self.fields.iter().enumerate() {
            let mut label = h_flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child(field.name.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(quiet)
                        .font_family(crate::design::MONO_FONT)
                        .child(field.type_name.clone()),
                )
                .when(field.key, |this| this.child(crate::ui::badge("key", cx)))
                .when(!field.nullable, |this| {
                    this.child(crate::ui::badge("required", cx))
                });
            if let Some(table) = &field.references {
                label = label.child(div().flex_1()).child(
                    Button::new(("row-form-follow", ix))
                        .ghost()
                        .xsmall()
                        .label(format!("Open in {table}"))
                        .tooltip("Show the row this value points at")
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.follow(ix, window, cx)),
                        ),
                );
            }
            let disabled = self.read_only || field.null;
            let widget = match &field.widget {
                Widget::Line(input) => Input::new(input)
                    .disabled(disabled)
                    .aria_label(format!("{} value", field.name))
                    .into_any_element(),
                Widget::Block(editor) => div()
                    .h(px(140.))
                    .p_2()
                    .border_1()
                    .border_color(border)
                    .rounded(cx.theme().radius)
                    .font_family(crate::design::MONO_FONT)
                    .child(
                        Editor::new(editor)
                            .h(relative(1.))
                            .appearance(false)
                            .readonly(disabled)
                            .aria_label(format!("{} value", field.name)),
                    )
                    .into_any_element(),
            };
            let null = field.null;
            list = list.child(v_flex().gap_2().child(label).child(widget).when(
                field.nullable && !self.read_only,
                |this| {
                    this.child(
                        Checkbox::new(("row-form-null", ix))
                            .checked(null)
                            .small()
                            .label("NULL")
                            .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                if let Some(field) = this.fields.get_mut(ix) {
                                    field.null = *checked;
                                }
                                cx.notify();
                            })),
                    )
                },
            ));
        }
        let new_row = matches!(self.place, Place::New(_));
        v_flex()
            .gap_4()
            .child(list)
            .child(
                div()
                    .id("row-form-problem")
                    .min_h_5()
                    .text_sm()
                    .text_color(danger)
                    .children(self.problem.clone()),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1())
                    .child(
                        Button::new("row-form-cancel")
                            .ghost()
                            .label(if self.read_only { "Close" } else { "Cancel" })
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .when(!self.read_only, |this| {
                        this.child(
                            Button::new("row-form-stage")
                                .primary()
                                .label(if new_row {
                                    "Stage new row"
                                } else {
                                    "Stage changes"
                                })
                                .tooltip("Nothing is written until Apply")
                                .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                        )
                    }),
            )
    }
}

impl RowForm {
    /// For tests: type into a field.
    pub fn set_text(
        &mut self,
        name: &str,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(field) = self.fields.iter().find(|f| f.name == name) {
            match &field.widget {
                Widget::Line(input) => {
                    input.update(cx, |input, cx| input.set_value(text.to_owned(), window, cx))
                }
                Widget::Block(editor) => editor.update(cx, |editor, cx| {
                    editor.set_value(text.to_owned(), window, cx)
                }),
            }
        }
    }

    /// For tests: stage the form.
    pub fn stage(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save(window, cx);
    }
}
