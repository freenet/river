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
        match self.load() {
            Some(t) if &t == rejected => {
                let _ = fs::remove_file(&self.path);
                None
            }
            other => other,
        }
    }
}

/// Make the rename durable. Best effort, and only meaningful on Unix.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
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
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)
}

#[cfg(unix)]
fn private_file(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn private_file(path: &Path) -> std::io::Result<fs::File> {
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
