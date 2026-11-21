//! The filter builder: rules picked from lists (column, condition, value)
//! instead of a typed `WHERE` condition.

use gpui_kit::component::{
    ActiveTheme as _, IndexPath, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    select::{Select, SelectEvent, SelectState},
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use sonde_core::{
    DbKind,
    filter::{self, ANY_FIELD, Class, Filter, Op, Rule},
};

use crate::ui::Glyph;

type Choice = SelectState<Vec<SharedString>>;

/// What the builder asks of the table view.
pub enum BuilderEvent {
    /// Run the rules: Enter in a value box, or Apply.
    Apply,
}

struct RuleRow {
    column: Entity<Choice>,
    op: Entity<Choice>,
    /// The conditions `op` lists, for the chosen column.
    ops: Vec<Op>,
    value: Entity<InputState>,
    value2: Entity<InputState>,
}

pub struct FilterBuilder {
    kind: DbKind,
    /// Columns as `(name, type)`, with the any-field search first where the
    /// database has one.
    columns: Vec<(String, String)>,
    rows: Vec<RuleRow>,
    any: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<BuilderEvent> for FilterBuilder {}

impl FilterBuilder {
    pub fn new(kind: DbKind, columns: Vec<(String, String)>) -> Self {
        let mut columns: Vec<(String, String)> = columns
            .into_iter()
            .filter(|(_, type_name)| !filter::ops(kind, Class::of(type_name)).is_empty())
            .collect();
        if kind == DbKind::Elasticsearch {
            columns.insert(0, (ANY_FIELD.to_owned(), "text".to_owned()));
        }
        Self {
            kind,
            columns,
            rows: Vec::new(),
            any: false,
            _subscriptions: Vec::new(),
        }
    }

    fn label(name: &str) -> SharedString {
        if name == ANY_FIELD {
            "Any field".into()
        } else {
            name.to_owned().into()
        }
    }

    fn ops_for(&self, column: usize) -> Vec<Op> {
        let (name, type_name) = &self.columns[column];
        if name == ANY_FIELD {
            return vec![Op::Matches];
        }
        filter::ops(self.kind, Class::of(type_name))
    }

    /// Add a rule on `column` (the first column when `None`), and put the
    /// keyboard in its value box.
    pub fn add_rule(&mut self, column: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        let column = column
            .and_then(|name| self.columns.iter().position(|(c, _)| c == name))
            .unwrap_or(0);
        if self.columns.is_empty() {
            return;
        }
        let rule = Rule::new(
            &self.columns[column].0,
            &self.columns[column].1,
            self.ops_for(column)[0],
            "",
        );
        self.push(&rule, window, cx);
        if let Some(row) = self.rows.last() {
            row.value.update(cx, |input, cx| input.focus(window, cx));
        }
        cx.notify();
    }

    fn push(&mut self, rule: &Rule, window: &mut Window, cx: &mut Context<Self>) {
        let Some(column) = self
            .columns
            .iter()
            .position(|(name, _)| *name == rule.column)
        else {
            return;
        };
        let names: Vec<SharedString> = self
            .columns
            .iter()
            .map(|(name, _)| Self::label(name))
            .collect();
        let ops = self.ops_for(column);
        let op_ix = ops.iter().position(|op| *op == rule.op).unwrap_or(0);
        let column_choice = cx.new(|cx| {
            SelectState::new(names, Some(IndexPath::new(column)), window, cx).searchable(true)
        });
        let op_choice =
            cx.new(|cx| SelectState::new(op_labels(&ops), Some(IndexPath::new(op_ix)), window, cx));
        let class = Class::of(&self.columns[column].1);
        let value = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(rule.value.clone())
                .placeholder(ops[op_ix].placeholder(class))
        });
        let value2 = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(rule.value2.clone())
                .placeholder(ops[op_ix].placeholder(class))
        });
        for input in [&value, &value2] {
            self._subscriptions
                .push(cx.subscribe(input, |_, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        cx.emit(BuilderEvent::Apply);
                    }
                }));
        }
        self._subscriptions.push(cx.subscribe_in(
            &column_choice,
            window,
            |this, choice, _: &SelectEvent<Vec<SharedString>>, window, cx| {
                this.column_changed(choice, window, cx);
            },
        ));
        self._subscriptions.push(cx.subscribe_in(
            &op_choice,
            window,
            |this, _, _: &SelectEvent<Vec<SharedString>>, _, cx| {
                // A condition with no value runs as soon as it is chosen.
                cx.notify();
                let _ = this;
            },
        ));
        self.rows.push(RuleRow {
            column: column_choice,
            op: op_choice,
            ops,
            value,
            value2,
        });
    }

    /// A new column brings its own conditions; the value stays.
    fn column_changed(
        &mut self,
        choice: &Entity<Choice>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row_ix) = self.rows.iter().position(|row| row.column == *choice) else {
            return;
        };
        let Some(column) = choice.read(cx).selected_index(cx).map(|ix| ix.row) else {
            return;
        };
        let ops = self.ops_for(column);
        let row = &mut self.rows[row_ix];
        let kept = row
            .op
            .read(cx)
            .selected_index(cx)
            .and_then(|ix| row.ops.get(ix.row).copied())
            .and_then(|op| ops.iter().position(|o| *o == op))
            .unwrap_or(0);
        row.op.update(cx, |state, cx| {
            state.set_items(op_labels(&ops), window, cx);
            state.set_selected_index(Some(IndexPath::new(kept)), window, cx);
        });
        let class = Class::of(&self.columns[column].1);
        let placeholder = ops[kept].placeholder(class);
        for input in [&row.value, &row.value2] {
            input.update(cx, |input, cx| {
                input.set_placeholder(placeholder, window, cx)
            });
        }
        row.ops = ops;
        cx.notify();
    }

    fn remove(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.rows.len() {
            self.rows.remove(ix);
            cx.emit(BuilderEvent::Apply);
            cx.notify();
        }
    }

    /// Drop every rule.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.rows.clear();
        cx.notify();
    }

    /// Replace the rules with `filter`'s. Rules on columns the table no
    /// longer has are dropped.
    pub fn set_filter(&mut self, filter: &Filter, window: &mut Window, cx: &mut Context<Self>) {
        self.rows.clear();
        self.any = filter.any && filter::supports_any(self.kind);
        for rule in &filter.rules {
            self.push(rule, window, cx);
        }
        cx.notify();
    }

    /// The rules as they stand.
    pub fn filter(&self, cx: &App) -> Filter {
        let rules = self
            .rows
            .iter()
            .filter_map(|row| {
                let column = row.column.read(cx).selected_index(cx)?.row;
                let op = *row.ops.get(row.op.read(cx).selected_index(cx)?.row)?;
                let (name, type_name) = self.columns.get(column)?;
                let mut rule = Rule::new(name, type_name, op, &row.value.read(cx).value());
                rule.value2 = row.value2.read(cx).value().to_string();
                Some(rule)
            })
            .collect();
        Filter {
            rules,
            any: self.any,
        }
    }

    /// How many rules there are.
    pub fn rule_count(&self) -> usize {
        self.rows.len()
    }

    /// Set a rule's value, as typing would. For tests.
    pub fn set_value(
        &mut self,
        ix: usize,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.rows.get(ix) {
            row.value
                .update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
        }
    }

    /// Choose a rule's condition. For tests.
    pub fn set_op(&mut self, ix: usize, op: Op, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get(ix)
            && let Some(position) = row.ops.iter().position(|o| *o == op)
        {
            row.op.update(cx, |state, cx| {
                state.set_selected_index(Some(IndexPath::new(position)), window, cx);
            });
            cx.notify();
        }
    }
}

