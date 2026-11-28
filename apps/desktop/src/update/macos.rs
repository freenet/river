//! The macOS installer: replace the whole `River.app` bundle.
//!
//! A signed, notarized bundle cannot be patched file by file (the signature seals every file),
//! so an update replaces the bundle as a unit. The new bundle is unpacked next to the old one
//! (same folder, so same volume), its code signature is checked, it is run once with
//! `--version`, and then the two directories are exchanged in one atomic step
//! (`renamex_np(RENAME_SWAP)`, on APFS). The running River goes on running from its files,
//! which are deleted once the exchange is done. River never signs anything on the user's Mac:
//! the bundle is used exactly as it was signed and notarized.
//!
//! The new bundle must pass `codesign --verify --deep --strict` against a requirement naming
//! the Team ID the RUNNING bundle is signed with, and `spctl --assess` (Gatekeeper's verdict:
//! Developer ID and notarized). So a build the update key signed but that is not signed by the
//! same developer, or whose signature was broken in transit, is refused before anything is
//! touched; macOS would otherwise refuse to open it, which would leave the user without a
//! working River.
//!
//! Not updated, with a message saying what to do: a River macOS is running from a
//! randomized read-only copy (App Translocation, for an app opened from Downloads without
//! moving it), from a disk image, from a folder the user cannot write to, a bundle owned by
//! another user, and an unsigned build (nothing to require of the new one).

use std::ffi::{CString, OsStr, OsString};
use std::fs;
use std::io::{Read, Seek};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::platform::{Installer, Support, remove_any};
use super::unpack::{output_within, preflight};
use crate::release::Version;

/// The executable inside the bundle (`apps/desktop/packaging/package-macos.sh`).
const EXECUTABLE: &str = "river";

/// How long `codesign`, `spctl` and `ditto` may take before the update gives up on them.
const TOOL_DEADLINE: Duration = Duration::from_secs(300);

/// Who signed a bundle: its Developer ID Team ID and its code-signing identifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signer {
    pub team: String,
    pub identifier: String,
}

/// What the new bundle's signature must satisfy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    /// A Developer ID Application signature from this team, for this identifier, and
    /// accepted by Gatekeeper. What a release uses.
    SameSigner(Signer),
    /// Any intact signature, ad-hoc included, and no Gatekeeper verdict. Tests only: an
    /// ad-hoc signature is all a test runner can make. Never constructed outside tests.
    #[cfg_attr(not(test), allow(dead_code))]
    IntactOnly,
}

/// The bundle `exe` runs from: `<X>.app/Contents/MacOS/<exe>`.
fn bundle_of(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    let is_app = bundle.extension().is_some_and(|e| e == "app");
    (macos.file_name()? == "MacOS" && contents.file_name()? == "Contents" && is_app)
        .then(|| bundle.to_path_buf())
}

/// Decide whether River may replace the bundle `exe` runs from, apart from the signature.
pub fn classify(exe: &Path) -> Support {
    let path = exe.to_string_lossy();
    if path.contains("/AppTranslocation/") {
        return Support::Unsupported(
            "macOS is running River from a temporary read-only copy, so River can't update \
             itself. Move River to your Applications folder, then open it from there."
                .into(),
        );
    }
    let in_cargo_tree = exe
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .windows(2)
        .any(|w| w[0] == "target" && matches!(w[1].as_str(), "debug" | "release"));
    if in_cargo_tree {
        return Support::External(
            "This is a development build, which is not updated automatically.".into(),
        );
    }
    let Some(bundle) = bundle_of(exe) else {
        return Support::Unsupported(
            "This River is not running from River.app, so it can't update itself. Download \
             the new version from github.com/freenet/river/releases."
                .into(),
        );
    };
    let Some(dir) = bundle.parent() else {
        return Support::Unsupported("River can't tell where it is installed.".into());
    };
    if !dir_is_writable(dir) {
        let why = if path.starts_with("/Volumes/") {
            "River is running from a disk or disk image it can't write to, so it can't update \
             itself. Move River to your Applications folder, then open it from there."
                .to_string()
        } else {
            format!(
                "River can't write to {}, so it can't update itself there. Move River to a \
                 folder you can change (your Applications folder, as an administrator), or \
                 download the new version from github.com/freenet/river/releases.",
                dir.display()
            )
        };
        return Support::Unsupported(why);
    }
    // SAFETY: geteuid only reads the process's credentials.
    let me = unsafe { libc::geteuid() };
    if fs::metadata(&bundle).map(|m| m.uid()).ok() != Some(me) {
        return Support::Unsupported(
            "River was installed by another user of this Mac, so it can't update itself. \
             Download the new version from github.com/freenet/river/releases."
                .into(),
        );
    }
    Support::Replaceable {
        install_path: bundle,
    }
}

fn dir_is_writable(dir: &Path) -> bool {
    tempfile::Builder::new()
        .prefix(".river-write-test-")
        .tempfile_in(dir)
        .is_ok()
}

