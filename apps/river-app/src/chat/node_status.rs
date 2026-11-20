//! "Node status": a dialog that says how River stands with the Freenet node, and copies a plain
//! text block of diagnostics for a bug report.
//!
//! It opens from the connection dot in the room list, from Settings, and from the screens
//! before the rooms (where it answers "why can't River reach my node"). Nothing runs while it
//! is closed. While it is open:
//!
//! - the node's version is asked again at most every [`REFRESH_EVERY`], by
//!   [`crate::Controller::probe_status`] on a worker thread (one request to the node's own
//!   version endpoint);
//! - the newest Freenet release is looked up once per opening of the dialog (again after a
//!   failure), by a request to github.com that the dialog says it makes. That request reveals
//!   the user's IP address to GitHub, so it is never made at startup or while the dialog is
//!   closed.
//!
//! The node's client interface could also report its connected peers, but River does not ask
//! for them (that is a second request on the rooms connection), so the dialog says "unknown".
//!
//! # What may leave the machine
//!
//! The diagnostics ([`format_diagnostics`]) are built only from a fixed set of fields: versions,
//! the OS, the node's host and port, the connection state, counts, and errors as a category
//! ([`ErrorKind`]) with a time. No free-form error text is ever copied: error text can come from
//! a remote node, or from a path or URL on this machine, and a blacklist of "secret-looking"
//! words cannot be complete. The free text is shown in the dialog only, after [`sanitize`].

use std::time::{Duration, Instant};

use rune::{El, col, row};
use rune_kit::{Button, ButtonVariant, Dialog, Kit};

use super::style::{self as st, Palette};
use super::view::label;
use super::{LinkStatus, Source};
use crate::rooms::grant::GRANT_NODE_VERSION;
use crate::{Access, RiverApp, Setup};

/// The longest the dialog waits before asking the node again.
pub const REFRESH_EVERY: Duration = Duration::from_secs(5);

/// How many past errors are kept for the dialog and the diagnostics.
pub const MAX_ERRORS: usize = 5;

/// The longest error text the dialog shows.
pub const MAX_DETAIL: usize = 300;

/// What the dialog says next to the latest release, because looking it up contacts GitHub.
pub const LATEST_DISCLOSURE: &str =
    "Checks github.com for the newest Freenet release when this opens.";

/// How the connection stands, in the words the dialog uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Connection {
    /// Example rooms: nothing is connected.
    Example,
    /// Checking, installing or starting Freenet, or opening the connection.
    Connecting,
    Connected,
    /// The rooms are shown, the link to the node was lost, River is reconnecting.
    Reconnecting,
    WaitingForApproval,
    NotApproved,
    PermissionRemoved,
    /// The node is too old for River.
    Unsupported,
    /// The node refused River.
    Refused,
    /// Nothing answered.
    Unreachable,
    /// Setup stopped on a problem (shown on the setup screen).
    Problem,
}

impl Connection {
    pub fn text(self) -> &'static str {
        match self {
            Connection::Example => "Not connected (example rooms)",
            Connection::Connecting => "Connecting",
            Connection::Connected => "Connected",
            Connection::Reconnecting => "Disconnected, reconnecting",
            Connection::WaitingForApproval => "Waiting for approval in the browser",
            Connection::NotApproved => "Not approved",
            Connection::PermissionRemoved => "Permission removed",
            Connection::Unsupported => "Node too old",
            Connection::Refused => "Refused by the node",
            Connection::Unreachable => "Disconnected: nothing answers",
            Connection::Problem => "Not connected",
        }
    }

    fn is_good(self) -> bool {
        self == Connection::Connected
    }

    fn is_bad(self) -> bool {
        matches!(
            self,
            Connection::Reconnecting
                | Connection::NotApproved
                | Connection::PermissionRemoved
                | Connection::Unsupported
                | Connection::Refused
                | Connection::Unreachable
                | Connection::Problem
        )
    }
}

/// Work out [`Connection`] from what the app knows.
pub fn connection_of(setup: &Setup, access: &Access, source: Source) -> Connection {
    match setup {
        Setup::Connected { .. } => match access {
            Access::Example => Connection::Example,
            Access::Connecting => Connection::Connecting,
            Access::WaitingForApproval { .. } => Connection::WaitingForApproval,
            Access::NotApproved { .. } => Connection::NotApproved,
            Access::PermissionRemoved => Connection::PermissionRemoved,
            Access::Unsupported => Connection::Unsupported,
            Access::Refused(_) => Connection::Refused,
            Access::Unreachable(_) => Connection::Unreachable,
            Access::Ready => match source {
                Source::Example => Connection::Example,
                Source::Node(LinkStatus::Live) => Connection::Connected,
                Source::Node(LinkStatus::Loading) => Connection::Connecting,
                Source::Node(LinkStatus::Reconnecting) => Connection::Reconnecting,
            },
        },
        Setup::Checking | Setup::Installing(_) | Setup::Starting => Connection::Connecting,
        Setup::NotInstalled | Setup::Failed(_) => Connection::Problem,
    }
}

/// What the node's version endpoint said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reach {
    Answered {
        version: String,
    },
    /// Something answered but is older than River supports (`None`: it gave no version).
    TooOld {
        running: Option<String>,
    },
    /// Nothing answered.
    Silent,
}

/// What River knows of the latest Freenet release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Latest {
    /// Not asked yet.
    Unknown,
    Fetching,
    Known(String),
    /// The lookup failed (offline, GitHub unreachable). Asked again when the dialog reopens.
    Unavailable,
}

/// A result from a worker thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatusEvent {
    /// The node at `authority` answered the version request.
    Reach {
        authority: String,
        reach: Reach,
    },
    /// The version request's worker ended without an answer (it panicked): ask again later.
    Aborted,
    Latest(Option<String>),
}

/// What kind of thing went wrong. This, with a time, is all the diagnostics say about an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    NodeUnreachable,
    ConnectionLost,
    PermissionDeclined,
    PermissionExpired,
    PermissionRemoved,
    NodeTooOld,
    /// The node refused River for a reason it gave.
    Refused,
    /// Finding, installing or starting Freenet failed, or setup stopped.
    SetupFailed,
}

