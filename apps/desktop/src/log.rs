//! The diagnostics log: River's stderr, kept in a file the user can attach to a bug report.
//!
//! A Windows release build has no console, and a River started from Finder, a desktop entry,
//! autostart or the tray has no terminal, so its `eprintln!` diagnostics went nowhere. When
//! stderr is not a terminal (and always in a Windows release build), [`start`] replaces the
//! process's stderr with a pipe at the operating system level (`dup2` onto fd 2, or
//! `SetStdHandle(STD_ERROR_HANDLE)`), so every existing `eprintln!`, panic message and child
//! process lands in the log with no change at the call sites. A thread reads the pipe, puts a
//! UTC timestamp on each line and appends it to `<config dir>/logs/river.log`. When stderr is a
//! terminal the thread also prints what it reads there, unchanged.
//!
//! Going through a pipe instead of pointing fd 2 at the file is what makes timestamps and a
//! size bound possible: this process owns the file, so it can rotate it while running, and no
//! child process holds the file open (on Windows an inherited handle would keep it from being
//! renamed). A child only inherits the pipe.
//!
//! The file is capped at about [`LIMIT`] bytes: past it, `river.log` becomes `river.log.1`
//! (replacing the one before) and a new `river.log` starts. If the rename fails, the current
//! file keeps growing and the rename is tried again later, so a failed rotation never costs the
//! current run a line.
//!
//! A line written just before the process exits (a panic message, `process::exit`) may still be
//! in the pipe, so [`flush`] sends a marker through the pipe and waits, for a short bounded
//! time, until the thread has read it. The panic hook and the exit paths in `main` call it. A
//! crash by signal can still lose the last few lines.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

/// Rotate when the file would pass this many bytes.
pub const LIMIT: u64 = 2 * 1024 * 1024;

/// The current file's name; the previous one is the same with `.1` added.
pub const FILE_NAME: &str = "river.log";

/// How long [`flush`] waits for the reader thread.
const FLUSH_WAIT: Duration = Duration::from_millis(500);

/// A line that never reaches the log: [`flush`] writes it and the reader thread consumes it.
const MARKER: &str = "\u{1}river-log-flush:";

/// A partial line longer than this is written without waiting for its newline.
const MAX_PARTIAL: usize = 16 * 1024;

/// After a failed rotation, try again once the file has grown by this much.
const RETRY_AFTER: u64 = 64 * 1024;

/// Where the log goes for a given config directory: a `logs` folder of its own, so that
/// opening the folder from Settings does not show the node's grant token next to the log.
pub fn path_in(config_dir: &Path) -> PathBuf {
    config_dir.join("logs").join(FILE_NAME)
}

/// What to do with stderr.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Leave stderr alone: nothing is logged.
    Off,
    /// The file only: nothing is listening on stderr.
    File,
    /// The file, and keep printing to the terminal stderr is.
    Tee,
}

/// The choice, from whether stderr is a terminal and whether this is a Windows release build
/// (a GUI-subsystem program, which has no console even when a stale handle looks usable).
pub fn decide(stderr_is_terminal: bool, windows_release: bool) -> Mode {
    if windows_release {
        Mode::File
    } else if stderr_is_terminal {
        Mode::Tee
    } else {
        Mode::File
    }
}

// ---- time ----------------------------------------------------------------------------------

/// `2026-11-23T14:05:09.123Z` for `millis` since the Unix epoch.
pub fn iso8601(millis: u64) -> String {
    let secs = millis / 1000;
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Howard Hinnant's civil-from-days.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60,
        millis % 1000
    )
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---- the rotating file ---------------------------------------------------------------------

/// The log file, which rotates itself.
pub struct LogFile {
    path: PathBuf,
    limit: u64,
    file: File,
    written: u64,
    /// After a failed rotation: do not try again before `written` reaches this.
    retry_at: u64,
}

