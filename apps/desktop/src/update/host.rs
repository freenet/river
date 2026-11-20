//! Connects the updater to the app: implements [`river_app::UpdateHost`] on an [`Updater`], and
//! the two calls `main` makes (at start, and once the window is running).

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use river_app::{UpdateCheck, UpdateHost, UpdateInfo, UpdateSupport};

use super::manifest::Offer;
use super::platform::Installer;
#[cfg(not(target_os = "linux"))]
use super::platform::Unsupported;
use super::{CheckResult, Config, HANDSHAKE_ENV, HANDSHAKE_TIMEOUT, Support, Updater};
use crate::release::{Version, parse_version};

/// Overrides [`super::MANIFEST_URL`] (tests, staging). The signature is still checked against
/// the compiled-in key, so this cannot point River at a build its key did not sign.
pub const MANIFEST_URL_ENV: &str = "RIVER_UPDATE_MANIFEST_URL";
/// The same override as a flag: `--update-manifest-url=URL`.
pub const MANIFEST_URL_FLAG: &str = "--update-manifest-url=";

pub struct DesktopUpdate {
    updater: Updater,
    key: [u8; 32],
}

fn installer() -> Arc<dyn Installer> {
    #[cfg(target_os = "linux")]
    {
        Arc::new(super::linux::LinuxInstaller::for_current_exe())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Arc::new(Unsupported {
            os: std::env::consts::OS,
        })
    }
}

/// The manifest URL override exists only in debug and `test-hooks` builds, so a release build
/// cannot be pointed anywhere else (the signature would still have to verify, but there is no
/// reason to offer the switch, and `allow_loopback_http` goes with it).
#[cfg(any(debug_assertions, feature = "test-hooks"))]
fn manifest_override() -> Option<String> {
    std::env::args()
        .find_map(|a| a.strip_prefix(MANIFEST_URL_FLAG).map(str::to_owned))
        .or_else(|| std::env::var(MANIFEST_URL_ENV).ok())
        .filter(|u| !u.is_empty())
}

#[cfg(not(any(debug_assertions, feature = "test-hooks")))]
fn manifest_override() -> Option<String> {
    None
}

