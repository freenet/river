//! The desktop answer to "where is Freenet, and how do I get one running?".

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use river_app::{InstallStep, NodeHost, NodeProbe};

use crate::install;
use crate::release::{self, Release};

/// The node's client API. `/v1/version` answers even while the node is still starting up.
const VERSION_URL: &str = "http://127.0.0.1:7509/v1/version";

pub struct DesktopHost {
    agent: ureq::Agent,
}

impl DesktopHost {
    pub fn new() -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(2))
            .build();
        DesktopHost { agent }
    }

    fn running_version(&self) -> Option<String> {
        let body = self
            .agent
            .get(VERSION_URL)
            .call()
            .ok()?
            .into_string()
            .ok()?;
        let value: serde_json::Value = serde_json::from_str(&body).ok()?;
        value.get("version")?.as_str().map(str::to_owned)
    }
}

impl NodeHost for DesktopHost {
    fn probe(&self) -> NodeProbe {
        if let Some(version) = self.running_version() {
            NodeProbe::Running { version }
        } else if find_installed().is_some() {
            NodeProbe::InstalledNotRunning
        } else {
            NodeProbe::NotInstalled
        }
    }

    fn can_install(&self) -> bool {
        release::asset_name("0").is_ok()
    }

    fn install(&self, progress: &mut dyn FnMut(InstallStep)) -> Result<(), String> {
        progress(InstallStep::FindingRelease);
        let release = Release::latest()?;
        let asset = release::asset_name(release.version())?;
        // Authenticate the manifest BEFORE downloading, so a bad release fails fast.
        let checksums = release.checksums()?;
        checksums.get(&asset)?;
        let bytes = release.fetch(&asset, &mut |received, total| {
            progress(InstallStep::Downloading { received, total })
        })?;
        progress(InstallStep::Verifying);
        release::verify_asset(&checksums, &asset, &bytes)?;
        install::install(&asset, &bytes, progress)
    }

    fn start(&self) -> Result<(), String> {
        match find_installed().ok_or("Freenet isn't installed")? {
            Installed::MacApp(app) => run(Command::new("open").arg(app)),
            Installed::Binary(bin) => {
                if run(Command::new(&bin).args(["service", "start"])).is_ok() {
                    return Ok(());
                }
                if system_service_installed() {
                    return Err("Freenet is installed as a system service. Start it with \
                                `sudo systemctl start freenet`, then try again."
                        .into());
                }
                // Installed as a bare binary with no service yet: register it, then start.
                run(Command::new(&bin).args(["service", "install"]))?;
                run(Command::new(&bin).args(["service", "start"]))
            }
        }
    }
}

#[derive(Debug)]
pub enum Installed {
    /// macOS: the Freenet app bundle, which runs the node (and its tray icon) when opened.
    MacApp(PathBuf),
    Binary(PathBuf),
}

pub fn find_installed() -> Option<Installed> {
    if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let bundles = [
            Some(PathBuf::from("/Applications/Freenet.app")),
            home.map(|h| h.join("Applications/Freenet.app")),
        ];
        if let Some(app) = bundles.into_iter().flatten().find(|p| p.exists()) {
            return Some(Installed::MacApp(app));
        }
    }
    install::binary_install_path()
        .filter(|p| p.is_file())
        .or_else(|| {
            on_path(if cfg!(windows) {
                "freenet.exe"
            } else {
                "freenet"
            })
        })
        .map(Installed::Binary)
}

fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

fn system_service_installed() -> bool {
    cfg!(target_os = "linux") && Path::new("/etc/systemd/system/freenet.service").exists()
}

/// Run a command to completion with no console input, returning its stderr on failure.
/// Every handle is set explicitly: a GUI process on Windows has no valid console handles to
/// inherit, and inheriting them makes `spawn` fail (freenet-core #3933/#3934).
pub fn run(cmd: &mut Command) -> Result<(), String> {
    let out = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("couldn't run {:?}: {e}", cmd.get_program()))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let detail = stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("no error output");
    Err(format!(
        "{:?} {} failed: {detail}",
        cmd.get_program(),
        cmd.get_args()
            .map(|a| a.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    ))
}
