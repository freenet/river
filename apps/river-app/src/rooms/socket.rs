//! A WebSocket connection to the node's client API, in its native encoding: each
//! message is the bincode of a freenet-stdlib `ClientRequest` one way and of a
//! `Result<HostResponse, ClientError>` the other, at the stdlib version River's web UI uses.
//!
//! **The socket has no receive or send timeout.** Microsoft documents that a Winsock receive
//! that times out (`SO_RCVTIMEO`, which is what `TcpStream::set_read_timeout` sets) leaves the
//! connection "in an indeterminate state" that "should be closed". A read timeout that races
//! with arriving data can lose that data, and on Windows it can end with os error 997 instead
//! of a timeout. So the stream is non-blocking: a read that finds nothing is a plain
//! `WouldBlock` on every platform, and [`NodeSocket::recv`] then waits for the socket to become
//! readable with the `polling` crate (epoll, kqueue, or AFD poll on Windows) for no longer than
//! its own deadline, and reads again. The wait is a separate readiness query, not a read, so a
//! deadline that passes cancels nothing and cannot take bytes: after a wait that timed out the
//! caller reads once more before it reports `Timeout`. If the poller cannot be made or used,
//! the wait is a sleep of at most [`POLL`] between reads instead, and that is said once on
//! stderr.
//!
//! A large response may arrive in `StreamChunk`s (announced by a `StreamHeader` or not); they
//! are put back together here, so the caller only ever sees whole responses.
//!
//! When the node rejects the grant token, it first sends an error whose text starts with
//! `AUTH_TOKEN_INVALID`, `AUTH_TOKEN_NOT_USABLE_HERE` or `AUTH_TOKEN_UNAVAILABLE`, and then
//! closes with 4401, 4403 or 4503 (freenet-core `client_events/websocket.rs`). Either one is
//! reported as the matching [`SocketError::Closed`], so the caller decides on the token the
//! same way whichever arrives first.
//!
//! **Registering the chat delegate happens only here, and only on a grant connection.** The
//! node records the app a delegate was FIRST registered under, permanently (freenet-core
//! `register_delegate_and_record_origin`). A connection with no token is local-scoped with no
//! app identity, so a registration sent on one would record no origin for River's delegate on
//! that node, for good. Every [`NodeSocket`] is opened by [`NodeSocket::connect`], which
//! presents a grant token as a bearer token; the node rejects and closes a connection whose
//! token it does not accept before it handles any request on it (freenet-core
//! `websocket_commands`). [`NodeSocket::register_chat_delegate`] is the only way to send a
//! `RegisterDelegate`, and [`NodeSocket::send`] refuses one, so no other path can send it.
//!
//! **Every write passes through here, and stops here once River has moved on.** Each
//! [`NodeSocket`] holds the link's [`WriteGate`] from the moment it opens. Once a verified
//! pointer record names a newer chat delegate or room contract than this build carries
//! ([`super::generation`]), the gate is closed and [`NodeSocket::send`] refuses everything
//! [`is_read`] does not name a read, and [`NodeSocket::register_chat_delegate`] refuses too. The
//! session stops asking for writes on its own; this is what holds if a path ever does not.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use freenet_stdlib::client_api::{
    ClientError, ClientRequest, DelegateRequest, ErrorKind as NodeErrorKind, HostResponse,
};
use tungstenite::client::IntoClientRequest;
use tungstenite::{Message, WebSocket};

use super::generation::WriteGate;
use super::grant::GrantToken;

/// The node's client API, in native encoding.
pub const COMMAND_PATH: &str = "/v1/contract/command?encodingProtocol=native";

/// The most chunks one response may arrive in (freenet-stdlib's `MAX_TOTAL_CHUNKS`).
const MAX_CHUNKS: u32 = 256;

/// The most responses reassembled at once, and how long an unfinished one is kept (the node's
/// own limits for requests it reassembles: 8 streams, 60 seconds).
const MAX_STREAMS: usize = 8;
const STREAM_TTL: Duration = Duration::from_secs(60);

/// How long a write may wait for room in the socket before the connection counts as gone.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the WebSocket upgrade may take.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest [`NodeSocket::recv`] sleeps between reads when the readiness poller is not
/// available (see the module docs): the most delay that fallback adds to a frame, and 50
/// wakeups a second while the connection is idle. Sleeps are cut short at the deadline.
const POLL: Duration = Duration::from_millis(20);

/// How long a write that found the socket full sleeps before it tries again.
const WRITE_POLL: Duration = Duration::from_millis(2);

/// The largest response River puts back together from chunks. A room's state is bounded far
/// below this by the contract; the bound stops a misbehaving node from filling memory.
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// The node's token rejections, as the error text it sends before the close frame starts,
/// with the close code that follows.
const TOKEN_REJECTIONS: [(&str, u16); 3] = [
    ("AUTH_TOKEN_INVALID", 4401),
    ("AUTH_TOKEN_NOT_USABLE_HERE", 4403),
    ("AUTH_TOKEN_UNAVAILABLE", 4503),
];

pub type HostResult = Result<HostResponse, ClientError>;

/// Why the connection could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SocketError {
    /// The node closed the connection with a code (`4401`, `4403`, `4503`, ...) and reason, or
    /// sent the token rejection that precedes such a close.
    Closed { code: Option<u16>, reason: String },
    /// The node answered the WebSocket upgrade with this HTTP status instead.
    Handshake(u16),
    /// Nothing arrived within the read wait. The connection is still usable.
    Timeout,
    /// Anything else: the node went away, or sent something River can't read.
    Failed(String),
}

impl std::fmt::Display for SocketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SocketError::Closed {
                code: Some(c),
                reason,
            } if !reason.is_empty() => {
                write!(f, "Freenet closed the connection ({c}: {reason})")
            }
            SocketError::Closed { code: Some(c), .. } => {
                write!(f, "Freenet closed the connection ({c})")
            }
            SocketError::Closed { code: None, .. } => f.write_str("Freenet closed the connection"),
            SocketError::Handshake(s) => write!(f, "Freenet refused the connection (HTTP {s})"),
            SocketError::Timeout => f.write_str("Freenet stopped answering"),
            SocketError::Failed(e) => f.write_str(e),
        }
    }
}

/// The TCP stream, counting the bytes read from it. tungstenite reports only whole messages, so
/// without this a large frame arriving in pieces looks like silence until its last byte.
struct Counted {
    tcp: TcpStream,
    read: u64,
}

impl Read for Counted {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.tcp.read(buf)?;
        self.read += n as u64;
        Ok(n)
    }
}

impl Write for Counted {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.tcp.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.tcp.flush()
    }
}

type Ws = WebSocket<Counted>;
type HandshakeResult = Result<
    (Ws, tungstenite::handshake::client::Response),
    tungstenite::HandshakeError<tungstenite::ClientHandshake<Counted>>,
>;

/// How a wait for data ended.
enum Waited {
    /// The socket has data (or the wait was interrupted): read again.
    Ready,
    /// The wait's time ran out with no data reported. The caller reads once more before it
    /// reports a timeout, so data that arrived at that very moment is not missed.
    TimedOut,
    /// The wait itself failed; the caller sleeps instead.
    Failed(String),
}

/// Waits for a socket to have data to read. The only implementation in use is [`PollWaiter`];
/// tests put their own in to make a wait time out at the exact moment data arrives, and to
/// count the waits.
trait Waiter: Send {
    fn wait(&mut self, tcp: &TcpStream, left: Duration) -> Waited;
    /// Called once before the stream closes.
    fn release(&mut self, tcp: &TcpStream);
    /// Which waiter this is, so a test can tell the poller from a stand-in.
    #[cfg(test)]
    fn name(&self) -> &'static str;
}

/// The readiness poller (epoll, kqueue or AFD poll) registered with one stream.
struct PollWaiter {
    poller: polling::Poller,
    events: polling::Events,
}

impl PollWaiter {
    fn new(tcp: &TcpStream) -> Result<PollWaiter, String> {
        let poller = polling::Poller::new().map_err(|e| format!("creating the poller: {e}"))?;
        // SAFETY: the stream is owned by the `NodeSocket` that owns this waiter, and
        // `NodeSocket::drop` calls `release` (which deletes the registration) before the
        // stream closes.
        unsafe { poller.add(tcp, polling::Event::readable(0)) }
            .map_err(|e| format!("registering the socket with the poller: {e}"))?;
        Ok(PollWaiter {
            poller,
            events: polling::Events::with_capacity(std::num::NonZeroUsize::MIN),
        })
    }
}

impl Waiter for PollWaiter {
    fn wait(&mut self, tcp: &TcpStream, left: Duration) -> Waited {
        // One-shot interest: arm it again for each wait. Data that is already there makes
        // the wait return at once, so none can arrive unnoticed between the read that found
        // nothing and this call.
        self.events.clear();
        let result = self
            .poller
            .modify(tcp, polling::Event::readable(0))
            .and_then(|()| self.poller.wait(&mut self.events, Some(left)));
        match result {
            Ok(_) if self.events.iter().next().is_some() => Waited::Ready,
            Ok(_) => Waited::TimedOut,
            Err(e) if e.kind() == ErrorKind::Interrupted => Waited::Ready,
            Err(e) => Waited::Failed(e.to_string()),
        }
    }

    fn release(&mut self, tcp: &TcpStream) {
        let _ = self.poller.delete(tcp);
    }

    #[cfg(test)]
    fn name(&self) -> &'static str {
        "poll"
    }
}

/// Say once per process that waiting falls back to sleeping, and why: the fallback adds up to
/// [`POLL`] of delay to every answer, so it must not go unnoticed.
fn note_poller_failure(why: &str) {
    static SAID: AtomicBool = AtomicBool::new(false);
    if !SAID.swap(true, Ordering::Relaxed) {
        eprintln!(
            "River: the readiness poller for the Freenet connection failed ({why}); waiting for \
             Freenet by sleeping {}ms at a time instead, which slows every answer",
            POLL.as_millis()
        );
    }
}

/// Proof that a connection presented a grant token in its upgrade. Only
/// [`NodeSocket::connect`] makes one, after setting the `Authorization` header.
struct GrantPresented(());

pub struct NodeSocket {
    /// Tells when the socket has data (`None`: sleep between reads, see [`POLL`]). It is
    /// registered with the stream in `ws`, and [`Drop`] removes the registration before the
    /// stream closes.
    waiter: Option<Box<dyn Waiter>>,
    /// Times the sleep fallback was used (for tests: the poller path must use none).
    #[cfg(test)]
    fallback_sleeps: u64,
    /// How long a write may wait for room in the socket ([`WRITE_TIMEOUT`]).
    write_timeout: Duration,
    ws: Ws,
    /// Every connection presented a grant: what lets it register the chat delegate.
    grant: GrantPresented,
    /// Closed once River has moved on: then only reads leave.
    writes: WriteGate,
    /// Chunks of responses still arriving, by stream id.
    chunks: HashMap<u32, Chunks>,
    /// When anything (a response, a pong) last arrived.
    last_heard: Instant,
    /// Whether a whole message has arrived on this connection. Partial bytes move
    /// `last_heard` but not this.
    heard_anything: bool,
    /// How long [`recv`](NodeSocket::recv) waits for anything before it reports `Timeout`.
    read_wait: Duration,
}

