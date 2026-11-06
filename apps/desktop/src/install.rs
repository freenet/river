//! Putting a verified Freenet release on disk, per platform. Every function here receives bytes
//! whose SHA-256 has already matched the release's signed manifest.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use river_app::InstallStep;
use sha2::{Digest, Sha256};

use crate::host::run;

/// How long the Windows setup wizard may stay open before River stops waiting for it.
const INSTALLER_DEADLINE: Duration = Duration::from_secs(30 * 60);

/// The `freenet` binary is about 66 MB; anything far larger in a release archive is refused.
const MAX_BINARY: u64 = 512 * 1024 * 1024;

/// Where River records the `freenet` binary it installed and hasn't yet registered a service
/// for. River registers a service only for that copy: a binary at the same path put there by
/// freenet-core's own installer, or by hand, belongs to whoever put it there.
fn marker_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "Freenet", "River")
        .map(|dirs| dirs.data_local_dir().join("installed-freenet"))
}

/// Record the SHA-256 of the bytes River installed and where it installed them, so the record
/// matches only that exact file. Freenet's auto-update later replaces the binary, which is
/// fine: the record is only needed until the service is registered, and is then removed.
fn record_installed_at(marker: &Path, binary: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(dir) = marker.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
    }
    let mut record = hex::encode(Sha256::digest(bytes)).into_bytes();
    record.push(b'\n');
    record.extend_from_slice(binary.as_os_str().as_encoded_bytes());
    // Written whole or not at all, like the binary: a torn record would disown River's copy.
    let dir = marker.parent().ok_or("bad record path")?;
    let write_err = |e: std::io::Error| format!("couldn't write {}: {e}", marker.display());
    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(write_err)?;
    tmp.write_all(&record).map_err(write_err)?;
    tmp.as_file().sync_all().map_err(write_err)?;
    tmp.persist(marker).map_err(|e| write_err(e.error))?;
    sync_dir(dir);
    Ok(())
}

/// Make a rename into `dir` durable. With the binary synced before its record is written,
/// River's record never survives a crash without the binary. The reverse can happen (a crash
/// between the two leaves a binary with no record, which River then treats as not its own).
/// Best effort, and only meaningful on Unix.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

fn was_installed_at(marker: &Path, binary: &Path) -> bool {
    let Ok(record) = fs::read(marker) else {
        return false;
    };
    let Some((hash, path)) = record.split_first_chunk::<64>() else {
        return false;
    };
    let Some(path) = path.strip_prefix(b"\n") else {
        return false;
    };
    path == binary.as_os_str().as_encoded_bytes()
        && fs::read(binary)
            .is_ok_and(|bytes| hex::encode(Sha256::digest(&bytes)).as_bytes() == hash.as_slice())
}

/// Whether the `freenet` binary at `binary` is the one River installed, unchanged. Strictly,
/// "the bytes River installed, still at that path": the same release installed there another
/// way would match too, which is harmless, since it is the same signed binary.
pub fn installed_by_river(binary: &Path) -> bool {
    marker_path().is_some_and(|marker| was_installed_at(&marker, binary))
}

/// Forget River's install once a service exists for it. From then on the binary is managed
/// like any other: River starts it, and never registers a service for it again.
pub fn forget_install() {
    if let Some(marker) = marker_path() {
        forget_install_at(&marker);
    }
}

fn forget_install_at(marker: &Path) {
    let _ = fs::remove_file(marker);
}

/// Where a bare `freenet` binary lives, matching freenet-core's install scripts
/// (`install.sh`: `~/.local/bin`; `install.ps1`: `%LOCALAPPDATA%\Freenet\bin`).
pub fn binary_install_path() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|d| {
            PathBuf::from(d)
                .join("Freenet")
                .join("bin")
                .join("freenet.exe")
        })
    } else {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".local").join("bin").join("freenet"))
    }
}

