//! Carrying the user's rooms over from earlier chat delegates (freenet/river#889).
//!
//! The chat delegate's key is a hash of its WASM, so every River release that changes that WASM
//! gives the user a new, empty store. The rooms stay in the older delegates on the node, which
//! the node keeps running for whoever registered them. The web UI carries them forward on every
//! load with freenet-migrate's journaled walk (`ui/src/components/app/freenet_api/
//! migration_journal.rs` and `delegate_migration.rs`); a user who only runs this app never runs
//! that walk, so the app carries them itself, here.
//!
//! **Which delegates.** The predecessors are the rows of `legacy_delegates.toml`, compiled in,
//! oldest first, as the web UI's `LEGACY_DELEGATES` is generated from it. Never from the network.
//! A predecessor is asked as the web UI asks one (`send_legacy_delegate_request`): its recorded
//! key and code hash, empty parameters, a CBOR [`ChatDelegateRequestMsg`]. This app never
//! registers one; a predecessor the node does not hold answers `DelegateError::Missing` and is
//! passed over.
//!
//! **What is carried.** The walk reads predecessors newest first. For each room key
//! (`room:<owner>`) the current store did not list, the NEWEST generation holding a slot for it
//! decides: a room is added, a tombstone (a room the user left) is written as a tombstone, so it
//! stays left. This is the web UI's ranked merge (`Rooms::merge_from_source`) for a room the
//! current delegate does not hold: a tombstone is recorded at its generation's rank and a present
//! copy survives it only from a strictly newer generation, and of two identities the strictly
//! newer one wins. A room the current store holds in any form, room or tombstone, is never
//! touched, as the web UI never lets an older generation replace the current one's copy.
//!
//! The room is written as [`NewRoom`], the fields this app already writes for a room it creates
//! or joins, which the `stored_rooms_drift` tests in `ui/src/room_data.rs` pin as decodable by
//! the web UI. Raw bytes from an older web UI are not copied: a field whose shape changed since
//! would be refused by the current web UI's reader, and an unreadable slot stops its walk on
//! every load (`room_set_complete`). The room keeps the contract key it was stored with, so the
//! web UI still sees that the room contract moved and merges the old contract's state
//! (`regenerate_contract_key`).
//!
//! Each write is the web UI's compare-and-swap, through [`super::work`], and only where the
//! current key still holds nothing; another writer's value, read at the write, wins.
//!
//! **What is left to the web UI.** This app never writes freenet-migrate's journal or its
//! markers, so a browser tab's journaled walk still runs as it would have, and merges over what
//! was carried here (the same rooms at the same ranks, so they converge). What only that walk
//! does: folding older generations' other fields into the winning copy (`own_purges`,
//! `invitation_secrets`, read markers), merging `rooms_meta`'s room order, the single-blob
//! `rooms_data` era, and the sent-message store (`outbound_dms`).
//!
//! **When.** Once per run, after the first read of the stored rooms; again with Try again.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::OnceLock;

use freenet_stdlib::client_api::{ClientRequest, DelegateRequest, HostResponse};
use freenet_stdlib::prelude::{
    ApplicationMessage, CodeHash, DelegateKey, InboundDelegateMsg, OutboundDelegateMsg,
    Parameters,
};
use river_core::chat_delegate::{ChatDelegateKey, ChatDelegateRequestMsg, ChatDelegateResponseMsg};
use river_core::stored_rooms::{
    NewRoom, StoredRoom, StoredRoomSlot, decode_room_slot, encode_new_room,
    parse_room_storage_key,
};
use zeroize::Zeroize;

/// River's registry of earlier chat delegates, as committed. CI checks each row
/// (`cargo make check-migration`).
const REGISTRY_TOML: &str = include_str!("../../../../legacy_delegates.toml");

/// One earlier chat delegate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Predecessor {
    /// Its version in the registry ("V39").
    pub version: String,
    pub key: DelegateKey,
}

#[derive(serde::Deserialize)]
struct Registry {
    entry: Vec<Row>,
}