struct Chunks {
    total: u32,
    parts: Vec<Option<Vec<u8>>>,
    have: u32,
    bytes: usize,
    started: Instant,
}

/// Connect to the first of `addrs` that accepts. A name such as `localhost` can resolve to
/// `::1` before `127.0.0.1` while a tunnel (or the node) listens on only one of them, as the
/// HTTP calls already cope with; trying only the first would be refused.
fn connect_any(
    addrs: impl Iterator<Item = std::net::SocketAddr>,
    timeout: Duration,
) -> std::io::Result<TcpStream> {
    let mut last = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "the name has no address")
    }))
}

impl NodeSocket {
    /// Open `ws://<authority><COMMAND_PATH>` presenting `token` as a bearer grant. `authority`
    /// is `host:port` and must be a loopback address: the node honours a grant only there.
    /// `writes` is the link's gate: while it is closed, the connection only reads.
    pub fn connect(
        authority: &str,
        token: &GrantToken,
        writes: &WriteGate,
    ) -> Result<NodeSocket, SocketError> {
        Self::connect_within(authority, token, writes, HANDSHAKE_TIMEOUT)
    }

    /// [`connect`](Self::connect) with the upgrade bounded by `handshake_timeout`.
    fn connect_within(
        authority: &str,
        token: &GrantToken,
        writes: &WriteGate,
        handshake_timeout: Duration,
    ) -> Result<NodeSocket, SocketError> {
        let fail = |e: &dyn std::fmt::Display| SocketError::Failed(e.to_string());
        let addrs = authority.to_socket_addrs().map_err(|e| fail(&e))?;
        let stream = connect_any(addrs, Duration::from_secs(5)).map_err(|e| fail(&e))?;
        stream.set_nodelay(true).map_err(|e| fail(&e))?;
        // No SO_RCVTIMEO / SO_SNDTIMEO anywhere on this socket (see the module docs).
        stream.set_nonblocking(true).map_err(|e| fail(&e))?;
        let mut request = format!("ws://{authority}{COMMAND_PATH}")
            .into_client_request()
            .map_err(|e| fail(&e))?;
        // A token that is not a bearer token (RFC 6750 token68: visible ASCII from a small
        // set) names nothing the node could accept, and some such tokens draw an HTTP 400
        // rather than a rejection. Treat it as the node would treat a rejected one, so it is
        // discarded rather than kept for good.
        let not_a_token = || SocketError::Closed {
            code: Some(4401),
            reason: "the saved grant is not a valid token".into(),
        };
        if !is_token68(token.expose()) {
            return Err(not_a_token());
        }
        let mut bearer: tungstenite::http::HeaderValue = format!("Bearer {}", token.expose())
            .parse()
            .map_err(|_| not_a_token())?;
        bearer.set_sensitive(true);
        request.headers_mut().insert("Authorization", bearer);
        let grant = GrantPresented(());
        let counted = Counted {
            tcp: stream,
            read: 0,
        };
        let (ws, _) = finish_handshake(tungstenite::client(request, counted), handshake_timeout)?;
        let waiter: Option<Box<dyn Waiter>> = match PollWaiter::new(&ws.get_ref().tcp) {
            Ok(w) => Some(Box::new(w)),
            Err(why) => {
                note_poller_failure(&why);
                None
            }
        };
        Ok(NodeSocket {
            waiter,
            #[cfg(test)]
            fallback_sleeps: 0,
            write_timeout: WRITE_TIMEOUT,
            ws,
            grant,
            writes: writes.clone(),
            chunks: HashMap::new(),
            last_heard: Instant::now(),
            heard_anything: false,
            read_wait: Duration::from_secs(10),
        })
    }

    /// Wait until the socket has data to read, or `left` has passed, whichever comes first. A
    /// wakeup with nothing to read is harmless: the caller reads again and finds `WouldBlock`.
    fn wait_for_data(&mut self, left: Duration) {
        let waited = match self.waiter.as_mut() {
            Some(waiter) => waiter.wait(&self.ws.get_ref().tcp, left),
            None => Waited::Failed(String::new()),
        };
        match waited {
            Waited::Ready | Waited::TimedOut => {}
            Waited::Failed(why) => {
                if !why.is_empty() {
                    note_poller_failure(&why);
                }
                #[cfg(test)]
                {
                    self.fallback_sleeps += 1;
                }
                std::thread::sleep(left.min(POLL));
            }
        }
    }

    /// Whether the connection still has unsent bytes queued (a pong or close reply that
    /// tungstenite queued inside a read, or part of a frame). Pushing them is retried here
    /// because a wait for readability would never notice the socket becoming writable.
    fn write_pending(&mut self) -> bool {
        matches!(
            self.ws.flush(),
            Err(tungstenite::Error::Io(e)) if e.kind() == ErrorKind::WouldBlock
        )
    }

    /// How long [`recv`](Self::recv) waits before reporting [`SocketError::Timeout`]. The wait
    /// runs from the start of each `recv`, whatever arrives meanwhile (see there).
    pub fn set_read_timeout(&mut self, timeout: Duration) -> Result<(), SocketError> {
        self.read_wait = timeout;
        Ok(())
    }

    /// Send a request. A `RegisterDelegate` is refused: it goes only through
    /// [`register_chat_delegate`](Self::register_chat_delegate) (see the module docs). While
    /// the write gate is closed, anything but a read ([`is_read`]) is refused.
    pub fn send(&mut self, request: &ClientRequest<'_>) -> Result<(), SocketError> {
        if matches!(
            request,
            ClientRequest::DelegateOp(DelegateRequest::RegisterDelegate { .. })
        ) {
            return Err(SocketError::Failed(
                "River registers its storage only through register_chat_delegate".into(),
            ));
        }
        if self.writes.is_closed() && !is_read(request) {
            return Err(SocketError::Failed(STALE_REFUSAL.into()));
        }
        self.write(request)
    }

    /// Register River's chat delegate (the WASM this build carries) on this grant connection,
    /// as the web UI does on every load. Registering one the node already has changes
    /// nothing. The node answers with an empty response for the delegate's key, or an error.
    /// Refused while the write gate is closed: this build's delegate is no longer River's.
    pub fn register_chat_delegate(&mut self) -> Result<(), SocketError> {
        if self.writes.is_closed() {
            return Err(SocketError::Failed(STALE_REFUSAL.into()));
        }
        let request = registration(&self.grant);
        self.write(&request)
    }

    fn write(&mut self, request: &ClientRequest<'_>) -> Result<(), SocketError> {
        let bytes = bincode::serialize(request).map_err(|e| SocketError::Failed(e.to_string()))?;
        self.write_message(Message::Binary(bytes.into()))
    }

