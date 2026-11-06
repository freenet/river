//! Getting from "app launched" to "connected to a running Freenet node".
//!
//! Each step runs on a worker thread and lands its result through the app's [`AppHandle`], so
//! the window never blocks on the network or an installer.

use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use rune::AppHandle;

use crate::RiverApp;
use crate::node::{InstallStep, NodeHost, NodeProbe};

/// How long a freshly started node may take to answer before we call it a failure.
const START_TIMEOUT: Duration = Duration::from_secs(90);
const START_POLL: Duration = Duration::from_millis(500);

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
}

impl Controller {
    pub fn new(host: Arc<dyn NodeHost>, handle: AppHandle<RiverApp>) -> Self {
        Controller::reporting_to(host, move |setup| {
            handle.update(move |s, _| s.setup = setup)
        })
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
        }
    }

    pub fn can_install(&self) -> bool {
        self.host.can_install()
    }

    /// Find out whether a node is running. An installed node that isn't running is started
    /// without asking: River can't work without it, and starting it is what the user would do.
    pub fn check(&self) {
        self.spawn(Setup::Checking, |ctl| match ctl.host.probe() {
            NodeProbe::Running { version } => ctl.set(Setup::Connected { version }),
            NodeProbe::NotInstalled => ctl.set(Setup::NotInstalled),
            NodeProbe::InstalledNotRunning => ctl.start_and_wait(),
        });
    }

    /// Install Freenet, then start it if the installer didn't.
    pub fn install(&self) {
        self.spawn(Setup::Installing(InstallStep::FindingRelease), |ctl| {
            let progress_ctl = ctl.clone();
            let mut progress = move |step: InstallStep| progress_ctl.set(Setup::Installing(step));
            if let Err(e) = ctl.host.install(&mut progress) {
                return ctl.set(Setup::Failed(format!("Couldn't install Freenet: {e}")));
            }
            match ctl.host.probe() {
                NodeProbe::Running { version } => ctl.set(Setup::Connected { version }),
                NodeProbe::NotInstalled => ctl.set(Setup::Failed(
                    "The installer finished, but Freenet still isn't installed.".into(),
                )),
                NodeProbe::InstalledNotRunning => ctl.start_and_wait(),
            }
        });
    }

    /// Run `work` on a worker thread unless something is already running. Whatever `work`
    /// does, including panicking, setup ends on a settled state and the controller is free.
    fn spawn(&self, first: Setup, work: impl FnOnce(&Controller) + Send + 'static) {
        if self.busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let ctl = self.clone();
        ctl.set(first);
        thread::spawn(move || {
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
        let deadline = Instant::now() + self.start_timeout;
        while Instant::now() < deadline {
            if let NodeProbe::Running { version } = self.host.probe() {
                return self.set(Setup::Connected { version });
            }
            thread::sleep(START_POLL.min(self.start_timeout));
        }
        self.set(Setup::Failed(format!(
            "Freenet was started but didn't answer within {} seconds.",
            self.start_timeout.as_secs()
        )));
    }

    /// Report a new state. A settled state ends the current work, so the controller is freed
    /// BEFORE it is reported: the buttons that state shows must work the moment they appear.
    fn set(&self, setup: Setup) {
        if setup.is_settled() {
            self.busy.store(false, Ordering::Release);
        }
        (self.report)(setup);
    }
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
        install_delay: Duration,
        install_panics: bool,
        start: Result<(), String>,
        installs: AtomicUsize,
        starts: AtomicUsize,
    }

    impl FakeHost {
        fn new(probes: Vec<NodeProbe>) -> Self {
            FakeHost {
                probes: Mutex::new(probes.into()),
                install: Ok(()),
                install_delay: Duration::ZERO,
                install_panics: false,
                start: Ok(()),
                installs: AtomicUsize::new(0),
                starts: AtomicUsize::new(0),
            }
        }
    }

    impl NodeHost for FakeHost {
        fn probe(&self) -> NodeProbe {
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
            thread::sleep(self.install_delay);
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

        let (seen, _) = drive(
            FakeHost::new(vec![NodeProbe::NotInstalled]),
            Controller::install,
        );
        assert_eq!(
            seen.last(),
            Some(&Setup::Failed(
                "The installer finished, but Freenet still isn't installed.".into()
            ))
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
        let mut host = FakeHost::new(vec![running()]);
        host.install_delay = Duration::from_millis(300);
        let (ctl, host, rx) = controller(host);
        ctl.install();
        ctl.install();
        ctl.check();
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
}
