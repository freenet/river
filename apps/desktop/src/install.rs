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
        install_binary(&extract_freenet(bytes)?, &dest)
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
/// (`host::start_binary`), which also handles systems without a usable service manager.
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
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
    }
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
        let mut entry = entry.map_err(|e| format!("bad release archive: {e}"))?;
        let is_freenet = entry
            .path()
            .ok()
            .and_then(|p| p.file_name().map(|n| n == "freenet"))
            .unwrap_or(false);
        if is_freenet && entry.header().entry_type().is_file() {
            let mut binary = Vec::new();
            entry
                .read_to_end(&mut binary)
                .map_err(|e| format!("bad release archive: {e}"))?;
            return Ok(binary);
        }
    }
    Err("the release archive has no freenet binary".into())
}

/// A private scratch directory: randomly named, readable only by this user, removed on drop.
fn scratch_dir() -> Result<tempfile::TempDir, String> {
    tempfile::Builder::new()
        .prefix("river-freenet-")
        .tempdir()
        .map_err(|e| format!("couldn't create a temporary directory: {e}"))
}

/// macOS: copy Freenet.app out of the signed DMG into /Applications (or ~/Applications).
fn install_mac_app(dmg: &[u8]) -> Result<(), String> {
    let scratch = scratch_dir()?;
    let image = scratch.path().join("Freenet.dmg");
    let mount = scratch.path().join("mnt");
    fs::write(&image, dmg).map_err(|e| e.to_string())?;
    fs::create_dir(&mount).map_err(|e| e.to_string())?;
    run(Command::new("/usr/bin/hdiutil")
        .arg("attach")
        .args(["-nobrowse", "-readonly", "-noautoopen", "-mountpoint"])
        .arg(&mount)
        .arg(&image))?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let copied = applications_dir(Path::new("/Applications"), home.as_deref())
        .and_then(|dest| copy_app_from(&mount, &dest));
    let _ = run(Command::new("/usr/bin/hdiutil")
        .args(["detach", "-quiet"])
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
    let staging = tempfile::Builder::new()
        .prefix(".Freenet.app.river-")
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
    let scratch = scratch_dir()?;
    let installer = scratch.path().join("freenet.exe");
    fs::write(&installer, exe).map_err(|e| e.to_string())?;
    let mut cmd = Command::new(&installer);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("couldn't run the Freenet installer: {e}"))?;
    let deadline = Instant::now() + INSTALLER_DEADLINE;
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return if status.success() {
                Ok(())
            } else {
                Err(format!("the Freenet installer stopped ({status})"))
            };
        }
        if node_up() {
            // The node may be running from the scratch directory; leave it in place.
            let _ = scratch.keep();
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err("the Freenet installer is still open; finish it, then try again".into());
        }
        thread::sleep(Duration::from_millis(500));
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
}
