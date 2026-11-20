//! Keeping River itself up to date: what the platform must provide ([`UpdateHost`]), the
//! background work ([`UpdateController`]), and what the screen shows ([`UpdateUi`], the
//! "Update available / Restart to update" block and the settings dialog).
//!
//! The verification, download and swap are the desktop shell's (`apps/desktop/src/update/`):
//! this module never sees a key, a hash or a path. It asks the host to check, to download and to
//! restart, and shows what comes back.
//!
//! # What the person sees
//!
//! - **Automatic (the default).** A newer signed version downloads in the background, is
//!   verified, and then the room list shows "Version X is ready" with a Restart to update
//!   button, and a system notification says so once. River is never restarted by itself: a
//!   restart can lose what is half typed, so it waits for the press.
//! - **Automatic off.** The check still runs, so "Version X is available" shows, with a
//!   Download button; nothing is downloaded until it is pressed.
//! - **A copy something else updates** (an AppImage, Flatpak, a distro package, a development
//!   build): the update is announced but the settings say who updates it, and River replaces
//!   nothing.
//! - **Freenet** is not updated here. The node updates itself; the settings show its version,
//!   say so, and note when it has changed under the app. How the node does it (freenet-core,
//!   `crates/core/src/bin/commands/auto_update.rs`): it polls GitHub releases about every 6
//!   hours and on a version mismatch with peers, waits a random 0 to 4 hours, then EXITS with
//!   code 42; the service supervisor it was installed under (systemd, launchd, the Windows
//!   wrapper) runs `freenet update` and starts it again. Only `--disable-auto-update` on the
//!   node's command line turns that off, and River never passes it (pinned by a test in the
//!   desktop crate). A node River did not start is none of River's business: it is never
//!   stopped, replaced or restarted. The one thing River does about the node's version is
//!   to compare it with the floor it needs, 0.2.156 for the app-grant flow, and explain.
//!
//! The check runs shortly after start and then every 6 to 12 hours (a random delay, so every
//! install does not ask in the same minute).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// What the platform says about updating this copy of River.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateSupport {
    /// River may download, replace and restart itself.
    Automatic,
    /// Someone else updates this copy; the text says who, for the person.
    External(String),
    /// River cannot update this copy (not written for this platform yet, no permission, no
    /// key in this build); the text says why.
    Unavailable(String),
}

/// A newer version, as the host verified it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateInfo {
    pub version: String,
    pub notes: String,
    pub size: u64,
    /// The running version is below the signed minimum. Still never forced.
    pub required: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateCheck {
    UpToDate,
    Available(UpdateInfo),
}

/// What the desktop shell does for River's self-update. Every method blocks and runs on a
/// worker thread.
pub trait UpdateHost: Send + Sync + 'static {
    /// The running version, `MAJOR.MINOR.PATCH`.
    fn version(&self) -> String;
    fn support(&self) -> UpdateSupport;
    /// A version already downloaded and verified by an earlier run, waiting for a restart.
    fn staged(&self) -> Option<String> {
        None
    }
    /// This version has shown it works (it connected to the node, or has run a while): end
    /// the probation after an update. Idempotent.
    fn confirm_started(&self) {}
    /// A message about a rollback that happened while River was not watching, once.
    fn take_notice(&self) -> Option<String> {
        None
    }
    /// Fetch and verify the signed manifest.
    fn check(&self) -> Result<UpdateCheck, String>;
    /// Download and verify what the last check offered, reporting `(received, total)`.
    /// Returns the version now ready.
    fn download(&self, progress: &mut dyn FnMut(u64, u64)) -> Result<String, String>;
    /// Install the downloaded version and start it. When it starts, this process exits and the
    /// call does not return. An `Err` means nothing changed (or the old version was put back)
    /// and says why.
    fn restart(&self) -> Result<(), String>;
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum UpdateStatus {
    /// Nothing is known yet.
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(UpdateInfo),
    Downloading {
        version: String,
        received: u64,
        total: u64,
    },
    /// Downloaded and verified: Restart to update.
    Ready {
        version: String,
    },
    Restarting,
    /// The last attempt failed. Shown in the settings, not as a banner.
    Failed(String),
}

/// What the controller tells the window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateEvent {
    Status(UpdateStatus),
    /// A message that is always shown (a rollback).
    Notice(String),
}

/// How long to wait before the first check, and between checks.
const FIRST_CHECK: (Duration, Duration) = (Duration::from_secs(15), Duration::from_secs(45));
const CHECK_EVERY: (Duration, Duration) = (
    Duration::from_secs(6 * 3600),
    Duration::from_secs(12 * 3600),
);

