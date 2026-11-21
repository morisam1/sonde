//! Sonde's visual system, with neutral surfaces and semantic accents.
//!
//! A sidebar on its own surface beside a plain background; content sits in
//! rounded cards (`rounded-xl`, a hairline border, `bg-card`) or on muted
//! wells. Geist for everything, Geist Mono for data and SQL. All authored
//! colour and every face name lives in this file.

use std::{rc::Rc, sync::Once};

use gpui_kit::{
    App, Global, Hsla, SharedString, Window, black,
    component::{
        ActiveTheme as _, Colorize as _, Theme, ThemeConfig, ThemeMode,
        highlighter::{LanguageRegistry, SyntaxColors},
        oklch,
    },
};
use sonde_core::{DbKind, Value};

use crate::layout::Appearance;

/// Geist: controls, labels, headings, prose.
pub const UI_FONT: &str = "Geist";
/// Geist Mono: cell data, SQL, types and figures.
pub const MONO_FONT: &str = "Geist Mono";

const FONTS: [&[u8]; 5] = [
    include_bytes!("../assets/fonts/Geist-Regular.ttf"),
    include_bytes!("../assets/fonts/Geist-Medium.ttf"),
    include_bytes!("../assets/fonts/Geist-SemiBold.ttf"),
    include_bytes!("../assets/fonts/GeistMono-Regular.ttf"),
    include_bytes!("../assets/fonts/GeistMono-Medium.ttf"),
];

/// Card radius (`rounded-xl`).
pub const RADIUS_CARD: f32 = 12.;
/// Sidebar menu buttons and small controls (`rounded-md`).
pub const RADIUS_ITEM: f32 = 8.;
/// Icons in menus, rows and toolbars.
pub const ICON: f32 = 18.;
/// Sidebar menu buttons, tree rows and data rows.
pub const ROW_HEIGHT: f32 = 36.;
pub const GRID_HEADER_HEIGHT: f32 = 40.;
pub const GRID_CELL_INSET: f32 = 12.;
/// How far each tree level is indented.
pub const INDENT: f32 = 20.;

#[derive(Clone, Copy)]
struct Color(f32, f32, f32);

impl Color {
    fn hsla(self) -> Hsla {
        oklch(self.0, self.1, self.2)
    }
}

impl From<Color> for SharedString {
    fn from(color: Color) -> Self {
        color.hsla().to_hex().into()
    }
}

impl Appearance {
    pub(crate) fn apply(self, window: &mut Window, cx: &mut App) {
        match self {
            Self::System => Theme::sync_system_appearance(Some(window), cx),
            Self::Light => Theme::change(ThemeMode::Light, Some(window), cx),
            Self::Dark => Theme::change(ThemeMode::Dark, Some(window), cx),
        }
    }
}

/// Translucent borders are flattened onto the surface they usually sit on.
struct Palette {
    background: Color,
    foreground: Color,
    card: Color,
    primary: Color,
    primary_hover: Color,
    primary_foreground: Color,
    secondary: Color,
    secondary_hover: Color,
    muted: Color,
    /// `bg-muted/50`: the placeholder wells in the content area.
    well: Color,
    muted_foreground: Color,
    border: Color,
    input: Color,
    scrollbar_thumb_hover: Color,
    destructive: Color,
    success: Color,
    sidebar: Color,
    sidebar_accent: Color,
    sidebar_border: Color,
    /// A selected range of cells.
    selection: Color,
    /// A staged, not yet applied, value in the table grid.
    staged: Color,
    accent: Color,
    on_primary: Color,
    violet: Color,
    teal: Color,
    gold: Color,
    moss: Color,
    copper: Color,
}

