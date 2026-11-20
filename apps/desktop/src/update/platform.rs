//! The platform-specific half of an update: where the running binary is, whether River may
//! replace it, and how to swap it and put it back.
//!
//! Only Linux (any Unix path layout, in fact) has a real implementation, [`super::linux`].
//! macOS and Windows get [`Unsupported`], which says so. Their swaps need a different
//! mechanism (a signed, notarized app bundle that cannot be patched in place; an exe that
//! cannot be replaced while it runs) that cannot be written responsibly without a machine of
//! that kind to test it on. The orchestrator in [`super`] only talks to this trait, so adding
//! them does not touch the verification or rollback logic.

use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Child;

use crate::release::Version;

/// Whether this running copy of River can be updated by River.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub enum Support {
    /// River installed itself here and may replace this file.
    Replaceable { install_path: PathBuf },
    /// Something else updates this copy (a package manager, the AppImage tooling, Flatpak,
    /// a cargo build). The reason says what, in words for the user.
    External(String),
    /// River cannot update this copy: not yet written for this platform, or no permission.
    Unsupported(String),
}

pub trait Installer: Send + Sync {
    /// The key of this platform's build in the manifest, if there is one.
    fn platform_key(&self) -> Option<&'static str>;

    fn support(&self) -> Support;

    /// Unpack the new binary from a verified `artifact` into a file beside `install_path`
    /// (same filesystem, so a rename can replace it), and check that it runs and reports
    /// `version`. Returns that file. Never touches `install_path`.
    fn prepare(
        &self,
        artifact: File,
        version: Version,
        install_path: &Path,
    ) -> Result<PathBuf, String>;

    /// Keep a copy of `install_path` at `previous`, then atomically replace `install_path` with
    /// `prepared`. Returns the SHA-256 of the copy. If it fails, `install_path` is as it was.
    ///
    /// If `expected_sha256` is given, `install_path` must hash to it (it is what this River
    /// started as); otherwise nothing is changed and an error says it moved.
    fn swap(
        &self,
        prepared: &Path,
        install_path: &Path,
        previous: &Path,
        expected_sha256: Option<&str>,
    ) -> Result<String, String>;

    /// Put the copy at `previous` (which must hash to `sha256`) back at `install_path`.
    fn restore(&self, previous: &Path, sha256: &str, install_path: &Path) -> Result<(), String>;

    /// Start the binary at `install_path`.
    fn spawn(
        &self,
        install_path: &Path,
        args: &[OsString],
        env: &[(&str, String)],
    ) -> std::io::Result<Child>;
}

/// The platforms River cannot update itself on yet.
#[cfg(not(target_os = "linux"))]
pub struct Unsupported {
    pub os: &'static str,
}

#[cfg(not(target_os = "linux"))]
impl Installer for Unsupported {
    fn platform_key(&self) -> Option<&'static str> {
        None
    }

    fn support(&self) -> Support {
        Support::Unsupported(format!(
            "River can't update itself on {} yet. Download the new version from \
             github.com/freenet/river/releases.",
            self.os
        ))
    }

    fn prepare(&self, _: File, _: Version, _: &Path) -> Result<PathBuf, String> {
        Err(format!("self-update is not supported on {} yet", self.os))
    }

    fn swap(&self, _: &Path, _: &Path, _: &Path, _: Option<&str>) -> Result<String, String> {
        Err(format!("self-update is not supported on {} yet", self.os))
    }

    fn restore(&self, _: &Path, _: &str, _: &Path) -> Result<(), String> {
        Err(format!("self-update is not supported on {} yet", self.os))
    }

    fn spawn(&self, _: &Path, _: &[OsString], _: &[(&str, String)]) -> std::io::Result<Child> {
        Err(std::io::Error::other(
            "self-update is not supported on this platform yet",
        ))
    }
}
