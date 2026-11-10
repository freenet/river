//! Asking the node for River's app identity: freenet-core's native app grants.
//!
//! The web UI's rooms are stored in the chat delegate under the web app's identity, which the
//! node gives a browser tab that loaded River from the node. A native app has no such tab, so it
//! asks the node for that identity and the user approves it in the browser. The protocol is in
//! freenet-core's `crates/core/src/server/client_api/app_grants.rs` (module docs):
//!
//! 1. `POST /v1/app-grants/request {"app", "client_name"}` answers
//!    `{request_id, approve_url, expires_in_secs}`.
//! 2. The app opens `approve_url` in the browser and long-polls
//!    `GET /v1/app-grants/request/<request_id>`; each poll waits up to 30 seconds.
//! 3. `202 pending`: poll again. `200 approved` carries the token. `200 denied`: stop. `409
//!    grant_limit` and `503`: keep polling (the node paces these). `404`: the request expired or
//!    is unknown; start over.
//!
//! The requests carry no `Origin` and no `Sec-Fetch-*` header, which is what the node requires
//! of a native client. The HTTP itself is behind [`GrantHttp`] so the state machine is tested
//! against a scripted responder.

use std::thread;
use std::time::{Duration, Instant};

/// The web container contract id of the River web app whose identity River asks for.
pub const RIVER_WEB_APP: &str = "raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv";

/// The name the approval page shows (labelled there as unverified).
pub const CLIENT_NAME: &str = "River desktop app";

/// The first freenet-core release with app grants. A node that answers the grant routes with
/// 404 or 405 ([`GrantOutcome::Unsupported`]) is older than this.
pub const GRANT_NODE_VERSION: &str = "0.2.156";

const ROUTE_REQUEST: &str = "/v1/app-grants/request";

/// How long to wait before polling again after a refusal the node answered at once. The node
/// already holds a poll for up to 30 seconds when it can; this only stops a hot loop when it
/// answers immediately.
const REPOLL_PAUSE: Duration = Duration::from_secs(1);

/// How many times a request that the node could not take right now (`429`, `503`) is retried.
const REQUEST_RETRIES: u32 = 5;

/// A grant token. It is a credential: it opens River's delegate storage, including room
/// signing keys. It is never printed, so `Debug` hides it and there is no `Display`.
#[derive(Clone, PartialEq, Eq)]
pub struct GrantToken(String);

impl GrantToken {
    pub fn new(token: String) -> Self {
        GrantToken(token)
    }

    /// The token's text, for the `Authorization` header and the token file only.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for GrantToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GrantToken(<redacted>)")
    }
}

/// One HTTP answer: its status and body.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpReply {
    pub status: u16,
    pub body: String,
}

/// The body can carry a grant token, so `Debug` shows only the status.
impl std::fmt::Debug for HttpReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpReply")
            .field("status", &self.status)
            .field("body", &format_args!("<{} bytes>", self.body.len()))
            .finish()
    }
}

/// The two HTTP calls the grant flow makes. `Err` is a transport failure (no node answered);
/// any status, success or not, is `Ok`.
pub trait GrantHttp: Send + Sync {
    fn post_json(&self, path: &str, body: &str) -> Result<HttpReply, String>;
    fn get(&self, path: &str) -> Result<HttpReply, String>;
}

/// How a grant request ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GrantOutcome {
    Approved(GrantToken),
    /// The user chose Deny.
    Denied,
    /// The request expired before it was answered (or the node forgot it).
    Expired,
    /// The node has no grant routes: it is older than [`GRANT_NODE_VERSION`].
    Unsupported,
    /// The node refused for a reason the user has to act on (the message says what).
    Refused(String),
    /// No node answered.
    Unreachable(String),
}

/// Ask for a grant and wait for the user's answer. `waiting` is called once, with the page the
/// user approves on, when the node has accepted the request; it opens that page. `authority`
/// is the node's `host:port`: the page must be on it.
pub fn request_grant(
    http: &dyn GrantHttp,
    authority: &str,
    waiting: &mut dyn FnMut(&str),
) -> GrantOutcome {
    request_grant_with(http, authority, waiting, &GrantPacing::default())
}

/// [`request_grant`] with its own pacing.
pub fn request_grant_with(
    http: &dyn GrantHttp,
    authority: &str,
    waiting: &mut dyn FnMut(&str),
    pacing: &GrantPacing,
) -> GrantOutcome {
    request_grant_paced(
        http,
        authority,
        waiting,
        pacing,
        &mut |d| thread::sleep(d),
        &Instant::now,
    )
}