impl ErrorKind {
    /// The category, as the diagnostics name it.
    pub fn label(self) -> &'static str {
        match self {
            ErrorKind::NodeUnreachable => "node unreachable",
            ErrorKind::ConnectionLost => "connection lost",
            ErrorKind::PermissionDeclined => "permission declined",
            ErrorKind::PermissionExpired => "permission request expired",
            ErrorKind::PermissionRemoved => "permission removed",
            ErrorKind::NodeTooOld => "node too old",
            ErrorKind::Refused => "refused by the node",
            ErrorKind::SetupFailed => "setup failed",
        }
    }

    /// The sentence the dialog shows (the node's own words, when any, follow it).
    fn sentence(self) -> &'static str {
        match self {
            ErrorKind::NodeUnreachable => "No node answered.",
            ErrorKind::ConnectionLost => "Lost the connection to the node. River is reconnecting.",
            ErrorKind::PermissionDeclined => "The permission request was declined.",
            ErrorKind::PermissionExpired => {
                "The permission request expired before it was answered."
            }
            ErrorKind::PermissionRemoved => "The node removed River's permission.",
            ErrorKind::NodeTooOld => "The node is too old to give River permission.",
            ErrorKind::Refused => "The node refused River.",
            ErrorKind::SetupFailed => "Setting up Freenet failed.",
        }
    }
}

/// One thing that went wrong: when (seconds since 1970, UTC), what kind, which node it
/// concerned, and the sanitised free text for the dialog (never copied).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorNote {
    pub at: u64,
    pub kind: ErrorKind,
    pub authority: String,
    pub detail: String,
}

impl ErrorNote {
    /// What the dialog's "Last error" says.
    pub fn shown(&self) -> String {
        if self.detail.is_empty() {
            self.kind.sentence().to_owned()
        } else {
            format!("{} {}", self.kind.sentence(), self.detail)
        }
    }
}

/// The dialog's state.
pub struct NodeStatusUi {
    pub open: bool,
    pub connection: Connection,
    /// The address of the node in use (`host:port`).
    pub address: String,
    /// Whether that node is not on this computer.
    pub remote: bool,
    /// The last answer from the node, and the address it came from.
    pub reach: Option<(String, Reach)>,
    /// The last version a node answered with, and its address.
    seen: Option<(String, String)>,
    pub latest: Latest,
    /// Rooms River has, and (once the link is live) how many it is following. `None` until the
    /// rooms are loaded.
    pub rooms: Option<(usize, Option<usize>)>,
    pub errors: Vec<ErrorNote>,
    /// Copy diagnostics was pressed since the dialog opened.
    pub copied: bool,
    next_refresh: Option<Instant>,
    in_flight: bool,
    last_link: Option<LinkStatus>,
}

impl Default for NodeStatusUi {
    fn default() -> Self {
        NodeStatusUi {
            open: false,
            connection: Connection::Connecting,
            address: crate::address::DEFAULT_AUTHORITY.to_owned(),
            remote: false,
            reach: None,
            seen: None,
            latest: Latest::Unknown,
            rooms: None,
            errors: Vec::new(),
            copied: false,
            next_refresh: None,
            in_flight: false,
            last_link: None,
        }
    }
}

impl NodeStatusUi {
    pub fn open(&mut self) {
        self.open = true;
        self.copied = false;
        // Ask again at once: what the dialog shows should be current when it appears, and a
        // failed release lookup gets another try.
        self.next_refresh = None;
        if self.latest == Latest::Unavailable {
            self.latest = Latest::Unknown;
        }
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    /// Whether the node should be asked its version now. Never while closed, and never while
    /// an earlier question is unanswered.
    pub fn should_refresh(&self, now: Instant) -> bool {
        self.open && !self.in_flight && self.next_refresh.is_none_or(|t| now >= t)
    }

    pub fn refresh_started(&mut self) {
        self.in_flight = true;
    }

    /// Whether the latest release should be looked up (when the dialog is open and it has not
    /// been asked for since it opened).
    pub fn wants_latest(&self) -> bool {
        self.open && self.latest == Latest::Unknown
    }

    pub fn latest_started(&mut self) {
        self.latest = Latest::Fetching;
    }

    /// Take an answer. One for a node River has since left is dropped, but it still ends the
    /// wait so the next question is asked.
    pub fn apply(&mut self, current: &str, event: StatusEvent, now: Instant) {
        match event {
            StatusEvent::Reach { authority, reach } => {
                self.in_flight = false;
                self.next_refresh = Some(now + REFRESH_EVERY);
                if authority == current {
                    let version = match &reach {
                        Reach::Answered { version } => Some(version.clone()),
                        Reach::TooOld { running } => running.clone(),
                        Reach::Silent => None,
                    };
                    if let Some(v) = version {
                        self.seen = Some((authority.clone(), v));
                    }
                    self.reach = Some((authority, reach));
                }
            }
            StatusEvent::Aborted => {
                self.in_flight = false;
                self.next_refresh = Some(now + REFRESH_EVERY);
            }
            StatusEvent::Latest(version) => {
                self.latest = match version {
                    Some(v) => Latest::Known(v),
                    None => Latest::Unavailable,
                };
            }
        }
    }

    /// Remember something that went wrong at the node `authority`. `detail` is free text: it is
    /// sanitised and kept for the dialog only. The same thing twice in a row is recorded once.
    pub fn note(&mut self, kind: ErrorKind, authority: &str, at: u64, detail: &str) {
        let detail = sanitize(detail);
        if self
            .errors
            .last()
            .is_some_and(|e| e.kind == kind && e.authority == authority && e.detail == detail)
        {
            return;
        }
        self.errors.push(ErrorNote {
            at,
            kind,
            authority: authority.to_owned(),
            detail,
        });
        if self.errors.len() > MAX_ERRORS {
            self.errors.remove(0);
        }
    }

    /// Remember a lost connection, once per loss.
    pub fn note_link(&mut self, status: LinkStatus, authority: &str, at: u64) {
        if status == LinkStatus::Reconnecting && self.last_link != Some(LinkStatus::Reconnecting) {
            self.note(ErrorKind::ConnectionLost, authority, at, "");
        }
        self.last_link = Some(status);
    }

    /// Remember why access to the rooms stopped, when it did.
    pub fn note_access(&mut self, access: &Access, authority: &str, at: u64) {
        let (kind, detail) = match access {
            Access::NotApproved { expired: false } => (ErrorKind::PermissionDeclined, ""),
            Access::NotApproved { expired: true } => (ErrorKind::PermissionExpired, ""),
            Access::PermissionRemoved => (ErrorKind::PermissionRemoved, ""),
            Access::Unsupported => (ErrorKind::NodeTooOld, ""),
            // The node's own words: attacker-controlled for a remote node, so sanitised, shown
            // in the dialog only.
            Access::Refused(why) => (ErrorKind::Refused, why.as_str()),
            Access::Unreachable(_) => (ErrorKind::NodeUnreachable, ""),
            Access::Example
            | Access::Connecting
            | Access::WaitingForApproval { .. }
            | Access::Ready => return,
        };
        self.note(kind, authority, at, detail);
    }

    /// The errors that concern the node in use, oldest first.
    pub fn errors_here(&self) -> impl Iterator<Item = &ErrorNote> {
        self.errors.iter().filter(|e| e.authority == self.address)
    }

    pub fn last_error(&self) -> Option<&ErrorNote> {
        self.errors_here().last()
    }

    /// The node's version: what it last answered with, else what River learned when it
    /// connected.
    pub fn node_version<'a>(&'a self, known: Option<&'a str>) -> Option<&'a str> {
        match &self.seen {
            Some((a, v)) if *a == self.address => Some(v),
            _ => known,
        }
    }

    /// Whether the node answered the last version request: `None` before any answer for the
    /// node in use.
    pub fn answering(&self) -> Option<bool> {
        match &self.reach {
            Some((a, Reach::Answered { .. })) if *a == self.address => Some(true),
            Some((a, Reach::TooOld { running })) if *a == self.address => Some(running.is_some()),
            Some((a, Reach::Silent)) if *a == self.address => Some(false),
            _ => None,
        }
    }
}

// Versions ------------------------------------------------------------------------------------

/// A Freenet version ordered as the desktop host orders them (`classify` in
/// `desktop/src/host.rs`): `MAJOR.MINOR.PATCH`, and a pre-release (`-rc.1`) comes before its
/// release.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    core: (u64, u64, u64),
    /// False for a pre-release, so it sorts before the release.
    release: bool,
}