fn op_labels(ops: &[Op]) -> Vec<SharedString> {
    ops.iter()
        .map(|op| SharedString::from(op.label()))
        .collect()
}

impl Render for FilterBuilder {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let quiet = cx.theme().muted_foreground;
        let mut list = v_flex().gap_2();
        for (ix, row) in self.rows.iter().enumerate() {
            let values = row
                .op
                .read(cx)
                .selected_index(cx)
                .and_then(|i| row.ops.get(i.row))
                .map_or(1, |op| op.values());
            let joiner = match (ix, self.any) {
                (0, _) => "Where",
                (_, true) => "or",
                (_, false) => "and",
            };
            list = list.child(
                h_flex()
                    .id(("filter-rule", ix))
                    .flex_wrap()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(px(52.))
                            .flex_shrink_0()
                            .text_sm()
                            .text_color(quiet)
                            .child(joiner),
                    )
                    .child(
                        div().w(px(180.)).child(
                            Select::new(&row.column)
                                .id(("filter-column", ix))
                                .small()
                                .accessibility_label(format!("Rule {} column", ix + 1)),
                        ),
                    )
                    .child(
                        div().w(px(170.)).child(
                            Select::new(&row.op)
                                .id(("filter-op", ix))
                                .small()
                                .accessibility_label(format!("Rule {} condition", ix + 1)),
                        ),
                    )
                    .when(values > 0, |this| {
                        this.child(
                            div().w(px(180.)).child(
                                Input::new(&row.value)
                                    .small()
                                    .aria_label(format!("Rule {} value", ix + 1)),
                            ),
                        )
                    })
                    .when(values > 1, |this| {
                        this.child(div().text_sm().text_color(quiet).child("and"))
                            .child(
                                div().w(px(180.)).child(
                                    Input::new(&row.value2)
                                        .small()
                                        .aria_label(format!("Rule {} second value", ix + 1)),
                                ),
                            )
                    })
                    .child(
                        Button::new(("filter-remove", ix))
                            .ghost()
                            .small()
                            .icon(crate::ui::icon(Glyph::X))
                            .tooltip("Remove this rule")
                            .on_click(cx.listener(move |this, _, _, cx| this.remove(ix, cx))),
                    ),
            );
        }
        let can_any = filter::supports_any(self.kind) && self.rows.len() > 1;
        let any = self.any;
        list.child(
            h_flex()
                .flex_wrap()
                .gap_2()
                .items_center()
                .child(
                    Button::new("filter-add-rule")
                        .ghost()
                        .small()
                        .icon(crate::ui::icon(Glyph::Plus))
                        .label("Add rule")
                        .on_click(cx.listener(|this, _, window, cx| this.add_rule(None, window, cx))),
                )
                .when(can_any, |this| {
                    this.child(
                        Button::new("filter-join")
                            .ghost()
                            .small()
                            .label(if any { "Match any rule" } else { "Match all rules" })
                            .tooltip("Switch between rows that match every rule and rows that match at least one")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.any = !this.any;
                                cx.notify();
                            })),
                    )
                }),
        )
    }
}