#[derive(serde::Deserialize)]
struct Row {
    version: String,
    delegate_key: String,
    code_hash: String,
}

fn hex32(s: &str) -> Result<[u8; 32], String> {
    if s.len() != 64 {
        return Err(format!("{s:?} is not 64 hex digits"));
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16)
            .map_err(|_| format!("{s:?} is not hex"))?;
    }
    Ok(out)
}

/// The predecessors in a registry, NEWEST first (the registry lists them oldest first).
pub fn parse_registry(toml_text: &str) -> Result<Vec<Predecessor>, String> {
    let registry: Registry =
        toml::from_str(toml_text).map_err(|e| format!("legacy_delegates.toml: {e}"))?;
    let mut out = Vec::new();
    for row in registry.entry {
        let key = hex32(&row.delegate_key)?;
        let code_hash = hex32(&row.code_hash)?;
        out.push(Predecessor {
            version: row.version,
            key: DelegateKey::new(key, CodeHash::new(code_hash)),
        });
    }
    out.reverse();
    Ok(out)
}

/// River's earlier chat delegates, newest first.
pub fn predecessors() -> &'static [Predecessor] {
    static LIST: OnceLock<Vec<Predecessor>> = OnceLock::new();
    LIST.get_or_init(|| {
        // The registry is committed and checked by CI (and by a test here), so this does not
        // fail in a build; if it did, carrying nothing is the safe outcome.
        parse_registry(REGISTRY_TOML).unwrap_or_else(|e| {
            eprintln!("River: {e}");
            Vec::new()
        })
    })
}

/// A request to a predecessor, as the web UI sends one: its key, empty parameters, one CBOR
/// application message.
pub fn request(key: &DelegateKey, msg: &ChatDelegateRequestMsg) -> ClientRequest<'static> {
    let mut payload = Vec::new();
    ciborium::ser::into_writer(msg, &mut payload).expect("CBOR into a Vec cannot fail");
    ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages {
        key: key.clone(),
        params: Parameters::from(Vec::<u8>::new()),
        inbound: vec![InboundDelegateMsg::ApplicationMessage(
            ApplicationMessage::new(payload),
        )],
    })
}

/// The answers in a response from the delegate `key`, if the response is that delegate's. The
/// payloads are wiped once decoded: a room's slot holds the user's signing key for it.
pub fn answers_from(response: &mut HostResponse) -> Option<(DelegateKey, Vec<ChatDelegateResponseMsg>)> {
    let HostResponse::DelegateResponse { key, values } = response else {
        return None;
    };
    let mut out = Vec::new();
    for v in values.iter_mut() {
        if let OutboundDelegateMsg::ApplicationMessage(m) = v {
            if let Ok(answer) = ciborium::de::from_reader(m.payload.as_slice()) {
                out.push(answer);
            }
            m.payload.zeroize();
        }
    }
    Some((key.clone(), out))
}

/// What the walk asks next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ask {
    /// List the keys of this predecessor.
    List(DelegateKey),
    /// Read this key of this predecessor.
    Get(DelegateKey, Vec<u8>),
}

impl Ask {
    pub fn delegate(&self) -> &DelegateKey {
        match self {
            Ask::List(k) | Ask::Get(k, _) => k,
        }
    }

    pub fn to_request(&self) -> ClientRequest<'static> {
        match self {
            Ask::List(k) => request(k, &ChatDelegateRequestMsg::ListRequest),
            Ask::Get(k, key) => request(
                k,
                &ChatDelegateRequestMsg::GetRequest {
                    key: ChatDelegateKey::new(key.clone()),
                },
            ),
        }
    }
}

/// What a predecessor holds for a room key, as decided.
enum Found {
    /// A room, as it will be written. No `Debug`: it holds the user's signing key.
    Room(Box<NewRoom>),
    Tombstone,
}