/// Install a verified asset. `node_up` says whether a node is answering, which is how River
/// notices that the Windows wizard has started one.
pub fn install(
    asset: &str,
    bytes: &[u8],
    progress: &mut dyn FnMut(InstallStep),
    node_up: &dyn Fn() -> bool,
) -> Result<(), String> {
    if asset.ends_with(".tar.gz") {
        progress(InstallStep::Installing);
        let dest = binary_install_path().ok_or("couldn't find your home directory")?;
        let marker = marker_path().ok_or("couldn't find your home directory")?;
        install_and_record(&extract_freenet(bytes)?, &dest, &marker)
    } else if asset.ends_with(".dmg") {
        progress(InstallStep::Installing);
        install_mac_app(bytes)
    } else if asset.ends_with(".exe") {
        progress(InstallStep::WaitingForInstaller);
        run_windows_installer(bytes, node_up)
    } else {
        Err(format!("don't know how to install {asset}"))
    }
}

/// What installing the binary did.
#[derive(Debug, PartialEq, Eq)]
enum Placed {
    Installed,
    /// Something appeared at the destination while River was downloading (the user ran
    /// another installer, say). It is left alone.
    AlreadyThere,
}

/// Install the binary, then record that River installed it. The record comes second so it
/// never names a binary River didn't install. If recording fails, River's binary is removed
/// again, since a binary River can't show it installed would never get a service; a retry then
/// starts clean. Anything that appeared at `dest` meanwhile is neither replaced nor recorded,
/// and the user is told what River found.
fn install_and_record(binary: &[u8], dest: &Path, marker: &Path) -> Result<(), String> {
    if install_binary(binary, dest)? == Placed::AlreadyThere {
        return Err(if dest.is_file() {
            format!(
                "Freenet appeared at {} while River was downloading it, so River left that \
                 copy alone. Choose Try again to start it.",
                dest.display()
            )
        } else {
            // A dangling symlink, or a directory: neither runs, and River won't remove it.
            format!(
                "{} already exists but isn't a program River can run. Remove it, then try \
                 again.",
                dest.display()
            )
        });
    }
    record_installed_at(marker, dest, binary).inspect_err(|_| {
        let _ = fs::remove_file(dest);
    })
}

/// Linux: put `freenet` at `dest`, unless something is already there. Registering it as a
/// service is left to starting it (`host::start_binary`).
fn install_binary(binary: &[u8], dest: &Path) -> Result<Placed, String> {
    let dir = dest.parent().ok_or("bad install path")?;
    fs::create_dir_all(dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
    // Write to a fresh, uniquely named file beside the destination, then move it into place,
    // so a half-written binary is never runnable and no existing file is written through.
    let mut tmp = tempfile::Builder::new()
        .prefix(".freenet.river-install-")
        .tempfile_in(dir)
        .map_err(|e| format!("couldn't write to {}: {e}", dir.display()))?;
    tmp.write_all(binary).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
    }
    // On disk before the move, so a crash can't leave an empty file at `dest`.
    tmp.as_file().sync_all().map_err(|e| e.to_string())?;
    // Never replace what's there: the probe that offered to install was minutes ago.
    match tmp.persist_noclobber(dest) {
        Ok(_) => {
            sync_dir(dir);
            Ok(Placed::Installed)
        }
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(Placed::AlreadyThere),
        Err(e) => Err(format!("couldn't install {}: {}", dest.display(), e.error)),
    }
}

/// The `freenet` executable from a release tarball. Only that one regular file is read, so
/// the archive's paths are never used to write anywhere.
fn extract_freenet(tarball: &[u8]) -> Result<Vec<u8>, String> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tarball));
    for entry in archive
        .entries()
        .map_err(|e| format!("bad release archive: {e}"))?
    {
        let entry = entry.map_err(|e| format!("bad release archive: {e}"))?;
        let is_freenet = entry
            .path()
            .ok()
            .and_then(|p| p.file_name().map(|n| n == "freenet"))
            .unwrap_or(false);
        if is_freenet && entry.header().entry_type().is_file() {
            let mut binary = Vec::new();
            entry
                .take(MAX_BINARY + 1)
                .read_to_end(&mut binary)
                .map_err(|e| format!("bad release archive: {e}"))?;
            if binary.len() as u64 > MAX_BINARY {
                return Err("the freenet binary in the release archive is too large".into());
            }
            return Ok(binary);
        }
    }
    Err("the release archive has no freenet binary".into())
}

/// A private scratch directory, removed on drop. See [`private_dir`].
fn scratch_dir() -> Result<tempfile::TempDir, String> {
    private_dir("river-freenet-")
        .tempdir()
        .map_err(|e| format!("couldn't create a temporary directory: {e}"))
}

