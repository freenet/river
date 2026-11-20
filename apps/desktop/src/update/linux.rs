//! The Linux installer: an in-place swap of River's own binary.
//!
//! This is for the layout River's install script and tarball produce: one `river` file in a
//! directory the user owns (`~/.local/bin`, or wherever they put it). The swap is a rename, which
//! is atomic and leaves the running process on the old inode, so the old binary goes on
//! running until it is told to restart and nothing can see half a file.
//!
//! A copy of River that something else manages is left alone: AppImage, Flatpak, Snap, Nix,
//! anything under a system prefix (a distro or AUR package), a cargo build tree. Those say
//! "updated by your package manager" instead of replacing files a package manager owns, which
//! would make it report the file as modified or overwrite the update with the old one.

use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

use super::platform::{Installer, Support};
use crate::release::Version;

/// The name of the binary inside an artifact.
const BINARY_NAME: &str = "river";
/// The most a decompressed binary may be. A tarball bomb would otherwise fill the disk.
const MAX_UNPACKED: u64 = 512 * 1024 * 1024;
/// How long the new binary gets to answer `--version` before the swap.
const PREFLIGHT_DEADLINE: Duration = Duration::from_secs(10);

/// What `classify` looks at, so tests can supply it.
pub struct Environment<'a> {
    pub exe: &'a Path,
    pub var: &'a dyn Fn(&str) -> Option<String>,
    pub exists: &'a dyn Fn(&str) -> bool,
}

/// Directories whose contents belong to a package manager or the system.
const SYSTEM_PREFIXES: &[&str] = &[
    "/usr/bin/",
    "/usr/sbin/",
    "/usr/lib/",
    "/usr/libexec/",
    "/usr/share/",
    "/bin/",
    "/sbin/",
    "/opt/",
    "/app/",
];

/// Decide whether River may replace `exe`.
pub fn classify(env: &Environment<'_>) -> Support {
    let exe = env.exe;
    let path = exe.to_string_lossy();
    let external = |what: &str| Support::External(what.to_string());
    if (env.var)("APPIMAGE").is_some() || path.starts_with("/tmp/.mount_") {
        return external(
            "This is an AppImage. Update it by downloading the new AppImage, or with \
             AppImageUpdate.",
        );
    }
    if (env.var)("FLATPAK_ID").is_some() || (env.exists)("/.flatpak-info") {
        return external("River is installed with Flatpak. Update it with `flatpak update`.");
    }
    if (env.var)("SNAP").is_some() || path.starts_with("/snap/") {
        return external("River is installed with Snap. It updates with `snap refresh`.");
    }
    if path.starts_with("/nix/store/") {
        return external("River is managed by Nix. Update it through your Nix configuration.");
    }
    if path.contains("/.cargo/bin/") {
        return external("River was installed by cargo. Update it with `cargo install` again.");
    }
    if path.starts_with("/home/linuxbrew/") || path.contains("/.linuxbrew/") {
        return external("River was installed by Homebrew. Update it with `brew upgrade`.");
    }
    if SYSTEM_PREFIXES.iter().any(|p| path.starts_with(p)) {
        return external("River was installed by your package manager, which updates it.");
    }
    let in_cargo_tree = exe
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .windows(2)
        .any(|w| w[0] == "target" && matches!(w[1].as_str(), "debug" | "release"));
    if in_cargo_tree {
        return external("This is a development build, which is not updated automatically.");
    }
    let Some(dir) = exe.parent() else {
        return Support::Unsupported("River can't tell where it is installed.".into());
    };
    if !dir_is_writable(dir) {
        return Support::Unsupported(format!(
            "River can't write to {}, so it can't update itself there. Download the new \
             version from github.com/freenet/river/releases.",
            dir.display()
        ));
    }
    Support::Replaceable {
        install_path: exe.to_path_buf(),
    }
}

fn dir_is_writable(dir: &Path) -> bool {
    tempfile::Builder::new()
        .prefix(".river-write-test-")
        .tempfile_in(dir)
        .is_ok()
}