pub fn parse_version(v: &str) -> Option<Version> {
    let before_build = v.trim().split('+').next()?;
    let release = !before_build.contains('-');
    let core = before_build.split('-').next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some(Version {
        core: (major, minor, patch),
        release,
    })
}

/// How a node's version compares with what River needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Needs {
    Meets,
    TooOld,
    Unknown,
}

pub fn meets_minimum(node: Option<&str>, minimum: &str) -> Needs {
    match (node.and_then(parse_version), parse_version(minimum)) {
        (Some(n), Some(m)) if n >= m => Needs::Meets,
        (Some(_), Some(_)) => Needs::TooOld,
        _ => Needs::Unknown,
    }
}

/// How a node's version compares with the latest release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recency {
    Latest,
    Behind,
    Ahead,
    Unknown,
}

pub fn against_latest(node: Option<&str>, latest: Option<&str>) -> Recency {
    match (node.and_then(parse_version), latest.and_then(parse_version)) {
        (Some(n), Some(l)) if n == l => Recency::Latest,
        (Some(n), Some(l)) if n < l => Recency::Behind,
        (Some(_), Some(_)) => Recency::Ahead,
        _ => Recency::Unknown,
    }
}

// Keeping secrets out -------------------------------------------------------------------------

/// `host:port` from whatever was given as a node address, or `unknown address`. Everything
/// before the last `@` is dropped (credentials, even ones that contain `/`), then a scheme, a
/// path, a query and a fragment; what is left must parse as a node address.
pub fn redact_address(address: &str) -> String {
    let s = address.trim();
    let s = s.split_once("://").map_or(s, |(_, rest)| rest);
    let s = s.rsplit_once('@').map_or(s, |(_, host)| host);
    let s = s.split(['/', '\\', '?', '#']).next().unwrap_or_default();
    crate::address::NodeAddress::parse(s)
        .map_or_else(|_| "unknown address".to_owned(), |a| a.authority())
}

const REMOVED: &str = "[removed]";

/// Words that name a secret.
const SECRET_WORDS: &[&str] = &[
    "token",
    "password",
    "passwd",
    "pwd",
    "pass",
    "secret",
    "auth",
    "authorization",
    "bearer",
    "apikey",
    "key",
    "sig",
    "signature",
    "credential",
    "credentials",
    "cookie",
];

/// Whether `word` names a secret: it, or one of its `_`, `-` or camelCase parts, is a
/// whole secret word. `author`, `keyboard` and `monkey` are not.
fn names_secret(word: &str) -> bool {
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in word.chars() {
        if c == '_' || c == '-' {
            parts.push(std::mem::take(&mut cur));
            prev_lower = false;
            continue;
        }
        if c.is_ascii_uppercase() && prev_lower {
            parts.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        cur.push(c.to_ascii_lowercase());
    }
    parts.push(cur);
    parts.iter().any(|p| SECRET_WORDS.contains(&p.as_str()))
}

/// A long run of token characters holding both letters and digits: what an opaque credential
/// looks like.
fn looks_opaque(word: &str) -> bool {
    word.len() >= 24
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | '+' | '='))
        && word.chars().any(|c| c.is_ascii_digit())
        && word.chars().any(|c| c.is_ascii_alphabetic())
}

/// Characters that end a URL or a value.
fn ends_value(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '"' | '\'' | '<' | '>' | ')' | ']' | '}' | ',' | ';' | '&'
        )
}

/// Whether `host` (with an optional `:port`) can be shown: letters, digits, dots, hyphens,
/// brackets and colons only.
fn plain_host(host: &str) -> bool {
    !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
}

