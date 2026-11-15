//! Whether this node holds rooms in an older River chat delegate, which River in the browser
//! has not yet moved forward to the delegate this app uses.
//!
//! The web UI moves rooms forward from older delegates only when the current delegate holds
//! none: if the current delegate has a room, the move is skipped for good
//! (`plan_load_from_keys` in `ui/src/components/app/freenet_api/response_handler.rs`). So a
//! room this app stored in an empty current delegate would hide every older room from River
//! in the browser. Before the app's first write into an empty current delegate, the session
//! asks each older delegate for its key list. If one holds rooms (or might: it is installed
//! but did not answer plainly), the app does not create or join rooms and says to open River
//! in the browser once, which moves the rooms forward.
//!
//! The older delegates are the ones `legacy_delegates.toml` at the repository root records,
//! the same file the web UI's build reads (`ui/build.rs`). It is compiled in here as text.

use std::collections::HashMap;
use std::sync::OnceLock;

use freenet_stdlib::client_api::{
    ClientError, ClientRequest, DelegateError, DelegateRequest, ErrorKind, HostResponse,
    RequestError,
};
use freenet_stdlib::prelude::{
    ApplicationMessage, CodeHash, DelegateKey, InboundDelegateMsg, OutboundDelegateMsg, Parameters,
};
use river_core::chat_delegate::{ChatDelegateRequestMsg, ChatDelegateResponseMsg};
use river_core::stored_rooms::parse_room_storage_key;

/// The registry of older chat delegates, as the web UI's build reads it.
const REGISTRY: &str = include_str!("../../../../legacy_delegates.toml");

/// The key the oldest delegates kept every room under, before rooms had a key each.
pub const ROOMS_BLOB_KEY: &[u8] = b"rooms_data";

/// Every older chat delegate's key, oldest first.
pub fn legacy_delegate_keys() -> &'static [DelegateKey] {
    static KEYS: OnceLock<Vec<DelegateKey>> = OnceLock::new();
    KEYS.get_or_init(|| parse_registry(REGISTRY).expect("legacy_delegates.toml is readable"))
}

fn hex32(s: &str) -> Result<[u8; 32], String> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{s:?} is not 64 hex digits"));
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

/// The `(delegate_key, code_hash)` of each `[[entry]]`, as delegate keys.
fn parse_registry(text: &str) -> Result<Vec<DelegateKey>, String> {
    let doc: toml::Table = text.parse().map_err(|e| format!("{e}"))?;
    let entries = doc
        .get("entry")
        .and_then(|v| v.as_array())
        .ok_or("no [[entry]] list")?;
    entries
        .iter()
        .map(|entry| {
            let field = |name: &str| {
                entry
                    .get(name)
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| format!("an entry has no {name}"))
                    .and_then(hex32)
            };
            Ok(DelegateKey::new(
                field("delegate_key")?,
                CodeHash::new(field("code_hash")?),
            ))
        })
        .collect()
}

/// The request that asks an older delegate for its key list.
pub fn list_request(key: &DelegateKey) -> ClientRequest<'static> {
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&ChatDelegateRequestMsg::ListRequest, &mut payload)
        .expect("CBOR into a Vec cannot fail");
    ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages {
        key: key.clone(),
        params: Parameters::from(Vec::<u8>::new()),
        inbound: vec![InboundDelegateMsg::ApplicationMessage(
            ApplicationMessage::new(payload),
        )],
    })
}

/// What the older delegates hold, as far as the check found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Legacy {
    /// None of them is on this node, or those that are hold no rooms.
    Clear,
    /// One holds rooms, or might: it is on this node and answered something other than a
    /// plain key list, or did not answer in time.
    Found,
}

/// What one older delegate said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Said {
    /// Not on this node.
    Missing,
    /// Its key list, with or without rooms.
    Listed { rooms: bool },
    /// On this node, but its answer can't be read as a key list.
    Unclear,
}

/// How many older delegates are asked at once. Every delegate request on a node shares one
/// admission queue, and a request refused there comes back naming no delegate; the web UI asks
/// in waves of 4 for the same reason (freenet/river#809).
pub const WAVE: usize = 4;

/// The answers to [`list_request`] sent to every older delegate, collected as they arrive.
/// Asked in waves of [`WAVE`]: [`Probe::next_requests`] gives what to send now.
#[derive(Debug, Default)]
pub struct Probe {
    said: HashMap<[u8; 32], Said>,
    /// Errors that name no older delegate. Nothing else is out while the probe runs, so each is
    /// one older delegate's answer.
    unnamed: usize,
    /// How many older delegates have been asked.
    sent: usize,
}

