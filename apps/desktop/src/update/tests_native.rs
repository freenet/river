//! The updater end to end on Windows and macOS: a real HTTP server on loopback serving a signed
//! manifest and the platform's own artifact (a zip holding `river.exe`; a zip of an ad-hoc
//! signed `River.app`), a real compiled program as the installed River, and the real check,
//! download, verification, swap, start, confirm and rollback code between them.
//!
//! What only a release can show (a Developer ID signature, Gatekeeper's verdict, Program
//! Files) is covered by the per-platform unit tests where it can be, and by the release
//! checklist where it cannot.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::Signer;
use sha2::{Digest, Sha256};

use super::manifest::tests::{NOW, key, pubkey};
use super::manifest::{Artifact, FORMAT, Manifest, Trust, signed_bytes};
use super::testserver::{Ports, Reply, TestServer};
use super::unpack::testexe::Starts;
use super::*;

/// The installed River and where its state lives.
struct Rig {
    data: tempfile::TempDir,
    /// Holds the install (and anything an update leaves beside it).
    _root: tempfile::TempDir,
    install: PathBuf,
    installer: Arc<dyn Installer>,
}

#[cfg(windows)]
impl Rig {
    fn new() -> Rig {
        let root = tempfile::tempdir().unwrap();
        let install = root.path().join("River").join("river.exe");
        fs::create_dir_all(install.parent().unwrap()).unwrap();
        fs::copy(super::unpack::testexe::build("0.1.0"), &install).unwrap();
        let installer = super::windows::WindowsInstaller::anywhere(install.clone());
        let Support::Replaceable { install_path } = installer.support() else {
            panic!("{:?}", installer.support())
        };
        Rig {
            data: tempfile::tempdir().unwrap(),
            _root: root,
            install: install_path,
            installer: Arc::new(installer),
        }
    }

    fn artifact(version: &str, starts: Starts) -> Vec<u8> {
        use std::io::Write;
        let exe = fs::read(super::unpack::testexe::build_as(version, starts)).unwrap();
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        let folder = format!("River-{version}-windows-x86_64");
        w.start_file(format!("{folder}/README.txt"), opts).unwrap();
        w.write_all(b"readme").unwrap();
        w.start_file(format!("{folder}/river.exe"), opts).unwrap();
        w.write_all(&exe).unwrap();
        w.finish().unwrap().into_inner()
    }
}

#[cfg(target_os = "macos")]
impl Rig {
    fn new() -> Rig {
        use super::macos::{Check, MacInstaller, tests::bundle};
        let root = tempfile::tempdir().unwrap();
        let apps = root.path().join("Applications");
        fs::create_dir_all(&apps).unwrap();
        let app = bundle(&apps, "River.app", "0.1.0");
        let installer =
            MacInstaller::with_check(app.join("Contents/MacOS/river"), Some(Check::IntactOnly));
        let Support::Replaceable { install_path } = installer.support() else {
            panic!("{:?}", installer.support())
        };
        Rig {
            data: tempfile::tempdir().unwrap(),
            _root: root,
            install: install_path,
            installer: Arc::new(installer),
        }
    }

    fn artifact(version: &str, starts: Starts) -> Vec<u8> {
        use super::macos::tests::{bundle_as, zip_of};
        let build = tempfile::tempdir().unwrap();
        let app = bundle_as(build.path(), "River.app", version, starts);
        let zip = build.path().join("a.zip");
        zip_of(&app, &zip);
        fs::read(zip).unwrap()
    }
}

impl Rig {
    fn updater(&self, running: &str, manifest_url: &str) -> Updater {
        Updater::new(
            Config {
                manifest_url: manifest_url.to_string(),
                pubkey: pubkey(1),
                data_dir: self.data.path().to_path_buf(),
                running: parse_version(running).unwrap(),
                channel: "stable".into(),
                allow_loopback_http: true,
                clock: Arc::new(|| NOW),
                handshake_timeout: Duration::from_secs(20),
                floor: Trust::default(),
                retry_delay: Duration::ZERO,
                key_authority: None,
            },
            self.installer.clone(),
        )
    }