    /// Queue `message` and push it out. The socket is non-blocking, so when it is full the
    /// frame stays queued in tungstenite (`send` queues before it flushes) and is flushed
    /// again until it is out or the write timeout passes, which fails the connection.
    ///
    /// Known limit: while it waits, this does not read, so a node that is itself blocked
    /// sending to us cannot be drained meanwhile. On loopback, with a bound of 10 s after which
    /// the connection fails and is opened again, that costs a reconnect and no data.
    fn write_message(&mut self, message: Message) -> Result<(), SocketError> {
        let mut result = self.ws.send(message);
        let deadline = Instant::now() + self.write_timeout;
        loop {
            match result {
                Ok(()) => return Ok(()),
                Err(tungstenite::Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(SocketError::Failed(
                            "the connection to Freenet is not taking data".into(),
                        ));
                    }
                    std::thread::sleep(WRITE_POLL);
                    result = self.ws.flush();
                }
                Err(e) => return Err(map_ws_error(e, false)),
            }
        }
    }

    /// Ask the node to show it is still there. The pong, like any message, moves
    /// [`last_heard`](Self::last_heard).
    pub fn ping(&mut self) -> Result<(), SocketError> {
        self.write_message(Message::Ping(Vec::new().into()))
    }

    /// Whether a whole message has arrived on this connection. Partial bytes move
    /// `last_heard` but not this.
    pub fn heard_anything(&self) -> bool {
        self.heard_anything
    }

    /// Read for up to `wait` before sending anything. The node rejects a bad token as soon as
    /// the connection opens, with an error and a close, and then drops the connection. A
    /// request written after that drop is answered with a reset, and on some platforms
    /// (Windows) a reset discards what had already arrived, rejection included. Reading
    /// first gets the rejection before anything is written. Nothing else is sent before the
    /// first request, so a timeout is the normal outcome.
    pub fn read_early(&mut self, wait: Duration) -> Result<(), SocketError> {
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            self.set_read_timeout(left)?;
            match self.recv() {
                // Anything else that arrives first is not a rejection; keep reading for one.
                Ok(_) => {}
                Err(SocketError::Timeout) => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }

    /// When anything last arrived from the node: a message of any kind, pongs included, or
    /// bytes of one still arriving.
    pub fn last_heard(&self) -> Instant {
        self.last_heard
    }

    /// The next whole response, or [`SocketError::Timeout`] once the read wait has passed since
    /// this call began.
    ///
    /// The deadline is fixed when the call starts and does not move when messages arrive: a
    /// node that only sends pongs (or a trickle of a large frame) cannot keep the caller from
    /// getting `Timeout` and doing its housekeeping on schedule. What does move is
    /// [`last_heard`](Self::last_heard): any bytes read, whole message or not, count as
    /// hearing from the node, so a big frame arriving slowly does not trip a ping/pong
    /// watchdog. A `Timeout` in the middle of a frame loses nothing: the next call carries on.
    pub fn recv(&mut self) -> Result<HostResult, SocketError> {
        let deadline = Instant::now() + self.read_wait;
        let mut seen = self.ws.get_ref().read;
        loop {
            let result = self.ws.read();
            if self.ws.get_ref().read != seen {
                seen = self.ws.get_ref().read;
                self.last_heard = Instant::now();
            }
            let message = match result {
                Ok(m) => m,
                Err(tungstenite::Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(SocketError::Timeout);
                    }
                    if self.write_pending() {
                        std::thread::sleep(left.min(WRITE_POLL));
                    } else {
                        self.wait_for_data(left);
                    }
                    continue;
                }
                Err(e) => return Err(map_ws_error(e, true)),
            };
            self.last_heard = Instant::now();
            self.heard_anything = true;
            let bytes = match message {
                Message::Binary(b) => b.to_vec(),
                Message::Close(frame) => {
                    return Err(match frame {
                        Some(f) => SocketError::Closed {
                            code: Some(u16::from(f.code)),
                            reason: f.reason.to_string(),
                        },
                        None => SocketError::Closed {
                            code: None,
                            reason: String::new(),
                        },
                    });
                }
                // Pings are answered by tungstenite; text is not part of the native protocol.
                // Empty means "not an answer" below (an empty Binary would not decode either).
                _ => Vec::new(),
            };
            if !bytes.is_empty() {
                match self.take(&bytes)? {
                    Some(Err(e)) => {
                        if let Some(rejected) = token_rejection(&e) {
                            return Err(rejected);
                        }
                        return Ok(Err(e));
                    }
                    Some(whole) => return Ok(whole),
                    None => {}
                }
            }
            // A message that is not an answer (a ping, a pong, text, a chunk of a response still
            // arriving, one this build cannot read) does not stop the wait running out: a node
            // that keeps sending them faster than they are read never lets the read block, so
            // the deadline is checked here too. A chunk already taken is kept in `self.chunks`.
            if Instant::now() >= deadline {
                return Err(SocketError::Timeout);
            }
        }
    }

    /// Decode one message, or hold it if it is a chunk of a response still arriving.
    fn take(&mut self, bytes: &[u8]) -> Result<Option<HostResult>, SocketError> {
        // A node newer than this stdlib can send a response variant it doesn't know. Skip that
        // one message rather than drop the connection and every subscription with it.
        let decoded: HostResult = match bincode::deserialize(bytes) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("River: skipping a message from Freenet it can't read: {e}");
                return Ok(None);
            }
        };
        let (stream_id, index, total, data) = match decoded {
            // The chunks that follow carry the response itself.
            Ok(HostResponse::StreamHeader { .. }) => return Ok(None),
            Ok(HostResponse::StreamChunk {
                stream_id,
                index,
                total,
                data,
            }) => (stream_id, index, total, data),
            other => return Ok(Some(other)),
        };
        if total == 0 || total > MAX_CHUNKS || index >= total {
            return Err(SocketError::Failed("Freenet sent a malformed chunk".into()));
        }
        // Forget responses that stopped arriving, and keep the number in progress bounded.
        self.chunks.retain(|_, c| c.started.elapsed() < STREAM_TTL);
        if !self.chunks.contains_key(&stream_id) && self.chunks.len() >= MAX_STREAMS {
            let oldest = self
                .chunks
                .iter()
                .min_by_key(|(_, c)| c.started)
                .map(|(id, _)| *id);
            if let Some(oldest) = oldest {
                self.chunks.remove(&oldest);
            }
        }
        let entry = self.chunks.entry(stream_id).or_insert_with(|| Chunks {
            total,
            parts: vec![None; total as usize],
            have: 0,
            bytes: 0,
            started: Instant::now(),
        });
        if entry.total != total {
            return Err(SocketError::Failed("Freenet sent a malformed chunk".into()));
        }
        let slot = &mut entry.parts[index as usize];
        if slot.is_none() {
            *slot = Some(data.to_vec());
            entry.have += 1;
            entry.bytes += data.len();
            if entry.bytes > MAX_RESPONSE_BYTES {
                self.chunks.remove(&stream_id);
                return Err(SocketError::Failed(
                    "Freenet sent a response larger than River accepts".into(),
                ));
            }
        }
        if entry.have < entry.total {
            return Ok(None);
        }
        let entry = self.chunks.remove(&stream_id).expect("present");
        let whole: Vec<u8> = entry.parts.into_iter().flatten().flatten().collect();
        let decoded: HostResult = bincode::deserialize(&whole)
            .map_err(|e| SocketError::Failed(format!("couldn't read Freenet's answer: {e}")))?;
        Ok(Some(decoded))
    }

    /// Responses being put back together (for tests).
    #[cfg(test)]
    fn streams_in_progress(&self) -> usize {
        self.chunks.len()
    }
}

/// Run an upgrade on a non-blocking stream to its end: try again after a short sleep until it
/// is done or `limit` passes.
fn finish_handshake(
    first: HandshakeResult,
    limit: Duration,
) -> Result<(Ws, tungstenite::handshake::client::Response), SocketError> {
    let deadline = Instant::now() + limit;
    let mut step = first;
    loop {
        match step {
            Ok(done) => return Ok(done),
            Err(tungstenite::HandshakeError::Interrupted(mid)) => {
                if Instant::now() >= deadline {
                    return Err(SocketError::Failed(
                        "Freenet did not answer the connection in time".into(),
                    ));
                }
                std::thread::sleep(WRITE_POLL);
                step = mid.handshake();
            }
            Err(tungstenite::HandshakeError::Failure(tungstenite::Error::Http(r))) => {
                return Err(SocketError::Handshake(r.status().as_u16()));
            }
            Err(tungstenite::HandshakeError::Failure(other)) => {
                return Err(SocketError::Failed(other.to_string()));
            }
        }
    }
}

/// What a write refused by a closed gate fails with.
pub const STALE_REFUSAL: &str = "a newer River is out, so this build no longer writes to Freenet";

/// Whether `request` only reads: a GET or a subscription of a contract, or one of the chat
/// delegate's four read requests (`ListRequest`, `GetRequest`, `GetVersionedRequest`,
/// `GetPublicKey`) and nothing else in the same message. Everything else counts as a write,
/// a request this app never sends included, and so does a delegate message that does not
/// decode: what is not known to read is refused while the gate is closed.
pub fn is_read(request: &ClientRequest<'_>) -> bool {
    use freenet_stdlib::client_api::ContractRequest;
    use freenet_stdlib::prelude::InboundDelegateMsg;
    use river_core::chat_delegate::ChatDelegateRequestMsg as Msg;
    match request {
        ClientRequest::ContractOp(ContractRequest::Get { .. })
        | ClientRequest::ContractOp(ContractRequest::Subscribe { .. }) => true,
        ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages { inbound, .. }) => {
            !inbound.is_empty()
                && inbound.iter().all(|m| match m {
                    InboundDelegateMsg::ApplicationMessage(m) => matches!(
                        ciborium::de::from_reader::<Msg, _>(m.payload.as_slice()),
                        Ok(Msg::ListRequest
                            | Msg::GetRequest { .. }
                            | Msg::GetVersionedRequest { .. }
                            | Msg::GetPublicKey { .. })
                    ),
                    _ => false,
                })
        }
        _ => false,
    }
}

/// The `RegisterDelegate` for River's chat delegate. It takes the proof that the connection
/// presented a grant, so it is built only for one.
///
/// The node ignores `cipher` and `nonce` (freenet-core derives each delegate's key from its
/// own; `secrets_store/store.rs`). Only nodes before freenet-core 0.2.60 used them, and River
/// needs 0.2.156 for grants, so fixed values do.
fn registration(_grant: &GrantPresented) -> ClientRequest<'static> {
    ClientRequest::DelegateOp(DelegateRequest::RegisterDelegate {
        delegate: super::delegate::chat_delegate_container(),
        cipher: [0; 32],
        nonce: [0; 24],
    })
}

/// Whether `t` is a token68 (RFC 7235): letters, digits, `-._~+/`, then optional `=` padding.
/// The node's grant tokens are base58, whose alphabet is a subset of this, so every token the
/// node issues passes; this only refuses what no node could have issued.
fn is_token68(t: &str) -> bool {
    let body = t.trim_end_matches('=');
    !body.is_empty()
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~+/".contains(&b))
}

/// The close a token rejection announces, if this error is one.
fn token_rejection(e: &ClientError) -> Option<SocketError> {
    let NodeErrorKind::Unhandled { cause } = e.kind() else {
        return None;
    };
    TOKEN_REJECTIONS.iter().find_map(|(prefix, code)| {
        cause
            .strip_prefix(prefix)
            .filter(|rest| rest.starts_with(':'))
            .map(|_| SocketError::Closed {
                code: Some(*code),
                reason: cause.to_string(),
            })
    })
}

/// Windows' `ERROR_IO_PENDING` / `WSA_IO_PENDING` ("Overlapped I/O operation is in progress").
/// Both names are 997. Rust's std does not map it to an `ErrorKind` (it is `Uncategorized`),
/// unlike `WSAEWOULDBLOCK` (10035, `WouldBlock`) and `WSAETIMEDOUT` (10060, `TimedOut`).
///
/// It was seen once on a read that had a receive timeout set (`SO_RCVTIMEO`), and once more
/// as a reply that never arrived. Microsoft says a socket whose receive timed out "is in an
/// indeterminate state and should be closed", so the code is a failure: the connection is
/// dropped and opened again, never carried on with. (Treating it as "nothing yet", as an
/// earlier change did, lost the bytes that were in flight.) The node socket no longer has a
/// receive timeout at all (see [`NodeSocket`]), so this is not expected to occur.
const IO_PENDING: i32 = 997;

/// The platform's raw code for "operation still pending", where there is one. On other
/// platforms 997 is an unrelated errno. The code is a parameter of [`map_ws_error_with`] so a
/// test can run the Windows rule anywhere.
const PLATFORM_PENDING: Option<i32> = if cfg!(windows) {
    Some(IO_PENDING)
} else {
    None
};

/// Whether the `count`th pending result of this run (1 for the first) is logged: the first, each
/// power of ten, and any other after `since_last` is a minute, so a stuck connection stays
/// visible without a line per result.
fn should_log_pending(count: u64, since_last: Option<Duration>) -> bool {
    count == 1 || is_power_of_ten(count) || since_last.is_some_and(|d| d >= Duration::from_secs(60))
}

fn is_power_of_ten(mut n: u64) -> bool {
    if n == 0 {
        return false;
    }
    while n.is_multiple_of(10) {
        n /= 10;
    }
    n == 1
}

/// Log a pending result (see [`should_log_pending`]): which call, the raw code, and how many so
/// far this run.
fn note_pending(reading: bool, e: &std::io::Error) {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNT: AtomicU64 = AtomicU64::new(0);
    static LAST_LOGGED: Mutex<Option<Instant>> = Mutex::new(None);
    let count = COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    let mut last = LAST_LOGGED.lock().unwrap_or_else(|p| p.into_inner());
    if !should_log_pending(count, last.map(|t| t.elapsed())) {
        return;
    }
    *last = Some(Instant::now());
    eprintln!(
        "River: a socket {} ended with os error {:?} ({e}); the connection is dropped and opened again (number {count} this run)",
        if reading { "read" } else { "write" },
        e.raw_os_error(),
    );
}

