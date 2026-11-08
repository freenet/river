//! A blocking WebSocket connection to the node's client API, in its native encoding: each
//! message is the bincode of a freenet-stdlib `ClientRequest` one way and of a
//! `Result<HostResponse, ClientError>` the other, at the stdlib version River's web UI uses.
//!
//! A large response may arrive in `StreamChunk`s (announced by a `StreamHeader` or not); they
//! are put back together here, so the caller only ever sees whole responses.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use freenet_stdlib::client_api::{ClientError, ClientRequest, HostResponse};
use tungstenite::client::IntoClientRequest;
use tungstenite::{Message, WebSocket};

use super::grant::GrantToken;

/// The node's client API, in native encoding.
pub const COMMAND_PATH: &str = "/v1/contract/command?encodingProtocol=native";

/// The most chunks one response may arrive in (freenet-stdlib's `MAX_TOTAL_CHUNKS`).
const MAX_CHUNKS: u32 = 256;

pub type HostResult = Result<HostResponse, ClientError>;

/// Why the connection could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SocketError {
    /// The node closed the connection with a code (`4401`, `4403`, `4503`, ...) and reason.
    Closed { code: Option<u16>, reason: String },
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
            SocketError::Timeout => f.write_str("Freenet stopped answering"),
            SocketError::Failed(e) => f.write_str(e),
        }
    }
}

pub struct NodeSocket {
    ws: WebSocket<TcpStream>,
    /// Chunks of responses still arriving, by stream id.
    chunks: HashMap<u32, Chunks>,
}

struct Chunks {
    total: u32,
    parts: Vec<Option<Vec<u8>>>,
    have: u32,
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
        stream.set_nodelay(true).map_err(|e| fail(&e))?;
        let mut request = format!("ws://{authority}{COMMAND_PATH}")
            .into_client_request()
            .map_err(|e| fail(&e))?;
        let bearer = format!("Bearer {}", token.expose())
            .parse()
            .map_err(|_| SocketError::Failed("the saved grant is not a valid header".into()))?;
        request.headers_mut().insert("Authorization", bearer);
        let (ws, _) = tungstenite::client(request, stream).map_err(|e| match e {
            tungstenite::HandshakeError::Failure(tungstenite::Error::Http(r)) => {
                SocketError::Closed {
                    code: Some(r.status().as_u16()),
                    reason: String::new(),
                }
            }
            other => fail(&other),
        })?;
        Ok(NodeSocket {
            ws,
            chunks: HashMap::new(),
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

    /// Check the connection is still there; a dead one fails the next read or write.
    pub fn ping(&mut self) -> Result<(), SocketError> {
        self.ws
            .send(Message::Ping(Vec::new().into()))
            .map_err(map_ws_error)
    }

    /// The next whole response.
    pub fn recv(&mut self) -> Result<HostResult, SocketError> {
        loop {
            let message = self.ws.read().map_err(map_ws_error)?;
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
            if let Some(whole) = self.take(&bytes)? {
                return Ok(whole);
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
        let entry = self.chunks.entry(stream_id).or_insert_with(|| Chunks {
            total,
            parts: vec![None; total as usize],
            have: 0,
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

/// What the node's close code says to do with the grant token
/// (freenet-core `app_grants.rs`, "WebSocket close codes for a token").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenVerdict {
    /// 4401: the token names nothing (revoked, or never issued). Discard it and ask again.
    Discard,
    /// 4403: this connection can't use a grant (not loopback, a proxy, a hosted node). Keep it.
    Unusable,
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
}
