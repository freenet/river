//! Getting from "app launched" to "connected to a running Freenet node".
//!
//! Each step runs on a worker thread and lands its result through the app's [`AppHandle`], so
//! the window never blocks on the network or an installer.

use std::sync::Arc;
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
    Connected { version: String },
}

/// Starts setup work on worker threads and reports back to the window.
#[derive(Clone)]
pub struct Controller {
    host: Arc<dyn NodeHost>,
    report: Arc<dyn Fn(Setup) + Send + Sync>,
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
        }
    }

    pub fn can_install(&self) -> bool {
        self.host.can_install()
    }

    /// Find out whether a node is running, starting an installed one if it isn't.
    pub fn check(&self) {
        let ctl = self.clone();
        ctl.set(Setup::Checking);
        thread::spawn(move || match ctl.host.probe() {
            NodeProbe::Running { version } => ctl.set(Setup::Connected { version }),
            NodeProbe::NotInstalled => ctl.set(Setup::NotInstalled),
            NodeProbe::InstalledNotRunning => ctl.start_and_wait(),
        });
    }

    /// Install Freenet, then start it if the installer didn't.
    pub fn install(&self) {
        let ctl = self.clone();
        ctl.set(Setup::Installing(InstallStep::FindingRelease));
        thread::spawn(move || {
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

    fn start_and_wait(&self) {
        self.set(Setup::Starting);
        if let Err(e) = self.host.start() {
            return self.set(Setup::Failed(format!("Couldn't start Freenet: {e}")));
        }
        let deadline = Instant::now() + START_TIMEOUT;
        while Instant::now() < deadline {
            if let NodeProbe::Running { version } = self.host.probe() {
                return self.set(Setup::Connected { version });
            }
            thread::sleep(START_POLL);
        }
        self.set(Setup::Failed(format!(
            "Freenet was started but didn't answer within {} seconds.",
            START_TIMEOUT.as_secs()
        )));
    }

    fn set(&self, setup: Setup) {
        (self.report)(setup);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    /// A host whose probes answer from a script; the last answer repeats.
    struct FakeHost {
        probes: Mutex<VecDeque<NodeProbe>>,
        install: Result<(), String>,
        start: Result<(), String>,
        starts: AtomicUsize,
    }

    impl FakeHost {
        fn new(probes: Vec<NodeProbe>) -> Self {
            FakeHost {
                probes: Mutex::new(probes.into()),
                install: Ok(()),
                start: Ok(()),
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

    /// Run `act` and collect every reported state until one that ends setup.
    fn drive(host: FakeHost, act: impl FnOnce(&Controller)) -> (Vec<Setup>, Arc<FakeHost>) {
        let host = Arc::new(host);
        let (tx, rx) = mpsc::channel();
        let ctl = Controller::reporting_to(host.clone(), move |s| tx.send(s).unwrap());
        act(&ctl);
        let mut seen = Vec::new();
        loop {
            let s = rx
                .recv_timeout(Duration::from_secs(10))
                .expect("setup should finish");
            let done = matches!(
                s,
                Setup::Connected { .. } | Setup::NotInstalled | Setup::Failed(_)
            );
            seen.push(s);
            if done {
                return (seen, host);
            }
        }
    }

    #[test]
    fn a_running_node_connects_without_starting_anything() {
        let (seen, host) = drive(FakeHost::new(vec![running()]), Controller::check);
        assert_eq!(
            seen,
            vec![
                Setup::Checking,
                Setup::Connected {
                    version: "0.2.155".into()
                }
            ]
        );
        assert_eq!(host.starts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_missing_node_asks_before_installing() {
        let (seen, _) = drive(
            FakeHost::new(vec![NodeProbe::NotInstalled]),
            Controller::check,
        );
        assert_eq!(seen, vec![Setup::Checking, Setup::NotInstalled]);
    }

    #[test]
    fn an_installed_but_stopped_node_is_started_and_waited_for() {
        let probes = vec![
            NodeProbe::InstalledNotRunning,
            NodeProbe::InstalledNotRunning,
            running(),
        ];
        let (seen, host) = drive(FakeHost::new(probes), Controller::check);
        assert_eq!(
            seen,
            vec![
                Setup::Checking,
                Setup::Starting,
                Setup::Connected {
                    version: "0.2.155".into()
                }
            ]
        );
        assert_eq!(host.starts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn install_then_start() {
        let probes = vec![NodeProbe::InstalledNotRunning, running()];
        let (seen, host) = drive(FakeHost::new(probes), Controller::install);
        assert_eq!(
            seen.first(),
            Some(&Setup::Installing(InstallStep::FindingRelease))
        );
        assert!(seen.contains(&Setup::Installing(InstallStep::Downloading {
            received: 1,
            total: Some(2)
        })));
        assert_eq!(
            &seen[seen.len() - 2..],
            &[
                Setup::Starting,
                Setup::Connected {
                    version: "0.2.155".into()
                }
            ]
        );
        assert_eq!(host.starts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_installer_that_starts_the_node_itself_is_not_started_twice() {
        let (seen, host) = drive(FakeHost::new(vec![running()]), Controller::install);
        assert_eq!(
            seen.last(),
            Some(&Setup::Connected {
                version: "0.2.155".into()
            })
        );
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
}