/// `reading`: the error came from a read, where `Timeout` means "nothing yet, still usable".
/// tungstenite keeps a half-read frame in its own buffers whatever error the read ended with
/// (`FrameCodec::read_frame` returns the io error with `?` before touching the buffered bytes
/// or the parsed header), so the next read carries on from it. A write that ends in an error
/// leaves the frame half sent, so callers drop the connection on any write error, `Timeout`
/// included.
fn map_ws_error(e: tungstenite::Error, reading: bool) -> SocketError {
    map_ws_error_with(e, reading, PLATFORM_PENDING)
}

fn map_ws_error_with(e: tungstenite::Error, reading: bool, pending: Option<i32>) -> SocketError {
    match e {
        tungstenite::Error::Io(io)
            if matches!(io.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
        {
            SocketError::Timeout
        }
        tungstenite::Error::Io(io) if pending.is_some() && io.raw_os_error() == pending => {
            note_pending(reading, &io);
            SocketError::Failed(tungstenite::Error::Io(io).to_string())
        }
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
            SocketError::Closed {
                code: None,
                reason: String::new(),
            }
        }
        other => SocketError::Failed(other.to_string()),
    }
}

impl Drop for NodeSocket {
    fn drop(&mut self) {
        if let Some(mut waiter) = self.waiter.take() {
            waiter.release(&self.ws.get_ref().tcp);
        }
    }
}

/// What the node's answer says to do with the grant token
/// (freenet-core `app_grants.rs`, "WebSocket close codes for a token").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenVerdict {
    /// 4401: the token names nothing (revoked, or never issued). Discard it.
    Discard,
    /// 4403: this connection can't use a grant (not loopback, a proxy, a hosted node). Keep it.
    Unusable,
    /// The node refused the WebSocket upgrade itself (an HTTP 4xx): retrying won't change it.
    Refused,
    /// 4503, or any other loss: keep the token and try again later.
    Retry,
}

