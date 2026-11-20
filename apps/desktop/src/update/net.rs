//! Fetching the manifest and the artifact.
//!
//! The rules, all enforced here rather than left to the HTTP library:
//!
//! - **HTTPS only.** Plain HTTP is accepted only for a loopback address, and only when the
//!   caller turned that on (tests, and a manifest URL overridden by environment).
//! - **Redirects are followed by hand**, at most [`MAX_REDIRECTS`], and only to the origin
//!   (scheme, host and port) of the URL asked for. The one exception is GitHub, whose release
//!   downloads are always served from its asset hosts ([`GITHUB_ASSET_HOSTS`]). A redirect
//!   to anywhere else is refused, as is one that downgrades to HTTP.
//! - **Size caps** that do not trust the server: a `Content-Length` over the cap is refused
//!   up front, and a body is cut off the moment it passes the cap, whatever it announced.
//! - **Deadlines**: a connect timeout, a read timeout per read, and an overall deadline, so a
//!   server that trickles bytes cannot hold the updater forever.
//!
//! None of this is what makes an update trustworthy: the signature and the SHA-256 do that.
//! It limits what an unauthenticated server can make the app spend or reveal.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

pub const MAX_REDIRECTS: usize = 3;
pub const GITHUB_ASSET_HOSTS: &[&str] = &[
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "github-releases.githubusercontent.com",
];
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
pub const SMALL_DEADLINE: Duration = Duration::from_secs(60);
pub const ARTIFACT_DEADLINE: Duration = Duration::from_secs(60 * 60);
const PROGRESS_EVERY: Duration = Duration::from_millis(150);

/// Scheme, host and port of a URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Origin {
    pub https: bool,
    pub host: String,
    pub port: u16,
}

/// Parse `url` far enough to know where it points, refusing anything unusual: userinfo,
/// control characters, a non-numeric port, a scheme other than `https` (or `http` to loopback
/// when `allow_loopback_http`).
pub fn parse_origin(url: &str, allow_loopback_http: bool) -> Result<Origin, String> {
    let bad = || format!("not an acceptable download address: {url:?}");
    if !url.is_ascii() || url.bytes().any(|b| b <= b' ' || b == 0x7f || b == b'\\') {
        return Err(bad());
    }
    let (https, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(bad());
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') || authority.starts_with('[') {
        return Err(bad());
    }
    let (host, port) = match authority.split_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().map_err(|_| bad())?),
        None => (authority, if https { 443 } else { 80 }),
    };
    let host = host.to_ascii_lowercase();
    let host_ok = !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.');
    if !host_ok {
        return Err(bad());
    }
    if !https && !(allow_loopback_http && (host == "127.0.0.1" || host == "localhost")) {
        return Err(format!("{url:?} is not HTTPS"));
    }
    Ok(Origin { https, host, port })
}

/// Whether a redirect from `from` to `to` may be followed.
pub fn redirect_allowed(from: &Origin, to: &Origin) -> bool {
    if from.https && !to.https {
        return false;
    }
    if from == to {
        return true;
    }
    from.https
        && to.https
        && to.port == 443
        && from.host == "github.com"
        && GITHUB_ASSET_HOSTS.contains(&to.host.as_str())
}

/// Resolve a `Location` against the URL it answered.
fn resolve(base: &str, base_origin: &Origin, location: &str) -> Result<String, String> {
    if location.starts_with("https://") || location.starts_with("http://") {
        return Ok(location.to_string());
    }
    if location.starts_with('/') && !location.starts_with("//") {
        let scheme = if base_origin.https { "https" } else { "http" };
        return Ok(format!(
            "{scheme}://{}:{}{location}",
            base_origin.host, base_origin.port
        ));
    }
    Err(format!(
        "redirect from {base} to an address River will not follow"
    ))
}

pub struct Net {
    agent: ureq::Agent,
    allow_loopback_http: bool,
}

impl Net {
    pub fn new(allow_loopback_http: bool) -> Net {
        Net {
            agent: ureq::AgentBuilder::new()
                .timeout_connect(CONNECT_TIMEOUT)
                .timeout_read(READ_TIMEOUT)
                .redirects(0)
                .user_agent(concat!("river-desktop/", env!("CARGO_PKG_VERSION")))
                .build(),
            allow_loopback_http,
        }
    }

    /// GET `url`, following redirects by the rules above. The response is a 2xx.
    fn open(&self, url: &str) -> Result<ureq::Response, String> {
        let mut current = url.to_string();
        let mut origin = parse_origin(&current, self.allow_loopback_http)?;
        for _ in 0..=MAX_REDIRECTS {
            let resp = match self.agent.get(&current).call() {
                Ok(r) => r,
                Err(ureq::Error::Status(code, _)) => {
                    return Err(format!("the server answered {code}"));
                }
                Err(e) => return Err(format!("couldn't reach the server: {e}")),
            };
            if !(300..400).contains(&resp.status()) {
                return if (200..300).contains(&resp.status()) {
                    Ok(resp)
                } else {
                    Err(format!("the server answered {}", resp.status()))
                };
            }
            let location = resp
                .header("location")
                .ok_or("a redirect without a location")?;
            let next = resolve(&current, &origin, location)?;
            let next_origin = parse_origin(&next, self.allow_loopback_http)?;
            if !redirect_allowed(&origin, &next_origin) {
                return Err(format!(
                    "refused a redirect from {} to {}",
                    origin.host, next_origin.host
                ));
            }
            current = next;
            origin = next_origin;
        }
        Err("too many redirects".into())
    }

