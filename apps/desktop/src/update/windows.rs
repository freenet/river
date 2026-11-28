//! The Windows installer: replace River's own `river.exe` while it runs.
//!
//! Windows will not let a running exe be overwritten or deleted, but it can be renamed. So the
//! swap renames the running `river.exe` aside (`.river-old-<pid>-<n>.exe`, same folder) and
//! renames the new one into its place; both are renames within one folder, so nothing can see
//! half a file and the old process goes on running from the renamed file until it exits. The
//! renamed-aside file cannot be deleted while it runs, so the next start's sweep deletes it.
//!
//! This is for the layout the release zip has: one `river.exe` that the user extracted into a
//! folder of their own. A copy run from inside the zip (Explorer runs it from a temporary
//! folder) or from a folder River cannot write to (Program Files without administrator rights)
//! is not updated, and the message says what to do instead.
//!
//! What this does not do:
//! - Check the new exe's Authenticode signature. As on Linux, the trust anchor is the update
//!   key: the exe's bytes are fixed by the SHA-256 in the signed manifest, and the exe is run
//!   (`--version`) only after that matched. macOS adds a check of the Developer ID team because
//!   macOS itself refuses a bundle whose signature is broken; Windows runs an exe either way.
//! - Survive a crash in the moment between the two renames of a swap. If that happens no
//!   `river.exe` is in the folder (the old one is the `.river-old-*.exe` beside it, and a copy is
//!   in River's data folder under `update/previous/`), and no River is left to notice. A
//!   failure there, rather than a crash, is handled: the swap puts the kept copy back.

use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use super::platform::{Installer, Support};
use super::unpack::{MAX_UNPACKED, copy_hashing, preflight};
use crate::release::Version;

/// The name of the binary inside the release zip.
const BINARY_NAME: &str = "river.exe";

/// Run `op` again for a while if Windows says the file is in use or access is denied. An
/// antivirus scanner or the search indexer opens a new exe for a moment, which makes a rename
/// fail with a sharing violation; it passes in well under the time allowed here.
fn retry_in_use<T>(mut op: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    const ACCESS_DENIED: i32 = 5;
    const SHARING_VIOLATION: i32 = 32;
    const LOCK_VIOLATION: i32 = 33;
    let mut tries = 0;
    loop {
        match op() {
            Err(e)
                if tries < 60
                    && (e.kind() == std::io::ErrorKind::PermissionDenied
                        || matches!(
                            e.raw_os_error(),
                            Some(ACCESS_DENIED | SHARING_VIOLATION | LOCK_VIOLATION)
                        )) =>
            {
                tries += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            other => return other,
        }
    }
}

/// What `classify` looks at, so tests can supply it.
pub struct Environment<'a> {
    pub exe: &'a Path,
    /// The user's temporary folder (`%TEMP%`), canonical.
    pub temp_dir: Option<&'a Path>,
}

/// Lower-case a path for comparison: Windows paths are case-insensitive.
fn folded(p: &Path) -> String {
    p.to_string_lossy().replace('/', "\\").to_lowercase()
}

/// `\\?\C:\x` -> `C:\x`, for messages and comparisons. `canonicalize` returns the long form.
fn plain(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    // Only a drive path (`\\?\C:\...`): a UNC or volume-GUID path keeps its prefix, which
    // stripping would turn into a relative path.
    match s.strip_prefix(r"\\?\") {
        Some(rest)
            if rest.len() >= 3
                && rest.as_bytes()[0].is_ascii_alphabetic()
                && rest.as_bytes()[1] == b':'
                && rest.as_bytes()[2] == b'\\' =>
        {
            PathBuf::from(rest)
        }
        _ => p.to_path_buf(),
    }
}

