//! The platform-specific half of an update: where the running binary is, whether River may
//! replace it, and how to swap it and put it back.
//!
//! Linux: [`super::linux`], an in-place rename of the one `river` file. Windows:
//! [`super::windows`], which renames the running `river.exe` aside (a running exe cannot be
//! overwritten but can be renamed) and moves the new one into its place. macOS:
//! [`super::macos`], which replaces the whole signed `River.app` bundle with an atomic exchange
//! and checks the new bundle's code signature first. Anything else gets [`Unsupported`], which
//! says so. The orchestrator in [`super`] only talks to this trait, so none of them touches the
//! verification or rollback logic.
//!
//! The "install" is a file on Linux and Windows and a directory (the bundle) on macOS, so the
//! orchestrator never opens it itself: it asks for a [`Installer::fingerprint`] and removes
//! leftovers with [`remove_any`].

use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Child;

use crate::release::Version;

/// Whether this running copy of River can be updated by River.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(any(target_os = "linux", target_os = "macos", windows)),
    allow(dead_code)
)]
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

    /// What identifies the install at `install_path`: the SHA-256 (hex) of the file, or, for an
    /// install that is a directory, of its contents. [`Installer::swap`] returns the same
    /// measure of the copy it keeps, and [`Installer::restore`] checks it.
    fn fingerprint(&self, install_path: &Path) -> Result<String, String> {
        let file = std::fs::File::open(install_path).map_err(|e| e.to_string())?;
        super::hash_file(file)
            .map(|(sum, _)| hex::encode(sum))
            .map_err(|e| e.to_string())
    }

    /// The program to run for the install at `install_path` (the bundle's executable on macOS).
    fn executable(&self, install_path: &Path) -> PathBuf {
        install_path.to_path_buf()
    }

    /// Start the binary at `install_path`.
    fn spawn(
        &self,
        install_path: &Path,
        args: &[OsString],
        env: &[(&str, String)],
    ) -> std::io::Result<Child>;
}

/// Delete `path`, a file or a whole directory; a symlink is removed, never followed. Best
/// effort: what cannot be deleted now (a Windows exe that is still running) is left for
/// [`super::Updater::sweep`].
pub fn remove_any(path: &Path) {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => {
            let _ = std::fs::remove_dir_all(path);
        }
        Ok(_) => {
            let _ = std::fs::remove_file(path);
        }
        Err(_) => {}
    }
}

/// The platforms River cannot update itself on. This is what 0.1.0 used on Windows and macOS,
/// and the tests keep it to show such a build is told plainly, not given an error.
#[cfg(any(test, not(any(target_os = "linux", target_os = "macos", windows))))]
pub struct Unsupported {
    pub os: &'static str,
}

#[cfg(any(test, not(any(target_os = "linux", target_os = "macos", windows))))]
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::manifest::tests::{NOW, manifest, pubkey, signed};
    use crate::update::manifest::{Artifact, Trust, Verdict, evaluate, verify_and_parse};

    /// The keys a release manifest lists since Windows and macOS update themselves.
    const KEYS: [&str; 3] = ["linux-x86_64", "windows-x86_64", "macos-universal"];

    fn three_platform_manifest() -> (Vec<u8>, Vec<u8>) {
        let mut m = manifest("0.2.0", 5);
        for key in KEYS {
            m.artifacts.insert(
                key.to_string(),
                Artifact {
                    url: format!("https://example.org/River-0.2.0-{key}"),
                    sha256: "cd".repeat(32),
                    size: 1000,
                },
            );
        }
        signed(&m)
    }

    #[test]
    fn a_manifest_for_three_platforms_offers_each_its_own_build() {
        let (bytes, sig) = three_platform_manifest();
        let parsed = verify_and_parse(&bytes, &sig, &pubkey(1)).unwrap();
        for key in KEYS {
            let verdict = evaluate(
                &parsed,
                crate::release::Version(0, 1, 0),
                "stable",
                key,
                NOW,
                Trust::default(),
            )
            .unwrap();
            let Verdict::Available(offer) = verdict else {
                panic!("{key}: {verdict:?}")
            };
            assert!(offer.artifact.url.ends_with(key), "{key}");
        }
    }

    /// What River 0.1.0 does on Windows and macOS with such a manifest: it has no platform key,
    /// so it says plainly that it can't update itself, and every action refuses with words,
    /// never touching a file.
    #[test]
    fn a_build_without_a_platform_installer_is_told_plainly() {
        let old = Unsupported { os: "windows" };
        assert_eq!(old.platform_key(), None);
        assert!(matches!(old.support(), Support::Unsupported(m) if m.contains("Download")));
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f");
        std::fs::write(&f, b"x").unwrap();
        assert!(
            old.prepare(
                std::fs::File::open(&f).unwrap(),
                crate::release::Version(0, 2, 0),
                &f
            )
            .is_err()
        );
        assert!(old.swap(&f, &f, &f, None).is_err());
        assert_eq!(std::fs::read(&f).unwrap(), b"x");
    }
}