/// How the grant flow paces itself. Tests shrink these.
#[derive(Clone, Debug)]
pub struct GrantPacing {
    /// The pause before polling again (after an answer the node gave at once) or retrying.
    pub pause: Duration,
    /// How long past the request's own expiry River keeps polling.
    pub margin: Duration,
    /// How long polls may fail to reach the node before River reports it unreachable.
    pub outage: Duration,
}

impl Default for GrantPacing {
    fn default() -> Self {
        GrantPacing {
            pause: REPOLL_PAUSE,
            margin: Duration::from_secs(30),
            outage: Duration::from_secs(30),
        }
    }
}

fn request_grant_paced(
    http: &dyn GrantHttp,
    authority: &str,
    waiting: &mut dyn FnMut(&str),
    pacing: &GrantPacing,
    sleep: &mut dyn FnMut(Duration),
    now: &dyn Fn() -> Instant,
) -> GrantOutcome {
    let body = serde_json::json!({ "app": RIVER_WEB_APP, "client_name": CLIENT_NAME }).to_string();
    let mut tries = 0;
    let created = loop {
        let reply = match http.post_json(ROUTE_REQUEST, &body) {
            Ok(r) => r,
            Err(e) => return GrantOutcome::Unreachable(e),
        };
        match reply.status {
            200 => break reply,
            404 | 405 => return GrantOutcome::Unsupported,
            409 => return GrantOutcome::Refused(grant_limit_message(&reply.body)),
            // "node not ready" (it answers HTTP before it can take grants) or too many
            // requests: nothing has opened yet, so this is safe to retry, here and then by
            // the caller with backoff.
            429 | 503 if tries < REQUEST_RETRIES => {
                tries += 1;
                sleep(pacing.pause);
            }
            429 | 503 => {
                return GrantOutcome::Unreachable("Freenet isn't ready for River yet".into());
            }
            status => return GrantOutcome::Refused(refusal(status, &reply.body)),
        }
    };
    let Some(created) = parse_created(&created.body, authority) else {
        return GrantOutcome::Refused("Freenet sent an answer River doesn't understand.".into());
    };
    // The request is answerable for `expires_in_secs`; past that (and a margin for a slow
    // answer) the node has forgotten it, whatever it says.
    let deadline = now() + created.expires_in + pacing.margin;
    waiting(&created.approve_url);
    let poll = format!("{ROUTE_REQUEST}/{}", created.request_id);
    // Since when polls have been failing to reach the node, and the last failure.
    let mut failing: Option<(Instant, String)> = None;
    // Polls failed, and the node has not yet shown it still holds the request (a 202). A
    // restarting node can answer 409, 429 or 503 before it forgets the request, so only a
    // 202 clears this.
    let mut may_have_restarted = false;
    loop {
        if now() >= deadline {
            // A node that was down when the request ran out is reported as that.
            return match failing {
                Some((_, e)) => GrantOutcome::Unreachable(e),
                None => GrantOutcome::Expired,
            };
        }
        let reply = match http.get(&poll) {
            Ok(r) => {
                failing = None;
                r
            }
            // The node may be restarting. The request id still names the same request, so
            // keep polling it for a short outage rather than start over (which would open
            // another page). A longer one is reported as the node being gone, not left on
            // the approval screen until the request expires.
            Err(e) => {
                let since = failing.as_ref().map_or_else(now, |(at, _)| *at);
                if now().saturating_duration_since(since) >= pacing.outage {
                    return GrantOutcome::Unreachable(e);
                }
                failing = Some((since, e));
                may_have_restarted = true;
                sleep(pacing.pause);
                continue;
            }
        };
        match (reply.status, status_field(&reply.body).as_deref()) {
            (200, Some("approved")) => {
                return match token_field(&reply.body) {
                    Some(t) => GrantOutcome::Approved(GrantToken::new(t)),
                    None => GrantOutcome::Refused(
                        "Freenet approved River but sent no token. Try again.".into(),
                    ),
                };
            }
            (200, Some("denied")) => return GrantOutcome::Denied,
            // Still pending (the node normally holds this answer for up to 30 seconds), at the
            // grant limit, the grant not stored yet, or too many polls at once across every
            // client of the node: the request is kept, so poll again after a pause.
            (202, _) => {
                may_have_restarted = false;
                sleep(pacing.pause);
            }
            (409, _) | (429, _) | (503, _) => sleep(pacing.pause),
            // After the node was unreachable, and before it showed it still holds the request,
            // "unknown request" means it restarted and forgot it: the node's doing, not an
            // unanswered page. A 404 once it has answered 202 is the request expiring as usual.
            (404, _) if may_have_restarted => {
                return GrantOutcome::Unreachable("Freenet restarted".into());
            }
            (404, _) => return GrantOutcome::Expired,
            (status, _) => return GrantOutcome::Refused(refusal(status, &reply.body)),
        }
    }
}