const LIGHT: Palette = Palette {
    background: Color(1., 0., 0.),
    foreground: Color(0.145, 0., 0.),
    card: Color(1., 0., 0.),
    primary: Color(0.205, 0., 0.),
    primary_hover: Color(0.30, 0., 0.),
    primary_foreground: Color(0.985, 0., 0.),
    secondary: Color(0.97, 0., 0.),
    secondary_hover: Color(0.94, 0., 0.),
    muted: Color(0.97, 0., 0.),
    well: Color(0.985, 0., 0.),
    muted_foreground: Color(0.53, 0., 0.),
    border: Color(0.922, 0., 0.),
    input: Color(0.88, 0., 0.),
    scrollbar_thumb_hover: Color(0.62, 0., 0.),
    destructive: Color(0.577, 0.245, 27.325),
    success: Color(0.52, 0.13, 150.),
    sidebar: Color(0.985, 0., 0.),
    sidebar_accent: Color(0.95, 0., 0.),
    sidebar_border: Color(0.922, 0., 0.),
    selection: Color(0.93, 0.03, 255.),
    staged: Color(0.488, 0.105, 64.2),
    accent: Color(0.502, 0.150, 262.0),
    on_primary: Color(0.821, 0.089, 267.5),
    violet: Color(0.476, 0.135, 303.7),
    teal: Color(0.503, 0.083, 187.2),
    gold: Color(0.520, 0.094, 87.5),
    moss: Color(0.483, 0.093, 130.6),
    copper: Color(0.514, 0.107, 38.4),
};

const DARK: Palette = Palette {
    background: Color(0.145, 0., 0.),
    foreground: Color(0.985, 0., 0.),
    card: Color(0.18, 0., 0.),
    primary: Color(0.922, 0., 0.),
    primary_hover: Color(0.85, 0., 0.),
    primary_foreground: Color(0.205, 0., 0.),
    secondary: Color(0.269, 0., 0.),
    secondary_hover: Color(0.31, 0., 0.),
    muted: Color(0.269, 0., 0.),
    well: Color(0.205, 0., 0.),
    muted_foreground: Color(0.708, 0., 0.),
    border: Color(0.275, 0., 0.),
    input: Color(0.33, 0., 0.),
    scrollbar_thumb_hover: Color(0.556, 0., 0.),
    destructive: Color(0.704, 0.191, 22.216),
    success: Color(0.76, 0.15, 150.),
    sidebar: Color(0.205, 0., 0.),
    sidebar_accent: Color(0.269, 0., 0.),
    sidebar_border: Color(0.29, 0., 0.),
    selection: Color(0.33, 0.06, 255.),
    staged: Color(0.829, 0.120, 77.7),
    accent: Color(0.761, 0.121, 266.5),
    on_primary: Color(0.413, 0.127, 261.7),
    violet: Color(0.772, 0.103, 305.0),
    teal: Color(0.804, 0.105, 182.1),
    gold: Color(0.823, 0.115, 92.5),
    moss: Color(0.793, 0.100, 124.8),
    copper: Color(0.780, 0.102, 39.6),
};

fn palette(cx: &App) -> &'static Palette {
    if cx.theme().mode.is_dark() {
        &DARK
    } else {
        &LIGHT
    }
}

/// Roles GPUI's theme has no slot for.
pub struct Tokens {
    /// `bg-card`: rounded content cards.
    pub card: Hsla,
    /// `bg-muted/50`: wells, toolbars and empty placeholders.
    pub well: Hsla,
    /// A staged, not yet applied, value in the table grid.
    pub staged: Hsla,
    /// The fill for a hovered or active sidebar item.
    pub sidebar_accent: Hsla,
    pub accent: Hsla,
    pub view: Hsla,
    pub key: Hsla,
}

pub fn tokens(cx: &App) -> Tokens {
    let p = palette(cx);
    Tokens {
        card: p.card.hsla(),
        well: p.well.hsla(),
        staged: p.staged.hsla(),
        sidebar_accent: p.sidebar_accent.hsla(),
        accent: p.accent.hsla(),
        view: p.teal.hsla(),
        key: p.gold.hsla(),
    }
}

pub(crate) fn engine_ink(kind: DbKind, cx: &App) -> Hsla {
    let p = palette(cx);
    // The newer engines take the teal's lightness and chroma, so they sit
    // at the same weight in both themes, at their own hue.
    let hued = |hue: f32| Color(p.teal.0, p.teal.1, hue);
    match kind {
        DbKind::Postgres => p.accent,
        DbKind::Mysql => p.teal,
        DbKind::Mariadb => hued(55.0),
        DbKind::SqlServer => p.gold,
        DbKind::Sqlite => p.violet,
        DbKind::Snowflake => hued(230.0),
        DbKind::Mongodb => hued(145.0),
        DbKind::Redis => hued(25.0),
        DbKind::Cassandra => hued(265.0),
        DbKind::Elasticsearch => hued(205.0),
    }
    .hsla()
}

pub(crate) fn value_ink(value: &Value, cx: &App) -> Hsla {
    let p = palette(cx);
    match value {
        Value::Null => p.violet.hsla(),
        Value::Bool(_) => p.teal.hsla(),
        _ => cx.theme().foreground,
    }
}

