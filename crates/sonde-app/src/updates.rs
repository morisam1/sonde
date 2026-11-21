//! Keeping Sonde up to date, with the user's consent.
//!
//! The first start asks whether Sonde may check for new versions, and
//! whether it may install them by itself. Until the user answers, nothing is
//! checked. Checks ask GitHub for the release list, at most once a day, and
//! send nothing else. The answer, the channel and the time of the last check
//! are kept in `updates.toml` beside the other settings, and can be changed
//! in the Settings panel.

use std::{
    fs,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};
use gpui_kit::*;
use semver::Version;
use serde::{Deserialize, Serialize};
use sonde_core::update::{self, Channel, Release, Source};

/// How long between automatic checks.
const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// How often a running Sonde looks at whether a check is due.
const WAKE_EVERY: Duration = Duration::from_secs(60 * 60);

/// What the user allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Consent {
    /// Check daily, download and install new versions without asking.
    Automatic,
    /// Check daily and say when a new version is out; install on request.
    Ask,
    /// Never check by itself.
    Off,
}

impl Consent {
    pub const ALL: [Self; 3] = [Self::Automatic, Self::Ask, Self::Off];

    pub fn label(self) -> &'static str {
        match self {
            Self::Automatic => "Install automatically",
            Self::Ask => "Ask before installing",
            Self::Off => "Don't check",
        }
    }
}

/// `updates.toml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// `None` until the user has answered the first-start question.
    pub consent: Option<Consent>,
    pub channel: Channel,
    /// Seconds since the Unix epoch of the last check that got an answer.
    pub last_check: Option<u64>,
    /// Another `owner/name` on GitHub to take releases from, for forks.
    pub repository: Option<String>,
}

/// Where an update stands.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Installing(Release),
    /// Installed; it runs from the next start.
    Installed(Release),
    Failed(SharedString),
}

/// The running version: the release's, or the crate's for a local build.
pub fn version() -> Version {
    let text = option_env!("SONDE_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"));
    Version::parse(text.trim_start_matches('v')).unwrap_or_else(|_| Version::new(0, 0, 0))
}

/// Whether this build can replace itself: release builds can; a build run
/// from a source checkout would overwrite the developer's binary.
fn installable() -> bool {
    !cfg!(debug_assertions) && option_env!("SONDE_VERSION").is_some()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The updater's state, shared by the menu, the Settings panel and the
/// background checks.
pub struct Updater {
    path: Option<PathBuf>,
    pub prefs: Prefs,
    pub status: Status,
    window: Option<AnyWindowHandle>,
}

impl Global for Updater {}

impl Updater {
    fn load(path: Option<PathBuf>) -> Self {
        let prefs = path
            .as_ref()
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default();
        Self {
            path,
            prefs,
            status: Status::Idle,
            window: None,
        }
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Ok(text) = toml::to_string(&self.prefs) {
            let header = "# Sonde's update preferences. Change them in Settings.\n";
            let _ = fs::write(path, format!("{header}{text}"));
        }
    }

    fn source(&self) -> Source {
        match &self.prefs.repository {
            Some(repository) => Source {
                repository: repository.clone(),
                ..Source::default()
            },
            None => Source::default(),
        }
    }

    fn due(&self) -> bool {
        matches!(self.prefs.consent, Some(Consent::Automatic | Consent::Ask))
            && self
                .prefs
                .last_check
                .is_none_or(|last| now().saturating_sub(last) >= CHECK_EVERY.as_secs())
    }
}

/// Make sure the updater's state exists, for code (and tests) that build a
/// workspace without [`start`]. It then never checks by itself.
pub fn ensure(cx: &mut App) {
    if !cx.has_global::<Updater>() {
        cx.set_global(Updater::load(None));
    }
}

/// Set up the updater for the main window: ask for consent on first start,
/// then check when a check is due, now and every hour after.
pub fn start(config_dir: Option<PathBuf>, window: AnyWindowHandle, cx: &mut App) {
    let mut updater = Updater::load(config_dir.map(|dir| dir.join("updates.toml")));
    updater.window = Some(window);
    let ask = updater.prefs.consent.is_none();
    cx.set_global(updater);

    if ask {
        // After the window has drawn, so the question sits on the workspace.
        cx.spawn(async move |cx| {
            cx.background_executor()
                .timer(Duration::from_millis(600))
                .await;
            cx.update(|cx| {
                let _ = window.update(cx, |_, window, cx| ask_consent(window, cx));
            });
        })
        .detach();
    }
    cx.spawn(async move |cx| {
        loop {
            cx.update(|cx| {
                if cx.global::<Updater>().due() {
                    check(false, cx);
                }
            });
            cx.background_executor().timer(WAKE_EVERY).await;
        }
    })
    .detach();
}

/// Ask whether Sonde may check for and install updates.
pub fn ask_consent(window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, |dialog, _, cx| {
        let dialog = dialog.overlay_closable(false);
        let quiet = cx.theme().muted_foreground;
        let choose = |consent: Consent| {
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                set_consent(consent, cx);
                window.close_dialog(cx);
            }
        };
        dialog
            .title("Keep Sonde up to date?")
            .w(px(520.))
            .p_6()
            .child(
                v_flex()
                    .gap_3()
                    .child(div().text_sm().child(
                        "Sonde can check for a new version once a day. It asks GitHub for \
                         the list of Sonde releases and sends nothing about you or your \
                         databases.",
                    ))
                    .child(div().text_sm().text_color(quiet).child(
                        "New versions are installed only when their checksum matches, and \
                         start the next time you open Sonde. You can change this any time \
                         in Settings.",
                    ))
                    .child(
                        h_flex()
                            .pt_2()
                            .gap_2()
                            .child(
                                Button::new("updates-off")
                                    .ghost()
                                    .label(Consent::Off.label())
                                    .on_click(choose(Consent::Off)),
                            )
                            .child(div().flex_1())
                            .child(
                                Button::new("updates-ask")
                                    .outline()
                                    .label(Consent::Ask.label())
                                    .on_click(choose(Consent::Ask)),
                            )
                            .child(
                                Button::new("updates-automatic")
                                    .primary()
                                    .label(Consent::Automatic.label())
                                    .on_click(choose(Consent::Automatic)),
                            ),
                    ),
            )
    });
}