fn json(body: &str) -> Option<serde_json::Value> {
    serde_json::from_str(body).ok()
}

struct Created {
    request_id: String,
    approve_url: String,
    expires_in: Duration,
}

fn parse_created(body: &str, authority: &str) -> Option<Created> {
    let v = json(body)?;
    let id = v.get("request_id")?.as_str()?;
    let url = v.get("approve_url")?.as_str()?;
    // The id goes into a URL path; refuse anything that is not a plain token.
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    // Only ever open the node's own page: the URL's authority must be exactly the one River
    // is talking to (no userinfo, no other host or port).
    let rest = url.strip_prefix("http://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    if &rest[..end] != authority {
        return None;
    }
    let expires_in = v
        .get("expires_in_secs")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(600)
        .min(24 * 60 * 60);
    Some(Created {
        request_id: id.to_owned(),
        approve_url: url.to_owned(),
        expires_in: Duration::from_secs(expires_in),
    })
}

fn status_field(body: &str) -> Option<String> {
    Some(json(body)?.get("status")?.as_str()?.to_owned())
}

fn token_field(body: &str) -> Option<String> {
    let t = json(body)?.get("token")?.as_str()?.to_owned();
    (!t.is_empty()).then_some(t)
}

fn grant_limit_message(body: &str) -> String {
    json(body)
        .and_then(|v| v.get("message")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| {
            "Freenet already allows as many programs as it can. Revoke one on Freenet's Apps \
             and permissions page, then try again."
                .into()
        })
}

fn refusal(status: u16, body: &str) -> String {
    let reason = json(body).and_then(|v| v.get("error")?.as_str().map(str::to_owned));
    match reason {
        Some(r) => format!("Freenet refused River's request ({status}: {r})."),
        None => format!("Freenet refused River's request ({status})."),
    }
}

/// [`GrantHttp`] over blocking HTTP to the local node.
pub struct NodeHttp {
    base: String,
    agent: ureq::Agent,
}

impl NodeHttp {
    pub fn new(base: &str) -> Self {
        NodeHttp {
            base: base.trim_end_matches('/').to_owned(),
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(5))
                // A poll is held for up to 30 seconds.
                .timeout_read(Duration::from_secs(45))
                .redirects(0)
                .build(),
        }
    }

    fn reply(result: Result<ureq::Response, ureq::Error>) -> Result<HttpReply, String> {
        let response = match result {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            // Not `t.to_string()`: that starts with the URL, and the poll URL carries the
            // request id, which can collect the token. The kind says enough.
            Err(ureq::Error::Transport(t)) => {
                return Err(format!("Freenet didn't answer ({})", t.kind()));
            }
        };
        let status = response.status();
        // Grant answers are small; cap what is read.
        let mut body = String::new();
        use std::io::Read;
        response
            .into_reader()
            .take(64 * 1024)
            .read_to_string(&mut body)
            .map_err(|e| e.to_string())?;
        Ok(HttpReply { status, body })
    }
}

impl GrantHttp for NodeHttp {
    fn post_json(&self, path: &str, body: &str) -> Result<HttpReply, String> {
        Self::reply(
            self.agent
                .post(&format!("{}{path}", self.base))
                .set("Content-Type", "application/json")
                .send_string(body),
        )
    }