/// Who signed `bundle`. `Ok(None)` for an unsigned or ad-hoc signed bundle; `Err` when
/// `codesign` could not answer (it did not run or took too long), which says nothing about the
/// bundle and is not remembered.
pub fn signer_of(bundle: &Path) -> Result<Option<Signer>, String> {
    let mut cmd = Command::new("/usr/bin/codesign");
    cmd.args(["--display", "--verbose=2"]).arg(bundle);
    let out = output_within(cmd, TOOL_DEADLINE).map_err(|e| format!("codesign: {e}"))?;
    if !out.status.success() {
        // Not signed at all ("code object is not signed at all").
        return Ok(None);
    }
    let text =
        String::from_utf8_lossy(&out.stderr).into_owned() + &String::from_utf8_lossy(&out.stdout);
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name))
            .map(|v| v.trim().to_string())
    };
    // Only these characters may reach the requirement text in `verify`. A Team ID is ten
    // upper-case letters and digits ("not set" for ad-hoc); an identifier is a bundle id.
    let team = field("TeamIdentifier=").filter(|t| {
        t.len() == 10
            && t.bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    });
    let identifier = field("Identifier=").filter(|i| {
        !i.is_empty()
            && i.len() <= 255
            && i.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
    });
    Ok(team
        .zip(identifier)
        .map(|(team, identifier)| Signer { team, identifier }))
}

/// The code requirement a release bundle must satisfy: Apple's Developer ID Application
/// certificate chain (the Developer ID CA and leaf extensions), this team, this identifier.
fn requirement(signer: &Signer) -> String {
    format!(
        "=anchor apple generic and identifier \"{}\" \
         and certificate 1[field.1.2.840.113635.100.6.2.6] exists \
         and certificate leaf[field.1.2.840.113635.100.6.1.13] exists \
         and certificate leaf[subject.OU] = \"{}\"",
        signer.identifier, signer.team
    )
}

/// Refuse `app` unless its signature is intact and satisfies `check`.
pub fn verify(app: &Path, check: &Check) -> Result<(), String> {
    let mut codesign = Command::new("/usr/bin/codesign");
    codesign.args(["--verify", "--deep", "--strict"]);
    if let Check::SameSigner(signer) = check {
        codesign.arg("-R").arg(requirement(signer));
    }
    codesign.arg(app);
    let out = output_within(codesign, TOOL_DEADLINE)
        .map_err(|e| format!("couldn't check the new River's signature: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "the new River's code signature is not intact or not from the same developer, so \
             it was not installed ({})",
            first_line(&out.stderr)
        ));
    }
    if matches!(check, Check::SameSigner(_)) {
        let mut spctl = Command::new("/usr/sbin/spctl");
        spctl.args(["--assess", "--type", "execute"]).arg(app);
        let out = output_within(spctl, TOOL_DEADLINE)
            .map_err(|e| format!("couldn't ask macOS about the new River: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "macOS does not accept the new River, so it was not installed ({})",
                first_line(&out.stderr)
            ));
        }
    }
    Ok(())
}

fn first_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Copy the directory `from` to `to` (which must not exist) with `ditto`, which keeps
/// symlinks, permissions and extended attributes, so a signed bundle stays signed.
fn ditto(args: &[&OsStr]) -> Result<(), String> {
    let mut cmd = Command::new("/usr/bin/ditto");
    cmd.args(args);
    let out = output_within(cmd, TOOL_DEADLINE).map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(first_line(&out.stderr))
    }
}

fn copy_bundle(from: &Path, to: &Path) -> Result<(), String> {
    remove_any(to);
    if let Some(dir) = to.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    ditto(&[from.as_os_str(), to.as_os_str()])?;
    touch(to);
    sync_tree(to).map_err(|e| e.to_string())
}

/// Give a staged bundle the current time. `ditto` keeps the times it copied, so a bundle
/// built hours ago would otherwise look like a leftover to [`super::Updater::sweep`], which
/// deletes `.river-update-` and `.river-restore-` entries older than an hour.
fn touch(dir: &Path) {
    // A file time, never signed.
    #[allow(clippy::disallowed_methods)]
    let now = std::time::SystemTime::now();
    let _ = fs::File::open(dir).and_then(|f| f.set_modified(now));
}