/// Decide whether River may replace `exe`.
pub fn classify(env: &Environment<'_>) -> Support {
    let exe = plain(env.exe);
    if exe.as_os_str().is_empty() {
        return Support::Unsupported("River can't tell where it is installed.".into());
    }
    let parts: Vec<String> = exe
        .components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_string_lossy().to_lowercase()),
            _ => None,
        })
        .collect();
    if parts
        .windows(2)
        .any(|w| w[0] == "target" && matches!(w[1].as_str(), "debug" | "release"))
    {
        return Support::External(
            "This is a development build, which is not updated automatically.".into(),
        );
    }
    let in_temp = env.temp_dir.is_some_and(|t| {
        let t = folded(&plain(t));
        let t = t.trim_end_matches('\\');
        folded(&exe).starts_with(&format!("{t}\\"))
    });
    // Explorer runs an exe double-clicked inside a zip from `%TEMP%\Temp1_<zip name>\`; the
    // name is checked too in case the temporary folder is somewhere unusual.
    let in_zip = parts
        .iter()
        .any(|p| p.starts_with("temp1_") || p.ends_with(".zip"));
    if in_temp || in_zip {
        return Support::Unsupported(
            "River is running from inside the zip or from a temporary folder, so it can't \
             update itself. Extract the zip to a folder of your own on this computer (not one \
             that OneDrive or another service syncs) and run river.exe from there."
                .into(),
        );
    }
    let Some(dir) = exe.parent() else {
        return Support::Unsupported("River can't tell where it is installed.".into());
    };
    if !dir_is_writable(dir) {
        return Support::Unsupported(format!(
            "River can't write to {}, so it can't update itself there. Move river.exe to a \
             folder of your own, or download the new version from \
             github.com/freenet/river/releases.",
            dir.display()
        ));
    }
    Support::Replaceable {
        install_path: env.exe.to_path_buf(),
    }
}

fn dir_is_writable(dir: &Path) -> bool {
    tempfile::Builder::new()
        .prefix(".river-write-test-")
        .tempfile_in(dir)
        .is_ok()
}

pub struct WindowsInstaller {
    exe: PathBuf,
    /// Tests run from the temporary folder, which [`classify`] rightly refuses.
    #[cfg(test)]
    anywhere: bool,
}

impl WindowsInstaller {
    /// For the running exe.
    pub fn for_current_exe() -> WindowsInstaller {
        WindowsInstaller::at(std::env::current_exe().unwrap_or_default())
    }

    /// For an exe at `exe`. A link is followed: the file it points to is the one replaced.
    pub fn at(exe: PathBuf) -> WindowsInstaller {
        let exe = exe.canonicalize().map(|p| plain(&p)).unwrap_or(exe);
        WindowsInstaller {
            exe,
            #[cfg(test)]
            anywhere: false,
        }
    }

    /// An installer whose install may be in the temporary folder.
    #[cfg(test)]
    pub fn anywhere(exe: PathBuf) -> WindowsInstaller {
        WindowsInstaller {
            anywhere: true,
            ..WindowsInstaller::at(exe)
        }
    }
}

/// A fresh name beside `install_path` for the exe being moved out of the way.
fn aside_path(install_path: &Path) -> Result<PathBuf, String> {
    let dir = install_path
        .parent()
        .ok_or("the install path has no directory")?;
    for n in 0..1000u32 {
        let p = dir.join(format!(".river-old-{}-{n}.exe", std::process::id()));
        if !p.exists() {
            return Ok(p);
        }
    }
    Err("couldn't find a free name to move the old River aside".into())
}

/// `fs::rename`, which a test can make fail from the `n`th call on this thread
/// ([`tests::fail_renames_from`]), to reach the recovery paths a real filesystem rarely takes.
fn rename(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if tests::rename_should_fail() {
        return Err(std::io::Error::other("rename refused by the test"));
    }
    fs::rename(from, to)
}

/// Move `install_path` aside and `incoming` into its place. If the second rename fails, the
/// first is undone, so `install_path` is the old file again. Returns where the old file went.
fn replace_running(incoming: &Path, install_path: &Path) -> Result<PathBuf, String> {
    let aside = aside_path(install_path)?;
    retry_in_use(|| rename(install_path, &aside))
        .map_err(|e| format!("couldn't move the running River aside: {e}"))?;
    if let Err(e) = retry_in_use(|| rename(incoming, install_path)) {
        return match retry_in_use(|| rename(&aside, install_path)) {
            Ok(()) => Err(format!("couldn't replace the installed River: {e}")),
            Err(back) => Err(format!(
                "couldn't replace the installed River ({e}), and couldn't move it back \
                 ({back}); it is at {}",
                aside.display()
            )),
        };
    }
    Ok(aside)
}

fn arch_key() -> Option<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Some("windows-x86_64"),
        _ => None,
    }
}