pub struct LinuxInstaller {
    exe: PathBuf,
}

impl LinuxInstaller {
    /// For the running binary.
    pub fn for_current_exe() -> LinuxInstaller {
        LinuxInstaller::at(std::env::current_exe().unwrap_or_default())
    }

    /// For a binary at `exe`. A symlink is followed: the file it points to is the one
    /// replaced, so the link stays valid.
    pub fn at(exe: PathBuf) -> LinuxInstaller {
        let exe = exe.canonicalize().unwrap_or(exe);
        LinuxInstaller { exe }
    }
}

fn arch_key() -> Option<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Some("linux-x86_64"),
        "aarch64" => Some("linux-aarch64"),
        _ => None,
    }
}

impl Installer for LinuxInstaller {
    fn platform_key(&self) -> Option<&'static str> {
        arch_key()
    }

    fn support(&self) -> Support {
        if self.exe.as_os_str().is_empty() {
            return Support::Unsupported("River can't tell where it is installed.".into());
        }
        classify(&Environment {
            exe: &self.exe,
            var: &|k| std::env::var(k).ok().filter(|v| !v.is_empty()),
            exists: &|p| Path::new(p).exists(),
        })
    }

    fn prepare(
        &self,
        artifact: fs::File,
        version: Version,
        install_path: &Path,
    ) -> Result<PathBuf, String> {
        let dir = install_path
            .parent()
            .ok_or("the install path has no directory")?;
        let mut tmp = tempfile::Builder::new()
            .prefix(".river-update-")
            .suffix(".new")
            .tempfile_in(dir)
            .map_err(|e| format!("couldn't write beside the installed River: {e}"))?;
        unpack_binary(artifact, tmp.as_file_mut())?;
        tmp.as_file().sync_all().map_err(|e| e.to_string())?;
        // The new file takes the installed one's permission bits (a group-readable install
        // stays so). It is refused if it would belong to someone else, which a rename over it
        // would silently change; extended attributes are not carried over.
        let installed = fs::metadata(install_path)
            .map_err(|e| format!("couldn't read the installed River: {e}"))?;
        let mine = tmp.as_file().metadata().map_err(|e| e.to_string())?;
        if installed.uid() != mine.uid() || installed.gid() != mine.gid() {
            return Err(
                "the installed River belongs to another user, so River will not replace it".into(),
            );
        }
        let mode = installed.permissions().mode() & 0o7777;
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(mode | 0o100))
            .map_err(|e| e.to_string())?;
        // Keep the file: `keep` stops the temp file deleting itself, and the caller owns it.
        let (file, path) = tmp.keep().map_err(|e| e.to_string())?;
        // Linux refuses to execute a file that is still open for writing (ETXTBSY).
        drop(file);
        if let Err(e) = preflight(&path, version) {
            let _ = fs::remove_file(&path);
            return Err(e);
        }
        Ok(path)
    }

    fn swap(
        &self,
        prepared: &Path,
        install_path: &Path,
        previous: &Path,
        expected_sha256: Option<&str>,
    ) -> Result<String, String> {
        let sha = copy_atomically(install_path, previous, 0o755)
            .map_err(|e| format!("couldn't keep a copy of the current River: {e}"))?;
        // What is installed must be what this River started as; if another River or a package
        // manager changed it meanwhile, the copy would be mislabelled as the previous version.
        if let Some(expected) = expected_sha256
            && expected != sha
        {
            let _ = fs::remove_file(previous);
            let _ = fs::remove_file(prepared);
            return Err(
                "the installed River changed while this one was running, so River will not \
                 replace it"
                    .into(),
            );
        }
        fs::rename(prepared, install_path).map_err(|e| {
            let _ = fs::remove_file(prepared);
            format!("couldn't replace the installed River: {e}")
        })?;
        Ok(sha)
    }

    fn restore(&self, previous: &Path, sha256: &str, install_path: &Path) -> Result<(), String> {
        let dir = install_path
            .parent()
            .ok_or("the install path has no directory")?;
        let staged = dir.join(format!(".river-restore-{}", std::process::id()));
        let sha = copy_atomically(previous, &staged, 0o755)
            .map_err(|e| format!("couldn't read the previous River: {e}"))?;
        if sha != sha256 {
            let _ = fs::remove_file(&staged);
            return Err(
                "the kept copy of the previous River has changed, so it was not restored".into(),
            );
        }
        fs::rename(&staged, install_path).map_err(|e| {
            let _ = fs::remove_file(&staged);
            format!("couldn't restore the previous River: {e}")
        })
    }

    fn spawn(
        &self,
        install_path: &Path,
        args: &[OsString],
        env: &[(&str, String)],
    ) -> std::io::Result<Child> {
        let mut cmd = Command::new(install_path);
        cmd.args(args).stdin(Stdio::null());
        for (k, v) in env {
            cmd.env(k, v);
        }
        retry_busy(|| cmd.spawn())
    }
}

