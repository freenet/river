//! One River per user. A second launch asks the first to show its window, then exits.
//!
//! The first instance holds an exclusive lock on `instance.lock` in River's config directory.
//! The operating system drops the lock when the process ends, however it ends, so a crash
//! leaves nothing to clean up and nothing to mistake for a running River. The same instance
//! listens on a loopback port and writes the port and a random token to `instance.port`; a
//! second launch that finds the lock taken reads that file and sends the token and a request
//! to show, and waits for the first to say it did. The token keeps another user on the same
//! computer from raising this user's window.
//!
//! A River that starts its replacement (a restart into an update) hands the lock over by
//! letting go of it first ([`Guard::release`]) and takes it back if the replacement does not
//! start ([`Guard::reacquire`]); [`release_current`] and [`reacquire_current`] do that for
//! the guard the app registered. They are the only process-wide state here, and the tests do
//! not use them.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

const LOCK_FILE: &str = "instance.lock";
const PORT_FILE: &str = "instance.port";
/// The first word of a request.
const SHOW: &str = "river-show";
/// What the first instance sends back once it has acted on a valid request.
const ACK: u8 = b'1';
/// The longest request read: a token and a window-system activation token fit easily.
const MAX_REQUEST: usize = 1024;
/// How long one connection may take in all, to say its line and hear the answer, so one that
/// trickles bytes cannot hold a thread for long.
const CONNECTION_DEADLINE: Duration = Duration::from_secs(3);
/// How many connections are handled at once. The rest are dropped.
const MAX_CONNECTIONS: usize = 8;
/// How many times a second launch tries, and how long it waits between tries. The holder has
/// the lock a moment before it has written its port, and may be on its way out.
const TRIES: u32 = 20;
const TRY_WAIT: Duration = Duration::from_millis(100);
/// How long a River that handed its lock over waits to take it back.
const REACQUIRE_TRIES: u32 = 20;
const REACQUIRE_WAIT: Duration = Duration::from_millis(250);

/// What launching found.
pub enum Start {
    /// This is the only River: keep the guard for as long as the app runs.
    First(Guard),
    /// Another River holds the lock. `asked` is whether it answered a request to show its
    /// window.
    Running { asked: bool },
}

/// The right to be the only River, held until dropped.
pub struct Guard {
    dir: PathBuf,
    /// Held open so the lock stays; `None` while it is handed over.
    lock: Mutex<Option<File>>,
    listener: TcpListener,
    token: String,
}

/// The guard the app registered, weakly, so dropping it still releases the lock.
static CURRENT: Mutex<Weak<Guard>> = Mutex::new(Weak::new());

/// Make `guard` the one [`release_current`] and [`reacquire_current`] act on.
pub fn register(guard: &Arc<Guard>) {
    *CURRENT.lock().unwrap_or_else(|p| p.into_inner()) = Arc::downgrade(guard);
}

/// Let go of this process's lock now, for a replacement about to start.
pub fn release_current() {
    let guard = CURRENT.lock().unwrap_or_else(|p| p.into_inner()).upgrade();
    if let Some(g) = guard {
        g.release();
    }
}

/// Take the lock back after a replacement did not start, trying for a moment: the replacement
/// may have left a process behind that is still letting go. Tells the user when it cannot.
pub fn reacquire_current() {
    let guard = CURRENT.lock().unwrap_or_else(|p| p.into_inner()).upgrade();
    if let Some(g) = guard
        && !matches!(
            g.reacquire_within(REACQUIRE_TRIES, REACQUIRE_WAIT),
            Ok(true)
        )
    {
        crate::tell_user(
            "River could not take back its place as the only running River: another River \
             may be running. Quit River from its tray icon or window and start it again.",
        );
    }
}

/// Take the lock, or ask the River that has it to show itself. `activation` is the window
/// system's token for raising a window (the second launch's `XDG_ACTIVATION_TOKEN`), passed
/// on so the first can take focus on Wayland.
pub fn acquire(dir: &Path, activation: Option<&str>) -> std::io::Result<Start> {
    acquire_with(dir, activation, TRIES, TRY_WAIT)
}