/// A time in `[lo, hi]`, spread by the clock and the process, which is all the spread needs.
fn jittered((lo, hi): (Duration, Duration)) -> Duration {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| {
        d.subsec_nanos() as u64 ^ (d.as_secs().wrapping_mul(2654435761))
    });
    let spread = (hi - lo).as_millis().max(1) as u64;
    lo + Duration::from_millis(nanos.wrapping_add(u64::from(std::process::id())) % spread)
}

/// Runs the checks, downloads and restarts on worker threads and reports to the window.
/// One piece of work at a time: a request made while one runs is ignored.
#[derive(Clone)]
pub struct UpdateController {
    host: Arc<dyn UpdateHost>,
    report: Arc<dyn Fn(UpdateEvent) + Send + Sync>,
    busy: Arc<AtomicBool>,
    auto: Arc<AtomicBool>,
}

impl UpdateController {
    pub fn new(
        host: Arc<dyn UpdateHost>,
        auto: bool,
        report: impl Fn(UpdateEvent) + Send + Sync + 'static,
    ) -> Self {
        UpdateController {
            host,
            report: Arc::new(report),
            busy: Arc::new(AtomicBool::new(false)),
            auto: Arc::new(AtomicBool::new(auto)),
        }
    }

    pub fn host(&self) -> &Arc<dyn UpdateHost> {
        &self.host
    }

    pub fn set_auto(&self, auto: bool) {
        self.auto.store(auto, Ordering::SeqCst);
    }

    fn status(&self, status: UpdateStatus) {
        (self.report)(UpdateEvent::Status(status));
    }

    /// Take the single work slot, or `None` if something is running.
    fn claim(&self) -> Option<Claim> {
        self.busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| Claim(self.busy.clone()))
    }

    /// Report an update left by an earlier run or a rollback, once, at start.
    pub fn startup(&self) {
        if let Some(notice) = self.host.take_notice() {
            (self.report)(UpdateEvent::Notice(notice));
        }
        if let Some(version) = self.host.staged() {
            self.status(UpdateStatus::Ready { version });
        }
    }

    /// Check now, and (when `auto`) download what it finds. On a worker thread.
    pub fn check_now(&self) {
        let Some(claim) = self.claim() else { return };
        let this = self.clone();
        thread::spawn(move || {
            let _claim = claim;
            this.check_and_fetch();
        });
    }

    /// Download what the last check offered. On a worker thread.
    pub fn download(&self) {
        let Some(claim) = self.claim() else { return };
        let this = self.clone();
        thread::spawn(move || {
            let _claim = claim;
            this.fetch();
        });
    }

    /// Install and restart. On a worker thread; on success the process exits.
    pub fn restart(&self) {
        let Some(claim) = self.claim() else { return };
        let this = self.clone();
        thread::spawn(move || {
            let _claim = claim;
            this.status(UpdateStatus::Restarting);
            if let Err(e) = this.host.restart() {
                this.status(UpdateStatus::Failed(e));
            }
        });
    }

    /// The background schedule: a first check soon after start, then one every 6 to 12 hours.
    pub fn start_schedule(&self) {
        let this = self.clone();
        thread::spawn(move || {
            thread::sleep(jittered(FIRST_CHECK));
            loop {
                if let Some(_claim) = this.claim() {
                    this.check_and_fetch();
                }
                thread::sleep(jittered(CHECK_EVERY));
            }
        });
    }

    fn check_and_fetch(&self) {
        self.status(UpdateStatus::Checking);
        match self.host.check() {
            Ok(UpdateCheck::UpToDate) => self.status(UpdateStatus::UpToDate),
            Ok(UpdateCheck::Available(info)) => {
                let fetch = self.auto.load(Ordering::SeqCst)
                    && self.host.support() == UpdateSupport::Automatic;
                self.status(UpdateStatus::Available(info));
                if fetch {
                    self.fetch();
                }
            }
            Err(e) => self.status(UpdateStatus::Failed(e)),
        }
    }

    fn fetch(&self) {
        let version = match self.host.download(&mut |received, total| {
            self.status(UpdateStatus::Downloading {
                version: String::new(),
                received,
                total,
            })
        }) {
            Ok(v) => v,
            Err(e) => return self.status(UpdateStatus::Failed(e)),
        };
        self.status(UpdateStatus::Ready { version });
    }
}

/// Holds the work slot; frees it when the work ends, however it ends.
struct Claim(Arc<AtomicBool>);

