//! Putting a verified Freenet release on disk, per platform. Every function here receives bytes
//! whose SHA-256 has already matched the release's signed manifest.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use river_app::InstallStep;

use crate::host::run;

/// How long the Windows setup wizard may stay open before River stops waiting for it.
const INSTALLER_DEADLINE: Duration = Duration::from_secs(30 * 60);

/// The `freenet` binary is about 66 MB; anything far larger in a release archive is refused.
const MAX_BINARY: u64 = 512 * 1024 * 1024;

/// Where River records the `freenet` binary it installed. River registers a service only for
/// that copy: a binary at the same path put there by freenet-core's own installer belongs to
/// whoever installed it.
fn marker_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "Freenet", "River")
        .map(|dirs| dirs.data_local_dir().join("installed-freenet"))
}

fn record_installed_at(marker: &Path, binary: &Path) -> Result<(), String> {
    if let Some(dir) = marker.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
    }
    fs::write(marker, binary.as_os_str().as_encoded_bytes())
        .map_err(|e| format!("couldn't write {}: {e}", marker.display()))
}

fn was_installed_at(marker: &Path, binary: &Path) -> bool {
    fs::read(marker).is_ok_and(|recorded| recorded == binary.as_os_str().as_encoded_bytes())
}

/// Whether River installed the `freenet` binary at `binary`.
pub fn installed_by_river(binary: &Path) -> bool {
    marker_path().is_some_and(|marker| was_installed_at(&marker, binary))
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
        install_binary(&extract_freenet(bytes)?, &dest)?;
        record_installed_at(&marker, &dest)
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

/// Linux: put `freenet` at `dest`. Registering it as a service is left to starting it
/// (`host::start_binary`).
fn install_binary(binary: &[u8], dest: &Path) -> Result<(), String> {
    let dir = dest.parent().ok_or("bad install path")?;
    fs::create_dir_all(dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
    // Write to a fresh, uniquely named file beside the destination, then rename it into place,
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
    // On disk before the rename, so a crash can't leave an empty file at `dest`.
    tmp.as_file().sync_all().map_err(|e| e.to_string())?;
    tmp.persist(dest)
        .map_err(|e| format!("couldn't install {}: {}", dest.display(), e.error))?;
    Ok(())
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
    // Detach whatever happened, including an attach that failed or was cut off by its
    // deadline after mounting. `-force` because Spotlight often holds a fresh mount busy.
    let _ = run(Command::new("/usr/bin/hdiutil")
        .args(["detach", "-quiet", "-force"])
        .arg(&mount));
    copied
}

/// Copy `mount/Freenet.app` to `dest_dir/Freenet.app`: into a staging directory first, then
/// renamed into place, so a copy cut short never looks like an installed app.
fn copy_app_from(mount: &Path, dest_dir: &Path) -> Result<(), String> {
    let source = mount.join("Freenet.app");
    if !source.is_dir() {
        return Err("the disk image has no Freenet.app in it".into());
    }
    let dest = dest_dir.join("Freenet.app");
    if dest.exists() {
        return Err(format!("{} already exists", dest.display()));
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
    let mut child = Command::new(&installer)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("couldn't run the Freenet installer: {e}"))?;
    let deadline = Instant::now() + deadline;
    let outcome = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return if status.success() {
                Ok(())
            } else {
                Err(format!("the Freenet installer stopped ({status})"))
            };
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
    // directory can't be removed while it runs, so remove it once it exits.
    thread::spawn(move || {
        let _ = child.wait();
        drop(scratch);
    });
    outcome
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
    fn installs_the_binary_executable_and_replaces_an_old_one() {
        let home = tempfile::tempdir().unwrap();
        let dest = home.path().join(".local/bin/freenet");
        install_binary(b"first", &dest).unwrap();
        install_binary(b"second", &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"second");
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
        assert!(err.ends_with("already exists"), "{err}");
        let names: Vec<_> = fs::read_dir(apps.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["Freenet.app"],
            "staging directories are removed"
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
        let ours = dir.path().join("bin/freenet");
        assert!(!was_installed_at(&marker, &ours), "nothing recorded yet");
        record_installed_at(&marker, &ours).unwrap();
        assert!(was_installed_at(&marker, &ours));
        assert!(!was_installed_at(
            &marker,
            &dir.path().join("other/freenet")
        ));
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

    /// A stand-in installer: a shell script that records where it ran, then runs `body`.
    #[cfg(unix)]
    fn script(record: &Path, body: &str) -> Vec<u8> {
        format!("#!/bin/sh\necho \"$0\" > '{}'\n{body}\n", record.display()).into_bytes()
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

        // Its private directory is gone once it has exited.
        let ran_at = PathBuf::from(fs::read_to_string(&record).unwrap().trim());
        wait_until("the installer's directory is removed", || {
            !ran_at.parent().unwrap().exists()
        });
    }

    #[cfg(unix)]
    #[test]
    fn a_node_answering_ends_the_wait_and_the_directory_goes_when_the_installer_exits() {
        // Like the wizard's "run without installing": the installer keeps running, and a node
        // answers. River stops waiting at once, and cleans up when the installer exits.
        let out = tempfile::tempdir().unwrap();
        let record = out.path().join("ran-at");
        let exe = script(&record, "sleep 2; exit 0");
        let started = Instant::now();
        let node_up = || record.exists();
        run_installer(
            &exe,
            "setup",
            &node_up,
            Duration::from_millis(20),
            Duration::from_secs(30),
        )
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        let ran_at = PathBuf::from(fs::read_to_string(&record).unwrap().trim());
        assert!(ran_at.exists(), "kept while the installer runs");
        wait_until("the installer's directory is removed", || {
            !ran_at.parent().unwrap().exists()
        });
    }

    #[cfg(unix)]
    #[test]
    fn an_installer_left_open_past_the_deadline_is_reported() {
        let out = tempfile::tempdir().unwrap();
        let exe = script(&out.path().join("ran-at"), "sleep 1; exit 0");
        let err = run_installer(
            &exe,
            "setup",
            &|| false,
            Duration::from_millis(20),
            Duration::ZERO,
        )
        .unwrap_err();
        assert!(err.contains("still open"), "{err}");
    }
}