    /// A small document (the manifest, its signature), read whole.
    pub fn get_small(&self, url: &str, max: u64) -> Result<Vec<u8>, String> {
        let resp = self.open(url)?;
        let announced = resp
            .header("content-length")
            .and_then(|v| v.parse::<u64>().ok());
        if announced.is_some_and(|n| n > max) {
            return Err(format!("it is larger than the {max}-byte limit"));
        }
        let mut sink = Vec::new();
        copy_capped(
            resp.into_reader(),
            &mut sink,
            max,
            Instant::now() + SMALL_DEADLINE,
            None,
            &mut |_| {},
        )?;
        Ok(sink)
    }

    /// Download an artifact into `out`, which must come out exactly `size` bytes. Returns the
    /// SHA-256 of what was written.
    pub fn download(
        &self,
        url: &str,
        size: u64,
        deadline: Duration,
        out: &mut dyn Write,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<[u8; 32], String> {
        let resp = self.open(url)?;
        if let Some(n) = resp
            .header("content-length")
            .and_then(|v| v.parse::<u64>().ok())
            && n != size
        {
            return Err(format!(
                "the server announced {n} bytes, the manifest says {size}"
            ));
        }
        let mut hasher = Sha256::new();
        let mut tee = Tee {
            out,
            hasher: &mut hasher,
        };
        let written = copy_capped(
            resp.into_reader(),
            &mut tee,
            size,
            Instant::now() + deadline,
            Some(size),
            &mut |n| progress(n, Some(size)),
        )?;
        if written != size {
            return Err(format!(
                "received {written} bytes, the manifest says {size}"
            ));
        }
        Ok(hasher.finalize().into())
    }
}

struct Tee<'a> {
    out: &'a mut dyn Write,
    hasher: &'a mut Sha256,
}

impl Write for Tee<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.out.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