impl Probe {
    /// The requests to send now, keeping up to [`WAVE`] unanswered.
    pub fn next_requests(&mut self) -> Vec<ClientRequest<'static>> {
        let keys = legacy_delegate_keys();
        let until = (self.answered() + WAVE).min(keys.len());
        let out: Vec<ClientRequest<'static>> = keys
            .get(self.sent..until)
            .unwrap_or_default()
            .iter()
            .map(list_request)
            .collect();
        self.sent = self.sent.max(until);
        out
    }

    fn answered(&self) -> usize {
        self.said.len() + self.unnamed
    }

    /// Take one response or error. True when it was an answer to the probe.
    pub fn take(&mut self, response: &Result<HostResponse, ClientError>) -> bool {
        match response {
            Ok(HostResponse::DelegateResponse { key, values }) if is_legacy(key) => {
                let said = values
                    .iter()
                    .find_map(|v| match v {
                        OutboundDelegateMsg::ApplicationMessage(m) => {
                            ciborium::de::from_reader::<ChatDelegateResponseMsg, _>(
                                m.payload.as_slice(),
                            )
                            .ok()
                        }
                        _ => None,
                    })
                    .map_or(Said::Unclear, |answer| match answer {
                        ChatDelegateResponseMsg::ListResponse { keys } => Said::Listed {
                            rooms: keys.iter().any(|k| {
                                parse_room_storage_key(k.as_bytes()).is_some()
                                    || k.as_bytes() == ROOMS_BLOB_KEY
                            }),
                        },
                        _ => Said::Unclear,
                    });
                self.said.insert(*key.bytes_array(), said);
                true
            }
            Ok(_) => false,
            Err(e) => {
                match e.kind() {
                    ErrorKind::RequestError(RequestError::DelegateError(
                        DelegateError::Missing(key),
                    )) if is_legacy(key) => {
                        self.said.insert(*key.bytes_array(), Said::Missing);
                    }
                    // Installed, but its answer was an error (a delegate this old may not
                    // run any more). It may still hold rooms.
                    _ => self.unnamed += 1,
                }
                true
            }
        }
    }

    /// Whether every older delegate has answered.
    pub fn complete(&self) -> bool {
        self.answered() >= legacy_delegate_keys().len()
    }

    /// The finding so far. Only [`Legacy::Clear`] once every older delegate has answered and
    /// none holds rooms or answered unclearly.
    pub fn finding(&self) -> Legacy {
        let worrying = self.unnamed > 0
            || self
                .said
                .values()
                .any(|s| matches!(s, Said::Listed { rooms: true } | Said::Unclear));
        if worrying || !self.complete() {
            Legacy::Found
        } else {
            Legacy::Clear
        }
    }
}

fn is_legacy(key: &DelegateKey) -> bool {
    legacy_delegate_keys()
        .iter()
        .any(|k| k.bytes() == key.bytes())
}

/// Whether an error is an older delegate's: the probe's, arriving after it gave up waiting.
pub fn names_a_legacy_delegate(e: &ClientError) -> bool {
    matches!(
        e.kind(),
        ErrorKind::RequestError(RequestError::DelegateError(DelegateError::Missing(key)))
            if is_legacy(key)
    )
}

trait BytesArray {
    fn bytes_array(&self) -> &[u8; 32];
}

