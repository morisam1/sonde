//! The main window, laid out like shadcn/ui's sidebar shell: the sidebar on
//! its own surface (workspace items, then the connection tree), and beside
//! it the inset, a header with the sidebar trigger and a breadcrumb over
//! the open item.

mod preferences;

use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, TitleBar,
    breadcrumb::{Breadcrumb, BreadcrumbItem},
    button::{Button, ButtonVariants as _},
    h_flex, v_flex, window_corner_radii,
};
use gpui_kit::{prelude::FluentBuilder as _, *};

use crate::{
    browser::{BrowserEvent, BrowserPanel},
    connections::{
        Connections,
        setup::{SetupEvent, SetupWizard},
    },
    layout::{Appearance, Layout, LayoutFile},
    query::{HistoryStore, QueryPanel, QueryTarget},
    session::{SavedSession, SavedTab, SessionFile},
    settings::{Settings, SettingsFile},
    table_view::TableView,
    ui::{self, Glyph},
};

actions!(
    workspace,
    [
        /// Show or hide the sidebar.
        ToggleSidebar,
        /// Close the open tab.
        CloseTab,
    ]
);

const CONTEXT: &str = "Workspace";
const SIDEBAR_HEADER_INSET: Pixels = px(12.);
const SIDEBAR_BORDER: Pixels = px(1.);

pub(crate) fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-b", ToggleSidebar, Some(CONTEXT)),
        KeyBinding::new("ctrl-w", CloseTab, Some(CONTEXT)),
    ]);
}

#[derive(Clone)]
enum TabKind {
    Query(Entity<QueryPanel>),
    Table(Entity<TableView>),
}

struct Tab {
    kind: TabKind,
    _changes: Subscription,
}

impl TabKind {
    fn view(&self) -> AnyView {
        match self {
            Self::Query(panel) => panel.clone().into(),
            Self::Table(panel) => panel.clone().into(),
        }
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self {
            Self::Query(panel) => panel.focus_handle(cx),
            Self::Table(panel) => panel.focus_handle(cx),
        }
    }

    fn title(&self, cx: &App) -> SharedString {
        match self {
            Self::Query(panel) => panel.read(cx).title(),
            Self::Table(panel) => panel.read(cx).name(),
        }
    }

    fn crumbs(&self, cx: &App) -> Vec<SharedString> {
        match self {
            Self::Query(panel) => {
                let panel = panel.read(cx);
                vec![panel.connection_name().to_owned().into(), panel.title()]
            }
            Self::Table(panel) => {
                let panel = panel.read(cx);
                let mut crumbs: Vec<SharedString> = vec![panel.connection_name().to_owned().into()];
                if let Some(schema) = panel.schema_name() {
                    crumbs.push(schema.to_owned().into());
                }
                crumbs.push(panel.name());
                crumbs
            }
        }
    }
}

/// Where history goes when the app chose it; otherwise each workspace keeps
/// it beside its layout.
pub struct HistoryLocation(pub std::path::PathBuf);

impl Global for HistoryLocation {}