/// Copy `reader` to `out`, failing past `max` bytes or `deadline`. Returns the bytes copied.
fn copy_capped(
    mut reader: impl Read,
    out: &mut dyn Write,
    max: u64,
    deadline: Instant,
    _total: Option<u64>,
    progress: &mut dyn FnMut(u64),
) -> Result<u64, String> {
    let mut buf = [0u8; 64 * 1024];
    let mut copied = 0u64;
    let mut last = Instant::now();
    loop {
        if Instant::now() > deadline {
            return Err("it took too long".into());
        }
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("the download failed: {e}"))?;
        if n == 0 {
            break;
        }
        copied += n as u64;
        if copied > max {
            return Err(format!("it is larger than the {max}-byte limit"));
        }
        out.write_all(&buf[..n])
            .map_err(|e| format!("couldn't save the download: {e}"))?;
        if last.elapsed() >= PROGRESS_EVERY {
            progress(copied);
            last = Instant::now();
        }
    }
    progress(copied);
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(url: &str) -> Origin {
        parse_origin(url, true).unwrap()
    }

    #[test]
    fn urls_are_parsed_strictly() {
        for bad in [
            "http://example.org/x",
            "ftp://example.org/x",
            "file:///etc/passwd",
            "https://user@example.org/x",
            "https://user:pw@example.org/x",
            "https:///x",
            "https://exa mple.org/x",
            "https://example.org:notaport/x",
            "https://example.org\\@evil/x",
            "https://[::1]/x",
            "https://ex\u{e9}.org/x",
            "//example.org/x",
            "",
            "http://example.org.evil.test/",
        ] {
            assert!(parse_origin(bad, false).is_err(), "{bad}");
        }
        assert!(parse_origin("http://127.0.0.1:40400/x", false).is_err());
        assert!(parse_origin("http://127.0.0.1:40400/x", true).is_ok());
        assert!(parse_origin("http://example.org/x", true).is_err());
        assert_eq!(
            parse_origin("https://Example.ORG/x", false).unwrap(),
            Origin {
                https: true,
                host: "example.org".into(),
                port: 443
            }
        );
    }

    #[test]
    fn redirect_rules() {
        let gh = o("https://github.com/freenet/river/releases/download/v1/river.tar.gz");
        assert!(redirect_allowed(
            &gh,
            &o("https://objects.githubusercontent.com/x")
        ));
        assert!(redirect_allowed(&gh, &o("https://github.com/other")));
        assert!(!redirect_allowed(&gh, &o("https://evil.example/x")));
        assert!(!redirect_allowed(
            &gh,
            &o("https://objects.githubusercontent.com.evil.example/x")
        ));
        assert!(!redirect_allowed(&gh, &o("http://127.0.0.1/x")));
        let site = o("https://freenet.org/river/manifest.json");
        assert!(redirect_allowed(
            &site,
            &o("https://freenet.org/river/other")
        ));
        // The GitHub allowance belongs to GitHub, not to anyone.
        assert!(!redirect_allowed(
            &site,
            &o("https://objects.githubusercontent.com/x")
        ));
        assert!(!redirect_allowed(&site, &o("https://freenet.org:8443/x")));
        assert!(!redirect_allowed(&site, &o("https://www.freenet.org/x")));
    }

    #[test]
    fn relative_redirects_stay_on_the_origin() {
        let base = o("https://freenet.org/a");
        assert_eq!(
            resolve("https://freenet.org/a", &base, "/b").unwrap(),
            "https://freenet.org:443/b"
        );
        assert!(resolve("https://freenet.org/a", &base, "//evil.example/b").is_err());
        assert!(resolve("https://freenet.org/a", &base, "b").is_err());
    }

    /// Tests that talk to a local server; the server is Linux-only test support.
    #[cfg(target_os = "linux")]
    mod served {
        use super::*;
        use crate::update::testserver::{Ports, Reply};

        #[test]
        fn fetches_a_small_document_and_enforces_the_cap() {
            let ports = Ports::lock();
            let server = ports.serve(vec![
                ("/ok", Reply::Body(b"hello".to_vec())),
                ("/big", Reply::Body(vec![b'x'; 2000])),
                ("/endless", Reply::Endless),
                ("/404", Reply::Status(404)),
            ]);
            let net = Net::new(true);
            assert_eq!(net.get_small(&server.url("/ok"), 100).unwrap(), b"hello");
            assert!(
                net.get_small(&server.url("/big"), 100)
                    .unwrap_err()
                    .contains("larger")
            );
            // No Content-Length: cut off at the cap.
            assert!(
                net.get_small(&server.url("/endless"), 1000)
                    .unwrap_err()
                    .contains("larger")
            );
            assert!(
                net.get_small(&server.url("/404"), 100)
                    .unwrap_err()
                    .contains("404")
            );
            assert!(Net::new(false).get_small(&server.url("/ok"), 100).is_err());
        }

        #[test]
        fn redirects_are_followed_on_one_origin_and_refused_elsewhere() {
            let ports = Ports::lock();
            let other = ports.serve(vec![("/payload", Reply::Body(b"elsewhere".to_vec()))]);
            let server = ports.serve(vec![
                ("/real", Reply::Body(b"real".to_vec())),
                ("/hop", Reply::Redirect("/real".into())),
                ("/away", Reply::Redirect(other.url("/payload"))),
                (
                    "/away-host",
                    Reply::Redirect(other.url_via_localhost("/payload")),
                ),
                ("/loop", Reply::Redirect("/loop".into())),
                (
                    "/downgrade-ish",
                    Reply::Redirect("ftp://127.0.0.1/x".into()),
                ),
            ]);
            let net = Net::new(true);
            assert_eq!(net.get_small(&server.url("/hop"), 100).unwrap(), b"real");
            for path in ["/away", "/away-host"] {
                let err = net.get_small(&server.url(path), 100).unwrap_err();
                assert!(err.contains("refused a redirect"), "{path}: {err}");
            }
            assert!(
                net.get_small(&server.url("/loop"), 100)
                    .unwrap_err()
                    .contains("too many")
            );
            assert!(net.get_small(&server.url("/downgrade-ish"), 100).is_err());
            // The other server was never asked.
            assert_eq!(other.hits("/payload"), 0);
        }

        #[test]
        fn a_download_must_match_the_announced_size_exactly() {
            let ports = Ports::lock();
            let body = vec![7u8; 5000];
            let server = ports.serve(vec![
                ("/a", Reply::Body(body.clone())),
                ("/endless", Reply::Endless),
            ]);
            let net = Net::new(true);
            let mut out = Vec::new();
            let sum = net
                .download(
                    &server.url("/a"),
                    5000,
                    Duration::from_secs(10),
                    &mut out,
                    &mut |_, _| {},
                )
                .unwrap();
            assert_eq!(out, body);
            assert_eq!(sum, <[u8; 32]>::from(Sha256::digest(&body)));
            for (size, what) in [(4999u64, "announced"), (5001, "announced")] {
                let mut out = Vec::new();
                let err = net
                    .download(
                        &server.url("/a"),
                        size,
                        Duration::from_secs(10),
                        &mut out,
                        &mut |_, _| {},
                    )
                    .unwrap_err();
                assert!(err.contains(what), "{size}: {err}");
            }
            let mut out = Vec::new();
            let err = net
                .download(
                    &server.url("/endless"),
                    4000,
                    Duration::from_secs(10),
                    &mut out,
                    &mut |_, _| {},
                )
                .unwrap_err();
            assert!(err.contains("larger"), "{err}");
            assert!(out.len() <= 4000);
        }
    }
}
