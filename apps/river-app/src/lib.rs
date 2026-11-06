//! River's native UI, shared by every platform shell.
//!
//! A shell (today `apps/desktop`) supplies a [`NodeHost`] that knows how to find, install and
//! start Freenet on its platform; everything the user sees lives here.

pub mod node;
pub mod setup;

use rune::tok::{self, type_};
use rune::{AppHandle, AppSpec, Cx, El, Tx, col, el, row, text};
use rune_kit::{Button, ButtonVariant, Kit};

pub use node::{InstallStep, MANUAL_INSTALL_URL, NodeHost, NodeProbe};
pub use setup::{Controller, Setup};

pub struct RiverApp {
    pub setup: Setup,
    /// `None` in headless snapshots and tests, where nothing runs in the background.
    ctl: Option<Controller>,
}

impl RiverApp {
    /// A window with nothing running behind it, for snapshots and tests.
    pub fn detached() -> Self {
        RiverApp {
            setup: Setup::Checking,
            ctl: None,
        }
    }

    /// The live app: starts checking for a node straight away.
    pub fn attached(ctl: Controller) -> Self {
        ctl.check();
        RiverApp {
            setup: Setup::Checking,
            ctl: Some(ctl),
        }
    }

    fn can_install(&self) -> bool {
        self.ctl.as_ref().is_none_or(Controller::can_install)
    }

    fn install(&mut self) {
        if let Some(ctl) = &self.ctl {
            ctl.install();
        }
    }

    fn retry(&mut self) {
        if let Some(ctl) = &self.ctl {
            ctl.check();
        }
    }
}