/// Record the user's answer, and check at once when it allows checks.
pub fn set_consent(consent: Consent, cx: &mut App) {
    let updater = cx.global_mut::<Updater>();
    updater.prefs.consent = Some(consent);
    updater.save();
    if cx.global::<Updater>().due() {
        check(false, cx);
    }
    cx.refresh_windows();
}

pub fn set_channel(channel: Channel, cx: &mut App) {
    let updater = cx.global_mut::<Updater>();
    if updater.prefs.channel != channel {
        updater.prefs.channel = channel;
        // A different channel may have a different newest version.
        updater.prefs.last_check = None;
        updater.status = Status::Idle;
        updater.save();
    }
    cx.refresh_windows();
}

fn notify(message: String, cx: &mut App) {
    if let Some(window) = cx.global::<Updater>().window {
        let _ = window.update(cx, |_, window, cx| window.push_notification(message, cx));
    }
}

fn set_status(status: Status, cx: &mut App) {
    cx.global_mut::<Updater>().status = status;
    cx.refresh_windows();
}

/// Look for a new version. `manual` checks are the user's own request: they
/// run whatever the consent, and report when there is nothing new.
pub fn check(manual: bool, cx: &mut App) {
    let updater = cx.global::<Updater>();
    if matches!(updater.status, Status::Checking | Status::Installing(_)) {
        return;
    }
    let source = updater.source();
    let channel = updater.prefs.channel;
    set_status(Status::Checking, cx);
    cx.spawn(async move |cx| {
        let found = update::check(&source, channel, &version()).await;
        cx.update(|cx| match found {
            Ok(found) => {
                let updater = cx.global_mut::<Updater>();
                updater.prefs.last_check = Some(now());
                updater.save();
                match found {
                    None => {
                        set_status(Status::UpToDate, cx);
                        if manual {
                            notify(format!("Sonde {} is the newest version.", version()), cx);
                        }
                    }
                    Some(release) => {
                        let automatic =
                            cx.global::<Updater>().prefs.consent == Some(Consent::Automatic);
                        set_status(Status::Available(release.clone()), cx);
                        if automatic && installable() {
                            install(cx);
                        } else {
                            notify(
                                format!(
                                    "Sonde {} is available. Install it from the menu next \
                                         to the Sonde logo.",
                                    release.version
                                ),
                                cx,
                            );
                        }
                    }
                }
            }
            Err(error) => {
                set_status(Status::Failed(error.to_string().into()), cx);
                if manual {
                    notify(format!("Could not check for updates: {error}"), cx);
                }
            }
        });
    })
    .detach();
}

