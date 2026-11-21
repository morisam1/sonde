//! A table tab's Summary mode: group the filtered rows by a column, compute
//! a count, sum, average, minimum or maximum for each group, and draw the
//! groups as bars. The statement it runs can be opened in a query tab.

use std::sync::Arc;

use futures::StreamExt as _;
use gpui_kit::component::{
    ActiveTheme as _, IndexPath, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    select::{Select, SelectEvent, SelectState},
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use sonde_core::{
    DbKind, Session, TableRef, Value,
    filter::{self, Bucket, Class, Measure, Summary},
};

use crate::query::cell;

type Choice = SelectState<Vec<SharedString>>;

/// What the panel asks of its tab.
pub enum SummaryEvent {
    /// Open the statement in a query tab.
    OpenQuery(String),
}

struct Group {
    label: SharedString,
    value: Option<f64>,
    shown: SharedString,
}

enum Outcome {
    Idle,
    Running(#[allow(dead_code)] Task<()>),
    Done(Vec<Group>),
    Failed(SharedString),
}

pub struct SummaryPanel {
    kind: DbKind,
    table: TableRef,
    session: Option<Arc<dyn Session>>,
    /// Columns as `(name, type)`.
    columns: Vec<(String, String)>,
    /// The filter the rows are narrowed by, as the tab built it.
    condition: String,
    group: Entity<Choice>,
    bucket: Entity<Choice>,
    measure: Entity<Choice>,
    of: Entity<Choice>,
    /// Columns `of` lists: the numeric ones, or every one for min and max.
    of_columns: Vec<String>,
    sql: Option<String>,
    outcome: Outcome,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SummaryEvent> for SummaryPanel {}

const NO_GROUP: &str = "Nothing (one total)";

impl SummaryPanel {
    pub fn new(
        kind: DbKind,
        table: TableRef,
        columns: Vec<(String, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut groups: Vec<SharedString> = vec![NO_GROUP.into()];
        groups.extend(
            columns
                .iter()
                .filter(|(_, t)| Class::of(t) != Class::Nested)
                .map(|(name, _)| SharedString::from(name.clone())),
        );
        // Start on something worth seeing: the first date by month, or the
        // first column that is not a key.
        let start = columns
            .iter()
            .position(|(_, t)| Class::of(t) == Class::Date)
            .or_else(|| {
                columns.iter().position(|(name, t)| {
                    name != "id" && name != "_id" && Class::of(t) == Class::Text
                })
            })
            .and_then(|ix| groups.iter().position(|g| g.as_ref() == columns[ix].0))
            .unwrap_or(0);
        let is_date = start > 0
            && columns
                .iter()
                .find(|(name, _)| *name == groups[start].as_ref())
                .is_some_and(|(_, t)| Class::of(t) == Class::Date);
        let group = cx.new(|cx| {
            SelectState::new(groups, Some(IndexPath::new(start)), window, cx).searchable(true)
        });
        let buckets: Vec<SharedString> = Bucket::ALL
            .iter()
            .map(|b| SharedString::from(b.label()))
            .collect();
        let bucket = cx.new(|cx| {
            SelectState::new(
                buckets,
                Some(IndexPath::new(if is_date { 2 } else { 0 })),
                window,
                cx,
            )
        });
        let measures: Vec<SharedString> = Measure::ALL
            .iter()
            .map(|m| SharedString::from(m.label()))
            .collect();
        let measure = cx.new(|cx| SelectState::new(measures, Some(IndexPath::new(0)), window, cx));
        let of_columns = numeric(&columns);
        let of = cx.new(|cx| {
            SelectState::new(
                of_columns
                    .iter()
                    .cloned()
                    .map(SharedString::from)
                    .collect::<Vec<_>>(),
                Some(IndexPath::new(0)),
                window,
                cx,
            )
        });
        let mut subscriptions = Vec::new();
        for choice in [&group, &bucket, &measure, &of] {
            subscriptions.push(cx.subscribe_in(
                choice,
                window,
                |this, choice, _: &SelectEvent<Vec<SharedString>>, window, cx| {
                    if *choice == this.group {
                        this.group_changed(window, cx);
                    } else if *choice == this.measure {
                        this.measure_changed(window, cx);
                    }
                    this.run(cx);
                },
            ));
        }
        Self {
            kind,
            table,
            session: None,
            columns,
            condition: String::new(),
            group,
            bucket,
            measure,
            of,
            of_columns,
            sql: None,
            outcome: Outcome::Idle,
            _subscriptions: subscriptions,
        }
    }

    fn type_of(&self, name: &str) -> Option<&str> {
        self.columns
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, t)| t.as_str())
    }

    fn group_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let date = self
            .chosen_group(cx)
            .and_then(|g| self.type_of(&g))
            .is_some_and(|t| Class::of(t) == Class::Date);
        let bucket = if date { 2 } else { 0 };
        self.bucket.update(cx, |state, cx| {
            state.set_selected_index(Some(IndexPath::new(bucket)), window, cx)
        });
    }

    fn measure_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let measure = self.chosen_measure(cx);
        // The smallest or largest of anything sorts; sums need numbers.
        let columns = if matches!(measure, Measure::Min | Measure::Max) {
            self.columns
                .iter()
                .filter(|(_, t)| Class::of(t) != Class::Nested)
                .map(|(n, _)| n.clone())
                .collect()
        } else {
            numeric(&self.columns)
        };
        if columns != self.of_columns {
            self.of_columns = columns;
            let items: Vec<SharedString> = self
                .of_columns
                .iter()
                .cloned()
                .map(SharedString::from)
                .collect();
            self.of.update(cx, |state, cx| {
                state.set_items(items, window, cx);
                state.set_selected_index(Some(IndexPath::new(0)), window, cx);
            });
        }
    }

    fn chosen_group(&self, cx: &App) -> Option<String> {
        self.group
            .read(cx)
            .selected_value()
            .filter(|g| g.as_ref() != NO_GROUP)
            .map(ToString::to_string)
    }

    fn chosen_measure(&self, cx: &App) -> Measure {
        self.measure
            .read(cx)
            .selected_index(cx)
            .and_then(|ix| Measure::ALL.get(ix.row).copied())
            .unwrap_or_default()
    }

    /// The summary the choices describe.
    pub fn summary(&self, cx: &App) -> Summary {
        let group = self.chosen_group(cx);
        let date = group
            .as_deref()
            .and_then(|g| self.type_of(g))
            .is_some_and(|t| Class::of(t) == Class::Date);
        let bucket = if date {
            self.bucket
                .read(cx)
                .selected_index(cx)
                .and_then(|ix| Bucket::ALL.get(ix.row).copied())
                .unwrap_or_default()
        } else {
            Bucket::Value
        };
        let measure = self.chosen_measure(cx);
        Summary {
            group,
            bucket,
            measure,
            of: measure
                .needs_column()
                .then(|| self.of.read(cx).selected_value().map(ToString::to_string))
                .flatten(),
        }
    }

    /// Summarize on `session`, over the rows `condition` keeps.
    pub fn show(&mut self, session: Arc<dyn Session>, condition: String, cx: &mut Context<Self>) {
        self.session = Some(session);
        self.condition = condition;
        self.run(cx);
    }

    pub fn run(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let summary = self.summary(cx);
        let sql = match filter::summary(self.kind, &self.table, &self.condition, &summary) {
            Ok(sql) => sql,
            Err(message) => {
                self.sql = None;
                self.outcome = Outcome::Failed(message.into());
                cx.notify();
                return;
            }
        };
        self.sql = Some(sql.clone());
        let grouped = summary.group.is_some();
        self.outcome = Outcome::Running(cx.spawn(async move |this, cx| {
            let result = async {
                let mut handle = session.execute(&sql, &[]).await?;
                let mut rows = Vec::new();
                while let Some(page) = handle.pages.next().await {
                    rows.extend(page?.rows);
                }
                Ok::<_, sonde_core::Error>(rows)
            }
            .await;
            let _ = this.update(cx, |this, cx| {
                this.outcome = match result {
                    Ok(rows) => Outcome::Done(groups(rows, grouped)),
                    Err(error) => Outcome::Failed(error.to_string().into()),
                };
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub fn is_running(&self) -> bool {
        matches!(self.outcome, Outcome::Running(_))
    }

    /// The groups as `(label, value)` text, once they are in. For tests.
    pub fn results(&self) -> Option<Vec<(String, String)>> {
        match &self.outcome {
            Outcome::Done(groups) => Some(
                groups
                    .iter()
                    .map(|g| (g.label.to_string(), g.shown.to_string()))
                    .collect(),
            ),
            _ => None,
        }
    }

    /// Choose what to group on. For tests.
    pub fn group_by(&mut self, column: &str, window: &mut Window, cx: &mut Context<Self>) {
        let value = SharedString::from(column.to_owned());
        self.group
            .update(cx, |state, cx| state.set_selected_value(&value, window, cx));
        self.group_changed(window, cx);
        self.run(cx);
    }

    pub fn problem(&self) -> Option<SharedString> {
        match &self.outcome {
            Outcome::Failed(message) => Some(message.clone()),
            _ => None,
        }
    }
}

fn numeric(columns: &[(String, String)]) -> Vec<String> {
    columns
        .iter()
        .filter(|(_, t)| Class::of(t) == Class::Number)
        .map(|(n, _)| n.clone())
        .collect()
}

fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Int(n) => Some(*n as f64),
        Value::Float(n) => Some(*n),
        Value::Null => None,
        other => other.to_string().parse().ok(),
    }
}

fn groups(rows: Vec<Vec<Value>>, grouped: bool) -> Vec<Group> {
    rows.into_iter()
        .map(|row| {
            let (label, value) = if grouped {
                (
                    row.first().cloned().unwrap_or(Value::Null),
                    row.get(1).cloned().unwrap_or(Value::Null),
                )
            } else {
                (
                    Value::text("All rows"),
                    row.first().cloned().unwrap_or(Value::Null),
                )
            };
            Group {
                label: cell::display(&label).unwrap_or_else(|| "NULL".into()),
                value: number(&value),
                shown: cell::display(&value).unwrap_or_else(|| "NULL".into()),
            }
        })
        .collect()
}

impl Render for SummaryPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (quiet, danger, bar, border) = (
            theme.muted_foreground,
            theme.danger,
            theme.primary,
            theme.border,
        );
        let summary = self.summary(cx);
        let date = summary
            .group
            .as_deref()
            .and_then(|g| self.type_of(g))
            .is_some_and(|t| Class::of(t) == Class::Date);
        let needs_column = summary.measure.needs_column();
        let label = |text: &'static str| div().text_sm().text_color(quiet).child(text);
        let controls = h_flex()
            .flex_wrap()
            .gap_3()
            .items_center()
            .child(label("Group by"))
            .child(
                div().w(px(200.)).child(
                    Select::new(&self.group)
                        .id("summary-group")
                        .small()
                        .accessibility_label("Group by"),
                ),
            )
            .when(date, |this| {
                this.child(label("by")).child(
                    div().w(px(130.)).child(
                        Select::new(&self.bucket)
                            .id("summary-bucket")
                            .small()
                            .accessibility_label("Group dates by"),
                    ),
                )
            })
            .child(label("show"))
            .child(
                div().w(px(160.)).child(
                    Select::new(&self.measure)
                        .id("summary-measure")
                        .small()
                        .accessibility_label("Measure"),
                ),
            )
            .when(needs_column, |this| {
                this.child(
                    div().w(px(180.)).child(
                        Select::new(&self.of)
                            .id("summary-of")
                            .small()
                            .placeholder("no number columns")
                            .accessibility_label("Measure column"),
                    ),
                )
            })
            .child(div().flex_1())
            .when_some(self.sql.clone(), |this, sql| {
                this.child(
                    Button::new("summary-open-query")
                        .ghost()
                        .small()
                        .label("Open as query")
                        .tooltip("Open the statement behind this summary in a query tab")
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(SummaryEvent::OpenQuery(sql.clone()))
                        })),
                )
            });

        let body = match &self.outcome {
            Outcome::Idle => div()
                .text_sm()
                .text_color(quiet)
                .child("Choose what to group by.")
                .into_any_element(),
            Outcome::Running(_) => div()
                .text_sm()
                .text_color(quiet)
                .child("Summarizing…")
                .into_any_element(),
            Outcome::Failed(message) => div()
                .id("summary-problem")
                .test_support()
                .aria_label(message.clone())
                .text_sm()
                .text_color(danger)
                .child(message.clone())
                .into_any_element(),
            Outcome::Done(groups) if groups.is_empty() => div()
                .text_sm()
                .text_color(quiet)
                .child("No rows to summarize.")
                .into_any_element(),
            Outcome::Done(groups) => {
                let largest = groups
                    .iter()
                    .filter_map(|g| g.value)
                    .fold(0.0_f64, |a, b| a.max(b.abs()));
                let mut list = v_flex().gap_1();
                for (ix, group) in groups.iter().enumerate() {
                    let share = match (group.value, largest > 0.0) {
                        (Some(v), true) => (v.abs() / largest) as f32,
                        _ => 0.0,
                    };
                    list = list.child(
                        h_flex()
                            .id(("summary-group-row", ix))
                            .test_support()
                            .aria_label(SharedString::from(format!(
                                "{}: {}",
                                group.label, group.shown
                            )))
                            .gap_3()
                            .items_center()
                            .h(px(26.))
                            .child(
                                div()
                                    .w(px(200.))
                                    .flex_shrink_0()
                                    .truncate()
                                    .text_sm()
                                    .font_family(crate::design::MONO_FONT)
                                    .child(group.label.clone()),
                            )
                            .child(
                                div().flex_1().h(px(14.)).child(
                                    div()
                                        .h_full()
                                        .w(relative(share.max(0.004)))
                                        .rounded(px(3.))
                                        .bg(bar),
                                ),
                            )
                            .child(
                                div()
                                    .w(px(120.))
                                    .flex_shrink_0()
                                    .text_right()
                                    .text_sm()
                                    .font_family(crate::design::MONO_FONT)
                                    .child(group.shown.clone()),
                            ),
                    );
                }
                let note = (groups.len() == filter::SUMMARY_GROUPS)
                    .then(|| format!("Showing the first {} groups.", filter::SUMMARY_GROUPS));
                v_flex()
                    .gap_2()
                    .child(list)
                    .when_some(note, |this, note| {
                        this.child(div().text_xs().text_color(quiet).child(note))
                    })
                    .into_any_element()
            }
        };

        v_flex()
            .id("summary-panel")
            .size_full()
            .gap_4()
            .p_5()
            .child(controls)
            .child(div().border_b_1().border_color(border))
            .child(
                div()
                    .id("summary-results")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body),
            )
    }
}