/// Cut every URL in `text` to `scheme://host:port`. Credentials (everything up to the last
/// `@`), the path, the query and the fragment go; a URL whose host does not look like one
/// becomes `[url]`.
fn cut_urls(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let before = &rest[..at];
        let scheme_at = before
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
            .map_or(0, |i| i + 1);
        out.push_str(&rest[..scheme_at]);
        let scheme = &before[scheme_at..];
        let after = &rest[at + 3..];
        let end = after.find(ends_value_in_url).unwrap_or(after.len());
        let run = &after[..end];
        let host = run.rsplit_once('@').map_or(run, |(_, h)| h);
        let host = host.split(['/', '\\', '?', '#']).next().unwrap_or_default();
        let trimmed = host.trim_end_matches(['.', ':', '!']);
        if plain_host(trimmed) {
            out.push_str(&format!("{scheme}://{trimmed}"));
        } else {
            out.push_str("[url]");
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// A URL ends at whitespace or a closing quote or bracket (a comma may be inside a password).
fn ends_value_in_url(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ')' | ']' | '}')
}

/// Reduce a path to its file name, so a user name in a home directory never shows.
fn basename_words(text: &str) -> String {
    let is_drive = |w: &str| {
        let b = w.as_bytes();
        b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/')
    };
    text.split_whitespace()
        .map(|word| {
            let lead = word
                .find(|c: char| !matches!(c, '(' | '"' | '\'' | '[' | '<' | '=' | '{'))
                .unwrap_or(word.len());
            let (prefix, body) = word.split_at(lead);
            let is_path = body.starts_with('/')
                || body.starts_with("~/")
                || body.starts_with("\\\\")
                || is_drive(body)
                || body.contains('\\');
            if !is_path {
                return word.to_owned();
            }
            let parts: Vec<&str> = body
                .split(['/', '\\'])
                .filter(|part| !part.is_empty())
                .collect();
            let rest = match parts.first() {
                Some(p) if p.ends_with(':') => &parts[1..],
                _ => &parts[..],
            };
            // A home directory is named after the person: show it as `~`.
            let is_home = rest.len() == 2
                && (rest[0].eq_ignore_ascii_case("home") || rest[0].eq_ignore_ascii_case("users"));
            let name = if is_home {
                "~"
            } else {
                parts.last().copied().unwrap_or("")
            };
            format!("{prefix}{name}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Remove the value after a secret's name: `name=value`, `name: value`, `name = value`,
/// `"name":"value"`, `name%3Dvalue`, `Authorization: Basic value` and `Bearer value`.
fn redact_pairs(text: &str) -> String {
    let c: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < c.len() {
        if !c[i].is_ascii_alphanumeric() {
            out.push(c[i]);
            i += 1;
            continue;
        }
        let start = i;
        while i < c.len() && (c[i].is_ascii_alphanumeric() || c[i] == '_' || c[i] == '-') {
            i += 1;
        }
        let word: String = c[start..i].iter().collect();
        out.push_str(&word);
        if !names_secret(&word) {
            continue;
        }
        let Some(value_at) = value_start(&c, i, &word) else {
            continue;
        };
        out.extend(&c[i..value_at]);
        let mut end = value_end(&c, value_at);
        // `Basic xxx`, `Bearer xxx`: the scheme is followed by the credential.
        let first: String = c[value_at..end].iter().collect();
        if matches!(
            first.to_ascii_lowercase().as_str(),
            "basic" | "bearer" | "digest" | "negotiate" | "token"
        ) {
            let mut j = end;
            while j < c.len() && c[j] == ' ' {
                j += 1;
            }
            if j > end && j < c.len() && !ends_value(c[j]) {
                end = value_end(&c, j);
            }
        }
        out.push_str(REMOVED);
        i = end;
    }
    out
}

/// Where the value after the secret word `word` (which ends at `i`) starts, if there is one.
fn value_start(c: &[char], mut i: usize, word: &str) -> Option<usize> {
    let at = i;
    if i < c.len() && matches!(c[i], '"' | '\'') {
        i += 1;
    }
    while i < c.len() && c[i] == ' ' {
        i += 1;
    }
    let url_encoded = |i: usize| {
        (i + 2 < c.len() && c[i] == '%')
            .then(|| (c[i + 1] == '3' && matches!(c[i + 2], 'D' | 'd' | 'A' | 'a')).then_some(3))
            .flatten()
    };
    let colon = i < c.len() && c[i] == ':';
    if i < c.len() && (c[i] == '=' || colon) {
        if colon && c.get(i + 1) == Some(&'/') && c.get(i + 2) == Some(&'/') {
            return None;
        }
        i += 1;
    } else if let Some(n) = url_encoded(i) {
        i += n;
    } else if word.eq_ignore_ascii_case("bearer") && i > at {
        // `Bearer value`: a space is the separator.
    } else {
        return None;
    }
    while i < c.len() && c[i] == ' ' {
        i += 1;
    }
    if i < c.len() && matches!(c[i], '"' | '\'') {
        i += 1;
    }
    (i < c.len() && !ends_value(c[i])).then_some(i)
}

fn value_end(c: &[char], mut i: usize) -> usize {
    while i < c.len() && !ends_value(c[i]) {
        i += 1;
    }
    i
}

/// Control characters and the characters that reorder or hide text (bidi controls, zero-width
/// marks, the byte-order mark).
fn is_unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}' | '\u{FEFF}'
        )
}

/// Make free text fit for the dialog: bounded, one line, no control or bidi characters, URLs
/// cut to host and port, paths cut to their file name, secrets after secret-looking names
/// removed, long opaque strings removed. A second line of defence: nothing sanitised here is
/// ever copied (see the module docs).
pub fn sanitize(text: &str) -> String {
    // Bound the work first: the input may be megabytes.
    let bounded: String = text
        .chars()
        .take(MAX_DETAIL * 8)
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|c| !is_unsafe_char(*c))
        .collect();
    let cut = redact_pairs(&basename_words(&cut_urls(&bounded)));
    let words: Vec<String> = cut
        .split_whitespace()
        .map(|w| {
            let t = w.trim_matches(|c: char| matches!(c, '.' | ',' | ';' | '"' | '\'' | '(' | ')'));
            if looks_opaque(t) {
                REMOVED.to_owned()
            } else {
                w.to_owned()
            }
        })
        .collect();
    let line = words.join(" ");
    if line.chars().count() > MAX_DETAIL {
        let mut cut: String = line.chars().take(MAX_DETAIL).collect();
        cut.push('…');
        cut
    } else {
        line
    }
}

// Time ----------------------------------------------------------------------------------------

/// Seconds since 1970 now, for stamping an error. 0 when the clock is before 1970.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `2026-11-20 14:03:22 UTC`.
pub fn utc_stamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

// Diagnostics ---------------------------------------------------------------------------------

/// What the diagnostics are made from. There is deliberately no field for a room's name, key,
/// message, key material or free-form error text: a field added here must be added on purpose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostics {
    pub river_version: String,
    pub os: String,
    pub arch: String,
    pub node_address: String,
    pub node_is_remote: bool,
    pub connection: Connection,
    pub node_version: Option<String>,
    /// Whether the node answered the last version request.
    pub node_answering: Option<bool>,
    pub minimum_version: String,
    pub latest_version: Option<String>,
    pub peers: Option<usize>,
    pub rooms: Option<usize>,
    pub rooms_following: Option<usize>,
    /// Errors as a category and a time.
    pub errors: Vec<(u64, ErrorKind)>,
}

fn or_unknown(v: Option<&str>) -> String {
    v.map_or_else(|| "unknown".to_owned(), str::to_owned)
}

fn count(n: Option<usize>) -> String {
    n.map_or_else(|| "unknown".to_owned(), |n| n.to_string())
}

