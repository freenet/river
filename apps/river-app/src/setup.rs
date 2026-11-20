//! Getting from "app launched" to "connected to a running Freenet node".
//!
//! Each step runs on a worker thread and lands its result through the app's [`AppHandle`], so
//! the window never blocks on the network or an installer.

use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use rune::AppHandle;

use crate::RiverApp;
use crate::address::{NodeChoice, NodeStore};
use crate::chat::node_status::{Reach, StatusEvent};
use crate::node::{InstallStep, NodeHost, NodeProbe};

/// How long a freshly started node may take to answer before we call it a failure.
const START_TIMEOUT: Duration = Duration::from_secs(90);
/// How long new work on this computer's Freenet waits for earlier such work to finish.
const LOCAL_WORK_WAIT: Duration = Duration::from_secs(3);
const START_POLL: Duration = Duration::from_millis(500);
/// How long to look for an install after an installer exits successfully without one.
const POST_INSTALL_GRACE: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Setup {
    Checking,
    NotInstalled,
    Installing(InstallStep),
    Starting,
    Failed(String),
    /// The node's client API answered. That means its HTTP server is up, not that it has
    /// finished joining the network.
    Connected {
        version: String,
    },
}

impl Setup {
    /// Whether setup has stopped: it is done, or waiting for the user to choose what next.
    pub fn is_settled(&self) -> bool {
        matches!(
            self,
            Setup::NotInstalled | Setup::Failed(_) | Setup::Connected { .. }
        )
    }
}

/// Starts setup work on worker threads and reports back to the window.
///
/// At most one piece of work runs at a time. A request made while one is running (a
/// double-click on Install, say) is ignored: two installs would write the same files, and
/// whichever finished last would overwrite the other's result on screen.
#[derive(Clone)]
pub struct Controller {
    host: Arc<dyn NodeHost>,
    report: Arc<dyn Fn(Setup) + Send + Sync>,
    busy: Arc<AtomicBool>,
    start_timeout: Duration,
    /// The node River uses. A node the person already has (see
    /// [`NodeChoice::is_existing_node`]) is only asked its version: nothing is installed,
    /// started or stopped on this computer for it.
    node: Arc<Mutex<NodeChoice>>,
    /// Bumped when the node changes, so work for the previous node cannot report.
    epoch: Arc<AtomicU64>,
    /// The epoch this clone's work belongs to.
    my_epoch: u64,
    /// Held while work on this computer's own Freenet (a check that may start it, an install)
    /// runs, however long after the person has moved to another node. A second install or start
    /// must not begin beside one nobody is watching any more.
    local_work: Arc<AtomicBool>,
    /// Where the node status dialog's answers go (see [`Controller::probe_status`]). `None` for a
    /// controller with no window to tell.
    status_report: Option<Arc<dyn Fn(StatusEvent) + Send + Sync>>,
}

/// Reports a fallback event if the worker ends (a panic included) before it reports its answer.
struct EndGuard {
    report: Arc<dyn Fn(StatusEvent) + Send + Sync>,
    fallback: Option<StatusEvent>,
}

impl EndGuard {
    fn new(report: Arc<dyn Fn(StatusEvent) + Send + Sync>, fallback: StatusEvent) -> Self {
        EndGuard {
            report,
            fallback: Some(fallback),
        }
    }

    fn finish(&mut self, event: StatusEvent) {
        self.fallback = None;
        (self.report)(event);
    }
}

impl Drop for EndGuard {
    fn drop(&mut self) {
        if let Some(event) = self.fallback.take() {
            (self.report)(event);
        }
    }
}

/// Frees [`Controller::local_work`] when the worker that held it ends.
struct LocalWork(Arc<AtomicBool>);

impl Drop for LocalWork {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Controller {
    pub fn new(host: Arc<dyn NodeHost>, handle: AppHandle<RiverApp>) -> Self {
        let status = handle.clone();
        let mut ctl = Controller::reporting_to(host, move |setup| {
            handle.update(move |s, _| s.set_setup(setup))
        });
        ctl.status_report = Some(Arc::new(move |event| {
            status.update(move |s, _| {
                let current = s.chat.node.choice.address.authority();
                s.chat.status.apply(&current, event, Instant::now());
            })
        }));
        ctl
    }

    /// A controller that hands each new [`Setup`] to `report` (from any thread).
    pub fn reporting_to(
        host: Arc<dyn NodeHost>,
        report: impl Fn(Setup) + Send + Sync + 'static,
    ) -> Self {
        Controller {
            host,
            report: Arc::new(report),
            busy: Arc::new(AtomicBool::new(false)),
            start_timeout: START_TIMEOUT,
            node: Arc::new(Mutex::new(NodeChoice::local())),
            epoch: Arc::new(AtomicU64::new(0)),
            my_epoch: 0,
            local_work: Arc::new(AtomicBool::new(false)),
            status_report: None,
        }
    }

