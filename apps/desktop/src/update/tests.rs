//! The updater end to end: a real HTTP server on 127.0.0.1:4040x serving a signed manifest and
//! an artifact, a stand-in "installed River" (a shell script) in a temporary directory, and the
//! real verification, download, swap, start, confirm and rollback code between them.
//!
//! The fake binaries are scripts that answer `--version` and, when started, write the handshake
//! the way the real binary's `confirm_started` does (or fail to).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};

use super::linux::LinuxInstaller;
use super::linux::tests::tarball;
use super::manifest::tests::{NOW, key, pubkey};
use super::manifest::{Artifact, FORMAT, Manifest, Trust, signed_bytes};
use super::testserver::{Ports, Reply, TestServer};
use super::*;

const OLD: &[u8] = b"#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'river 0.1.0'; fi\n";

/// A new binary that starts and confirms.
fn healthy(version: &str) -> Vec<u8> {
    format!(
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'river {version}'; exit 0; fi\n\
         if [ -n \"$RIVER_UPDATE_HANDSHAKE\" ]; then printf '{version}' > \"$RIVER_UPDATE_HANDSHAKE\"; fi\n\
         sleep 1\n"
    )
    .into_bytes()
}

/// A new binary that passes the version check and then dies when really started.
fn crashes(version: &str) -> Vec<u8> {
    format!("#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'river {version}'; exit 0; fi\nexit 3\n")
        .into_bytes()
}

/// A new binary that starts and never confirms.
fn hangs(version: &str) -> Vec<u8> {
    format!("#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'river {version}'; exit 0; fi\nsleep 30\n")
        .into_bytes()
}

struct Rig {
    data: tempfile::TempDir,
    bin: tempfile::TempDir,
    install: PathBuf,
}

impl Rig {
    fn new() -> Rig {
        let rig = Rig {
            data: tempfile::tempdir().unwrap(),
            bin: tempfile::tempdir().unwrap(),
            install: PathBuf::new(),
        };
        let install = rig.bin.path().join("river");
        fs::write(&install, OLD).unwrap();
        fs::set_permissions(&install, fs::Permissions::from_mode(0o755)).unwrap();
        Rig { install, ..rig }
    }

    fn updater(&self, running: &str, manifest_url: &str, seed: u8) -> Updater {
        self.updater_with(
            running,
            manifest_url,
            seed,
            NOW,
            Arc::new(LinuxInstaller::at(self.install.clone())),
        )
    }

    fn updater_with(
        &self,
        running: &str,
        manifest_url: &str,
        seed: u8,
        now: u64,
        installer: Arc<dyn Installer>,
    ) -> Updater {
        Updater::new(
            Config {
                manifest_url: manifest_url.to_string(),
                pubkey: pubkey(seed),
                data_dir: self.data.path().to_path_buf(),
                running: parse_version(running).unwrap(),
                channel: "stable".into(),
                allow_loopback_http: true,
                clock: Arc::new(move || now),
                handshake_timeout: Duration::from_secs(3),
                floor: Trust::default(),
                retry_delay: Duration::ZERO,
                key_authority: None,
            },
            installer,
        )
    }

    fn installed(&self) -> Vec<u8> {
        fs::read(&self.install).unwrap()
    }

    /// Nothing about the installation changed: same bytes, no kept copy, no swap recorded,
    /// nothing left beside it.
    fn assert_untouched(&self, why: &str) {
        assert_eq!(self.installed(), OLD, "{why}: the installed binary changed");
        let state = state::load(self.data.path());
        assert!(state.probation.is_none(), "{why}: a swap was recorded");
        assert!(
            !self.data.path().join("update/previous").exists(),
            "{why}: a copy was kept"
        );
        let beside: Vec<_> = fs::read_dir(self.bin.path()).unwrap().collect();
        assert_eq!(
            beside.len(),
            1,
            "{why}: files left beside the binary: {beside:?}"
        );
    }
}

fn platform() -> &'static str {
    LinuxInstaller::at(PathBuf::new())
        .platform_key()
        .expect("tests run on x86_64 or aarch64")
}

struct Release {
    version: &'static str,
    sequence: u64,
    binary: Vec<u8>,
    /// Which key signs the manifest.
    signer: u8,
    released_at: u64,
    expires_at: u64,
}

impl Release {
    fn new(version: &'static str, sequence: u64, binary: Vec<u8>) -> Release {
        Release {
            version,
            sequence,
            binary,
            signer: 1,
            released_at: NOW - 100,
            expires_at: NOW + 100_000,
        }
    }

    /// The artifact's bytes.
    fn artifact(&self) -> Vec<u8> {
        tarball(&[("River/river", &self.binary), ("River/LICENSE", b"license")])
    }

    fn manifest(&self, server: &TestServer, artifact: &[u8]) -> Manifest {
        let mut artifacts = std::collections::BTreeMap::new();
        artifacts.insert(
            platform().to_string(),
            Artifact {
                url: server.url(&format!("/river-{}.tar.gz", self.version)),
                sha256: hex::encode(Sha256::digest(artifact)),
                size: artifact.len() as u64,
            },
        );
        Manifest {
            format: FORMAT,
            channel: "stable".into(),
            version: self.version.into(),
            sequence: self.sequence,
            released_at: self.released_at,
            expires_at: self.expires_at,
            min_supported: "0.1.0".into(),
            notes: format!("River {}", self.version),
            artifacts,
        }
    }

    /// Serve this release: manifest, signature and artifact.
    fn publish(&self, server: &TestServer) {
        let artifact = self.artifact();
        let manifest = self.manifest(server, &artifact).canonical_bytes();
        let sig = key(self.signer)
            .sign(&signed_bytes(&manifest))
            .to_bytes()
            .to_vec();
        server.set("/manifest.json", Reply::Body(manifest));
        server.set("/manifest.json.sig", Reply::Body(sig));
        server.set(
            &format!("/river-{}.tar.gz", self.version),
            Reply::Body(artifact),
        );
    }
}

fn offer(r: CheckResult) -> Offer {
    match r {
        CheckResult::Available(o) => o,
        other => panic!("expected an offer, got {other:?}"),
    }
}

#[test]
fn an_update_is_verified_swapped_started_and_confirmed() {
    let ports = Ports::lock();
    let server = ports.serve(vec![]);
    Release::new("0.2.0", 10, healthy("0.2.0")).publish(&server);
    let rig = Rig::new();
    let u = rig.updater("0.1.0", &server.url("/manifest.json"), 1);

    let found = offer(u.check(&KeyRecord::Unavailable).unwrap());
    assert_eq!(found.version, Version(0, 2, 0));
    assert_eq!(rig.installed(), OLD, "checking changes nothing");

    let mut last = (0, 0);
    assert_eq!(
        u.download(&mut |got, total| last = (got, total)).unwrap(),
        Version(0, 2, 0)
    );
    assert_eq!(last.0, last.1, "progress ends complete");
    assert_eq!(rig.installed(), OLD, "downloading changes nothing");
    assert_eq!(u.staged_version(), Some(Version(0, 2, 0)));

    let applied = u.apply().unwrap();
    assert_eq!(
        rig.installed(),
        healthy("0.2.0"),
        "the new binary is installed"
    );
    assert_eq!(
        fs::metadata(&rig.install).unwrap().permissions().mode() & 0o777,
        0o755
    );
    let st = state::load(rig.data.path());
    let probation = st
        .probation
        .expect("the swap is on probation until confirmed");
    assert_eq!(
        fs::read(&probation.previous).unwrap(),
        OLD,
        "the old binary is kept for rollback"
    );
    assert!(st.staged.is_none());

    // Start it. The healthy script writes the handshake, so the launcher is satisfied.
    u.launch(&applied, &[]).unwrap();

    // The new binary's side of it: it confirms, and probation ends.
    let new = rig.updater("0.2.0", &server.url("/manifest.json"), 1);
    new.on_start(true);
    assert!(
        state::load(rig.data.path()).probation.is_some(),
        "a supervised start does not end probation"
    );
    let marker = rig.data.path().join("update/handshake-test");
    new.confirm_started(Some(&marker));
    assert_eq!(fs::read_to_string(&marker).unwrap(), "0.2.0");
    assert!(state::load(rig.data.path()).probation.is_none());
    // The version it now runs is current.
    assert_eq!(
        new.check(&KeyRecord::Unavailable).unwrap(),
        CheckResult::UpToDate
    );
}