/// The block that Copy diagnostics puts on the clipboard.
pub fn format_diagnostics(d: &Diagnostics) -> String {
    let node = d.node_version.as_deref();
    let needs = match meets_minimum(node, &d.minimum_version) {
        Needs::Meets => "meets the minimum",
        Needs::TooOld => "OLDER than River needs",
        Needs::Unknown => "minimum not checked",
    };
    let answering = match d.node_answering {
        Some(false) => ", last seen, not answering now",
        _ => "",
    };
    let mut s = String::new();
    s.push_str("River node status\n");
    s.push_str(
        "Includes: versions, OS, the node's host and port, connection state, counts, error \
         categories. Leaves out: room names and keys, messages, credentials, error text.\n",
    );
    s.push_str(&format!("River: {}\n", sanitize(&d.river_version)));
    s.push_str(&format!("OS: {} {}\n", sanitize(&d.os), sanitize(&d.arch)));
    s.push_str(&format!(
        "Node host and port (identifying if remote): {} ({})\n",
        redact_address(&d.node_address),
        if d.node_is_remote {
            "remote"
        } else {
            "this computer"
        }
    ));
    s.push_str(&format!("Connection: {}\n", d.connection.text()));
    s.push_str(&format!(
        "Node version: {} ({needs}{answering}; River needs {} or newer)\n",
        or_unknown(node.map(sanitize).as_deref()),
        sanitize(&d.minimum_version)
    ));
    s.push_str(&format!(
        "Latest Freenet release: {}\n",
        or_unknown(d.latest_version.as_deref().map(sanitize).as_deref())
    ));
    s.push_str(&format!("Connected peers: {}\n", count(d.peers)));
    s.push_str(&format!(
        "Rooms: {} ({} following)\n",
        count(d.rooms),
        count(d.rooms_following)
    ));
    if d.errors.is_empty() {
        s.push_str("Recent errors: none\n");
    } else {
        s.push_str("Recent errors (category, newest last):\n");
        for (at, kind) in &d.errors {
            s.push_str(&format!("  {}  {}\n", utc_stamp(*at), kind.label()));
        }
    }
    s
}

impl Diagnostics {
    /// Gather what River knows now.
    pub fn of(chat: &super::Chat) -> Diagnostics {
        let st = &chat.status;
        Diagnostics {
            river_version: chat.update.version.clone(),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            node_address: st.address.clone(),
            node_is_remote: st.remote,
            connection: st.connection,
            node_version: st
                .node_version(chat.update.node_version.as_deref())
                .map(str::to_owned),
            node_answering: st.answering(),
            minimum_version: GRANT_NODE_VERSION.to_owned(),
            latest_version: match &st.latest {
                Latest::Known(v) => Some(v.clone()),
                _ => None,
            },
            // The node can report its connected peers, but River does not ask.
            peers: None,
            rooms: st.rooms.map(|(all, _)| all),
            rooms_following: st.rooms.and_then(|(_, following)| following),
            errors: st.errors_here().map(|e| (e.at, e.kind)).collect(),
        }
    }
}

// The dialog ----------------------------------------------------------------------------------

impl super::Chat {
    pub fn close_node_status(&mut self) {
        self.status.close();
    }
}

/// One line of the dialog: a muted name, the value, and a note under it when there is one.
fn line(
    key: &'static str,
    name: &str,
    value: String,
    color: rune::Color,
    note: Option<String>,
    p: &Palette,
) -> El {
    let mut v = col()
        .grow(1.0)
        .gap(2.0)
        .child(label(value, st::BODY, color).wrap().w_full().key(key));
    if let Some(n) = note {
        v = v.child(label(n, st::SMALL, p.text_muted).wrap().w_full());
    }
    row()
        .w_full()
        .gap(12.0)
        .child(label(name.to_owned(), st::SMALL, p.text_muted).w(130.0))
        .child(v)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Good,
    Bad,
    Muted,
}

/// The dialog's text, line by line. Separate from drawing so a test can read it.
pub struct Line {
    pub key: &'static str,
    pub name: &'static str,
    pub value: String,
    pub tone: Tone,
    pub note: Option<String>,
}

pub fn lines(chat: &super::Chat) -> Vec<Line> {
    let st = &chat.status;
    let node = st.node_version(chat.update.node_version.as_deref());
    let mut out = Vec::new();
    out.push(Line {
        key: "ns-connection",
        name: "Connection",
        value: st.connection.text().to_owned(),
        tone: if st.connection.is_good() {
            Tone::Good
        } else if st.connection.is_bad() {
            Tone::Bad
        } else {
            Tone::Plain
        },
        note: None,
    });
    out.push(Line {
        key: "ns-address",
        name: "Node address",
        value: redact_address(&st.address),
        tone: Tone::Plain,
        note: Some(if st.remote {
            "A node on another computer.".to_owned()
        } else {
            "The node on this computer.".to_owned()
        }),
    });
    let (value, mut tone, mut note) = match (node, meets_minimum(node, GRANT_NODE_VERSION)) {
        (Some(v), Needs::TooOld) => (
            v.to_owned(),
            Tone::Bad,
            Some(format!(
                "Older than River needs ({GRANT_NODE_VERSION}). Update Freenet."
            )),
        ),
        (Some(v), _) => (v.to_owned(), Tone::Plain, None),
        (None, _) => ("unknown".to_owned(), Tone::Muted, None),
    };
    if st.answering() == Some(false) {
        tone = Tone::Muted;
        let why = if st.connection == Connection::Connected {
            "River is connected, but the node's version request got no answer. The node may be \
             restarting, or something in front of it blocks that page."
        } else {
            "The node is not answering."
        };
        note = Some(if node.is_some() {
            format!("Last seen. {why}")
        } else {
            why.to_owned()
        });
    }
    out.push(Line {
        key: "ns-node-version",
        name: "Freenet version",
        value,
        tone,
        note,
    });
    out.push(Line {
        key: "ns-minimum",
        name: "River needs",
        value: format!("{GRANT_NODE_VERSION} or newer"),
        tone: Tone::Plain,
        note: None,
    });
    let latest = match &st.latest {
        Latest::Known(v) => Some(v.as_str()),
        _ => None,
    };
    let (value, tone, recency) = match &st.latest {
        Latest::Unknown | Latest::Fetching => ("checking".to_owned(), Tone::Muted, None),
        Latest::Unavailable => (
            "unknown".to_owned(),
            Tone::Muted,
            Some("The latest release could not be looked up.".to_owned()),
        ),
        Latest::Known(v) => (
            v.clone(),
            Tone::Plain,
            match against_latest(node, latest) {
                Recency::Behind if st.remote => Some("A newer Freenet is out.".to_owned()),
                Recency::Behind => Some(
                    "A newer Freenet is out. A node on this computer updates itself.".to_owned(),
                ),
                Recency::Latest => Some("This node is up to date.".to_owned()),
                Recency::Ahead | Recency::Unknown => None,
            },
        ),
    };
    out.push(Line {
        key: "ns-latest",
        name: "Latest Freenet",
        value,
        tone,
        note: Some(match recency {
            Some(r) => format!("{r} {LATEST_DISCLOSURE}"),
            None => LATEST_DISCLOSURE.to_owned(),
        }),
    });
    out.push(Line {
        key: "ns-river",
        name: "River",
        value: chat.update.version.clone(),
        tone: Tone::Plain,
        note: None,
    });
    out.push(Line {
        key: "ns-peers",
        name: "Connected peers",
        value: "unknown".to_owned(),
        tone: Tone::Muted,
        note: Some("River does not ask the node for its peer list.".to_owned()),
    });
    out.push(Line {
        key: "ns-rooms",
        name: "Rooms",
        value: match st.rooms {
            Some((all, Some(following))) => {
                format!("{all} in River, {following} receiving updates")
            }
            Some((all, None)) => format!("{all} in River"),
            None => "unknown".to_owned(),
        },
        tone: Tone::Plain,
        note: None,
    });
    out.push(match st.last_error() {
        Some(e) => Line {
            key: "ns-error",
            name: "Last error",
            value: e.shown(),
            tone: Tone::Bad,
            note: Some(utc_stamp(e.at)),
        },
        None => Line {
            key: "ns-error",
            name: "Last error",
            value: "none".to_owned(),
            tone: Tone::Muted,
            note: None,
        },
    });
    out
}

