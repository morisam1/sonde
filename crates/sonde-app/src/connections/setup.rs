//! First-run setup, using the same fields, validation and storage as the dialog.

use gpui_kit::{
    component::{
        ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        input::{Input, InputEvent, InputState},
        scroll::ScrollableElement as _,
        v_flex,
    },
    prelude::FluentBuilder as _,
    *,
};
use sonde_core::{ConnectionConfig, DbKind, Secret, TlsMode};

use super::{
    Connections,
    form::{ConnectionForm, ConnectionSaved},
};
use crate::{design, layout::Appearance};

actions!(setup, [Advance, Dismiss]);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Step {
    Database,
    Details,
    Review,
}

pub(crate) enum SetupEvent {
    Saved {
        id: String,
        password: Option<Secret>,
    },
    Dismissed,
    AppearanceChanged(Appearance),
}

pub(crate) struct SetupWizard {
    step: Step,
    kind: Option<DbKind>,
    form: Entity<ConnectionForm>,
    focus: FocusHandle,
    url: Entity<InputState>,
    url_problem: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SetupEvent> for SetupWizard {}

impl SetupWizard {
    pub fn new(
        connections: Entity<Connections>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.bind_keys([
            KeyBinding::new("enter", Advance, Some("SetupWizard")),
            KeyBinding::new("escape", Dismiss, Some("SetupWizard")),
        ]);
        let form = cx.new(|cx| ConnectionForm::for_setup(connections, window, cx));
        let url = cx.new(|cx| InputState::new(window, cx).placeholder("Paste a connection URL"));
        let subscriptions = vec![
            cx.observe(&form, |_, _, cx| cx.notify()),
            cx.subscribe(&form, |_, _, event: &ConnectionSaved, cx| {
                cx.emit(SetupEvent::Saved {
                    id: event.id.clone(),
                    password: event.password.clone(),
                });
            }),
            cx.subscribe(&url, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.url_problem = None;
                    cx.notify();
                }
            }),
        ];
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Self {
            step: Step::Database,
            kind: None,
            form,
            focus,
            url,
            url_problem: None,
            _subscriptions: subscriptions,
        }
    }

    fn advance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.form.read(cx).busy() {
            return;
        }
        match self.step {
            Step::Database => match super::url::parse(self.url.read(cx).value().as_str()) {
                Ok(imported) => {
                    self.kind = Some(imported.config.kind());
                    self.form
                        .update(cx, |form, cx| form.import_url(imported, window, cx));
                    self.url.update(cx, |url, cx| url.set_value("", window, cx));
                    self.step = Step::Details;
                    self.form.update(cx, |form, cx| form.focus_name(window, cx));
                }
                Err(problem) => self.url_problem = Some(problem.into()),
            },
            Step::Details => {
                if self.form.update(cx, |form, cx| form.validate(cx)) {
                    self.step = Step::Review;
                    window.focus(&self.focus, cx);
                }
            }
            Step::Review => self.form.update(cx, |form, cx| form.save(window, cx)),
        }
        cx.notify();
    }

    fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.step = match self.step {
            Step::Database | Step::Details => Step::Database,
            Step::Review => Step::Details,
        };
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn choose(&mut self, kind: DbKind, window: &mut Window, cx: &mut Context<Self>) {
        self.kind = Some(kind);
        self.form
            .update(cx, |form, cx| form.choose(kind, window, cx));
        self.url.update(cx, |url, cx| url.set_value("", window, cx));
        self.url_problem = None;
        self.step = Step::Details;
        self.form.update(cx, |form, cx| form.focus_name(window, cx));
        cx.notify();
    }

    fn review(&self, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let Ok((name, config)) = self.form.read(cx).review(cx) else {
            return div()
                .child("Go back to complete the connection details.")
                .into_any_element();
        };
        let mut facts = vec![("Name", name)];
        match config {
            ConnectionConfig::Postgres(config)
            | ConnectionConfig::Mysql(config)
            | ConnectionConfig::Mariadb(config)
            | ConnectionConfig::SqlServer(config)
            | ConnectionConfig::Snowflake(config)
            | ConnectionConfig::Mongodb(config)
            | ConnectionConfig::Redis(config)
            | ConnectionConfig::Cassandra(config)
            | ConnectionConfig::Elasticsearch(config) => facts.extend([
                ("Database", config.database),
                ("Host", config.host),
                ("Port", config.port.to_string()),
                ("User", config.user),
                (
                    "Encryption",
                    match config.tls {
                        TlsMode::Disable => "Off",
                        TlsMode::Prefer => "Prefer",
                        TlsMode::Require => "Require",
                        TlsMode::VerifyFull => "Verify server certificate",
                    }
                    .to_owned(),
                ),
            ]),
            ConnectionConfig::Sqlite(config) => facts.extend([
                ("Database file", config.path.to_string_lossy().into_owned()),
                (
                    "Access",
                    if config.read_only {
                        "Read-only"
                    } else {
                        "Read-write"
                    }
                    .to_owned(),
                ),
            ]),
        }
        let mut summary = v_flex().gap_0();
        for (label, value) in facts {
            summary = summary.child(
                h_flex()
                    .items_start()
                    .gap_4()
                    .py_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .w_24()
                            .flex_shrink_0()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(label),
                    )
                    .child(div().flex_1().min_w_0().text_sm().child(value)),
            );
        }
        v_flex()
            .gap_5()
            .child(summary)
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Test the connection before saving, or connect when you’re ready."),
            )
            .child(self.form.read(cx).feedback(cx))
            .into_any_element()
    }
}

