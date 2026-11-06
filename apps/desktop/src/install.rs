//! Putting a verified Freenet release on disk, per platform. Every function here receives bytes
//! whose SHA-256 has already matched the release's signed manifest.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use river_app::InstallStep;

use crate::host::run;

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

pub fn install(
    asset: &str,
    bytes: &[u8],
    progress: &mut dyn FnMut(InstallStep),
) -> Result<(), String> {
    let scratch = Scratch::new()?;
    if asset.ends_with(".tar.gz") {
        progress(InstallStep::Installing);
        install_binary_from_tarball(bytes)
    } else if asset.ends_with(".dmg") {
        progress(InstallStep::Installing);
        install_mac_app(&scratch.0, bytes)
    } else if asset.ends_with(".exe") {
        progress(InstallStep::WaitingForInstaller);
        run_windows_installer(&scratch.0, bytes)
    } else {
        Err(format!("don't know how to install {asset}"))
    }
}

/// Linux: unpack `freenet` into `~/.local/bin` and register it as a systemd user service.
fn install_binary_from_tarball(tarball: &[u8]) -> Result<(), String> {
    let binary = extract_freenet(tarball)?;
    let dest = binary_install_path().ok_or("couldn't find your home directory")?;
    let dir = dest.parent().expect("install path has a parent");
    fs::create_dir_all(dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
    // Write beside the destination and rename, so a half-written binary is never runnable.
    let tmp = dir.join(".freenet.river-install");
    fs::write(&tmp, &binary).map_err(|e| format!("couldn't write {}: {e}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp, &dest).map_err(|e| format!("couldn't install {}: {e}", dest.display()))?;
    run(Command::new(&dest).args(["service", "install"]))
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

/// macOS: copy Freenet.app out of the signed DMG into /Applications (or ~/Applications).
fn install_mac_app(scratch: &Path, dmg: &[u8]) -> Result<(), String> {
    let image = scratch.join("Freenet.dmg");
    let mount = scratch.join("mnt");
    fs::write(&image, dmg).map_err(|e| e.to_string())?;
    fs::create_dir_all(&mount).map_err(|e| e.to_string())?;
    run(Command::new("hdiutil")
        .arg("attach")
        .args(["-nobrowse", "-readonly", "-noautoopen", "-mountpoint"])
        .arg(&mount)
        .arg(&image))?;
    let copied = copy_app_from(&mount);
    let _ = run(Command::new("hdiutil")
        .args(["detach", "-quiet"])
        .arg(&mount));
    copied
}

fn copy_app_from(mount: &Path) -> Result<(), String> {
    let app = fs::read_dir(mount)
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "app"))
        .ok_or("the disk image has no app in it")?;
    let dest_dir = applications_dir()?;
    run(Command::new("ditto")
        .arg(&app)
        .arg(dest_dir.join("Freenet.app")))
}

/// `/Applications` when this user can write to it, else `~/Applications`.
fn applications_dir() -> Result<PathBuf, String> {
    let system = PathBuf::from("/Applications");
    let probe = system.join(".river-write-test");
    if fs::write(&probe, b"").is_ok() {
        let _ = fs::remove_file(&probe);
        return Ok(system);
    }
    let home = std::env::var_os("HOME").ok_or("couldn't find your home directory")?;
    let user = PathBuf::from(home).join("Applications");
    fs::create_dir_all(&user).map_err(|e| e.to_string())?;
    Ok(user)
}

/// Windows: the signed `freenet.exe` is its own installer. Run with no arguments, it opens a
/// setup wizard that installs Freenet, registers it to start at login, and starts it.
fn run_windows_installer(scratch: &Path, exe: &[u8]) -> Result<(), String> {
    let installer = scratch.join("freenet.exe");
    fs::write(&installer, exe).map_err(|e| e.to_string())?;
    run(&mut Command::new(&installer))
        .map_err(|_| "the Freenet installer closed before it finished".to_string())
}

/// A private temporary directory, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Scratch, String> {
        let dir =
            std::env::temp_dir().join(format!("river-freenet-install-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
        Ok(Scratch(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tarball(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut builder = tar::Builder::new(gz);
        for (path, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, path, *data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn extracts_only_the_freenet_binary() {
        let archive = tarball(&[("README.md", b"docs"), ("freenet", b"\x7fELF binary")]);
        assert_eq!(extract_freenet(&archive).unwrap(), b"\x7fELF binary");
    }

    #[test]
    fn finds_freenet_inside_a_directory_but_not_a_lookalike() {
        let archive = tarball(&[
            ("freenet-x/freenet-helper", b"no"),
            ("freenet-x/freenet", b"yes"),
        ]);
        assert_eq!(extract_freenet(&archive).unwrap(), b"yes");
        assert!(extract_freenet(&tarball(&[("fdev", b"no")])).is_err());
    }
}
