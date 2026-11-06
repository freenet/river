//! River's native UI, shared by every platform shell.
//!
//! A shell (today `apps/desktop`) supplies a [`NodeHost`] that knows how to find, install and
//! start Freenet on its platform; everything the user sees lives here.

pub mod chat;
pub mod node;
pub mod setup;

use rune::{AppHandle, AppSpec, Cx, El, Tx, col, el, row, text};
use rune_kit::{Button, ButtonVariant, Kit};

pub use chat::Chat;
pub use node::{InstallStep, MANUAL_INSTALL_URL, NodeHost, NodeProbe};
pub use setup::{Controller, Setup};

pub struct RiverApp {
    pub setup: Setup,
    /// The chat screen, shown once setup reaches [`Setup::Connected`]. It holds example data
    /// for now ([`chat::Source::Example`]): nothing is read from or sent to the node yet.
    pub chat: Chat,
    /// `None` in headless snapshots and tests, where nothing runs in the background.
    ctl: Option<Controller>,
    /// River's logo, rasterized once by [`load_logo`]. `None` until then, and in tests, which
    /// draw an empty square in its place.
    logo: Option<Logo>,
}

/// The logo and the cache that keeps its pixels alive.
struct Logo {
    _cache: rune::image::ImageCache,
    image: rune::image::Image,
}

/// The web UI's logo, so both apps carry the same mark. It is shared: an edit to it changes
/// this app too, which is why `apps.yml` runs for that file.
const LOGO_SVG: &[u8] = include_bytes!("../../../ui/assets/river_logo.svg");
const LOGO_PATH: &str = "ui/assets/river_logo.svg";
/// The logo's largest drawn size in points (the setup card draws it at this size, the room
/// list smaller), and the scale it rasterizes at. Smaller draws use its mips.
const LOGO_PT: f32 = 36.0;
const LOGO_SCALE: f32 = 2.0;

/// `AppSpec::setup`: start rasterizing the logo on the image cache's worker. A headless run
/// (`--snap`) waits for it, so snapshots show it, and fails if it does not rasterize, so a
/// broken logo stops CI's render step instead of rendering as an empty square.
fn load_logo(s: &mut RiverApp, waker: rune::Waker) {
    let cache = rune::image::ImageCache::new(waker);
    let image = cache.svg(LOGO_SVG, LOGO_PT, LOGO_SCALE);
    if rune::is_headless()
        && let Err(e) = image.wait()
    {
        panic!("River's logo ({LOGO_PATH}) did not rasterize: {e}");
    }
    s.logo = Some(Logo {
        _cache: cache,
        image,
    });
}

impl RiverApp {
    /// A window with nothing running behind it, for snapshots and tests.
    pub fn detached() -> Self {
        RiverApp {
            setup: Setup::Checking,
            // TODO: example rooms; this goes once the app reads real rooms from the node.
            chat: Chat::example(),
            ctl: None,
            logo: None,
        }
    }