impl LogFile {
    /// Open `path` for appending, creating its folder. A file already at the limit (the last
    /// run's) is rotated first.
    pub fn open(path: &Path, limit: u64) -> io::Result<LogFile> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut log = LogFile {
            path: path.to_owned(),
            limit,
            file: open_append(path)?,
            written: 0,
            retry_at: 0,
        };
        log.written = log.file.metadata().map(|m| m.len()).unwrap_or(0);
        if log.written >= limit {
            log.rotate();
        }
        Ok(log)
    }

    /// The path of the previous file.
    pub fn previous(path: &Path) -> PathBuf {
        let mut name = path.as_os_str().to_owned();
        name.push(".1");
        PathBuf::from(name)
    }

    /// Append `line` (which already ends in a newline), rotating first when it would not fit.
    pub fn write(&mut self, line: &[u8]) -> io::Result<()> {
        let size = line.len() as u64;
        if self.written > 0 && self.written + size > self.limit && self.written >= self.retry_at {
            self.rotate();
        }
        self.file.write_all(line)?;
        self.written += size;
        Ok(())
    }

    /// Move the file to `.1` and start a new one. On failure keep the current file, which
    /// loses nothing, and try again later.
    fn rotate(&mut self) {
        let _ = self.file.flush();
        let previous = Self::previous(&self.path);
        let moved = fs::rename(&self.path, &previous);
        let reopened = match moved {
            // Created fresh: the old handle now points at `.1`.
            Ok(()) => open_append(&self.path),
            Err(e) => Err(e),
        };
        match reopened {
            Ok(file) => {
                self.file = file;
                self.written = 0;
                self.retry_at = 0;
            }
            Err(_) => self.retry_at = self.written + RETRY_AFTER,
        }
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

// ---- reading the pipe ----------------------------------------------------------------------

/// Where the reader thread sends a complete line. Tests use a vector.
pub trait Sink {
    fn line(&mut self, stamped: &[u8]);
}

impl Sink for LogFile {
    fn line(&mut self, stamped: &[u8]) {
        // A full disk or a vanished folder has nowhere to report to; the next line tries again.
        let _ = self.write(stamped);
    }
}

/// Cuts a byte stream into lines, stamps each, and recognises flush markers.
pub struct Stamper {
    partial: Vec<u8>,
    clock: fn() -> u64,
}

impl Stamper {
    pub fn new(clock: fn() -> u64) -> Self {
        Stamper {
            partial: Vec::new(),
            clock,
        }
    }

    /// Take `chunk`; hand every finished line to `sink`; return the numbers of the flush
    /// markers seen (after writing everything before them).
    pub fn feed(&mut self, chunk: &[u8], sink: &mut dyn Sink) -> Vec<u64> {
        let mut seen = Vec::new();
        for &b in chunk {
            self.partial.push(b);
            if b == b'\n' {
                self.finish_line(sink, &mut seen);
            } else if self.partial.len() >= MAX_PARTIAL {
                // A very long line, or one that never ends: write it as it is, and put
                // the rest after a fresh stamp rather than hold it back.
                self.partial.push(b'\n');
                self.finish_line(sink, &mut seen);
            }
        }
        seen
    }

    /// Write out an unfinished line (the stream ended, or a flush asked for it).
    pub fn flush_partial(&mut self, sink: &mut dyn Sink) {
        if !self.partial.is_empty() {
            self.partial.push(b'\n');
            self.finish_line(sink, &mut Vec::new());
        }
    }

    fn finish_line(&mut self, sink: &mut dyn Sink, seen: &mut Vec<u64>) {
        let line = std::mem::take(&mut self.partial);
        let text = &line[..line.len() - 1];
        // The marker can follow the unfinished line of an `eprint!` without a newline.
        let (text, marker) = match find(text, MARKER.as_bytes()) {
            Some(at) => (&text[..at], Some(&text[at + MARKER.len()..])),
            None => (text, None),
        };
        if let Some(n) = marker
            .and_then(|rest| std::str::from_utf8(rest).ok())
            .and_then(|s| s.parse().ok())
        {
            seen.push(n);
            if text.is_empty() {
                return;
            }
        }
        // A line from a Windows child may end in \r\n; keep the text, normalise the end.
        let text = text.strip_suffix(b"\r").unwrap_or(text);
        let mut out = Vec::with_capacity(text.len() + 26);
        out.extend_from_slice(iso8601((self.clock)()).as_bytes());
        out.push(b' ');
        out.extend_from_slice(text);
        out.push(b'\n');
        sink.line(&out);
    }
}

/// Read `reader` until it ends: stamp and write its lines to `sink`, copy the raw bytes to
/// `tee`, and tell `done` about each flush marker.
fn pump(
    mut reader: impl Read,
    sink: &mut dyn Sink,
    mut tee: Option<Box<dyn Write + Send>>,
    done: &Flushed,
) {
    let mut stamper = Stamper::new(now_millis);
    let mut buf = [0u8; 8192];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let chunk = &buf[..n];
        if let Some(t) = tee.as_mut() {
            // The marker is for this thread alone: do not show it on the terminal.
            let _ = t.write_all(&strip_markers(chunk));
            let _ = t.flush();
        }
        let seen = stamper.feed(chunk, sink);
        if !seen.is_empty() {
            // Whatever was written before the marker is already in `sink`; a half line before
            // it is the caller's `eprint!` without a newline, and goes out now too.
            stamper.flush_partial(sink);
            for n in seen {
                done.set(n);
            }
        }
    }
    stamper.flush_partial(sink);
}

