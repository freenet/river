//! One River per user. A second launch asks the first to show its window, then exits.
//!
//! The first instance holds an exclusive lock on `instance.lock` in River's config directory.
//! The operating system drops the lock when the process ends, however it ends, so a crash
//! leaves nothing to clean up and nothing to mistake for a running River. The same instance
//! listens on a loopback port and writes the port and a random token to `instance.port`; a
//! second launch that finds the lock taken reads that file and sends the token and a request
//! to show. The token keeps another user on the same computer from raising this user's window.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

const LOCK_FILE: &str = "instance.lock";
const PORT_FILE: &str = "instance.port";
/// The first word of a request.
const SHOW: &str = "river-show";
/// The longest request read: a token and a window-system activation token fit easily.
const MAX_REQUEST: u64 = 1024;
/// How long a connection may take to say its one line, so one that never does cannot hold up
/// the next.
const READ_TIMEOUT: Duration = Duration::from_secs(2);
/// How many times a second launch tries to reach the first, which holds the lock a moment
/// before it has written its port, and how long it waits between tries.
const TRIES: u32 = 20;
const TRY_WAIT: Duration = Duration::from_millis(100);

/// The lock of the most recent guard, weakly, so dropping the guard still releases it.
static CURRENT: Mutex<Weak<Mutex<Option<File>>>> = Mutex::new(Weak::new());

/// Let go of this process's lock now, so a new River started in its place (a restart into an
/// update that waits for the new one to confirm it started) is first, not a second launch.
pub fn release_current() {
    let lock = CURRENT.lock().unwrap_or_else(|p| p.into_inner()).upgrade();
    if let Some(lock) = lock {
        drop(lock.lock().unwrap_or_else(|p| p.into_inner()).take());
    }
}

/// What launching found.
pub enum Start {
    /// This is the only River: keep the guard for as long as the app runs.
    First(Guard),
    /// Another River runs. `asked` is whether it was told to show its window.
    Running { asked: bool },
}

/// The right to be the only River, held until dropped.
pub struct Guard {
    /// Held open so the lock stays; [`release_current`] lets it go early.
    _lock: Arc<Mutex<Option<File>>>,
    listener: TcpListener,
    token: String,
}

/// Take the lock, or ask the River that has it to show itself. `activation` is the window
/// system's token for raising a window (the second launch's `XDG_ACTIVATION_TOKEN`), passed
/// on so the first can take focus on Wayland.
pub fn acquire(dir: &Path, activation: Option<&str>) -> std::io::Result<Start> {
    std::fs::create_dir_all(dir)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(LOCK_FILE))?;
    match lock.try_lock() {
        Ok(()) => {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
            let token = new_token()?;
            write_port(&dir.join(PORT_FILE), listener.local_addr()?.port(), &token)?;
            let lock = Arc::new(Mutex::new(Some(lock)));
            *CURRENT.lock().unwrap_or_else(|p| p.into_inner()) = Arc::downgrade(&lock);
            Ok(Start::First(Guard {
                _lock: lock,
                listener,
                token,
            }))
        }
        Err(TryLockError::WouldBlock) => Ok(Start::Running {
            asked: ask_to_show(dir, activation, TRIES, TRY_WAIT),
        }),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

impl Guard {
    /// Answer requests to show the window, on a thread of its own. `show` gets the activation
    /// token the second launch sent, if any.
    pub fn serve(&self, show: impl Fn(Option<String>) + Send + 'static) -> std::io::Result<()> {
        let listener = self.listener.try_clone()?;
        let token = self.token.clone();
        std::thread::Builder::new()
            .name("single-instance".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    if let Some(activation) = read_request(stream, &token) {
                        show(activation);
                    }
                }
            })?;
        Ok(())
    }
}

/// The activation token a valid request carried (`Some(None)` for one with no token), or
/// `None` for anything else.
fn read_request(stream: TcpStream, token: &str) -> Option<Option<String>> {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let mut line = String::new();
    BufReader::new(stream.take(MAX_REQUEST))
        .read_line(&mut line)
        .ok()?;
    let mut words = line.split_whitespace();
    (words.next()? == SHOW && words.next()? == token).then(|| words.next().map(str::to_owned))
}