    /// The live app: starts checking for a node straight away.
    pub fn attached(ctl: Controller) -> Self {
        ctl.check();
        RiverApp {
            setup: Setup::Checking,
            // TODO: example rooms; this goes once the app reads real rooms from the node.
            chat: Chat::example(),
            ctl: Some(ctl),
            logo: None,
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
    // Connected: the chat screen, on example data.
    ("chat", connect),
    ("chat-members-hidden", |s| {
        connect(s);
        s.chat.members_open = false;
    }),
    ("chat-private-room", |s| {
        connect(s);
        s.chat.select(chat::example::PRIVATE_ROOM);
    }),
    ("chat-composing", |s| {
        connect(s);
        s.chat
            .composer
            .replace_text("Sounds good, I'll bring the slides on Thursday");
        s.chat.composer.focused = true;
    }),
    ("chat-empty-room", |s| {
        connect(s);
        s.chat.select(chat::example::EMPTY_ROOM);
    }),
    // Scenes named `*narrow*` are rendered at a narrow size (`--size 820x720`, see
    // `.github/workflows/apps.yml`): the layout follows the window's width, which a scene
    // cannot set. At the default size they look like the wide layout.
    ("chat-narrow", connect),
    ("chat-narrow-members", |s| {
        connect(s);
        s.chat.members_overlay = true;
    }),
];

fn connect(s: &mut RiverApp) {
    s.setup = Setup::Connected {
        version: "0.2.155".into(),
    };
}

/// The window's opening size, in points: room for the room list, a conversation and the
/// members panel side by side.
pub const WINDOW_SIZE: (f32, f32) = (1280.0, 800.0);

/// The size the `*narrow*` scenes are rendered at. `.github/workflows/apps.yml` passes it as
/// `--size 820x720`; keep the two in step.
pub const NARROW_SCENE_SIZE: (f32, f32) = (820.0, 720.0);

/// The window size a scene is meant to be rendered at.
pub fn scene_size(name: &str) -> (f32, f32) {
    if name.contains("narrow") {
        NARROW_SCENE_SIZE
    } else {
        WINDOW_SIZE
    }
}

pub fn spec() -> AppSpec<RiverApp> {
    AppSpec {
        scenes: SCENES,
        min_size: Some((640.0, 480.0)),
        setup: Some(load_logo),
        ..AppSpec::new("River", WINDOW_SIZE, RiverApp::detached, view)
    }
}

/// Build the live app: `init` receives the app's own handle so background work can report back.
pub fn app(make: impl FnOnce(AppHandle<RiverApp>) -> RiverApp + 'static) -> rune::App<RiverApp> {
    let app = rune::App::new(spec());
    let handle = app.handle();
    app.init(move || make(handle))
}

pub fn view(s: &RiverApp, cx: &mut Cx<RiverApp>) -> El {
    // River is dark-only for now, like the chat screen; the setup screens match it.
    let palette = chat::style::palette();
    let k = Kit::neutral(true);
    let logo = s.logo.as_ref().map(|l| l.image.pixels());
    let t = (&palette, logo);
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
                // For someone who installed Freenet another way and came back.
                Button::new("Check again")
                    .variant(ButtonVariant::Ghost)
                    .on_press(|s: &mut RiverApp| s.retry())
                    .build(&k)
                    .key("retry"),
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
            let progress = fraction.map(|f| (f, download_detail(step)));
            panel(t, title, "This only happens once.", progress, vec![])
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
        Setup::Connected { .. } => return chat::view(&s.chat, logo, cx),
    };
    col()
        .key("root")
        .w_full()
        .h_full()
        .center()
        .padding(24.0)
        .fill(palette.rail)
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
        // The release's signature was checked before downloading; this matches the download
        // against the signed checksum.
        InstallStep::Verifying => ("Checking the download…", None),
        InstallStep::Installing => ("Installing Freenet…", None),
        InstallStep::WaitingForInstaller => {
            ("Finish setting up Freenet in its installer window", None)
        }
    }
}

/// "9.4 of 23.4 MB" under the download bar, when the download's size is known.
fn download_detail(step: &InstallStep) -> Option<String> {
    let InstallStep::Downloading {
        received,
        total: Some(total),
    } = step
    else {
        return None;
    };
    let mb = |b: u64| b as f64 / 1_000_000.0;
    Some(format!(
        "{:.1} of {:.1} MB",
        mb((*received).min(*total)),
        mb(*total)
    ))
}

/// The setup card's inner width.
const PANEL_W: f32 = 380.0;
/// The card's least height. With it every step's card is the same height, so the card, which
/// is centred, keeps its top (and the logo) still as setup moves from step to step.
const PANEL_MIN_H: f32 = 276.0;

type Look<'a> = (&'a chat::style::Palette, Option<&'a rune::Pixels>);

/// A progress bar's fraction, and the text under it.
type Progress = Option<(f32, Option<String>)>;

fn panel(t: Look, title: &str, body: &str, progress: Progress, actions: Vec<El>) -> El {
    let (p, logo) = t;
    let mark = match logo {
        Some(px) => el().size(LOGO_PT, LOGO_PT).pixels(px),
        None => el().size(LOGO_PT, LOGO_PT),
    };
    let mut card = col()
        .key("panel")
        .w(PANEL_W + 64.0)
        .min_h(PANEL_MIN_H)
        .padding(32.0)
        .gap(12.0)
        .radius(16.0)
        .fill(p.bg)
        .border(1.0, p.line)
        .shadow(18.0, 48.0, p.shadow)
        .child(
            row()
                .key("wordmark")
                .gap(10.0)
                .mb(10.0)
                .center_y()
                .child(mark)
                .child(text("River").font(chat::style::BRAND).color(p.text)),
        )
        .child(
            text(title.to_owned())
                .key("title")
                .font(chat::style::TITLE)
                .color(p.text)
                .wrap()
                .w_full(),
        )
        .child(
            text(body.to_owned())
                .key("body")
                .font(chat::style::BODY.lh(21.0))
                .color(p.text_secondary)
                .wrap()
                .w_full(),
        );
    if let Some((fraction, detail)) = progress {
        card = card.child(progress_bar(p, fraction));
        if let Some(detail) = detail {
            card = card.child(
                text(detail)
                    .key("progress-detail")
                    .font(chat::style::SMALL)
                    .color(p.text_muted),
            );
        }
    }
    if !actions.is_empty() {
        let mut buttons = row().key("actions").gap(8.0).mt(10.0).flex_wrap();
        for action in actions {
            buttons = buttons.child(action);
        }
        card = card.child(buttons);
    }
    card
}

