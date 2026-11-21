//! The overview of whatever is selected in the sidebar tree, from metadata
//! the browser already loaded. A title with a line of facts under it, then
//! plain sections: a heading over a bordered table (shadcn's data table),
//! or a description list for a connection's settings.

use gpui_kit::{
    component::{ActiveTheme as _, h_flex, scroll::ScrollableElement as _, v_flex},
    prelude::FluentBuilder as _,
    *,
};
use sonde_core::{ConnectionConfig, TableDetail, TlsMode, schema::RelationKind};

use crate::{
    browser::Inspection,
    design::{self, MONO_FONT},
    ui::{self, Glyph},
};

pub(crate) fn render(inspection: Inspection<'_>, window: &Window, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let compact = window.viewport_size().width < px(900.);
    let title = inspection
        .table
        .as_ref()
        .map(|table| table.name.as_str())
        .or_else(|| inspection.schema.map(|schema| schema.name.as_str()))
        .unwrap_or(&inspection.connection.name);
    let (kind, kind_glyph) = if inspection.table.is_some() {
        ("Table", Glyph::Table)
    } else if inspection.schema.is_some() {
        ("Schema", Glyph::Layers)
    } else {
        (inspection.connection.config.kind().name(), Glyph::Database)
    };

    // The facts line: what kind of object, its state, and its counts.
    let mut facts: Vec<(Glyph, String)> = vec![(kind_glyph, kind.to_owned())];
    if let Some(detail) = inspection.detail {
        facts.push((
            Glyph::Columns3,
            counted(detail.columns.len(), "column", "columns"),
        ));
        facts.push((
            Glyph::Hash,
            counted(detail.indexes.len(), "index", "indexes"),
        ));
        facts.push((
            Glyph::Link,
            counted(detail.foreign_keys.len(), "foreign key", "foreign keys"),
        ));
    } else if inspection.table.is_none() {
        if let Some(schema) = inspection.schema {
            let tables = schema
                .relations
                .iter()
                .filter(|relation| {
                    matches!(
                        relation.kind,
                        RelationKind::Table | RelationKind::Collection | RelationKind::Key
                    )
                })
                .count();
            facts.push((Glyph::Table, counted(tables, "table", "tables")));
            facts.push((
                Glyph::Eye,
                counted(schema.relations.len() - tables, "view", "views"),
            ));
        } else if let Some(catalog) = inspection.catalog {
            let relations: usize = catalog
                .schemas
                .iter()
                .map(|schema| schema.relations.len())
                .sum();
            facts.push((
                Glyph::Layers,
                counted(catalog.schemas.len(), "schema", "schemas"),
            ));
            facts.push((Glyph::Table, counted(relations, "relation", "relations")));
        }
    }

    let status_color = if inspection.error {
        theme.danger
    } else if inspection.status == "Connected" {
        theme.success
    } else {
        theme.muted_foreground
    };
    let status = h_flex()
        .gap_2()
        .items_center()
        .text_color(theme.foreground)
        .child(div().size(px(8.)).rounded_full().bg(status_color))
        .child(inspection.status);

    let heading = v_flex()
        .gap_3()
        .child(
            div()
                .id("inspector-title")
                .test_support()
                .aria_label(format!("Inspecting {title}"))
                .text_3xl()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.to_owned()),
        )
        .child(
            h_flex()
                .flex_wrap()
                .gap_x_6()
                .gap_y_2()
                .items_center()
                .text_color(theme.muted_foreground)
                .child(status)
                .children(facts.into_iter().map(|(glyph, text)| {
                    let color = match glyph {
                        Glyph::Database => {
                            design::engine_ink(inspection.connection.config.kind(), cx)
                        }
                        Glyph::Table => design::tokens(cx).accent,
                        Glyph::Link => design::tokens(cx).key,
                        _ => theme.muted_foreground,
                    };
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(ui::icon(glyph).text_color(color))
                        .child(text)
                })),
        );

    let mut content = v_flex().gap_8().child(heading);

    if let Some(message) = inspection.message {
        content = content.child(
            h_flex()
                .id("inspector-message")
                .test_support()
                .role(Role::Status)
                .aria_label(message.to_owned())
                .gap_3()
                .px_5()
                .py_4()
                .rounded(px(crate::design::RADIUS_CARD))
                .border_1()
                .border_color(if inspection.error {
                    theme.danger.opacity(0.4)
                } else {
                    theme.border
                })
                .text_color(if inspection.error {
                    theme.danger
                } else {
                    theme.muted_foreground
                })
                .child(ui::icon(if inspection.error {
                    Glyph::CircleAlert
                } else {
                    Glyph::Info
                }))
                .child(message.to_owned()),
        );
    }

    if let Some(detail) = inspection.detail {
        content = content.child(table_detail(detail, cx));
    } else if inspection.table.is_none() {
        if let Some(schema) = inspection.schema {
            let rows: Vec<[String; 2]> = schema
                .relations
                .iter()
                .map(|relation| {
                    let kind = match relation.kind {
                        RelationKind::Table => "table",
                        RelationKind::View => "view",
                        RelationKind::MaterializedView => "materialized view",
                        RelationKind::ForeignTable => "foreign table",
                        RelationKind::Collection => "collection",
                        RelationKind::Key => "key",
                    };
                    [relation.name.clone(), kind.to_owned()]
                })
                .collect();
            content = content.child(section(
                div(),
                "Tables and views",
                &["Name", "Kind"],
                rows,
                "No tables or views in this schema.",
                cx,
            ));
        } else {
            content = content.child(settings(&inspection.connection.config, compact, cx));
            if let Some(catalog) = inspection.catalog {
                let rows = catalog
                    .schemas
                    .iter()
                    .map(|schema| [schema.name.clone(), schema.relations.len().to_string()])
                    .collect();
                content = content.child(section(
                    div(),
                    "Schemas",
                    &["Name", "Relations"],
                    rows,
                    "No schemas are available.",
                    cx,
                ));
            }
        }
    }

    div()
        .id("object-inspector")
        .test_support()
        .size_full()
        .child(
            div()
                .id("inspector-scroll")
                .size_full()
                .overflow_y_scrollbar()
                .child(content.pb_6()),
        )
        .into_any_element()
}

