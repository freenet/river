//! A blocking WebSocket connection to the node's client API, in its native encoding: each
//! message is the bincode of a freenet-stdlib `ClientRequest` one way and of a
//! `Result<HostResponse, ClientError>` the other, at the stdlib version River's web UI uses.
//!
//! A large response may arrive in `StreamChunk`s (announced by a `StreamHeader` or not); they
//! are put back together here, so the caller only ever sees whole responses.
//!
//! When the node rejects the grant token, it first sends an error whose text starts with
//! `AUTH_TOKEN_INVALID`, `AUTH_TOKEN_NOT_USABLE_HERE` or `AUTH_TOKEN_UNAVAILABLE`, and then
//! closes with 4401, 4403 or 4503 (freenet-core `client_events/websocket.rs`). Either one is
//! reported as the matching [`SocketError::Closed`], so the caller decides on the token the
//! same way whichever arrives first.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use freenet_stdlib::client_api::{
    ClientError, ClientRequest, ErrorKind as NodeErrorKind, HostResponse,
};
use tungstenite::client::IntoClientRequest;
use tungstenite::{Message, WebSocket};

use super::grant::GrantToken;

/// The node's client API, in native encoding.
pub const COMMAND_PATH: &str = "/v1/contract/command?encodingProtocol=native";

/// The most chunks one response may arrive in (freenet-stdlib's `MAX_TOTAL_CHUNKS`).
const MAX_CHUNKS: u32 = 256;

/// The most responses reassembled at once, and how long an unfinished one is kept (the node's
/// own limits for requests it reassembles: 8 streams, 60 seconds).
const MAX_STREAMS: usize = 8;
const STREAM_TTL: Duration = Duration::from_secs(60);

/// How long a write may block before the connection counts as gone.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

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
    /// Nothing arrived within the read timeout. The connection is still usable.
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

pub struct NodeSocket {
    ws: WebSocket<TcpStream>,
    /// Chunks of responses still arriving, by stream id.
    chunks: HashMap<u32, Chunks>,
    /// When anything (a response, a pong) last arrived.
    last_heard: Instant,
}

struct Chunks {
    total: u32,
    parts: Vec<Option<Vec<u8>>>,
    have: u32,
    started: Instant,
}

impl NodeSocket {
    /// Open `ws://<authority><COMMAND_PATH>` presenting `token` as a bearer grant. `authority`
    /// is `host:port` and must be a loopback address: the node honours a grant only there.
    pub fn connect(authority: &str, token: &GrantToken) -> Result<NodeSocket, SocketError> {
        let fail = |e: &dyn std::fmt::Display| SocketError::Failed(e.to_string());
        let addr = authority
            .to_socket_addrs()
            .map_err(|e| fail(&e))?
            .next()
            .ok_or_else(|| SocketError::Failed(format!("no address for {authority}")))?;
        let stream =
            TcpStream::connect_timeout(&addr, Duration::from_secs(5)).map_err(|e| fail(&e))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| fail(&e))?;
        stream
            .set_write_timeout(Some(WRITE_TIMEOUT))
            .map_err(|e| fail(&e))?;
        stream.set_nodelay(true).map_err(|e| fail(&e))?;
        let mut request = format!("ws://{authority}{COMMAND_PATH}")
            .into_client_request()
            .map_err(|e| fail(&e))?;
        let mut bearer: tungstenite::http::HeaderValue = format!("Bearer {}", token.expose())
            .parse()
            .map_err(|_| SocketError::Failed("the saved grant is not a valid header".into()))?;
        bearer.set_sensitive(true);
        request.headers_mut().insert("Authorization", bearer);
        let (ws, _) = tungstenite::client(request, stream).map_err(|e| match e {
            tungstenite::HandshakeError::Failure(tungstenite::Error::Http(r)) => {
                SocketError::Handshake(r.status().as_u16())
            }
            other => fail(&other),
        })?;
        Ok(NodeSocket {
            ws,
            chunks: HashMap::new(),
            last_heard: Instant::now(),
        })
    }

    /// How long [`recv`](Self::recv) waits before reporting [`SocketError::Timeout`].
    pub fn set_read_timeout(&mut self, timeout: Duration) -> Result<(), SocketError> {
        self.ws
            .get_mut()
            .set_read_timeout(Some(timeout))
            .map_err(|e| SocketError::Failed(e.to_string()))
    }

    pub fn send(&mut self, request: &ClientRequest<'_>) -> Result<(), SocketError> {
        let bytes = bincode::serialize(request).map_err(|e| SocketError::Failed(e.to_string()))?;
        self.ws
            .send(Message::Binary(bytes.into()))
            .map_err(map_ws_error)
    }

    /// Ask the node to show it is still there. The pong, like any message, moves
    /// [`last_heard`](Self::last_heard).
    pub fn ping(&mut self) -> Result<(), SocketError> {
        self.ws
            .send(Message::Ping(Vec::new().into()))
            .map_err(map_ws_error)
    }

    /// When anything last arrived from the node, pongs included.
    pub fn last_heard(&self) -> Instant {
        self.last_heard
    }

    /// The next whole response.
    pub fn recv(&mut self) -> Result<HostResult, SocketError> {
        loop {
            let message = self.ws.read().map_err(map_ws_error)?;
            self.last_heard = Instant::now();
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
                _ => continue,
            };
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
            started: Instant::now(),
        });
        if entry.total != total {
            return Err(SocketError::Failed("Freenet sent a malformed chunk".into()));
        }
        let slot = &mut entry.parts[index as usize];
        if slot.is_none() {
            *slot = Some(data.to_vec());
            entry.have += 1;
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

fn map_ws_error(e: tungstenite::Error) -> SocketError {
    match e {
        tungstenite::Error::Io(io)
            if matches!(io.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
        {
            SocketError::Timeout
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
        let mut socket = NodeSocket::connect(&authority, &GrantToken::new("t0k".into())).unwrap();
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
        let mut socket = NodeSocket::connect(&authority, &GrantToken::new("t".into())).unwrap();
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
            let mut socket = NodeSocket::connect(&authority, &GrantToken::new("t".into())).unwrap();
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
            let mut socket = NodeSocket::connect(&authority, &GrantToken::new("t".into())).unwrap();
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
            let mut socket = NodeSocket::connect(&authority, &GrantToken::new("t".into())).unwrap();
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
            let mut buf = [0u8; 4096];
            let _ = conn.read(&mut buf).unwrap();
            conn.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });
        let err = match NodeSocket::connect(&authority, &GrantToken::new("t".into())) {
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
        let mut socket = NodeSocket::connect(&authority, &GrantToken::new("t".into())).unwrap();
        assert!(matches!(socket.recv(), Ok(Ok(HostResponse::Ok))));
        assert_eq!(socket.streams_in_progress(), MAX_STREAMS);
        drop(socket);
        server.join().unwrap();
    }

    #[test]
    fn a_pong_counts_as_hearing_from_the_node() {
        let (authority, server) = serve(Vec::new());
        let mut socket = NodeSocket::connect(&authority, &GrantToken::new("t".into())).unwrap();
        let before = socket.last_heard();
        thread::sleep(Duration::from_millis(20));
        socket.ping().unwrap();
        socket.set_read_timeout(Duration::from_millis(300)).unwrap();
        assert_eq!(socket.recv().unwrap_err(), SocketError::Timeout);
        assert!(socket.last_heard() > before);
        drop(socket);
        server.join().unwrap();
    }
}