fn progress_bar(p: &chat::style::Palette, fraction: f32) -> El {
    el().key("track")
        .w_full()
        .h(4.0)
        .mt(6.0)
        .radius(2.0)
        .fill(p.surface_hover)
        .child(
            el().key("bar")
                .w(PANEL_W * fraction.clamp(0.0, 1.0))
                .h(4.0)
                .radius(2.0)
                .fill(p.accent)
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
            (Setup::NotInstalled, &["install", "manual", "retry"][..]),
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
            chat: Chat::example(),
            ctl: Some(ctl),
            logo: None,
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
                Setup::Connected { .. } => "chat",
            }
        }
        let mut covered: Vec<&str> = SCENES
            .iter()
            .map(|(name, apply)| {
                let mut s = RiverApp::detached();
                apply(&mut s);
                let shown = screen(&s.setup);
                // Variants of one screen share its name as a prefix ("chat-private-room").
                assert!(
                    *name == shown || name.starts_with(&format!("{shown}-")),
                    "scene {name} shows the {shown} screen"
                );
                shown
            })
            .collect();
        covered.sort();
        covered.dedup();
        assert_eq!(covered.len(), 10, "{covered:?}");
    }

    /// The logo is a gradient SVG shared with the web UI; this fails if an edit there breaks
    /// it for the native renderer. It checks the raster's size only: Rune's image handle does
    /// not expose decoded pixels, and checking coverage directly would mean depending on
    /// resvg here. The CI render step shows the result.
    #[test]
    fn the_logo_rasterizes() {
        let cache = rune::image::ImageCache::new(std::sync::Arc::new(|| {}));
        let image = cache.svg(LOGO_SVG, LOGO_PT, LOGO_SCALE);
        let [w, h] = image.wait().expect("the logo rasterizes");
        let px = (LOGO_PT * LOGO_SCALE) as u32;
        assert!(w > px / 2 && h > px / 2 && w <= px && h <= px, "{w}x{h}");
    }

    /// CI renders the `*narrow*` scenes with a `--size` it spells out itself; it must be the
    /// size [`scene_size`] and the tests use.
    #[test]
    fn ci_renders_narrow_scenes_at_the_narrow_size() {
        const WORKFLOW: &str = include_str!("../../../.github/workflows/apps.yml");
        let line = WORKFLOW
            .lines()
            .find(|l| l.contains("*narrow*)"))
            .expect("apps.yml picks a size for the narrow scenes");
        let size = line
            .split("--size ")
            .nth(1)
            .and_then(|rest| rest.split(|c: char| !c.is_ascii_alphanumeric()).next())
            .expect("a --size WxH");
        let (w, h) = NARROW_SCENE_SIZE;
        assert_eq!(size, format!("{w}x{h}"));
        // And the size it picks reaches the render.
        let snap = WORKFLOW
            .lines()
            .find(|l| l.contains("--snap"))
            .expect("apps.yml renders scenes with --snap");
        assert!(snap.contains("\"${size[@]}\""), "{snap}");
    }

    #[test]
    fn a_download_of_known_size_says_how_far_it_is() {
        assert_eq!(
            download_detail(&downloading(9_400_000, Some(23_372_438))).as_deref(),
            Some("9.4 of 23.4 MB")
        );
        assert_eq!(download_detail(&downloading(9_400_000, None)), None);
        assert_eq!(
            download_detail(&downloading(150, Some(100))).as_deref(),
            Some("0.0 of 0.0 MB")
        );
        let rt = render(Setup::Installing(downloading(50, Some(100))));
        assert!(rt.find("progress-detail").is_some());
    }
}