/// A SHA-256 (hex) over a directory tree: every entry's relative path (as bytes) and kind, a
/// symlink's target, a file's owner-execute bit, length and contents, in a fixed order. Two
/// trees have the same fingerprint exactly when they hold the same files. `.DS_Store` files are
/// left out: Finder writes them into a bundle someone merely looked inside, and they are not
/// part of River.
pub fn fingerprint_tree(root: &Path) -> std::io::Result<String> {
    fn walk(dir: &Path, rel: &[u8], h: &mut Sha256) -> std::io::Result<()> {
        let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name();
            if name == ".DS_Store" {
                continue;
            }
            let mut rel = rel.to_vec();
            rel.push(b'/');
            rel.extend_from_slice(name.as_bytes());
            let path = e.path();
            let meta = fs::symlink_metadata(&path)?;
            let kind = meta.file_type();
            if kind.is_symlink() {
                h.update(b"l\0");
                h.update(&rel);
                h.update(b"\0");
                h.update(fs::read_link(&path)?.as_os_str().as_bytes());
                h.update(b"\0");
            } else if kind.is_dir() {
                h.update(b"d\0");
                h.update(&rel);
                h.update(b"\0");
                walk(&path, &rel, h)?;
            } else if kind.is_file() {
                h.update(b"f\0");
                h.update(&rel);
                h.update(b"\0");
                h.update(if meta.permissions().mode() & 0o100 != 0 {
                    b"x"
                } else {
                    b"-"
                });
                h.update(meta.len().to_le_bytes());
                let mut file = fs::File::open(&path)?;
                let mut inner = Sha256::new();
                let mut buf = [0u8; 64 * 1024];
                loop {
                    let n = file.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    inner.update(&buf[..n]);
                }
                h.update(inner.finalize());
            } else {
                return Err(std::io::Error::other(format!(
                    "{} is not a file, folder or link",
                    path.display()
                )));
            }
        }
        Ok(())
    }
    let meta = fs::symlink_metadata(root)?;
    if !meta.is_dir() {
        return Err(std::io::Error::other("not a folder"));
    }
    let mut h = Sha256::new();
    h.update(b"river-tree-v1\0");
    walk(root, b"", &mut h)?;
    Ok(hex::encode(h.finalize()))
}

/// Flush every file and folder under `root` to the disk (`fsync` is `F_FULLFSYNC` on macOS in
/// Rust's standard library), so a power cut after the exchange cannot leave a bundle of empty
/// files in its place. `ditto` does not sync.
fn sync_tree(root: &Path) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(root)?;
    if meta.is_dir() {
        for e in fs::read_dir(root)? {
            sync_tree(&e?.path())?;
        }
        fs::File::open(root)?.sync_all()
    } else if meta.is_file() {
        fs::File::open(root)?.sync_all()
    } else {
        Ok(())
    }
}

/// The most a release zip may unpack to. River.app is about 50 MiB.
const MAX_BUNDLE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// The most entries a release zip may hold.
const MAX_ENTRIES: usize = 100_000;

/// Refuse a zip that could write outside the folder `ditto` unpacks it into, or fill the disk,
/// before `ditto` sees it: an absolute or `..` name, a top level that is not one `.app` folder,
/// a symlink pointing out of the bundle (a later entry could be written through it), or more
/// than [`MAX_BUNDLE_BYTES`] unpacked. A genuine release never trips these; they matter only
/// if the update key signed something it should not have, which the signature check that
/// follows exists to catch, and that check must not run on files already written elsewhere.
fn scan_zip(file: &fs::File) -> Result<(), String> {
    let bad = |why: &str| format!("the download is not a River release archive ({why})");
    let mut archive = zip::ZipArchive::new(file).map_err(|e| bad(&e.to_string()))?;
    if archive.len() > MAX_ENTRIES {
        return Err(bad("too many entries"));
    }
    let mut top: Option<std::ffi::OsString> = None;
    let mut total = 0u64;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| bad(&e.to_string()))?;
        let name = entry.enclosed_name().ok_or_else(|| bad("an unsafe name"))?;
        let mut parts = name.components();
        let first = parts
            .next()
            .ok_or_else(|| bad("an empty name"))?
            .as_os_str()
            .to_owned();
        // ditto keeps resource forks and extended attributes under `__MACOSX/`.
        if first != "__MACOSX" {
            if !Path::new(&first).extension().is_some_and(|x| x == "app") {
                return Err(bad("something other than an app"));
            }
            match &top {
                None => top = Some(first.clone()),
                Some(t) if *t == first => {}
                Some(_) => return Err(bad("more than one app")),
            }
        }
        total = total.saturating_add(entry.size());
        if total > MAX_BUNDLE_BYTES {
            return Err(bad("too large"));
        }
        if entry.is_symlink() {
            let mut target = Vec::new();
            (&mut entry)
                .take(4096)
                .read_to_end(&mut target)
                .map_err(|e| bad(&e.to_string()))?;
            // Depth below the top folder of the folder holding the link, then walk the target.
            let mut depth = name.components().count() as i64 - 2;
            let target = Path::new(OsStr::from_bytes(&target));
            for c in target.components() {
                match c {
                    std::path::Component::Normal(_) => depth += 1,
                    std::path::Component::CurDir => {}
                    std::path::Component::ParentDir => depth -= 1,
                    _ => return Err(bad("a link to an absolute path")),
                }
                if depth < 0 {
                    return Err(bad("a link out of the app"));
                }
            }
        }
    }
    if top.is_none() {
        return Err("the download holds no River.app".into());
    }
    Ok(())
}

/// Exchange the directories at `a` and `b` in one step. Falls back to three renames on a
/// filesystem without `RENAME_SWAP` (HFS+), undoing the first if the second fails.
fn exchange(a: &Path, b: &Path) -> std::io::Result<()> {
    let ca = CString::new(a.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
    let cb = CString::new(b.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
    // SAFETY: both are valid NUL-terminated paths that outlive the call.
    if unsafe { libc::renamex_np(ca.as_ptr(), cb.as_ptr(), libc::RENAME_SWAP) } == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ENOTSUP) => exchange_by_renames(a, b),
        _ => Err(err),
    }
}