/// A builder for a randomly named directory that only this user can use. On Unix that is
/// mode 0700, set explicitly: left to the default, the umask decides, and it is commonly
/// 0775. On Windows the directory inherits its parent's access list; `%TEMP%` is per-user.
fn private_dir(prefix: &str) -> tempfile::Builder<'_, 'static> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(prefix);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    builder
}

/// macOS: copy Freenet.app out of the signed DMG into /Applications (or ~/Applications).
fn install_mac_app(dmg: &[u8]) -> Result<(), String> {
    let scratch = scratch_dir()?;
    let image = scratch.path().join("Freenet.dmg");
    let mount = scratch.path().join("mnt");
    fs::write(&image, dmg).map_err(|e| e.to_string())?;
    fs::create_dir(&mount).map_err(|e| e.to_string())?;
    let copied = run(Command::new("/usr/bin/hdiutil")
        .arg("attach")
        .args(["-nobrowse", "-readonly", "-noautoopen", "-mountpoint"])
        .arg(&mount)
        .arg(&image))
    .and_then(|()| {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let dest = applications_dir(Path::new("/Applications"), home.as_deref())?;
        copy_app_from(&mount, &dest)
    });
    // Detach if the image is mounted, whatever happened, including an attach that failed or
    // was cut off by its deadline after mounting. Only then: given a directory that is not a
    // mount point, `hdiutil detach` acts on the volume holding it, which could be the user's
    // own disk. `-force` because Spotlight often holds a fresh mount busy.
    if is_mount_point(&mount, scratch.path()) {
        let _ = run(Command::new("/usr/bin/hdiutil")
            .args(["detach", "-quiet", "-force"])
            .arg(&mount));
    }
    copied
}

/// Whether `dir` is the root of a different filesystem from `parent`, i.e. something is
/// mounted on it.
fn is_mount_point(dir: &Path, parent: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (fs::metadata(dir), fs::metadata(parent)) {
            (Ok(d), Ok(p)) => d.dev() != p.dev(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (dir, parent);
        false
    }
}

/// Copy `mount/Freenet.app` to `dest_dir/Freenet.app`: into a staging directory first, then
/// renamed into place, so a copy cut short never looks like an installed app.
fn copy_app_from(mount: &Path, dest_dir: &Path) -> Result<(), String> {
    let source = mount.join("Freenet.app");
    if !source.is_dir() {
        return Err("the disk image has no Freenet.app in it".into());
    }
    let dest = dest_dir.join("Freenet.app");
    // `symlink_metadata`, not `exists`: `exists` misses a dangling symlink, and River would
    // then offer to install and fail with an unclear error from the rename below.
    if fs::symlink_metadata(&dest).is_ok() {
        return Err(if dest.is_dir() {
            // Most likely installed another way while River was downloading.
            format!(
                "Freenet appeared at {} while River was downloading it, so River left it \
                 alone. Choose Try again to start it.",
                dest.display()
            )
        } else {
            format!(
                "{} already exists but isn't an app River can open. Remove it, then try \
                 again.",
                dest.display()
            )
        });
    }
    let staging = private_dir(".Freenet.app.river-")
        .tempdir_in(dest_dir)
        .map_err(|e| format!("couldn't write to {}: {e}", dest_dir.display()))?;
    let staged = staging.path().join("Freenet.app");
    run(&mut copy_tree(&source, &staged))?;
    fs::rename(&staged, &dest).map_err(|e| format!("couldn't install {}: {e}", dest.display()))
}

/// `ditto` keeps an app bundle's signature, extended attributes and symlinks intact. Off
/// macOS (only ever in tests) a recursive `cp` stands in.
fn copy_tree(source: &Path, dest: &Path) -> Command {
    let mut cmd = if cfg!(target_os = "macos") {
        Command::new("/usr/bin/ditto")
    } else {
        let mut cp = Command::new("cp");
        cp.arg("-R");
        cp
    };
    cmd.arg(source).arg(dest);
    cmd
}

/// `system` when this user can write to it, else `~/Applications`.
fn applications_dir(system: &Path, home: Option<&Path>) -> Result<PathBuf, String> {
    if tempfile::tempfile_in(system).is_ok() {
        return Ok(system.to_path_buf());
    }
    let user = home
        .ok_or("couldn't find your home directory")?
        .join("Applications");
    fs::create_dir_all(&user).map_err(|e| e.to_string())?;
    Ok(user)
}

/// Windows: the signed `freenet.exe` is its own installer. Run with no arguments, it opens a
/// setup wizard that installs Freenet, registers it to start at login, and starts it.
///
/// Done when the wizard exits successfully, or as soon as a node answers. The wizard's "run
/// without installing" choice runs the node in the wizard's own process, which never exits.
fn run_windows_installer(exe: &[u8], node_up: &dyn Fn() -> bool) -> Result<(), String> {
    run_installer(
        exe,
        "freenet.exe",
        node_up,
        Duration::from_millis(500),
        INSTALLER_DEADLINE,
    )
}

/// Write `exe` to a private directory, run it, and wait as described above. The directory is
/// removed once the installer exits, even when River stops waiting first.
fn run_installer(
    exe: &[u8],
    name: &str,
    node_up: &dyn Fn() -> bool,
    poll: Duration,
    deadline: Duration,
) -> Result<(), String> {
    let scratch = scratch_dir()?;
    let installer = scratch.path().join(name);
    fs::write(&installer, exe).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&installer, fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    let mut child = spawn_installer(&installer)
        .map_err(|e| format!("couldn't run the Freenet installer: {e}"))?;
    let deadline = Instant::now() + deadline;
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("the Freenet installer stopped ({status})")),
            Ok(None) => {}
            Err(e) => break Err(e.to_string()),
        }
        if node_up() {
            break Ok(());
        }
        if Instant::now() > deadline {
            break Err("the Freenet installer is still open; finish it, then try again".into());
        }
        thread::sleep(poll);
    };
    // The installer is still open (a success screen, or a node it is running itself). Its
    // directory can't be removed while it runs, so remove it once it exits. If River exits
    // first, the directory stays in the user's temp folder.
    thread::spawn(move || {
        let _ = child.wait();
        drop(scratch);
    });
    outcome
}