    fn get(&self, path: &str) -> Result<HttpReply, String> {
        Self::reply(self.agent.get(&format!("{}{path}", self.base)).call())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// A node whose answers come from a script, recording every call.
    struct Scripted {
        replies: Mutex<VecDeque<Result<HttpReply, String>>>,
        calls: Mutex<Vec<(String, String, Option<String>)>>,
    }

    impl Scripted {
        fn new(replies: Vec<Result<(u16, &str), &str>>) -> Self {
            Scripted {
                replies: Mutex::new(
                    replies
                        .into_iter()
                        .map(|r| {
                            r.map(|(status, body)| HttpReply {
                                status,
                                body: body.to_owned(),
                            })
                            .map_err(str::to_owned)
                        })
                        .collect(),
                ),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn next(&self) -> Result<HttpReply, String> {
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("the flow made a call the script does not expect")
        }

        fn calls(&self) -> Vec<(String, String, Option<String>)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl GrantHttp for Scripted {
        fn post_json(&self, path: &str, body: &str) -> Result<HttpReply, String> {
            self.calls
                .lock()
                .unwrap()
                .push(("POST".into(), path.into(), Some(body.into())));
            self.next()
        }
        fn get(&self, path: &str) -> Result<HttpReply, String> {
            self.calls
                .lock()
                .unwrap()
                .push(("GET".into(), path.into(), None));
            self.next()
        }
    }

    const AUTHORITY: &str = "127.0.0.1:7509";

    std::thread_local! {
        /// How many pauses the last `run` on this thread made.
        static PAUSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn pauses() -> usize {
        PAUSES.with(|p| p.get())
    }

    const CREATED: &str = r#"{"request_id":"req42","approve_url":"http://127.0.0.1:7509/v1/app-grants/approve/ap1","expires_in_secs":600}"#;
    const PENDING: &str = r#"{"status":"pending"}"#;

    fn run(script: Vec<Result<(u16, &str), &str>>) -> (GrantOutcome, Vec<String>, Scripted) {
        let http = Scripted::new(script);
        let mut opened = Vec::new();
        let mut pauses = 0;
        let start = Instant::now();
        let outcome = request_grant_paced(
            &http,
            AUTHORITY,
            &mut |url| opened.push(url.to_owned()),
            &GrantPacing {
                pause: Duration::ZERO,
                margin: Duration::ZERO,
                outage: Duration::from_secs(3600),
            },
            &mut |_| pauses += 1,
            &|| start,
        );
        PAUSES.with(|p| p.set(pauses));
        assert!(
            http.replies.lock().unwrap().is_empty(),
            "the flow stopped before using the whole script"
        );
        (outcome, opened, http)
    }

    #[test]
    fn approval_after_pending_polls_returns_the_token() {
        let (outcome, opened, http) = run(vec![
            Ok((200, CREATED)),
            Ok((202, PENDING)),
            Ok((202, PENDING)),
            Ok((
                200,
                r#"{"status":"approved","app":"x","token":"tok-secret"}"#,
            )),
        ]);
        assert_eq!(
            outcome,
            GrantOutcome::Approved(GrantToken::new("tok-secret".into()))
        );
        assert_eq!(opened, ["http://127.0.0.1:7509/v1/app-grants/approve/ap1"]);
        let calls = http.calls();
        assert_eq!(calls[0].0, "POST");
        assert_eq!(calls[0].1, "/v1/app-grants/request");
        let sent: serde_json::Value = serde_json::from_str(calls[0].2.as_ref().unwrap()).unwrap();
        assert_eq!(sent["app"], RIVER_WEB_APP);
        assert_eq!(sent["client_name"], CLIENT_NAME);
        for call in &calls[1..] {
            assert_eq!(
                (call.0.as_str(), call.1.as_str()),
                ("GET", "/v1/app-grants/request/req42")
            );
        }
    }

    #[test]
    fn denial_stops() {
        let (outcome, opened, _) = run(vec![
            Ok((200, CREATED)),
            Ok((200, r#"{"status":"denied"}"#)),
        ]);
        assert_eq!(outcome, GrantOutcome::Denied);
        assert_eq!(opened.len(), 1);
    }

    #[test]
    fn grant_limit_and_storage_refusals_keep_polling() {
        let (outcome, _, _) = run(vec![
            Ok((200, CREATED)),
            Ok((
                409,
                r#"{"status":"grant_limit","error":"grant_limit","message":"m"}"#,
            )),
            Ok((503, r#"{"error":"could not store the grant; poll again"}"#)),
            Ok((200, r#"{"status":"approved","token":"t"}"#)),
        ]);
        assert_eq!(outcome, GrantOutcome::Approved(GrantToken::new("t".into())));
    }

    #[test]
    fn an_expired_request_reports_expired() {
        let (outcome, _, _) = run(vec![
            Ok((200, CREATED)),
            Ok((202, PENDING)),
            Ok((404, r#"{"error":"unknown or expired request"}"#)),
        ]);
        assert_eq!(outcome, GrantOutcome::Expired);
    }

    #[test]
    fn a_node_without_grant_routes_is_unsupported_and_nothing_opens() {
        let (outcome, opened, _) = run(vec![Ok((404, "Not Found"))]);
        assert_eq!(outcome, GrantOutcome::Unsupported);
        assert!(opened.is_empty());
    }

    #[test]
    fn a_full_node_says_why_before_opening_anything() {
        let (outcome, opened, _) = run(vec![Ok((
            409,
            r#"{"status":"grant_limit","message":"This node already has 64 programs allowed."}"#,
        ))]);
        assert_eq!(
            outcome,
            GrantOutcome::Refused("This node already has 64 programs allowed.".into())
        );
        assert!(opened.is_empty());
    }

    #[test]
    fn a_busy_node_is_retried_at_request_time() {
        let (outcome, opened, _) = run(vec![
            Ok((503, r#"{"error":"node not ready"}"#)),
            Ok((200, CREATED)),
            Ok((200, r#"{"status":"denied"}"#)),
        ]);
        assert_eq!(outcome, GrantOutcome::Denied);
        assert_eq!(opened.len(), 1);
    }

    #[test]
    fn transport_failures_are_unreachable() {
        let (outcome, _, _) = run(vec![Err("connection refused")]);
        assert_eq!(
            outcome,
            GrantOutcome::Unreachable("connection refused".into())
        );
    }

    #[test]
    fn an_approval_without_a_token_is_refused() {
        let (outcome, _, _) = run(vec![
            Ok((200, CREATED)),
            Ok((200, r#"{"status":"approved","token":""}"#)),
        ]);
        assert!(matches!(outcome, GrantOutcome::Refused(_)), "{outcome:?}");
    }

    #[test]
    fn only_the_nodes_own_page_is_opened() {
        for created in [
            r#"{"request_id":"r","approve_url":"https://evil.example/approve"}"#,
            r#"{"request_id":"../x","approve_url":"http://127.0.0.1:7509/a"}"#,
            r#"{"approve_url":"http://127.0.0.1:7509/a"}"#,
            // Userinfo makes the host evil.example.
            r#"{"request_id":"r","approve_url":"http://127.0.0.1:7509@evil.example/a"}"#,
            r#"{"request_id":"r","approve_url":"http://127.0.0.1:1@evil.example/"}"#,
            // Another port on loopback is another program.
            r#"{"request_id":"r","approve_url":"http://127.0.0.1:7510/a"}"#,
            r#"{"request_id":"r","approve_url":"http://localhost:7509/a"}"#,
            r#"{"request_id":"r","approve_url":"http://127.0.0.1:75091/a"}"#,
            "not json",
        ] {
            let (outcome, opened, _) = run(vec![Ok((200, created))]);
            assert!(matches!(outcome, GrantOutcome::Refused(_)), "{created}");
            assert!(opened.is_empty(), "{created}");
        }
    }

    #[test]
    fn other_statuses_are_refusals_that_name_the_reason() {
        let (outcome, _, _) = run(vec![Ok((403, r#"{"error":"loopback only"}"#))]);
        assert_eq!(
            outcome,
            GrantOutcome::Refused("Freenet refused River's request (403: loopback only).".into())
        );
    }

    #[test]
    fn the_token_never_shows_in_debug_output() {
        let t = GrantToken::new("very-secret".into());
        assert!(!format!("{t:?}").contains("very-secret"));
        assert!(!format!("{:?}", GrantOutcome::Approved(t)).contains("very-secret"));
    }

    /// The real transport against a tiny HTTP server: no Origin or Sec-Fetch headers, JSON
    /// content type on the POST, and non-2xx statuses come back as replies, not errors.
    #[test]
    fn node_http_sends_what_the_node_requires() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let mut seen = Vec::new();
            for (status, body) in [(200, CREATED), (404, r#"{"error":"gone"}"#)] {
                let (mut conn, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(conn.try_clone().unwrap());
                let mut head = Vec::new();
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                    head.push(line);
                }
                let mut body_in = vec![0; len];
                std::io::Read::read_exact(&mut reader, &mut body_in).unwrap();
                seen.push(head);
                write!(
                    conn,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
            seen
        });
        let http = NodeHttp::new(&format!("http://127.0.0.1:{port}"));
        let created = http.post_json("/v1/app-grants/request", "{}").unwrap();
        assert_eq!(created.status, 200);
        assert_eq!(created.body, CREATED);
        let gone = http.get("/v1/app-grants/request/req42").unwrap();
        assert_eq!(gone.status, 404);
        let seen = server.join().unwrap();
        let lower = |h: &Vec<String>| h.iter().map(|l| l.to_ascii_lowercase()).collect::<Vec<_>>();
        let post = lower(&seen[0]);
        assert!(
            post[0].starts_with("post /v1/app-grants/request "),
            "{post:?}"
        );
        assert!(
            post.iter()
                .any(|l| l.starts_with("content-type: application/json"))
        );
        assert!(lower(&seen[1])[0].starts_with("get /v1/app-grants/request/req42 "));
        for head in &seen {
            for line in lower(head) {
                assert!(!line.starts_with("origin:"), "{line}");
                assert!(!line.starts_with("sec-fetch-"), "{line}");
            }
        }
    }

    #[test]
    fn every_answer_that_means_poll_again_pauses_first() {
        let (outcome, _, _) = run(vec![
            Ok((200, CREATED)),
            Ok((202, PENDING)),
            Ok((409, r#"{"status":"grant_limit"}"#)),
            Ok((429, r#"{"error":"too many polls"}"#)),
            Ok((503, r#"{"error":"x"}"#)),
            Ok((200, r#"{"status":"denied"}"#)),
        ]);
        assert_eq!(outcome, GrantOutcome::Denied);
        assert_eq!(pauses(), 4);
    }

    #[test]
    fn a_poll_that_cannot_reach_the_node_retries_the_same_request() {
        let (outcome, opened, http) = run(vec![
            Ok((200, CREATED)),
            Err("Freenet didn't answer (Network Error)"),
            Err("Freenet didn't answer (Network Error)"),
            Ok((200, r#"{"status":"approved","token":"t"}"#)),
        ]);
        assert_eq!(outcome, GrantOutcome::Approved(GrantToken::new("t".into())));
        // One page, one request, polled three times.
        assert_eq!(opened.len(), 1);
        let calls = http.calls();
        assert_eq!(calls.iter().filter(|c| c.0 == "POST").count(), 1);
        assert!(
            calls[1..]
                .iter()
                .all(|c| c.1 == "/v1/app-grants/request/req42")
        );
        assert_eq!(pauses(), 2);

        // A node that stays gone past the outage limit (60 s here, polls every 30 s) is
        // reported as gone, not left on the approval screen until the request expires.
        let (outcome, gets) = run_clocked(Err("gone"));
        assert_eq!(outcome, GrantOutcome::Unreachable("gone".into()));
        assert_eq!(gets, 3);
    }

    /// A restarting node can answer "not ready" before it forgets the request: still a restart.
    #[test]
    fn an_unknown_request_after_an_outage_and_a_not_ready_is_a_restart() {
        let (outcome, _, _) = run(vec![
            Ok((200, CREATED)),
            Err("reset"),
            Ok((503, r#"{"error":"node not ready"}"#)),
            Ok((404, r#"{"error":"unknown or expired request"}"#)),
        ]);
        assert!(
            matches!(outcome, GrantOutcome::Unreachable(_)),
            "{outcome:?}"
        );
    }

    /// A blip that the node answered through, and then a natural expiry: that is an expiry.
    #[test]
    fn an_expiry_after_an_earlier_blip_is_an_expiry() {
        let (outcome, _, _) = run(vec![
            Ok((200, CREATED)),
            Err("reset"),
            Ok((202, PENDING)),
            Ok((404, r#"{"error":"unknown or expired request"}"#)),
        ]);
        assert_eq!(outcome, GrantOutcome::Expired);
    }

    /// The node went down shortly before the request ran out: that is the node being gone.
    #[test]
    fn a_node_down_when_the_request_runs_out_is_unreachable() {
        // 600 s request, 30 s margin, polls every 30 s, outage limit 60 s: answers until 570 s,
        // fails at 600 s, and the deadline (630 s) comes before the outage limit.
        let mut script = vec![Ok((200, CREATED))];
        script.extend(std::iter::repeat_n(Ok((202, PENDING)), 20));
        script.extend(std::iter::repeat_n(Err("gone"), 5));
        let http = Scripted::new(script);
        let clock = std::cell::Cell::new(Instant::now());
        let outcome = request_grant_paced(
            &http,
            AUTHORITY,
            &mut |_| {},
            &GrantPacing {
                pause: Duration::from_secs(30),
                margin: Duration::from_secs(30),
                outage: Duration::from_secs(60),
            },
            &mut |d| clock.set(clock.get() + d),
            &|| clock.get(),
        );
        assert_eq!(outcome, GrantOutcome::Unreachable("gone".into()));
    }

    /// A node that was unreachable and then forgets the request has restarted: that is not an
    /// unanswered page.
    #[test]
    fn an_unknown_request_after_an_outage_is_a_restart_not_an_expiry() {
        let (outcome, _, _) = run(vec![
            Ok((200, CREATED)),
            Err("reset"),
            Ok((404, r#"{"error":"unknown or expired request"}"#)),
        ]);
        assert!(
            matches!(outcome, GrantOutcome::Unreachable(_)),
            "{outcome:?}"
        );
    }

    #[test]
    fn polling_stops_when_the_request_has_expired() {
        let created =
            r#"{"request_id":"req42","approve_url":"http://127.0.0.1:7509/a","expires_in_secs":0}"#;
        let (outcome, opened, _) = run(vec![Ok((200, created))]);
        assert_eq!(outcome, GrantOutcome::Expired);
        assert_eq!(opened.len(), 1);
    }

    #[test]
    fn a_reply_never_shows_its_body_in_debug_output() {
        let r = HttpReply {
            status: 200,
            body: r#"{"token":"very-secret"}"#.into(),
        };
        assert!(!format!("{r:?}").contains("very-secret"));
    }

    /// A transport error names no URL: the poll URL carries the request id.
    #[test]
    fn transport_errors_do_not_name_the_url() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let http = NodeHttp::new(&format!("http://127.0.0.1:{port}"));
        let err = http
            .get("/v1/app-grants/request/secret-request-id")
            .unwrap_err();
        assert!(!err.contains("secret-request-id"), "{err}");
        assert!(!err.contains("127.0.0.1"), "{err}");
    }

    /// Run against a node that answers every poll with `reply`, on a clock that only moves
    /// when the flow pauses (30 s a pause, a 600 s request and a 30 s margin). Returns the
    /// outcome and how many polls were made.
    fn run_clocked(reply: Result<(u16, &str), &str>) -> (GrantOutcome, usize) {
        let mut script = vec![Ok((200, CREATED))];
        script.extend(std::iter::repeat_n(reply, 100));
        let http = Scripted::new(script);
        let clock = std::cell::Cell::new(Instant::now());
        let outcome = request_grant_paced(
            &http,
            AUTHORITY,
            &mut |_| {},
            &GrantPacing {
                pause: Duration::from_secs(30),
                margin: Duration::from_secs(30),
                outage: Duration::from_secs(60),
            },
            &mut |d| clock.set(clock.get() + d),
            &|| clock.get(),
        );
        let gets = http.calls().iter().filter(|c| c.0 == "GET").count();
        (outcome, gets)
    }

    /// Polls run at 0, 30, ..., 600 s; at 630 s (expiry plus margin) River stops.
    #[test]
    fn polling_stops_at_the_requests_expiry_plus_the_margin() {
        let (outcome, gets) = run_clocked(Ok((202, PENDING)));
        assert_eq!(outcome, GrantOutcome::Expired);
        assert_eq!(gets, 21);
    }

    /// A node that is starting ("node not ready") is not a refusal: nothing opened yet, and
    /// the caller retries with backoff.
    #[test]
    fn a_node_not_ready_for_grants_is_retried_not_refused() {
        let mut script =
            vec![Ok((503, r#"{"error":"node not ready"}"#)); REQUEST_RETRIES as usize + 1];
        let (outcome, opened, _) = run(std::mem::take(&mut script));
        assert!(
            matches!(outcome, GrantOutcome::Unreachable(_)),
            "{outcome:?}"
        );
        assert!(opened.is_empty());
        let (outcome, _, _) = run(vec![
            Ok((429, r#"{"error":"too many"}"#));
            REQUEST_RETRIES as usize + 1
        ]);
        assert!(
            matches!(outcome, GrantOutcome::Unreachable(_)),
            "{outcome:?}"
        );
    }
}