/// The walk over the predecessors and what it found to carry. No `Debug`: it holds rooms.
pub struct Carry {
    /// Newest first.
    preds: Vec<Predecessor>,
    /// The one being read.
    at: usize,
    /// `None` until its key list arrived; then the room keys still to read from it.
    gets: Option<VecDeque<Vec<u8>>>,
    /// The room keys the current store listed when the walk began: never carried.
    current: HashSet<Vec<u8>>,
    /// Room keys a newer generation already decided (or that held nothing readable there and
    /// are left to an older one): by key, what will be written.
    found: BTreeMap<Vec<u8>, (String, Found)>,
    /// After the walk: the writes still to make, in key order.
    writes: VecDeque<Vec<u8>>,
    /// The write being made.
    writing: Option<Vec<u8>>,
    /// Predecessors the node does not hold.
    pub not_installed: usize,
    /// Predecessors that could not be read, with why.
    pub unread: Vec<(String, String)>,
    /// Rooms written (or found already there, written by another writer meanwhile).
    pub carried: usize,
    /// Tombstones written.
    pub tombstones: usize,
    /// Whether the room list was empty when the walk began, so the screen says what is going on.
    pub list_was_empty: bool,
}

impl Carry {
    /// A walk over `preds` (newest first), carrying no room key in `current` (the keys the
    /// current store listed, tombstones included).
    pub fn new(
        preds: Vec<Predecessor>,
        current: impl IntoIterator<Item = Vec<u8>>,
        list_was_empty: bool,
    ) -> Carry {
        Carry {
            preds,
            at: 0,
            gets: None,
            current: current.into_iter().collect(),
            found: BTreeMap::new(),
            writes: VecDeque::new(),
            writing: None,
            not_installed: 0,
            unread: Vec::new(),
            carried: 0,
            tombstones: 0,
            list_was_empty,
        }
    }

    /// What to ask next, or `None` once every predecessor was read.
    pub fn next_ask(&self) -> Option<Ask> {
        let pred = self.preds.get(self.at)?;
        Some(match &self.gets {
            None => Ask::List(pred.key.clone()),
            Some(gets) => Ask::Get(pred.key.clone(), gets.front()?.clone()),
        })
    }

    /// Whether every predecessor was read.
    pub fn walked(&self) -> bool {
        self.at >= self.preds.len()
    }

    fn next_pred(&mut self) {
        self.at += 1;
        self.gets = None;
        if self.walked() {
            self.writes = self.found.keys().cloned().collect();
        }
    }

    /// How many rooms (not tombstones) the walk found to carry.
    pub fn rooms_found(&self) -> usize {
        self.found
            .values()
            .filter(|(_, f)| matches!(f, Found::Room(_)))
            .count()
    }