fn acquire_with(
    dir: &Path,
    activation: Option<&str>,
    tries: u32,
    wait: Duration,
) -> std::io::Result<Start> {
    std::fs::create_dir_all(dir)?;
    let lock = open_lock(dir)?;
    for attempt in 0..tries.max(1) {
        if attempt > 0 {
            std::thread::sleep(wait);
        }
        // Each round tries the lock again: a holder that is exiting lets it go, and then this
        // launch should start rather than give up.
        match lock.try_lock() {
            Ok(()) => return first(dir, lock),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(e)) => return Err(e),
        }
        if ask_once(dir, activation) {
            return Ok(Start::Running { asked: true });
        }
    }
    Ok(Start::Running { asked: false })
}

fn open_lock(dir: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(LOCK_FILE))
}

fn first(dir: &Path, lock: File) -> std::io::Result<Start> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let token = new_token()?;
    write_port(&dir.join(PORT_FILE), listener.local_addr()?.port(), &token)?;
    Ok(Start::First(Guard {
        dir: dir.to_owned(),
        lock: Mutex::new(Some(lock)),
        listener,
        token,
    }))
}

impl Guard {
    /// Let go of the lock, so a River started now is first. Requests already queued on the
    /// port still get answered.
    pub fn release(&self) {
        drop(self.lock.lock().unwrap_or_else(|p| p.into_inner()).take());
    }

    /// Take the lock back, and put this instance's port and token in the port file again (a
    /// replacement that started and failed may have written its own). `Ok(false)` when
    /// something else holds the lock now.
    pub fn reacquire(&self) -> std::io::Result<bool> {
        let mut held = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        if held.is_some() {
            return Ok(true);
        }
        let lock = open_lock(&self.dir)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(false),
            Err(TryLockError::Error(e)) => return Err(e),
        }
        write_port(
            &self.dir.join(PORT_FILE),
            self.listener.local_addr()?.port(),
            &self.token,
        )?;
        *held = Some(lock);
        Ok(true)
    }

    /// [`reacquire`](Self::reacquire), tried up to `tries` times with `wait` between.
    pub fn reacquire_within(&self, tries: u32, wait: Duration) -> std::io::Result<bool> {
        let mut last = Ok(false);
        for attempt in 0..tries.max(1) {
            if attempt > 0 {
                std::thread::sleep(wait);
            }
            last = self.reacquire();
            if matches!(last, Ok(true)) {
                break;
            }
        }
        last
    }

    /// Answer requests to show the window, each on a thread of its own. `show` gets the
    /// activation token the second launch sent, if any.
    pub fn serve(
        &self,
        show: impl Fn(Option<String>) + Send + Sync + 'static,
    ) -> std::io::Result<()> {
        let listener = self.listener.try_clone()?;
        let token = self.token.clone();
        let show = Arc::new(show);
        let busy = Arc::new(AtomicUsize::new(0));
        std::thread::Builder::new()
            .name("single-instance".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    // `incoming` never ends, so a persistent accept error (out of file
                    // handles) would spin: pause briefly and go on.
                    let Ok(stream) = stream else {
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    };
                    if busy.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
                        busy.fetch_sub(1, Ordering::AcqRel);
                        continue;
                    }
                    let (token, show, busy) = (token.clone(), show.clone(), busy.clone());
                    let spawned = std::thread::Builder::new()
                        .name("single-instance-request".into())
                        .spawn({
                            let busy = busy.clone();
                            move || {
                                handle(stream, &token, &*show);
                                busy.fetch_sub(1, Ordering::AcqRel);
                            }
                        });
                    if spawned.is_err() {
                        busy.fetch_sub(1, Ordering::AcqRel);
                    }
                }
            })?;
        Ok(())
    }
}

/// One connection: read a line within the deadline, act on it if it is a valid request, say so.
fn handle(mut stream: TcpStream, token: &str, show: &(impl Fn(Option<String>) + ?Sized)) {
    let Some(line) = read_line(&mut stream, Instant::now() + CONNECTION_DEADLINE) else {
        return;
    };
    if let Some(activation) = parse_request(&line, token) {
        show(activation);
        let _ = stream.write_all(&[ACK]);
    }
}

