//! Which Freenet node River talks to: its address, how it is chosen, and where the choice is
//! kept.
//!
//! The default is the node on this computer, `127.0.0.1:7509`. A person who runs a node on
//! another machine can point River at it instead. The address comes from, in order: the
//! `--node` flag, the `FREENET_NODE` environment variable, the address saved from the app's
//! own screens, and last the default. Any address but the default means "use a node I already
//! have": River then never installs, starts or stops a Freenet on this computer.
//!
//! A name such as `localhost:7600` can front different nodes at different times (a tunnel to one
//! machine today, to another tomorrow); River keeps a separate saved permission for each
//! address, and a node that no longer accepts it is asked again.
//!
//! An address is `host:port` and nothing else: no scheme, path, query, credentials or spaces.
//! It is parsed strictly, because it ends up in a URL, a WebSocket handshake and a `Host`
//! header, and the node's answers are checked against it.

use std::ffi::OsString;
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// The node's client API on this computer.
pub const DEFAULT_AUTHORITY: &str = "127.0.0.1:7509";
/// The port a bare host name is given.
pub const DEFAULT_PORT: u16 = 7509;
/// The environment variable that names the node.
pub const ENV_VAR: &str = "FREENET_NODE";
/// The command line flag that names the node.
pub const FLAG: &str = "--node";

/// The longest host name DNS allows.
const MAX_HOST_LEN: usize = 253;
const MAX_LABEL_LEN: usize = 63;

/// A node's client API address: a host (name or IP) and a port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeAddress {
    host: Host,
    port: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Host {
    /// A DNS name, lowercased.
    Name(String),
    V4(Ipv4Addr),
    V6(Ipv6Addr),
}

/// Why a typed address was refused. The text is shown to the person.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddressError(pub String);

impl fmt::Display for AddressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn bad(why: &str) -> AddressError {
    AddressError(why.to_owned())
}

impl NodeAddress {
    /// The node on this computer, on Freenet's default port.
    pub fn default_local() -> Self {
        NodeAddress {
            host: Host::V4(Ipv4Addr::LOCALHOST),
            port: DEFAULT_PORT,
        }
    }

    /// Parse `host:port`. A host with no port gets [`DEFAULT_PORT`]. IPv6 addresses are written
    /// in brackets (`[::1]:7509`). Surrounding whitespace is ignored; nothing else is.
    pub fn parse(input: &str) -> Result<NodeAddress, AddressError> {
        let s = input.trim();
        if s.is_empty() {
            return Err(bad("Enter the node's address, such as localhost:7509."));
        }
        if s.contains("://") {
            return Err(bad(
                "Enter just the host and port, such as localhost:7509, without http:// or any \
                 other prefix.",
            ));
        }
        if s.contains(['/', '?', '#', '\\']) {
            return Err(bad(
                "Enter just the host and port, such as localhost:7509, with no path after it.",
            ));
        }
        if s.contains('@') {
            return Err(bad(
                "Enter just the host and port. A user name or password isn't used.",
            ));
        }
        if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(bad("An address has no spaces in it."));
        }
        if !s.is_ascii() {
            return Err(bad(
                "Use the ASCII form of the host name (internationalized names are written in \
                 their xn-- form).",
            ));
        }
        if s.contains('%') {
            return Err(bad("An address can't contain %."));
        }
        let (host_text, port_text) = if let Some(rest) = s.strip_prefix('[') {
            let (inner, after) = rest
                .split_once(']')
                .ok_or_else(|| bad("An IPv6 address needs a closing ]."))?;
            let port = match after {
                "" => None,
                p => Some(
                    p.strip_prefix(':')
                        .ok_or_else(|| bad("After the ] comes :port, such as [::1]:7509."))?,
                ),
            };
            let v6: Ipv6Addr = inner
                .parse()
                .map_err(|_| bad("That isn't a valid IPv6 address."))?;
            return NodeAddress::finish(Host::V6(v6), port);
        } else {
            match s.split_once(':') {
                Some((_, p)) if p.contains(':') => {
                    return Err(bad(
                        "An IPv6 address goes in brackets with the port after, such as \
                         [::1]:7509.",
                    ));
                }
                Some((h, p)) => (h, Some(p)),
                None => (s, None),
            }
        };
        let host = parse_host(host_text)?;
        NodeAddress::finish(host, port_text)
    }

    fn finish(host: Host, port: Option<&str>) -> Result<NodeAddress, AddressError> {
        let port = match port {
            None => DEFAULT_PORT,
            Some(p) => parse_port(p)?,
        };
        let unusable = match &host {
            Host::V4(a) => a.is_unspecified() || a.is_multicast() || a.is_broadcast(),
            Host::V6(a) => a.is_unspecified() || a.is_multicast(),
            Host::Name(_) => false,
        };
        if unusable {
            return Err(bad("That address can't be a Freenet node's."));
        }
        Ok(NodeAddress { host, port })
    }

    /// `host:port` as used in URLs and the `Host` header, with IPv6 in brackets.
    pub fn authority(&self) -> String {
        format!("{self}")
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Whether this is the default address, the node on this computer.
    pub fn is_default(&self) -> bool {
        *self == NodeAddress::default_local()
    }

    /// Whether the address is on this computer, as the node itself judges a `Host`: exactly
    /// `localhost`, `127.0.0.1` or `::1` (freenet-core's app grants accept no other name, and
    /// not the rest of `127.0.0.0/8`). Any other name could point anywhere.
    pub fn is_loopback(&self) -> bool {
        match &self.host {
            Host::Name(n) => n == "localhost",
            Host::V4(a) => *a == Ipv4Addr::LOCALHOST,
            Host::V6(a) => *a == Ipv6Addr::LOCALHOST,
        }
    }
}