/// Spawn the installer with null stdio. On Unix (only ever in tests), an exec can fail with
/// "text file busy" while another thread's fork still holds the just-written file open for
/// writing; that clears once the fork execs, so retry briefly.
fn spawn_installer(installer: &Path) -> std::io::Result<std::process::Child> {
    let mut attempts = 0;
    loop {
        let mut cmd = Command::new(installer);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Deliberately NOT `host::no_console_window`. `freenet.exe` is a console program, so a
        // console window opens behind the wizard, as it does when the user double-clicks it;
        // the wizard detaches it once the user chooses to install. With "run without
        // installing" the node runs in this same process, and the console is the only window
        // that stops it. It stays blank (stdio is null, which a GUI parent has to pass), and
        // closing it while the wizard is open closes the wizard too.
        let spawned = cmd.spawn();
        match spawned {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && attempts < 50 => {
                attempts += 1;
                thread::sleep(Duration::from_millis(20));
            }
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tarball(entries: &[(&str, tar::EntryType, &[u8])]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut builder = tar::Builder::new(gz);
        for (path, kind, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(*kind);
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, path, *data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    const FILE: tar::EntryType = tar::EntryType::Regular;

    #[test]
    fn extracts_only_the_freenet_binary() {
        let archive = tarball(&[
            ("README.md", FILE, b"docs"),
            ("freenet", FILE, b"\x7fELF binary"),
        ]);
        assert_eq!(extract_freenet(&archive).unwrap(), b"\x7fELF binary");
    }

    #[test]
    fn finds_freenet_inside_a_directory_but_not_a_lookalike_or_a_directory() {
        let archive = tarball(&[
            ("freenet-x/freenet-helper", FILE, b"no"),
            ("freenet-x/freenet", FILE, b"yes"),
        ]);
        assert_eq!(extract_freenet(&archive).unwrap(), b"yes");
        assert!(extract_freenet(&tarball(&[("fdev", FILE, b"no")])).is_err());
        assert!(extract_freenet(&tarball(&[("freenet", tar::EntryType::Directory, b"")])).is_err());
    }

    #[test]
    fn installs_the_binary_executable_and_never_replaces_one() {
        let home = tempfile::tempdir().unwrap();
        let dest = home.path().join(".local/bin/freenet");
        assert_eq!(install_binary(b"first", &dest), Ok(Placed::Installed));
        assert_eq!(install_binary(b"second", &dest), Ok(Placed::AlreadyThere));
        assert_eq!(fs::read(&dest).unwrap(), b"first");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        let leftovers: Vec<_> = fs::read_dir(dest.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, vec!["freenet"], "no temporary files left behind");
    }

    #[cfg(unix)]
    #[test]
    fn the_app_is_copied_whole_and_never_merged_into_an_existing_one() {
        let mount = tempfile::tempdir().unwrap();
        let apps = tempfile::tempdir().unwrap();
        assert!(
            copy_app_from(mount.path(), apps.path()).is_err(),
            "no Freenet.app in the image"
        );

        fs::create_dir_all(mount.path().join("Freenet.app/Contents")).unwrap();
        fs::write(
            mount.path().join("Freenet.app/Contents/Info.plist"),
            b"plist",
        )
        .unwrap();
        copy_app_from(mount.path(), apps.path()).unwrap();
        assert_eq!(
            fs::read(apps.path().join("Freenet.app/Contents/Info.plist")).unwrap(),
            b"plist"
        );
        let err = copy_app_from(mount.path(), apps.path()).unwrap_err();
        // A real app already there is reported as such, not as something to remove.
        assert!(err.contains("left it alone"), "{err}");
        assert!(!err.contains("Remove it"), "{err}");
        let names: Vec<_> = fs::read_dir(apps.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["Freenet.app"],
            "staging directories are removed"
        );

        // A dangling symlink where the app would go is refused, and left as it was.
        let other_apps = tempfile::tempdir().unwrap();
        let dangling = other_apps.path().join("Freenet.app");
        std::os::unix::fs::symlink(other_apps.path().join("gone"), &dangling).unwrap();
        let err = copy_app_from(mount.path(), other_apps.path()).unwrap_err();
        assert!(err.contains("Remove it"), "{err}");
        assert!(
            fs::symlink_metadata(&dangling)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn falls_back_to_the_users_applications_folder() {
        let home = tempfile::tempdir().unwrap();
        let missing = home.path().join("not-there");
        assert_eq!(
            applications_dir(&missing, Some(home.path())).unwrap(),
            home.path().join("Applications")
        );
        assert_eq!(
            applications_dir(home.path(), Some(home.path())).unwrap(),
            home.path()
        );
    }

    #[test]
    fn only_the_recorded_copy_counts_as_installed_by_river() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("state/installed-freenet");
        let ours = dir.path().join("freenet");
        let other = dir.path().join("other-freenet");
        fs::write(&ours, b"river's bytes").unwrap();
        fs::write(&other, b"river's bytes").unwrap();
        assert!(!was_installed_at(&marker, &ours), "nothing recorded yet");
        record_installed_at(&marker, &ours, b"river's bytes").unwrap();
        assert!(was_installed_at(&marker, &ours));
        assert!(
            !was_installed_at(&marker, &other),
            "same bytes, another path"
        );
        // Replaced by hand (or by anything else) at the same path: no longer River's.
        fs::write(&ours, b"someone else's bytes").unwrap();
        assert!(!was_installed_at(&marker, &ours));
        fs::remove_file(&ours).unwrap();
        assert!(!was_installed_at(&marker, &ours), "deleted");
        // A corrupt record matches nothing, even River's own unchanged copy.
        fs::write(&ours, b"river's bytes").unwrap();
        assert!(
            was_installed_at(&marker, &ours),
            "sanity: the good record matches"
        );
        fs::write(&marker, b"garbage").unwrap();
        assert!(!was_installed_at(&marker, &ours));

        // Forgetting removes the record; forgetting twice is fine.
        record_installed_at(&marker, &ours, b"river's bytes").unwrap();
        forget_install_at(&marker);
        assert!(!was_installed_at(&marker, &ours));
        forget_install_at(&marker);
    }

    #[test]
    fn a_failed_record_write_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let ours = dir.path().join("freenet");
        // The final rename can't happen (a directory is where the record goes).
        let blocked = dir.path().join("state/installed-freenet");
        fs::create_dir_all(blocked.join("in-use")).unwrap();
        assert!(record_installed_at(&blocked, &ours, b"bytes").is_err());
        let left: Vec<_> = fs::read_dir(dir.path().join("state"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, vec!["installed-freenet"], "no temporary record left");
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_rewrite_of_a_record_leaves_it_whole() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let ours = dir.path().join("freenet");
        fs::write(&ours, b"river's bytes").unwrap();
        let state = dir.path().join("state");
        let marker = state.join("installed-freenet");
        record_installed_at(&marker, &ours, b"river's bytes").unwrap();
        let before = fs::read(&marker).unwrap();

        // Writing a new record into the same place fails (its directory is read-only). The
        // old record must be exactly as it was: an in-place write could have truncated it.
        fs::set_permissions(&state, fs::Permissions::from_mode(0o500)).unwrap();
        let probe_blocked = fs::write(state.join("probe"), b"").is_err();
        let result = record_installed_at(&marker, &ours, b"other bytes");
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        if !probe_blocked {
            return; // Running as root: permissions don't stop the write, nothing to test.
        }
        assert!(result.is_err());
        assert_eq!(fs::read(&marker).unwrap(), before);
        assert!(was_installed_at(&marker, &ours));
    }

    #[test]
    fn scratch_directories_are_private_unique_and_removed() {
        let a = scratch_dir().unwrap();
        let b = scratch_dir().unwrap();
        assert_ne!(a.path(), b.path());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(a.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        let path = a.path().to_path_buf();
        drop(a);
        assert!(!path.exists());
    }

    #[test]
    fn the_install_is_recorded_only_when_it_succeeds_and_undone_if_recording_fails() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("bin/freenet");
        let marker = dir.path().join("state/installed-freenet");
        install_and_record(b"binary", &dest, &marker).unwrap();
        assert!(dest.exists() && was_installed_at(&marker, &dest));

        // Something appeared at the destination meanwhile: left alone, and not recorded.
        let theirs = dir.path().join("theirs/freenet");
        fs::create_dir_all(theirs.parent().unwrap()).unwrap();
        fs::write(&theirs, b"their binary").unwrap();
        let their_marker = dir.path().join("state/their-marker");
        let err = install_and_record(b"binary", &theirs, &their_marker).unwrap_err();
        assert!(err.contains("left that copy alone"), "{err}");
        assert_eq!(fs::read(&theirs).unwrap(), b"their binary");
        assert!(!their_marker.exists());

        // Something that isn't a program (a directory, or a dangling symlink) is reported, not
        // treated as an install that worked.
        let squatted = dir.path().join("squatted/freenet");
        fs::create_dir_all(&squatted).unwrap();
        let err = install_and_record(b"binary", &squatted, &their_marker).unwrap_err();
        assert!(err.contains("isn't a program"), "{err}");
        assert!(squatted.is_dir(), "left alone");
        #[cfg(unix)]
        {
            let dangling = dir.path().join("dangling/freenet");
            fs::create_dir_all(dangling.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(dir.path().join("gone"), &dangling).unwrap();
            let err = install_and_record(b"binary", &dangling, &their_marker).unwrap_err();
            assert!(err.contains("isn't a program"), "{err}");
            let kind = fs::symlink_metadata(&dangling).unwrap().file_type();
            assert!(kind.is_symlink(), "left alone");
        }
        assert!(!their_marker.exists());
        // Nothing but what was there before is left beside any of them.
        for parent in ["theirs", "squatted", "dangling"] {
            let Ok(entries) = fs::read_dir(dir.path().join(parent)) else {
                continue;
            };
            let names: Vec<_> = entries.map(|e| e.unwrap().file_name()).collect();
            assert_eq!(names, vec!["freenet"], "no temporary file left in {parent}");
        }

        // The binary can't be installed: nothing is recorded.
        let blocked = dir.path().join("not-a-dir");
        fs::write(&blocked, b"").unwrap();
        let other_marker = dir.path().join("state/other-marker");
        assert!(install_and_record(b"binary", &blocked.join("freenet"), &other_marker).is_err());
        assert!(!other_marker.exists());

        // The record can't be written (its directory is a file): the binary is removed again.
        let dest2 = dir.path().join("bin2/freenet");
        let bad_marker = blocked.join("marker");
        assert!(install_and_record(b"binary", &dest2, &bad_marker).is_err());
        assert!(!dest2.exists(), "a binary River can't record is removed");
    }

    #[test]
    fn staging_and_scratch_builders_make_private_directories() {
        let parent = tempfile::tempdir().unwrap();
        let staging = private_dir(".Freenet.app.river-")
            .tempdir_in(parent.path())
            .unwrap();
        assert!(staging.path().starts_with(parent.path()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(staging.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }

    #[test]
    fn a_plain_directory_is_not_a_mount_point() {
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("mnt");
        fs::create_dir(&dir).unwrap();
        assert!(!is_mount_point(&dir, parent.path()));
        assert!(!is_mount_point(
            &parent.path().join("missing"),
            parent.path()
        ));
    }

    /// A stand-in installer: a shell script that records where it ran (written to a temporary
    /// name and moved into place, so the record is never seen half-written), then runs `body`.
    #[cfg(unix)]
    fn script(record: &Path, body: &str) -> Vec<u8> {
        let r = record.display();
        format!("#!/bin/sh\necho \"$0\" > '{r}.tmp'\nmv '{r}.tmp' '{r}'\n{body}\n").into_bytes()
    }

    /// Script body that waits until the test creates `release`, so whether the installer is
    /// still running is decided by the test, never by the clock. It also gives up if the
    /// test's directory is gone (a failed test unwound) or after about a minute, so a failing
    /// test can't leave it spinning.
    #[cfg(unix)]
    fn hold_until(release: &Path) -> String {
        let dir = release.parent().unwrap().display();
        format!(
            "n=0\nwhile [ ! -e '{}' ] && [ -d '{dir}' ] && [ $n -lt 3000 ]; do \
             sleep 0.02; n=$((n+1)); done\nexit 0",
            release.display()
        )
    }

    #[cfg(unix)]
    fn wait_until(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting until {what}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(unix)]
    fn ran_at(record: &Path) -> PathBuf {
        PathBuf::from(fs::read_to_string(record).unwrap().trim())
    }

    #[cfg(unix)]
    #[test]
    fn the_installer_succeeds_or_fails_with_its_exit_status() {
        let out = tempfile::tempdir().unwrap();
        let record = out.path().join("ran-at");
        let poll = Duration::from_millis(20);
        let long = Duration::from_secs(30);
        let never = || false;

        run_installer(&script(&record, "exit 0"), "setup", &never, poll, long).unwrap();
        let err =
            run_installer(&script(&record, "exit 3"), "setup", &never, poll, long).unwrap_err();
        assert!(err.contains("stopped"), "{err}");
        assert!(
            !ran_at(&record).exists(),
            "removed when it exits while River waits"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_node_answering_ends_the_wait_and_the_directory_goes_when_the_installer_exits() {
        // Like the wizard's "run without installing": the installer keeps running, and a node
        // answers. River stops waiting at once, and cleans up when the installer exits.
        let out = tempfile::tempdir().unwrap();
        let record = out.path().join("ran-at");
        let release = out.path().join("release");
        let exe = script(&record, &hold_until(&release));
        let node_up = || record.exists();
        run_installer(
            &exe,
            "setup",
            &node_up,
            Duration::from_millis(20),
            Duration::from_secs(30),
        )
        .unwrap();
        let ran_at = ran_at(&record);
        assert!(ran_at.exists(), "kept while the installer runs");
        fs::write(&release, b"").unwrap();
        wait_until("the installer's directory is removed", || {
            !ran_at.parent().unwrap().exists()
        });
    }

    #[cfg(unix)]
    #[test]
    fn an_installer_left_open_past_the_deadline_is_reported() {
        let out = tempfile::tempdir().unwrap();
        let record = out.path().join("ran-at");
        let release = out.path().join("release");
        let exe = script(&record, &hold_until(&release));
        let err = run_installer(
            &exe,
            "setup",
            &|| false,
            Duration::from_millis(20),
            Duration::ZERO,
        )
        .unwrap_err();
        assert!(err.contains("still open"), "{err}");
        // Let it finish, and its directory is cleaned up.
        wait_until("the installer has run", || record.exists());
        let ran_at = ran_at(&record);
        fs::write(&release, b"").unwrap();
        wait_until("the installer's directory is removed", || {
            !ran_at.parent().unwrap().exists()
        });
    }
}