/// A line of at most [`MAX_REQUEST`] bytes, all of it read before `deadline`.
fn read_line(stream: &mut TcpStream, deadline: Instant) -> Option<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        // A socket read timeout is safe here: this is a one-shot loopback request line, and a
        // read that times out (or ends in any error) returns `None` and the connection is
        // dropped, which is what Windows documents for a socket whose receive timed out
        // ("the connection is in an indeterminate state and should be closed"). Nothing
        // reads from it again.
        stream
            .set_read_timeout(Some(left.max(Duration::from_millis(1))))
            .ok()?;
        match stream.read(&mut byte) {
            Ok(1) if byte[0] == b'\n' => return String::from_utf8(line).ok(),
            Ok(1) if line.len() < MAX_REQUEST => line.push(byte[0]),
            _ => return None,
        }
    }
}

/// The activation token a valid request carried (`Some(None)` for one with no token), or
/// `None` for anything else.
fn parse_request(line: &str, token: &str) -> Option<Option<String>> {
    let mut words = line.split_whitespace();
    (words.next()? == SHOW && same(words.next()?, token)).then(|| words.next().map(str::to_owned))
}

/// Equality that takes as long for a near miss as for a match.
fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
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

/// One try at telling the River holding the lock to show its window. True only when it
/// answered that it did.
fn ask_once(dir: &Path, activation: Option<&str>) -> bool {
    let Some((port, token)) = read_port(dir) else {
        return false;
    };
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, CONNECTION_DEADLINE) else {
        return false;
    };
    let _ = stream.set_write_timeout(Some(CONNECTION_DEADLINE));
    let activation = activation.filter(|a| !a.contains(char::is_whitespace));
    let request = format!("{SHOW} {token} {}\n", activation.unwrap_or(""));
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    // As in `read_line`: a timeout or any error here ends the attempt and the stream is
    // dropped, so a socket left indeterminate by a timeout is never read again.
    let _ = stream.set_read_timeout(Some(CONNECTION_DEADLINE));
    let mut answer = [0u8; 1];
    matches!(stream.read(&mut answer), Ok(1) if answer[0] == ACK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn first_in(dir: &Path) -> Guard {
        match acquire_with(dir, None, 1, Duration::ZERO).unwrap() {
            Start::First(g) => g,
            Start::Running { .. } => panic!("expected to be first"),
        }
    }

    fn second(dir: &Path, activation: Option<&str>) -> bool {
        match acquire_with(dir, activation, 3, Duration::ZERO).unwrap() {
            Start::Running { asked } => asked,
            Start::First(_) => panic!("a second River must not start"),
        }
    }

    fn served(guard: &Guard) -> mpsc::Receiver<Option<String>> {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        guard
            .serve(move |a| tx.lock().unwrap().send(a).unwrap())
            .unwrap();
        rx
    }

    /// A bounded wait: a broken guard fails the test rather than hanging it.
    fn next(rx: &mpsc::Receiver<Option<String>>) -> Option<String> {
        rx.recv_timeout(Duration::from_secs(30))
            .expect("the first instance was asked to show")
    }

    #[test]
    fn a_second_launch_asks_the_first_to_show_and_hears_that_it_did() {
        let dir = tempfile::tempdir().unwrap();
        let guard = first_in(dir.path());
        let shown = served(&guard);
        assert!(second(dir.path(), Some("token-1")));
        assert_eq!(next(&shown), Some("token-1".to_owned()));
        assert!(second(dir.path(), None));
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
        let guard = first_in(dir.path());
        let shown = served(&guard);
        assert!(second(dir.path(), None));
        next(&shown);
    }

    #[test]
    fn dropping_the_guard_lets_the_next_launch_be_first() {
        let dir = tempfile::tempdir().unwrap();
        drop(first_in(dir.path()));
        drop(first_in(dir.path()));
    }

    #[test]
    fn releasing_the_lock_lets_a_replacement_be_first() {
        let dir = tempfile::tempdir().unwrap();
        let guard = first_in(dir.path());
        guard.release();
        let replacement = first_in(dir.path());
        // The replacement owns the lock now: taking it back is refused.
        assert!(!guard.reacquire().unwrap());
        drop(replacement);
    }

    #[test]
    fn a_failed_handover_takes_the_lock_back_so_a_second_launch_is_not_first() {
        let dir = tempfile::tempdir().unwrap();
        let guard = first_in(dir.path());
        let shown = served(&guard);
        guard.release();
        // The replacement started far enough to write its own port, then failed.
        let (port, _) = read_port(dir.path()).unwrap();
        write_port(
            &dir.path().join(PORT_FILE),
            port.wrapping_add(1),
            "failed-child",
        )
        .unwrap();
        assert!(guard.reacquire().unwrap());
        // A launch after the failure finds the lock taken and reaches the original instance.
        assert!(second(dir.path(), Some("again")));
        assert_eq!(next(&shown), Some("again".to_owned()));
        // Taking it back twice is harmless.
        assert!(guard.reacquire().unwrap());
    }

    #[test]
    fn taking_the_lock_back_keeps_trying_and_reports_when_it_cannot() {
        let dir = tempfile::tempdir().unwrap();
        let guard = first_in(dir.path());
        guard.release();
        // Something else (a process the replacement left behind) holds the lock.
        let holder = open_lock(dir.path()).unwrap();
        holder.try_lock().unwrap();
        assert!(!guard.reacquire_within(3, Duration::ZERO).unwrap());
        // It lets go; the next attempt takes the lock back.
        drop(holder);
        assert!(guard.reacquire_within(3, Duration::ZERO).unwrap());
    }

    #[test]
    fn a_request_with_the_wrong_token_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let guard = first_in(dir.path());
        let shown = served(&guard);
        let (port, token) = read_port(dir.path()).unwrap();
        let send = |line: String| {
            let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            s.write_all(line.as_bytes()).unwrap();
            let mut answer = Vec::new();
            let _ = s.read_to_end(&mut answer);
            answer
        };
        assert!(send(format!("{SHOW} not-the-token\n")).is_empty());
        assert!(send("garbage\n".to_owned()).is_empty());
        assert!(send(format!("something-else {token}\n")).is_empty());
        assert_eq!(send(format!("{SHOW} {token} real\n")), [ACK]);
        assert_eq!(next(&shown), Some("real".to_owned()));
        assert!(shown.try_recv().is_err());
    }

    #[test]
    fn a_connection_that_never_finishes_its_line_does_not_stop_the_next() {
        let dir = tempfile::tempdir().unwrap();
        let guard = first_in(dir.path());
        let shown = served(&guard);
        let (port, _) = read_port(dir.path()).unwrap();
        // Connects, sends half a line and sits there.
        let mut stalled = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        stalled.write_all(SHOW.as_bytes()).unwrap();
        assert!(second(dir.path(), Some("through")));
        assert_eq!(next(&shown), Some("through".to_owned()));
    }

    #[test]
    fn the_port_file_is_for_this_user_only() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = first_in(dir.path());
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
    fn a_holder_that_never_answers_is_reported_not_claimed() {
        let dir = tempfile::tempdir().unwrap();
        // The lock is held, but nothing listens: the holder is not answering.
        let lock = open_lock(dir.path()).unwrap();
        lock.try_lock().unwrap();
        assert!(!second(dir.path(), None));
    }

    #[test]
    fn a_launch_takes_over_when_the_holder_lets_go_while_it_waits() {
        let dir = tempfile::tempdir().unwrap();
        let guard = first_in(dir.path());
        // No server answers; the holder drops its lock before the next try.
        guard.release();
        match acquire_with(dir.path(), None, 3, Duration::ZERO).unwrap() {
            Start::First(_) => {}
            Start::Running { .. } => panic!("the lock was free"),
        }
    }

    #[test]
    fn tokens_compare_whole_and_exactly() {
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("abc", "abcd"));
        assert!(!same("", "a"));
    }
}