/// Run `spawn`, trying again for a moment while Linux says the file is busy (ETXTBSY). A
/// binary written a moment ago is "busy" if another thread's `fork` happened to copy the
/// descriptor it was written through before that child's `exec` closed it; it passes in
/// milliseconds.
fn retry_busy<T>(mut spawn: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let mut tries = 0;
    loop {
        match spawn() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && tries < 40 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(25));
            }
            other => return other,
        }
    }
}

/// Copy `from` to `to` through a temporary file in `to`'s directory, so `to` is either absent,
/// its old contents, or the whole new file. Returns the SHA-256 (hex) of what was written.
fn copy_atomically(from: &Path, to: &Path, mode: u32) -> std::io::Result<String> {
    let dir = to
        .parent()
        .ok_or_else(|| std::io::Error::other("no directory"))?;
    fs::create_dir_all(dir)?;
    let mut src = fs::File::open(from)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".river-copy-")
        .tempfile_in(dir)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        tmp.write_all(&buf[..n])?;
    }
    tmp.as_file().sync_all()?;
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(mode))?;
    tmp.persist(to).map_err(|e| e.error)?;
    // Make the new directory entry durable too.
    fs::File::open(dir)?.sync_all()?;
    Ok(hex::encode(hasher.finalize()))
}

/// Write the one file called `river` in the gzipped tar `artifact` to `out`. Nothing else in the
/// archive is read or written anywhere, so no entry name can send a file elsewhere.
fn unpack_binary(file: fs::File, out: &mut fs::File) -> Result<(), String> {
    let mut archive = tar::Archive::new(GzDecoder::new(file).take(MAX_UNPACKED + 1));
    let mut found = false;
    let entries = archive
        .entries()
        .map_err(|e| format!("the download is not a valid archive: {e}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("the download is not a valid archive: {e}"))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        let depth = path.components().count();
        let named = path.file_name().is_some_and(|n| n == BINARY_NAME);
        let clean = path
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)));
        if !(named && clean && depth <= 2) {
            continue;
        }
        if found {
            return Err("the download holds more than one River binary".into());
        }
        found = true;
        let mut written = 0u64;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = entry
                .read(&mut buf)
                .map_err(|e| format!("the download is damaged: {e}"))?;
            if n == 0 {
                break;
            }
            written += n as u64;
            if written > MAX_UNPACKED {
                return Err("the River binary in the download is implausibly large".into());
            }
            out.write_all(&buf[..n])
                .map_err(|e| format!("couldn't write the new River: {e}"))?;
        }
        if written == 0 {
            return Err("the River binary in the download is empty".into());
        }
    }
    if found {
        Ok(())
    } else {
        Err("the download holds no River binary".into())
    }
}