#[test]
fn a_new_binary_that_crashes_is_rolled_back() {
    let ports = Ports::lock();
    let server = ports.serve(vec![]);
    Release::new("0.2.0", 10, crashes("0.2.0")).publish(&server);
    let rig = Rig::new();
    let u = rig.updater("0.1.0", &server.url("/manifest.json"), 1);
    u.check(&KeyRecord::Unavailable).unwrap();
    u.download(&mut |_, _| {}).unwrap();
    let applied = u.apply().unwrap();
    assert_eq!(rig.installed(), crashes("0.2.0"));

    let err = u.launch(&applied, &[]).unwrap_err();
    assert!(err.contains("went back to 0.1.0"), "{err}");
    assert_eq!(
        rig.installed(),
        OLD,
        "the old binary is back, byte for byte"
    );
    assert!(state::load(rig.data.path()).probation.is_none());
    assert_eq!(u.take_notice().as_deref(), Some(err.as_str()));
    assert_eq!(u.take_notice(), None, "told once");
}

#[test]
fn a_new_binary_that_never_confirms_is_killed_and_rolled_back() {
    let ports = Ports::lock();
    let server = ports.serve(vec![]);
    Release::new("0.2.0", 10, hangs("0.2.0")).publish(&server);
    let rig = Rig::new();
    let mut cfg_updater = rig.updater("0.1.0", &server.url("/manifest.json"), 1);
    cfg_updater.cfg.handshake_timeout = Duration::from_millis(600);
    cfg_updater.check(&KeyRecord::Unavailable).unwrap();
    cfg_updater.download(&mut |_, _| {}).unwrap();
    let applied = cfg_updater.apply().unwrap();
    let started = std::time::Instant::now();
    let err = cfg_updater.launch(&applied, &[]).unwrap_err();
    assert!(err.contains("did not start in time"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(rig.installed(), OLD);
}

#[test]
fn a_binary_that_will_not_run_is_never_installed() {
    let ports = Ports::lock();
    let server = ports.serve(vec![]);
    for (name, binary) in [
        ("wrong version", healthy("0.9.9")),
        ("not a program", b"garbage".to_vec()),
    ] {
        Release::new("0.2.0", 10, binary).publish(&server);
        let rig = Rig::new();
        let u = rig.updater("0.1.0", &server.url("/manifest.json"), 1);
        u.check(&KeyRecord::Unavailable).unwrap();
        u.download(&mut |_, _| {}).unwrap();
        assert!(u.apply().is_err(), "{name}");
        rig.assert_untouched(name);
    }
}

#[test]
fn hostile_servers_and_manifests_are_refused_and_change_nothing() {
    let ports = Ports::lock();
    let server = ports.serve(vec![]);
    let url = server.url("/manifest.json");
    let good = Release::new("0.2.0", 10, healthy("0.2.0"));

    // A manifest signed by someone else.
    let rig = Rig::new();
    Release {
        signer: 2,
        ..Release::new("0.2.0", 10, healthy("0.2.0"))
    }
    .publish(&server);
    let err = rig
        .updater("0.1.0", &url, 1)
        .check(&KeyRecord::Unavailable)
        .unwrap_err();
    assert!(err.contains("signature"), "{err}");
    rig.assert_untouched("bad signature");
    assert_eq!(
        state::load(rig.data.path()).highest_sequence,
        0,
        "a refused manifest moves nothing"
    );

    // Unsigned: the signature file is missing.
    let rig = Rig::new();
    good.publish(&server);
    server.set("/manifest.json.sig", Reply::Status(404));
    assert!(
        rig.updater("0.1.0", &url, 1)
            .check(&KeyRecord::Unavailable)
            .is_err()
    );
    rig.assert_untouched("unsigned");

    // Manifest bytes altered after signing.
    good.publish(&server);
    let artifact = good.artifact();
    let mut altered = good.manifest(&server, &artifact).canonical_bytes();
    let at = altered.windows(5).position(|w| w == b"0.2.0").unwrap();
    altered[at + 2] = b'9';
    server.set("/manifest.json", Reply::Body(altered));
    let err = rig
        .updater("0.1.0", &url, 1)
        .check(&KeyRecord::Unavailable)
        .unwrap_err();
    assert!(err.contains("signature"), "{err}");
    rig.assert_untouched("altered manifest");

    // Tampered artifact: signed manifest, other bytes at the artifact URL.
    good.publish(&server);
    let genuine = good.artifact();
    let mut evil = genuine.clone();
    let last = evil.len() - 1;
    evil[last] ^= 0xff;
    server.set("/river-0.2.0.tar.gz", Reply::Body(evil));
    let u = rig.updater("0.1.0", &url, 1);
    u.check(&KeyRecord::Unavailable).unwrap();
    let err = u.download(&mut |_, _| {}).unwrap_err();
    assert!(err.contains("signed checksum"), "{err}");
    assert!(u.staged_version().is_none());
    assert!(u.apply().is_err());
    rig.assert_untouched("tampered artifact");
    assert!(
        !rig.data.path().join("update/staged").exists(),
        "the bad download was deleted"
    );

    // An artifact of the wrong size, and one that is endless.
    server.set(
        "/river-0.2.0.tar.gz",
        Reply::Body([genuine.clone(), vec![0]].concat()),
    );
    assert!(u.download(&mut |_, _| {}).is_err());
    server.set("/river-0.2.0.tar.gz", Reply::Endless);
    assert!(u.download(&mut |_, _| {}).unwrap_err().contains("larger"));
    rig.assert_untouched("wrong size");

    // An artifact URL that redirects to another origin.
    let other = ports.serve(vec![("/payload", Reply::Body(genuine))]);
    server.set(
        "/river-0.2.0.tar.gz",
        Reply::Redirect(other.url_via_localhost("/payload")),
    );
    assert!(
        u.download(&mut |_, _| {})
            .unwrap_err()
            .contains("refused a redirect")
    );
    assert_eq!(other.hits("/payload"), 0);
    rig.assert_untouched("cross-origin redirect");
}

#[test]
fn downgrades_replays_rollbacks_and_expiry_are_refused() {
    let ports = Ports::lock();
    let server = ports.serve(vec![]);
    let url = server.url("/manifest.json");
    let rig = Rig::new();
    let u = rig.updater("0.1.0", &url, 1);

    // A genuine signed manifest for an older version than the one running: nothing offered.
    Release::new("0.0.9", 5, healthy("0.0.9")).publish(&server);
    assert_eq!(
        u.check(&KeyRecord::Unavailable).unwrap(),
        CheckResult::UpToDate
    );
    assert!(
        u.download(&mut |_, _| {}).is_err(),
        "nothing was offered, so nothing downloads"
    );
    rig.assert_untouched("downgrade");

    // The current release is accepted and remembered.
    Release::new("0.2.0", 10, healthy("0.2.0")).publish(&server);
    offer(u.check(&KeyRecord::Unavailable).unwrap());
    // Replay: the older signed manifest, genuinely signed, comes back.
    Release::new("0.0.9", 5, healthy("0.0.9")).publish(&server);
    let err = u.check(&KeyRecord::Unavailable).unwrap_err();
    assert!(err.contains("older than one already seen"), "{err}");
    // Replay of an old sequence carrying a newer version number is just as stale.
    Release::new("0.3.0", 9, healthy("0.3.0")).publish(&server);
    assert!(
        u.check(&KeyRecord::Unavailable)
            .unwrap_err()
            .contains("older than one already seen")
    );
    // Rollback: a new signing that names an older version than was already offered.
    Release::new("0.1.5", 11, healthy("0.1.5")).publish(&server);
    let err = u.check(&KeyRecord::Unavailable).unwrap_err();
    assert!(err.contains("already offered"), "{err}");
    // The remembered floor survives a restart of the app.
    let again = rig.updater("0.1.0", &url, 1);
    assert!(again.check(&KeyRecord::Unavailable).is_err());
    rig.assert_untouched("replay and rollback");

    // Expiry.
    let rig2 = Rig::new();
    Release::new("0.2.0", 10, healthy("0.2.0")).publish(&server);
    let late = rig2.updater_with(
        "0.1.0",
        &url,
        1,
        NOW + 100_001,
        Arc::new(LinuxInstaller::at(rig2.install.clone())),
    );
    assert!(
        late.check(&KeyRecord::Unavailable)
            .unwrap_err()
            .contains("expired")
    );
    rig2.assert_untouched("expired");
    // A manifest dated far ahead of this clock.
    let early = rig2.updater_with(
        "0.1.0",
        &url,
        1,
        NOW - 10 * 86_400,
        Arc::new(LinuxInstaller::at(rig2.install.clone())),
    );
    assert!(
        early
            .check(&KeyRecord::Unavailable)
            .unwrap_err()
            .contains("future")
    );
    rig2.assert_untouched("from the future");
}

#[test]
fn a_staged_update_is_verified_again_before_it_is_applied() {
    let ports = Ports::lock();
    let server = ports.serve(vec![]);
    Release::new("0.2.0", 10, healthy("0.2.0")).publish(&server);
    let url = server.url("/manifest.json");

    // The artifact changed on disk after it was downloaded.
    let rig = Rig::new();
    let u = rig.updater("0.1.0", &url, 1);
    u.check(&KeyRecord::Unavailable).unwrap();
    u.download(&mut |_, _| {}).unwrap();
    let staged = state::load(rig.data.path()).staged.unwrap();
    let mut bytes = fs::read(&staged.artifact).unwrap();
    bytes[10] ^= 1;
    fs::write(&staged.artifact, bytes).unwrap();
    assert!(u.apply().unwrap_err().contains("signed checksum"));
    rig.assert_untouched("artifact changed on disk");

    // The stored manifest was swapped for one signed by another key.
    let rig = Rig::new();
    let u = rig.updater("0.1.0", &url, 1);
    u.check(&KeyRecord::Unavailable).unwrap();
    u.download(&mut |_, _| {}).unwrap();
    let staged = state::load(rig.data.path()).staged.unwrap();
    let mut sig = fs::read(&staged.signature).unwrap();
    sig[3] ^= 1;
    fs::write(&staged.signature, sig).unwrap();
    assert!(u.apply().unwrap_err().contains("signature"));
    rig.assert_untouched("signature changed on disk");

    // A newer River than the staged one already runs.
    let rig = Rig::new();
    let u = rig.updater("0.1.0", &url, 1);
    u.check(&KeyRecord::Unavailable).unwrap();
    u.download(&mut |_, _| {}).unwrap();
    let running_newer = rig.updater("0.2.0", &url, 1);
    running_newer.remember_install();
    assert!(
        running_newer
            .apply()
            .unwrap_err()
            .contains("no longer newer")
    );
    rig.assert_untouched("already current");
}

#[test]
fn a_copy_managed_by_something_else_is_never_replaced() {
    let ports = Ports::lock();
    let server = ports.serve(vec![]);
    Release::new("0.2.0", 10, healthy("0.2.0")).publish(&server);
    let rig = Rig::new();
    // The binary "lives" in a system directory.
    let u = rig.updater_with(
        "0.1.0",
        &server.url("/manifest.json"),
        1,
        NOW,
        Arc::new(LinuxInstaller::at(PathBuf::from("/usr/bin/river"))),
    );
    assert!(matches!(u.support(), Support::External(_)));
    // It can still tell the person an update exists.
    offer(u.check(&KeyRecord::Unavailable).unwrap());
    let err = u.download(&mut |_, _| {}).unwrap_err();
    assert!(err.contains("package manager"), "{err}");
    assert!(u.apply().is_err());
    rig.assert_untouched("external");
    assert_eq!(
        server.hits("/river-0.2.0.tar.gz"),
        0,
        "nothing was even downloaded"
    );
}

#[test]
fn a_binary_started_by_hand_that_never_confirms_rolls_itself_back() {
    let rig = Rig::new();
    // Set up a swap on probation by hand: previous kept, new installed.
    let previous = rig.data.path().join("update/previous/river-0.1.0");
    fs::create_dir_all(previous.parent().unwrap()).unwrap();
    fs::write(&previous, OLD).unwrap();
    fs::write(&rig.install, healthy("0.2.0")).unwrap();
    let st = state::State {
        probation: Some(state::Probation {
            from: "0.1.0".into(),
            to: "0.2.0".into(),
            install_path: rig.install.clone(),
            previous: previous.clone(),
            previous_sha256: hex::encode(Sha256::digest(OLD)),
            starts: 0,
        }),
        ..Default::default()
    };
    state::save(rig.data.path(), &st).unwrap();
    let new = rig.updater("0.2.0", "http://127.0.0.1:1/x", 1);

    for n in 1..=MAX_UNCONFIRMED_STARTS {
        assert_eq!(new.on_start(false), None, "start {n} is allowed");
        assert_eq!(state::load(rig.data.path()).probation.unwrap().starts, n);
    }
    assert_eq!(rig.installed(), healthy("0.2.0"));
    // One start too many.
    assert_eq!(new.on_start(false), Some(rig.install.clone()));
    assert_eq!(rig.installed(), OLD, "the previous version is back");
    assert!(state::load(rig.data.path()).probation.is_none());
    assert!(new.take_notice().unwrap().contains("kept failing to start"));
}

#[test]
fn stale_probation_records_are_cleared() {
    let rig = Rig::new();
    let st = state::State {
        probation: Some(state::Probation {
            from: "0.1.0".into(),
            to: "0.2.0".into(),
            install_path: rig.install.clone(),
            previous: rig.install.clone(),
            previous_sha256: String::new(),
            starts: 5,
        }),
        ..Default::default()
    };
    state::save(rig.data.path(), &st).unwrap();
    // The old binary running after a manual restore must not touch anything.
    let old = rig.updater("0.1.0", "http://127.0.0.1:1/x", 1);
    assert_eq!(old.on_start(false), None);
    assert!(state::load(rig.data.path()).probation.is_none());
    assert_eq!(rig.installed(), OLD);
}

#[test]
fn the_handshake_is_only_written_inside_the_update_folder() {
    let rig = Rig::new();
    let u = rig.updater("0.2.0", "http://127.0.0.1:1/x", 1);
    let outside = rig.bin.path().join("pwned");
    u.confirm_started(Some(&outside));
    assert!(!outside.exists());
    let traversal = rig.data.path().join("update/../pwned");
    u.confirm_started(Some(&traversal));
    assert!(!rig.data.path().join("pwned").exists());
}

#[test]
fn two_applies_cannot_run_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("apply.lock");
    let first = Lock::take(&path).unwrap();
    assert!(Lock::take(&path).unwrap_err().contains("another River"));
    drop(first);
    assert!(Lock::take(&path).is_ok());
}

#[test]
fn the_placeholder_key_is_recognised_and_trusts_nothing() {
    assert!(is_placeholder(&PLACEHOLDER_PUBKEY));
    assert!(!is_placeholder(&pubkey(1)));
    // Built without an override, the key in this binary is the placeholder.
    if option_env!("RIVER_UPDATE_PUBKEY_HEX").is_none() {
        assert!(is_placeholder(&super::pubkey()));
    }
}

/// River installs and starts the node without turning off the node's own auto-update, which
/// is what keeps a River-installed node current. Nothing in the code that installs or starts
/// it may pass the flag.
#[test]
fn river_never_turns_off_the_nodes_auto_update() {
    for (name, source) in [
        ("install.rs", include_str!("../install.rs")),
        ("host.rs", include_str!("../host.rs")),
        ("release.rs", include_str!("../release.rs")),
    ] {
        // Source files checked out on Windows have CRLF line ends.
        let code: String = source
            .replace("\r\n", "\n")
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for flag in ["disable-auto-update", "disable_auto_update"] {
            assert!(!code.contains(flag), "{name} passes {flag} to the node");
        }
    }
}

fn stage_and_apply(u: &Updater) -> Applied {
    offer(u.check(&KeyRecord::Unavailable).unwrap());
    u.download(&mut |_, _| {}).unwrap();
    u.apply().unwrap()
}

#[test]
fn a_resigned_same_version_is_restaged_instead_of_wedging() {
    let ports = Ports::lock();
    let server = ports.serve(vec![]);
    let url = server.url("/manifest.json");
    let rig = Rig::new();
    let u = rig.updater("0.1.0", &url, 1);
    Release::new("0.2.0", 10, healthy("0.2.0")).publish(&server);
    offer(u.check(&KeyRecord::Unavailable).unwrap());
    u.download(&mut |_, _| {}).unwrap();
    // The same version is signed again with a newer sequence before anyone pressed Restart.
    Release::new("0.2.0", 11, healthy("0.2.0")).publish(&server);
    offer(u.check(&KeyRecord::Unavailable).unwrap());
    u.download(&mut |_, _| {}).unwrap();
    assert_eq!(
        server.hits("/river-0.2.0.tar.gz"),
        2,
        "the stale staged copy was not reused"
    );
    let applied = u.apply().expect("applies after the re-sign");
    assert_eq!(rig.installed(), healthy("0.2.0"));
    drop(applied);
    // A check that finds the same manifest again reuses what is staged.
    let rig = Rig::new();
    let u = rig.updater("0.1.0", &url, 1);
    offer(u.check(&KeyRecord::Unavailable).unwrap());
    u.download(&mut |_, _| {}).unwrap();
    offer(u.check(&KeyRecord::Unavailable).unwrap());
    u.download(&mut |_, _| {}).unwrap();
    assert_eq!(
        server.hits("/river-0.2.0.tar.gz"),
        3,
        "an unchanged staged copy is reused"
    );
}

#[test]
fn a_staged_manifest_that_expired_before_the_press_recovers() {
    let ports = Ports::lock();
    let server = ports.serve(vec![]);
    let url = server.url("/manifest.json");
    let rig = Rig::new();
    let mut short = Release::new("0.2.0", 10, healthy("0.2.0"));
    short.expires_at = NOW + 1000;
    short.publish(&server);
    let u = rig.updater("0.1.0", &url, 1);
    offer(u.check(&KeyRecord::Unavailable).unwrap());
    u.download(&mut |_, _| {}).unwrap();
    // Later, the staged manifest has expired. Pressing Restart fails once and clears it.
    let later = NOW + 2000;
    let u2 = rig.updater_with(
        "0.1.0",
        &url,
        1,
        later,
        Arc::new(LinuxInstaller::at(rig.install.clone())),
    );
    assert_eq!(
        u2.staged_version(),
        None,
        "an expired staged update is not offered as ready"
    );
    assert!(u2.apply().unwrap_err().contains("expired"));
    assert!(state::load(rig.data.path()).staged.is_none());
    assert!(!rig.data.path().join("update/staged").exists());
    rig.assert_untouched("expired staged update");
    // The publisher signs a fresh manifest; the next check stages and applies it.
    Release::new("0.2.0", 11, healthy("0.2.0")).publish(&server);
    offer(u2.check(&KeyRecord::Unavailable).unwrap());
    u2.download(&mut |_, _| {}).unwrap();
    u2.apply().unwrap();
    assert_eq!(rig.installed(), healthy("0.2.0"));
}

#[test]
fn a_superseded_staged_update_is_cleared_when_it_fails_to_apply() {
    let ports = Ports::lock();
    let server = ports.serve(vec![]);
    let url = server.url("/manifest.json");
    let rig = Rig::new();
    Release::new("0.2.0", 10, healthy("0.2.0")).publish(&server);
    let u = rig.updater("0.1.0", &url, 1);
    offer(u.check(&KeyRecord::Unavailable).unwrap());
    u.download(&mut |_, _| {}).unwrap();
    // A newer signing is accepted by a check that never downloads it.
    Release::new("0.3.0", 12, healthy("0.3.0")).publish(&server);
    offer(u.check(&KeyRecord::Unavailable).unwrap());
    let err = u.apply().unwrap_err();
    assert!(err.contains("older than one already seen"), "{err}");
    assert!(state::load(rig.data.path()).staged.is_none());
    // And the newer one goes through.
    u.download(&mut |_, _| {}).unwrap();
    u.apply().unwrap();
    assert_eq!(rig.installed(), healthy("0.3.0"));
}

#[test]
fn a_signature_that_lags_the_manifest_is_fetched_again_once() {
    let ports = Ports::lock();
    let server = ports.serve(vec![]);
    let rig = Rig::new();
    let r = Release::new("0.2.0", 10, healthy("0.2.0"));
    r.publish(&server);
    let good = r.manifest(&server, &r.artifact()).canonical_bytes();
    let good_sig = key(1).sign(&signed_bytes(&good)).to_bytes().to_vec();
    // The first signature fetch is the old one (another key), the second is right.
    server.set(
        "/manifest.json.sig",
        Reply::Then(
            Box::new(Reply::Body(vec![9; 64])),
            Box::new(Reply::Body(good_sig)),
        ),
    );
    let u = rig.updater("0.1.0", &server.url("/manifest.json"), 1);
    offer(
        u.check(&KeyRecord::Unavailable)
            .expect("one retry covers CDN skew"),
    );
    assert_eq!(server.hits("/manifest.json.sig"), 2);
    // A signature that stays wrong is still refused, after one retry only.
    let rig = Rig::new();
    server.set("/manifest.json.sig", Reply::Body(vec![9; 64]));
    let before = server.hits("/manifest.json.sig");
    assert!(
        rig.updater("0.1.0", &server.url("/manifest.json"), 1)
            .check(&KeyRecord::Unavailable)
            .unwrap_err()
            .contains("signature")
    );
    assert_eq!(server.hits("/manifest.json.sig") - before, 2);
}

#[test]
fn an_unconfigured_build_refuses_to_update() {
    let ports = Ports::lock();
    let server = ports.serve(vec![]);
    Release::new("0.2.0", 10, healthy("0.2.0")).publish(&server);
    let rig = Rig::new();
    let placeholder = |rig: &Rig| {
        let mut u = rig.updater("0.1.0", &server.url("/manifest.json"), 1);
        u.cfg.pubkey = PLACEHOLDER_PUBKEY;
        u
    };
    let u = placeholder(&rig);
    assert!(
        u.check(&KeyRecord::Unavailable)
            .unwrap_err()
            .contains("no update key")
    );
    assert!(u.download(&mut |_, _| {}).is_err());
    assert!(u.apply().is_err());
    assert_eq!(server.hits("/manifest.json"), 0, "it did not even ask");
    rig.assert_untouched("placeholder key");
    // Through the app's host too.
    use river_app::{UpdateHost, UpdateSupport};
    let host = host::DesktopUpdate::with_parts(placeholder(&rig), PLACEHOLDER_PUBKEY);
    assert!(host.check(KeyRecord::Unavailable).is_err());
    assert!(matches!(host.support(), UpdateSupport::Unavailable(_)));
    assert!(host.restart().is_err());
}

#[test]
fn a_damaged_state_file_stops_updating_and_changes_nothing() {
    let ports = Ports::lock();
    let server = ports.serve(vec![]);
    Release::new("0.2.0", 10, healthy("0.2.0")).publish(&server);
    let rig = Rig::new();
    fs::write(state::path(rig.data.path()), b"{ truncated").unwrap();
    let u = rig.updater("0.1.0", &server.url("/manifest.json"), 1);
    let err = u.check(&KeyRecord::Unavailable).unwrap_err();
    assert!(err.contains("damaged"), "{err}");
    assert!(u.download(&mut |_, _| {}).is_err());
    assert!(u.apply().is_err());
    assert!(rig.data.path().join("update-state.json.bak").exists());
    assert_eq!(
        fs::read(state::path(rig.data.path())).unwrap(),
        b"{ truncated"
    );
    assert_eq!(rig.installed(), OLD);
}

#[test]
fn a_build_floor_refuses_old_manifests_on_a_fresh_install() {
    let ports = Ports::lock();
    let server = ports.serve(vec![]);
    Release::new("0.2.0", 10, healthy("0.2.0")).publish(&server);
    let rig = Rig::new();
    let mut u = rig.updater("0.1.0", &server.url("/manifest.json"), 1);
    u.cfg.floor = Trust {
        highest_sequence: 100,
        highest_version: None,
    };
    assert!(
        u.check(&KeyRecord::Unavailable)
            .unwrap_err()
            .contains("older than one already seen")
    );
    u.cfg.floor = Trust {
        highest_sequence: 0,
        highest_version: Some(Version(0, 3, 0)),
    };
    assert!(
        u.check(&KeyRecord::Unavailable)
            .unwrap_err()
            .contains("already offered")
    );
    rig.assert_untouched("build floor");
}

#[test]
fn an_install_that_changed_under_a_running_river_is_not_replaced() {
    let ports = Ports::lock();
    let server = ports.serve(vec![]);
    Release::new("0.2.0", 10, healthy("0.2.0")).publish(&server);
    let rig = Rig::new();
    let u = rig.updater("0.1.0", &server.url("/manifest.json"), 1);
    offer(u.check(&KeyRecord::Unavailable).unwrap());
    u.download(&mut |_, _| {}).unwrap();
    // A package manager (or another River) replaces the file after this River started.
    fs::write(&rig.install, b"#!/bin/sh\necho river 0.1.5\n").unwrap();
    let err = u.apply().unwrap_err();
    assert!(err.contains("changed while"), "{err}");
    assert_eq!(rig.installed(), b"#!/bin/sh\necho river 0.1.5\n");
    assert!(!rig.data.path().join("update/previous").exists());
    assert!(state::load(rig.data.path()).probation.is_none());
}

#[test]
fn the_apply_lock_is_held_until_the_launch_is_over() {
    let ports = Ports::lock();
    let server = ports.serve(vec![]);
    Release::new("0.2.0", 10, healthy("0.2.0")).publish(&server);
    let rig = Rig::new();
    let u = rig.updater("0.1.0", &server.url("/manifest.json"), 1);
    let applied = stage_and_apply(&u);
    let other = rig.updater("0.1.0", &server.url("/manifest.json"), 1);
    assert!(other.apply().unwrap_err().contains("another River"));
    drop(applied);
    assert!(Lock::take(&rig.data.path().join("update/apply.lock")).is_ok());
}

#[test]
fn a_failed_rollback_starts_nothing_and_leaves_a_notice() {
    let rig = Rig::new();
    let previous = rig.data.path().join("update/previous/river-0.1.0");
    fs::create_dir_all(previous.parent().unwrap()).unwrap();
    fs::write(&previous, b"someone tampered with the kept copy").unwrap();
    fs::write(&rig.install, healthy("0.2.0")).unwrap();
    let st = state::State {
        probation: Some(state::Probation {
            from: "0.1.0".into(),
            to: "0.2.0".into(),
            install_path: rig.install.clone(),
            previous: previous.clone(),
            previous_sha256: hex::encode(Sha256::digest(OLD)),
            starts: MAX_UNCONFIRMED_STARTS,
        }),
        ..Default::default()
    };
    state::save(rig.data.path(), &st).unwrap();
    let new = rig.updater("0.2.0", "http://127.0.0.1:1/x", 1);
    assert_eq!(
        new.on_start(false),
        None,
        "the unrestored binary is not started over"
    );
    assert_eq!(rig.installed(), healthy("0.2.0"));
    let after = state::load(rig.data.path());
    assert!(after.probation.is_some(), "the probation record is kept");
    assert!(new.take_notice().unwrap().contains("could not be restored"));
}

#[test]
fn a_clean_exit_resets_the_count_so_a_good_binary_is_never_rolled_back() {
    let rig = Rig::new();
    let previous = rig.data.path().join("update/previous/river-0.1.0");
    fs::create_dir_all(previous.parent().unwrap()).unwrap();
    fs::write(&previous, OLD).unwrap();
    fs::write(&rig.install, healthy("0.2.0")).unwrap();
    let st = state::State {
        probation: Some(state::Probation {
            from: "0.1.0".into(),
            to: "0.2.0".into(),
            install_path: rig.install.clone(),
            previous,
            previous_sha256: hex::encode(Sha256::digest(OLD)),
            starts: 0,
        }),
        ..Default::default()
    };
    state::save(rig.data.path(), &st).unwrap();
    let new = rig.updater("0.2.0", "http://127.0.0.1:1/x", 1);
    for _ in 0..10 {
        assert_eq!(new.on_start(false), None);
        new.clean_exit(Duration::from_secs(6));
    }
    assert_eq!(rig.installed(), healthy("0.2.0"));
}

#[test]
fn old_leftovers_are_swept_and_young_ones_kept() {
    let ports = Ports::lock();
    let server = ports.serve(vec![]);
    Release::new("0.1.0", 10, healthy("0.1.0")).publish(&server);
    let rig = Rig::new();
    let old = rig.bin.path().join(".river-update-abc.new");
    let young = rig.bin.path().join(".river-update-def.new");
    let handshake = rig.data.path().join("update/handshake-1-2");
    fs::create_dir_all(handshake.parent().unwrap()).unwrap();
    for p in [&old, &young, &handshake] {
        fs::write(p, b"x").unwrap();
    }
    let two_hours = std::time::SystemTime::now() - Duration::from_secs(2 * 3600);
    for p in [&old, &handshake] {
        fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(two_hours)
            .unwrap();
    }
    rig.updater("0.1.0", &server.url("/manifest.json"), 1)
        .check(&KeyRecord::Unavailable)
        .unwrap();
    assert!(!old.exists() && !handshake.exists());
    assert!(young.exists());
}

#[test]
fn a_run_that_ends_at_once_still_counts_as_a_failed_start() {
    let rig = Rig::new();
    let previous = rig.data.path().join("update/previous/river-0.1.0");
    fs::create_dir_all(previous.parent().unwrap()).unwrap();
    fs::write(&previous, OLD).unwrap();
    fs::write(&rig.install, healthy("0.2.0")).unwrap();
    let st = state::State {
        probation: Some(state::Probation {
            from: "0.1.0".into(),
            to: "0.2.0".into(),
            install_path: rig.install.clone(),
            previous,
            previous_sha256: hex::encode(Sha256::digest(OLD)),
            starts: 0,
        }),
        ..Default::default()
    };
    state::save(rig.data.path(), &st).unwrap();
    let new = rig.updater("0.2.0", "http://127.0.0.1:1/x", 1);
    for _ in 0..MAX_UNCONFIRMED_STARTS {
        assert_eq!(new.on_start(false), None);
        new.clean_exit(Duration::from_secs(1));
    }
    assert_eq!(
        new.on_start(false),
        Some(rig.install.clone()),
        "a window that returns at once is a bad binary"
    );
    assert_eq!(rig.installed(), OLD);
}

/// Sets up a swap that has happened: the new binary installed, the old one kept.
fn swapped(rig: &Rig, starts: u32) -> state::Probation {
    let previous = rig.data.path().join("update/previous/river-0.1.0");
    fs::create_dir_all(previous.parent().unwrap()).unwrap();
    fs::write(&previous, OLD).unwrap();
    fs::write(&rig.install, healthy("0.2.0")).unwrap();
    let p = state::Probation {
        from: "0.1.0".into(),
        to: "0.2.0".into(),
        install_path: rig.install.clone(),
        previous,
        previous_sha256: hex::encode(Sha256::digest(OLD)),
        starts,
    };
    state::save(
        rig.data.path(),
        &state::State {
            probation: Some(p.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    p
}

#[test]
fn a_copy_started_by_hand_from_previous_does_not_clear_a_swap_on_probation() {
    let rig = Rig::new();
    let p = swapped(&rig, 0);
    // The old binary, run from its kept copy: a different path from the install.
    let elsewhere = rig.updater_with(
        "0.1.0",
        "http://127.0.0.1:1/x",
        1,
        NOW,
        Arc::new(LinuxInstaller::at(p.previous.clone())),
    );
    assert_eq!(elsewhere.on_start(false), None);
    assert!(
        state::load(rig.data.path()).probation.is_some(),
        "the swap is still on probation"
    );
    // The installed binary running as another version does clear a stale record.
    let installed_old = rig.updater("0.1.0", "http://127.0.0.1:1/x", 1);
    assert_eq!(installed_old.on_start(false), None);
    assert!(state::load(rig.data.path()).probation.is_none());
}

#[test]
fn rollback_restores_even_when_the_state_file_is_damaged() {
    let ports = Ports::lock();
    let server = ports.serve(vec![]);
    Release::new("0.2.0", 10, crashes("0.2.0")).publish(&server);
    let rig = Rig::new();
    let u = rig.updater("0.1.0", &server.url("/manifest.json"), 1);
    let applied = stage_and_apply(&u);
    assert_eq!(rig.installed(), crashes("0.2.0"));
    fs::write(state::path(rig.data.path()), b"{ damaged").unwrap();
    let err = u.launch(&applied, &[]).unwrap_err();
    assert!(err.contains("went back to 0.1.0"), "{err}");
    assert_eq!(
        rig.installed(),
        OLD,
        "the bad binary does not stay installed"
    );
}

#[test]
fn a_failed_rollback_is_retried_by_the_next_apply_in_the_old_process() {
    let rig = Rig::new();
    let p = swapped(&rig, 0);
    let good_previous = fs::read(&p.previous).unwrap();
    fs::write(&p.previous, b"damaged copy").unwrap();
    let u = rig.updater("0.1.0", "http://127.0.0.1:1/x", 1);
    u.remember_install();
    // The restore fails; the record stays and an apply says so rather than swapping on top.
    let (message, restored) = u.restore_probation(&p, "test");
    assert!(!restored, "{message}");
    assert!(u.apply().unwrap_err().contains("could not be"));
    assert!(state::load(rig.data.path()).probation.is_some());
    assert_eq!(rig.installed(), healthy("0.2.0"));
    // Once the kept copy is whole again the next apply puts the old binary back.
    fs::write(&p.previous, good_previous).unwrap();
    let err = u.apply().unwrap_err();
    assert!(err.contains("no downloaded update"), "{err}");
    assert_eq!(rig.installed(), OLD);
    assert!(state::load(rig.data.path()).probation.is_none());
}

#[test]
fn a_swap_is_refused_while_the_installed_hash_is_unknown() {
    let ports = Ports::lock();
    let server = ports.serve(vec![]);
    Release::new("0.2.0", 10, healthy("0.2.0")).publish(&server);
    let rig = Rig::new();
    let u = rig.updater("0.1.0", &server.url("/manifest.json"), 1);
    offer(u.check(&KeyRecord::Unavailable).unwrap());
    u.download(&mut |_, _| {}).unwrap();
    // A fresh updater (a new run) that has not yet recorded the install.
    let fresh = rig.updater("0.1.0", &server.url("/manifest.json"), 1);
    assert!(fresh.apply().unwrap_err().contains("not finished noting"));
    rig.assert_untouched("unknown install hash");
}

#[test]
fn download_waits_for_an_apply_in_progress() {
    let ports = Ports::lock();
    let server = ports.serve(vec![]);
    Release::new("0.2.0", 10, healthy("0.2.0")).publish(&server);
    let rig = Rig::new();
    let u = rig.updater("0.1.0", &server.url("/manifest.json"), 1);
    offer(u.check(&KeyRecord::Unavailable).unwrap());
    let held = Lock::take(&rig.data.path().join("update/apply.lock")).unwrap();
    assert!(
        u.download(&mut |_, _| {})
            .unwrap_err()
            .contains("another River")
    );
    drop(held);
    u.download(&mut |_, _| {}).unwrap();
}

#[test]
fn a_restart_lets_go_of_the_lock_so_the_next_update_can_start() {
    let ports = Ports::lock();
    let server = ports.serve(vec![]);
    Release::new("0.2.0", 10, healthy("0.2.0")).publish(&server);
    let rig = Rig::new();
    let u = rig.updater("0.1.0", &server.url("/manifest.json"), 1);
    offer(u.check(&KeyRecord::Unavailable).unwrap());
    u.download(&mut |_, _| {}).unwrap();
    let host = host::DesktopUpdate::with_parts(u, pubkey(1));
    host.restart_without_exiting(&[]).unwrap();
    assert!(
        Lock::take(&rig.data.path().join("update/apply.lock")).is_ok(),
        "the lock was released"
    );
}

#[test]
fn a_launch_that_only_shows_the_other_river_releases_the_updater_and_is_not_counted() {
    let ports = Ports::lock();
    let server = ports.serve(vec![]);
    Release::new("0.2.0", 10, healthy("0.2.0")).publish(&server);
    let rig = Rig::new();
    let old = rig.updater("0.1.0", &server.url("/manifest.json"), 1);
    old.check(&KeyRecord::Unavailable).unwrap();
    old.download(&mut |_, _| {}).unwrap();
    old.apply().unwrap();
    let starts = || state::load(rig.data.path()).probation.unwrap().starts;
    assert_eq!(starts(), 0);

    // The new binary starts while a River already holds the instance lock (a GUI open when
    // `river --self-update` ran): its launch reaches that River and then leaves.
    let config = tempfile::tempdir().unwrap();
    let running = match crate::single::acquire(config.path(), None).unwrap() {
        crate::single::Start::First(g) => g,
        _ => panic!("expected to be first"),
    };
    running.serve(|_| {}).unwrap();
    assert!(matches!(
        crate::single::acquire(config.path(), None).unwrap(),
        crate::single::Start::Running { asked: true }
    ));
    let new = host::DesktopUpdate::with_parts(
        rig.updater("0.2.0", &server.url("/manifest.json"), 1),
        pubkey(1),
    );

    // Run by hand (no handshake): nothing is confirmed and nothing is counted.
    new.second_launch(None);
    assert_eq!(starts(), 0, "a second launch is not an unconfirmed start");
    assert!(state::load(rig.data.path()).probation.is_some());

    // Run by an updater: the waiting launcher is released, but probation stays for a start
    // that really ran.
    let marker = rig.data.path().join("update/handshake-second");
    new.second_launch(Some(&marker));
    assert_eq!(fs::read_to_string(&marker).unwrap(), "0.2.0");
    assert!(state::load(rig.data.path()).probation.is_some());
    assert_eq!(starts(), 0);
}

/// `main` takes the instance lock before the updater counts the start, and the restart hands
/// the lock over between `apply` and `launch` and takes it back on failure. These orders are
/// what keep a second launch from counting against a good update and a failed restart from
/// leaving two Rivers, and nothing else would notice them move.
#[test]
fn the_instance_lock_is_taken_before_the_start_is_counted_and_handed_over_around_launch() {
    let main = include_str!("../main.rs").replace("\r\n", "\n");
    let acquire = main.find("single::acquire(").unwrap();
    let counted = main.find("u.on_start()").unwrap();
    assert!(acquire < counted, "acquire the lock, then count the start");

    let host = include_str!("host.rs").replace("\r\n", "\n");
    let f = &host[host.find("fn restart_and_release").unwrap()..];
    let f = &f[..f.find("\n    }\n").unwrap()];
    let apply = f.find("self.updater.apply()").unwrap();
    let handover = f.find("hand_over_instance(").unwrap();
    let launch = f.find("self.updater.launch(").unwrap();
    assert!(apply < handover && handover < launch);

    let h = &host[host.find("fn hand_over_instance").unwrap()..];
    let h = &h[..h.find("\n}\n").unwrap()];
    let release = h.find("release_current()").unwrap();
    let start = h.find("start()").unwrap();
    let back = h.find("reacquire_current()").unwrap();
    assert!(release < start && start < back);
    assert!(h[back - 40..back].contains("is_err"), "only on failure");
}

#[test]
fn a_restart_keeps_the_arguments_but_not_the_tray_only_flag() {
    let args = ["--node", "h:1", "--background", "x"].map(std::ffi::OsString::from);
    assert_eq!(
        host::restart_args(args.into_iter()),
        ["--node", "h:1", "x"].map(std::ffi::OsString::from)
    );
}

#[test]
fn a_state_file_from_a_newer_build_is_read_but_never_written_over() {
    let ports = Ports::lock();
    let server = ports.serve(vec![]);
    Release::new("0.2.0", 10, healthy("0.2.0")).publish(&server);
    let rig = Rig::new();
    let newer = br#"{"schema":99,"highest_sequence":5,"some_future_field":{"a":1}}"#;
    fs::write(state::path(rig.data.path()), newer).unwrap();
    let u = rig.updater("0.1.0", &server.url("/manifest.json"), 1);
    offer(
        u.check(&KeyRecord::Unavailable)
            .expect("a newer file is not a damaged one"),
    );
    assert_eq!(
        fs::read(state::path(rig.data.path())).unwrap(),
        newer,
        "left as the newer build wrote it"
    );
    assert!(!rig.data.path().join("update-state.json.bak").exists());
}

/// Keep clippy honest about helpers only some tests use.
#[allow(dead_code)]
fn _unused(_: &Path, _: &SigningKey) {}

// ---- the update-key record (freenet/river#927) ----

mod key_record {
    use super::*;
    use crate::update::keyrecord::tests::{authority, key_of, record};

    /// A rig whose build trusts key 1 and reads the record.
    fn rotating(rig: &Rig, server: &TestServer) -> Updater {
        let mut u = rig.updater("0.1.0", &server.url("/manifest.json"), 1);
        u.cfg.key_authority = Some(authority());
        u
    }

    fn release_signed_by(server: &TestServer, signer: u8) {
        let mut r = Release::new("0.2.0", 10, healthy("0.2.0"));
        r.signer = signer;
        r.publish(server);
    }

    fn check(u: &Updater, fetched: Vec<u8>) -> Result<CheckResult, String> {
        u.check(&KeyRecord::State(fetched))
    }

    fn offered(r: Result<CheckResult, String>) {
        offer(r.unwrap());
    }

    #[test]
    fn no_record_trusts_the_compiled_key() {
        let ports = Ports::lock();
        let server = ports.serve(vec![]);
        release_signed_by(&server, 1);
        let rig = Rig::new();
        let u = rotating(&rig, &server);
        offered(u.check(&KeyRecord::Absent));
        offered(u.check(&KeyRecord::Unavailable));
        release_signed_by(&server, 2);
        assert!(u.check(&KeyRecord::Absent).is_err());
    }

    #[test]
    fn a_record_replaces_the_compiled_key() {
        let ports = Ports::lock();
        let server = ports.serve(vec![]);
        let rig = Rig::new();
        let u = rotating(&rig, &server);
        release_signed_by(&server, 1);
        // A manifest the old key signed is refused once the record names another.
        assert!(check(&u, record(1, key_of(2))).is_err());
        release_signed_by(&server, 2);
        offered(u.check(&KeyRecord::Unavailable));
    }

    #[test]
    fn a_higher_record_rotates_again() {
        let ports = Ports::lock();
        let server = ports.serve(vec![]);
        let rig = Rig::new();
        let u = rotating(&rig, &server);
        release_signed_by(&server, 2);
        offered(check(&u, record(1, key_of(2))));
        release_signed_by(&server, 3);
        assert!(u.check(&KeyRecord::Unavailable).is_err());
        offered(check(&u, record(2, key_of(3))));
        release_signed_by(&server, 2);
        assert!(u.check(&KeyRecord::Unavailable).is_err());
    }

    #[test]
    fn a_stale_record_does_not_roll_the_key_back() {
        let ports = Ports::lock();
        let server = ports.serve(vec![]);
        let rig = Rig::new();
        let u = rotating(&rig, &server);
        release_signed_by(&server, 3);
        offered(check(&u, record(2, key_of(3))));
        // A node serving the older record keeps it.
        offered(check(&u, record(1, key_of(2))));
        release_signed_by(&server, 2);
        assert!(check(&u, record(1, key_of(2))).is_err());
        release_signed_by(&server, 3);
        offered(u.check(&KeyRecord::Unavailable));
    }

    #[test]
    fn a_record_from_another_signer_or_app_is_ignored() {
        use crate::update::keyrecord::tests::{APP_ID, authority_key, record_for};
        let ports = Ports::lock();
        let server = ports.serve(vec![]);
        let rig = Rig::new();
        let u = rotating(&rig, &server);
        let stranger = ed25519_dalek::SigningKey::from_bytes(&[9; 32]);
        release_signed_by(&server, 2);
        for bad in [
            record_for(&stranger, APP_ID, 5, key_of(2)),
            record_for(&authority_key(), "river.chat-delegate", 5, key_of(2)),
        ] {
            assert!(check(&u, bad).is_err(), "key 2 must not be trusted");
        }
        release_signed_by(&server, 1);
        offered(u.check(&KeyRecord::Unavailable));
    }

    #[test]
    fn a_tombstone_refuses_every_update_and_a_higher_record_lifts_it() {
        let ports = Ports::lock();
        let server = ports.serve(vec![]);
        let rig = Rig::new();
        let u = rotating(&rig, &server);
        release_signed_by(&server, 1);
        let err = check(&u, record(1, [0; 32])).unwrap_err();
        assert!(err.contains("withdrawn"), "{err}");
        // Not the old key, not a node that goes quiet, not a replay of the record before it.
        for fetched in [
            KeyRecord::Unavailable,
            KeyRecord::Absent,
            KeyRecord::State(record(1, key_of(1))),
        ] {
            let err = u.check(&fetched).unwrap_err();
            assert!(err.contains("withdrawn"), "{err}");
        }
        assert!(u.download(&mut |_, _| {}).is_err());
        release_signed_by(&server, 6);
        offered(check(&u, record(2, key_of(6))));
    }

    #[test]
    fn an_unreachable_node_keeps_the_key_the_last_record_named() {
        let ports = Ports::lock();
        let server = ports.serve(vec![]);
        let rig = Rig::new();
        release_signed_by(&server, 2);
        offered(check(&rotating(&rig, &server), record(1, key_of(2))));
        // A later run, no node: the record is on disk.
        let later = rotating(&rig, &server);
        offered(later.check(&KeyRecord::Unavailable));
        release_signed_by(&server, 1);
        assert!(
            later.check(&KeyRecord::Unavailable).is_err(),
            "never the compiled key"
        );
        assert!(later.check(&KeyRecord::Absent).is_err());
    }

    #[test]
    fn a_state_file_someone_else_wrote_does_not_change_the_key() {
        use crate::update::keyrecord::tests::{APP_ID, record_for};
        let ports = Ports::lock();
        let server = ports.serve(vec![]);
        let rig = Rig::new();
        let stranger = ed25519_dalek::SigningKey::from_bytes(&[9; 32]);
        let forged = hex::encode(record_for(&stranger, APP_ID, 1, key_of(2)));
        let mut flipped = record(1, key_of(2));
        flipped[40] ^= 1;
        for kept in [forged, hex::encode(flipped), "zz".to_string()] {
            state::modify(rig.data.path(), |st| st.key_record = Some(kept)).unwrap();
            let u = rotating(&rig, &server);
            release_signed_by(&server, 2);
            assert!(
                u.check(&KeyRecord::Unavailable).is_err(),
                "key 2 not trusted"
            );
            release_signed_by(&server, 1);
            offered(u.check(&KeyRecord::Unavailable));
        }
    }

    #[test]
    fn a_build_staged_under_a_key_revoked_since_is_refused_at_apply() {
        let ports = Ports::lock();
        let server = ports.serve(vec![]);
        let rig = Rig::new();
        let u = rotating(&rig, &server);
        release_signed_by(&server, 1);
        offered(u.check(&KeyRecord::Unavailable));
        u.download(&mut |_, _| {}).unwrap();
        assert_eq!(u.staged_version(), Some(Version(0, 2, 0)));
        // The key is replaced (this check then fails: the server still offers the old build).
        assert!(check(&u, record(1, key_of(2))).is_err());
        assert_eq!(u.staged_version(), None, "no longer offered as ready");
        assert!(u.apply().is_err(), "refused at restart");
        rig.assert_untouched("a revoked key's build");
        assert!(state::load(rig.data.path()).staged.is_none(), "discarded");
    }

    #[test]
    fn a_withdrawn_key_refuses_a_staged_build_at_apply() {
        let ports = Ports::lock();
        let server = ports.serve(vec![]);
        let rig = Rig::new();
        let u = rotating(&rig, &server);
        release_signed_by(&server, 1);
        offered(u.check(&KeyRecord::Unavailable));
        u.download(&mut |_, _| {}).unwrap();
        assert!(check(&u, record(1, [0; 32])).is_err());
        let err = u.apply().unwrap_err();
        assert!(err.contains("withdrawn"), "{err}");
        rig.assert_untouched("a withdrawn key's build");
    }

    #[test]
    fn a_weak_or_zero_key_in_a_record_is_not_adopted() {
        let ports = Ports::lock();
        let server = ports.serve(vec![]);
        let rig = Rig::new();
        let u = rotating(&rig, &server);
        release_signed_by(&server, 1);
        let mut identity = [0u8; 32];
        identity[0] = 1;
        offered(check(&u, record(1, identity)));
        assert_eq!(state::load(rig.data.path()).key_record, None);
    }

    #[test]
    fn the_record_is_kept_in_the_state_file_and_old_files_still_load() {
        let ports = Ports::lock();
        let server = ports.serve(vec![]);
        let rig = Rig::new();
        release_signed_by(&server, 2);
        offered(check(&rotating(&rig, &server), record(1, key_of(2))));
        let kept = state::load(rig.data.path()).key_record.unwrap();
        assert_eq!(kept, hex::encode(record(1, key_of(2))));
        let old: state::State =
            serde_json::from_str(r#"{"schema":1,"highest_sequence":3}"#).unwrap();
        assert_eq!(old.key_record, None);
    }
}