fn counted(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

/// A connection's saved settings as a description list: a quiet label over
/// its value, laid out in a grid.
fn settings(config: &ConnectionConfig, compact: bool, cx: &App) -> Div {
    let theme = cx.theme();
    let facts: Vec<(&str, String)> = match config {
        ConnectionConfig::Postgres(config)
        | ConnectionConfig::Mysql(config)
        | ConnectionConfig::Mariadb(config)
        | ConnectionConfig::SqlServer(config)
        | ConnectionConfig::Snowflake(config)
        | ConnectionConfig::Mongodb(config)
        | ConnectionConfig::Redis(config)
        | ConnectionConfig::Cassandra(config)
        | ConnectionConfig::Elasticsearch(config) => vec![
            ("Host", config.host.clone()),
            ("Port", config.port.to_string()),
            ("Database", config.database.clone()),
            ("User", config.user.clone()),
            (
                "TLS",
                match config.tls {
                    TlsMode::Disable => "Off",
                    TlsMode::Prefer => "Prefer encryption",
                    TlsMode::Require => "Require encryption",
                    TlsMode::VerifyFull => "Verify server certificate",
                }
                .to_owned(),
            ),
        ],
        ConnectionConfig::Sqlite(config) => vec![
            ("Database file", config.path.to_string_lossy().into_owned()),
            (
                "Access",
                if config.read_only {
                    "Read-only"
                } else {
                    "Read-write"
                }
                .into(),
            ),
        ],
    };
    v_flex()
        .gap_3()
        .child(section_heading("Connection", None, cx))
        .child(
            div()
                .grid()
                .grid_cols(if compact { 2 } else { 4 })
                .gap_x_8()
                .gap_y_5()
                .pt_4()
                .border_t_1()
                .border_color(theme.border)
                .children(facts.into_iter().map(|(label, value)| {
                    v_flex()
                        .min_w_0()
                        .gap_1()
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(label.to_owned()),
                        )
                        .child(div().truncate().font_family(MONO_FONT).child(value))
                })),
        )
}

/// A section's heading, with its count in a quiet badge.
fn section_heading(title: &'static str, count: Option<usize>, cx: &App) -> Div {
    h_flex()
        .gap_2()
        .items_center()
        .child(
            div()
                .text_lg()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title),
        )
        .when_some(count, |this, count| {
            this.child(ui::badge(count.to_string(), cx))
        })
}