/// Run `binary --version` and require `river <version>`.
fn preflight(binary: &Path, version: Version) -> Result<(), String> {
    let mut child = retry_busy(|| {
        Command::new(binary)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
    })
    .map_err(|e| format!("the new River does not run on this computer: {e}"))?;
    let deadline = Instant::now() + PREFLIGHT_DEADLINE;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("the new River did not answer a version check in time".into());
            }
        }
    };
    let mut said = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.by_ref().take(256).read_to_string(&mut said);
    }
    if !status.success() || said.trim() != format!("river {version}") {
        return Err(format!(
            "the new River did not report version {version} (it said {:?})",
            said.trim()
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashMap;

    fn classify_with(exe: &str, vars: &[(&str, &str)], exists: &[&str]) -> Support {
        let vars: HashMap<_, _> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let exists: Vec<String> = exists.iter().map(|s| s.to_string()).collect();
        classify(&Environment {
            exe: Path::new(exe),
            var: &|k| vars.get(k).cloned(),
            exists: &|p| exists.iter().any(|e| e == p),
        })
    }

    #[test]
    fn package_managed_copies_are_not_replaced() {
        let external = |s: Support| matches!(s, Support::External(_));
        assert!(external(classify_with("/usr/bin/river", &[], &[])));
        assert!(external(classify_with("/opt/river/river", &[], &[])));
        assert!(external(classify_with("/usr/lib/river/river", &[], &[])));
        assert!(external(classify_with(
            "/home/u/River.AppImage",
            &[("APPIMAGE", "/home/u/River.AppImage")],
            &[]
        )));
        assert!(external(classify_with(
            "/tmp/.mount_RiverXYZ/usr/bin/river",
            &[],
            &[]
        )));
        assert!(external(classify_with(
            "/home/u/bin/river",
            &[("FLATPAK_ID", "org.freenet.River")],
            &[]
        )));
        assert!(external(classify_with(
            "/app/bin/river",
            &[],
            &["/.flatpak-info"]
        )));
        assert!(external(classify_with(
            "/home/u/bin/river",
            &[],
            &["/.flatpak-info"]
        )));
        assert!(external(classify_with("/snap/river/1/river", &[], &[])));
        // Each environment variable alone, on an otherwise ordinary path.
        assert!(external(classify_with(
            "/home/u/bin/river",
            &[("APPIMAGE", "x")],
            &[]
        )));
        assert!(external(classify_with(
            "/home/u/bin/river",
            &[("SNAP", "/snap/river/1")],
            &[]
        )));
        assert!(external(classify_with(
            "/home/u/.cargo/bin/river",
            &[],
            &[]
        )));
        assert!(external(classify_with(
            "/home/linuxbrew/.linuxbrew/bin/river",
            &[],
            &[]
        )));
        assert!(external(classify_with(
            "/home/u/.linuxbrew/bin/river",
            &[],
            &[]
        )));
        assert!(external(classify_with(
            "/nix/store/abc-river/bin/river",
            &[],
            &[]
        )));
        assert!(external(classify_with(
            "/home/u/code/river/apps/target/debug/river",
            &[],
            &[]
        )));
        assert!(external(classify_with(
            "/home/u/code/river/apps/target/release/river",
            &[],
            &[]
        )));
        // A directory merely called target, or a prefix lookalike, is not special.
        assert!(!external(classify_with("/home/u/usr/bin/river", &[], &[])));
        assert!(!external(classify_with("/home/u/optimus/river", &[], &[])));
    }

    #[test]
    fn a_user_owned_install_is_replaceable_and_a_readonly_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("river");
        fs::write(&exe, b"x").unwrap();
        let exe_s = exe.to_str().unwrap();
        assert_eq!(
            classify_with(exe_s, &[], &[]),
            Support::Replaceable {
                install_path: exe.clone()
            }
        );
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
        let readonly = classify_with(exe_s, &[], &[]);
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        // Root ignores directory permissions; everyone else is refused.
        let root = fs::metadata("/proc/self")
            .map(|m| std::os::unix::fs::MetadataExt::uid(&m) == 0)
            .unwrap_or(false);
        if !root {
            assert!(matches!(readonly, Support::Unsupported(_)), "{readonly:?}");
        }
    }

    pub fn tarball(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (name, data) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o755);
            h.set_entry_type(tar::EntryType::Regular);
            h.set_cksum();
            tar.append_data(&mut h, name, *data).unwrap();
        }
        let raw = tar.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&raw).unwrap();
        gz.finish().unwrap()
    }

    pub fn script(version: &str) -> Vec<u8> {
        format!("#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'river {version}'; fi\n")
            .into_bytes()
    }

    #[test]
    fn prepare_unpacks_only_the_binary_and_checks_its_version() {
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path().join("bin/river");
        fs::create_dir_all(install.parent().unwrap()).unwrap();
        fs::write(&install, b"old").unwrap();
        fs::set_permissions(&install, fs::Permissions::from_mode(0o644)).unwrap();
        let v = Version(0, 2, 0);
        let art = dir.path().join("a.tar.gz");
        let installer = LinuxInstaller::at(install.clone());

        // The packaged layout: a directory holding river and other files.
        fs::write(
            &art,
            tarball(&[
                ("River-0.2.0-linux-x86_64/LICENSE", b"l"),
                ("River-0.2.0-linux-x86_64/river", &script("0.2.0")),
            ]),
        )
        .unwrap();
        let prepared = installer
            .prepare(fs::File::open(&art).unwrap(), v, &install)
            .unwrap();
        assert_eq!(fs::read(&prepared).unwrap(), script("0.2.0"));
        // The installed file is 0644; the new one keeps that and gains owner-execute.
        assert_eq!(
            fs::metadata(&prepared).unwrap().permissions().mode() & 0o777,
            0o744
        );
        assert_eq!(
            fs::read(&install).unwrap(),
            b"old",
            "prepare must not touch the install"
        );
        fs::remove_file(&prepared).unwrap();

        // A binary that reports another version, or does not run, is refused and cleaned up.
        for (name, files, want) in [
            (
                "wrong version",
                vec![("river", script("0.9.9"))],
                "did not report",
            ),
            (
                "not executable format",
                vec![("river", b"garbage".to_vec())],
                "does not run",
            ),
            (
                "no binary",
                vec![("other", b"x".to_vec())],
                "no River binary",
            ),
            (
                "two binaries",
                vec![("a/river", script("0.2.0")), ("b/river", script("0.2.0"))],
                "more than one",
            ),
            ("empty binary", vec![("river", Vec::new())], "empty"),
        ] {
            let files: Vec<(&str, &[u8])> = files.iter().map(|(n, d)| (*n, d.as_slice())).collect();
            fs::write(&art, tarball(&files)).unwrap();
            let err = installer
                .prepare(fs::File::open(&art).unwrap(), v, &install)
                .unwrap_err();
            assert!(err.contains(want), "{name}: {err}");
        }
        fs::write(&art, b"not a gzip").unwrap();
        assert!(
            installer
                .prepare(fs::File::open(&art).unwrap(), v, &install)
                .is_err()
        );
        let leftovers: Vec<_> = fs::read_dir(install.parent().unwrap()).unwrap().collect();
        assert_eq!(
            leftovers.len(),
            1,
            "failed prepares leave nothing behind: {leftovers:?}"
        );
    }

    #[test]
    fn archive_paths_cannot_send_the_binary_elsewhere() {
        // Names are never used as destinations, so traversal names are merely not "the binary".
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path().join("river");
        fs::write(&install, b"old").unwrap();
        let art = dir.path().join("a.tar.gz");
        let mut raw = Vec::new();
        {
            let mut tar = tar::Builder::new(&mut raw);
            let mut h = tar::Header::new_gnu();
            let data = script("0.2.0");
            h.set_size(data.len() as u64);
            h.set_entry_type(tar::EntryType::Regular);
            h.as_old_mut().name[..16].copy_from_slice(b"../../../x/river");
            h.set_cksum();
            tar.append(&h, data.as_slice()).unwrap();
            tar.finish().unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&raw).unwrap();
        fs::write(&art, gz.finish().unwrap()).unwrap();
        let err = LinuxInstaller::at(install)
            .prepare(
                fs::File::open(&art).unwrap(),
                Version(0, 2, 0),
                &dir.path().join("river"),
            )
            .unwrap_err();
        assert!(err.contains("no River binary"), "{err}");
    }

    #[test]
    fn swap_keeps_the_old_binary_and_restore_puts_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path().join("river");
        let prepared = dir.path().join("new");
        let previous = dir.path().join("keep/river-0.1.0");
        fs::write(&install, b"old binary").unwrap();
        fs::write(&prepared, b"new binary").unwrap();
        let installer = LinuxInstaller::at(install.clone());

        let sha = installer
            .swap(&prepared, &install, &previous, None)
            .unwrap();
        assert_eq!(fs::read(&install).unwrap(), b"new binary");
        assert_eq!(fs::read(&previous).unwrap(), b"old binary");
        assert!(!prepared.exists());
        assert_eq!(sha, hex::encode(Sha256::digest(b"old binary")));

        // A kept copy that no longer matches is not restored.
        fs::write(&previous, b"tampered").unwrap();
        assert!(
            installer
                .restore(&previous, &sha, &install)
                .unwrap_err()
                .contains("changed")
        );
        assert_eq!(fs::read(&install).unwrap(), b"new binary");
        fs::write(&previous, b"old binary").unwrap();
        installer.restore(&previous, &sha, &install).unwrap();
        assert_eq!(fs::read(&install).unwrap(), b"old binary");
        assert_eq!(
            fs::metadata(&install).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn a_changed_install_is_not_replaced_and_leaves_no_mislabelled_copy() {
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path().join("river");
        let prepared = dir.path().join("new");
        let previous = dir.path().join("keep/river-0.1.0");
        fs::write(&install, b"someone else installed this").unwrap();
        fs::write(&prepared, b"new binary").unwrap();
        let installer = LinuxInstaller::at(install.clone());
        let expected = hex::encode(Sha256::digest(b"what this River started as"));
        let err = installer
            .swap(&prepared, &install, &previous, Some(&expected))
            .unwrap_err();
        assert!(err.contains("changed while"), "{err}");
        assert_eq!(fs::read(&install).unwrap(), b"someone else installed this");
        assert!(!previous.exists());
        // The right hash goes through.
        fs::write(&prepared, b"new binary").unwrap();
        let ok = hex::encode(Sha256::digest(b"someone else installed this"));
        installer
            .swap(&prepared, &install, &previous, Some(&ok))
            .unwrap();
    }

    #[test]
    fn a_symlinked_install_keeps_its_link_and_the_mode() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real/river");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::write(&real, OLD_SCRIPT).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o750)).unwrap();
        let link = dir.path().join("link-river");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let installer = LinuxInstaller::at(link.clone());
        assert_eq!(installer.exe, real.canonicalize().unwrap());
        let art = dir.path().join("a.tar.gz");
        fs::write(&art, tarball(&[("river", &script("0.2.0"))])).unwrap();
        let prepared = installer
            .prepare(
                fs::File::open(&art).unwrap(),
                Version(0, 2, 0),
                &installer.exe,
            )
            .unwrap();
        assert_eq!(
            fs::metadata(&prepared).unwrap().permissions().mode() & 0o777,
            0o750,
            "mode kept"
        );
        installer
            .swap(&prepared, &installer.exe, &dir.path().join("prev"), None)
            .unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&link).unwrap(), script("0.2.0"));
    }

    const OLD_SCRIPT: &[u8] = b"#!/bin/sh\necho river 0.1.0\n";

    #[test]
    fn a_failed_swap_leaves_the_install_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path().join("river");
        fs::write(&install, b"old binary").unwrap();
        let installer = LinuxInstaller::at(install.clone());
        // The prepared file is missing: the rename fails after the copy was kept.
        let err = installer
            .swap(
                &dir.path().join("missing"),
                &install,
                &dir.path().join("prev"),
                None,
            )
            .unwrap_err();
        assert!(err.contains("couldn't replace"), "{err}");
        assert_eq!(fs::read(&install).unwrap(), b"old binary");
    }
}