impl fmt::Display for NodeAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.host {
            Host::Name(n) => write!(f, "{n}:{}", self.port),
            Host::V4(a) => write!(f, "{a}:{}", self.port),
            Host::V6(a) => write!(f, "[{a}]:{}", self.port),
        }
    }
}

fn parse_port(p: &str) -> Result<u16, AddressError> {
    let ok = !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit());
    match ok.then(|| p.parse::<u16>().ok()).flatten() {
        Some(n) if n >= 1 => Ok(n),
        _ => Err(bad("The port is a number from 1 to 65535.")),
    }
}

fn parse_host(h: &str) -> Result<Host, AddressError> {
    if h.is_empty() {
        return Err(bad("The address has no host before the port."));
    }
    // An all-digits-and-dots host is an IPv4 address and must be written in full: the system's
    // resolver would read `127.1` or `2130706433` as 127.0.0.1, which River would not.
    if h.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return h
            .parse::<Ipv4Addr>()
            .map(Host::V4)
            .map_err(|_| bad("That isn't a valid IPv4 address."));
    }
    if h.len() > MAX_HOST_LEN {
        return Err(bad("That host name is too long."));
    }
    let labels: Vec<&str> = h.split('.').collect();
    for label in &labels {
        let valid = !label.is_empty()
            && label.len() <= MAX_LABEL_LEN
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-');
        if !valid {
            return Err(bad(
                "A host name is letters, digits and hyphens in dot-separated parts.",
            ));
        }
    }
    // A name whose last part is a number (or a hex number) can be read as an address by the
    // resolver: refuse it.
    let last = labels[labels.len() - 1];
    if last.bytes().all(|b| b.is_ascii_digit()) || last.to_ascii_lowercase().starts_with("0x") {
        return Err(bad("That isn't a valid host name or IP address."));
    }
    Ok(Host::Name(h.to_ascii_lowercase()))
}

/// Where the address in use came from, strongest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Flag,
    Env,
    Saved,
    Default,
}

/// The node River uses: its address and where that came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeChoice {
    pub address: NodeAddress,
    pub source: Source,
}

impl NodeChoice {
    pub fn local() -> Self {
        NodeChoice {
            address: NodeAddress::default_local(),
            source: Source::Default,
        }
    }

    /// A node the person chose in the app, to be saved.
    pub fn saved(address: NodeAddress) -> Self {
        NodeChoice {
            address,
            source: Source::Saved,
        }
    }

    /// Whether River should use a node somebody else set up, and so leave this computer's own
    /// Freenet alone: anything chosen by flag, variable or the app's screens, even the default
    /// address (a tunnel can be forwarded to it).
    pub fn is_existing_node(&self) -> bool {
        self.source != Source::Default
    }