impl BytesArray for DelegateKey {
    fn bytes_array(&self) -> &[u8; 32] {
        self.bytes().try_into().expect("a delegate key is 32 bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use river_core::chat_delegate::ChatDelegateKey;

    fn listed(key: &DelegateKey, keys: &[&[u8]]) -> Result<HostResponse, ClientError> {
        let msg = ChatDelegateResponseMsg::ListResponse {
            keys: keys
                .iter()
                .map(|k| ChatDelegateKey::new(k.to_vec()))
                .collect(),
        };
        let mut payload = Vec::new();
        ciborium::ser::into_writer(&msg, &mut payload).unwrap();
        Ok(HostResponse::DelegateResponse {
            key: key.clone(),
            values: vec![OutboundDelegateMsg::ApplicationMessage(
                ApplicationMessage::new(payload),
            )],
        })
    }

    fn missing(key: &DelegateKey) -> Result<HostResponse, ClientError> {
        Err(
            ErrorKind::RequestError(RequestError::DelegateError(DelegateError::Missing(
                key.clone(),
            )))
            .into(),
        )
    }

    /// Every entry of the registry the web UI builds from is read, with its recorded key and
    /// code hash. The key is the recorded one, not one derived here: V1's is irregular.
    #[test]
    fn the_registry_is_the_web_uis() {
        let keys = legacy_delegate_keys();
        assert_eq!(keys.len(), REGISTRY.matches("[[entry]]").count());
        assert!(keys.len() >= 35);
        for (key, line) in keys.iter().zip(
            REGISTRY
                .lines()
                .filter(|l| l.starts_with("delegate_key = ")),
        ) {
            assert_eq!(line, format!("delegate_key = \"{}\"", hex(key.bytes())));
        }
        // Entries other than the irregular V1 key are BLAKE3 of their code hash.
        for key in &keys[1..] {
            let code_hash: &[u8; 32] = key.code_hash().as_ref().try_into().unwrap();
            assert_eq!(blake3::hash(code_hash).as_bytes(), key.bytes());
        }
        // The current delegate is not among them.
        assert!(!is_legacy(crate::rooms::delegate::chat_delegate_key()));
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// The key each request asks.
    fn asked(requests: &[ClientRequest<'static>]) -> Vec<DelegateKey> {
        requests
            .iter()
            .map(|r| match r {
                ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages { key, .. }) => {
                    key.clone()
                }
                other => panic!("not a delegate request: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn a_node_that_never_ran_river_is_clear_and_is_asked_in_waves() {
        let mut probe = Probe::default();
        let mut all = Vec::new();
        let mut out = asked(&probe.next_requests());
        assert_eq!(out.len(), WAVE);
        while let Some(key) = out.first().cloned() {
            assert_eq!(probe.finding(), Legacy::Found, "not every one has answered");
            assert!(probe.take(&missing(&key)));
            out.remove(0);
            all.push(key);
            out.extend(asked(&probe.next_requests()));
            assert!(out.len() <= WAVE);
        }
        assert_eq!(all, legacy_delegate_keys());
        assert!(probe.complete());
        assert_eq!(probe.finding(), Legacy::Clear);
        assert!(probe.next_requests().is_empty());
    }

    #[test]
    fn an_older_delegate_with_rooms_or_an_unclear_answer_is_found() {
        let keys = legacy_delegate_keys();
        let room_key = river_core::stored_rooms::room_storage_key(
            &ed25519_dalek::SigningKey::from_bytes(&[3; 32]).verifying_key(),
        );
        let cases: Vec<(&str, Result<HostResponse, ClientError>, Legacy)> = vec![
            (
                "no rooms",
                listed(&keys[4], &[b"rooms_meta"]),
                Legacy::Clear,
            ),
            ("nothing at all", listed(&keys[4], &[]), Legacy::Clear),
            ("a room key", listed(&keys[4], &[&room_key]), Legacy::Found),
            (
                "the old blob",
                listed(&keys[0], &[b"rooms_data"]),
                Legacy::Found,
            ),
            (
                "an error",
                Err(ErrorKind::Unhandled {
                    cause: "the delegate trapped".into(),
                }
                .into()),
                Legacy::Found,
            ),
            (
                "not a list",
                Ok(HostResponse::DelegateResponse {
                    key: keys[4].clone(),
                    values: vec![],
                }),
                Legacy::Found,
            ),
        ];
        for (what, answer, expected) in cases {
            let mut probe = Probe::default();
            assert!(probe.take(&answer), "{what}");
            for key in keys {
                if !probe.complete() && !probe.said.contains_key(key.bytes_array()) {
                    probe.take(&missing(key));
                }
            }
            assert!(probe.complete(), "{what}");
            assert_eq!(probe.finding(), expected, "{what}");
        }
    }

    #[test]
    fn answers_of_other_delegates_are_not_the_probes() {
        let mut probe = Probe::default();
        let current = crate::rooms::delegate::chat_delegate_key();
        assert!(!probe.take(&listed(current, &[b"rooms_data"])));
        assert!(!names_a_legacy_delegate(&missing(current).unwrap_err()));
        assert!(names_a_legacy_delegate(
            &missing(&legacy_delegate_keys()[2]).unwrap_err()
        ));
        assert!(!probe.complete());
    }

    #[test]
    fn requests_ask_the_older_delegate_for_its_list() {
        let key = &legacy_delegate_keys()[7];
        let ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages {
            key: k, inbound, ..
        }) = list_request(key)
        else {
            panic!("not a delegate request");
        };
        assert_eq!(&k, key);
        let [InboundDelegateMsg::ApplicationMessage(m)] = inbound.as_slice() else {
            panic!("one message");
        };
        assert!(matches!(
            ciborium::de::from_reader(m.payload.as_slice()).unwrap(),
            ChatDelegateRequestMsg::ListRequest
        ));
    }
}