fn exchange_by_renames(a: &Path, b: &Path) -> std::io::Result<()> {
    let dir = b
        .parent()
        .ok_or_else(|| std::io::Error::other("no directory"))?;
    let aside = dir.join(format!(".river-update-aside-{}.app", std::process::id()));
    remove_any(&aside);
    fs::rename(b, &aside)?;
    if let Err(e) = fs::rename(a, b) {
        fs::rename(&aside, b)?;
        return Err(e);
    }
    // `b` now holds what was at `a`: the exchange has taken effect. Moving the old contents to
    // `a` is tidying; if it fails they stay at `aside`, which the sweep deletes.
    let _ = fs::rename(&aside, a);
    Ok(())
}

/// Say why a replace failed, in words, for the case macOS's App Management protection blocks
/// it (macOS 13 and later can refuse to let one app change another app's bundle).
fn replace_error(what: &str, e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(libc::EPERM) | Some(libc::EACCES) => format!(
            "{what}: macOS did not allow it ({e}). If System Settings > Privacy & Security > \
             App Management lists River, turn it on and try again."
        ),
        _ => format!("{what}: {e}"),
    }
}

pub struct MacInstaller {
    exe: PathBuf,
    /// What a new bundle must satisfy, once known: it asks `codesign` about the running bundle,
    /// which takes a moment. Only an answer is remembered; a `codesign` that failed to answer
    /// is asked again next time.
    check: Mutex<Option<Option<Check>>>,
}

impl MacInstaller {
    /// For the running app.
    pub fn for_current_exe() -> MacInstaller {
        let exe = std::env::current_exe().unwrap_or_default();
        MacInstaller {
            exe: exe.canonicalize().unwrap_or(exe),
            check: Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub fn with_check(exe: PathBuf, check: Option<Check>) -> MacInstaller {
        let exe = exe.canonicalize().unwrap_or(exe);
        MacInstaller {
            exe,
            check: Mutex::new(Some(check)),
        }
    }

    /// `Ok(None)`: the running bundle is not signed by a developer, so nothing can be required
    /// of a new one. `Err`: `codesign` did not answer.
    fn check(&self, bundle: &Path) -> Result<Option<Check>, String> {
        let mut known = self.check.lock().unwrap();
        if let Some(check) = known.as_ref() {
            return Ok(check.clone());
        }
        let check = signer_of(bundle)?.map(Check::SameSigner);
        *known = Some(check.clone());
        Ok(check)
    }
}

impl Installer for MacInstaller {
    fn platform_key(&self) -> Option<&'static str> {
        Some("macos-universal")
    }

    fn support(&self) -> Support {
        if self.exe.as_os_str().is_empty() {
            return Support::Unsupported("River can't tell where it is installed.".into());
        }
        let support = classify(&self.exe);
        if let Support::Replaceable { install_path } = &support {
            match self.check(install_path) {
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Support::Unsupported(
                        "This copy of River is not signed by its developer, so River can't \
                         check that an update comes from the same place. Download the new \
                         version from github.com/freenet/river/releases."
                            .into(),
                    );
                }
                Err(e) => {
                    return Support::Unsupported(format!(
                        "River couldn't check its own signature just now ({e}). It will try \
                         again at the next update check."
                    ));
                }
            }
        }
        support
    }

    fn prepare(
        &self,
        mut artifact: fs::File,
        version: Version,
        install_path: &Path,
    ) -> Result<PathBuf, String> {
        let check = self
            .check(install_path)?
            .ok_or("this copy of River is not signed, so it can't check an update")?;
        let scanned = artifact
            .try_clone()
            .map_err(|e| format!("couldn't read the downloaded update: {e}"))?;
        scan_zip(&scanned)?;
        artifact
            .seek(std::io::SeekFrom::Start(0))
            .map_err(|e| format!("couldn't read the downloaded update: {e}"))?;
        let dir = install_path
            .parent()
            .ok_or("the install path has no directory")?;
        let work = tempfile::Builder::new()
            .prefix(".river-update-")
            .tempdir_in(dir)
            .map_err(|e| format!("couldn't write beside the installed River: {e}"))?;
        // ditto needs a path. This copy is of the file the caller already hashed.
        let zip = work.path().join("artifact.zip");
        {
            let mut out = fs::File::create(&zip).map_err(|e| e.to_string())?;
            std::io::copy(&mut artifact, &mut out)
                .map_err(|e| format!("couldn't read the downloaded update: {e}"))?;
        }
        let unpacked = work.path().join("x");
        ditto(&[
            OsStr::new("-x"),
            OsStr::new("-k"),
            zip.as_os_str(),
            unpacked.as_os_str(),
        ])
        .map_err(|e| format!("the download is not a valid archive: {e}"))?;
        let mut apps = Vec::new();
        for e in fs::read_dir(&unpacked)
            .map_err(|e| e.to_string())?
            .flatten()
        {
            let name = e.file_name();
            if name.to_string_lossy().starts_with("__MACOSX") {
                continue;
            }
            let meta = fs::symlink_metadata(e.path()).map_err(|e| e.to_string())?;
            let is_app = meta.is_dir() && Path::new(&name).extension().is_some_and(|x| x == "app");
            if !is_app {
                return Err("the download holds something other than River.app".into());
            }
            apps.push(e.path());
        }
        let app = match apps.as_slice() {
            [one] => one.clone(),
            [] => return Err("the download holds no River.app".into()),
            _ => return Err("the download holds more than one app".into()),
        };
        // The signature before anything in the bundle is run.
        verify(&app, &check)?;
        preflight(&self.executable(&app), version)?;
        let staged = dir.join(format!(
            "{}.app",
            work.path()
                .file_name()
                .ok_or("bad staging name")?
                .to_string_lossy()
        ));
        fs::rename(&app, &staged).map_err(|e| e.to_string())?;
        touch(&staged);
        if let Err(e) = sync_tree(&staged) {
            remove_any(&staged);
            return Err(format!("couldn't save the new River: {e}"));
        }
        Ok(staged)
    }