    /// Choose by precedence: flag, then environment, then the saved address, then the default.
    /// A flag or variable that doesn't parse is an error, since a script that sets one wants
    /// that node and nothing else. A saved address that doesn't parse (the file was edited) is
    /// ignored.
    pub fn resolve(
        flag: Option<&str>,
        env: Option<&str>,
        saved: Option<&str>,
    ) -> Result<NodeChoice, String> {
        let from = |value: &str, source: Source, name: &str| {
            NodeAddress::parse(value)
                .map(|address| NodeChoice { address, source })
                .map_err(|e| format!("{name} \"{}\" isn't a node address: {e}", shown(value)))
        };
        if let Some(v) = flag {
            return from(v, Source::Flag, FLAG);
        }
        if let Some(v) = env.filter(|v| !v.trim().is_empty()) {
            return from(v, Source::Env, ENV_VAR);
        }
        if let Some(v) = saved
            && let Ok(address) = NodeAddress::parse(v)
        {
            return Ok(NodeChoice {
                address,
                source: Source::Saved,
            });
        }
        Ok(NodeChoice::local())
    }

    /// Resolve from the process's arguments and environment and the saved address in
    /// `config_dir`.
    pub fn from_environment(
        args: impl IntoIterator<Item = OsString>,
        config_dir: Option<&Path>,
    ) -> Result<NodeChoice, String> {
        let flag = flag_value(args)?;
        let env = match std::env::var(ENV_VAR) {
            Ok(v) => Some(v),
            Err(std::env::VarError::NotPresent) => None,
            Err(_) => return Err(format!("{ENV_VAR} isn't valid text.")),
        };
        let saved = config_dir.and_then(|d| NodeStore::in_dir(d).load());
        NodeChoice::resolve(flag.as_deref(), env.as_deref(), saved.as_deref())
    }

    /// What the app starts with. Never fails: a flag or variable that isn't an address, or a
    /// saved address that isn't valid, is returned for the window to say, and the node on this
    /// computer is not touched until the person chooses.
    pub fn startup(args: impl IntoIterator<Item = OsString>, config_dir: Option<&Path>) -> Startup {
        match NodeChoice::from_environment(args, config_dir) {
            Ok(choice) => {
                let notice = config_dir
                    .and_then(|d| NodeStore::in_dir(d).load())
                    .filter(|saved| {
                        choice.source != Source::Flag
                            && choice.source != Source::Env
                            && NodeAddress::parse(saved).is_err()
                    })
                    .map(|saved| {
                        format!(
                            "Your saved node address {} is not valid; using this computer's \
                             Freenet.",
                            shown(&saved)
                        )
                    });
                Startup {
                    choice,
                    notice,
                    problem: None,
                }
            }
            Err(problem) => Startup {
                choice: NodeChoice::local(),
                notice: None,
                problem: Some(problem),
            },
        }
    }
}

/// How the app starts: the node, and anything the person has to be told first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Startup {
    pub choice: NodeChoice,
    /// Said above the rooms, and the app goes on with `choice`.
    pub notice: Option<String>,
    /// The app does not go on until the person chooses a node: the flag or variable they gave
    /// was not an address.
    pub problem: Option<String>,
}

/// A value for an error message: short, with controls dropped.
fn shown(v: &str) -> String {
    v.chars().filter(|c| !c.is_control()).take(80).collect()
}

/// The value of `--node host:port` or `--node=host:port` in `args` (the program name first).
pub fn flag_value(args: impl IntoIterator<Item = OsString>) -> Result<Option<String>, String> {
    let text = |a: OsString| {
        a.into_string()
            .map_err(|_| format!("{FLAG} wasn't given valid text."))
    };
    let mut args = args.into_iter().skip(1);
    let mut found = None;
    while let Some(a) = args.next() {
        // An argument that isn't text can't be ours.
        let Some(a) = a.to_str().map(str::to_owned) else {
            continue;
        };
        let value = if a == FLAG {
            let v = args
                .next()
                .ok_or_else(|| format!("{FLAG} needs an address, such as {FLAG} host:7509"))?;
            Some(text(v)?)
        } else {
            a.strip_prefix("--node=").map(str::to_owned)
        };
        if let Some(v) = value {
            found = Some(v);
        }
    }
    Ok(found)
}