impl Render for SetupWizard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let tokens = crate::design::tokens(cx);
        let (paper, surface, ink, quiet, rule) = (
            theme.background,
            tokens.card,
            theme.foreground,
            theme.muted_foreground,
            theme.border,
        );
        let busy = self.form.read(cx).busy();
        let choosing = self.step == Step::Database;
        let title = match self.step {
            Step::Database => "Connect a database",
            Step::Details => "Connection details",
            Step::Review => "Ready to connect",
        };
        let engine = self.kind.map_or("", DbKind::name);
        let dark = cx.theme().mode.is_dark();
        let url_empty = self.url.read(cx).value().trim().is_empty();
        // A single centred card, like shadcn's login block.
        let mut panel = v_flex()
            .w_full()
            .max_w(px(480.))
            .min_w_0()
            .bg(surface)
            .rounded(px(crate::design::RADIUS_CARD))
            .border_1()
            .border_color(rule)
            .shadow_sm()
            .overflow_hidden()
            .child(
                v_flex()
                    .px_6()
                    .pt_6()
                    .pb_2()
                    .gap_1p5()
                    .child(
                        div()
                            .text_xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(div().text_sm().text_color(quiet).child(match self.step {
                        Step::Database => {
                            "Paste a connection URL, or pick an engine to fill in the details."
                        }
                        Step::Details => "Where Sonde should connect.",
                        Step::Review => "Check the settings, test them, then save.",
                    })),
            );

        if choosing {
            let setup = cx.weak_entity();
            let engines = super::engines::grid("setup", cx, |kind| {
                let setup = setup.clone();
                Box::new(move |_, window, cx| {
                    let _ = setup.update(cx, |this, cx| this.choose(kind, window, cx));
                })
            })
            .px_6()
            .py_4();
            panel = panel
                .child(
                    h_flex()
                        .px_6()
                        .pt_4()
                        .gap_2()
                        .child(
                            Input::new(&self.url)
                                .id("setup-url")
                                .aria_label("Connection URL")
                                .h_10()
                                .prefix(Icon::new(IconName::ExternalLink).text_color(quiet)),
                        )
                        .child(
                            Button::new("setup-next")
                                .outline()
                                .h_10()
                                .icon(IconName::ChevronRight)
                                .accessibility_label("Use connection URL")
                                .tooltip("Use connection URL")
                                .disabled(url_empty)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.advance(window, cx)),
                                ),
                        ),
                )
                .when_some(self.url_problem.clone(), |this, problem| {
                    this.child(
                        div()
                            .id("setup-url-error")
                            .test_support()
                            .role(Role::Status)
                            .aria_label(problem.clone())
                            .px_4()
                            .pt_3()
                            .text_sm()
                            .text_color(cx.theme().danger)
                            .child(problem),
                    )
                })
                .child(engines)
                .child(
                    v_flex()
                        .px_6()
                        .py_4()
                        .gap_3()
                        .border_t_1()
                        .border_color(rule)
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .child("Saved connections"),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(quiet)
                                .child("No connections yet"),
                        ),
                )
                .child(
                    v_flex()
                        .px_4()
                        .py_2()
                        .gap_1()
                        .border_t_1()
                        .border_color(rule)
                        .child(
                            Button::new("setup-skip")
                                .ghost()
                                .h_10()
                                .w_full()
                                .child(
                                    h_flex()
                                        .w_full()
                                        .gap_3()
                                        .child(
                                            Icon::new(IconName::LayoutDashboard).with_size(px(16.)),
                                        )
                                        .child(div().text_sm().child("Open workspace")),
                                )
                                .on_click(
                                    cx.listener(|_, _, _, cx| cx.emit(SetupEvent::Dismissed)),
                                ),
                        )
                        .child(
                            Button::new("setup-theme")
                                .ghost()
                                .h_10()
                                .w_full()
                                .tooltip(if dark {
                                    "Switch to light theme"
                                } else {
                                    "Switch to dark theme"
                                })
                                .child(
                                    h_flex()
                                        .w_full()
                                        .gap_3()
                                        .child(Icon::new(IconName::Palette).with_size(px(16.)))
                                        .child(div().text_sm().child("Theme"))
                                        .child(div().flex_1())
                                        .child(div().text_sm().text_color(quiet).child(if dark {
                                            "Dark"
                                        } else {
                                            "Light"
                                        })),
                                )
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(SetupEvent::AppearanceChanged(if dark {
                                        Appearance::Light
                                    } else {
                                        Appearance::Dark
                                    }));
                                })),
                        ),
                );
        } else {
            panel = panel
                .child(
                    h_flex()
                        .px_4()
                        .py_3()
                        .border_b_1()
                        .border_color(rule)
                        .gap_3()
                        .child(
                            Icon::new(match self.kind {
                                Some(DbKind::Sqlite) => IconName::HardDrive,
                                _ => IconName::Network,
                            })
                            .with_size(px(16.))
                            .when_some(self.kind, |icon, kind| {
                                icon.text_color(design::engine_ink(kind, cx))
                            }),
                        )
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .child(engine),
                        )
                        .child(div().flex_1())
                        .child(
                            div()
                                .id("setup-step")
                                .test_support()
                                .role(Role::Status)
                                .aria_label(if self.step == Step::Details {
                                    "Step 2 of 3"
                                } else {
                                    "Step 3 of 3"
                                })
                                .text_sm()
                                .text_color(quiet)
                                .child(if self.step == Step::Details {
                                    "Details"
                                } else {
                                    "Review"
                                }),
                        ),
                )
                .child(
                    div()
                        .id("setup-scroll")
                        .min_h_0()
                        .max_h(px(
                            (f32::from(window.viewport_size().height) - 248.).max(120.)
                        ))
                        .overflow_y_scrollbar()
                        .child(div().p_5().child(if self.step == Step::Details {
                            self.form.clone().into_any_element()
                        } else {
                            self.review(cx)
                        })),
                )
                .child(
                    h_flex()
                        .p_3()
                        .gap_2()
                        .border_t_1()
                        .border_color(rule)
                        .child(
                            Button::new("setup-back")
                                .ghost()
                                .h_9()
                                .label("Back")
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, window, cx| this.back(window, cx))),
                        )
                        .when(self.step == Step::Review, |this| {
                            this.child(
                                Button::new("setup-test")
                                    .outline()
                                    .h_9()
                                    .label("Test connection")
                                    .loading(self.form.read(cx).testing())
                                    .disabled(busy)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.form.update(cx, |form, cx| form.test(cx));
                                    })),
                            )
                        })
                        .child(div().flex_1())
                        .child(
                            Button::new(if self.step == Step::Review {
                                "setup-connect"
                            } else {
                                "setup-next"
                            })
                            .primary()
                            .h_9()
                            .label(if self.step == Step::Review {
                                "Save and connect"
                            } else {
                                "Continue"
                            })
                            .disabled(busy)
                            .loading(self.form.read(cx).saving())
                            .on_click(cx.listener(|this, _, window, cx| this.advance(window, cx))),
                        ),
                );
        }

        v_flex()
            .id("setup-wizard")
            .test_support()
            .key_context("SetupWizard")
            .track_focus(&self.focus)
            .size_full()
            .min_w_0()
            .bg(paper)
            .text_color(ink)
            .p_4()
            .items_center()
            .justify_center()
            .on_action(cx.listener(|this, _: &Advance, window, cx| this.advance(window, cx)))
            .on_action(cx.listener(|this, _: &Dismiss, _, cx| {
                if !this.form.read(cx).busy() {
                    cx.emit(SetupEvent::Dismissed);
                }
            }))
            .child(panel)
    }
}