/// `chunk` without any flush marker (the marker and the rest of its line).
fn strip_markers(chunk: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(chunk.len());
    for line in chunk.split_inclusive(|&b| b == b'\n') {
        match find(line, MARKER.as_bytes()) {
            Some(at) => out.extend_from_slice(&line[..at]),
            None => out.extend_from_slice(line),
        }
    }
    out
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The number of the last flush marker the thread has read.
struct Flushed {
    last: Mutex<u64>,
    cv: Condvar,
}

impl Flushed {
    fn new() -> Self {
        Flushed {
            last: Mutex::new(0),
            cv: Condvar::new(),
        }
    }

    fn set(&self, n: u64) {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        *last = (*last).max(n);
        self.cv.notify_all();
    }

    fn wait_for(&self, n: u64, limit: Duration) -> bool {
        let last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let (last, _) = self
            .cv
            .wait_timeout_while(last, limit, |l| *l < n)
            .unwrap_or_else(|e| e.into_inner());
        *last >= n
    }
}

// ---- installing it -------------------------------------------------------------------------

struct Running {
    path: PathBuf,
    done: Arc<Flushed>,
    next: Mutex<u64>,
}

static RUNNING: OnceLock<Running> = OnceLock::new();

/// Where the log is being written, once [`start`] has succeeded.
pub fn active_path() -> Option<PathBuf> {
    RUNNING.get().map(|r| r.path.clone())
}

/// Send this process's stderr through the log, as `mode` says. Returns the log's path; `None`
/// when nothing was redirected (mode off, or the file or the pipe could not be made, in which
/// case stderr is left as it was). Call once, early, after the single-instance check, since it
/// rotates the file.
pub fn start(config_dir: &Path, mode: Mode) -> Option<PathBuf> {
    if mode == Mode::Off || RUNNING.get().is_some() {
        return active_path();
    }
    let path = path_in(config_dir);
    let mut file = match LogFile::open(&path, LIMIT) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("river: no log file at {}: {e}", path.display());
            return None;
        }
    };
    let header = format!(
        "--- River {} started, pid {}, {} ---\n",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        std::env::consts::OS
    );
    let mut stamper = Stamper::new(now_millis);
    stamper.feed(header.as_bytes(), &mut file);
    let (reader, tee) = match sys::redirect_stderr(mode == Mode::Tee) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("river: could not redirect stderr to the log: {e}");
            return None;
        }
    };
    let done = Arc::new(Flushed::new());
    let thread_done = done.clone();
    let spawned = std::thread::Builder::new()
        .name("river-log".into())
        .spawn(move || pump(reader, &mut file, tee, &thread_done));
    if spawned.is_err() {
        // Nothing reads the pipe: put stderr back or the first big write would block forever.
        sys::restore_stderr();
        return None;
    }
    let _ = RUNNING.set(Running {
        path: path.clone(),
        done,
        next: Mutex::new(0),
    });
    // A panic message is the line most worth having, and the process is about to end.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        flush();
    }));
    Some(path)
}