impl Drop for Claim {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// What the screen knows about updates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateUi {
    pub status: UpdateStatus,
    pub support: UpdateSupport,
    pub version: String,
    /// Download and prepare updates by themselves.
    pub auto: bool,
    /// The settings dialog is open.
    pub settings_open: bool,
    /// A rollback or a node change worth telling the person, in the settings and as a note.
    pub notice: Option<String>,
    /// The Freenet node's version, and whether it changed while River was open.
    pub node_version: Option<String>,
    pub node_note: Option<String>,
    /// The last time a check succeeded, for "Checked 5 minutes ago".
    pub checked: Option<Instant>,
    /// The version a notification was shown for, so each is announced once.
    pub announced: Option<String>,
    /// A downloaded, verified version waiting for a restart. Kept apart from `status`, which
    /// follows the latest check: a later check that finds nothing, fails, or runs offline must
    /// not hide a restart that is ready.
    pub staged: Option<String>,
    /// Set when Restart was pressed with work that a restart would lose: what would be lost,
    /// and the choice of waiting or restarting anyway.
    pub confirm_restart: Option<String>,
    /// Why the last Restart did not happen (the new version did not start and the old one was
    /// put back, or the swap was refused). Shown beside the restart that is still ready.
    pub restart_error: Option<String>,
}

impl Default for UpdateUi {
    fn default() -> Self {
        UpdateUi {
            status: UpdateStatus::Idle,
            support: UpdateSupport::Unavailable("River can't update itself here.".into()),
            version: env!("CARGO_PKG_VERSION").into(),
            auto: true,
            settings_open: false,
            notice: None,
            node_version: None,
            node_note: None,
            checked: None,
            announced: None,
            staged: None,
            confirm_restart: None,
            restart_error: None,
        }
    }
}

impl UpdateUi {
    /// The version the banner is about, if there is something to act on.
    pub fn pending_version(&self) -> Option<&str> {
        match &self.status {
            UpdateStatus::Available(i) => Some(&i.version),
            UpdateStatus::Downloading { version, .. } if !version.is_empty() => Some(version),
            UpdateStatus::Ready { version } => Some(version),
            _ => self.staged.as_deref(),
        }
    }

    /// The version downloaded and waiting for a restart: the one just reported ready, or one
    /// staged earlier that no later check has changed.
    pub fn ready(&self) -> Option<&str> {
        match &self.status {
            UpdateStatus::Ready { version } => Some(version),
            _ => self.staged.as_deref(),
        }
    }

    /// Whether the room list shows the update block.
    pub fn shows_block(&self) -> bool {
        self.ready().is_some()
            || matches!(
                self.status,
                UpdateStatus::Available(_)
                    | UpdateStatus::Downloading { .. }
                    | UpdateStatus::Ready { .. }
                    | UpdateStatus::Restarting
            )
            || self.notice.is_some()
    }

