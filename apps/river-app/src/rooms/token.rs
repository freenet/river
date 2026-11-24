//! Keeping the grant token between runs, readable by this user only.
//!
//! The token opens River's delegate storage, which holds each room's signing key, so it is a
//! credential: the file is created owner-only (`0600`, in a `0700` directory) on Unix and
//! written whole or not at all. On Windows it lives in the user's own application data
//! directory, whose default permissions already admit only that user. It is never logged.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::grant::GrantToken;

const FILE_NAME: &str = "node-grant-token";

/// The directory that holds the saved grant for the node at `authority`, inside the app's
/// config directory `dir`. A grant is given by one node and means nothing to another, so each
/// node River is pointed at keeps its own, and going back to a node finds the permission it
/// already gave.
///
/// `explicit` says the person pointed River at this node (by flag, variable or the app's
/// screens). Only the node River finds on its own, the Freenet on this computer at the default
/// address, keeps the directory itself, where its grant has always been. A node chosen
/// explicitly always gets a directory of its own, even at `127.0.0.1:7509`, because that
/// address can then be the end of a tunnel to another machine, and one node's token must
/// never be shown to the other. (A grant saved by an earlier River for such an address is not
/// reused: the node is asked again once.) The name is a hash of the address, so a long host
/// name cannot make a path too long.
pub fn node_dir(dir: &Path, authority: &str, explicit: bool) -> PathBuf {
    if !explicit && authority == crate::address::DEFAULT_AUTHORITY {
        return dir.to_owned();
    }
    let hash = blake3::hash(authority.as_bytes());
    let name: String = hash.as_bytes()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    dir.join("nodes").join(name)
}

pub struct TokenStore {
    path: PathBuf,
}

impl TokenStore {
    /// A store in `dir`, the app's config directory.
    pub fn in_dir(dir: &Path) -> Self {
        TokenStore {
            path: dir.join(FILE_NAME),
        }
    }

    pub fn load(&self) -> Option<GrantToken> {
        let text = fs::read_to_string(&self.path).ok()?;
        let token = text.trim();
        (!token.is_empty()).then(|| GrantToken::new(token.to_owned()))
    }

    pub fn save(&self, token: &GrantToken) -> Result<(), String> {
        let dir = self.path.parent().ok_or("bad token path")?;
        create_private_dir(dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
        let err = |e: std::io::Error| format!("couldn't save the Freenet grant: {e}");
        // A name of its own, so two saves at once (two app windows) can't write one file.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = dir.join(format!(".{FILE_NAME}.{}.{n}.tmp", std::process::id()));
        let mut file = private_file(&tmp).map_err(err)?;
        let written = file
            .write_all(token.expose().as_bytes())
            .and_then(|()| file.sync_all());
        drop(file);
        let _lock = self.lock(dir);
        if let Err(e) = written.and_then(|()| fs::rename(&tmp, &self.path)) {
            let _ = fs::remove_file(&tmp);
            return Err(err(e));
        }
        sync_dir(dir);
        Ok(())
    }

    /// Forget `rejected`, which the node said names nothing. The file is removed only if it
    /// still holds that token: another window may have saved a newer grant in the meantime,
    /// which is returned to use instead.
    pub fn discard(&self, rejected: &GrantToken) -> Option<GrantToken> {
        // Held across the read and the delete, so a save from another window can't land
        // between them and be deleted.
        let _lock = self.path.parent().and_then(|dir| self.lock(dir));
        match self.load() {
            Some(t) if &t == rejected => {
                let _ = fs::remove_file(&self.path);
                None
            }
            other => other,
        }
    }
}

impl TokenStore {
    /// An exclusive advisory lock on the token, released when dropped. `None` if it can't
    /// be taken; the caller goes ahead unlocked, as before locking existed.
    fn lock(&self, dir: &Path) -> Option<fs::File> {
        let file = lock_file(&dir.join(format!(".{FILE_NAME}.lock"))).ok()?;
        file.lock().ok()?;
        Some(file)
    }
}

#[cfg(unix)]
pub(super) fn lock_file(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
pub(super) fn lock_file(path: &Path) -> std::io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

/// Make the rename durable. Best effort, and only meaningful on Unix.
pub(super) fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

#[cfg(unix)]
pub(super) fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if !dir.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    // A directory made earlier by something else may be wider; narrow it.
    let mode = fs::metadata(dir)?.permissions().mode();
    if mode & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(mode & 0o700))?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)
}