/// The label of the copy button: it says what is in what it copies.
pub const COPY_LABEL: &str = "Copy diagnostics (includes node address)";

/// The dialog, when open.
pub(crate) fn dialog(chat: &super::Chat, k: &Kit, layer: &mut Vec<El>) {
    if !chat.status.open {
        return;
    }
    let p = st::palette();
    let mut body = col().key("ns-body").w_full().px(16.0).pb(16.0).gap(10.0);
    for l in lines(chat) {
        let color = match l.tone {
            Tone::Plain => p.text,
            Tone::Good => p.success,
            Tone::Bad => p.danger,
            Tone::Muted => p.text_muted,
        };
        body = body.child(line(l.key, l.name, l.value, color, l.note, &p));
    }
    body = body.child(
        label(
            "Copy diagnostics copies versions, the OS, the node's host and port, the connection \
             state, counts and the kind and time of recent errors. It leaves out room names, \
             keys, messages, credentials and error text.",
            st::SMALL,
            p.text_muted,
        )
        .wrap()
        .w_full()
        .key("ns-copy-note"),
    );
    body = body.child(
        row()
            .w_full()
            .gap(8.0)
            .justify_end()
            .child(
                Button::new(if chat.status.copied {
                    "Copied"
                } else {
                    COPY_LABEL
                })
                .variant(ButtonVariant::Contrast)
                .on_press_tx(|s: &mut RiverApp, tx: &mut rune::Tx| {
                    tx.copy(format_diagnostics(&Diagnostics::of(&s.chat)));
                    s.chat.status.copied = true;
                })
                .build(k)
                .key("ns-copy"),
            )
            .child(
                Button::new("Close")
                    .variant(ButtonVariant::Ghost)
                    .on_press(|s: &mut RiverApp| s.chat.close_node_status())
                    .build(k)
                    .key("ns-close"),
            ),
    );
    Dialog::new("node-status", "Node status")
        .width(520.0)
        .fit()
        .open(true)
        .on_close(|s: &mut RiverApp| s.chat.close_node_status())
        .child(body)
        .build(k, layer);
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "node.example:7509";

    fn sample() -> Diagnostics {
        Diagnostics {
            river_version: "0.1.0".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            node_address: "127.0.0.1:7509".into(),
            node_is_remote: false,
            connection: Connection::Connected,
            node_version: Some("0.2.158".into()),
            node_answering: Some(true),
            minimum_version: "0.2.156".into(),
            latest_version: Some("0.2.160".into()),
            peers: None,
            rooms: Some(3),
            rooms_following: Some(3),
            errors: vec![(1_700_000_000, ErrorKind::NodeUnreachable)],
        }
    }

    #[test]
    fn the_diagnostics_have_a_fixed_shape() {
        let expected = "River node status\n\
            Includes: versions, OS, the node's host and port, connection state, counts, error categories. Leaves out: room names and keys, messages, credentials, error text.\n\
            River: 0.1.0\n\
            OS: linux x86_64\n\
            Node host and port (identifying if remote): 127.0.0.1:7509 (this computer)\n\
            Connection: Connected\n\
            Node version: 0.2.158 (meets the minimum; River needs 0.2.156 or newer)\n\
            Latest Freenet release: 0.2.160\n\
            Connected peers: unknown\n\
            Rooms: 3 (3 following)\n\
            Recent errors (category, newest last):\n  2023-11-14 22:13:20 UTC  node unreachable\n";
        assert_eq!(format_diagnostics(&sample()), expected);
    }

    #[test]
    fn unknown_and_silent_fields_say_so() {
        let mut d = sample();
        d.node_version = None;
        d.latest_version = None;
        d.rooms = None;
        d.rooms_following = None;
        d.errors.clear();
        let t = format_diagnostics(&d);
        assert!(
            t.contains("Node version: unknown (minimum not checked"),
            "{t}"
        );
        assert!(t.contains("Latest Freenet release: unknown"), "{t}");
        assert!(t.contains("Rooms: unknown (unknown following)"), "{t}");
        assert!(t.contains("Recent errors: none"), "{t}");
        let mut d = sample();
        d.node_answering = Some(false);
        let t = format_diagnostics(&d);
        assert!(
            t.contains("0.2.158 (meets the minimum, last seen, not answering now"),
            "{t}"
        );
    }

    #[test]
    fn credentials_in_a_node_address_never_reach_the_diagnostics() {
        for address in [
            "ws://alice:hunter2@node.example:7509/path?token=abc123",
            "http://node.example:7509/?password=hunter2",
            "alice:hunter2@node.example:7509",
            "node.example:7509?token=abc123#frag",
            "https://x:p@ss@node.example:7509",
            "ws://bob:pa/ss@node.example:7509",
            "ws://bob:p/a/s/s@node.example:7509/x",
            "ws://bob:pa?ss@node.example:7509",
            "ws://bob:pa#ss@node.example:7509",
        ] {
            assert_eq!(redact_address(address), HOME, "{address}");
            let mut d = sample();
            d.node_address = address.into();
            d.node_is_remote = true;
            let t = format_diagnostics(&d);
            for secret in [
                "hunter2", "abc123", "alice", "frag", "ss@", "bob", "pa/", "pa?", "pa#",
            ] {
                assert!(!t.contains(secret), "{secret} leaked from {address}: {t}");
            }
            assert!(t.contains(&format!("{HOME} (remote)")), "{address}: {t}");
        }
    }

    #[test]
    fn an_address_that_does_not_parse_is_not_shown() {
        for address in ["", "@", "ws://", "a b c", "bob:pa/ss@"] {
            assert_eq!(redact_address(address), "unknown address", "{address:?}");
        }
        let junk = "x".repeat(1_000_000);
        assert_eq!(redact_address(&junk), "unknown address");
    }

    /// Every shape of secret, in the places error text can carry them.
    const SECRETS: &[(&str, &str)] = &[
        (
            "failed at /home/alice/.config/river/grant.json now",
            "alice",
        ),
        (
            "failed at C:\\Users\\alice\\AppData\\river.log now",
            "alice",
        ),
        ("failed at (/home/alice/x.rs:12)", "alice"),
        ("read \"C:\\Users\\alice smith\\x.log\"", "alice"),
        ("Authorization: Basic dXNlcjpwYXNz", "dXNlcjpwYXNz"),
        ("Authorization: Bearer abc123", "abc123"),
        ("sent Bearer abc123 to it", "abc123"),
        ("password:hunter2", "hunter2"),
        ("token = abc", "abc"),
        ("token=abc", "abc"),
        ("{\"token\":\"abc123\"}", "abc123"),
        ("{\"authToken\": \"abc123\"}", "abc123"),
        ("api_key=abc123 and more", "abc123"),
        ("token%3Dabc", "abc"),
        ("ws://bob:pa/ss@host:7509/x", "bob"),
        ("ws://bob:pa/ss@host:7509/x", "pa/ss"),
        (
            "see (http://u:pw@h.example:1/x?k=SECRETVALUE).",
            "SECRETVALUE",
        ),
        (
            "grant AbCdEf0123456789AbCdEf0123456789xyz was refused",
            "AbCdEf0123456789",
        ),
    ];

    #[test]
    fn secrets_never_survive_sanitising() {
        for (text, secret) in SECRETS {
            let s = sanitize(text);
            assert!(!s.contains(secret), "{secret} survived in {text:?}: {s}");
        }
    }

    #[test]
    fn secrets_in_errors_never_reach_the_diagnostics_at_all() {
        // Free text is not copied, however it is written, so a shape the sanitiser misses
        // still stays out of the clipboard.
        let mut ui = NodeStatusUi::default();
        let at = ui.address.clone();
        for (text, _) in SECRETS {
            ui.note(ErrorKind::Refused, &at, 1_700_000_000, text);
        }
        ui.note(
            ErrorKind::SetupFailed,
            &at,
            1_700_000_001,
            "totally-novel-shape: zzSECRETzz",
        );
        let mut chat = super::super::Chat::new(Source::Example, Vec::new(), 0, 0, 0);
        chat.status = ui;
        let text = format_diagnostics(&Diagnostics::of(&chat));
        for (_, secret) in SECRETS {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
        assert!(!text.contains("zzSECRETzz"), "{text}");
        assert!(text.contains("refused by the node"), "{text}");
        assert!(text.contains("setup failed"), "{text}");
    }

    #[test]
    fn the_dialog_line_is_bounded_and_clean() {
        let huge = "a b ".repeat(300_000);
        let s = sanitize(&huge);
        assert!(s.chars().count() <= MAX_DETAIL + 1, "{}", s.len());
        let s = sanitize(&"x".repeat(1_000_000));
        assert!(s.chars().count() <= MAX_DETAIL + 1);
        let s = sanitize("ok\u{202e}evil\u{200b}\u{0007}\nnext\ttab \u{2066}x");
        assert_eq!(s, "okevil next tab x");
    }

    #[test]
    fn ordinary_words_are_kept() {
        assert_eq!(
            sanitize("The node refused River: the app is not approved."),
            "The node refused River: the app is not approved."
        );
        assert_eq!(
            sanitize("Nothing answered at localhost:7509."),
            "Nothing answered at localhost:7509."
        );
        for text in [
            "author=Ann",
            "keyboard=us",
            "monkey=see",
            "authority: example",
        ] {
            assert_eq!(sanitize(text), text, "{text}");
        }
    }

    #[test]
    fn gathered_diagnostics_hold_no_room_text() {
        let mut chat = super::super::Chat::new(
            Source::Node(LinkStatus::Live),
            super::super::example::data().rooms,
            0,
            0,
            0,
        );
        chat.status.rooms = Some((chat.rooms().len(), Some(chat.rooms().len())));
        chat.status.connection = Connection::Connected;
        let text = format_diagnostics(&Diagnostics::of(&chat));
        for room in chat.rooms() {
            assert!(!text.contains(&room.name), "{}", room.name);
            assert!(!text.contains(&room.key), "{}", room.key);
            for m in &room.messages {
                assert!(!text.contains(&m.text), "{}", m.text);
            }
        }
    }

    #[test]
    fn versions_compare_as_the_desktop_host_does() {
        assert!(parse_version("0.2").is_none());
        assert!(parse_version("x.y.z").is_none());
        // The host: a pre-release is below its release, a later patch's pre-release is above.
        assert_eq!(meets_minimum(Some("0.2.156"), "0.2.156"), Needs::Meets);
        assert_eq!(
            meets_minimum(Some("0.2.156-rc.1"), "0.2.156"),
            Needs::TooOld
        );
        assert_eq!(meets_minimum(Some("0.2.157-rc.1"), "0.2.156"), Needs::Meets);
        assert_eq!(meets_minimum(Some("0.2.156+abc"), "0.2.156"), Needs::Meets);
        assert_eq!(meets_minimum(Some("0.2.155"), "0.2.156"), Needs::TooOld);
        assert_eq!(meets_minimum(None, "0.2.156"), Needs::Unknown);
        assert_eq!(
            against_latest(Some("0.2.158"), Some("0.2.160")),
            Recency::Behind
        );
        assert_eq!(
            against_latest(Some("0.2.160-rc.1"), Some("0.2.160")),
            Recency::Behind
        );
        assert_eq!(
            against_latest(Some("0.2.160"), Some("0.2.160")),
            Recency::Latest
        );
        assert_eq!(
            against_latest(Some("0.3.0"), Some("0.2.160")),
            Recency::Ahead
        );
        assert_eq!(against_latest(Some("0.2.160"), None), Recency::Unknown);
    }

    #[test]
    fn utc_stamps_are_calendar_correct() {
        assert_eq!(utc_stamp(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc_stamp(951_782_400), "2000-02-29 00:00:00 UTC");
        assert_eq!(utc_stamp(1_700_000_000), "2023-11-14 22:13:20 UTC");
    }

    #[test]
    fn refresh_waits_for_the_answer_and_the_interval() {
        let t0 = Instant::now();
        let mut ui = NodeStatusUi::default();
        assert!(!ui.should_refresh(t0), "closed: no work");
        ui.open();
        assert!(ui.should_refresh(t0));
        ui.refresh_started();
        assert!(
            !ui.should_refresh(t0 + Duration::from_secs(60)),
            "one question at a time"
        );
        let at = ui.address.clone();
        ui.apply(
            &at,
            StatusEvent::Reach {
                authority: at.clone(),
                reach: Reach::Silent,
            },
            t0,
        );
        assert!(!ui.should_refresh(t0 + REFRESH_EVERY - Duration::from_millis(1)));
        assert!(ui.should_refresh(t0 + REFRESH_EVERY));
        ui.close();
        assert!(!ui.should_refresh(t0 + REFRESH_EVERY * 10));
        assert!(!ui.wants_latest(), "no lookup while closed");
    }

    #[test]
    fn an_aborted_worker_frees_the_next_question() {
        let t0 = Instant::now();
        let mut ui = NodeStatusUi::default();
        ui.open();
        ui.refresh_started();
        ui.apply("x", StatusEvent::Aborted, t0);
        assert!(ui.should_refresh(t0 + REFRESH_EVERY));
        ui.latest_started();
        ui.apply("x", StatusEvent::Latest(None), t0);
        assert_eq!(ui.latest, Latest::Unavailable);
        assert!(!ui.wants_latest());
        ui.close();
        ui.open();
        assert!(
            ui.wants_latest(),
            "a failed lookup is retried when it reopens"
        );
    }

    #[test]
    fn the_latest_release_is_asked_once_per_opening() {
        let mut ui = NodeStatusUi::default();
        assert!(!ui.wants_latest());
        ui.open();
        assert!(ui.wants_latest());
        ui.latest_started();
        assert!(!ui.wants_latest());
        ui.apply(
            "x",
            StatusEvent::Latest(Some("0.2.160".into())),
            Instant::now(),
        );
        ui.close();
        ui.open();
        assert!(!ui.wants_latest(), "a known release is kept");
    }

    #[test]
    fn an_answer_for_another_node_is_dropped_but_ends_the_wait() {
        let t0 = Instant::now();
        let mut ui = NodeStatusUi::default();
        ui.open();
        ui.refresh_started();
        ui.apply(
            "127.0.0.1:7509",
            StatusEvent::Reach {
                authority: "old.example:7509".into(),
                reach: Reach::Answered {
                    version: "0.2.1".into(),
                },
            },
            t0,
        );
        assert_eq!(ui.reach, None);
        assert_eq!(ui.node_version(None), None);
        assert!(ui.should_refresh(t0 + REFRESH_EVERY));
    }

    #[test]
    fn a_silent_node_keeps_its_last_known_version() {
        let t0 = Instant::now();
        let mut ui = NodeStatusUi::default();
        let at = ui.address.clone();
        assert_eq!(ui.node_version(Some("0.2.155")), Some("0.2.155"));
        assert_eq!(ui.answering(), None);
        let event = |reach| StatusEvent::Reach {
            authority: at.clone(),
            reach,
        };
        ui.apply(
            &at,
            event(Reach::Answered {
                version: "0.2.158".into(),
            }),
            t0,
        );
        assert_eq!(ui.node_version(Some("0.2.155")), Some("0.2.158"));
        assert_eq!(ui.answering(), Some(true));
        ui.apply(&at, event(Reach::Silent), t0);
        assert_eq!(ui.node_version(Some("0.2.155")), Some("0.2.158"));
        assert_eq!(ui.answering(), Some(false));
        ui.address = "elsewhere:1".into();
        assert_eq!(ui.node_version(Some("0.2.155")), Some("0.2.155"));
        assert_eq!(ui.answering(), None);
    }

    #[test]
    fn errors_are_tagged_by_node_deduplicated_and_capped() {
        let mut ui = NodeStatusUi::default();
        let here = ui.address.clone();
        ui.note(ErrorKind::Refused, &here, 1, "failed token=abc");
        ui.note(ErrorKind::Refused, &here, 2, "failed token=abc");
        assert_eq!(ui.errors.len(), 1);
        assert_eq!(ui.errors[0].detail, "failed token=[removed]");
        ui.note(ErrorKind::Refused, "other:1", 3, "from another node");
        assert_eq!(
            ui.last_error().map(|e| e.at),
            Some(1),
            "another node's error is not shown"
        );
        for i in 0..10 {
            ui.note(
                ErrorKind::SetupFailed,
                &here,
                10 + i,
                &format!("problem {i}"),
            );
        }
        assert_eq!(ui.errors.len(), MAX_ERRORS);
        assert_eq!(
            ui.last_error().map(|e| e.detail.as_str()),
            Some("problem 9")
        );
    }

    #[test]
    fn a_lost_connection_is_noted_once_per_loss() {
        let mut ui = NodeStatusUi::default();
        let at = ui.address.clone();
        ui.note_link(LinkStatus::Live, &at, 1);
        ui.note_link(LinkStatus::Reconnecting, &at, 2);
        ui.note_link(LinkStatus::Reconnecting, &at, 3);
        assert_eq!(ui.errors.len(), 1);
        assert_eq!(ui.errors[0].kind, ErrorKind::ConnectionLost);
    }

    #[test]
    fn access_problems_are_recorded_by_kind() {
        let mut ui = NodeStatusUi::default();
        let at = ui.address.clone();
        ui.note_access(&Access::Refused("bad token=abc".into()), &at, 1);
        assert_eq!(ui.errors[0].kind, ErrorKind::Refused);
        assert_eq!(ui.errors[0].detail, "bad token=[removed]");
        ui.note_access(&Access::Ready, &at, 2);
        ui.note_access(&Access::Connecting, &at, 2);
        assert_eq!(ui.errors.len(), 1);
    }

    #[test]
    fn the_connection_follows_setup_access_and_link() {
        let connected = Setup::Connected {
            version: "0.2.158".into(),
        };
        let live = Source::Node(LinkStatus::Live);
        assert_eq!(
            connection_of(&connected, &Access::Ready, live),
            Connection::Connected
        );
        assert_eq!(
            connection_of(
                &connected,
                &Access::Ready,
                Source::Node(LinkStatus::Reconnecting)
            ),
            Connection::Reconnecting
        );
        assert_eq!(
            connection_of(
                &connected,
                &Access::Ready,
                Source::Node(LinkStatus::Loading)
            ),
            Connection::Connecting
        );
        assert_eq!(
            connection_of(
                &connected,
                &Access::WaitingForApproval {
                    approve_url: String::new()
                },
                live
            ),
            Connection::WaitingForApproval
        );
        assert_eq!(
            connection_of(&connected, &Access::Unreachable(String::new()), live),
            Connection::Unreachable
        );
        assert_eq!(
            connection_of(&Setup::Checking, &Access::Connecting, live),
            Connection::Connecting
        );
        assert_eq!(
            connection_of(&Setup::Failed("x".into()), &Access::Connecting, live),
            Connection::Problem
        );
        assert_eq!(
            connection_of(&connected, &Access::Example, Source::Example),
            Connection::Example
        );
    }
}