impl DesktopUpdate {
    /// The updater for the running app, or `None` when there is nowhere to keep its state.
    pub fn new() -> Option<Arc<DesktopUpdate>> {
        let data_dir = directories::ProjectDirs::from("", "Freenet", "River")?
            .data_local_dir()
            .to_owned();
        let running = parse_version(env!("CARGO_PKG_VERSION")).ok()?;
        let over = manifest_override();
        let key = super::pubkey();
        let cfg = Config {
            allow_loopback_http: over.is_some(),
            manifest_url: over.unwrap_or_else(|| super::MANIFEST_URL.to_string()),
            pubkey: key,
            data_dir,
            running,
            channel: super::CHANNEL.to_string(),
            clock: Config::system_clock(),
            handshake_timeout: HANDSHAKE_TIMEOUT,
            floor: super::build_floor(),
            retry_delay: Duration::from_secs(30),
        };
        let updater = Updater::new(cfg, installer());
        // So the swap guard knows what is installed however early an update is applied.
        updater.remember_install_in_background();
        Some(Arc::new(DesktopUpdate { updater, key }))
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn with_parts(updater: Updater, key: [u8; 32]) -> DesktopUpdate {
        DesktopUpdate { updater, key }
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn restart_without_exiting(&self, args: &[OsString]) -> Result<(), String> {
        self.restart_and_release(args)
    }

    /// Whether this process was started by an updater that is waiting for it.
    fn handshake() -> Option<PathBuf> {
        std::env::var_os(HANDSHAKE_ENV).map(PathBuf::from)
    }

    /// Install the downloaded version and start it with `args`, and let go of the apply lock
    /// before returning. `Ok` means the new one has confirmed it started.
    fn restart_and_release(&self, args: &[OsString]) -> Result<(), String> {
        let applied = self.updater.apply()?;
        let launched = self.updater.launch(&applied, args);
        drop(applied);
        launched
    }

    /// Install the downloaded version and start it with `args`; exits this process once the
    /// new one has confirmed it started (the lock is released first).
    fn restart_with(&self, args: &[OsString]) -> Result<(), String> {
        self.restart_and_release(args)?;
        std::process::exit(0);
    }

    /// Call first thing in `main`. A binary that was swapped in and keeps failing to start
    /// rolls itself back here and starts the previous one in its place.
    pub fn on_start(&self) {
        if let Some(previous) = self.updater.on_start(Self::handshake().is_some()) {
            restart_into(&previous);
        }
    }

    /// The window closed normally.
    pub fn clean_exit(&self, ran: Duration) {
        self.updater.clean_exit(ran);
    }
}

/// Replace this process with `path`. Only returns if that fails.
fn restart_into(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(path)
            .args(std::env::args_os().skip(1))
            .exec();
        eprintln!(
            "River: couldn't start the previous version {}: {err}",
            path.display()
        );
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn info(offer: &Offer) -> UpdateInfo {
    UpdateInfo {
        version: offer.version.to_string(),
        notes: offer.notes.clone(),
        size: offer.artifact.size,
        required: offer.required,
    }
}

impl UpdateHost for DesktopUpdate {
    fn version(&self) -> String {
        self.updater.running().to_string()
    }

    fn support(&self) -> UpdateSupport {
        if super::is_placeholder(&self.key) {
            return UpdateSupport::Unavailable(
                "This build of River has no update key, so it cannot check for updates. \
                 Download new versions from github.com/freenet/river/releases."
                    .into(),
            );
        }
        match self.updater.support() {
            Support::Replaceable { .. } => UpdateSupport::Automatic,
            Support::External(why) => UpdateSupport::External(why),
            Support::Unsupported(why) => UpdateSupport::Unavailable(why),
        }
    }

    fn staged(&self) -> Option<String> {
        self.updater
            .staged_version()
            .map(|v: Version| v.to_string())
    }

    /// Tells a waiting updater this version started, and ends probation.
    fn confirm_started(&self) {
        self.updater.confirm_started(Self::handshake().as_deref());
    }

    fn take_notice(&self) -> Option<String> {
        self.updater.take_notice()
    }

    fn check(&self) -> Result<UpdateCheck, String> {
        if super::is_placeholder(&self.key) {
            return Err("this build has no update key".into());
        }
        Ok(match self.updater.check()? {
            CheckResult::UpToDate => UpdateCheck::UpToDate,
            CheckResult::Available(offer) => UpdateCheck::Available(info(&offer)),
        })
    }

    fn download(&self, progress: &mut dyn FnMut(u64, u64)) -> Result<String, String> {
        self.updater.download(progress).map(|v| v.to_string())
    }

    fn restart(&self) -> Result<(), String> {
        self.restart_with(&std::env::args_os().skip(1).collect::<Vec<_>>())
    }
}

/// `river --self-update`: check, download, verify, install and start the new version, once,
/// with no window. Exits 0 when the new version started (this process exits as it does),
/// 10 when already current, 11 when this copy is not River's to replace, 1 on any refusal
/// or failure (the installed River is then unchanged or rolled back). For scripts and the
/// end-to-end test; the app does the same on its own schedule.
pub fn run_cli() -> i32 {
    let Some(u) = DesktopUpdate::new() else {
        eprintln!("river: no place to keep update state");
        return 1;
    };
    println!("river {}: checking for an update", u.version());
    match u.check() {
        Err(e) => {
            eprintln!("river: update refused: {e}");
            1
        }
        Ok(UpdateCheck::UpToDate) => {
            println!("river: up to date");
            10
        }
        Ok(UpdateCheck::Available(info)) => {
            println!("river: {} is available", info.version);
            if u.support() != UpdateSupport::Automatic {
                println!(
                    "river: this copy is not updated by River: {:?}",
                    u.support()
                );
                return 11;
            }
            if let Err(e) = u.download(&mut |_, _| {}) {
                eprintln!("river: update refused: {e}");
                return 1;
            }
            println!("river: downloaded and verified {}", info.version);
            // No arguments: the new River opens its window, not another update run.
            match u.restart_with(&[]) {
                Err(e) => {
                    eprintln!("river: update failed: {e}");
                    1
                }
                Ok(()) => 0,
            }
        }
    }
}

/// `river --update-status`: print whether this build can verify updates. Exit 0 when it has a
/// real update key, 12 when the key is still the placeholder. The release workflow runs this
/// on the binary it built and fails the release on 12, so a build cannot ship that silently
/// cannot update.
pub fn print_status() -> i32 {
    let placeholder = super::is_placeholder(&super::pubkey());
    println!(
        "update key: {}",
        if placeholder {
            "PLACEHOLDER (this build cannot update itself)"
        } else {
            "configured"
        }
    );
    println!("update key bytes: {}", hex::encode(super::pubkey()));
    if placeholder { 12 } else { 0 }
}
