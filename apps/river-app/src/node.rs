//! What River needs from the platform it runs on, to reach a Freenet node.
//!
//! The desktop shell answers these by probing the node's address (`127.0.0.1:7509` unless the
//! person chose another, see [`crate::address`]) and installing Freenet from the
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
    /// Nothing answered at a node address the person gave ([`NodeHost::probe_at`]). Whether
    /// Freenet is installed on this computer says nothing about it, so it is not asked.
    Unreachable,
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

    /// Ask the node at `authority` (`host:port`) which version it is, as [`probe`] does for the
    /// node on this computer. The answer is only ever [`NodeProbe::Running`],
    /// [`NodeProbe::Outdated`] or [`NodeProbe::Unreachable`]: a node somebody else runs is not
    /// installed, started or stopped from here. It must send nothing but the version request,
    /// and follow no redirect.
    ///
    /// [`probe`]: NodeHost::probe
    fn probe_at(&self, authority: &str) -> NodeProbe;

    /// Whether this platform can install Freenet itself.
    fn can_install(&self) -> bool;

    /// Download, verify and install Freenet, reporting progress. Runs on a worker thread.
    /// On return the node may or may not be running; the caller probes and calls [`start`].
    ///
    /// [`start`]: NodeHost::start
    fn install(&self, progress: &mut dyn FnMut(InstallStep)) -> Result<(), String>;

    /// Start an installed node that is not running. Runs on a worker thread.
    fn start(&self) -> Result<(), String>;

    /// The latest Freenet release, for the node status dialog. Blocks on the network, so it runs
    /// on a worker thread. `None` when it can't be found (or this shell doesn't look).
    fn latest_version(&self) -> Option<String> {
        None
    }

    /// Whether [`latest_version`](NodeHost::latest_version) looks anything up. When it does
    /// not, the node status dialog says nothing about the latest release.
    fn looks_up_latest(&self) -> bool {
        false
    }

    /// A directory private to this user where River keeps its settings, including the
    /// permission the node grants it to open the user's rooms. `None` keeps that permission
    /// for this run only, so it is asked for again next time.
    fn config_dir(&self) -> Option<std::path::PathBuf> {
        None
    }
}

/// Where to send someone who wants to install Freenet by hand.
pub const MANUAL_INSTALL_URL: &str = "https://freenet.org/quickstart/";