pub struct Workspace {
    connections: Entity<Connections>,
    session_file: SessionFile,
    settings_file: SettingsFile,
    /// Why `settings.toml` was not used, if it was not.
    settings_problem: Option<SharedString>,
    browser: Entity<BrowserPanel>,
    layout_file: LayoutFile,
    sidebar_open: bool,
    sidebar_width: f32,
    appearance: Appearance,
    setup: Option<Entity<SetupWizard>>,
    _setup_events: Option<Subscription>,
    tabs: Vec<Tab>,
    /// The tab on screen; `None` shows the overview.
    active: Option<usize>,
    queries_opened: usize,
    /// The tree row selected when the overview last followed it.
    seen_selection: Option<SharedString>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(
        connections: Entity<Connections>,
        layout_file: LayoutFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        crate::updates::ensure(cx);
        let dir = layout_file.dir();
        let settings_file = SettingsFile::at(dir.join("settings.toml"));
        let loaded = settings_file.load();
        if let Some(problem) = &loaded.problem {
            eprintln!("sonde: {problem}");
        }
        cx.set_global(loaded.settings);
        let history_dir = cx
            .try_global::<HistoryLocation>()
            .map_or_else(|| dir.join("history"), |location| location.0.clone());
        cx.set_global(HistoryStore(Some(sonde_core::history::History::at(
            history_dir,
        ))));
        crate::design::install(cx);
        crate::query::init(cx);
        crate::table_view::init(cx);
        init(cx);
        let layout = layout_file.load();
        layout.appearance.apply(window, cx);
        let browser = cx.new(|cx| BrowserPanel::new(connections.clone(), window, cx));
        let first_run =
            connections.read(cx).list().is_empty() && connections.read(cx).load_error().is_none();
        let setup =
            first_run.then(|| cx.new(|cx| SetupWizard::new(connections.clone(), window, cx)));
        let setup_browser = browser.clone();
        let setup_events = setup.as_ref().map(|setup| {
            cx.subscribe_in(
                setup,
                window,
                move |this, _, event: &SetupEvent, window, cx| {
                    match event {
                        SetupEvent::Saved { id, password } => {
                            this.setup = None;
                            setup_browser.update(cx, |browser, cx| {
                                browser.connect(id, password.clone(), window, cx);
                                browser.select_connection(id, window, cx);
                            });
                        }
                        SetupEvent::Dismissed => this.setup = None,
                        SetupEvent::AppearanceChanged(appearance) => {
                            this.set_appearance(*appearance, window, cx);
                        }
                    }
                    cx.notify();
                },
            )
        });
        let subscriptions = vec![
            cx.observe_window_appearance(window, |this, window, cx| {
                if this.appearance == Appearance::System {
                    this.appearance.apply(window, cx);
                }
            }),
            cx.subscribe_in(
                &browser,
                window,
                |this, _, event: &BrowserEvent, window, cx| match event {
                    BrowserEvent::OpenQuery(target) => this.open_query(target.clone(), window, cx),
                    BrowserEvent::OpenTable(target, table) => {
                        this.open_table(target.clone(), table.clone(), window, cx);
                    }
                },
            ),
            // Picking a row in the tree shows what it is.
            cx.observe(&browser, |this, browser, cx| {
                let selected = browser.read(cx).selected_row(cx);
                if selected != this.seen_selection {
                    if selected.is_some() {
                        this.active = None;
                    }
                    this.seen_selection = selected;
                }
                cx.notify();
            }),
            cx.observe(&connections, |_, _, cx| cx.notify()),
        ];

        // Window resizes are not otherwise observed, so save on the way out.
        let this = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            if let Some(this) = this.upgrade() {
                let this = this.read(cx);
                this.layout_file.save(this.layout(window));
                this.save_session(cx);
            }
            true
        });
        if !first_run && Settings::get(cx).restore_tabs {
            cx.defer_in(window, |this, window, cx| this.restore_session(window, cx));
        }

        Self {
            connections,
            session_file: SessionFile::at(dir.join("session.toml")),
            settings_file,
            settings_problem: loaded.problem.map(Into::into),
            browser,
            layout_file,
            sidebar_open: layout.sidebar_open,
            sidebar_width: layout.sidebar_width,
            appearance: layout.appearance,
            setup,
            _setup_events: setup_events,
            tabs: Vec::new(),
            active: None,
            queries_opened: 0,
            seen_selection: None,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    fn push_tab(&mut self, kind: TabKind, window: &mut Window, cx: &mut Context<Self>) {
        let changes = match &kind {
            TabKind::Query(panel) => cx.observe(panel, |_, _, cx| cx.notify()),
            TabKind::Table(panel) => cx.observe(panel, |_, _, cx| cx.notify()),
        };
        self.tabs.push(Tab {
            kind,
            _changes: changes,
        });
        self.activate(Some(self.tabs.len() - 1), window, cx);
    }

    fn activate(&mut self, ix: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        self.active = ix.filter(|ix| *ix < self.tabs.len());
        // The row that led here is not a new pick; only a later one should
        // bring the overview back.
        self.seen_selection = self.browser.read(cx).selected_row(cx);
        match self.active {
            Some(ix) => self.tabs[ix].kind.focus_handle(cx).focus(window, cx),
            None => self.focus_handle.focus(window, cx),
        }
        cx.notify();
    }

    fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        // Dropping the last handle closes the tab's session.
        self.tabs.remove(ix);
        let next = match self.active {
            Some(active) if active == ix => {
                if self.tabs.is_empty() {
                    None
                } else {
                    Some(ix.min(self.tabs.len() - 1))
                }
            }
            Some(active) if active > ix => Some(active - 1),
            other => other,
        };
        self.activate(next, window, cx);
    }

    /// The query tabs as they are now, for the next start.
    pub fn session(&self, cx: &App) -> SavedSession {
        let mut active = None;
        let mut tabs = Vec::new();
        for (ix, tab) in self.tabs.iter().enumerate() {
            if let TabKind::Query(panel) = &tab.kind {
                if self.active == Some(ix) {
                    active = Some(tabs.len());
                }
                let panel = panel.read(cx);
                tabs.push(SavedTab {
                    connection_id: panel.connection_id().to_owned(),
                    sql: panel.text(cx),
                });
            }
        }
        SavedSession { tabs, active }
    }

    /// Save the open query tabs to `session.toml`.
    pub fn save_session(&self, cx: &App) {
        self.session_file.save(&self.session(cx));
    }

    /// Reopen the query tabs of the last session whose connections still
    /// exist.
    fn restore_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let saved = self.session_file.load();
        let mut opened = Vec::new();
        for (ix, tab) in saved.tabs.iter().enumerate() {
            // A tab on a listed database reopens on that database.
            let parent = crate::connections::keyring_id(&tab.connection_id);
            let Some(connection) = self.connections.read(cx).get(parent).cloned() else {
                continue;
            };
            let target = match crate::connections::database_of(&tab.connection_id) {
                Some(database) => QueryTarget {
                    id: tab.connection_id.clone(),
                    name: format!("{} · {database}", connection.name),
                    config: connection.config.with_database(database),
                    password: None,
                },
                None => QueryTarget {
                    id: connection.id,
                    name: connection.name,
                    config: connection.config,
                    password: None,
                },
            };
            self.open_query(target, window, cx);
            if let Some(TabKind::Query(panel)) = self.tabs.last().map(|tab| tab.kind.clone()) {
                panel.update(cx, |panel, cx| panel.set_text(&tab.sql, window, cx));
            }
            opened.push((ix, self.tabs.len() - 1));
        }
        let active = saved
            .active
            .and_then(|want| opened.iter().find(|(ix, _)| *ix == want))
            .map(|(_, tab)| *tab);
        if !opened.is_empty() {
            self.activate(active.or(Some(0)), window, cx);
        }
    }

    /// Where `settings.toml` is, and why it was not used if it was not.
    pub fn settings_file(&self) -> (&std::path::Path, Option<&SharedString>) {
        (self.settings_file.path(), self.settings_problem.as_ref())
    }

    /// Open a query tab and put the cursor in it.
    fn open_query(&mut self, target: QueryTarget, window: &mut Window, cx: &mut Context<Self>) {
        self.queries_opened += 1;
        let title = format!("Query {}", self.queries_opened).into();
        let secrets = self.connections.read(cx).secrets();
        let panel = cx.new(|cx| QueryPanel::new(title, target, secrets, window, cx));
        self.push_tab(TabKind::Query(panel), window, cx);
    }

    /// Open a tab with a table's rows, or go to the one already open.
    fn open_table(
        &mut self,
        target: QueryTarget,
        table: sonde_core::TableRef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let open = self.tabs.iter().position(|tab| match &tab.kind {
            TabKind::Table(panel) => panel.read(cx).shows(&target.id, &table),
            TabKind::Query(_) => false,
        });
        match open {
            Some(ix) => self.activate(Some(ix), window, cx),
            None => {
                let secrets = self.connections.read(cx).secrets();
                let panel = cx.new(|cx| TableView::new(target, table, secrets, window, cx));
                self.push_tab(TabKind::Table(panel), window, cx);
            }
        }
    }

    /// The table tabs that are open, oldest first.
    pub fn tables(&self) -> Vec<Entity<TableView>> {
        self.tabs
            .iter()
            .filter_map(|tab| match &tab.kind {
                TabKind::Table(panel) => Some(panel.clone()),
                TabKind::Query(_) => None,
            })
            .collect()
    }

    /// The query tabs that are open, oldest first.
    pub fn queries(&self) -> Vec<Entity<QueryPanel>> {
        self.tabs
            .iter()
            .filter_map(|tab| match &tab.kind {
                TabKind::Query(panel) => Some(panel.clone()),
                TabKind::Table(_) => None,
            })
            .collect()
    }

    fn toggle_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_open = !self.sidebar_open;
        self.layout_file.save(self.layout(window));
        cx.notify();
    }

    /// The layout as it is on screen now.
    fn layout(&self, window: &Window) -> Layout {
        let size = window.window_bounds().get_bounds().size;
        Layout {
            window_width: size.width.into(),
            window_height: size.height.into(),
            sidebar_width: self.sidebar_width,
            sidebar_open: self.sidebar_open,
            appearance: self.appearance,
        }
    }

    fn crumbs(&self, cx: &App) -> Vec<SharedString> {
        if let Some(tab) = self.active.and_then(|ix| self.tabs.get(ix)) {
            return tab.kind.crumbs(cx);
        }
        let browser = self.browser.read(cx);
        let Some(inspection) = browser.inspection(cx) else {
            return vec!["Overview".into()];
        };
        let mut crumbs: Vec<SharedString> = vec![inspection.connection.name.clone().into()];
        if let Some(schema) = inspection.schema {
            crumbs.push(schema.name.clone().into());
        }
        if let Some(table) = &inspection.table {
            crumbs.push(table.name.clone().into());
        }
        crumbs
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The window is client-side decorated, so its corners are ours to
        // round; the surfaces below would otherwise poke out square.
        let radii = window_corner_radii(window, cx);
        let theme = cx.theme();
        let (background, foreground) = (theme.background, theme.foreground);

        if let Some(setup) = &self.setup {
            return v_flex()
                .size_full()
                .font_family(crate::design::UI_FONT)
                .bg(background)
                .text_color(foreground)
                .rounded_tl(radii.top_left)
                .rounded_tr(radii.top_right)
                .rounded_bl(radii.bottom_left)
                .rounded_br(radii.bottom_right)
                .child(
                    TitleBar::new()
                        .h(px(48.))
                        .pl_4()
                        .bg(background)
                        .child(identity(cx)),
                )
                .child(div().flex_1().min_h_0().child(setup.clone()))
                .into_any_element();
        }

        let sidebar_open = self.sidebar_open;
        let sidebar = sidebar_open.then(|| {
            self.render_sidebar(cx)
                .rounded_tl(radii.top_left)
                .rounded_bl(radii.bottom_left)
        });

        let content: AnyElement = match self.active.and_then(|ix| self.tabs.get(ix)) {
            Some(tab) => tab.kind.view().into_any_element(),
            None => self.render_overview(window, cx),
        };

        let main = v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(background)
            .rounded_tr(radii.top_right)
            .rounded_br(radii.bottom_right)
            .when(!sidebar_open, |this| {
                this.rounded_tl(radii.top_left)
                    .rounded_bl(radii.bottom_left)
            })
            .child(
                self.render_header(sidebar_open, cx)
                    .when(sidebar_open, |bar| bar.rounded_tl(px(0.))),
            )
            .child(div().flex_1().min_h_0().p_6().child(content));

        h_flex()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(
                cx.listener(|this, _: &ToggleSidebar, window, cx| this.toggle_sidebar(window, cx)),
            )
            .on_action(cx.listener(|this, _: &CloseTab, window, cx| {
                if let Some(ix) = this.active {
                    this.close_tab(ix, window, cx);
                }
            }))
            .size_full()
            .text_color(foreground)
            .font_family(crate::design::UI_FONT)
            .when_some(sidebar, |this, sidebar| this.child(sidebar))
            .child(main)
            .into_any_element()
    }
}