impl Installer for WindowsInstaller {
    fn platform_key(&self) -> Option<&'static str> {
        arch_key()
    }

    fn support(&self) -> Support {
        let temp = std::env::temp_dir();
        let temp = temp.canonicalize().unwrap_or(temp);
        #[cfg(test)]
        let temp_dir = (!self.anywhere).then_some(temp.as_path());
        #[cfg(not(test))]
        let temp_dir = Some(temp.as_path());
        classify(&Environment {
            exe: &self.exe,
            temp_dir,
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
            .suffix(".exe")
            .tempfile_in(dir)
            .map_err(|e| format!("couldn't write beside the installed River: {e}"))?;
        unpack_exe(artifact, tmp.as_file_mut())?;
        tmp.as_file().sync_all().map_err(|e| e.to_string())?;
        let (file, path) = tmp.keep().map_err(|e| e.to_string())?;
        // Windows will not run a file that is open for writing.
        drop(file);
        if let Err(e) = preflight(&path, version) {
            let _ = retry_in_use(|| fs::remove_file(&path));
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
        // Reading a running exe is allowed; only writing and deleting it are not.
        let sha = copy_hashing(install_path, previous)
            .map_err(|e| format!("couldn't keep a copy of the current River: {e}"))?;
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
        if let Err(e) = replace_running(prepared, install_path) {
            let _ = fs::remove_file(prepared);
            // The new exe would not go in and the old one would not go back: put the kept copy
            // there instead, so a River is installed whatever happens to this process.
            if fs::symlink_metadata(install_path).is_err() {
                return match copy_hashing(previous, install_path) {
                    Ok(_) => Err(format!("{e}; the copy kept beforehand was put back")),
                    Err(c) => Err(format!("{e}; and the kept copy could not be put back: {c}")),
                };
            }
            return Err(e);
        }
        // The old exe is still running; the next start's sweep deletes it.
        Ok(sha)
    }

    fn restore(&self, previous: &Path, sha256: &str, install_path: &Path) -> Result<(), String> {
        let dir = install_path
            .parent()
            .ok_or("the install path has no directory")?;
        let staged = dir.join(format!(".river-restore-{}.exe", std::process::id()));
        let sha = copy_hashing(previous, &staged)
            .map_err(|e| format!("couldn't read the previous River: {e}"))?;
        if sha != sha256 {
            let _ = fs::remove_file(&staged);
            return Err(
                "the kept copy of the previous River has changed, so it was not restored".into(),
            );
        }
        // The file being replaced may be running (a new version rolling itself back), so it is
        // moved aside like the original was.
        let result = if install_path.exists() {
            replace_running(&staged, install_path).map(|aside| {
                let _ = fs::remove_file(aside);
            })
        } else {
            retry_in_use(|| fs::rename(&staged, install_path))
                .map_err(|e| format!("couldn't restore the previous River: {e}"))
        };
        if result.is_err() {
            let _ = fs::remove_file(&staged);
        }
        result
    }

    fn spawn(
        &self,
        install_path: &Path,
        args: &[OsString],
        env: &[(&str, String)],
    ) -> std::io::Result<Child> {
        let mut cmd = Command::new(install_path);
        // Null standard handles: a release build (no console) has none worth passing on, and
        // inheriting invalid ones makes the spawn fail. The new River starts its own log.
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.spawn()
    }
}

/// Write the one file called `river.exe` in the zip `artifact` to `out`. Nothing else in the
/// archive is read or written anywhere, and its names are never used as destinations.
fn unpack_exe(file: fs::File, out: &mut fs::File) -> Result<(), String> {
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| format!("the download is not a valid archive: {e}"))?;
    let mut found = None;
    for i in 0..archive.len() {
        let entry = archive
            .by_index_raw(i)
            .map_err(|e| format!("the download is not a valid archive: {e}"))?;
        if !entry.is_file() {
            continue;
        }
        // `enclosed_name` is None for an absolute name or one that climbs out with `..`.
        let Some(path) = entry.enclosed_name() else {
            continue;
        };
        let named = path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(BINARY_NAME));
        if !(named && path.components().count() <= 2) {
            continue;
        }
        if found.is_some() {
            return Err("the download holds more than one River binary".into());
        }
        found = Some(i);
    }
    let Some(index) = found else {
        return Err("the download holds no River binary".into());
    };
    let mut entry = archive
        .by_index(index)
        .map_err(|e| format!("the download is not a valid archive: {e}"))?;
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::unpack::testexe;
    use sha2::{Digest, Sha256};
    use std::cell::Cell;

    thread_local! {
        /// Renames on this thread: how many have run, and from which one on they fail.
        static RENAMES: Cell<(u32, Option<u32>)> = const { Cell::new((0, None)) };
    }

    /// Make this thread's renames fail from the `n`th (0-based) on.
    pub fn fail_renames_from(n: Option<u32>) {
        RENAMES.with(|r| r.set((0, n)));
    }

    pub fn rename_should_fail() -> bool {
        RENAMES.with(|r| {
            let (count, from) = r.get();
            r.set((count + 1, from));
            from.is_some_and(|f| count >= f)
        })
    }

    /// A child that is killed when the test ends, passing or not.
    struct Running(Child);

    impl Drop for Running {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in files {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn version_of(exe: &Path) -> String {
        let out = Command::new(exe).arg("--version").output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A copy of the test program at `path`, running (it sleeps), so Windows holds it open the
    /// way it holds a running River.
    fn run_from(path: &Path) -> Running {
        let child = Command::new(path)
            .args(["--sleep", "60"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // While it runs, Windows refuses to delete or overwrite it: the premise of this module.
        assert!(fs::remove_file(path).is_err(), "a running exe is locked");
        Running(child)
    }

    fn install_of(dir: &Path, version: &str) -> PathBuf {
        let install = dir.join("River").join("river.exe");
        fs::create_dir_all(install.parent().unwrap()).unwrap();
        fs::copy(testexe::build(version), &install).unwrap();
        install
    }

    fn asides(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".river-old-")
            })
            .collect()
    }

    #[test]
    fn where_river_runs_from_decides_whether_it_updates() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("river.exe");
        fs::write(&exe, b"x").unwrap();
        let temp = Path::new(r"C:\Users\u\AppData\Local\Temp");
        let ok = classify(&Environment {
            exe: &exe,
            temp_dir: Some(temp),
        });
        assert_eq!(
            ok,
            Support::Replaceable {
                install_path: exe.clone()
            }
        );
        for inside in [
            r"C:\Users\u\AppData\Local\Temp\Temp1_River-0.1.0-windows-x86_64.zip\River-0.1.0-windows-x86_64\river.exe",
            r"c:\users\u\appdata\local\temp\x\river.exe",
            r"D:\elsewhere\Temp1_River.zip\river.exe",
            r"D:\elsewhere\River-0.1.0-windows-x86_64.zip\river.exe",
        ] {
            let s = classify(&Environment {
                exe: Path::new(inside),
                temp_dir: Some(temp),
            });
            assert!(
                matches!(&s, Support::Unsupported(m) if m.contains("Extract the zip")),
                "{inside}: {s:?}"
            );
        }
        // A folder whose name merely starts like the temp folder is not inside it.
        let mut shorter = dir.path().as_os_str().to_owned().into_string().unwrap();
        shorter.pop();
        let s = classify(&Environment {
            exe: &exe,
            temp_dir: Some(Path::new(&shorter)),
        });
        assert!(matches!(s, Support::Replaceable { .. }), "{s:?}");
        let dev = classify(&Environment {
            exe: Path::new(r"C:\code\river\apps\target\release\river.exe"),
            temp_dir: Some(temp),
        });
        assert!(matches!(dev, Support::External(_)), "{dev:?}");
        // A folder that does not exist cannot be written to.
        let gone = classify(&Environment {
            exe: &dir.path().join("missing").join("river.exe"),
            temp_dir: Some(temp),
        });
        assert!(
            matches!(&gone, Support::Unsupported(m) if m.contains("can't write")),
            "{gone:?}"
        );
    }

    #[test]
    fn the_running_installer_reports_this_platform() {
        assert_eq!(
            WindowsInstaller::for_current_exe().platform_key(),
            Some("windows-x86_64")
        );
    }

    #[test]
    fn prepare_unpacks_only_the_exe_and_checks_its_version() {
        let dir = tempfile::tempdir().unwrap();
        let install = install_of(dir.path(), "0.1.0");
        let folder = install.parent().unwrap();
        let installer = WindowsInstaller::at(install.clone());
        let new = fs::read(testexe::build("0.2.0")).unwrap();
        let art = dir.path().join("a.zip");
        // The packaged layout: a folder holding river.exe, the licence and a README.
        fs::write(
            &art,
            zip_of(&[
                ("River-0.2.0-windows-x86_64/LICENSE", b"l"),
                ("River-0.2.0-windows-x86_64/README.txt", b"r"),
                ("River-0.2.0-windows-x86_64/river.exe", &new),
            ]),
        )
        .unwrap();
        let prepared = installer
            .prepare(fs::File::open(&art).unwrap(), Version(0, 2, 0), &install)
            .unwrap();
        assert_eq!(prepared.parent(), Some(folder));
        assert_eq!(fs::read(&prepared).unwrap(), new);
        assert_eq!(
            version_of(&install),
            "river 0.1.0",
            "prepare must not touch it"
        );
        fs::remove_file(&prepared).unwrap();

        let old = fs::read(testexe::build("0.1.0")).unwrap();
        for (name, files, want) in [
            (
                "wrong version",
                vec![("river.exe", old.clone())],
                "did not report",
            ),
            (
                "not a program",
                vec![("river.exe", b"garbage".to_vec())],
                "does not run",
            ),
            (
                "no exe",
                vec![("other.exe", new.clone())],
                "no River binary",
            ),
            (
                "two exes",
                vec![("a/river.exe", new.clone()), ("b/river.exe", new.clone())],
                "more than one",
            ),
            (
                "too deep",
                vec![("a/b/river.exe", new.clone())],
                "no River binary",
            ),
            (
                "climbs out",
                vec![("../river.exe", new.clone())],
                "no River binary",
            ),
            ("empty", vec![("river.exe", Vec::new())], "empty"),
        ] {
            let files: Vec<(&str, &[u8])> = files.iter().map(|(n, d)| (*n, d.as_slice())).collect();
            fs::write(&art, zip_of(&files)).unwrap();
            let err = installer
                .prepare(fs::File::open(&art).unwrap(), Version(0, 2, 0), &install)
                .unwrap_err();
            assert!(err.contains(want), "{name}: {err}");
        }
        fs::write(&art, b"not a zip").unwrap();
        assert!(
            installer
                .prepare(fs::File::open(&art).unwrap(), Version(0, 2, 0), &install)
                .is_err()
        );
        let left: Vec<_> = fs::read_dir(folder).unwrap().flatten().collect();
        assert_eq!(left.len(), 1, "failed prepares leave nothing: {left:?}");
    }

    #[test]
    fn swap_replaces_a_running_exe_and_restore_replaces_the_running_new_one() {
        let dir = tempfile::tempdir().unwrap();
        let install = install_of(dir.path(), "0.1.0");
        let folder = install.parent().unwrap().to_path_buf();
        let installer = WindowsInstaller::at(install.clone());
        let install = installer.exe.clone();
        let old_bytes = fs::read(&install).unwrap();
        let prepared = folder.join(".river-update-test.exe");
        fs::copy(testexe::build("0.2.0"), &prepared).unwrap();
        let previous = dir.path().join("data/previous/river-0.1.0");

        let mut old = run_from(&install);
        let expected = installer.fingerprint(&install).unwrap();
        let sha = installer
            .swap(&prepared, &install, &previous, Some(&expected))
            .unwrap();
        assert_eq!(sha, hex::encode(Sha256::digest(&old_bytes)));
        assert_eq!(sha, expected, "fingerprint and swap agree");
        assert_eq!(version_of(&install), "river 0.2.0");
        assert_eq!(fs::read(&previous).unwrap(), old_bytes);
        assert!(!prepared.exists());
        assert_eq!(asides(&folder).len(), 1, "the running exe was moved aside");
        assert!(
            old.0.try_wait().unwrap().is_none(),
            "the old River goes on running"
        );

        // A new version rolling itself back: the exe being replaced is the running one.
        let mut new = run_from(&install);
        installer.restore(&previous, &sha, &install).unwrap();
        assert_eq!(version_of(&install), "river 0.1.0");
        assert!(new.0.try_wait().unwrap().is_none());
        drop((old, new));
        // Nothing but the install and the renamed-aside files the sweep deletes.
        let names: Vec<String> = fs::read_dir(&folder)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with(".river-old-"))
            .collect();
        assert_eq!(names, vec!["river.exe".to_string()]);
    }

    #[test]
    fn a_tampered_previous_copy_is_not_restored() {
        let dir = tempfile::tempdir().unwrap();
        let install = install_of(dir.path(), "0.2.0");
        let installer = WindowsInstaller::at(install.clone());
        let previous = dir.path().join("previous.exe");
        fs::copy(testexe::build("0.1.0"), &previous).unwrap();
        let sha = installer.fingerprint(&previous).unwrap();
        fs::write(&previous, b"tampered").unwrap();
        let err = installer
            .restore(&previous, &sha, &installer.exe)
            .unwrap_err();
        assert!(err.contains("changed"), "{err}");
        assert_eq!(version_of(&install), "river 0.2.0");
    }

    #[test]
    fn a_changed_install_is_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let install = install_of(dir.path(), "0.1.0");
        let installer = WindowsInstaller::at(install.clone());
        let prepared = install.with_file_name(".river-update-x.exe");
        fs::copy(testexe::build("0.2.0"), &prepared).unwrap();
        let previous = dir.path().join("prev/river-0.1.0");
        let err = installer
            .swap(&prepared, &installer.exe, &previous, Some(&"0".repeat(64)))
            .unwrap_err();
        assert!(err.contains("changed while"), "{err}");
        assert_eq!(version_of(&install), "river 0.1.0");
        assert!(!previous.exists() && !prepared.exists());
    }

    #[test]
    fn a_failed_swap_puts_the_running_exe_back() {
        let dir = tempfile::tempdir().unwrap();
        let install = install_of(dir.path(), "0.1.0");
        let installer = WindowsInstaller::at(install.clone());
        let old = run_from(&installer.exe);
        // The prepared file is missing: the second rename fails after the first moved the
        // running exe aside, which must be undone.
        let err = installer
            .swap(
                &dir.path().join("missing.exe"),
                &installer.exe,
                &dir.path().join("prev"),
                None,
            )
            .unwrap_err();
        assert!(err.contains("couldn't replace"), "{err}");
        assert_eq!(version_of(&install), "river 0.1.0");
        assert!(asides(install.parent().unwrap()).is_empty());
        drop(old);
    }

    /// The new exe will not go in and the old one will not go back: the swap puts the copy it
    /// kept in their place, so a River is installed whatever happens next.
    #[test]
    fn a_swap_whose_undo_fails_puts_the_kept_copy_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let install = install_of(dir.path(), "0.1.0");
        let installer = WindowsInstaller::at(install.clone());
        let old = run_from(&installer.exe);
        let prepared = install.with_file_name(".river-update-x.exe");
        fs::copy(testexe::build("0.2.0"), &prepared).unwrap();
        let previous = dir.path().join("prev/river-0.1.0.exe");
        // The first rename (the running exe aside) works; the next two fail.
        fail_renames_from(Some(1));
        let err = installer
            .swap(&prepared, &installer.exe, &previous, None)
            .unwrap_err();
        fail_renames_from(None);
        assert!(err.contains("copy kept beforehand was put back"), "{err}");
        assert_eq!(version_of(&install), "river 0.1.0");
        assert!(!prepared.exists());
        drop(old);
    }

    /// An exe another program holds open (an antivirus scan) cannot be renamed for a moment;
    /// the swap waits for it rather than failing.
    #[test]
    fn a_rename_waits_for_a_file_held_open_for_a_moment() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let held = dir.path().join("held.exe");
        fs::write(&held, b"x").unwrap();
        // No sharing at all, as a scanner opens a file.
        let file = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&held)
            .unwrap();
        assert!(
            fs::rename(&held, dir.path().join("probe")).is_err(),
            "it is held"
        );
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(file);
        });
        let to = dir.path().join("moved.exe");
        retry_in_use(|| fs::rename(&held, &to)).unwrap();
        release.join().unwrap();
        assert!(to.exists());
        // Other errors are not waited for.
        let start = std::time::Instant::now();
        assert!(retry_in_use(|| fs::rename(dir.path().join("missing"), &to)).is_err());
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn spawn_starts_the_installed_exe_with_its_environment() {
        let dir = tempfile::tempdir().unwrap();
        let install = install_of(dir.path(), "0.1.0");
        let mark = dir.path().join("mark");
        let mut child = WindowsInstaller::at(install.clone())
            .spawn(
                &install,
                &["--mark".into()],
                &[(testexe::MARK_ENV, mark.to_string_lossy().into_owned())],
            )
            .unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(fs::read_to_string(&mark).unwrap(), "river 0.1.0");
    }
}