pub fn token_verdict(error: &SocketError) -> TokenVerdict {
    match error {
        SocketError::Closed {
            code: Some(4401), ..
        } => TokenVerdict::Discard,
        SocketError::Closed {
            code: Some(4403), ..
        } => TokenVerdict::Unusable,
        // A busy node (408, 429) may let River in later; other 4xx are about the request.
        SocketError::Handshake(s) if (400..500).contains(s) && *s != 408 && *s != 429 => {
            TokenVerdict::Refused
        }
        _ => TokenVerdict::Retry,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;
    use tungstenite::protocol::CloseFrame;
    use tungstenite::protocol::frame::coding::CloseCode;

    fn chunk(stream_id: u32, index: u32, total: u32, data: &[u8]) -> Vec<u8> {
        let r: HostResult = Ok(HostResponse::StreamChunk {
            stream_id,
            index,
            total,
            data: data.to_vec().into(),
        });
        bincode::serialize(&r).unwrap()
    }

    /// A server that checks the upgrade request and then plays `script`.
    // The handshake callback's error type is tungstenite's, not ours.
    #[allow(clippy::result_large_err)]
    fn serve(script: Vec<Message>) -> (String, thread::JoinHandle<Vec<(String, String)>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut headers = Vec::new();
            let mut ws = tungstenite::accept_hdr(
                stream,
                |req: &tungstenite::handshake::server::Request, resp| {
                    headers.push(("uri".into(), req.uri().to_string()));
                    for (k, v) in req.headers() {
                        headers.push((k.as_str().to_owned(), v.to_str().unwrap().to_owned()));
                    }
                    Ok(resp)
                },
            )
            .unwrap();
            for m in script {
                ws.send(m).unwrap();
            }
            // Wait for the client to go away.
            while ws.read().is_ok() {}
            headers
        });
        (authority, handle)
    }

    impl NodeSocket {
        /// A connection whose write gate is open, as on a current build.
        fn connect_open(authority: &str, token: &GrantToken) -> Result<NodeSocket, SocketError> {
            NodeSocket::connect(authority, token, &WriteGate::default())
        }
    }

    /// Every kind of request this app sends, by name, with whether it only reads.
    fn every_request_kind() -> Vec<(&'static str, ClientRequest<'static>, bool)> {
        use super::super::delegate::{self, Read, Versioned};
        use freenet_stdlib::client_api::ContractRequest;
        use freenet_stdlib::prelude::{
            ContractCode, ContractContainer, ContractKey, ContractWasmAPIVersion, Parameters,
            RelatedContracts, StateDelta, UpdateData, WrappedContract, WrappedState,
        };
        let key = ContractKey::from_params_and_code(
            Parameters::from(vec![1u8]),
            ContractCode::from(vec![2u8]),
        );
        let owner = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let contract = ContractContainer::Wasm(ContractWasmAPIVersion::V1(WrappedContract::new(
            std::sync::Arc::new(ContractCode::from(vec![2u8])),
            Parameters::from(vec![1u8]),
        )));
        vec![
            (
                "subscribe GET",
                ClientRequest::ContractOp(ContractRequest::Get {
                    key: *key.id(),
                    return_contract_code: false,
                    subscribe: true,
                    blocking_subscribe: false,
                }),
                true,
            ),
            (
                "subscribe",
                ClientRequest::ContractOp(ContractRequest::Subscribe {
                    key: *key.id(),
                    summary: None,
                }),
                true,
            ),
            ("list rooms", delegate::request(&Read::List), true),
            (
                "get a room",
                delegate::request(&Read::Get(b"room:x".to_vec())),
                true,
            ),
            (
                "versioned read",
                delegate::versioned(&Versioned::Read(b"room:x".to_vec())),
                true,
            ),
            (
                "signing key check",
                delegate::public_key(&owner.verifying_key()),
                true,
            ),
            (
                "message UPDATE",
                ClientRequest::ContractOp(ContractRequest::Update {
                    key,
                    data: UpdateData::Delta(StateDelta::from(vec![0u8])),
                }),
                false,
            ),
            (
                "seed PUT",
                ClientRequest::ContractOp(ContractRequest::Put {
                    contract,
                    state: WrappedState::new(vec![0u8]),
                    related_contracts: RelatedContracts::default(),
                    subscribe: false,
                    blocking_subscribe: false,
                }),
                false,
            ),
            (
                "stored-room CAS",
                delegate::versioned(&Versioned::Store {
                    key: b"room:x".to_vec(),
                    value: vec![1],
                    generation: 0,
                }),
                false,
            ),
            (
                "signing key store",
                delegate::store_signing_key(&owner.verifying_key(), &owner),
                false,
            ),
            (
                "unregister a delegate",
                ClientRequest::DelegateOp(DelegateRequest::UnregisterDelegate(
                    delegate::chat_delegate_key().clone(),
                )),
                false,
            ),
            ("a read and a write in one message", mixed_batch(), false),
            (
                "an undecodable delegate message",
                ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages {
                    key: delegate::chat_delegate_key().clone(),
                    params: Parameters::from(Vec::<u8>::new()),
                    inbound: vec![
                        freenet_stdlib::prelude::InboundDelegateMsg::ApplicationMessage(
                            freenet_stdlib::prelude::ApplicationMessage::new(vec![0xff, 0x00]),
                        ),
                    ],
                }),
                false,
            ),
        ]
    }

    /// One delegate message carrying a read and a CAS store.
    fn mixed_batch() -> ClientRequest<'static> {
        use super::super::delegate::{self, Read, Versioned};
        let inbound = |r: ClientRequest<'static>| match r {
            ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages { inbound, .. }) => {
                inbound
            }
            _ => unreachable!(),
        };
        let mut all = inbound(delegate::request(&Read::List));
        all.extend(inbound(delegate::versioned(&Versioned::Store {
            key: b"room:x".to_vec(),
            value: vec![1],
            generation: 0,
        })));
        ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages {
            key: delegate::chat_delegate_key().clone(),
            params: freenet_stdlib::prelude::Parameters::from(Vec::<u8>::new()),
            inbound: all,
        })
    }

    /// The classification itself, for every request kind the app sends.
    #[test]
    fn reads_and_writes_are_told_apart() {
        for (name, request, read) in every_request_kind() {
            assert_eq!(is_read(&request), read, "{name}");
        }
        assert!(!is_read(&registration(&GrantPresented(()))));
    }

    /// While the gate is closed, every write kind is refused at the socket and never reaches
    /// the node, registration included; reads still go. With the gate open, all of them go.
    #[test]
    fn a_closed_gate_refuses_every_write_kind() {
        let gate = WriteGate::default();
        gate.close();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            let mut got = Vec::new();
            while let Ok(m) = ws.read() {
                if let Message::Binary(b) = m {
                    got.push(
                        bincode::deserialize::<ClientRequest>(&b)
                            .unwrap()
                            .into_owned(),
                    );
                }
            }
            got
        });
        let mut socket =
            NodeSocket::connect(&authority, &GrantToken::new("t".into()), &gate).unwrap();
        let mut reads = 0;
        for (name, request, read) in every_request_kind() {
            let sent = socket.send(&request);
            if read {
                assert!(sent.is_ok(), "{name}: a read was refused: {sent:?}");
                reads += 1;
            } else {
                assert!(
                    matches!(&sent, Err(SocketError::Failed(m)) if m == STALE_REFUSAL),
                    "{name}: a write was not refused: {sent:?}"
                );
            }
        }
        assert!(matches!(
            socket.register_chat_delegate(),
            Err(SocketError::Failed(m)) if m == STALE_REFUSAL
        ));
        drop(socket);
        let got = server.join().unwrap();
        assert_eq!(got.len(), reads, "only the reads reached the node");
        assert!(got.iter().all(is_read), "{got:?}");

        // The same requests on an open gate all leave.
        let (authority, server) = serve(vec![]);
        let mut socket =
            NodeSocket::connect_open(&authority, &GrantToken::new("t".into())).unwrap();
        for (name, request, _) in every_request_kind() {
            assert!(socket.send(&request).is_ok(), "{name}");
        }
        socket.register_chat_delegate().unwrap();
        drop(socket);
        server.join().unwrap();
    }

    /// A `RegisterDelegate` leaves only through `register_chat_delegate`, on a connection that
    /// presented the grant, and carries the delegate this build reads rooms from.
    #[test]
    fn only_register_chat_delegate_sends_a_registration() {
        let (authority, server) = serve(vec![]);
        let mut socket =
            NodeSocket::connect_open(&authority, &GrantToken::new("grant-t0k".into())).unwrap();
        let register = registration(&GrantPresented(()));
        let ClientRequest::DelegateOp(DelegateRequest::RegisterDelegate { delegate, .. }) =
            &register
        else {
            panic!("not a registration");
        };
        assert_eq!(delegate.key(), super::super::delegate::chat_delegate_key());
        assert!(matches!(
            socket.send(&register),
            Err(SocketError::Failed(m)) if m.contains("register_chat_delegate")
        ));
        socket.register_chat_delegate().unwrap();
        drop(socket);
        let headers = server.join().unwrap();
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "authorization" && v == "Bearer grant-t0k"),
            "{headers:?}"
        );
    }

    /// A token the node could not take as a grant (empty, or not a bearer token) never opens a
    /// connection, so nothing can be sent, a registration included, without one.
    #[test]
    fn no_connection_opens_without_a_grant_token() {
        // A listener that never answers the upgrade: refusing the token comes first.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        for bad in ["", "   ", "a b", "tok\n"] {
            assert!(
                matches!(
                    NodeSocket::connect_open(&authority, &GrantToken::new(bad.into())),
                    Err(SocketError::Closed {
                        code: Some(4401),
                        ..
                    })
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn connects_with_the_bearer_token_and_reassembles_chunks() {
        let whole: HostResult = Ok(HostResponse::Ok);
        let whole = bincode::serialize(&whole).unwrap();
        let (a, b) = whole.split_at(whole.len() / 2);
        let header: HostResult = Ok(HostResponse::StreamHeader {
            stream_id: 9,
            total_bytes: whole.len() as u64,
            content: freenet_stdlib::client_api::StreamContent::Raw,
        });
        let script = vec![
            Message::Binary(bincode::serialize(&header).unwrap().into()),
            // Out of order, with a duplicate.
            Message::Binary(chunk(9, 1, 2, b).into()),
            Message::Binary(chunk(9, 1, 2, b).into()),
            Message::Binary(chunk(9, 0, 2, a).into()),
            Message::Close(Some(CloseFrame {
                code: CloseCode::from(4401),
                reason: "app grant revoked".into(),
            })),
        ];
        let (authority, server) = serve(script);
        let mut socket =
            NodeSocket::connect_open(&authority, &GrantToken::new("t0k".into())).unwrap();
        assert!(matches!(socket.recv(), Ok(Ok(HostResponse::Ok))));
        let closed = socket.recv().unwrap_err();
        assert_eq!(
            closed,
            SocketError::Closed {
                code: Some(4401),
                reason: "app grant revoked".into()
            }
        );
        assert_eq!(token_verdict(&closed), TokenVerdict::Discard);
        drop(socket);
        let headers = server.join().unwrap();
        let get = |k: &str| {
            headers
                .iter()
                .find(|(h, _)| h.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("authorization").as_deref(), Some("Bearer t0k"));
        assert_eq!(get("uri").as_deref(), Some(COMMAND_PATH));
        assert_eq!(get("origin"), None);
    }

    #[test]
    fn close_codes_decide_what_happens_to_the_token() {
        let closed = |code| SocketError::Closed {
            code: Some(code),
            reason: String::new(),
        };
        assert_eq!(token_verdict(&closed(4401)), TokenVerdict::Discard);
        assert_eq!(token_verdict(&closed(4403)), TokenVerdict::Unusable);
        assert_eq!(token_verdict(&closed(4503)), TokenVerdict::Retry);
        assert_eq!(token_verdict(&SocketError::Timeout), TokenVerdict::Retry);
        assert_eq!(
            token_verdict(&SocketError::Failed("x".into())),
            TokenVerdict::Retry
        );
    }

    #[test]
    fn an_unreadable_message_is_skipped() {
        let ok: HostResult = Ok(HostResponse::Ok);
        let (authority, server) = serve(vec![
            Message::Binary(vec![0xff, 0xff, 0xff, 0xff, 0xff].into()),
            Message::Binary(bincode::serialize(&ok).unwrap().into()),
        ]);
        let mut socket =
            NodeSocket::connect_open(&authority, &GrantToken::new("t".into())).unwrap();
        assert!(matches!(socket.recv(), Ok(Ok(HostResponse::Ok))));
        drop(socket);
        server.join().unwrap();
    }

    #[test]
    fn malformed_chunks_are_refused() {
        for bad in [
            chunk(1, 2, 2, b"x"),
            chunk(1, 0, 0, b"x"),
            chunk(1, 0, 999, b"x"),
        ] {
            let (authority, server) = serve(vec![Message::Binary(bad.into())]);
            let mut socket =
                NodeSocket::connect_open(&authority, &GrantToken::new("t".into())).unwrap();
            assert!(matches!(socket.recv(), Err(SocketError::Failed(_))));
            drop(socket);
            server.join().unwrap();
        }
    }

    /// The node's real sequence for a rejected token: the error, then the close frame
    /// (freenet-core `client_events/websocket.rs`, the `token_check != Valid` branch).
    pub(crate) fn rejection(prefix: &str, code: u16) -> Vec<Message> {
        let error: HostResult = Err(NodeErrorKind::Unhandled {
            cause: format!("{prefix}: explanation from the node").into(),
        }
        .into());
        vec![
            Message::Binary(bincode::serialize(&error).unwrap().into()),
            Message::Close(Some(CloseFrame {
                code: CloseCode::from(code),
                reason: prefix.to_owned().into(),
            })),
        ]
    }

    #[test]
    fn a_token_rejection_is_read_from_the_error_before_the_close() {
        for (prefix, code, verdict) in [
            ("AUTH_TOKEN_INVALID", 4401, TokenVerdict::Discard),
            ("AUTH_TOKEN_NOT_USABLE_HERE", 4403, TokenVerdict::Unusable),
            ("AUTH_TOKEN_UNAVAILABLE", 4503, TokenVerdict::Retry),
        ] {
            let (authority, server) = serve(rejection(prefix, code));
            let mut socket =
                NodeSocket::connect_open(&authority, &GrantToken::new("t".into())).unwrap();
            let err = socket.recv().unwrap_err();
            assert_eq!(
                err,
                SocketError::Closed {
                    code: Some(code),
                    reason: format!("{prefix}: explanation from the node"),
                },
                "{prefix}"
            );
            assert_eq!(token_verdict(&err), verdict, "{prefix}");
            drop(socket);
            server.join().unwrap();
        }
    }

    #[test]
    fn other_errors_stay_errors() {
        for cause in [
            "AUTH_TOKEN_INVALIDATED: no",
            "something AUTH_TOKEN_INVALID: x",
        ] {
            let error: HostResult = Err(NodeErrorKind::Unhandled {
                cause: cause.to_owned().into(),
            }
            .into());
            let (authority, server) = serve(vec![Message::Binary(
                bincode::serialize(&error).unwrap().into(),
            )]);
            let mut socket =
                NodeSocket::connect_open(&authority, &GrantToken::new("t".into())).unwrap();
            assert!(matches!(socket.recv(), Ok(Err(_))), "{cause}");
            drop(socket);
            server.join().unwrap();
        }
    }

    #[test]
    fn a_refused_upgrade_is_a_handshake_error() {
        use std::io::{Read, Write};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        let server = thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            // Read the whole upgrade request first: answering before it is read makes
            // Windows reset the connection, and the reset discards the answer.
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                conn.read_exact(&mut byte).unwrap();
                head.push(byte[0]);
            }
            conn.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });
        let err = match NodeSocket::connect_open(&authority, &GrantToken::new("t".into())) {
            Err(e) => e,
            Ok(_) => panic!("the upgrade was refused"),
        };
        assert_eq!(err, SocketError::Handshake(403));
        assert_eq!(token_verdict(&err), TokenVerdict::Refused);
        assert_eq!(
            token_verdict(&SocketError::Handshake(429)),
            TokenVerdict::Retry
        );
        assert_eq!(
            token_verdict(&SocketError::Handshake(503)),
            TokenVerdict::Retry
        );
        server.join().unwrap();
    }

    #[test]
    fn unfinished_responses_are_bounded() {
        let mut script: Vec<Message> = (0..20)
            .map(|id| Message::Binary(chunk(id, 0, 2, b"x").into()))
            .collect();
        let ok: HostResult = Ok(HostResponse::Ok);
        script.push(Message::Binary(bincode::serialize(&ok).unwrap().into()));
        let (authority, server) = serve(script);
        let mut socket =
            NodeSocket::connect_open(&authority, &GrantToken::new("t".into())).unwrap();
        assert!(matches!(socket.recv(), Ok(Ok(HostResponse::Ok))));
        assert_eq!(socket.streams_in_progress(), MAX_STREAMS);
        drop(socket);
        server.join().unwrap();
    }

    #[test]
    fn a_pong_counts_as_hearing_from_the_node() {
        let (authority, server) = serve(Vec::new());
        let mut socket =
            NodeSocket::connect_open(&authority, &GrantToken::new("t".into())).unwrap();
        let before = socket.last_heard();
        thread::sleep(Duration::from_millis(20));
        socket.ping().unwrap();
        socket.set_read_timeout(Duration::from_millis(300)).unwrap();
        assert_eq!(socket.recv().unwrap_err(), SocketError::Timeout);
        assert!(socket.last_heard() > before);
        drop(socket);
        server.join().unwrap();
    }

    /// A saved token that can't be sent at all is treated as one the node rejected, so it is
    /// discarded rather than retried forever.
    #[test]
    fn a_token_that_cannot_be_a_header_counts_as_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        for bad in ["bad\ntoken", "tökén", "has space", "", "=="] {
            let err = match NodeSocket::connect_open(&authority, &GrantToken::new(bad.into())) {
                Err(e) => e,
                Ok(_) => panic!("{bad:?} is not a bearer token"),
            };
            assert_eq!(token_verdict(&err), TokenVerdict::Discard, "{bad:?}");
        }
        assert!(is_token68("abc-DEF_123.~+/=="));
    }

    #[test]
    fn a_name_that_resolves_to_a_refusing_address_first_still_connects() {
        // What `localhost` can do: an address nobody listens on, then the one that does.
        let dead = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        let live = TcpListener::bind("127.0.0.1:0").unwrap();
        let addrs = vec![dead, live.local_addr().unwrap()];
        let stream = connect_any(addrs.into_iter(), Duration::from_secs(2)).unwrap();
        assert_eq!(stream.peer_addr().unwrap(), live.local_addr().unwrap());
        // Nothing accepting is still an error, and no address is too.
        assert!(connect_any(vec![dead].into_iter(), Duration::from_secs(2)).is_err());
        assert!(connect_any(Vec::new().into_iter(), Duration::from_secs(2)).is_err());
    }

    fn io(code: i32) -> tungstenite::Error {
        tungstenite::Error::Io(std::io::Error::from_raw_os_error(code))
    }

    /// Windows' pending code on a read or a write is a failure (the connection is dropped and
    /// opened again), never "nothing yet": the socket may have lost bytes. Ordinary timeouts
    /// stay `Timeout`, and without the platform code 997 is an ordinary error.
    #[test]
    fn the_pending_code_is_a_failure_not_nothing_yet() {
        for reading in [true, false] {
            let r = map_ws_error_with(io(IO_PENDING), reading, Some(IO_PENDING));
            assert!(matches!(r, SocketError::Failed(_)), "{r:?}");
        }
        let r = map_ws_error_with(io(IO_PENDING), true, None);
        assert!(matches!(r, SocketError::Failed(_)), "{r:?}");
        let timed_out = || tungstenite::Error::Io(ErrorKind::TimedOut.into());
        let would_block = || tungstenite::Error::Io(ErrorKind::WouldBlock.into());
        assert_eq!(
            map_ws_error_with(timed_out(), true, Some(IO_PENDING)),
            SocketError::Timeout
        );
        assert_eq!(
            map_ws_error_with(would_block(), true, Some(IO_PENDING)),
            SocketError::Timeout
        );
        assert_eq!(IO_PENDING, 997);
    }

    // ---- helpers for the wait and framing tests ----

    /// What happened to the waits of a socket whose waiter is a [`Recording`].
    #[derive(Default)]
    struct Counters {
        calls: AtomicUsize,
        ready: AtomicUsize,
        timed_out: AtomicUsize,
        failed: AtomicUsize,
    }

    /// Wraps the socket's real waiter and counts what each wait did.
    struct Recording {
        inner: Box<dyn Waiter>,
        counters: Arc<Counters>,
    }

    impl Waiter for Recording {
        fn wait(&mut self, tcp: &TcpStream, left: Duration) -> Waited {
            self.counters.calls.fetch_add(1, Ordering::SeqCst);
            let w = self.inner.wait(tcp, left);
            let slot = match &w {
                Waited::Ready => &self.counters.ready,
                Waited::TimedOut => &self.counters.timed_out,
                Waited::Failed(_) => &self.counters.failed,
            };
            slot.fetch_add(1, Ordering::SeqCst);
            w
        }
        fn release(&mut self, tcp: &TcpStream) {
            self.inner.release(tcp)
        }
        fn name(&self) -> &'static str {
            self.inner.name()
        }
    }

    /// Put a [`Recording`] around the socket's poller; panics if it has none (the poller
    /// could not be made on this machine, which a test must not pass over silently).
    fn record(socket: &mut NodeSocket) -> Arc<Counters> {
        let counters = Arc::new(Counters::default());
        let inner = socket.waiter.take().expect("the poller could not be made");
        assert_eq!(inner.name(), "poll");
        socket.waiter = Some(Box::new(Recording {
            inner,
            counters: counters.clone(),
        }));
        counters
    }

    /// A waiter that makes the race happen by construction: its first wait has the node send
    /// a frame, and only then reports that the wait ran out.
    /// Wait until `n` bytes are readable on the client side. A node's write returning does not
    /// make its bytes readable here yet (macOS loopback delivers them a little later), so a
    /// test that needs "the data is waiting" must wait for it, not for the node's write.
    fn wait_readable(tcp: &TcpStream, n: usize) {
        let until = Instant::now() + Duration::from_secs(10);
        let mut probe = vec![0u8; n];
        loop {
            match tcp.peek(&mut probe) {
                Ok(got) if got >= n => return,
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) => panic!("peek on the node socket: {e}"),
            }
            assert!(Instant::now() < until, "the node's bytes never arrived");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    struct Inject {
        go: std::sync::mpsc::Sender<()>,
        done: std::sync::mpsc::Receiver<()>,
        calls: Arc<AtomicUsize>,
        /// How long the first wait sleeps once the frame is readable: past the read wait.
        past: Duration,
    }

    impl Waiter for Inject {
        fn wait(&mut self, tcp: &TcpStream, _left: Duration) -> Waited {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.go.send(()).unwrap();
                self.done.recv().unwrap();
                // Report the timeout only once the frame is readable AND the read wait is
                // over, so the race is the one meant: data waiting, deadline passed.
                wait_readable(tcp, 1);
                std::thread::sleep(self.past);
            }
            Waited::TimedOut
        }
        fn release(&mut self, _tcp: &TcpStream) {}
        fn name(&self) -> &'static str {
            "inject"
        }
    }

    /// A waiter whose every wait fails.
    struct Broken;

    impl Waiter for Broken {
        fn wait(&mut self, _tcp: &TcpStream, _left: Duration) -> Waited {
            Waited::Failed("injected failure".into())
        }
        fn release(&mut self, _tcp: &TcpStream) {}
        fn name(&self) -> &'static str {
            "broken"
        }
    }

    /// The `i`th distinguishable answer.
    fn numbered(i: u32) -> HostResult {
        Err(NodeErrorKind::Unhandled {
            cause: format!("frame {i}").into(),
        }
        .into())
    }

    fn text_of(r: &HostResult) -> String {
        match r {
            Err(e) => match e.kind() {
                NodeErrorKind::Unhandled { cause } => cause.to_string(),
                other => format!("{other:?}"),
            },
            Ok(r) => format!("{r:?}"),
        }
    }

    /// The bytes of a server-to-client binary frame (unmasked) carrying `r`.
    fn frame_bytes(r: &HostResult) -> Vec<u8> {
        let payload = bincode::serialize(r).unwrap();
        let mut f = vec![0x82];
        match payload.len() {
            n if n < 126 => f.push(n as u8),
            n if n < 65536 => {
                f.push(126);
                f.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                f.push(127);
                f.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        f.extend_from_slice(&payload);
        f
    }

    /// A node that accepts one connection, completes the upgrade and runs `play` on it.
    fn node_that<T: Send + 'static>(
        play: impl FnOnce(WebSocket<TcpStream>) -> T + Send + 'static,
    ) -> (String, thread::JoinHandle<T>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            play(tungstenite::accept(stream).unwrap())
        });
        (authority, handle)
    }

    fn open(authority: &str) -> NodeSocket {
        NodeSocket::connect_open(authority, &GrantToken::new("t".into())).unwrap()
    }

    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::channel;

    /// No receive or send timeout is set on the node socket, before or after the wait is
    /// configured: that option is what Windows documents as leaving a timed-out socket
    /// indeterminate. This fails on the code that set `SO_RCVTIMEO` / `SO_SNDTIMEO`.
    #[test]
    fn the_node_socket_has_no_os_timeouts() {
        let (authority, server) = serve(Vec::new());
        let mut socket = open(&authority);
        let check = |socket: &NodeSocket| {
            let tcp = &socket.ws.get_ref().tcp;
            assert_eq!(tcp.read_timeout().unwrap(), None);
            assert_eq!(tcp.write_timeout().unwrap(), None);
        };
        check(&socket);
        socket.set_read_timeout(Duration::from_millis(5)).unwrap();
        assert_eq!(socket.recv().unwrap_err(), SocketError::Timeout);
        check(&socket);
        drop(socket);
        server.join().unwrap();
    }

    /// The original race, by construction: the wait runs out as a frame arrives (the stand-in
    /// waiter has the node send it, waits until it is readable and past the read wait, and
    /// only then reports the timeout). `recv` must read once more and return the frame, not
    /// `Timeout`, even though its deadline has passed. A later wait with nothing sent does
    /// time out.
    #[test]
    fn a_wait_that_runs_out_as_data_arrives_loses_nothing() {
        let (go_tx, go_rx) = channel();
        let (done_tx, done_rx) = channel();
        let (authority, server) = node_that(move |mut ws| {
            go_rx.recv().unwrap();
            ws.send(Message::Binary(
                bincode::serialize(&numbered(1)).unwrap().into(),
            ))
            .unwrap();
            done_tx.send(()).unwrap();
            while ws.read().is_ok() {}
        });
        let mut socket = open(&authority);
        let calls = Arc::new(AtomicUsize::new(0));
        socket.waiter = Some(Box::new(Inject {
            go: go_tx,
            done: done_rx,
            calls: calls.clone(),
            past: Duration::from_millis(150),
        }));
        socket.set_read_timeout(Duration::from_millis(100)).unwrap();
        let got = socket.recv().expect("the frame was lost to the timeout");
        assert_eq!(text_of(&got), "frame 1");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        socket.set_read_timeout(Duration::from_millis(20)).unwrap();
        assert_eq!(socket.recv().unwrap_err(), SocketError::Timeout);
        drop(socket);
        server.join().unwrap();
    }

    /// The poller path is the one in use: a frame sent after 50 ms ends a 30 s wait at once,
    /// by a wait that reported `Ready`, and the sleep fallback is never taken. A poller that
    /// failed on every call would fail this test instead of passing silently. With no poller
    /// the fallback still delivers the frame within [`POLL`].
    #[test]
    fn the_poller_wakes_the_wait_and_the_fallback_sleep_is_short() {
        for poller in [true, false] {
            let (authority, server) = node_that(move |mut ws| {
                thread::sleep(Duration::from_millis(50));
                ws.send(Message::Binary(
                    bincode::serialize(&numbered(2)).unwrap().into(),
                ))
                .unwrap();
                while ws.read().is_ok() {}
            });
            let mut socket = open(&authority);
            let counters = if poller {
                Some(record(&mut socket))
            } else {
                assert!(socket.waiter.is_some(), "the poller could not be made");
                socket.waiter = None;
                None
            };
            socket.set_read_timeout(Duration::from_secs(30)).unwrap();
            let start = Instant::now();
            let got = socket.recv().unwrap();
            let took = start.elapsed();
            assert_eq!(text_of(&got), "frame 2");
            assert!(took < Duration::from_secs(1), "poller {poller}: {took:?}");
            match counters {
                Some(c) => {
                    assert!(
                        c.calls.load(Ordering::SeqCst) >= 1,
                        "the poller was not used"
                    );
                    assert!(c.ready.load(Ordering::SeqCst) >= 1);
                    assert_eq!(c.failed.load(Ordering::SeqCst), 0);
                    assert_eq!(socket.fallback_sleeps, 0, "the sleep fallback ran");
                }
                None => assert!(socket.fallback_sleeps >= 1),
            }
            drop(socket);
            server.join().unwrap();
        }
    }

    /// A poller whose every wait fails falls back to sleeping, delivers the frame, and does
    /// not panic.
    #[test]
    fn a_poller_that_fails_falls_back_to_sleeping() {
        let (authority, server) = node_that(move |mut ws| {
            thread::sleep(Duration::from_millis(30));
            ws.send(Message::Binary(
                bincode::serialize(&numbered(3)).unwrap().into(),
            ))
            .unwrap();
            while ws.read().is_ok() {}
        });
        let mut socket = open(&authority);
        socket.waiter = Some(Box::new(Broken));
        socket.set_read_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(text_of(&socket.recv().unwrap()), "frame 3");
        assert!(socket.fallback_sleeps >= 1);
        drop(socket);
        server.join().unwrap();
    }

    /// Frames that arrive together are returned one `recv` at a time without waiting in
    /// between: three small frames in ONE write, and no wait at all is made, because
    /// tungstenite already holds the other two. (Waiting for the socket while a frame sits in
    /// tungstenite's buffer would stall until the next byte or the deadline.)
    #[test]
    fn frames_already_buffered_are_returned_without_waiting() {
        let (go_tx, go_rx) = channel::<()>();
        let (done_tx, done_rx) = channel::<()>();
        let (authority, server) = node_that(move |mut ws| {
            go_rx.recv().unwrap();
            let all: Vec<u8> = (10..13).flat_map(|i| frame_bytes(&numbered(i))).collect();
            ws.get_mut().write_all(&all).unwrap();
            ws.get_mut().flush().unwrap();
            done_tx.send(()).unwrap();
            while ws.read().is_ok() {}
        });
        let mut socket = open(&authority);
        let counters = record(&mut socket);
        socket.set_read_timeout(Duration::from_secs(1)).unwrap();
        go_tx.send(()).unwrap();
        done_rx.recv().unwrap();
        let total: usize = (10..13).map(|i| frame_bytes(&numbered(i)).len()).sum();
        wait_readable(&socket.ws.get_ref().tcp, total);
        let start = Instant::now();
        let texts: Vec<String> = (0..3).map(|_| text_of(&socket.recv().unwrap())).collect();
        assert_eq!(texts, ["frame 10", "frame 11", "frame 12"]);
        assert!(start.elapsed() < Duration::from_millis(500));
        assert_eq!(counters.calls.load(Ordering::SeqCst), 0, "a wait was made");
        drop(socket);
        server.join().unwrap();
    }

    /// A 4 MiB message, which takes many reads, arrives byte for byte.
    #[test]
    fn a_four_megabyte_message_arrives_whole() {
        let cause = "abcdefgh".repeat(512 * 1024);
        let expected = cause.clone();
        let (authority, server) = node_that(move |mut ws| {
            let r: HostResult = Err(NodeErrorKind::Unhandled {
                cause: cause.into(),
            }
            .into());
            ws.send(Message::Binary(bincode::serialize(&r).unwrap().into()))
                .unwrap();
            while ws.read().is_ok() {}
        });
        let mut socket = open(&authority);
        socket.set_read_timeout(Duration::from_secs(20)).unwrap();
        let got = socket.recv().unwrap();
        assert!(text_of(&got) == expected, "the message differs");
        drop(socket);
        server.join().unwrap();
    }

    /// A frame cut inside its 2-byte header and again inside its payload, each part released
    /// only after the client's previous `recv` returned `Timeout`, arrives whole on the third
    /// `recv`. Bytes that arrive count as hearing from the node (`last_heard` moves) even
    /// though no message is complete: a large frame trickling in must not look like silence.
    #[test]
    fn a_frame_in_three_parts_across_timeouts_arrives_whole_and_counts_as_heard() {
        let bytes = frame_bytes(&numbered(7));
        let (sent_tx, sent_rx) = channel::<()>();
        let (release_tx, release_rx) = channel::<()>();
        let (connected_tx, connected_rx) = channel::<()>();
        let (authority, server) = node_that(move |mut ws| {
            // Nothing is sent before the client has finished the upgrade: bytes that came
            // with the upgrade answer would be consumed by it, not by `recv`.
            connected_rx.recv().unwrap();
            let tcp = ws.get_mut();
            for (i, part) in [&bytes[..1], &bytes[1..5], &bytes[5..]].iter().enumerate() {
                if i > 0 {
                    release_rx.recv().unwrap();
                }
                tcp.write_all(part).unwrap();
                tcp.flush().unwrap();
                sent_tx.send(()).unwrap();
            }
            while ws.read().is_ok() {}
        });
        let mut socket = open(&authority);
        connected_tx.send(()).unwrap();
        socket.set_read_timeout(Duration::from_millis(50)).unwrap();
        for _ in 0..2 {
            sent_rx.recv().unwrap();
            wait_readable(&socket.ws.get_ref().tcp, 1);
            let before = socket.last_heard();
            assert_eq!(socket.recv().unwrap_err(), SocketError::Timeout);
            assert!(socket.last_heard() > before, "the bytes were not counted");
            release_tx.send(()).unwrap();
        }
        sent_rx.recv().unwrap();
        socket.set_read_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(text_of(&socket.recv().unwrap()), "frame 7");
        drop(socket);
        server.join().unwrap();
    }

    /// A node that sends pings faster than they can be read never lets the read block, so the
    /// deadline must be checked after each message too, or `recv` never returns. The node
    /// writes pings in 64 KiB batches, far faster than the client parses them and queues a
    /// pong for each; `recv` runs on its own thread so the old code fails this by the bound
    /// rather than hanging the suite.
    #[test]
    fn a_flood_of_pings_does_not_keep_recv_from_its_timeout() {
        use std::sync::atomic::AtomicBool;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_node = stop.clone();
        let (authority, server) = node_that(move |mut ws| {
            // An unmasked empty ping, from the node, is two bytes.
            let batch: Vec<u8> = [0x89u8, 0x00].repeat(32 * 1024);
            // Non-blocking with an explicit offset: a partial write must resume where it
            // stopped, or a batch restarted mid-frame would split a ping and break the stream.
            let tcp = ws.get_mut();
            tcp.set_nonblocking(true).unwrap();
            let mut at = 0;
            while !stop_node.load(Ordering::SeqCst) {
                match tcp.write(&batch[at..]) {
                    Ok(n) => at = (at + n) % batch.len(),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1))
                    }
                    Err(_) => return,
                }
            }
        });
        let (result_tx, result_rx) = channel();
        let reader = thread::spawn(move || {
            let mut socket = open(&authority);
            socket.set_read_timeout(Duration::from_millis(100)).unwrap();
            let start = Instant::now();
            let got = socket.recv();
            result_tx.send((got.map(|_| ()), start.elapsed())).unwrap();
            socket
        });
        let outcome = result_rx.recv_timeout(Duration::from_secs(5));
        stop.store(true, Ordering::SeqCst);
        let (got, took) = outcome.expect("recv never returned while the node kept sending pings");
        assert_eq!(got.unwrap_err(), SocketError::Timeout);
        assert!(took >= Duration::from_millis(100), "{took:?}");
        drop(reader.join().unwrap());
        server.join().unwrap();
    }

    /// Intended behaviour, stated: the read wait runs from the start of each `recv` and does
    /// NOT restart when messages arrive. A node that sends a pong every 5 ms must not keep the
    /// session from its tick; `recv` still reports `Timeout` at the wait, while `last_heard`
    /// keeps moving so the watchdog sees a live node.
    #[test]
    fn a_steady_stream_of_pongs_does_not_starve_the_timeout() {
        let (authority, server) = node_that(move |mut ws| {
            for _ in 0..120 {
                if ws.send(Message::Pong(Vec::new().into())).is_err() {
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
            while ws.read().is_ok() {}
        });
        let mut socket = open(&authority);
        socket.set_read_timeout(Duration::from_millis(100)).unwrap();
        let before = socket.last_heard();
        let start = Instant::now();
        assert_eq!(socket.recv().unwrap_err(), SocketError::Timeout);
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(100), "{took:?}");
        assert!(took < Duration::from_millis(400), "starved: {took:?}");
        assert!(socket.last_heard() > before);
        drop(socket);
        server.join().unwrap();
    }

    /// A node that never reads: writes fail with `Failed` after the write timeout (shortened
    /// here to 200 ms) and never hang.
    #[test]
    fn a_write_the_node_never_takes_fails_after_the_write_timeout() {
        let (end_tx, end_rx) = channel::<()>();
        let (authority, server) = node_that(move |ws| {
            end_rx.recv().unwrap();
            drop(ws);
        });
        let mut socket = open(&authority);
        socket.write_timeout = Duration::from_millis(200);
        let big = vec![1u8; 1024 * 1024];
        let start = Instant::now();
        let mut failed = None;
        for _ in 0..400 {
            if let Err(e) = socket.write_message(Message::Binary(big.clone().into())) {
                failed = Some(e);
                break;
            }
        }
        match failed {
            Some(SocketError::Failed(why)) => assert!(why.contains("not taking data"), "{why}"),
            other => panic!("expected a failed write, got {other:?}"),
        }
        assert!(start.elapsed() < Duration::from_secs(20));
        drop(socket);
        end_tx.send(()).unwrap();
        server.join().unwrap();
    }

    /// A node that closes in the middle of a frame, and one that resets the connection (it
    /// closes with the client's request unread), end `recv` with an error promptly and
    /// without spinning on the waiter.
    #[test]
    fn a_connection_that_ends_mid_frame_or_by_reset_is_an_error_not_a_hang() {
        // Closed in the middle of a frame.
        let bytes = frame_bytes(&numbered(5));
        let (authority, server) = node_that(move |mut ws| {
            ws.get_mut().write_all(&bytes[..6]).unwrap();
            ws.get_mut().flush().unwrap();
        });
        let mut socket = open(&authority);
        let counters = record(&mut socket);
        socket.set_read_timeout(Duration::from_secs(10)).unwrap();
        let start = Instant::now();
        let err = socket.recv().unwrap_err();
        assert!(!matches!(err, SocketError::Timeout), "{err:?}");
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(
            counters.calls.load(Ordering::SeqCst) < 50,
            "spun on the waiter"
        );
        drop(socket);
        server.join().unwrap();

        // Reset: the node closes with the client's request still unread.
        let (sent_tx, sent_rx) = channel::<()>();
        let (authority, server) = node_that(move |ws| {
            sent_rx.recv().unwrap();
            drop(ws);
        });
        let mut socket = open(&authority);
        let counters = record(&mut socket);
        socket.set_read_timeout(Duration::from_secs(10)).unwrap();
        socket
            .send(&every_request_kind().remove(2).1)
            .expect("the request fits the socket");
        sent_tx.send(()).unwrap();
        let start = Instant::now();
        let err = socket.recv().unwrap_err();
        assert!(!matches!(err, SocketError::Timeout), "{err:?}");
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(
            counters.calls.load(Ordering::SeqCst) < 50,
            "spun on the waiter"
        );
        drop(socket);
        server.join().unwrap();
    }

    /// A pong that tungstenite queued inside a read while the client's send buffer was full
    /// still goes out once the node reads again, though nothing more arrives from the node:
    /// the wait is for readability, so `recv` also retries the queued write itself.
    #[test]
    fn a_queued_pong_is_flushed_when_the_socket_becomes_writable() {
        let (filled_tx, filled_rx) = channel::<()>();
        let (go_tx, go_rx) = channel::<()>();
        let (authority, server) = node_that(move |mut ws| {
            filled_rx.recv().unwrap();
            ws.send(Message::Ping(Vec::new().into())).unwrap();
            go_rx.recv().unwrap();
            let mut pong = false;
            loop {
                match ws.read() {
                    Ok(Message::Pong(_)) => {
                        pong = true;
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            if pong {
                let ok: HostResult = Ok(HostResponse::Ok);
                let _ = ws.send(Message::Binary(bincode::serialize(&ok).unwrap().into()));
            }
            pong
        });
        let mut socket = open(&authority);
        // Fill the client's send buffer: the node reads nothing yet.
        let chunk = vec![0u8; 256 * 1024];
        let mut full = false;
        for _ in 0..2000 {
            match socket.ws.write(Message::Binary(chunk.clone().into())) {
                Ok(()) => {}
                Err(tungstenite::Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => {
                    full = true;
                    break;
                }
                Err(e) => panic!("{e}"),
            }
        }
        assert!(full, "the send buffer never filled");
        filled_tx.send(()).unwrap();
        // Read the ping by hand: the pong it queues cannot be written yet.
        let give_up = Instant::now() + Duration::from_secs(10);
        loop {
            match socket.ws.read() {
                Ok(Message::Ping(_)) => break,
                Ok(_) => {}
                Err(tungstenite::Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => {
                    assert!(Instant::now() < give_up, "no ping");
                    thread::sleep(Duration::from_millis(1));
                }
                Err(e) => panic!("{e}"),
            }
        }
        go_tx.send(()).unwrap();
        socket.set_read_timeout(Duration::from_secs(20)).unwrap();
        assert!(
            matches!(socket.recv(), Ok(Ok(HostResponse::Ok))),
            "the queued pong never went out"
        );
        drop(socket);
        assert!(server.join().unwrap());
    }

    /// The upgrade answer arriving in 5-byte pieces still completes the connection; one that
    /// never comes fails at the (shortened) handshake timeout instead of hanging.
    #[test]
    fn the_handshake_survives_split_segments_and_a_stalled_one_fails() {
        struct Split(TcpStream);
        impl Read for Split {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.0.read(buf)
            }
        }
        impl Write for Split {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                let n = buf.len().min(5);
                let k = self.0.write(&buf[..n])?;
                self.0.flush()?;
                thread::sleep(Duration::from_millis(1));
                Ok(k)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.0.flush()
            }
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(Split(stream)).unwrap();
            ws.send(Message::Binary(
                bincode::serialize(&numbered(9)).unwrap().into(),
            ))
            .unwrap();
            while ws.read().is_ok() {}
        });
        let mut socket = open(&authority);
        socket.set_read_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(text_of(&socket.recv().unwrap()), "frame 9");
        drop(socket);
        server.join().unwrap();

        // A node that accepts and never answers the upgrade.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        let (end_tx, end_rx) = channel::<()>();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            end_rx.recv().unwrap();
            drop(stream);
        });
        let start = Instant::now();
        let err = NodeSocket::connect_within(
            &authority,
            &GrantToken::new("t".into()),
            &WriteGate::default(),
            Duration::from_millis(300),
        )
        .err()
        .expect("a stalled upgrade must fail");
        assert!(matches!(err, SocketError::Failed(_)), "{err:?}");
        assert!(start.elapsed() < Duration::from_secs(5));
        end_tx.send(()).unwrap();
        server.join().unwrap();
    }

    /// Opening and dropping connections leaves no file descriptors behind (the socket and the
    /// poller's own descriptor are both closed). Other tests in this process open and close
    /// descriptors meanwhile, so the bound is loose: a leak of even one descriptor per cycle
    /// is 200.
    #[cfg(target_os = "linux")]
    #[test]
    fn connections_do_not_leak_descriptors() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        const CYCLES: usize = 200;
        let server = thread::spawn(move || {
            for _ in 0..=CYCLES {
                let (stream, _) = listener.accept().unwrap();
                let _ = tungstenite::accept(stream);
            }
        });
        let count = || std::fs::read_dir("/proc/self/fd").unwrap().count();
        drop(open(&authority));
        let warm = count();
        for _ in 0..CYCLES {
            drop(open(&authority));
        }
        let after = count();
        server.join().unwrap();
        assert!(
            after < warm + 150,
            "descriptors grew from {warm} to {after} over {CYCLES} connections"
        );
    }

    /// Frames sent at every offset around the moment the client's wait runs out are received
    /// exactly once, in order, and the connection is never dropped. The client waits 1 ms per
    /// `recv`; the node sends each frame after a delay that sweeps from 0 to 3 ms.
    ///
    /// This is a probabilistic check and it passes on the old receive-timeout code on Linux:
    /// the data-loss race it is aimed at is Windows-specific. It guards the new code only. The
    /// deterministic regression test is `a_wait_that_runs_out_as_data_arrives_loses_nothing`.
    #[test]
    fn frames_sent_as_the_wait_runs_out_are_never_lost_or_repeated() {
        const FRAMES: u32 = 2000;
        let (authority, server) = node_that(move |mut ws| {
            ws.get_ref().set_nodelay(true).unwrap();
            for i in 0..FRAMES {
                thread::sleep(Duration::from_micros(u64::from(i % 30) * 100));
                ws.send(Message::Binary(
                    bincode::serialize(&numbered(i)).unwrap().into(),
                ))
                .unwrap();
            }
            // Hold the connection open until the client has everything.
            while ws.read().is_ok() {}
        });
        let mut socket = open(&authority);
        socket.set_read_timeout(Duration::from_millis(1)).unwrap();
        let mut got: Vec<String> = Vec::new();
        let started = Instant::now();
        let give_up = started + Duration::from_secs(60);
        let mut timeouts = 0u32;
        while (got.len() as u32) < FRAMES {
            assert!(
                Instant::now() < give_up,
                "only {} of {FRAMES} after {:?}, {timeouts} timeouts",
                got.len(),
                started.elapsed()
            );
            match socket.recv() {
                Ok(r) => got.push(text_of(&r)),
                Err(SocketError::Timeout) => timeouts += 1,
                Err(e) => panic!(
                    "the connection was lost after {} frames ({:?}, {timeouts} timeouts): {e}",
                    got.len(),
                    started.elapsed()
                ),
            }
        }
        for (i, text) in got.iter().enumerate() {
            assert!(text.contains(&format!("frame {i}")), "frame {i} was {text}");
        }
        if timeouts == 0 {
            eprintln!("note: no wait ran out, so nothing was raced (a coarse-timer runner)");
        }
        drop(socket);
        server.join().unwrap();
    }

    /// A write while the node is not reading finishes once it does (the frame is kept and
    /// flushed again), and arrives once and whole.
    #[test]
    fn a_write_into_a_full_socket_is_kept_until_the_node_reads() {
        let (go_tx, go_rx) = channel::<()>();
        let (authority, server) = node_that(move |mut ws| {
            // Not reading until the client has been blocked on a full socket.
            go_rx.recv().unwrap();
            let mut sizes = Vec::new();
            while let Ok(m) = ws.read() {
                if let Message::Binary(b) = m {
                    sizes.push(b.len());
                }
            }
            sizes
        });
        let mut socket = open(&authority);
        let big = vec![7u8; 4 * 1024 * 1024];
        // The first writes cannot finish until the node reads, so release it from another
        // thread once the client has had time to block.
        let release = thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            go_tx.send(()).unwrap();
        });
        let sent = 6;
        for _ in 0..sent {
            socket
                .write_message(Message::Binary(big.clone().into()))
                .unwrap();
        }
        release.join().unwrap();
        drop(socket);
        let sizes = server.join().unwrap();
        assert_eq!(sizes, vec![big.len(); sent]);
    }

    #[test]
    fn pending_results_are_logged_first_then_by_tens_and_each_minute() {
        let logged: Vec<u64> = (1..=1100)
            .filter(|n| should_log_pending(*n, Some(Duration::from_secs(1))))
            .collect();
        assert_eq!(logged, [1, 10, 100, 1000]);
        assert!(should_log_pending(7, Some(Duration::from_secs(60))));
        assert!(!should_log_pending(7, Some(Duration::from_secs(59))));
        assert!(!should_log_pending(0, None));
    }

    /// Windows only: the real codes. 10035 and 10060 map to `WouldBlock` and `TimedOut` in
    /// std; 997 does not, and is a failure.
    #[cfg(windows)]
    #[test]
    fn windows_codes_for_nothing_yet() {
        assert_eq!(
            std::io::Error::from_raw_os_error(10035).kind(),
            ErrorKind::WouldBlock
        );
        assert_eq!(
            std::io::Error::from_raw_os_error(10060).kind(),
            ErrorKind::TimedOut
        );
        assert_eq!(map_ws_error(io(10035), true), SocketError::Timeout);
        assert_eq!(map_ws_error(io(10060), true), SocketError::Timeout);
        for reading in [true, false] {
            assert!(matches!(
                map_ws_error(io(997), reading),
                SocketError::Failed(_)
            ));
        }
    }

    /// The claim `map_ws_error` relies on: a read that ends in an error halfway through a frame,
    /// whatever the error, leaves tungstenite's buffered bytes and parsed header alone, so the
    /// next read returns the whole message. The error here is the pending code on any platform.
    #[test]
    fn a_frame_half_read_when_the_read_failed_is_finished_by_the_next_read() {
        use std::io::{Read, Write};
        struct Script {
            steps: std::collections::VecDeque<std::io::Result<Vec<u8>>>,
        }
        impl Read for Script {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                match self.steps.pop_front() {
                    Some(Ok(bytes)) => {
                        buf[..bytes.len()].copy_from_slice(&bytes);
                        Ok(bytes.len())
                    }
                    Some(Err(e)) => Err(e),
                    None => Err(std::io::ErrorKind::WouldBlock.into()),
                }
            }
        }
        impl Write for Script {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let payload = b"the whole message";
        let mut frame = vec![0x82, payload.len() as u8];
        frame.extend_from_slice(payload);
        let (head, tail) = frame.split_at(6);
        let mut ws = tungstenite::WebSocket::from_raw_socket(
            Script {
                steps: [
                    Ok(head.to_vec()),
                    Err(std::io::Error::from_raw_os_error(IO_PENDING)),
                    Ok(tail.to_vec()),
                ]
                .into(),
            },
            tungstenite::protocol::Role::Client,
            None,
        );
        let first = ws.read().unwrap_err();
        assert!(
            matches!(&first, tungstenite::Error::Io(e) if e.raw_os_error() == Some(IO_PENDING))
        );
        match ws.read().unwrap() {
            Message::Binary(b) => assert_eq!(&b[..], payload),
            other => panic!("{other:?}"),
        }
    }
}