fn new_token() -> std::io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The port and token, readable by this user only.
fn write_port(path: &Path, port: u16, token: &str) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp: PathBuf = dir.join(format!(".{PORT_FILE}.{}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = options
        .open(&tmp)
        .and_then(|mut f| f.write_all(format!("{port}\n{token}\n").as_bytes()))
        .and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn read_port(dir: &Path) -> Option<(u16, String)> {
    let text = std::fs::read_to_string(dir.join(PORT_FILE)).ok()?;
    let mut lines = text.lines();
    let port = lines.next()?.trim().parse().ok()?;
    let token = lines.next()?.trim().to_owned();
    (!token.is_empty()).then_some((port, token))
}

/// Tell the River holding the lock to show its window. Whether it answered the connection.
fn ask_to_show(dir: &Path, activation: Option<&str>, tries: u32, wait: Duration) -> bool {
    for attempt in 0..tries {
        if attempt > 0 {
            std::thread::sleep(wait);
        }
        let Some((port, token)) = read_port(dir) else {
            continue;
        };
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let Ok(mut stream) = TcpStream::connect_timeout(&addr, READ_TIMEOUT) else {
            continue;
        };
        let activation = activation.filter(|a| !a.contains(char::is_whitespace));
        let request = format!("{SHOW} {token} {}\n", activation.unwrap_or(""));
        if stream.write_all(request.as_bytes()).is_ok() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn first(dir: &Path) -> Guard {
        match acquire(dir, None).unwrap() {
            Start::First(g) => g,
            Start::Running { .. } => panic!("expected to be first"),
        }
    }

    fn served(guard: &Guard) -> mpsc::Receiver<Option<String>> {
        let (tx, rx) = mpsc::channel();
        guard.serve(move |a| tx.send(a).unwrap()).unwrap();
        rx
    }

    /// A bound, never-failing wait: a broken guard fails the test rather than hanging it.
    fn next(rx: &mpsc::Receiver<Option<String>>) -> Option<String> {
        rx.recv_timeout(Duration::from_secs(30))
            .expect("the first instance was asked to show")
    }

    #[test]
    fn a_second_launch_asks_the_first_to_show_and_is_not_first() {
        let dir = tempfile::tempdir().unwrap();
        let guard = first(dir.path());
        let shown = served(&guard);
        match acquire(dir.path(), Some("token-1")).unwrap() {
            Start::Running { asked } => assert!(asked),
            Start::First(_) => panic!("a second River must not start"),
        }
        assert_eq!(next(&shown), Some("token-1".to_owned()));
        // And again: the first keeps answering.
        assert!(matches!(
            acquire(dir.path(), None).unwrap(),
            Start::Running { asked: true }
        ));
        assert_eq!(next(&shown), None);
    }

    #[test]
    fn a_leftover_lock_and_port_file_from_a_dead_run_do_not_block_a_start() {
        let dir = tempfile::tempdir().unwrap();
        // What a crashed run leaves: both files, no process holding the lock, and a port
        // nothing listens on.
        let dead_port = {
            let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            l.local_addr().unwrap().port()
        };
        std::fs::write(dir.path().join(LOCK_FILE), "").unwrap();
        std::fs::write(dir.path().join(PORT_FILE), format!("{dead_port}\nold\n")).unwrap();
        let guard = first(dir.path());
        // The new run wrote its own port, which the next launch reaches.
        let shown = served(&guard);
        assert!(matches!(
            acquire(dir.path(), None).unwrap(),
            Start::Running { asked: true }
        ));
        next(&shown);
    }

    #[test]
    fn releasing_the_lock_early_lets_a_replacement_be_first() {
        let dir = tempfile::tempdir().unwrap();
        let guard = first(dir.path());
        release_current();
        let _second = first(dir.path());
        drop(guard);
    }

    #[test]
    fn dropping_the_guard_lets_the_next_launch_be_first() {
        let dir = tempfile::tempdir().unwrap();
        drop(first(dir.path()));
        drop(first(dir.path()));
    }

    #[test]
    fn a_request_with_the_wrong_token_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let guard = first(dir.path());
        let shown = served(&guard);
        let (port, token) = read_port(dir.path()).unwrap();
        let send = |line: String| {
            let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            s.write_all(line.as_bytes()).unwrap();
        };
        send(format!("{SHOW} not-the-token\n"));
        send("garbage\n".to_owned());
        send(format!("something-else {token}\n"));
        send(format!("{SHOW} {token} real\n"));
        // Requests are answered in order, so the first one to arrive is the only valid one.
        assert_eq!(next(&shown), Some("real".to_owned()));
        assert!(shown.try_recv().is_err());
    }

    #[test]
    fn the_port_file_is_for_this_user_only() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = first(dir.path());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join(PORT_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "mode {mode:o}");
        }
        assert!(read_port(dir.path()).is_some());
    }

    #[test]
    fn a_second_launch_with_nobody_listening_reports_that_it_could_not_ask() {
        let dir = tempfile::tempdir().unwrap();
        // The lock is held, but no port was ever written: the holder is not River.
        let lock = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(dir.path().join(LOCK_FILE))
            .unwrap();
        lock.try_lock().unwrap();
        assert!(!ask_to_show(dir.path(), None, 1, Duration::ZERO));
    }
}
