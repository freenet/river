//! What River needs from the platform it runs on, to reach a Freenet node.
//!
//! The desktop shell answers these by probing `127.0.0.1:7509` and installing Freenet from the
//! signed GitHub release. A mobile shell would answer them differently (it may connect to a node
//! rather than install one), which is why this is a trait and not desktop code.

/// The state of the local Freenet node, as the platform sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeProbe {
    /// A node answered on its client API.
    Running { version: String },
    /// A node is running, but it is older than River supports. `running` is `None` when it is
    /// too old to report its version at all.
    Outdated {
        running: Option<String>,
        minimum: String,
    },
    /// Freenet is installed but no node answered.
    InstalledNotRunning,
    /// Freenet is not installed.
    NotInstalled,
}

/// One step of installing Freenet, reported as it happens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallStep {
    FindingRelease,
    Downloading {
        received: u64,
        total: Option<u64>,
    },
    Verifying,
    Installing,
    /// Waiting on an installer the platform opened (Windows' setup wizard).
    WaitingForInstaller,
}

pub trait NodeHost: Send + Sync + 'static {
    fn probe(&self) -> NodeProbe;

    /// Whether this platform can install Freenet itself.
    fn can_install(&self) -> bool;

    /// Download, verify and install Freenet, reporting progress. Runs on a worker thread.
    /// On return the node may or may not be running; the caller probes and calls [`start`].
    ///
    /// [`start`]: NodeHost::start
    fn install(&self, progress: &mut dyn FnMut(InstallStep)) -> Result<(), String>;

    /// Start an installed node that is not running. Runs on a worker thread.
    fn start(&self) -> Result<(), String>;
}

/// Where to send someone who wants to install Freenet by hand.
pub const MANUAL_INSTALL_URL: &str = "https://freenet.org/quickstart/";