    fn swap(
        &self,
        prepared: &Path,
        install_path: &Path,
        previous: &Path,
        expected_sha256: Option<&str>,
    ) -> Result<String, String> {
        let kept = copy_bundle(install_path, previous)
            .and_then(|()| fingerprint_tree(previous).map_err(|e| e.to_string()));
        let sha = match kept {
            Ok(sha) => sha,
            Err(e) => {
                remove_any(previous);
                return Err(format!("couldn't keep a copy of the current River: {e}"));
            }
        };
        if let Some(expected) = expected_sha256
            && expected != sha
        {
            remove_any(previous);
            remove_any(prepared);
            return Err(
                "the installed River changed while this one was running, so River will not \
                 replace it"
                    .into(),
            );
        }
        exchange(prepared, install_path).map_err(|e| {
            remove_any(prepared);
            replace_error("couldn't replace the installed River", &e)
        })?;
        // `prepared` now holds the old bundle. The running River does not need its files on
        // disk any more (macOS keeps open files alive), and the copy in `previous` is kept.
        remove_any(prepared);
        Ok(sha)
    }

    fn restore(&self, previous: &Path, sha256: &str, install_path: &Path) -> Result<(), String> {
        let dir = install_path
            .parent()
            .ok_or("the install path has no directory")?;
        let staged = dir.join(format!(".river-restore-{}.app", std::process::id()));
        let copied = copy_bundle(previous, &staged)
            .and_then(|()| fingerprint_tree(&staged).map_err(|e| e.to_string()));
        match copied {
            Ok(sha) if sha == sha256 => {}
            Ok(_) => {
                remove_any(&staged);
                return Err(
                    "the kept copy of the previous River has changed, so it was not restored"
                        .into(),
                );
            }
            Err(e) => {
                remove_any(&staged);
                return Err(format!("couldn't read the previous River: {e}"));
            }
        }
        let result = if install_path.exists() {
            exchange(&staged, install_path)
        } else {
            fs::rename(&staged, install_path)
        };
        // Either the failed new bundle (after the exchange) or the unused copy.
        remove_any(&staged);
        result.map_err(|e| replace_error("couldn't restore the previous River", &e))
    }

    fn fingerprint(&self, install_path: &Path) -> Result<String, String> {
        fingerprint_tree(install_path).map_err(|e| e.to_string())
    }

    fn executable(&self, install_path: &Path) -> PathBuf {
        install_path.join("Contents/MacOS").join(EXECUTABLE)
    }