    /// Take a new status from the controller. Returns the notification to show, if this one
    /// calls for it: a version is announced once, when it is ready (or available, with
    /// automatic off).
    pub fn apply(&mut self, status: UpdateStatus) -> Option<(String, String)> {
        // Progress carries no version; keep the one the last status named.
        let status = match status {
            UpdateStatus::Downloading {
                received, total, ..
            } => UpdateStatus::Downloading {
                version: self.pending_version().unwrap_or_default().to_string(),
                received,
                total,
            },
            other => other,
        };
        if matches!(status, UpdateStatus::UpToDate | UpdateStatus::Available(_)) {
            self.checked = Some(Instant::now());
        }
        if let UpdateStatus::Ready { version } = &status {
            self.staged = Some(version.clone());
            self.restart_error = None;
        }
        if let (UpdateStatus::Restarting, UpdateStatus::Failed(e)) = (&self.status, &status) {
            self.restart_error = Some(e.clone());
            self.confirm_restart = None;
        }
        // A new download replaces the idea of an older staged one only once it is ready.
        self.status = status;
        let (version, body) = match &self.status {
            UpdateStatus::Ready { version } => (
                version.clone(),
                format!("River {version} is ready. Restart River to use it."),
            ),
            UpdateStatus::Available(i)
                if !self.auto || self.support != UpdateSupport::Automatic =>
            {
                (
                    i.version.clone(),
                    format!("River {} is available.", i.version),
                )
            }
            _ => return None,
        };
        if self.announced.as_deref() == Some(version.as_str()) {
            return None;
        }
        self.announced = Some(version);
        Some(("River update".to_string(), body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::{Notification, Notifier};
    use crate::{RiverApp, Setup, WINDOW_SIZE, view};
    use rune::{Env, Runtime};
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::{Receiver, channel};

    /// A host whose answers are scripted.
    struct FakeHost {
        support: UpdateSupport,
        checks: Mutex<VecDeque<Result<UpdateCheck, String>>>,
        download: Mutex<Result<String, String>>,
        restart: Mutex<Result<(), String>>,
        staged: Option<String>,
        notice: Mutex<Option<String>>,
        downloads: AtomicUsize,
        restarts: AtomicUsize,
        check_calls: AtomicUsize,
        confirms: AtomicUsize,
    }

    impl FakeHost {
        fn new(support: UpdateSupport, checks: Vec<Result<UpdateCheck, String>>) -> FakeHost {
            FakeHost {
                support,
                checks: Mutex::new(checks.into()),
                download: Mutex::new(Ok("0.2.0".into())),
                restart: Mutex::new(Ok(())),
                staged: None,
                notice: Mutex::new(None),
                downloads: AtomicUsize::new(0),
                restarts: AtomicUsize::new(0),
                check_calls: AtomicUsize::new(0),
                confirms: AtomicUsize::new(0),
            }
        }
    }

    impl UpdateHost for FakeHost {
        fn version(&self) -> String {
            "0.1.0".into()
        }
        fn support(&self) -> UpdateSupport {
            self.support.clone()
        }
        fn staged(&self) -> Option<String> {
            self.staged.clone()
        }
        fn take_notice(&self) -> Option<String> {
            self.notice.lock().unwrap().take()
        }
        fn confirm_started(&self) {
            self.confirms.fetch_add(1, Ordering::SeqCst);
        }
        fn check(&self) -> Result<UpdateCheck, String> {
            self.check_calls.fetch_add(1, Ordering::SeqCst);
            self.checks
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(UpdateCheck::UpToDate))
        }
        fn download(&self, progress: &mut dyn FnMut(u64, u64)) -> Result<String, String> {
            self.downloads.fetch_add(1, Ordering::SeqCst);
            progress(5, 10);
            self.download.lock().unwrap().clone()
        }
        fn restart(&self) -> Result<(), String> {
            self.restarts.fetch_add(1, Ordering::SeqCst);
            self.restart.lock().unwrap().clone()
        }
    }

    fn info() -> UpdateInfo {
        UpdateInfo {
            version: "0.2.0".into(),
            notes: String::new(),
            size: 10,
            required: false,
        }
    }

    fn available() -> Result<UpdateCheck, String> {
        Ok(UpdateCheck::Available(info()))
    }

    fn controller(host: Arc<FakeHost>, auto: bool) -> (UpdateController, Receiver<UpdateEvent>) {
        let (tx, rx) = channel();
        let tx = Mutex::new(tx);
        (
            UpdateController::new(host, auto, move |e| drop(tx.lock().unwrap().send(e))),
            rx,
        )
    }

    /// Everything reported until the controller is idle again.
    fn settle(ctl: &UpdateController, rx: &Receiver<UpdateEvent>) -> Vec<UpdateStatus> {
        let mut seen = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let idle = !ctl.busy.load(Ordering::SeqCst);
            while let Ok(UpdateEvent::Status(s)) = rx.try_recv() {
                seen.push(s);
            }
            if idle {
                return seen;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the controller never went idle: {seen:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn kinds(seen: &[UpdateStatus]) -> Vec<&'static str> {
        seen.iter()
            .map(|s| match s {
                UpdateStatus::Idle => "idle",
                UpdateStatus::Checking => "checking",
                UpdateStatus::UpToDate => "up-to-date",
                UpdateStatus::Available(_) => "available",
                UpdateStatus::Downloading { .. } => "downloading",
                UpdateStatus::Ready { .. } => "ready",
                UpdateStatus::Restarting => "restarting",
                UpdateStatus::Failed(_) => "failed",
            })
            .collect()
    }

    #[test]
    fn automatic_updates_download_and_then_wait_for_the_press() {
        let host = Arc::new(FakeHost::new(UpdateSupport::Automatic, vec![available()]));
        let (ctl, rx) = controller(host.clone(), true);
        ctl.check_now();
        let seen = settle(&ctl, &rx);
        assert_eq!(
            kinds(&seen),
            ["checking", "available", "downloading", "ready"]
        );
        assert_eq!(
            seen.last(),
            Some(&UpdateStatus::Ready {
                version: "0.2.0".into()
            })
        );
        assert_eq!(
            host.restarts.load(Ordering::SeqCst),
            0,
            "never restarts by itself"
        );
    }

    #[test]
    fn with_automatic_off_it_only_says_so() {
        let host = Arc::new(FakeHost::new(UpdateSupport::Automatic, vec![available()]));
        let (ctl, rx) = controller(host.clone(), false);
        ctl.check_now();
        assert_eq!(kinds(&settle(&ctl, &rx)), ["checking", "available"]);
        assert_eq!(host.downloads.load(Ordering::SeqCst), 0);
        // Pressing Download does it.
        ctl.download();
        assert_eq!(kinds(&settle(&ctl, &rx)), ["downloading", "ready"]);
        // Turning it on later is honoured by the next check.
        host.checks.lock().unwrap().push_back(available());
        ctl.set_auto(true);
        ctl.check_now();
        assert_eq!(*kinds(&settle(&ctl, &rx)).last().unwrap(), "ready");
    }

    #[test]
    fn a_copy_updated_by_something_else_is_never_downloaded() {
        for support in [
            UpdateSupport::External("a package manager".into()),
            UpdateSupport::Unavailable("not yet".into()),
        ] {
            let host = Arc::new(FakeHost::new(support, vec![available()]));
            let (ctl, rx) = controller(host.clone(), true);
            ctl.check_now();
            assert_eq!(kinds(&settle(&ctl, &rx)), ["checking", "available"]);
            assert_eq!(host.downloads.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn failures_are_reported_and_the_controller_stays_usable() {
        let host = Arc::new(FakeHost::new(
            UpdateSupport::Automatic,
            vec![
                Err("the update manifest's signature is not valid".into()),
                available(),
            ],
        ));
        *host.download.lock().unwrap() = Err("the download does not match".into());
        let (ctl, rx) = controller(host.clone(), true);
        ctl.check_now();
        let seen = settle(&ctl, &rx);
        assert_eq!(
            seen.last(),
            Some(&UpdateStatus::Failed(
                "the update manifest's signature is not valid".into()
            ))
        );
        ctl.check_now();
        let seen = settle(&ctl, &rx);
        assert_eq!(
            *kinds(&seen).last().unwrap(),
            "failed",
            "a failed download is reported"
        );
        *host.restart.lock().unwrap() = Err("River went back to 0.1.0".into());
        ctl.restart();
        let seen = settle(&ctl, &rx);
        assert_eq!(kinds(&seen), ["restarting", "failed"]);
    }

    #[test]
    fn work_in_progress_is_not_started_twice() {
        let host = Arc::new(FakeHost::new(UpdateSupport::Automatic, vec![]));
        let (ctl, _rx) = controller(host.clone(), false);
        let held = ctl.claim().unwrap();
        ctl.check_now();
        ctl.download();
        ctl.restart();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(host.check_calls.load(Ordering::SeqCst), 0);
        assert_eq!(host.restarts.load(Ordering::SeqCst), 0);
        drop(held);
        assert!(ctl.claim().is_some(), "the slot is free again");
    }

    #[test]
    fn startup_reports_a_staged_update_and_a_rollback_notice() {
        let mut host = FakeHost::new(UpdateSupport::Automatic, vec![]);
        host.staged = Some("0.2.0".into());
        *host.notice.lock().unwrap() = Some("went back".into());
        let (ctl, rx) = controller(Arc::new(host), true);
        ctl.startup();
        assert_eq!(
            rx.try_recv().unwrap(),
            UpdateEvent::Notice("went back".into())
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            UpdateEvent::Status(UpdateStatus::Ready {
                version: "0.2.0".into()
            })
        );
    }

    #[test]
    fn the_schedule_spreads_checks_between_6_and_12_hours() {
        for _ in 0..200 {
            let d = jittered(CHECK_EVERY);
            assert!(d >= CHECK_EVERY.0 && d <= CHECK_EVERY.1, "{d:?}");
        }
        let first = jittered(FIRST_CHECK);
        assert!(first >= FIRST_CHECK.0 && first <= FIRST_CHECK.1);
    }

    #[derive(Clone, Default)]
    struct Recorder(Rc<RefCell<Vec<Notification>>>);

    impl Notifier for Recorder {
        fn notify(&self, n: Notification) -> bool {
            self.0.borrow_mut().push(n);
            true
        }
        fn set_unread(&self, _: usize) {}
    }

    #[test]
    fn each_version_is_announced_once_and_only_when_the_window_is_away() {
        let rec = Recorder::default();
        let mut s = RiverApp::detached().with_notifier(Box::new(rec.clone()));
        let ready = || {
            UpdateEvent::Status(UpdateStatus::Ready {
                version: "0.2.0".into(),
            })
        };
        // Focused: the block is on screen, so no notification, and that counts as told.
        s.focus_changed(true);
        s.apply_update(ready());
        s.focus_changed(false);
        s.apply_update(ready());
        assert!(rec.0.borrow().is_empty());
        // A newer one, with the window away, is announced once.
        let newer = || {
            UpdateEvent::Status(UpdateStatus::Ready {
                version: "0.3.0".into(),
            })
        };
        s.apply_update(newer());
        s.apply_update(newer());
        assert_eq!(rec.0.borrow().len(), 1);
        assert_eq!(
            rec.0.borrow()[0].body,
            "River 0.3.0 is ready. Restart River to use it."
        );
        // Available, with automatic on and supported, waits for the download instead.
        s.chat.update.support = UpdateSupport::Automatic;
        s.chat.update.auto = true;
        s.apply_update(UpdateEvent::Status(UpdateStatus::Available(UpdateInfo {
            version: "0.4.0".into(),
            ..info()
        })));
        assert_eq!(rec.0.borrow().len(), 1);
        // With automatic off it is announced as available.
        s.chat.update.auto = false;
        s.apply_update(UpdateEvent::Status(UpdateStatus::Available(UpdateInfo {
            version: "0.5.0".into(),
            ..info()
        })));
        assert_eq!(rec.0.borrow().len(), 2);
        assert_eq!(rec.0.borrow()[1].body, "River 0.5.0 is available.");
    }

    #[test]
    fn an_update_notice_held_back_by_do_not_disturb_is_shown_once_after_it() {
        let rec = Recorder::default();
        let mut s = RiverApp::detached().with_notifier(Box::new(rec.clone()));
        s.chat.dnd.pinned = true;
        s.chat.dnd.settings.manual = true;
        s.apply_update(UpdateEvent::Status(UpdateStatus::Ready {
            version: "0.2.0".into(),
        }));
        s.flush_held(std::time::Instant::now());
        assert!(rec.0.borrow().is_empty());
        s.chat.dnd.settings.manual = false;
        s.flush_held(std::time::Instant::now());
        assert_eq!(rec.0.borrow().len(), 1);
        s.flush_held(std::time::Instant::now());
        s.apply_update(UpdateEvent::Status(UpdateStatus::Ready {
            version: "0.2.0".into(),
        }));
        assert_eq!(rec.0.borrow().len(), 1, "announced once");
    }

    // The screen ---------------------------------------------------------------------------

    fn connected() -> RiverApp {
        let mut s = RiverApp::detached();
        s.setup = Setup::Connected {
            version: "0.2.160".into(),
        };
        s.chat.update.support = UpdateSupport::Automatic;
        s.chat.update.version = "0.1.0".into();
        s
    }

    fn render(state: RiverApp) -> Runtime<RiverApp> {
        let mut rt = Runtime::new(
            state,
            view,
            WINDOW_SIZE,
            Env {
                system_dark: true,
                scale: 1.0,
            },
        );
        rt.frame(0.0);
        rt.frame(1.0 / 60.0);
        rt
    }

    fn press(rt: &mut Runtime<RiverApp>, key: &str) {
        let (x, y) = rt.find(key).unwrap_or_else(|| panic!("no element {key}"));
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        rt.frame(0.1);
        rt.frame(0.2);
    }

    fn shows(rt: &Runtime<RiverApp>, needle: &str) -> bool {
        rt.elements()
            .into_iter()
            .filter_map(|e| e.text)
            .any(|t| t.contains(needle))
    }

    #[test]
    fn nothing_shows_in_the_room_list_when_there_is_nothing_to_do() {
        for status in [
            UpdateStatus::Idle,
            UpdateStatus::UpToDate,
            UpdateStatus::Checking,
            UpdateStatus::Failed("x".into()),
        ] {
            let mut s = connected();
            s.chat.update.status = status.clone();
            let rt = render(s);
            assert!(rt.find("update-block").is_none(), "{status:?}");
            assert!(
                rt.find("settings").is_some(),
                "the settings button is always there"
            );
        }
    }

    #[test]
    fn a_ready_update_offers_restart_and_pressing_it_restarts() {
        let host = Arc::new(FakeHost::new(UpdateSupport::Automatic, vec![]));
        let (ctl, rx) = controller(host.clone(), true);
        let mut s = connected();
        s.updates = Some(ctl.clone());
        s.chat.update.status = UpdateStatus::Ready {
            version: "0.2.0".into(),
        };
        let mut rt = render(s);
        assert!(shows(&rt, "River 0.2.0 is ready"));
        press(&mut rt, "update-restart");
        let seen = settle(&ctl, &rx);
        assert_eq!(kinds(&seen), ["restarting"]);
        assert_eq!(host.restarts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_available_update_offers_a_download_when_river_may_replace_itself() {
        let host = Arc::new(FakeHost::new(UpdateSupport::Automatic, vec![]));
        let (ctl, rx) = controller(host.clone(), false);
        let mut s = connected();
        s.updates = Some(ctl.clone());
        s.chat.update.auto = false;
        s.chat.update.status = UpdateStatus::Available(info());
        let mut rt = render(s);
        assert!(shows(&rt, "River 0.2.0 is available"));
        press(&mut rt, "update-download");
        assert_eq!(kinds(&settle(&ctl, &rx)), ["downloading", "ready"]);

        // A copy a package manager owns: announced, with who updates it, and no button.
        let mut s = connected();
        s.chat.update.support = UpdateSupport::External("Your package manager updates it.".into());
        s.chat.update.status = UpdateStatus::Available(info());
        let rt = render(s);
        assert!(shows(&rt, "Your package manager updates it."));
        assert!(rt.find("update-download").is_none());
        assert!(rt.find("update-restart").is_none());
    }

    #[test]
    fn a_required_update_says_so_but_is_still_a_button() {
        let mut s = connected();
        s.chat.update.status = UpdateStatus::Available(UpdateInfo {
            required: true,
            ..info()
        });
        let rt = render(s);
        assert!(shows(&rt, "no longer supported"));
        assert!(rt.find("update-download").is_some());
    }

    #[test]
    fn a_rollback_is_explained_and_can_be_dismissed() {
        let mut s = connected();
        s.chat.update.notice =
            Some("River 0.2.0 did not start, so River went back to 0.1.0.".into());
        let mut rt = render(s);
        assert!(shows(&rt, "went back to 0.1.0"));
        press(&mut rt, "update-notice-dismiss");
        assert!(rt.find("update-block").is_none());
    }

    #[test]
    fn the_settings_show_versions_and_the_toggle_persists() {
        let host = Arc::new(FakeHost::new(UpdateSupport::Automatic, vec![]));
        let (ctl, rx) = controller(host.clone(), true);
        let mut s = connected();
        s.updates = Some(ctl.clone());
        s.chat.update.node_version = Some("0.2.160".into());
        s.chat.update.status = UpdateStatus::UpToDate;
        let mut rt = render(s);
        assert!(rt.find("settings-body").is_none());
        press(&mut rt, "settings");
        assert!(shows(&rt, "River 0.1.0"));
        assert!(shows(&rt, "Freenet 0.2.160"));
        assert!(shows(&rt, "River is up to date"));
        assert!(shows(&rt, "Freenet updates itself"));
        assert!(rt.state.chat.update.auto, "on by default");

        press(&mut rt, "settings-auto");
        assert!(!rt.state.chat.update.auto);
        assert_eq!(
            rt.state.update_prefs.get::<bool>(crate::UPDATE_AUTO_PREF),
            Some(false)
        );
        press(&mut rt, "settings-auto");
        assert!(rt.state.chat.update.auto);
        assert_eq!(
            rt.state.update_prefs.get::<bool>(crate::UPDATE_AUTO_PREF),
            Some(true)
        );
        // Turning it on checks at once.
        let _ = settle(&ctl, &rx);
        assert!(host.check_calls.load(Ordering::SeqCst) >= 1);

        let before = host.check_calls.load(Ordering::SeqCst);
        press(&mut rt, "settings-check-now");
        let _ = settle(&ctl, &rx);
        assert!(host.check_calls.load(Ordering::SeqCst) > before);
    }

    #[test]
    fn the_settings_of_a_managed_copy_explain_and_disable_the_toggle() {
        let mut s = connected();
        s.chat.update.support = UpdateSupport::External(
            "River is installed with Flatpak. Update it with `flatpak update`.".into(),
        );
        s.chat.update.settings_open = true;
        let mut rt = render(s);
        assert!(shows(&rt, "flatpak update"));
        let before = rt.state.chat.update.auto;
        press(&mut rt, "settings-auto");
        assert_eq!(
            rt.state.chat.update.auto, before,
            "a disabled toggle does nothing"
        );
        assert!(rt.find("settings-restart").is_none());
    }

    #[test]
    fn the_settings_explain_an_update_failure_and_a_node_that_updated() {
        let mut s = connected();
        s.chat.update.settings_open = true;
        s.chat.update.status = UpdateStatus::Failed("the update manifest has expired".into());
        s.chat.update.node_note =
            Some("Freenet updated itself from 0.2.158 to 0.2.160 while River was open.".into());
        let rt = render(s);
        assert!(shows(
            &rt,
            "Couldn't update: the update manifest has expired"
        ));
        assert!(shows(&rt, "Freenet updated itself from 0.2.158 to 0.2.160"));
    }

    #[test]
    fn a_node_that_updates_itself_is_noted_without_restarting_the_rooms() {
        let mut s = RiverApp::detached();
        s.set_setup(Setup::Connected {
            version: "0.2.158".into(),
        });
        assert_eq!(s.chat.update.node_version.as_deref(), Some("0.2.158"));
        assert_eq!(s.chat.update.node_note, None);
        s.set_setup(Setup::Connected {
            version: "0.2.158".into(),
        });
        assert_eq!(s.chat.update.node_note, None);
        s.set_setup(Setup::Connected {
            version: "0.2.160".into(),
        });
        assert_eq!(s.chat.update.node_version.as_deref(), Some("0.2.160"));
        assert!(
            s.chat
                .update
                .node_note
                .as_deref()
                .unwrap()
                .contains("0.2.158 to 0.2.160")
        );
    }

    #[test]
    fn a_ready_update_stays_visible_whatever_a_later_check_says() {
        for later in [
            UpdateStatus::UpToDate,
            UpdateStatus::Checking,
            UpdateStatus::Failed("couldn't reach the server".into()),
            UpdateStatus::Idle,
        ] {
            let mut s = connected();
            let _ = s.chat.update.apply(UpdateStatus::Ready {
                version: "0.2.0".into(),
            });
            let _ = s.chat.update.apply(later.clone());
            let rt = render(s);
            assert!(rt.find("update-restart").is_some(), "{later:?}");
            assert!(shows(&rt, "River 0.2.0 is ready"), "{later:?}");
        }
    }

    #[test]
    fn a_failed_restart_is_explained_beside_the_restart_that_is_still_ready() {
        let mut s = connected();
        let _ = s.chat.update.apply(UpdateStatus::Ready {
            version: "0.2.0".into(),
        });
        let _ = s.chat.update.apply(UpdateStatus::Restarting);
        let _ = s.chat.update.apply(UpdateStatus::Failed(
            "River 0.2.0 did not start, so River went back to 0.1.0.".into(),
        ));
        let rt = render(s);
        assert!(shows(&rt, "went back to 0.1.0"));
        assert!(rt.find("update-restart").is_some());
    }

    fn ready_app(host: Arc<FakeHost>) -> (RiverApp, Receiver<UpdateEvent>, UpdateController) {
        let (ctl, rx) = controller(host, true);
        let mut s = connected();
        s.updates = Some(ctl.clone());
        let _ = s.chat.update.apply(UpdateStatus::Ready {
            version: "0.2.0".into(),
        });
        (s, rx, ctl)
    }

    #[test]
    fn restart_asks_first_when_text_would_be_lost_and_waits_by_default() {
        let host = Arc::new(FakeHost::new(UpdateSupport::Automatic, vec![]));
        let (mut s, rx, ctl) = ready_app(host.clone());
        s.chat.composer = rune::TextEdit::new("half a thought");
        let mut rt = render(s);
        press(&mut rt, "update-restart");
        assert!(shows(
            &rt,
            "You have text you have not sent. Restart anyway?"
        ));
        assert!(rt.find("update-wait").is_some() && rt.find("update-restart-anyway").is_some());
        assert_eq!(host.restarts.load(Ordering::SeqCst), 0);
        // Waiting puts the plain button back and still does not restart.
        press(&mut rt, "update-wait");
        assert!(rt.find("update-restart").is_some());
        assert_eq!(host.restarts.load(Ordering::SeqCst), 0);
        // Asked again and answered Restart anyway: it restarts.
        press(&mut rt, "update-restart");
        press(&mut rt, "update-restart-anyway");
        let _ = settle(&ctl, &rx);
        assert_eq!(host.restarts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn restart_asks_first_while_messages_are_still_being_sent() {
        let host = Arc::new(FakeHost::new(UpdateSupport::Automatic, vec![]));
        let (mut s, _rx, _ctl) = ready_app(host.clone());
        let mut pending = crate::chat::model::Pending {
            entry: 1,
            failed: None,
            retry: false,
        };
        let room = s.chat.rooms()[0].key.clone();
        s.chat
            .mark_first_message_pending_for_test(&room, pending.clone());
        assert_eq!(
            s.chat.unsent_summary().as_deref(),
            Some("1 message or change is still being sent.")
        );
        // A failed one is lost on restart too.
        pending.failed = Some("refused".into());
        s.chat.mark_first_message_pending_for_test(&room, pending);
        assert!(s.chat.unsent_summary().is_some());
        s.restart_to_update();
        assert!(s.chat.update.confirm_restart.is_some());
        assert_eq!(host.restarts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn text_typed_into_an_open_dialog_counts_as_unsent() {
        let mut s = connected();
        assert_eq!(s.chat.unsent_summary(), None);
        s.chat.form.open = Some(crate::chat::DialogKind::Join);
        assert_eq!(
            s.chat.unsent_summary(),
            None,
            "an empty open dialog loses nothing"
        );
        s.chat.form.code = rune::TextEdit::new("river:invite:abc");
        assert_eq!(
            s.chat.unsent_summary().as_deref(),
            Some("You have text you have not sent.")
        );
        s.chat.form.open = None;
        assert_eq!(s.chat.unsent_summary(), None);
    }

    #[test]
    fn restart_does_not_ask_when_nothing_would_be_lost() {
        let s = connected();
        assert_eq!(s.chat.unsent_summary(), None);
    }

    #[test]
    fn the_first_connection_to_the_node_confirms_the_new_version_once() {
        let host = Arc::new(FakeHost::new(UpdateSupport::Automatic, vec![]));
        let (ctl, _rx) = controller(host.clone(), true);
        let mut s = RiverApp::detached();
        s.updates = Some(ctl);
        s.set_setup(Setup::Checking);
        assert_eq!(host.confirms.load(Ordering::SeqCst), 0);
        s.set_setup(Setup::Connected {
            version: "0.2.160".into(),
        });
        assert_eq!(host.confirms.load(Ordering::SeqCst), 1);
        // The node updating itself is not a first connection.
        s.set_setup(Setup::Connected {
            version: "0.2.161".into(),
        });
        assert_eq!(host.confirms.load(Ordering::SeqCst), 1);
    }
}
