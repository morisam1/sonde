//! The grid of database engines that the setup wizard and the connection
//! dialog both open with: one button per engine, its icon in the engine's
//! color.

use gpui_kit::{
    component::{Icon, IconName, Sizable as _, button::Button, h_flex},
    prelude::FluentBuilder as _,
    *,
};
use sonde_core::DbKind;

use crate::design;

/// What a button click does with the engine it names.
pub(super) type OnChoose = Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// The id part for an engine, as in `setup-postgres` or `kind-mariadb`.
pub(super) fn slug(kind: DbKind) -> String {
    match kind {
        DbKind::Postgres => "postgres".into(),
        DbKind::SqlServer => "sqlserver".into(),
        other => other.name().to_ascii_lowercase(),
    }
}

/// Every engine, two to a row so the grid stays narrow at any width.
/// Button ids are `{prefix}-{slug}`.
pub(super) fn grid(prefix: &str, cx: &App, on_choose: impl Fn(DbKind) -> OnChoose) -> Div {
    let mut engines = h_flex().flex_wrap().gap_2();
    for kind in DbKind::ALL {
        let id: SharedString = format!("{prefix}-{}", slug(kind)).into();
        let label = kind.name();
        let icon = if kind.is_server() {
            IconName::Network
        } else {
            IconName::HardDrive
        };
        engines = engines.child(
            Button::new(id)
                .outline()
                .h_11()
                .w(relative(0.48))
                .flex_grow(1.)
                .accessibility_label(label)
                .when_some(kind.also(), |button, also| {
                    button.tooltip(format!("Also {also}"))
                })
                .child(
                    h_flex()
                        .w_full()
                        .gap_3()
                        .child(
                            Icon::new(icon)
                                .with_size(px(16.))
                                .flex_shrink_0()
                                .text_color(design::engine_ink(kind, cx)),
                        )
                        .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(label)),
                )
                .on_click(on_choose(kind)),
        );
    }
    engines
}