/// A heading over a bordered table: a muted header row, then hairline rows.
/// The first column is in the UI face, the rest in mono.
fn section<E, const N: usize>(
    container: E,
    title: &'static str,
    head: &[&'static str; N],
    rows: Vec<[String; N]>,
    empty: &'static str,
    cx: &App,
) -> E
where
    E: ParentElement + Styled + IntoElement,
{
    let theme = cx.theme();
    let well = crate::design::tokens(cx).well;
    let cells = |values: Vec<SharedString>, header: bool| {
        h_flex()
            .gap_4()
            .px_4()
            .h(px(if header { 40. } else { 44. }))
            .items_center()
            .when(!header, |this| this.border_t_1().border_color(theme.border))
            .when(header, |this| {
                this.bg(well)
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.muted_foreground)
            })
            .children(values.into_iter().enumerate().map(move |(ix, value)| {
                div()
                    .min_w_0()
                    .truncate()
                    .map(|this| {
                        if ix == 0 {
                            this.w(px(240.)).flex_shrink_0()
                        } else {
                            this.flex_1()
                        }
                    })
                    .when(ix > 0 && !header, |this| {
                        this.font_family(MONO_FONT)
                            .text_sm()
                            .text_color(theme.muted_foreground)
                    })
                    .when(ix == 0 && !header, |this| {
                        this.font_weight(FontWeight::MEDIUM)
                    })
                    .child(value)
            }))
    };
    let count = rows.len();
    let table = v_flex()
        .rounded(px(crate::design::RADIUS_CARD))
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
        .child(cells(
            head.iter().map(|value| (*value).into()).collect(),
            true,
        ))
        .children(
            rows.into_iter()
                .map(|row| cells(row.into_iter().map(Into::into).collect(), false)),
        )
        .when(count == 0, |this| {
            this.child(
                div()
                    .px_4()
                    .py_5()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(ui::muted(empty, cx)),
            )
        });
    container
        .flex()
        .flex_col()
        .gap_3()
        .child(section_heading(title, Some(count), cx))
        .child(table)
}

fn table_detail(detail: &TableDetail, cx: &App) -> Div {
    let columns = detail
        .columns
        .iter()
        .map(|column| {
            let mut attributes = Vec::new();
            if detail.primary_key.contains(&column.name) {
                attributes.push("primary key".to_owned());
            }
            if !column.nullable {
                attributes.push("not null".to_owned());
            }
            if let Some(default) = &column.default {
                attributes.push(format!("default {default}"));
            }
            [
                column.name.clone(),
                column.type_name.clone(),
                attributes.join(", "),
            ]
        })
        .collect();
    let mut sections = v_flex().gap_8().child(section(
        div()
            .id("inspector-columns")
            .test_support()
            .aria_label(counted(detail.columns.len(), "column", "columns")),
        "Columns",
        &["Column", "Type", "Constraints"],
        columns,
        "No columns.",
        cx,
    ));

    if !detail.indexes.is_empty() {
        let rows = detail
            .indexes
            .iter()
            .map(|index| {
                let columns = index
                    .columns
                    .iter()
                    .map(|column| column.as_deref().unwrap_or("expression"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let kind = if index.primary {
                    "primary key"
                } else if index.unique {
                    "unique"
                } else {
                    "index"
                };
                [index.name.clone(), columns, kind.to_owned()]
            })
            .collect();
        sections = sections.child(section(
            div()
                .id("inspector-indexes")
                .test_support()
                .aria_label(counted(detail.indexes.len(), "index", "indexes")),
            "Indexes",
            &["Index", "Columns", "Kind"],
            rows,
            "No indexes.",
            cx,
        ));
    }
    if !detail.foreign_keys.is_empty() {
        let rows = detail
            .foreign_keys
            .iter()
            .map(|key| {
                let referenced = key
                    .references
                    .schema
                    .as_ref()
                    .map(|schema| format!("{schema}.{}", key.references.name))
                    .unwrap_or_else(|| key.references.name.clone());
                [
                    key.name.clone(),
                    key.columns.join(", "),
                    format!("{referenced} ({})", key.referenced_columns.join(", ")),
                ]
            })
            .collect();
        sections = sections.child(section(
            div()
                .id("inspector-foreign-keys")
                .test_support()
                .aria_label(counted(
                    detail.foreign_keys.len(),
                    "foreign key",
                    "foreign keys",
                )),
            "Foreign keys",
            &["Key", "Columns", "References"],
            rows,
            "No foreign keys.",
            cx,
        ));
    }
    sections
}