struct FontsLoaded;

impl Global for FontsLoaded {}

/// Load the faces and install both palettes. Called at the shared window
/// entry point, headless windows included, so a system appearance change
/// keeps Sonde's visual system.
pub fn install(cx: &mut App) {
    register_sql_literal_highlighting();
    if !cx.has_global::<FontsLoaded>() {
        let fonts = FONTS.iter().map(|font| (*font).into()).collect();
        // A face that will not load falls back to the platform's; the app
        // still works.
        if let Err(error) = cx.text_system().add_fonts(fonts) {
            eprintln!("sonde: could not load the bundled fonts: {error}");
        }
        cx.set_global(FontsLoaded);
    }
    let settings = crate::settings::Settings::get(cx);
    let mono = (settings.editor_font.clone(), settings.editor_font_size);
    let theme = Theme::global(cx);
    let light = configured(&theme.light_theme, &LIGHT, &mono);
    let dark = configured(&theme.dark_theme, &DARK, &mono);
    let mode = theme.mode;
    Theme::update(cx, |theme| {
        theme.light_theme = light;
        theme.dark_theme = dark;
    });
    Theme::change(mode, None, cx);
}

fn register_sql_literal_highlighting() {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| {
        let registry = LanguageRegistry::singleton();
        let Some(mut sql) = registry.language("sql") else {
            return;
        };
        // GPUI keeps the first capture for a span. Match typed literals before
        // the bundled grammar's catch-all string capture.
        const LITERALS: &str = r#"
((literal) @number
 (#match? @number "^[-+]?([0-9]+([.][0-9]*)?|[.][0-9]+)([eE][-+]?[0-9]+)?$"))
((literal) @boolean
 (#match? @boolean "^(?i:true|false)$"))
((literal) @keyword
 (#match? @keyword "^(?i:null)$"))
(literal) @string
"#;
        sql.highlights = sql
            .highlights
            .replacen("(literal) @string", LITERALS, 1)
            .into();
        registry.register("sql", &sql);
    });
}

fn configured(base: &ThemeConfig, p: &Palette, mono: &(String, f32)) -> Rc<ThemeConfig> {
    let mut config = base.clone();
    config.name = if config.mode.is_dark() {
        "Sonde dark"
    } else {
        "Sonde light"
    }
    .into();
    config.font_family = Some(UI_FONT.into());
    config.mono_font_family = Some(mono.0.clone().into());
    config.font_size = Some(15.);
    config.mono_font_size = Some(mono.1);
    config.radius = Some(RADIUS_ITEM as usize);
    config.radius_lg = Some(RADIUS_CARD as usize);
    //config.window_radius = Some(10);
    config.shadow = Some(true);
    let c = &mut config.colors;
    c.background = Some(p.background.into());
    c.foreground = Some(p.foreground.into());
    c.border = Some(p.border.into());
    c.window_border = Some(p.border.into());
    c.input = Some(p.input.into());
    c.ring = Some(p.accent.into());
    c.caret = Some(p.foreground.into());
    c.primary = Some(p.primary.into());
    c.primary_foreground = Some(p.primary_foreground.into());
    c.primary_hover = Some(p.primary_hover.into());
    c.primary_active = Some(p.primary_hover.into());
    c.button_primary = Some(p.primary.into());
    c.button_primary_foreground = Some(p.on_primary.into());
    c.button_primary_hover = Some(p.primary_hover.into());
    c.button_primary_active = Some(p.primary_hover.into());
    c.secondary = Some(p.secondary.into());
    c.secondary_foreground = Some(p.foreground.into());
    c.secondary_hover = Some(p.secondary_hover.into());
    c.secondary_active = Some(p.secondary_hover.into());
    c.button = Some(p.background.into());
    c.button_foreground = Some(p.foreground.into());
    c.button_hover = Some(p.muted.into());
    c.button_active = Some(p.secondary_hover.into());
    c.muted = Some(p.muted.into());
    c.muted_foreground = Some(p.muted_foreground.into());
    c.accent = Some(p.muted.into());
    c.accent_foreground = Some(p.foreground.into());
    c.selection = Some(p.selection.into());
    c.list = Some(p.sidebar.into());
    c.list_even = Some(p.sidebar.into());
    c.list_hover = Some(p.sidebar_accent.into());
    c.list_active = Some(p.sidebar_accent.into());
    c.list_active_border = Some(p.sidebar_accent.into());
    c.list_head = Some(p.sidebar.into());
    c.sidebar = Some(p.sidebar.into());
    c.sidebar_foreground = Some(p.foreground.into());
    c.sidebar_border = Some(p.sidebar_border.into());
    c.sidebar_accent = Some(p.sidebar_accent.into());
    c.sidebar_accent_foreground = Some(p.foreground.into());
    c.sidebar_primary = Some(p.primary.into());
    c.sidebar_primary_foreground = Some(p.on_primary.into());
    c.tab = Some(p.background.into());
    c.tab_bar = Some(p.muted.into());
    c.tab_bar_segmented = Some(p.muted.into());
    c.tab_foreground = Some(p.muted_foreground.into());
    c.tab_active = Some(p.background.into());
    c.tab_active_foreground = Some(p.accent.into());
    c.table = Some(p.card.into());
    c.table_even = Some(p.card.into());
    c.table_head = Some(p.card.into());
    c.table_head_foreground = Some(p.muted_foreground.into());
    c.table_foot = Some(p.card.into());
    c.table_foot_foreground = Some(p.muted_foreground.into());
    c.table_hover = Some(p.muted.into());
    c.table_active = Some(p.selection.into());
    c.table_active_border = Some(p.accent.into());
    c.table_row_border = Some(p.border.into());
    c.title_bar = Some(p.background.into());
    c.title_bar_border = Some(p.border.into());
    c.popover = Some(p.card.into());
    c.popover_foreground = Some(p.foreground.into());
    c.overlay = Some(black().opacity(0.5).to_hex().into());
    c.danger = Some(p.destructive.into());
    c.danger_foreground = Some(p.primary_foreground.into());
    c.button_danger = Some(p.destructive.into());
    c.button_danger_foreground = Some(Color(0.985, 0., 0.).into());
    c.success = Some(p.success.into());
    c.success_foreground = Some(p.background.into());
    c.scrollbar = Some(p.background.into());
    c.scrollbar_thumb = Some(p.input.into());
    c.scrollbar_thumb_hover = Some(p.scrollbar_thumb_hover.into());
    c.drag_border = Some(p.accent.into());
    c.drop_target = Some(p.selection.into());
    c.skeleton = Some(p.muted.into());
    if let Some(highlight) = &mut config.highlight
        && let Err(error) = configure_syntax(&mut highlight.syntax, p)
    {
        eprintln!("sonde: could not configure syntax colors: {error}");
    }
    Rc::new(config)
}

fn configure_syntax(syntax: &mut SyntaxColors, p: &Palette) -> serde_json::Result<()> {
    // GPUI exposes the syntax slots, with styles constructed from theme JSON.
    for (style, color) in [
        (&mut syntax.keyword, p.violet),
        (&mut syntax.string, p.moss),
        (&mut syntax.string_escape, p.moss),
        (&mut syntax.number, p.copper),
        (&mut syntax.boolean, p.teal),
        (&mut syntax.comment, p.muted_foreground),
        (&mut syntax.comment_doc, p.muted_foreground),
    ] {
        *style = serde_json::from_value(serde_json::json!({ "color": color.hsla() }))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use gpui_kit::{
        Hsla, Rgba,
        base::input::Rope,
        component::highlighter::{HighlightTheme, SyntaxHighlighter},
    };

    use super::*;

    fn luminance(color: Hsla) -> f32 {
        let rgb: Rgba = color.into();
        let linear = |channel: f32| {
            if channel <= 0.04045 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(rgb.r) + 0.7152 * linear(rgb.g) + 0.0722 * linear(rgb.b)
    }

    #[test]
    fn palettes_should_keep_text_and_controls_legible() {
        for (mode, palette) in [(ThemeMode::Light, &LIGHT), (ThemeMode::Dark, &DARK)] {
            let mut base = ThemeConfig::default();
            base.mode = mode;
            let config = configured(&base, palette, &(MONO_FONT.into(), 14.));
            let mut theme = Theme::default();
            theme.apply_config(&config);
            let colors = theme.colors;
            let p = palette;
            for (name, foreground, background, minimum) in [
                ("body", colors.foreground, colors.background, 4.5),
                (
                    "muted text",
                    colors.muted_foreground,
                    colors.background,
                    4.5,
                ),
                (
                    "muted text on the sidebar",
                    colors.muted_foreground,
                    colors.sidebar,
                    4.5,
                ),
                (
                    "muted text on a card",
                    p.muted_foreground.hsla(),
                    p.card.hsla(),
                    4.5,
                ),
                (
                    "muted text on an active item",
                    p.muted_foreground.hsla(),
                    p.sidebar_accent.hsla(),
                    4.,
                ),
                (
                    "primary button",
                    colors.button_primary_foreground,
                    colors.button_primary,
                    4.5,
                ),
                (
                    "hovered primary button",
                    colors.button_primary_foreground,
                    colors.button_primary_hover,
                    4.5,
                ),
                ("active item", p.accent.hsla(), p.sidebar_accent.hsla(), 4.5),
                (
                    "text in a selection",
                    colors.foreground,
                    colors.selection,
                    4.5,
                ),
                ("staged value", p.staged.hsla(), p.card.hsla(), 4.5),
                (
                    "staged badge",
                    p.staged.hsla(),
                    p.sidebar_accent.hsla(),
                    4.5,
                ),
                ("null", p.violet.hsla(), p.selection.hsla(), 4.5),
                ("boolean", p.teal.hsla(), p.selection.hsla(), 4.5),
                ("key icon", p.gold.hsla(), p.sidebar_accent.hsla(), 3.),
                ("error", colors.danger, colors.background, 4.5),
                ("success", colors.success, colors.background, 4.5),
                ("focus ring", colors.ring, colors.background, 3.),
            ] {
                let a = luminance(foreground);
                let b = luminance(background);
                let ratio = (a.max(b) + 0.05) / (a.min(b) + 0.05);
                assert!(
                    ratio >= minimum,
                    "{mode:?} {name}: {ratio:.2}:1, needs {minimum}:1"
                );
            }
        }
    }

    #[test]
    fn syntax_colors_should_preserve_editor_surfaces() {
        for (mode, palette, default) in [
            (ThemeMode::Light, &LIGHT, HighlightTheme::default_light()),
            (ThemeMode::Dark, &DARK, HighlightTheme::default_dark()),
        ] {
            let mut base = ThemeConfig::default();
            base.mode = mode;
            base.highlight = Some(default.style.clone());
            let config = configured(&base, palette, &(MONO_FONT.into(), 14.));
            let highlight = config.highlight.as_ref().unwrap();
            assert_eq!(highlight.editor_background, default.style.editor_background);
            assert_eq!(
                highlight.editor_active_line,
                default.style.editor_active_line
            );
            assert_eq!(
                highlight.editor_gutter_background,
                default.style.editor_gutter_background,
            );
            for (capture, color) in [
                ("keyword", palette.violet),
                ("string", palette.moss),
                ("number", palette.copper),
                ("boolean", palette.teal),
            ] {
                let ink = Rgba::from(highlight.syntax.style(capture).unwrap().color.unwrap());
                let expected = Rgba::from(color.hsla());
                // GPUI's JSON uses 8-bit color channels.
                for (actual, expected) in [
                    (ink.r, expected.r),
                    (ink.g, expected.g),
                    (ink.b, expected.b),
                ] {
                    assert!((actual - expected).abs() <= 1. / 255., "{mode:?} {capture}");
                }
            }
        }
    }

    #[test]
    fn sql_highlighting_should_distinguish_literal_types() {
        register_sql_literal_highlighting();
        let source = "select 42, 5.25, '42', true, null";
        let mut highlighter = SyntaxHighlighter::new("sql");
        assert!(highlighter.update(None, &Rope::from_str(source), None));
        let mut base = ThemeConfig::default();
        base.highlight = Some(HighlightTheme::default_light().style.clone());
        let mut theme = Theme::default();
        theme.apply_config(&configured(&base, &LIGHT, &(MONO_FONT.into(), 14.)));
        let theme = &*theme.highlight_theme;
        let styles = highlighter.styles(&(0..source.len()), theme);
        for (text, capture) in [
            ("42", "number"),
            ("5.25", "number"),
            ("'42'", "string"),
            ("true", "boolean"),
            ("null", "keyword"),
        ] {
            let start = source.find(text).unwrap();
            let expected = theme.style(capture).unwrap().color;
            let (range, style) = styles
                .iter()
                .find(|(range, _)| range.contains(&start))
                .unwrap();
            assert!(range.contains(&(start + text.len() - 1)), "{text}");
            assert_eq!(style.color, expected, "{text}");
        }
    }
}