/// The app's mark and name, as shadcn's team switcher shows a workspace.
fn identity(cx: &App) -> Div {
    h_flex()
        .gap_2()
        .items_center()
        .child(ui::logo(px(32.), cx))
        .child(
            v_flex().min_w_0().child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .line_height(rems(1.25))
                    .child(crate::APP_NAME),
            ),
        )
}

impl Workspace {
    fn render_sidebar(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (sidebar_bg, border) = (theme.sidebar, theme.sidebar_border);
        let browser = self.browser.read(cx);
        let (count, live) = (browser.connection_count(), browser.connected_count());

        let header = div()
            .flex_shrink_0()
            .px(SIDEBAR_HEADER_INSET)
            .pt_3()
            .pb_1()
            .child(self.render_app_menu(live, count, cx));

        // The "Workspace" group: New connection, then every open tab.
        let new_connection = ui::menu_button("new-connection", false, cx)
            .test_support()
            .role(Role::Button)
            .aria_label("New connection")
            .child(ui::icon(Glyph::Plus))
            .child(div().flex_1().truncate().child("New connection"))
            .on_click(cx.listener(|this, _, window, cx| {
                crate::connections::open_connection_form(
                    this.connections.clone(),
                    None,
                    window,
                    cx,
                );
            }));
        // Open tabs scroll after about five, so the tree keeps its room.
        let mut items = v_flex().gap_1();
        for (ix, tab) in self.tabs.iter().enumerate() {
            let active = self.active == Some(ix);
            let (glyph, badge, staged) = match &tab.kind {
                TabKind::Query(panel) => (
                    Glyph::SquareTerminal,
                    if panel.read(cx).is_running() {
                        "…".to_owned()
                    } else {
                        "SQL".to_owned()
                    },
                    false,
                ),
                TabKind::Table(panel) => {
                    let staged = panel.read(cx).staged(cx);
                    (
                        Glyph::Table,
                        if staged > 0 {
                            format!("+{staged}")
                        } else {
                            String::new()
                        },
                        staged > 0,
                    )
                }
            };
            let group: SharedString = format!("open-tab-{ix}").into();
            items = items.child(
                ui::menu_button(("open-tab", ix), active, cx)
                    .group(group.clone())
                    .child(ui::icon(glyph).flex_shrink_0())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(tab.kind.title(cx)),
                    )
                    .child(
                        div()
                            .flex()
                            .group_hover(group.clone(), |this| this.invisible())
                            .child(ui::menu_badge(badge, cx).when(staged, |this| {
                                this.text_color(crate::design::tokens(cx).staged)
                            })),
                    )
                    .child(
                        div()
                            .id(("close-tab", ix))
                            .absolute()
                            .right_1p5()
                            .size_7()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.))
                            .invisible()
                            .group_hover(group, |this| this.visible())
                            .hover(|this| this.bg(border))
                            .child(ui::icon(Glyph::X).with_size(px(16.)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_tab(ix, window, cx);
                            })),
                    )
                    .relative()
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.activate(Some(ix), window, cx)),
                    ),
            );
        }

        let workspace_group = v_flex()
            .flex_shrink_0()
            .px_3()
            .py_2()
            .gap_1()
            .child(ui::group_label("Workspace", cx))
            .child(new_connection)
            .when(!self.tabs.is_empty(), |this| {
                this.child(
                    div()
                        .id("open-tabs")
                        .max_h(px(crate::design::ROW_HEIGHT * 5. + 16.))
                        .overflow_y_scroll()
                        .child(items),
                )
            });

        let connections_group = v_flex()
            .flex_1()
            .min_h_0()
            .px_3()
            .pb_3()
            .child(self.browser.clone());

        v_flex()
            .w(px(self.sidebar_width))
            .h_full()
            .flex_shrink_0()
            .bg(sidebar_bg)
            .border_r(SIDEBAR_BORDER)
            .border_color(border)
            .child(header)
            .child(workspace_group)
            .child(connections_group)
    }

    /// `header`: h-14, the sidebar trigger, a separator, the breadcrumb.
    fn render_header(&self, sidebar_open: bool, cx: &mut Context<Self>) -> TitleBar {
        let crumbs = self.crumbs(cx);
        let last = crumbs.len().saturating_sub(1);
        let owner = cx.weak_entity();
        let breadcrumb =
            Breadcrumb::new()
                .text_base()
                .children(crumbs.into_iter().enumerate().map(|(ix, crumb)| {
                    let item = BreadcrumbItem::new(crumb);
                    if ix < last {
                        // Stepping up from an open tab returns to the overview.
                        let owner = owner.clone();
                        item.on_click(move |_, window, cx| {
                            let _ = owner.update(cx, |this, cx| this.activate(None, window, cx));
                        })
                    } else {
                        item.font_weight(FontWeight::MEDIUM)
                    }
                }));
        let toggle = Button::new("workspace-sidebar")
            .ghost()
            .icon(ui::icon(Glyph::PanelLeft))
            .accessibility_label(if sidebar_open {
                "Hide sidebar"
            } else {
                "Show sidebar"
            })
            .tooltip("Toggle sidebar (Ctrl+B)")
            .on_click(cx.listener(|this, _, window, cx| this.toggle_sidebar(window, cx)));

        TitleBar::new()
            .h(px(48.))
            .pl_4()
            .bg(cx.theme().background)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .gap_2()
                    .items_center()
                    .child(toggle)
                    .child(ui::vertical_separator(cx).mx_1())
                    .child(breadcrumb),
            )
    }

    fn render_overview(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if let Some(inspection) = self.browser.read(cx).inspection(cx) {
            return crate::inspector::render(inspection, window, cx);
        }
        let browser = self.browser.read(cx);
        let first_run = browser.connection_count() == 0;
        let quiet = cx.theme().muted_foreground;
        let empty = ui::well(cx)
            .flex_1()
            .min_h(px(240.))
            .items_center()
            .justify_center()
            .gap_3()
            .p_10()
            .child(ui::logo(px(48.), cx).mb_2())
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(if first_run {
                        "No connections yet"
                    } else {
                        "Nothing selected"
                    }),
            )
            .child(
                div()
                    .max_w(px(420.))
                    .text_center()
                    .text_color(quiet)
                    .child(if first_run {
                        "Save a database server or a SQLite file to browse its schema."
                    } else {
                        "Pick a connection in the sidebar to connect and browse its schemas, \
                         tables, columns and indexes."
                    }),
            )
            .when(first_run, |this| {
                this.child(
                    div().pt_3().child(
                        Button::new("start-new-connection")
                            .primary()
                            .icon(ui::icon(Glyph::Plus))
                            .label("New connection")
                            .on_click(cx.listener(|this, _, window, cx| {
                                crate::connections::open_connection_form(
                                    this.connections.clone(),
                                    None,
                                    window,
                                    cx,
                                );
                            })),
                    ),
                )
            });

        v_flex().size_full().child(empty).into_any_element()
    }
}