    /// The version the installed River reports when run.
    fn installed_version(&self) -> String {
        let out = Command::new(self.installer.executable(&self.install))
            .arg("--version")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Serve `version` as the current release, built to start as `starts`.
    fn publish(&self, server: &TestServer, version: &str, starts: Starts) {
        let artifact = Rig::artifact(version, starts);
        let path = format!("/River-{version}.zip");
        let mut artifacts = std::collections::BTreeMap::new();
        artifacts.insert(
            self.installer.platform_key().unwrap().to_string(),
            Artifact {
                url: server.url(&path),
                sha256: hex::encode(Sha256::digest(&artifact)),
                size: artifact.len() as u64,
            },
        );
        let manifest = Manifest {
            format: FORMAT,
            channel: "stable".into(),
            version: version.into(),
            sequence: 10,
            released_at: NOW - 100,
            expires_at: NOW + 100_000,
            min_supported: "0.1.0".into(),
            notes: format!("River {version}"),
            artifacts,
        }
        .canonical_bytes();
        let sig = key(1).sign(&signed_bytes(&manifest)).to_bytes().to_vec();
        server.set("/manifest.json", Reply::Body(manifest));
        server.set("/manifest.json.sig", Reply::Body(sig));
        server.set(&path, Reply::Body(artifact));
    }

    fn stage(&self, u: &Updater) {
        u.remember_install();
        assert!(matches!(
            u.check(&KeyRecord::Unavailable).unwrap(),
            CheckResult::Available(_)
        ));
        u.download(&mut |_, _| {}).unwrap();
        assert_eq!(
            self.installed_version(),
            "river 0.1.0",
            "staging changes nothing"
        );
    }

    /// Only the install is left beside the install, once the old process is gone (Windows
    /// leaves the renamed-aside exe for the next start's sweep, which runs here).
    fn assert_nothing_beside(&self, u: &Updater) {
        let folder = self.install.parent().unwrap();
        for entry in fs::read_dir(folder).unwrap().flatten() {
            let _ = filetime_back(&entry.path());
        }
        u.sweep();
        let names: Vec<String> = fs::read_dir(folder)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                self.install
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            ]
        );
    }
}

/// Age a leftover past the sweep's threshold.
fn filetime_back(path: &Path) -> std::io::Result<()> {
    let old = std::time::SystemTime::now() - LEFTOVER_AGE - Duration::from_secs(60);
    let f = fs::File::options()
        .write(true)
        .open(path)
        .or_else(|_| fs::File::open(path))?;
    f.set_modified(old)
}

#[test]
fn an_update_is_verified_swapped_started_and_confirmed() {
    let server = Ports::lock().serve(vec![]);
    let rig = Rig::new();
    rig.publish(&server, "0.2.0", Starts::Confirms);
    let u = rig.updater("0.1.0", &server.url("/manifest.json"));
    rig.stage(&u);

    let applied = u.apply().unwrap();
    assert_eq!(rig.installed_version(), "river 0.2.0");
    let probation = state::load(rig.data.path())
        .probation
        .expect("on probation");
    assert_eq!(
        rig.installer.fingerprint(&probation.previous).unwrap(),
        probation.previous_sha256,
        "the kept copy is the old River"
    );
    // The new River writes the handshake, so the launch succeeds.
    u.launch(&applied, &[]).unwrap();
    drop(applied);

    let new = rig.updater("0.2.0", &server.url("/manifest.json"));
    new.confirm_started(None);
    assert!(state::load(rig.data.path()).probation.is_none());
    assert_eq!(
        new.check(&KeyRecord::Unavailable).unwrap(),
        CheckResult::UpToDate
    );
    rig.assert_nothing_beside(&new);
}

#[test]
fn a_new_build_that_crashes_is_rolled_back() {
    let server = Ports::lock().serve(vec![]);
    let rig = Rig::new();
    rig.publish(&server, "0.2.0", Starts::Crashes);
    let u = rig.updater("0.1.0", &server.url("/manifest.json"));
    rig.stage(&u);
    let applied = u.apply().unwrap();
    assert_eq!(rig.installed_version(), "river 0.2.0");

    let err = u.launch(&applied, &[]).unwrap_err();
    assert!(err.contains("went back to 0.1.0"), "{err}");
    assert_eq!(rig.installed_version(), "river 0.1.0");
    assert!(state::load(rig.data.path()).probation.is_none());
    drop(applied);
    rig.assert_nothing_beside(&u);
}

/// A new build started by hand (no launcher) that never confirms rolls itself back, and what it
/// hands back to start in its place is the old River's program.
#[test]
fn a_new_build_started_by_hand_that_never_confirms_rolls_itself_back() {
    let server = Ports::lock().serve(vec![]);
    let rig = Rig::new();
    rig.publish(&server, "0.2.0", Starts::Confirms);
    let u = rig.updater("0.1.0", &server.url("/manifest.json"));
    rig.stage(&u);
    let applied = u.apply().unwrap();
    drop(applied);

    let new = rig.updater("0.2.0", &server.url("/manifest.json"));
    for _ in 0..MAX_UNCONFIRMED_STARTS {
        assert_eq!(new.on_start(false), None);
    }
    let start = new.on_start(false).expect("rolled back");
    assert_eq!(start, rig.installer.executable(&rig.install));
    assert_eq!(rig.installed_version(), "river 0.1.0");
    let out = Command::new(&start).arg("--version").output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "river 0.1.0");
}