/// Download and install the available version.
pub fn install(cx: &mut App) {
    let Status::Available(release) = cx.global::<Updater>().status.clone() else {
        return;
    };
    if !installable() {
        notify(
            "This build of Sonde was not installed from a release, so it does not replace \
             itself. Download the new version from the releases page."
                .into(),
            cx,
        );
        return;
    }
    set_status(Status::Installing(release.clone()), cx);
    cx.spawn(async move |cx| {
        let dir = std::env::temp_dir().join(format!("sonde-update-{}", release.version));
        let outcome = match update::download(&release, &dir).await {
            Ok(package) => {
                let exe =
                    std::env::current_exe().map_err(|e| sonde_core::Error::Config(e.to_string()));
                let installed = cx
                    .background_spawn(async move { update::install(&package, &exe?) })
                    .await;
                let _ = fs::remove_dir_all(&dir);
                installed
            }
            Err(error) => Err(error),
        };
        cx.update(|cx| match outcome {
            Ok(()) => {
                set_status(Status::Installed(release.clone()), cx);
                notify(
                    format!(
                        "Sonde {} is installed. It starts the next time you open Sonde, or \
                         restart now from the menu.",
                        release.version
                    ),
                    cx,
                );
            }
            Err(error) => {
                set_status(Status::Available(release.clone()), cx);
                notify(format!("The update could not be installed: {error}"), cx);
            }
        });
    })
    .detach();
}

/// Start the installed version and quit this one.
pub fn restart(cx: &mut App) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let bundle = exe
        .ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .map(std::path::Path::to_path_buf);
    let started = match bundle {
        Some(bundle) if cfg!(target_os = "macos") => std::process::Command::new("open")
            .arg("-n")
            .arg(bundle)
            .spawn(),
        _ => std::process::Command::new(exe).spawn(),
    };
    if started.is_ok() {
        cx.quit();
    }
}

/// The menu entry for the current state, and what choosing it does.
pub fn menu_entry(cx: &App) -> (String, bool) {
    match &cx.global::<Updater>().status {
        Status::Checking => ("Checking for updates…".into(), false),
        Status::Available(release) => (format!("Install Sonde {}", release.version), true),
        Status::Installing(release) => (format!("Installing Sonde {}…", release.version), false),
        Status::Installed(_) => ("Restart to update".into(), true),
        _ => ("Check for updates".into(), true),
    }
}

/// What the menu entry does.
pub fn menu_action(cx: &mut App) {
    match cx.global::<Updater>().status {
        Status::Available(_) => install(cx),
        Status::Installed(_) => restart(cx),
        Status::Checking | Status::Installing(_) => {}
        _ => check(true, cx),
    }
}

/// The Updates section of the Settings panel.
pub fn settings_section(cx: &App) -> impl IntoElement + use<> {
    use gpui_kit::component::radio::{Radio, RadioGroup};
    let updater = cx.global::<Updater>();
    let consent = updater.prefs.consent;
    let channel = updater.prefs.channel;
    let status = match &updater.status {
        Status::Idle => match updater.prefs.last_check {
            Some(_) => format!("Sonde {} is installed.", version()),
            None => format!("Sonde {}. Not checked yet.", version()),
        },
        Status::Checking => "Checking…".into(),
        Status::UpToDate => format!("Sonde {} is the newest version.", version()),
        Status::Available(release) => format!("Sonde {} is available.", release.version),
        Status::Installing(release) => format!("Installing Sonde {}…", release.version),
        Status::Installed(release) => format!("Sonde {} starts next time.", release.version),
        Status::Failed(problem) => format!("The last check failed: {problem}"),
    };
    let (action, enabled) = menu_entry(cx);
    v_flex()
        .id("settings-updates")
        .test_support()
        .gap_3()
        .pt_4()
        .border_t_1()
        .border_color(cx.theme().border)
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child("Updates"),
        )
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("When a new version is out"),
        )
        .child(
            RadioGroup::vertical("settings-update-consent")
                .children(Consent::ALL.map(|c| Radio::new(c.label()).label(c.label())))
                .selected_index(consent.and_then(|c| Consent::ALL.iter().position(|k| *k == c)))
                .on_change(|index: &usize, _, cx| set_consent(Consent::ALL[*index], cx)),
        )
        .child(
            div()
                .pt_1()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("Channel"),
        )
        .child(
            RadioGroup::horizontal("settings-update-channel")
                .child(Radio::new("stable").label("Stable releases"))
                .child(Radio::new("nightly").label("Nightly builds"))
                .selected_index(Some(usize::from(channel == Channel::Nightly)))
                .on_change(|index: &usize, _, cx| {
                    set_channel(
                        if *index == 1 {
                            Channel::Nightly
                        } else {
                            Channel::Stable
                        },
                        cx,
                    );
                }),
        )
        .child(
            h_flex()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(status),
                )
                .child(
                    Button::new("settings-check-updates")
                        .outline()
                        .label(action)
                        .disabled(!enabled)
                        .on_click(|_, _, cx| menu_action(cx)),
                ),
        )
}