/// Wait (at most half a second) until everything written to stderr so far is in the file. A
/// no-op when the log is not running. Call before `process::exit` and at the end of `main`.
pub fn flush() {
    let Some(r) = RUNNING.get() else { return };
    let n = {
        let mut next = r.next.lock().unwrap_or_else(|e| e.into_inner());
        *next += 1;
        *next
    };
    // The marker goes through the same pipe, so when the thread has read it everything before
    // it has been read too. Not eprintln!: that locks stderr and can panic on a closed pipe.
    let _ = sys::write_stderr(format!("{MARKER}{n}\n").as_bytes());
    r.done.wait_for(n, FLUSH_WAIT);
}

#[cfg(unix)]
mod sys {
    use std::io::{self, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    /// Make fd 2 the write end of a new pipe. Returns the read end and, when `tee`, a writer
    /// for what fd 2 was.
    pub fn redirect_stderr(
        tee: bool,
    ) -> io::Result<(std::io::PipeReader, Option<Box<dyn Write + Send>>)> {
        let (reader, writer) = std::io::pipe()?;
        let saved = if tee {
            // Close-on-exec: children get the pipe as fd 2, not a second route to the terminal.
            // SAFETY: fcntl on fd 2 with F_DUPFD_CLOEXEC returns a new descriptor we own.
            let fd = unsafe { libc::fcntl(2, libc::F_DUPFD_CLOEXEC, 3) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `fd` is a fresh descriptor nothing else owns.
            Some(unsafe { OwnedFd::from_raw_fd(fd) })
        } else {
            None
        };
        // SAFETY: dup2 onto fd 2 closes the old fd 2 and makes it a copy of the pipe's write
        // end. The pipe's own descriptor (close-on-exec) is dropped after.
        if unsafe { libc::dup2(writer.as_raw_fd(), 2) } < 0 {
            return Err(io::Error::last_os_error());
        }
        drop(writer);
        let tee = saved.map(|fd| Box::new(std::fs::File::from(fd)) as Box<dyn Write + Send>);
        Ok((reader, tee))
    }

    /// Only reached when the reader thread could not start, so there is no pipe worth closing
    /// properly: point fd 2 at /dev/null so writes do not block.
    pub fn restore_stderr() {
        if let Ok(null) = std::fs::OpenOptions::new().write(true).open("/dev/null") {
            // SAFETY: dup2 onto fd 2 from a valid descriptor.
            unsafe { libc::dup2(null.as_raw_fd(), 2) };
        }
    }

    pub fn write_stderr(bytes: &[u8]) -> io::Result<()> {
        // A write(2) on fd 2, without the lock `std::io::stderr()` takes.
        // SAFETY: the buffer is valid for its length.
        let n = unsafe { libc::write(2, bytes.as_ptr().cast(), bytes.len()) };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(windows)]
mod sys {
    use std::io::{self, Write};
    use std::os::windows::io::{FromRawHandle, RawHandle};
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, SetStdHandle};
    use windows_sys::Win32::System::Pipes::CreatePipe;

    /// The pipe's read end as a file the thread reads.
    pub struct Reader(std::fs::File);

    impl io::Read for Reader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.read(buf)
        }
    }