/// The saved node address, a one-line text file in the app's config directory.
pub struct NodeStore {
    path: PathBuf,
}

const FILE_NAME: &str = "node-address";

impl NodeStore {
    pub fn in_dir(dir: &Path) -> Self {
        NodeStore {
            path: dir.join(FILE_NAME),
        }
    }

    pub fn load(&self) -> Option<String> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        let line = text.lines().next()?.trim();
        (!line.is_empty()).then(|| line.to_owned())
    }

    /// Save `address`, written whole or not at all.
    pub fn save(&self, address: &NodeAddress) -> Result<(), String> {
        let dir = self.path.parent().ok_or("bad node address path")?;
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
        let tmp = dir.join(format!(".{FILE_NAME}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, format!("{address}\n"))
            .and_then(|()| std::fs::rename(&tmp, &self.path))
            .map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                format!("couldn't save the node address: {e}")
            })
    }

    /// Forget the saved address, so River uses the node on this computer.
    pub fn clear(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The authority links River writes into messages and descriptions point at: the node in use.
static CURRENT: RwLock<Option<String>> = RwLock::new(None);

#[cfg(test)]
thread_local! {
    /// A test's own node, so tests running side by side never see each other's.
    static TEST_AUTHORITY: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// The authority of the node in use, `127.0.0.1:7509` unless one was chosen.
pub fn current_authority() -> String {
    #[cfg(test)]
    if let Some(a) = TEST_AUTHORITY.with(|t| t.borrow().clone()) {
        return a;
    }
    CURRENT
        .read()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_else(|| DEFAULT_AUTHORITY.to_owned())
}

/// Record the node in use, for the links River writes.
pub fn set_current(address: &NodeAddress) {
    if let Ok(mut g) = CURRENT.write() {
        *g = Some(address.authority());
    }
}

/// For tests: use `authority` on this thread only, until the guard drops.
#[cfg(test)]
pub(crate) fn use_authority_in_test(authority: &str) -> impl Drop {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_AUTHORITY.with(|t| *t.borrow_mut() = None);
        }
    }
    TEST_AUTHORITY.with(|t| *t.borrow_mut() = Some(authority.to_owned()));
    Reset
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> String {
        NodeAddress::parse(s)
            .unwrap_or_else(|e| panic!("{s:?} should parse: {e}"))
            .authority()
    }

    #[test]
    fn valid_addresses_normalise() {
        let cases = [
            ("127.0.0.1:7509", "127.0.0.1:7509"),
            ("localhost:7509", "localhost:7509"),
            ("  localhost:40302  ", "localhost:40302"),
            ("LocalHost:1", "localhost:1"),
            ("node.example.com:8080", "node.example.com:8080"),
            ("My-Node.Example.COM:65535", "my-node.example.com:65535"),
            ("192.168.1.20:7509", "192.168.1.20:7509"),
            ("10.0.0.5", "10.0.0.5:7509"),
            ("myserver", "myserver:7509"),
            ("[::1]:7509", "[::1]:7509"),
            ("[::1]", "[::1]:7509"),
            ("[0:0:0:0:0:0:0:1]:9", "[::1]:9"),
            ("[fd00::2]:7509", "[fd00::2]:7509"),
            ("[2001:DB8::1]:80", "[2001:db8::1]:80"),
            ("a:7509", "a:7509"),
            ("xn--bcher-kva.example:7509", "xn--bcher-kva.example:7509"),
        ];
        for (input, want) in cases {
            assert_eq!(ok(input), want, "{input:?}");
        }
    }

    #[test]
    fn bad_addresses_are_refused() {
        let cases = [
            "",
            "   ",
            "http://localhost:7509",
            "https://localhost:7509/",
            "ws://localhost:7509",
            "//localhost:7509",
            "localhost:7509/",
            "localhost:7509/v1/version",
            "localhost:7509?x=1",
            "localhost:7509#frag",
            "localhost:7509\\x",
            "user@localhost:7509",
            "user:pass@localhost:7509",
            "127.0.0.1:7509@evil.example",
            "evil.example:7509@127.0.0.1:7509",
            "localhost: 7509",
            "local host:7509",
            "localhost\n:7509",
            "localhost:7509\r\nHost: evil",
            "localhost\t:7509",
            "localhost:",
            ":7509",
            ":",
            "localhost:0",
            "localhost:65536",
            "localhost:-1",
            "localhost:+80",
            "localhost:0x50",
            "localhost:80a",
            "localhost:007509000",
            "localhost:7509:7509",
            "::1",
            "::1:7509",
            "[::1",
            "[::1]7509",
            "[::1]:",
            "[::1]:abc",
            "[]:7509",
            "[localhost]:7509",
            "[127.0.0.1]:7509",
            "[fe80::1%eth0]:7509",
            "fe80::1%25eth0",
            "127.1:7509",
            "127.0.1:7509",
            "2130706433:7509",
            "0x7f.0.0.1:7509",
            "0x7f000001:7509",
            "127.0.0.1.:7509",
            "127.0.0.256:7509",
            "127.000.000.001:7509",
            "0.0.0.0:7509",
            "255.255.255.255:7509",
            "224.0.0.1:7509",
            "[::]:7509",
            "[ff02::1]:7509",
            "bücher.example:7509",
            "-bad.example:7509",
            "bad-.example:7509",
            "bad_name.example:7509",
            "a..b:7509",
            ".example:7509",
            "example.:7509",
            "exa mple:7509",
            "example.123:7509",
            "example.0x1f:7509",
            "ex%41mple:7509",
            "exa*mple:7509",
            "localhost:7509,evil.example:7509",
            "localhost;7509",
        ];
        for input in cases {
            assert!(
                NodeAddress::parse(input).is_err(),
                "{input:?} should be refused, got {:?}",
                NodeAddress::parse(input)
            );
        }
        let long_label = format!("{}.example:7509", "a".repeat(64));
        assert!(NodeAddress::parse(&long_label).is_err());
        let long_host = format!("{}:7509", ["abcdefgh"; 40].join("."));
        assert!(NodeAddress::parse(&long_host).is_err());
    }

    #[test]
    fn loopback_is_judged_on_the_exact_address() {
        for (s, loopback) in [
            ("127.0.0.1:7509", true),
            ("127.1.2.3:7509", false),
            ("127.0.0.2:7509", false),
            ("localhost:7509", true),
            ("LOCALHOST:7509", true),
            ("[::1]:7509", true),
            ("[0:0:0:0:0:0:0:1]:7509", true),
            ("192.168.1.2:7509", false),
            ("10.0.0.1:7509", false),
            ("128.0.0.1:7509", false),
            ("node.example.com:7509", false),
            // A name that merely starts like localhost is a name like any other.
            ("localhost.example.com:7509", false),
            ("foo.localhost:7509", false),
            ("notlocalhost:7509", false),
            ("[::2]:7509", false),
            ("[fd00::1]:7509", false),
            // An IPv4-mapped loopback is not what the node's Host check accepts.
            ("[::ffff:127.0.0.1]:7509", false),
        ] {
            assert_eq!(
                NodeAddress::parse(s).unwrap().is_loopback(),
                loopback,
                "{s}"
            );
        }
    }

    #[test]
    fn only_the_canonical_local_address_is_the_default() {
        assert!(NodeAddress::default_local().is_default());
        assert!(NodeAddress::parse("127.0.0.1:7509").unwrap().is_default());
        for s in [
            "localhost:7509",
            "127.0.0.1:7510",
            "[::1]:7509",
            "127.0.0.2:7509",
        ] {
            assert!(!NodeAddress::parse(s).unwrap().is_default(), "{s}");
        }
    }

    #[test]
    fn the_flag_beats_the_variable_beats_the_saved_address() {
        let pick = |f, e, s| NodeChoice::resolve(f, e, s).unwrap();
        let c = pick(Some("a:1"), Some("b:2"), Some("c:3"));
        assert_eq!(
            (c.address.authority().as_str(), c.source),
            ("a:1", Source::Flag)
        );
        let c = pick(None, Some("b:2"), Some("c:3"));
        assert_eq!(
            (c.address.authority().as_str(), c.source),
            ("b:2", Source::Env)
        );
        let c = pick(None, None, Some("c:3"));
        assert_eq!(
            (c.address.authority().as_str(), c.source),
            ("c:3", Source::Saved)
        );
        let c = pick(None, None, None);
        assert_eq!(c, NodeChoice::local());
        assert!(!c.is_existing_node());
        // An empty variable is unset, as shells leave it.
        assert_eq!(pick(None, Some("  "), None), NodeChoice::local());
        for c in [pick(Some("a:1"), None, None), pick(None, Some("b"), None)] {
            assert!(c.is_existing_node());
        }
    }

    #[test]
    fn a_bad_flag_or_variable_is_an_error_not_a_fallback() {
        let e = NodeChoice::resolve(Some("http://x"), None, Some("c:3")).unwrap_err();
        assert!(e.contains("--node") && e.contains("http://x"), "{e}");
        let e = NodeChoice::resolve(None, Some("a b"), Some("c:3")).unwrap_err();
        assert!(e.contains("FREENET_NODE"), "{e}");
        // A saved address that was edited into nonsense is ignored.
        assert_eq!(
            NodeChoice::resolve(None, None, Some("http://x/")).unwrap(),
            NodeChoice::local()
        );
        // Control characters in the value are not echoed.
        let e = NodeChoice::resolve(Some("a\u{1b}[31m b"), None, None).unwrap_err();
        assert!(!e.contains('\u{1b}'), "{e:?}");
    }

    #[test]
    fn the_flag_is_found_in_either_form() {
        let args = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(flag_value(args(&["river"])), Ok(None));
        assert_eq!(
            flag_value(args(&["river", "--node", "h:1"])),
            Ok(Some("h:1".into()))
        );
        assert_eq!(
            flag_value(args(&["river", "--node=h:2", "--other"])),
            Ok(Some("h:2".into()))
        );
        assert!(flag_value(args(&["river", "--node"])).is_err());
        // The program name is never read as a flag.
        assert_eq!(flag_value(args(&["--node", "x"])), Ok(None));
    }

    #[test]
    fn the_store_round_trips_and_clears() {
        let dir = std::env::temp_dir().join(format!("river-node-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = NodeStore::in_dir(&dir);
        assert_eq!(store.load(), None);
        let a = NodeAddress::parse("[::1]:4000").unwrap();
        store.save(&a).unwrap();
        assert_eq!(store.load().as_deref(), Some("[::1]:4000"));
        let c = NodeChoice::from_environment(vec![OsString::from("river")], Some(&dir));
        // The test process may have FREENET_NODE set; only check the saved path when it has not.
        if std::env::var_os(ENV_VAR).is_none() {
            assert_eq!(c.unwrap(), NodeChoice::saved(a));
        }
        store.clear();
        assert_eq!(store.load(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn links_follow_the_node_in_use() {
        assert_eq!(current_authority(), DEFAULT_AUTHORITY);
        let _g = use_authority_in_test("localhost:40302");
        assert_eq!(current_authority(), "localhost:40302");
    }

    #[test]
    #[cfg(unix)]
    fn arguments_that_are_not_text_do_not_panic() {
        use std::os::unix::ffi::OsStringExt;
        let bad = OsString::from_vec(vec![0xff, 0xfe]);
        let args = vec![OsString::from("river"), bad.clone()];
        assert_eq!(flag_value(args), Ok(None));
        let args = vec![OsString::from("river"), OsString::from("--node"), bad];
        assert!(flag_value(args).is_err());
    }

    #[test]
    fn a_bad_flag_starts_the_app_on_a_problem_and_a_bad_saved_address_is_said() {
        let dir = std::env::temp_dir().join(format!("river-startup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let args = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        let st = NodeChoice::startup(args(&["river", "--node", "a@b"]), Some(&dir));
        assert!(st.problem.as_deref().unwrap().contains("--node"));
        assert_eq!(st.choice, NodeChoice::local());
        // A saved address that was edited into nonsense is said, not silently dropped.
        if std::env::var_os(ENV_VAR).is_none() {
            std::fs::write(dir.join(FILE_NAME), "http://x/\n").unwrap();
            let st = NodeChoice::startup(args(&["river"]), Some(&dir));
            assert_eq!(st.choice, NodeChoice::local());
            let n = st.notice.unwrap();
            assert!(n.contains("http://x/") && n.contains("not valid"), "{n}");
            assert!(st.problem.is_none());
            std::fs::write(dir.join(FILE_NAME), "h:1\n").unwrap();
            assert!(
                NodeChoice::startup(args(&["river"]), Some(&dir))
                    .notice
                    .is_none()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