    fn spawn(
        &self,
        install_path: &Path,
        args: &[OsString],
        env: &[(&str, String)],
    ) -> std::io::Result<Child> {
        let mut cmd = Command::new(self.executable(install_path));
        // The new River starts its own log (see `log::child_stderr`).
        cmd.args(args)
            .stdin(Stdio::null())
            .stderr(crate::log::child_stderr());
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.spawn()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::update::unpack::testexe;

    /// `dir/<name>` as a minimal bundle around the test program for `version`, with a resource
    /// and a symlink (frameworks are full of them), ad-hoc signed.
    pub fn bundle(dir: &Path, name: &str, version: &str) -> PathBuf {
        bundle_as(dir, name, version, testexe::Starts::Confirms)
    }

    pub fn bundle_as(dir: &Path, name: &str, version: &str, starts: testexe::Starts) -> PathBuf {
        let app = dir.join(name);
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        fs::create_dir_all(app.join("Contents/Resources")).unwrap();
        fs::copy(
            testexe::build_as(version, starts),
            app.join("Contents/MacOS/river"),
        )
        .unwrap();
        fs::write(
            app.join("Contents/Info.plist"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>river</string>
<key>CFBundleIdentifier</key><string>org.freenet.River.test</string>
<key>CFBundleShortVersionString</key><string>{version}</string>
<key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>
"#
            ),
        )
        .unwrap();
        fs::write(app.join("Contents/Resources/data.txt"), version).unwrap();
        std::os::unix::fs::symlink("data.txt", app.join("Contents/Resources/link.txt")).unwrap();
        let st = Command::new("/usr/bin/codesign")
            .args(["--force", "--deep", "--sign", "-"])
            .arg(&app)
            .status()
            .unwrap();
        assert!(st.success(), "ad-hoc signing the test bundle");
        app
    }

    pub fn zip_of(app: &Path, out: &Path) {
        ditto(&[
            OsStr::new("-c"),
            OsStr::new("-k"),
            OsStr::new("--keepParent"),
            app.as_os_str(),
            out.as_os_str(),
        ])
        .unwrap();
    }

    fn version_of(app: &Path) -> String {
        let out = Command::new(app.join("Contents/MacOS/river"))
            .arg("--version")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn installer_for(app: &Path) -> MacInstaller {
        MacInstaller::with_check(app.join("Contents/MacOS/river"), Some(Check::IntactOnly))
    }

    #[test]
    fn where_river_runs_from_decides_whether_it_updates() {
        let dir = tempfile::tempdir().unwrap();
        let app = bundle(dir.path(), "River.app", "0.1.0");
        let exe = app.join("Contents/MacOS/river");
        assert_eq!(
            classify(&exe),
            Support::Replaceable {
                install_path: app.clone()
            }
        );
        let translocated = Path::new(
            "/private/var/folders/xy/abc/T/AppTranslocation/1234-5678/d/River.app/Contents/MacOS/river",
        );
        assert!(
            matches!(classify(translocated), Support::Unsupported(m) if m.contains("Applications folder"))
        );
        assert!(matches!(
            classify(Path::new("/Users/u/bin/river")),
            Support::Unsupported(m) if m.contains("River.app")
        ));
        assert!(matches!(
            classify(Path::new(
                "/Users/u/river/apps/target/release/River.app/Contents/MacOS/river"
            )),
            Support::External(_)
        ));
        // A folder that cannot be written to.
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
        let readonly = classify(&exe);
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            matches!(&readonly, Support::Unsupported(m) if m.contains("can't write")),
            "{readonly:?}"
        );
    }

    #[test]
    fn an_unsigned_or_ad_hoc_river_does_not_update_itself() {
        let dir = tempfile::tempdir().unwrap();
        let app = bundle(dir.path(), "River.app", "0.1.0");
        assert_eq!(signer_of(&app), Ok(None), "ad-hoc has no team");
        let installer = MacInstaller {
            exe: app.join("Contents/MacOS/river").canonicalize().unwrap(),
            check: Mutex::new(None),
        };
        let s = installer.support();
        assert!(
            matches!(&s, Support::Unsupported(m) if m.contains("not signed")),
            "{s:?}"
        );
        assert_eq!(installer.platform_key(), Some("macos-universal"));
    }

    #[test]
    fn a_release_check_refuses_a_bundle_from_anyone_else() {
        let dir = tempfile::tempdir().unwrap();
        let app = bundle(dir.path(), "River.app", "0.2.0");
        let release = Check::SameSigner(Signer {
            team: "R55ZESJCXG".into(),
            identifier: "org.freenet.River.test".into(),
        });
        let err = verify(&app, &release).unwrap_err();
        assert!(err.contains("not from the same developer"), "{err}");
        verify(&app, &Check::IntactOnly).unwrap();
    }

    /// The release check passes for a real Developer ID, notarized app, when the runner has
    /// one (GitHub's macOS images ship browsers). Skipped, and says so, otherwise.
    ///
    /// The app is copied without extended attributes first: an installed app often carries
    /// some (Finder information, say), which `--strict` refuses as "detritus". River's own
    /// update never meets that case, because it checks the bundle it unpacked from the release
    /// zip, and `sign-macos.sh` runs the same strict check on that unpacked zip before release.
    #[test]
    fn a_release_check_accepts_a_notarized_app_of_the_same_team() {
        let candidates = [
            "/Applications/Firefox.app",
            "/Applications/Google Chrome.app",
            "/Applications/Microsoft Edge.app",
        ];
        let Some((app, team)) = candidates
            .iter()
            .map(Path::new)
            .filter(|p| p.exists())
            .find_map(|p| signer_of(p).ok().flatten().map(|t| (p, t)))
        else {
            println!("skipped: no notarized third-party app on this machine");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let copy = dir.path().join("App.app");
        ditto(&[
            OsStr::new("--norsrc"),
            OsStr::new("--noextattr"),
            OsStr::new("--noqtn"),
            app.as_os_str(),
            copy.as_os_str(),
        ])
        .unwrap();
        verify(&copy, &Check::SameSigner(team.clone())).unwrap_or_else(|e| panic!("{app:?}: {e}"));
        // Another team, or another app of the same team, is refused.
        let other_team = if team.team == "R55ZESJCXG" {
            "AAAAAAAAAA"
        } else {
            "R55ZESJCXG"
        };
        for wrong in [
            Signer {
                team: other_team.into(),
                identifier: team.identifier.clone(),
            },
            Signer {
                team: team.team.clone(),
                identifier: "org.freenet.River".into(),
            },
        ] {
            assert!(
                verify(&copy, &Check::SameSigner(wrong.clone())).is_err(),
                "{wrong:?}"
            );
        }
    }

    #[test]
    fn prepare_checks_the_signature_and_version_and_stages_beside_the_install() {
        let dir = tempfile::tempdir().unwrap();
        let apps = dir.path().join("Applications");
        fs::create_dir_all(&apps).unwrap();
        let install = bundle(&apps, "River.app", "0.1.0");
        let installer = installer_for(&install);
        let install = bundle_of(&installer.exe).unwrap();
        let build = dir.path().join("build");
        fs::create_dir_all(&build).unwrap();
        let new = bundle(&build, "River.app", "0.2.0");
        let art = dir.path().join("a.zip");
        zip_of(&new, &art);

        let staged = installer
            .prepare(fs::File::open(&art).unwrap(), Version(0, 2, 0), &install)
            .unwrap();
        assert_eq!(staged.parent(), install.parent());
        assert_eq!(version_of(&staged), "river 0.2.0");
        verify(&staged, &Check::IntactOnly).unwrap();
        assert!(
            fs::symlink_metadata(staged.join("Contents/Resources/link.txt"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            version_of(&install),
            "river 0.1.0",
            "prepare must not touch it"
        );
        remove_any(&staged);

        // The wrong version is refused.
        let err = installer
            .prepare(fs::File::open(&art).unwrap(), Version(0, 3, 0), &install)
            .unwrap_err();
        assert!(err.contains("did not report"), "{err}");

        // A file changed after signing breaks the seal and is refused before it runs.
        fs::write(new.join("Contents/Resources/data.txt"), b"changed").unwrap();
        zip_of(&new, &art);
        let err = installer
            .prepare(fs::File::open(&art).unwrap(), Version(0, 2, 0), &install)
            .unwrap_err();
        assert!(err.contains("signature"), "{err}");

        // Not a zip; no app; two apps.
        fs::write(&art, b"not a zip").unwrap();
        assert!(
            installer
                .prepare(fs::File::open(&art).unwrap(), Version(0, 2, 0), &install)
                .is_err()
        );
        let two = dir.path().join("two");
        fs::create_dir_all(&two).unwrap();
        bundle(&two, "River.app", "0.2.0");
        bundle(&two, "Other.app", "0.2.0");
        ditto(&[
            OsStr::new("-c"),
            OsStr::new("-k"),
            two.as_os_str(),
            art.as_os_str(),
        ])
        .unwrap();
        let err = installer
            .prepare(fs::File::open(&art).unwrap(), Version(0, 2, 0), &install)
            .unwrap_err();
        assert!(err.contains("more than one"), "{err}");

        let left: Vec<_> = fs::read_dir(&apps).unwrap().flatten().collect();
        assert_eq!(left.len(), 1, "failed prepares leave nothing: {left:?}");
    }

    #[test]
    fn swap_exchanges_the_bundle_while_it_runs_and_restore_puts_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let apps = dir.path().join("Applications");
        fs::create_dir_all(&apps).unwrap();
        let install = bundle(&apps, "River.app", "0.1.0");
        let installer = installer_for(&install);
        let install = bundle_of(&installer.exe).unwrap();
        let prepared = bundle(&apps, ".river-update-test.app", "0.2.0");
        let previous = dir.path().join("data/update/previous/river-0.1.0");

        let mut old = Command::new(installer.executable(&install))
            .args(["--sleep", "60"])
            .spawn()
            .unwrap();
        let expected = installer.fingerprint(&install).unwrap();
        let sha = installer
            .swap(&prepared, &install, &previous, Some(&expected))
            .unwrap();
        assert_eq!(sha, expected);
        assert_eq!(version_of(&install), "river 0.2.0");
        assert_eq!(version_of(&previous), "river 0.1.0");
        verify(&install, &Check::IntactOnly).unwrap();
        verify(&previous, &Check::IntactOnly).unwrap();
        assert!(
            !prepared.exists(),
            "the old bundle is gone from Applications"
        );
        assert!(old.try_wait().unwrap().is_none(), "the old River runs on");

        // A tampered copy is not restored.
        let real = fs::read(previous.join("Contents/Resources/data.txt")).unwrap();
        fs::write(previous.join("Contents/Resources/data.txt"), b"x").unwrap();
        let err = installer.restore(&previous, &sha, &install).unwrap_err();
        assert!(err.contains("changed"), "{err}");
        assert_eq!(version_of(&install), "river 0.2.0");
        fs::write(previous.join("Contents/Resources/data.txt"), real).unwrap();

        installer.restore(&previous, &sha, &install).unwrap();
        assert_eq!(version_of(&install), "river 0.1.0");
        assert_eq!(installer.fingerprint(&install).unwrap(), sha);
        verify(&install, &Check::IntactOnly).unwrap();
        let _ = old.kill();
        let _ = old.wait();
        let names: Vec<_> = fs::read_dir(&apps)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["River.app".to_string()]);
    }

    #[test]
    fn a_changed_install_is_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let install = bundle(dir.path(), "River.app", "0.1.0");
        let installer = installer_for(&install);
        let install = bundle_of(&installer.exe).unwrap();
        let prepared = bundle(dir.path(), ".river-update-x.app", "0.2.0");
        let previous = dir.path().join("prev/river-0.1.0");
        let err = installer
            .swap(&prepared, &install, &previous, Some(&"0".repeat(64)))
            .unwrap_err();
        assert!(err.contains("changed while"), "{err}");
        assert_eq!(version_of(&install), "river 0.1.0");
        assert!(!previous.exists() && !prepared.exists());
    }

    /// A zip that could write outside the folder it is unpacked into, or is not one app, is
    /// refused before `ditto` runs; a real `ditto` zip of a bundle with links passes.
    #[test]
    fn a_hostile_zip_is_refused_before_it_is_unpacked() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.zip");
        zip_of(&bundle(dir.path(), "River.app", "0.2.0"), &good);
        scan_zip(&fs::File::open(&good).unwrap()).unwrap();

        let opts = zip::write::SimpleFileOptions::default();
        type Writer = zip::ZipWriter<std::io::Cursor<Vec<u8>>>;
        let hostile = |build: &dyn Fn(&mut Writer)| {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            build(&mut w);
            let path = dir.path().join("hostile.zip");
            fs::write(&path, w.finish().unwrap().into_inner()).unwrap();
            scan_zip(&fs::File::open(&path).unwrap())
        };
        let file = |w: &mut Writer, name: &str| {
            w.start_file(name, opts).unwrap();
            w.write_all(b"x").unwrap();
        };
        for (why, result) in [
            ("climbs out", hostile(&|w| file(w, "River.app/../../evil"))),
            ("absolute", hostile(&|w| file(w, "/tmp/evil"))),
            ("not an app", hostile(&|w| file(w, "evil.sh"))),
            (
                "two apps",
                hostile(&|w| {
                    file(w, "River.app/a");
                    file(w, "Other.app/a");
                }),
            ),
            (
                "link out",
                hostile(&|w| {
                    w.add_symlink("River.app/Contents/out", "../../..", opts)
                        .unwrap();
                    file(w, "River.app/Contents/out/evil");
                }),
            ),
            (
                "absolute link",
                hostile(&|w| {
                    w.add_symlink("River.app/l", "/Users", opts).unwrap();
                }),
            ),
            ("empty", hostile(&|_| {})),
        ] {
            assert!(result.is_err(), "{why}: {result:?}");
        }
        // A link that stays inside the bundle is fine.
        hostile(&|w| {
            w.add_symlink("River.app/Contents/Frameworks/Current", "A", opts)
                .unwrap();
            file(w, "River.app/Contents/Info.plist");
        })
        .unwrap();
    }

