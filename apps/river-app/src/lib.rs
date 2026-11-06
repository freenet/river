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

/// A named state for `--snap` and `--list-scenes`: a name and how it changes the initial state.
pub type Scene = (&'static str, fn(&mut RiverApp));

/// One scene per screen. CI renders every one of them.
pub const SCENES: &[Scene] = &[
    ("checking", |s| s.setup = Setup::Checking),
    ("not-installed", |s| s.setup = Setup::NotInstalled),
    ("finding-release", |s| {
        s.setup = Setup::Installing(InstallStep::FindingRelease)
    }),
    ("downloading", |s| {
        s.setup = Setup::Installing(InstallStep::Downloading {
            received: 9_400_000,
            total: Some(23_372_438),
        })
    }),
    ("verifying", |s| {
        s.setup = Setup::Installing(InstallStep::Verifying)
    }),
    ("installing", |s| {
        s.setup = Setup::Installing(InstallStep::Installing)
    }),
    ("waiting-for-installer", |s| {
        s.setup = Setup::Installing(InstallStep::WaitingForInstaller)
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
            "This can take a moment the first time.",
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
        // No bar when the size is unknown: an empty bar that never moves looks stuck.
        InstallStep::Downloading { received, total } => (
            "Downloading Freenet…",
            total
                .filter(|t| *t > 0)
                .map(|t| (*received as f32 / t as f32).clamp(0.0, 1.0)),
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

    fn downloading(received: u64, total: Option<u64>) -> InstallStep {
        InstallStep::Downloading { received, total }
    }

    #[test]
    fn only_a_download_of_known_size_shows_a_bar() {
        assert!(
            render(Setup::Installing(downloading(50, Some(100))))
                .find("bar")
                .is_some()
        );
        for step in [
            downloading(50, None),
            downloading(50, Some(0)),
            InstallStep::FindingRelease,
            InstallStep::Verifying,
            InstallStep::Installing,
            InstallStep::WaitingForInstaller,
        ] {
            assert!(
                render(Setup::Installing(step.clone()))
                    .find("bar")
                    .is_none(),
                "{step:?}"
            );
        }
        assert_eq!(install_progress(&downloading(50, Some(100))).1, Some(0.5));
        // A server that sends more than it announced doesn't overflow the bar.
        assert_eq!(install_progress(&downloading(150, Some(100))).1, Some(1.0));
    }

    /// A host that only counts probes, for pressing buttons in a live window.
    struct CountingHost {
        installable: bool,
        probes: std::sync::atomic::AtomicUsize,
    }

    impl NodeHost for CountingHost {
        fn probe(&self) -> NodeProbe {
            self.probes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            NodeProbe::NotInstalled
        }
        fn can_install(&self) -> bool {
            self.installable
        }
        fn install(&self, _: &mut dyn FnMut(InstallStep)) -> Result<(), String> {
            Ok(())
        }
        fn start(&self) -> Result<(), String> {
            Ok(())
        }
    }

    fn render_with_host(
        setup: Setup,
        installable: bool,
    ) -> (Runtime<RiverApp>, std::sync::Arc<CountingHost>) {
        let host = std::sync::Arc::new(CountingHost {
            installable,
            probes: Default::default(),
        });
        let ctl = Controller::reporting_to(host.clone(), |_| {});
        let state = RiverApp {
            setup,
            ctl: Some(ctl),
        };
        let env = Env {
            system_dark: true,
            scale: 1.0,
        };
        let mut rt = Runtime::new(state, view, (560.0, 440.0), env);
        rt.frame(0.0);
        (rt, host)
    }

    #[test]
    fn a_platform_that_cannot_install_offers_no_install_button() {
        let (rt, _) = render_with_host(Setup::NotInstalled, false);
        assert!(rt.find("install").is_none());
        assert!(rt.find("manual").is_some());
        assert!(rt.find("retry").is_some());
    }

    #[test]
    fn pressing_retry_checks_for_the_node_again() {
        let (mut rt, host) = render_with_host(Setup::Failed("x".into()), true);
        let (x, y) = rt.find("retry").expect("retry button");
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        rt.frame(1.0 / 60.0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while host.probes.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < deadline, "retry never probed");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Fails to compile when a new screen is added, and fails when it has no scene.
    #[test]
    fn every_screen_has_a_scene() {
        fn screen(s: &Setup) -> &'static str {
            match s {
                Setup::Checking => "checking",
                Setup::NotInstalled => "not-installed",
                Setup::Installing(InstallStep::FindingRelease) => "finding-release",
                Setup::Installing(InstallStep::Downloading { .. }) => "downloading",
                Setup::Installing(InstallStep::Verifying) => "verifying",
                Setup::Installing(InstallStep::Installing) => "installing",
                Setup::Installing(InstallStep::WaitingForInstaller) => "waiting-for-installer",
                Setup::Starting => "starting",
                Setup::Failed(_) => "failed",
                Setup::Connected { .. } => "connected",
            }
        }
        let mut covered: Vec<&str> = SCENES
            .iter()
            .map(|(name, apply)| {
                let mut s = RiverApp::detached();
                apply(&mut s);
                assert_eq!(screen(&s.setup), *name, "scene {name} shows another screen");
                *name
            })
            .collect();
        covered.sort();
        covered.dedup();
        assert_eq!(covered.len(), 10, "{covered:?}");
    }
}