    /// A terminal handle the thread writes to without ever closing it.
    struct Terminal(std::mem::ManuallyDrop<std::fs::File>);

    impl Write for Terminal {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.0.flush()
        }
    }

    /// Make the standard error handle the write end of a new pipe. Returns the read end and,
    /// when `tee`, a writer for what the handle was.
    pub fn redirect_stderr(tee: bool) -> io::Result<(Reader, Option<Box<dyn Write + Send>>)> {
        let (mut read, mut write): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
        // Not inheritable by default (null attributes); std's Command duplicates the standard
        // handle as inheritable for a child that is given stderr, so a child still logs.
        // SAFETY: both out-pointers are valid.
        if unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: GetStdHandle has no preconditions.
        let old = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
        let tee = if tee && !old.is_null() && old != INVALID_HANDLE_VALUE {
            // SAFETY: `old` is the process's stderr handle; ManuallyDrop never closes it.
            let f = unsafe { std::fs::File::from_raw_handle(old as RawHandle) };
            Some(Box::new(Terminal(std::mem::ManuallyDrop::new(f))) as Box<dyn Write + Send>)
        } else {
            None
        };
        // SAFETY: `write` is a valid handle owned by this process for its whole life.
        if unsafe { SetStdHandle(STD_ERROR_HANDLE, write) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `read` is a valid handle nothing else owns.
        let reader = Reader(unsafe { std::fs::File::from_raw_handle(read as RawHandle) });
        Ok((reader, tee))
    }

    /// Only reached when the reader thread could not start.
    pub fn restore_stderr() {
        // SAFETY: setting the handle to INVALID makes writes fail instead of block.
        unsafe { SetStdHandle(STD_ERROR_HANDLE, INVALID_HANDLE_VALUE) };
    }

    pub fn write_stderr(bytes: &[u8]) -> io::Result<()> {
        // SAFETY: GetStdHandle has no preconditions; the handle is only borrowed.
        let h = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        // SAFETY: borrowed for this write; ManuallyDrop never closes it.
        let mut f =
            std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_handle(h as RawHandle) });
        f.write_all(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Lines(Vec<String>);

    impl Sink for Lines {
        fn line(&mut self, stamped: &[u8]) {
            self.0.push(String::from_utf8_lossy(stamped).into_owned());
        }
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("river-log-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_path_is_a_logs_folder_of_the_config_dir() {
        let p = path_in(Path::new("/c/River"));
        assert_eq!(p, Path::new("/c/River/logs/river.log"));
        assert_eq!(
            LogFile::previous(&p),
            Path::new("/c/River/logs/river.log.1")
        );
    }

    #[test]
    fn a_terminal_is_teed_and_anything_else_goes_to_the_file() {
        assert_eq!(decide(true, false), Mode::Tee);
        assert_eq!(decide(false, false), Mode::File);
        // A Windows release build logs to the file whatever its stale handle looks like.
        assert_eq!(decide(true, true), Mode::File);
        assert_eq!(decide(false, true), Mode::File);
    }

    #[test]
    fn timestamps_are_utc_iso_8601() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(iso8601(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
        assert_eq!(iso8601(1_795_000_000_999), "2026-11-18T11:06:40.999Z");
    }

    #[test]
    fn lines_get_a_stamp_and_a_split_line_is_joined() {
        let mut s = Stamper::new(|| 0);
        let mut out = Lines::default();
        s.feed(b"first\nsec", &mut out);
        s.feed(b"ond\r\nthird", &mut out);
        assert_eq!(
            out.0,
            [
                "1970-01-01T00:00:00.000Z first\n",
                "1970-01-01T00:00:00.000Z second\n"
            ]
        );
        s.flush_partial(&mut out);
        assert_eq!(out.0[2], "1970-01-01T00:00:00.000Z third\n");
    }

    #[test]
    fn a_flush_marker_is_reported_after_the_lines_before_it_and_is_not_logged() {
        let mut s = Stamper::new(|| 0);
        let mut out = Lines::default();
        let seen = s.feed(format!("a\n{MARKER}7\nb\n").as_bytes(), &mut out);
        assert_eq!(seen, [7]);
        assert_eq!(out.0.len(), 2);
        assert!(out.0.iter().all(|l| !l.contains("flush")));
    }

    #[test]
    fn a_line_that_never_ends_is_written_anyway() {
        let mut s = Stamper::new(|| 0);
        let mut out = Lines::default();
        s.feed(&vec![b'x'; MAX_PARTIAL * 2 + 5], &mut out);
        assert_eq!(out.0.len(), 2);
    }

    #[test]
    fn it_rotates_at_the_limit_and_keeps_one_previous_file() {
        let dir = temp("rotate");
        let path = dir.join("river.log");
        let mut log = LogFile::open(&path, 100).unwrap();
        // 40-byte lines: two fit, the third rotates.
        let line = |c: char| format!("{}\n", c.to_string().repeat(39)).into_bytes();
        for c in ['a', 'b', 'c', 'd', 'e', 'f', 'g'] {
            log.write(&line(c)).unwrap();
        }
        let current = fs::read_to_string(&path).unwrap();
        let previous = fs::read_to_string(LogFile::previous(&path)).unwrap();
        // The current run's last lines are in the current file, the lines before it in the
        // previous one, and the earliest were dropped with the file before that.
        assert!(current.contains(&"g".repeat(39)), "{current}");
        assert!(previous.contains(&"f".repeat(39)), "{previous}");
        assert!(current.len() as u64 <= 100 && previous.len() as u64 <= 100);
        assert!(!current.contains('a') && !previous.contains('a'));
        assert!(!dir.join("river.log.2").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_run_that_starts_with_a_full_file_rotates_it_and_loses_nothing_of_the_new_run() {
        let dir = temp("startfull");
        let path = dir.join("river.log");
        fs::write(&path, "x".repeat(150)).unwrap();
        let mut log = LogFile::open(&path, 100).unwrap();
        log.write(b"new run\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new run\n");
        assert_eq!(
            fs::read_to_string(LogFile::previous(&path)).unwrap(),
            "x".repeat(150)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_rotation_keeps_every_line_in_the_current_file() {
        let dir = temp("norotate");
        let path = dir.join("river.log");
        // A non-empty directory where `.1` should go makes the rename fail.
        let blocker = LogFile::previous(&path);
        fs::create_dir_all(blocker.join("inside")).unwrap();
        let mut log = LogFile::open(&path, 100).unwrap();
        let mut want = String::new();
        for i in 0..20 {
            let l = format!("line {i:02} {}\n", "y".repeat(30));
            want.push_str(&l);
            log.write(l.as_bytes()).unwrap();
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), want);
        // Once the obstacle is gone, the next big step rotates.
        fs::remove_dir_all(&blocker).unwrap();
        for i in 20..3000 {
            log.write(format!("line {i:03} {}\n", "y".repeat(30)).as_bytes())
                .unwrap();
        }
        assert!(blocker.is_file(), "rotated after the obstacle was removed");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pump_stamps_writes_and_tees_but_never_shows_the_marker() {
        struct Shared(Arc<Mutex<Vec<u8>>>);
        impl Write for Shared {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut out = Lines::default();
        let done = Flushed::new();
        let input = format!("hello\n{MARKER}3\nbye").into_bytes();
        pump(
            io::Cursor::new(input),
            &mut out,
            Some(Box::new(Shared(seen.clone()))),
            &done,
        );
        assert!(out.0[0].ends_with(" hello\n") && out.0[1].ends_with(" bye\n"));
        assert_eq!(seen.lock().unwrap().as_slice(), b"hello\nbye");
        assert!(done.wait_for(3, Duration::from_millis(1)));
    }
}