    #[test]
    fn the_fallback_exchange_swaps_and_undoes_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("f"), "a").unwrap();
        fs::write(b.join("f"), "b").unwrap();
        exchange_by_renames(&a, &b).unwrap();
        assert_eq!(fs::read_to_string(a.join("f")).unwrap(), "b");
        assert_eq!(fs::read_to_string(b.join("f")).unwrap(), "a");
        // A missing source: b is put back as it was.
        let err = exchange_by_renames(&dir.path().join("missing"), &b);
        assert!(err.is_err());
        assert_eq!(fs::read_to_string(b.join("f")).unwrap(), "a");
    }

    #[test]
    fn the_fingerprint_sees_contents_links_and_the_execute_bit() {
        let dir = tempfile::tempdir().unwrap();
        let app = bundle(dir.path(), "River.app", "0.1.0");
        let base = fingerprint_tree(&app).unwrap();
        let copy = dir.path().join("copy.app");
        copy_bundle(&app, &copy).unwrap();
        assert_eq!(
            fingerprint_tree(&copy).unwrap(),
            base,
            "a ditto copy is the same"
        );
        let res = copy.join("Contents/Resources");
        fs::remove_file(res.join("link.txt")).unwrap();
        std::os::unix::fs::symlink("elsewhere", res.join("link.txt")).unwrap();
        let relinked = fingerprint_tree(&copy).unwrap();
        assert_ne!(relinked, base);
        fs::set_permissions(res.join("data.txt"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_ne!(fingerprint_tree(&copy).unwrap(), relinked);
    }

    #[test]
    fn spawn_runs_the_bundles_executable() {
        let dir = tempfile::tempdir().unwrap();
        let app = bundle(dir.path(), "River.app", "0.1.0");
        let mark = dir.path().join("mark");
        let mut child = installer_for(&app)
            .spawn(
                &app,
                &["--mark".into()],
                &[(testexe::MARK_ENV, mark.to_string_lossy().into_owned())],
            )
            .unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(fs::read_to_string(&mark).unwrap(), "river 0.1.0");
    }
}