    /// Use `choice` as the node from now on, for a controller that was just made.
    pub fn with_node(self, choice: NodeChoice) -> Self {
        self.set_node(choice);
        self
    }

    /// The node in use.
    pub fn node(&self) -> NodeChoice {
        self.node
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Use another node. Whatever setup work is running for the previous one is abandoned (its
    /// reports are dropped), and the caller checks the new node with [`check`](Self::check).
    pub fn set_node(&self, choice: NodeChoice) {
        *self
            .node
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = choice;
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.busy.store(false, Ordering::Release);
    }

    /// Where River keeps its settings (see [`NodeHost::config_dir`]).
    pub fn config_dir(&self) -> Option<std::path::PathBuf> {
        self.host.config_dir()
    }

    /// Save `choice` as the node to use next time, or forget the saved one when it is the node
    /// on this computer.
    pub fn save_node(&self, choice: &NodeChoice) -> Result<(), String> {
        let Some(dir) = self.config_dir() else {
            return Ok(());
        };
        let store = NodeStore::in_dir(&dir);
        if choice.is_existing_node() {
            store.save(&choice.address)
        } else {
            store.clear();
            Ok(())
        }
    }

    /// Whether this computer's own Freenet can be installed from here: only when River uses
    /// the node on this computer.
    pub fn can_install(&self) -> bool {
        !self.node().is_existing_node() && self.host.can_install()
    }

    /// Find out whether a node is running. An installed node that isn't running is started
    /// without asking: River can't work without it, and starting it is what the user would do.
    pub fn check(&self) {
        let node = self.node();
        if node.is_existing_node() {
            let authority = node.address.authority();
            return self.spawn(Setup::Checking, false, move |ctl| {
                ctl.check_existing(&authority)
            });
        }
        self.spawn(Setup::Checking, true, |ctl| match ctl.host.probe() {
            NodeProbe::Running { version } => ctl.set(Setup::Connected { version }),
            NodeProbe::Outdated { running, minimum } => ctl.set(outdated(running, minimum)),
            NodeProbe::NotInstalled => ctl.set(Setup::NotInstalled),
            NodeProbe::InstalledNotRunning => ctl.start_and_wait(),
            NodeProbe::Unreachable => ctl.set(Setup::NotInstalled),
        });
    }

    /// Ask a node the person already has for its version, once. It is never started, so a node
    /// that doesn't answer is reported, not waited for: the person fixes the address or the
    /// tunnel and tries again.
    fn check_existing(&self, authority: &str) {
        self.set(match self.host.probe_at(authority) {
            NodeProbe::Running { version } => Setup::Connected { version },
            NodeProbe::Outdated {
                running: Some(v),
                minimum,
            } => Setup::Failed(format!(
                "The Freenet node at {authority} is version {v}, and River needs {minimum} or \
                 newer. Update it on the computer it runs on (`freenet update`), then try again."
            )),
            NodeProbe::Outdated {
                running: None,
                minimum,
            } => Setup::Failed(format!(
                "Something answers at {authority}, but it doesn't say which version of Freenet \
                 it is. It may not be Freenet, or it may be a Freenet older than River \
                 supports (River needs {minimum} or newer). Check the address, or update the \
                 node (`freenet update`), then try again."
            )),
            NodeProbe::Unreachable
            | NodeProbe::InstalledNotRunning
            | NodeProbe::NotInstalled => Setup::Failed(format!(
                "Nothing answered at {authority}. Check that the node is running there and that \
                 the address and port are right. If you reach it through an SSH tunnel or a VPN, \
                 check that it is up."
            )),
        });
    }

    /// Every `every`, look at the running node's version and report it when it changes. The
    /// node updates itself (its supervisor restarts it onto the new version), so this is how
    /// the app notices. It never touches the node.
    pub fn watch_version(&self, every: Duration) {
        let this = self.clone();
        thread::spawn(move || {
            let mut last: Option<String> = None;
            loop {
                thread::sleep(every);
                if this.busy.load(Ordering::SeqCst) {
                    continue;
                }
                let node = this.node();
                let probe = if node.is_existing_node() {
                    this.host.probe_at(&node.address.authority())
                } else {
                    this.host.probe()
                };
                if let NodeProbe::Running { version } = probe
                    && last.as_ref() != Some(&version)
                {
                    if last.is_some() {
                        (this.report)(Setup::Connected {
                            version: version.clone(),
                        });
                    }
                    last = Some(version);
                }
            }
        });
    }

    /// Ask the node in use for its version once, for the node status dialog, and report what it
    /// said. Unlike [`check`](Self::check) this never starts anything and does not change
    /// [`Setup`]: a node that does not answer is shown in the dialog, not as a failed setup.
    pub fn probe_status(&self) {
        let Some(report) = self.status_report.clone() else {
            return;
        };
        let fallback = report.clone();
        let host = self.host.clone();
        let node = self.node();
        let started = thread::Builder::new().spawn(move || {
            // If the probe panics, the dialog still hears that the question ended.
            let mut guard = EndGuard::new(report, StatusEvent::Aborted);
            let authority = node.address.authority();
            let probe = if node.is_existing_node() {
                host.probe_at(&authority)
            } else {
                host.probe()
            };
            let reach = match probe {
                NodeProbe::Running { version } => Reach::Answered { version },
                NodeProbe::Outdated { running, .. } => Reach::TooOld { running },
                NodeProbe::Unreachable
                | NodeProbe::InstalledNotRunning
                | NodeProbe::NotInstalled => Reach::Silent,
            };
            guard.finish(StatusEvent::Reach { authority, reach });
        });
        if started.is_err() {
            fallback(StatusEvent::Aborted);
        }
    }

    /// Whether this shell looks up the latest Freenet release at all.
    pub fn looks_up_latest(&self) -> bool {
        self.host.looks_up_latest()
    }

    /// Look up the latest Freenet release, for the node status dialog. This contacts github.com,
    /// so it is only called while the dialog is open.
    pub fn fetch_latest(&self) {
        let Some(report) = self.status_report.clone() else {
            return;
        };
        let fallback = report.clone();
        let host = self.host.clone();
        let started = thread::Builder::new().spawn(move || {
            let mut guard = EndGuard::new(report, StatusEvent::Latest(None));
            let latest = host.latest_version();
            guard.finish(StatusEvent::Latest(latest));
        });
        if started.is_err() {
            fallback(StatusEvent::Latest(None));
        }
    }

    /// Install Freenet, then start it if the installer didn't. Does nothing for a node the
    /// person already has: River doesn't put a second Freenet on this computer.
    pub fn install(&self) {
        if self.node().is_existing_node() {
            return;
        }
        self.spawn(
            Setup::Installing(InstallStep::FindingRelease),
            true,
            |ctl| {
                let progress_ctl = ctl.clone();
                let mut progress =
                    move |step: InstallStep| progress_ctl.set(Setup::Installing(step));
                if let Err(e) = ctl.host.install(&mut progress) {
                    return ctl.set(Setup::Failed(format!("Couldn't install Freenet: {e}")));
                }
                match ctl.host.probe() {
                    NodeProbe::Running { version } => ctl.set(Setup::Connected { version }),
                    NodeProbe::Outdated { running, minimum } => ctl.set(outdated(running, minimum)),
                    // The installer exited successfully but River sees no install. Freenet's
                    // Windows wizard installs where River looks and also exits 0 when cancelled,
                    // so this is usually a cancel; look briefly rather than make someone who
                    // cancelled wait out a start. The known exception: when an earlier install's
                    // start-at-login entry is still registered, the wizard skips itself, starts a
                    // node from where it was run, and exits; that node may take longer than the
                    // look to answer, so the message leaves room for it.
                    NodeProbe::NotInstalled => {
                        ctl.set(Setup::Checking);
                        ctl.wait_for_node(
                            "The installer finished, but River can't find Freenet where it looks \
                         and no Freenet has answered. Choose Try again: if Freenet is still \
                         starting River will find it, and if the install was cancelled River \
                         will offer to install it again."
                                .into(),
                            true,
                            POST_INSTALL_GRACE.min(ctl.start_timeout),
                        );
                    }
                    // Right after an installer, a node it started may still be booting, and this
                    // start could launch a second one. That is safe: on Windows and macOS
                    // freenet-core's wrapper takes a single-instance lock (a named mutex,
                    // `WRAPPER_MUTEX_NAME`, on Windows; a lockfile on macOS) and a second wrapper
                    // exits (`run_wrapper` in freenet-core's
                    // crates/core/src/bin/commands/service/{wrapper,single_instance}.rs). On Linux
                    // the start goes through systemd, which won't run a second instance of a unit.
                    NodeProbe::InstalledNotRunning => ctl.start_and_wait(),
                    NodeProbe::Unreachable => ctl.set(Setup::NotInstalled),
                }
            },
        );
    }

    /// Take [`Controller::local_work`], waiting a few seconds for work that is just ending.
    fn hold_local_work(&self) -> Option<LocalWork> {
        let deadline = Instant::now() + LOCAL_WORK_WAIT.min(self.start_timeout);
        loop {
            if !self.local_work.swap(true, Ordering::AcqRel) {
                return Some(LocalWork(self.local_work.clone()));
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Run `work` on a worker thread unless something is already running. Whatever `work`
    /// does, including panicking, setup ends on a settled state and the controller is free.
    ///
    /// `local` says the work may install or start the Freenet on this computer: it takes
    /// [`Controller::local_work`] for as long as it runs, and is refused while earlier local
    /// work that the person has since left behind (by choosing another node) is still going.
    fn spawn(&self, first: Setup, local: bool, work: impl FnOnce(&Controller) + Send + 'static) {
        if self.busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut ctl = self.clone();
        ctl.my_epoch = self.epoch.load(Ordering::Acquire);
        ctl.set(first);
        thread::spawn(move || {
            // Taken here, not above: work that just ended frees it a moment after it reports,
            // which must not turn the person's next press into a refusal.
            let _held = if local {
                match ctl.hold_local_work() {
                    Some(held) => Some(held),
                    None => {
                        return ctl.set(Setup::Failed(
                            "River is still finishing what it was doing with Freenet on this \
                             computer before you chose another node. Wait a moment, then try \
                             again."
                                .into(),
                        ));
                    }
                }
            } else {
                None
            };
            // Every path through `work` ends with a settled report, which frees the controller
            // (see `set`). A panic skips that, so report it here.
            if panic::catch_unwind(AssertUnwindSafe(|| work(&ctl))).is_err() {
                ctl.set(Setup::Failed(
                    "Something unexpected went wrong. Try again.".into(),
                ));
            }
        });
    }

    fn start_and_wait(&self) {
        self.set(Setup::Starting);
        if let Err(e) = self.host.start() {
            return self.set(Setup::Failed(format!("Couldn't start Freenet: {e}")));
        }
        self.wait_for_node(
            format!(
                "Freenet was started but didn't answer within {} seconds.",
                self.start_timeout.as_secs()
            ),
            false,
            self.start_timeout,
        );
    }

    /// Poll until a node answers, or report `give_up` after `timeout`. With
    /// `start_once_found`, an install that becomes visible while waiting is started.
    fn wait_for_node(&self, give_up: String, start_once_found: bool, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.host.probe() {
                NodeProbe::Running { version } => return self.set(Setup::Connected { version }),
                NodeProbe::Outdated { running, minimum } => {
                    return self.set(outdated(running, minimum));
                }
                NodeProbe::InstalledNotRunning if start_once_found => {
                    return self.start_and_wait();
                }
                NodeProbe::InstalledNotRunning
                | NodeProbe::NotInstalled
                | NodeProbe::Unreachable => {}
            }
            thread::sleep(START_POLL.min(self.start_timeout));
        }
        self.set(Setup::Failed(give_up));
    }

    /// Report a new state. A settled state ends the current work, so the controller is freed
    /// BEFORE it is reported: the buttons that state shows must work the moment they appear.
    ///
    /// That leaves a moment where the controller is free but the settled state hasn't reached
    /// the window. A new job can't start in it from the window: the only buttons that start
    /// work are the ones the settled state itself shows, and while it is in flight the window
    /// still shows the previous, unsettled state, which has none.
    fn set(&self, setup: Setup) {
        // Work for a node since replaced says nothing about the one in use, and must not free
        // the controller for work that has started since.
        if self.epoch.load(Ordering::Acquire) != self.my_epoch {
            return;
        }
        if setup.is_settled() {
            self.busy.store(false, Ordering::Release);
        }
        (self.report)(setup);
    }
}

fn outdated(running: Option<String>, minimum: String) -> Setup {
    Setup::Failed(match running {
        Some(v) => format!(
            "The Freenet running on this computer is version {v}, and River needs {minimum} \
             or newer (older versions cannot give River permission to open your rooms). \
             Freenet normally updates itself in the background. To update it now, run \
             `freenet update` (or install the latest from freenet.org), then try again."
        ),
        None => format!(
            "Something is answering on Freenet's port (7509) but doesn't say which version of \
             Freenet it is. If it's an old Freenet, update it (run `freenet update`, or install \
             the latest from freenet.org; River needs {minimum} or newer), then try again."
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::{self, Receiver};

    /// A host whose probes answer from a script; the last answer repeats.
    struct FakeHost {
        probes: Mutex<VecDeque<NodeProbe>>,
        install: Result<(), String>,
        /// When set, `install` blocks until the test sends on the paired sender.
        install_gate: Mutex<Option<Receiver<()>>>,
        install_panics: bool,
        start: Result<(), String>,
        installs: AtomicUsize,
        starts: AtomicUsize,
        /// What `probe_at` answers (the last repeats), and which addresses it was asked.
        remote: Mutex<VecDeque<NodeProbe>>,
        asked: Mutex<Vec<String>>,
        /// Local probes made, which a node somebody else runs must never need.
        local_probes: AtomicUsize,
        /// When set, `probe_at` blocks until the test sends on the paired sender.
        remote_gate: Mutex<Option<Receiver<()>>>,
    }

    impl FakeHost {
        fn new(probes: Vec<NodeProbe>) -> Self {
            FakeHost {
                probes: Mutex::new(probes.into()),
                install: Ok(()),
                install_gate: Mutex::new(None),
                install_panics: false,
                start: Ok(()),
                installs: AtomicUsize::new(0),
                starts: AtomicUsize::new(0),
                remote: Mutex::new(VecDeque::from([NodeProbe::Unreachable])),
                asked: Mutex::new(Vec::new()),
                local_probes: AtomicUsize::new(0),
                remote_gate: Mutex::new(None),
            }
        }
    }

    impl NodeHost for FakeHost {
        fn probe_at(&self, authority: &str) -> NodeProbe {
            self.asked.lock().unwrap().push(authority.to_owned());
            if let Some(gate) = self.remote_gate.lock().unwrap().take() {
                gate.recv().expect("test releases the probe");
            }
            let mut remote = self.remote.lock().unwrap();
            if remote.len() > 1 {
                remote.pop_front().unwrap()
            } else {
                remote[0].clone()
            }
        }
        fn probe(&self) -> NodeProbe {
            self.local_probes.fetch_add(1, Ordering::SeqCst);
            let mut probes = self.probes.lock().unwrap();
            if probes.len() > 1 {
                probes.pop_front().unwrap()
            } else {
                probes[0].clone()
            }
        }
        fn can_install(&self) -> bool {
            true
        }
        fn install(&self, progress: &mut dyn FnMut(InstallStep)) -> Result<(), String> {
            self.installs.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = self.install_gate.lock().unwrap().take() {
                gate.recv().expect("test releases the install");
            }
            assert!(!self.install_panics, "installer bug");
            progress(InstallStep::Downloading {
                received: 1,
                total: Some(2),
            });
            self.install.clone()
        }
        fn start(&self) -> Result<(), String> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            self.start.clone()
        }
    }

    fn running() -> NodeProbe {
        NodeProbe::Running {
            version: "0.2.155".into(),
        }
    }

    fn connected() -> Setup {
        Setup::Connected {
            version: "0.2.155".into(),
        }
    }

    fn controller(host: FakeHost) -> (Controller, Arc<FakeHost>, Receiver<Setup>) {
        let host = Arc::new(host);
        let (tx, rx) = mpsc::channel();
        let ctl = Controller::reporting_to(host.clone(), move |s| {
            let _ = tx.send(s);
        });
        (ctl, host, rx)
    }

    /// Collect reported states up to the first settled one, then check nothing follows it.
    fn settle(rx: &Receiver<Setup>) -> Vec<Setup> {
        let mut seen = Vec::new();
        loop {
            let s = rx
                .recv_timeout(Duration::from_secs(10))
                .expect("setup should settle");
            let settled = s.is_settled();
            seen.push(s);
            if settled {
                let late = rx.recv_timeout(Duration::from_millis(200));
                assert!(late.is_err(), "nothing may follow {seen:?}, got {late:?}");
                return seen;
            }
        }
    }

    /// Run `act` once and collect what it reports.
    fn drive(host: FakeHost, act: impl FnOnce(&Controller)) -> (Vec<Setup>, Arc<FakeHost>) {
        let (ctl, host, rx) = controller(host);
        act(&ctl);
        (settle(&rx), host)
    }

    #[test]
    fn a_running_node_connects_without_starting_anything() {
        let (seen, host) = drive(FakeHost::new(vec![running()]), Controller::check);
        assert_eq!(seen, vec![Setup::Checking, connected()]);
        assert_eq!(host.starts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_missing_node_asks_before_installing() {
        let (seen, host) = drive(
            FakeHost::new(vec![NodeProbe::NotInstalled]),
            Controller::check,
        );
        assert_eq!(seen, vec![Setup::Checking, Setup::NotInstalled]);
        assert_eq!(host.installs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn an_installed_but_stopped_node_is_started_and_waited_for() {
        let probes = vec![
            NodeProbe::InstalledNotRunning,
            NodeProbe::InstalledNotRunning,
            running(),
        ];
        let (seen, host) = drive(FakeHost::new(probes), Controller::check);
        assert_eq!(seen, vec![Setup::Checking, Setup::Starting, connected()]);
        assert_eq!(host.starts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn install_then_start() {
        let probes = vec![NodeProbe::InstalledNotRunning, running()];
        let (seen, host) = drive(FakeHost::new(probes), Controller::install);
        assert_eq!(
            seen,
            vec![
                Setup::Installing(InstallStep::FindingRelease),
                Setup::Installing(InstallStep::Downloading {
                    received: 1,
                    total: Some(2)
                }),
                Setup::Starting,
                connected(),
            ]
        );
        assert_eq!(host.starts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_installer_that_starts_the_node_itself_is_not_started_twice() {
        let (seen, host) = drive(FakeHost::new(vec![running()]), Controller::install);
        assert_eq!(seen.last(), Some(&connected()));
        assert_eq!(host.starts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn failures_are_reported_not_swallowed() {
        let mut host = FakeHost::new(vec![NodeProbe::NotInstalled]);
        host.install = Err("signature mismatch".into());
        let (seen, _) = drive(host, Controller::install);
        assert_eq!(
            seen.last(),
            Some(&Setup::Failed(
                "Couldn't install Freenet: signature mismatch".into()
            ))
        );

        let mut host = FakeHost::new(vec![NodeProbe::InstalledNotRunning]);
        host.start = Err("systemctl failed".into());
        let (seen, _) = drive(host, Controller::check);
        assert_eq!(
            seen.last(),
            Some(&Setup::Failed(
                "Couldn't start Freenet: systemctl failed".into()
            ))
        );

        // Installed, but the node won't start.
        let mut host = FakeHost::new(vec![NodeProbe::InstalledNotRunning]);
        host.start = Err("no user session".into());
        let (seen, _) = drive(host, Controller::install);
        assert_eq!(
            seen.last(),
            Some(&Setup::Failed(
                "Couldn't start Freenet: no user session".into()
            ))
        );

        // The installer exited successfully, but no install can be seen and no node answers:
        // with Freenet's wizard, a cancel. Reported after a short look, not a start timeout.
        let (mut ctl, host, rx) = controller(FakeHost::new(vec![NodeProbe::NotInstalled]));
        ctl.start_timeout = Duration::from_secs(1);
        ctl.install();
        let seen = settle(&rx);
        assert!(
            !seen.contains(&Setup::Starting),
            "nothing was started: {seen:?}"
        );
        assert_eq!(
            seen.last(),
            Some(&Setup::Failed(
                "The installer finished, but River can't find Freenet where it looks and no \
                 Freenet has answered. Choose Try again: if Freenet is still starting River \
                 will find it, and if the install was cancelled River will offer to install \
                 it again."
                    .into()
            ))
        );
        assert_eq!(host.starts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_cancelled_installer_is_reported_well_before_a_start_would_time_out() {
        assert!(POST_INSTALL_GRACE * 4 < START_TIMEOUT);
    }

    #[test]
    fn a_node_an_installer_started_from_somewhere_unexpected_is_waited_for() {
        // The installer exits, River can't see where it installed Freenet, but the node it
        // started answers shortly after: that is a success, not "not installed".
        let probes = vec![NodeProbe::NotInstalled, NodeProbe::NotInstalled, running()];
        let (seen, host) = drive(FakeHost::new(probes), Controller::install);
        assert_eq!(seen.last(), Some(&connected()));
        assert_eq!(host.starts.load(Ordering::SeqCst), 0, "nothing to start");

        // The install becomes visible while River waits, but nothing runs: start it.
        let probes = vec![
            NodeProbe::NotInstalled,
            NodeProbe::NotInstalled,
            NodeProbe::InstalledNotRunning,
            running(),
        ];
        let (seen, host) = drive(FakeHost::new(probes), Controller::install);
        assert_eq!(seen.last(), Some(&connected()));
        assert_eq!(host.starts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_outdated_node_is_reported_not_restarted() {
        let old = NodeProbe::Outdated {
            running: Some("0.2.60".into()),
            minimum: "0.2.155".into(),
        };
        let (seen, host) = drive(FakeHost::new(vec![old]), Controller::check);
        assert_eq!(
            seen.last(),
            Some(&Setup::Failed(
                "The Freenet running on this computer is version 0.2.60, and River needs \
                 0.2.155 or newer (older versions cannot give River permission to open your \
                 rooms). Freenet normally updates itself in the background. To update it now, \
                 run `freenet update` (or install the latest from freenet.org), then try again."
                    .into()
            ))
        );
        assert_eq!(host.starts.load(Ordering::SeqCst), 0);

        // Too old to say its version, found after starting it.
        let unknown = NodeProbe::Outdated {
            running: None,
            minimum: "0.2.155".into(),
        };
        let probes = vec![NodeProbe::InstalledNotRunning, unknown];
        let (seen, _) = drive(FakeHost::new(probes), Controller::check);
        let Some(Setup::Failed(msg)) = seen.last() else {
            panic!("{seen:?}")
        };
        assert!(msg.contains("doesn't say which version"), "{msg}");
        assert!(
            msg.contains("freenet.org") && msg.contains("0.2.155"),
            "{msg}"
        );
    }

    #[test]
    fn a_node_that_never_answers_times_out() {
        let (mut ctl, _, rx) = controller(FakeHost::new(vec![NodeProbe::InstalledNotRunning]));
        ctl.start_timeout = Duration::from_secs(1);
        ctl.check();
        assert_eq!(
            settle(&rx).last(),
            Some(&Setup::Failed(
                "Freenet was started but didn't answer within 1 seconds.".into()
            ))
        );
    }

    #[test]
    fn a_second_press_while_installing_is_ignored() {
        // The install blocks until released, so the second press certainly lands while the
        // first install is still running, however slow the machine.
        let host = FakeHost::new(vec![running()]);
        let (release, gate) = mpsc::channel();
        *host.install_gate.lock().unwrap() = Some(gate);
        let (ctl, host, rx) = controller(host);
        ctl.install();
        ctl.install();
        ctl.check();
        release.send(()).unwrap();
        let seen = settle(&rx);
        assert_eq!(host.installs.load(Ordering::SeqCst), 1);
        assert_eq!(
            seen.first(),
            Some(&Setup::Installing(InstallStep::FindingRelease))
        );
        assert_eq!(seen.iter().filter(|s| **s == Setup::Checking).count(), 0);

        // Once settled, the controller takes requests again.
        ctl.install();
        settle(&rx);
        assert_eq!(host.installs.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_panicking_worker_still_settles_and_frees_the_controller() {
        let mut host = FakeHost::new(vec![NodeProbe::NotInstalled]);
        host.install_panics = true;
        let (ctl, _, rx) = controller(host);
        ctl.install();
        assert_eq!(
            settle(&rx).last(),
            Some(&Setup::Failed(
                "Something unexpected went wrong. Try again.".into()
            ))
        );
        ctl.check();
        assert_eq!(settle(&rx), vec![Setup::Checking, Setup::NotInstalled]);
    }

    // A node somebody else runs.

    fn existing(address: &str) -> NodeChoice {
        NodeChoice::saved(crate::address::NodeAddress::parse(address).unwrap())
    }

    fn drive_existing(
        host: FakeHost,
        address: &str,
        act: impl FnOnce(&Controller),
    ) -> (Vec<Setup>, Arc<FakeHost>) {
        let (ctl, host, rx) = controller(host);
        let ctl = ctl.with_node(existing(address));
        act(&ctl);
        (settle(&rx), host)
    }

    fn failed_text(seen: &[Setup]) -> String {
        match seen.last() {
            Some(Setup::Failed(m)) => m.clone(),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn a_node_the_person_has_is_checked_and_never_installed_or_started() {
        // This computer has no Freenet and nothing on its own port: irrelevant to a remote node.
        let host = FakeHost::new(vec![NodeProbe::NotInstalled]);
        *host.remote.lock().unwrap() = VecDeque::from([running()]);
        let (seen, host) = drive_existing(host, "localhost:40302", Controller::check);
        assert_eq!(seen, vec![Setup::Checking, connected()]);
        assert_eq!(*host.asked.lock().unwrap(), ["localhost:40302"]);
        assert_eq!(host.local_probes.load(Ordering::SeqCst), 0);
        assert_eq!(host.installs.load(Ordering::SeqCst), 0);
        assert_eq!(host.starts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn an_address_that_does_not_answer_is_reported_with_the_address_not_waited_for() {
        let host = FakeHost::new(vec![NodeProbe::InstalledNotRunning]);
        let started = Instant::now();
        let (seen, host) = drive_existing(host, "[::1]:7600", Controller::check);
        let message = failed_text(&seen);
        assert!(message.contains("[::1]:7600"), "{message}");
        assert!(message.contains("tunnel"), "{message}");
        // An installed Freenet here is not started to answer for it.
        assert_eq!(host.starts.load(Ordering::SeqCst), 0);
        assert_eq!(host.local_probes.load(Ordering::SeqCst), 0);
        assert_eq!(
            host.asked.lock().unwrap().len(),
            1,
            "asked once, no polling"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_node_that_is_too_old_or_not_freenet_says_which() {
        let old = FakeHost::new(vec![NodeProbe::NotInstalled]);
        *old.remote.lock().unwrap() = VecDeque::from([NodeProbe::Outdated {
            running: Some("0.2.100".into()),
            minimum: "0.2.155".into(),
        }]);
        let message = failed_text(&drive_existing(old, "example.org:7509", Controller::check).0);
        assert!(message.contains("example.org:7509"), "{message}");
        assert!(
            message.contains("0.2.100") && message.contains("0.2.155"),
            "{message}"
        );

        let other = FakeHost::new(vec![NodeProbe::NotInstalled]);
        *other.remote.lock().unwrap() = VecDeque::from([NodeProbe::Outdated {
            running: None,
            minimum: "0.2.155".into(),
        }]);
        let message = failed_text(&drive_existing(other, "example.org:7509", Controller::check).0);
        assert!(message.contains("may not be Freenet"), "{message}");
    }

    #[test]
    fn installing_is_refused_for_a_node_the_person_has() {
        let (ctl, host, rx) = controller(FakeHost::new(vec![NodeProbe::NotInstalled]));
        let ctl = ctl.with_node(existing("localhost:40302"));
        assert!(!ctl.can_install());
        ctl.install();
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
        assert_eq!(host.installs.load(Ordering::SeqCst), 0);
        assert_eq!(host.local_probes.load(Ordering::SeqCst), 0);
        // And the Freenet on this computer is still offered an install.
        let (ctl, _, _) = controller(FakeHost::new(vec![NodeProbe::NotInstalled]));
        assert!(ctl.can_install());
        assert!(!ctl.node().is_existing_node());
    }

    #[test]
    fn even_the_default_address_chosen_by_the_person_leaves_this_computers_freenet_alone() {
        // A tunnel can be forwarded to 7509 with no local node: the workaround for this.
        let host = FakeHost::new(vec![NodeProbe::InstalledNotRunning]);
        *host.remote.lock().unwrap() = VecDeque::from([running()]);
        let (seen, host) = drive_existing(host, "127.0.0.1:7509", Controller::check);
        assert_eq!(seen, vec![Setup::Checking, connected()]);
        assert_eq!(host.starts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn work_for_a_node_since_replaced_reports_nothing() {
        let host = FakeHost::new(vec![running()]);
        let (release, gate) = mpsc::channel();
        *host.remote_gate.lock().unwrap() = Some(gate);
        *host.remote.lock().unwrap() = VecDeque::from([running()]);
        let (ctl, host, rx) = controller(host);
        let ctl = ctl.with_node(existing("localhost:40302"));
        ctl.check();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Setup::Checking
        );
        // Back to this computer's Freenet while the first probe is still out.
        ctl.set_node(NodeChoice::local());
        ctl.check();
        let seen = settle(&rx);
        assert_eq!(seen, vec![Setup::Checking, connected()]);
        // The first probe answers late: nothing it says reaches the window, and it does not
        // free the controller for the work now running.
        release.send(()).unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
        assert_eq!(host.asked.lock().unwrap().len(), 1);
    }

    #[test]
    fn the_choice_is_saved_and_the_local_node_clears_it() {
        let dir = std::env::temp_dir().join(format!("river-setup-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        struct Dir(std::path::PathBuf, FakeHost);
        impl NodeHost for Dir {
            fn probe(&self) -> NodeProbe {
                self.1.probe()
            }
            fn probe_at(&self, a: &str) -> NodeProbe {
                self.1.probe_at(a)
            }
            fn can_install(&self) -> bool {
                true
            }
            fn install(&self, p: &mut dyn FnMut(InstallStep)) -> Result<(), String> {
                self.1.install(p)
            }
            fn start(&self) -> Result<(), String> {
                self.1.start()
            }
            fn config_dir(&self) -> Option<std::path::PathBuf> {
                Some(self.0.clone())
            }
        }
        let ctl = Controller::reporting_to(
            Arc::new(Dir(dir.clone(), FakeHost::new(vec![running()]))),
            |_| {},
        );
        let choice = existing("node.example:7509");
        ctl.save_node(&choice).unwrap();
        assert_eq!(
            NodeStore::in_dir(&dir).load().as_deref(),
            Some("node.example:7509")
        );
        ctl.save_node(&NodeChoice::local()).unwrap();
        assert_eq!(NodeStore::in_dir(&dir).load(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_install_left_behind_by_a_change_of_node_blocks_a_second_one() {
        // Once released, the install finds the node running and ends at once.
        let host = FakeHost::new(vec![running()]);
        let (release, gate) = mpsc::channel();
        *host.install_gate.lock().unwrap() = Some(gate);
        let (ctl, host, rx) = controller(host);
        ctl.install();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Setup::Installing(InstallStep::FindingRelease)
        );
        let started = Instant::now();
        while host.installs.load(Ordering::SeqCst) == 0 {
            assert!(started.elapsed() < Duration::from_secs(5));
            thread::sleep(Duration::from_millis(5));
        }
        // The person goes to another node and back while the install is still running.
        ctl.set_node(existing("localhost:40302"));
        ctl.set_node(NodeChoice::local());
        ctl.install();
        let seen = settle(&rx);
        match seen.last() {
            Some(Setup::Failed(m)) => assert!(m.contains("still finishing"), "{m}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert_eq!(
            host.installs.load(Ordering::SeqCst),
            1,
            "a second install began"
        );
        // Once the first ends, local work is possible again.
        release.send(()).unwrap();
        thread::sleep(Duration::from_millis(300));
        ctl.check();
        let seen = settle(&rx);
        assert!(!matches!(seen.last(), Some(Setup::Failed(_))), "{seen:?}");
    }
}