#[cfg(unix)]
pub(super) fn private_file(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
pub(super) fn private_file(path: &Path) -> std::io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "river-token-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn saves_loads_and_discards() {
        let dir = temp_dir().join("nested");
        let store = TokenStore::in_dir(&dir);
        assert_eq!(store.load(), None);
        store.save(&GrantToken::new("abc".into())).unwrap();
        assert_eq!(store.load(), Some(GrantToken::new("abc".into())));
        store.save(&GrantToken::new("def".into())).unwrap();
        assert_eq!(store.load(), Some(GrantToken::new("def".into())));
        // A different token on disk is kept, and handed back.
        assert_eq!(
            store.discard(&GrantToken::new("abc".into())),
            Some(GrantToken::new("def".into()))
        );
        assert_eq!(store.load(), Some(GrantToken::new("def".into())));
        assert_eq!(store.discard(&GrantToken::new("def".into())), None);
        assert_eq!(store.load(), None);
        let _ = fs::remove_dir_all(dir.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn the_file_and_its_directory_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_dir();
        let dir = root.join("cfg");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let store = TokenStore::in_dir(&dir);
        store.save(&GrantToken::new("abc".into())).unwrap();
        let file_mode = fs::metadata(dir.join(FILE_NAME))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(file_mode & 0o777, 0o600);
        let dir_mode = fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(dir_mode & 0o077, 0);
        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod node_dir_tests {
    use super::*;

    const D: &str = "/cfg";

    #[test]
    fn the_node_on_this_computer_keeps_the_directory_it_always_had() {
        assert_eq!(
            node_dir(Path::new(D), crate::address::DEFAULT_AUTHORITY, false),
            Path::new(D)
        );
    }

    #[test]
    fn each_other_node_gets_its_own_directory() {
        let dir = Path::new(D);
        let a = node_dir(dir, "localhost:40302", true);
        let b = node_dir(dir, "localhost:40303", true);
        assert_ne!(a, b);
        assert_ne!(a, dir);
        assert_ne!(b, dir);
        assert_eq!(a, node_dir(dir, "localhost:40302", true));
        // Not at the config dir's top level, where the local grant is.
        assert!(a.starts_with(dir.join("nodes")));
        // A node found on its own at another address is a different node too.
        assert_ne!(node_dir(dir, "localhost:40302", false), dir);
    }

    #[test]
    fn a_tunnel_to_the_default_address_never_shares_the_local_nodes_grant() {
        let dir = Path::new(D);
        let local = node_dir(dir, crate::address::DEFAULT_AUTHORITY, false);
        let tunnel = node_dir(dir, crate::address::DEFAULT_AUTHORITY, true);
        assert_ne!(local, tunnel);
        assert_ne!(tunnel, dir);
        // And the real stores: a token saved for one is not found by the other.
        let tmp = std::env::temp_dir().join(format!("river-node-dir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let local = TokenStore::in_dir(&node_dir(&tmp, crate::address::DEFAULT_AUTHORITY, false));
        let tunnel = TokenStore::in_dir(&node_dir(&tmp, crate::address::DEFAULT_AUTHORITY, true));
        local.save(&GrantToken::new("local-token".into())).unwrap();
        assert!(tunnel.load().is_none());
        tunnel
            .save(&GrantToken::new("tunnel-token".into()))
            .unwrap();
        assert_eq!(local.load().unwrap().expose(), "local-token");
        // Discarding the tunnel's rejected token leaves the local grant alone.
        assert!(
            tunnel
                .discard(&GrantToken::new("tunnel-token".into()))
                .is_none()
        );
        assert_eq!(local.load().unwrap().expose(), "local-token");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_token_saved_at_the_default_is_found_when_the_link_opens_at_the_default() {
        let tmp = std::env::temp_dir().join(format!("river-node-dir-up-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        // As an earlier River saved it: in the config dir itself.
        TokenStore::in_dir(&tmp)
            .save(&GrantToken::new("old".into()))
            .unwrap();
        let found = TokenStore::in_dir(&node_dir(&tmp, crate::address::DEFAULT_AUTHORITY, false));
        assert_eq!(found.load().unwrap().expose(), "old");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_long_host_name_does_not_make_a_long_path() {
        let host = format!("{}.example:7509", "a".repeat(63));
        let name = node_dir(Path::new(D), &host, true);
        assert!(name.file_name().unwrap().len() <= 32);
    }
}