    /// The predecessor's answer to `ask`, the request out. True when it was that answer.
    pub fn on_answer(&mut self, ask: &Ask, from: &DelegateKey, answer: &ChatDelegateResponseMsg) -> bool {
        if ask.delegate() != from || self.next_ask().as_ref() != Some(ask) {
            return false;
        }
        match (ask, answer) {
            (Ask::List(_), ChatDelegateResponseMsg::ListResponse { keys }) => {
                let mut gets = VecDeque::new();
                for k in keys {
                    let k = k.as_bytes();
                    if parse_room_storage_key(k).is_some()
                        && !self.current.contains(k)
                        && !self.found.contains_key(k)
                        && !gets.iter().any(|g: &Vec<u8>| g == k)
                    {
                        gets.push_back(k.to_vec());
                    }
                }
                if gets.is_empty() {
                    self.next_pred();
                } else {
                    self.gets = Some(gets);
                }
                true
            }
            (Ask::Get(_, key), ChatDelegateResponseMsg::GetResponse { key: k, value })
                if k.as_bytes() == key.as_slice() =>
            {
                let version = self.preds[self.at].version.clone();
                match value.as_deref().filter(|v| !v.is_empty()).map(decode_room_slot) {
                    Some(Ok(StoredRoomSlot::Present(room))) => {
                        // A slot under another room's key is not that room's: left alone.
                        if parse_room_storage_key(key) == Some(room.owner_vk) {
                            self.found
                                .insert(key.clone(), (version, Found::Room(Box::new(new_room(*room)))));
                        } else {
                            eprintln!("River: a room stored in {version} names another room; not carried");
                        }
                    }
                    Some(Ok(StoredRoomSlot::Tombstone)) => {
                        self.found.insert(key.clone(), (version, Found::Tombstone));
                    }
                    // Nothing there after all (listed, then gone), or a value this app can't
                    // read: an older generation may still hold the room.
                    None => {}
                    Some(Err(e)) => {
                        eprintln!("River: a room stored in {version} can't be read: {e}");
                    }
                }
                if let Some(gets) = &mut self.gets {
                    gets.pop_front();
                    if gets.is_empty() {
                        self.next_pred();
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// The node does not hold the predecessor being read. True when `key` is that one.
    pub fn not_installed(&mut self, key: &DelegateKey) -> bool {
        if self.preds.get(self.at).map(|p| &p.key) != Some(key) {
            return false;
        }
        self.not_installed += 1;
        self.next_pred();
        true
    }

    /// The predecessor being read could not be read (an error, or no answer): passed over, and
    /// said on screen when the walk ends. Rooms it already gave are kept.
    pub fn unreadable(&mut self, why: String) {
        if let Some(p) = self.preds.get(self.at) {
            self.unread.push((p.version.clone(), why));
        }
        self.next_pred();
    }

    /// The next write, `(room key, bytes)`, or `None` when all are made.
    pub fn write(&mut self) -> Option<(Vec<u8>, Vec<u8>)> {
        if self.writing.is_none() {
            self.writing = self.writes.pop_front();
        }
        let key = self.writing.clone()?;
        let bytes = match &self.found.get(&key)?.1 {
            Found::Room(room) => encode_new_room(room).ok()?,
            Found::Tombstone => super::leave::tombstone_write(None).ok()??,
        };
        Some((key, bytes))
    }

    /// The write being made ended: stored, or found something already there (which wins).
    pub fn written(&mut self, stored: bool) {
        if let Some(key) = self.writing.take()
            && let Some((_, found)) = self.found.remove(&key)
        {
            match found {
                Found::Room(_) if stored => self.carried += 1,
                Found::Tombstone if stored => self.tombstones += 1,
                _ => {}
            }
        }
    }

    /// Whether writes remain.
    pub fn writes_left(&self) -> bool {
        self.writing.is_some() || !self.writes.is_empty()
    }

    /// Start again (Try again): read every predecessor again, keeping what the current store
    /// listed then (the writes check the key again anyway).
    pub fn restart(&mut self) {
        self.at = 0;
        self.gets = None;
        self.found.clear();
        self.writes.clear();
        self.writing = None;
        self.not_installed = 0;
        self.unread.clear();
    }
}

/// The room as this app writes a room it adds (see the module docs for why not the bytes read).
fn new_room(room: StoredRoom) -> NewRoom {
    NewRoom {
        owner_vk: room.owner_vk,
        room_state: room.room_state,
        self_sk: room.self_sk,
        self_vk: room.self_vk,
        contract_key: room.contract_key,
        last_read_message_id: room.last_read_message_id,
        self_authorized_member: room.self_authorized_member,
        invite_chain: room.invite_chain,
        self_member_info: room.self_member_info,
        self_nickname: room.self_nickname,
        invitation_secrets: room.invitation_secrets,
    }
}

/// What a carry write stores, given what the current key holds (as a `GetVersionedResponse`
/// gave it): the bytes when the key holds nothing, `None` when it holds anything (which wins:
/// another writer got there first, or the user's own room or tombstone is there).
pub fn decide(current: Option<&[u8]>, bytes: &[u8]) -> Option<Vec<u8>> {
    match current {
        Some(v) if !v.is_empty() => None,
        _ => Some(bytes.to_vec()),
    }
}

#[cfg(test)]
#[path = "carry_tests.rs"]
mod tests;