/// Named states for `--snap` and `--list-scenes`.
/// A named state for snapshots: a name and how it changes the initial state.
pub type Scene = (&'static str, fn(&mut RiverApp));

pub const SCENES: &[Scene] = &[
    ("checking", |s| s.setup = Setup::Checking),
    ("not-installed", |s| s.setup = Setup::NotInstalled),
    ("downloading", |s| {
        s.setup = Setup::Installing(InstallStep::Downloading {
            received: 9_400_000,
            total: Some(23_372_438),
        })
    }),
    ("verifying", |s| {
        s.setup = Setup::Installing(InstallStep::Verifying)
    }),
    ("starting", |s| s.setup = Setup::Starting),
    ("failed", |s| {
        s.setup = Setup::Failed("Couldn't install Freenet: the download was interrupted.".into())
    }),
    ("connected", |s| {
        s.setup = Setup::Connected {
            version: "0.2.155".into(),
        }
    }),
];

pub fn spec() -> AppSpec<RiverApp> {
    AppSpec {
        scenes: SCENES,
        min_size: Some((420.0, 400.0)),
        ..AppSpec::new("River", (560.0, 440.0), RiverApp::detached, view)
    }
}

/// Build the live app: `init` receives the app's own handle so background work can report back.
pub fn app(make: impl FnOnce(AppHandle<RiverApp>) -> RiverApp + 'static) -> rune::App<RiverApp> {
    let app = rune::App::new(spec());
    let handle = app.handle();
    app.init(move || make(handle))
}

pub fn view(s: &RiverApp, cx: &mut Cx<RiverApp>) -> El {
    let dark = cx.system_dark();
    let t = tok::theme(dark);
    let k = Kit::neutral(dark);
    let body = match &s.setup {
        Setup::Checking => panel(
            t,
            "Looking for Freenet…",
            "River runs on Freenet, a peer-to-peer network on your own computer.",
            None,
            vec![],
        ),
        Setup::NotInstalled if s.can_install() => panel(
            t,
            "Freenet isn't installed",
            "River needs Freenet running on this computer. River can download it from the official release, check its signature, and set it up to run in the background.",
            None,
            vec![
                Button::new("Install Freenet")
                    .variant(ButtonVariant::Contrast)
                    .on_press(|s: &mut RiverApp| s.install())
                    .build(&k)
                    .key("install"),
                manual_install_button(&k),
            ],
        ),
        Setup::NotInstalled => panel(
            t,
            "Freenet isn't installed",
            "River needs Freenet running on this computer. Install it, then try again.",
            None,
            vec![manual_install_button(&k), retry_button(&k)],
        ),
        Setup::Installing(step) => {
            let (title, fraction) = install_progress(step);
            panel(t, title, "This only happens once.", fraction, vec![])
        }
        Setup::Starting => panel(
            t,
            "Starting Freenet…",
            "Connecting to the network. This can take a moment the first time.",
            None,
            vec![],
        ),
        Setup::Failed(message) => panel(
            t,
            "Something went wrong",
            message,
            None,
            vec![retry_button(&k), manual_install_button(&k)],
        ),
        Setup::Connected { version } => panel(
            t,
            "Connected to Freenet",
            &format!("Freenet {version} is running on this computer."),
            None,
            vec![],
        ),
    };
    col()
        .key("root")
        .w_full()
        .h_full()
        .center()
        .fill(t.bg)
        .child(body)
}

fn install_progress(step: &InstallStep) -> (&'static str, Option<f32>) {
    match step {
        InstallStep::FindingRelease => ("Finding the latest Freenet release…", None),
        InstallStep::Downloading { received, total } => (
            "Downloading Freenet…",
            Some(
                total
                    .filter(|t| *t > 0)
                    .map_or(0.0, |t| *received as f32 / t as f32),
            ),
        ),
        InstallStep::Verifying => ("Checking the download's signature…", None),
        InstallStep::Installing => ("Installing Freenet…", None),
        InstallStep::WaitingForInstaller => {
            ("Finish setting up Freenet in its installer window", None)
        }
    }
}

fn panel(t: tok::Theme, title: &str, body: &str, progress: Option<f32>, actions: Vec<El>) -> El {
    let mut card = col()
        .key("panel")
        .w(400.0)
        .gap(12.0)
        .child(
            text("River")
                .key("wordmark")
                .font(type_::LABEL)
                .color(tok::accent(0, true)),
        )
        .child(
            text(title.to_owned())
                .key("title")
                .font(type_::TITLE)
                .color(t.text)
                .wrap()
                .w_full(),
        )
        .child(
            text(body.to_owned())
                .key("body")
                .font(type_::BODY)
                .color(t.text_secondary)
                .wrap()
                .w_full(),
        );
    if let Some(fraction) = progress {
        card = card.child(progress_bar(t, fraction));
    }
    if !actions.is_empty() {
        let mut buttons = row().key("actions").gap(8.0).mt(8.0);
        for action in actions {
            buttons = buttons.child(action);
        }
        card = card.child(buttons);
    }
    card
}

fn progress_bar(t: tok::Theme, fraction: f32) -> El {
    el().key("track")
        .w_full()
        .h(4.0)
        .radius(2.0)
        .fill(t.control_off)
        .child(
            el().key("bar")
                .w(400.0 * fraction.clamp(0.0, 1.0))
                .h(4.0)
                .radius(2.0)
                .fill(tok::accent(0, true))
                .motion(rune::Spring::new(0.3, 0.0)),
        )
}

fn manual_install_button(k: &Kit) -> El {
    Button::new("Install it myself")
        .variant(ButtonVariant::Ghost)
        .on_press_tx(|_: &mut RiverApp, tx: &mut Tx| {
            let _ = tx.open_url(MANUAL_INSTALL_URL);
        })
        .build(k)
        .key("manual")
}

fn retry_button(k: &Kit) -> El {
    Button::new("Try again")
        .variant(ButtonVariant::Contrast)
        .on_press(|s: &mut RiverApp| s.retry())
        .build(k)
        .key("retry")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::{Env, Runtime};

    fn render(setup: Setup) -> Runtime<RiverApp> {
        let mut state = RiverApp::detached();
        state.setup = setup;
        let mut rt = Runtime::new(
            state,
            view,
            (560.0, 440.0),
            Env {
                system_dark: true,
                scale: 1.0,
            },
        );
        rt.frame(0.0);
        rt
    }

    #[test]
    fn each_stage_offers_the_right_actions() {
        let cases = [
            (Setup::Checking, &[][..]),
            (Setup::NotInstalled, &["install", "manual"][..]),
            (Setup::Installing(InstallStep::Verifying), &[][..]),
            (Setup::Starting, &[][..]),
            (Setup::Failed("x".into()), &["retry", "manual"][..]),
            (
                Setup::Connected {
                    version: "1".into(),
                },
                &[][..],
            ),
        ];
        for (setup, expected) in cases {
            let rt = render(setup.clone());
            for key in ["install", "manual", "retry"] {
                assert_eq!(
                    rt.find(key).is_some(),
                    expected.contains(&key),
                    "{setup:?}: button {key}"
                );
            }
        }
    }

    #[test]
    fn a_download_shows_how_far_it_has_got() {
        let rt = render(Setup::Installing(InstallStep::Downloading {
            received: 50,
            total: Some(100),
        }));
        assert!(rt.find("bar").is_some());
        assert_eq!(
            install_progress(&InstallStep::Downloading {
                received: 50,
                total: Some(100)
            })
            .1,
            Some(0.5)
        );
        assert_eq!(
            install_progress(&InstallStep::Downloading {
                received: 50,
                total: None
            })
            .1,
            Some(0.0)
        );
        assert_eq!(
            install_progress(&InstallStep::Downloading {
                received: 50,
                total: Some(0)
            })
            .1,
            Some(0.0)
        );
    }
}
