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
//! **Reading a predecessor.** As the web UI's `fetch_secrets` reads one, so a short export is
//! never taken for a complete one (freenet/river#592): the listing; the raw key index twice (the
//! two reads must agree, decode, and equal the listing: a frozen delegate's listing turns an
//! unreadable index into an empty one, and a failed read reports "not found",
//! freenet-core#5857); `rooms_meta` (stored but not indexed, or listed with no value, is a
//! fault); every room `rooms_meta` names that the index does not list (a stored one proves the
//! index short); then the room keys to carry (a listed one with no value is a read fault).
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
//! **A generation that can't be read stops the walk.** An error, no answer, or any fault above
//! ends the walk at that predecessor: it may hold a newer copy of any room (a rejoin under a new
//! identity) or the tombstone of one the user left, so NOTHING older may decide anything. What
//! newer generations decided, and what this one decided from its verified listing, is still
//! written; the rest waits for Try again. A slot that does not decode (or names another room)
//! holds its room back from older generations the same way. Only "not installed" (the node holds
//! no such delegate) is an empty store, passed over. Writing an older copy instead would be
//! permanent: the write never replaces what the current key holds, and the web UI reads a room
//! in the current delegate as authoritative.
//!
//! **Its rank is recorded.** For each room written, `rooms_meta.identity_sources` gets the
//! generation that supplied it, as the web UI's walk persists it, so the web UI's ranked merge
//! still lets a newer generation's copy win when that generation answers later.
//!
//! The room is written as [`NewRoom`], the fields this app already writes for a room it creates
//! or joins, which the `stored_rooms_drift` tests in `ui/src/room_data.rs` pin as decodable by
//! the web UI, plus the slot's `previous_contract_key` and `rekey_merge` when they read as the web
//! UI reads them (so a room-contract merge the web UI still owes is not dropped). Raw bytes from
//! an older web UI are not copied: a field whose shape changed since would be refused by the
//! current web UI's reader, and an unreadable slot stops its walk on every load
//! (`room_set_complete`). The room keeps the contract key it was stored with, so the web UI still
//! sees that the room contract moved and merges the old contract's state
//! (`regenerate_contract_key`). A slot is encoded when it is decided, so a room that would not
//! encode is held back and said, never a write that can't be made.
//!
//! Each write is the web UI's compare-and-swap, through [`super::work`], and only where the
//! current key still holds nothing; another writer's value, read at the write, wins.
//!
//! **What is left to the web UI.** This app never writes freenet-migrate's journal or its
//! markers, so a browser tab's journaled walk still runs as it would have, and merges over what
//! was carried here (the same rooms at the same ranks, so they converge). What only that walk
//! does: folding older generations' other fields into the winning copy (`own_purges`,
//! `invitation_secrets`, read markers), merging `rooms_meta`'s room order, the single-blob
//! `rooms_data` era, and the sent-message store (`outbound_dms`, with the archive list). For a
//! desktop-only user those stay in the earlier delegate (stranded, not lost): sent direct
//! messages from before the re-key do not show their text, and archived threads show again.
//!
//! **When.** Once per run, after the first read of the stored rooms; again with Try again. It is
//! a task like a create or join, one request at a time, behind the write gate; a task the user
//! asks for goes ahead of it at its next request. An earlier delegate that does not answer costs
//! one new connection (so its answer can't be taken for another request's) and stops the walk;
//! the whole walk stops at [`super::work::WALK_BUDGET`]. The node answers a delegate it does not
//! hold with a typed `Missing` (freenet-core 0.2.156 and later, which the app grant needs).

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::OnceLock;

use ciborium::Value;
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::client_api::{ClientRequest, DelegateRequest, HostResponse};
use freenet_stdlib::prelude::ContractKey;
use freenet_stdlib::prelude::{
    ApplicationMessage, CodeHash, DelegateKey, InboundDelegateMsg, OutboundDelegateMsg, Parameters,
};
use river_core::chat_delegate::{ChatDelegateKey, ChatDelegateRequestMsg, ChatDelegateResponseMsg};
use river_core::stored_rooms::{
    NewRoom, ROOMS_META_KEY, StoredRoom, StoredRoomSlot, decode_room_slot, decode_rooms_meta,
    encode_new_room, parse_room_storage_key, room_storage_key,
};
use zeroize::{Zeroize, Zeroizing};

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
        let mut list = parse_registry(REGISTRY_TOML).unwrap_or_else(|e| {
            eprintln!("River: {e}");
            Vec::new()
        });
        // Never this build's own delegate (a registry row committed before the WASM swap, or a
        // revert): its answers are the current store's, not an earlier one's.
        list.retain(|p| &p.key != super::delegate::chat_delegate_key());
        list
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
pub fn answers_from(
    response: &mut HostResponse,
) -> Option<(DelegateKey, Vec<ChatDelegateResponseMsg>)> {
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

/// The chat delegate's own key index, read as a plain key: every generation stores it under
/// this key, and a `GetRequest` reads its raw bytes (the web UI's `KEY_INDEX_STORAGE_KEY`).
pub const KEY_INDEX_KEY: &[u8] = b"::key_index";

/// The delegate's stored key index (the web UI's `RawKeyIndex`).
#[derive(serde::Deserialize)]
struct RawKeyIndex {
    keys: Vec<ChatDelegateKey>,
}

/// The web UI's `RekeyMerge` (`ui/src/room_data.rs`), every field defaulted as there: read only
/// to check a carried slot's value is one the web UI reads, before it is kept.
#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct RekeyMergeShape {
    #[serde(default)]
    owed: Vec<ContractKey>,
    #[serde(default)]
    previous_queued: Option<ContractKey>,
    #[serde(default)]
    first_owed_at: Option<u64>,
    #[serde(default)]
    unfinished_loads: u32,
}

/// Where the read of one predecessor stands. Its reads follow the web UI's `fetch_secrets`: the
/// listing, the raw index twice, `rooms_meta`, the rooms `rooms_meta` names but the index does
/// not list, then the room keys to carry.
enum Phase {
    List,
    /// The first read of the raw index; `listed` is what the listing said.
    Index {
        listed: Vec<Vec<u8>>,
    },
    /// The second read; `first` is the first read's bytes (`None`: no index stored).
    IndexAgain {
        listed: Vec<Vec<u8>>,
        first: Option<Vec<u8>>,
    },
    Meta {
        listed: Vec<Vec<u8>>,
        indexed: bool,
    },
    /// Room keys `rooms_meta` names that the index does not list: a stored one proves the
    /// index short. Then the room keys to read.
    Named {
        named: VecDeque<Vec<u8>>,
        rooms: VecDeque<Vec<u8>>,
    },
    Rooms(VecDeque<Vec<u8>>),
}

/// What decides a room key, as found.
enum Item {
    /// The slot to write (zeroed when dropped: it holds the user's signing key), and the room's
    /// owner, for `rooms_meta`'s identity sources.
    Room(Zeroizing<Vec<u8>>, Box<VerifyingKey>),
    Tombstone,
}

/// A room key decided by a predecessor.
struct Found {
    /// The predecessor's version and its delegate key.
    version: String,
    delegate: [u8; 32],
    item: Item,
}

/// The walk over the predecessors and what it found to carry. No `Debug`: it holds rooms.
pub struct Carry {
    /// Newest first.
    preds: Vec<Predecessor>,
    /// The one being read.
    at: usize,
    phase: Phase,
    /// The room keys the current store listed when the walk began: never carried.
    current: HashSet<Vec<u8>>,
    /// Room keys a predecessor decided (the newest that holds one), with what to write.
    found: BTreeMap<Vec<u8>, Found>,
    /// Room keys a predecessor holds in a form this app can't read: no older generation may
    /// decide them, so they are not carried (the web UI's walk may).
    held: BTreeMap<Vec<u8>, String>,
    /// The predecessor the walk stopped at, and why: it could not be read (an error, no answer,
    /// an index that does not add up). Nothing older is read, since that one may hold a newer
    /// copy of any room, or the tombstone of one the user left.
    pub stopped: Option<(String, String)>,
    /// After the walk: the writes still to make, in key order.
    writes: VecDeque<Vec<u8>>,
    /// The write being made.
    writing: Option<Vec<u8>>,
    /// The identity sources to add to `rooms_meta` for rooms written: (room, delegate key).
    sources: Vec<(VerifyingKey, [u8; 32])>,
    /// Predecessors the node does not hold.
    pub not_installed: usize,
    /// Rooms written.
    pub carried: usize,
    /// Tombstones written.
    pub tombstones: usize,
    /// Whether the room list was empty when the walk began.
    pub list_was_empty: bool,
    /// When the walk sent its first request ([`super::work::WALK_BUDGET`]).
    pub began: Option<std::time::Instant>,
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
            phase: Phase::List,
            current: current.into_iter().collect(),
            found: BTreeMap::new(),
            held: BTreeMap::new(),
            stopped: None,
            writes: VecDeque::new(),
            writing: None,
            sources: Vec::new(),
            not_installed: 0,
            carried: 0,
            tombstones: 0,
            list_was_empty,
            began: None,
        }
    }

    /// What to ask next, or `None` once the walk is over.
    pub fn next_ask(&self) -> Option<Ask> {
        if self.stopped.is_some() {
            return None;
        }
        let pred = self.preds.get(self.at)?;
        let key = pred.key.clone();
        Some(match &self.phase {
            Phase::List => Ask::List(key),
            Phase::Index { .. } | Phase::IndexAgain { .. } => Ask::Get(key, KEY_INDEX_KEY.to_vec()),
            Phase::Meta { .. } => Ask::Get(key, ROOMS_META_KEY.to_vec()),
            Phase::Named { named, rooms } => match named.front() {
                Some(k) => Ask::Get(key, k.clone()),
                None => Ask::Get(key, rooms.front()?.clone()),
            },
            Phase::Rooms(rooms) => Ask::Get(key, rooms.front()?.clone()),
        })
    }

    /// Whether the walk is over: every predecessor read, or stopped at one.
    pub fn walked(&self) -> bool {
        self.stopped.is_some() || self.at >= self.preds.len()
    }

    fn next_pred(&mut self) {
        self.at += 1;
        self.phase = Phase::List;
        if self.walked() {
            self.end_walk();
        }
    }

    fn end_walk(&mut self) {
        self.writes = self.found.keys().cloned().collect();
    }

    /// Stop the walk at the predecessor being read: it could not be read (`why`).
    pub fn stop(&mut self, why: String) {
        if self.stopped.is_some() || self.at >= self.preds.len() {
            return;
        }
        let version = self.preds[self.at].version.clone();
        eprintln!("River: an earlier River's storage ({version}) could not be read: {why}");
        self.stopped = Some((version, why));
        self.end_walk();
    }

    /// How many rooms (not tombstones) the walk found to carry and has not written yet.
    pub fn rooms_found(&self) -> usize {
        self.found
            .values()
            .filter(|f| matches!(f.item, Item::Room(..)))
            .count()
    }

    /// The room keys left behind: held (unreadable there) or, when the walk stopped, not known.
    pub fn held(&self) -> usize {
        self.held.len()
    }

    /// The rooms the current walk could not read, with why (for the screen).
    pub fn held_reasons(&self) -> impl Iterator<Item = &String> {
        self.held.values()
    }

    /// The room keys to read from the predecessor being read, given what it lists: room keys
    /// the current store does not hold and no newer predecessor decided or held.
    fn wanted(&self, listed: &[Vec<u8>]) -> VecDeque<Vec<u8>> {
        let mut out = VecDeque::new();
        for k in listed {
            if parse_room_storage_key(k).is_some()
                && !self.current.contains(k)
                && !self.found.contains_key(k)
                && !self.held.contains_key(k)
                && !out.contains(k)
            {
                out.push_back(k.clone());
            }
        }
        out
    }

    /// Go on to the room keys of the predecessor being read, or to the next one.
    fn read_rooms(&mut self, rooms: VecDeque<Vec<u8>>) {
        if rooms.is_empty() {
            self.next_pred();
        } else {
            self.phase = Phase::Rooms(rooms);
        }
    }

    /// The predecessor's answer to `ask`, the request out. True when it was that answer.
    pub fn on_answer(
        &mut self,
        ask: &Ask,
        from: &DelegateKey,
        answer: &ChatDelegateResponseMsg,
    ) -> bool {
        if ask.delegate() != from || self.next_ask().as_ref() != Some(ask) {
            return false;
        }
        let phase = std::mem::replace(&mut self.phase, Phase::List);
        match (phase, ask, answer) {
            (Phase::List, Ask::List(_), ChatDelegateResponseMsg::ListResponse { keys }) => {
                let listed = keys.iter().map(|k| k.as_bytes().to_vec()).collect();
                self.phase = Phase::Index { listed };
            }
            (phase, Ask::Get(_, key), ChatDelegateResponseMsg::GetResponse { key: k, value })
                if k.as_bytes() == key.as_slice() =>
            {
                self.on_value(phase, key, value.as_deref());
            }
            (phase, ..) => {
                self.phase = phase;
                return false;
            }
        }
        true
    }

    fn on_value(&mut self, phase: Phase, key: &[u8], value: Option<&[u8]>) {
        match phase {
            Phase::Index { listed } => {
                self.phase = Phase::IndexAgain {
                    listed,
                    first: value.map(<[u8]>::to_vec),
                };
            }
            Phase::IndexAgain { listed, first } => {
                if first.as_deref() != value {
                    return self.stop(
                        "two reads of its key index disagree, so one read failed".to_owned(),
                    );
                }
                let indexed: Vec<Vec<u8>> = match value {
                    None => Vec::new(),
                    Some(bytes) => match ciborium::de::from_reader::<RawKeyIndex, _>(bytes) {
                        Ok(index) => index.keys.iter().map(|k| k.as_bytes().to_vec()).collect(),
                        Err(e) => return self.stop(format!("its key index does not decode ({e})")),
                    },
                };
                if indexed != listed {
                    return self.stop(format!(
                        "it listed {} keys but its index holds {}",
                        listed.len(),
                        indexed.len()
                    ));
                }
                self.phase = Phase::Meta {
                    listed,
                    indexed: value.is_some(),
                };
            }
            Phase::Meta { listed, indexed } => {
                let meta_listed = listed.iter().any(|k| k.as_slice() == ROOMS_META_KEY);
                match (value, meta_listed) {
                    (Some(_), _) if !indexed => {
                        return self.stop("rooms_meta is stored but it has no key index".into());
                    }
                    (Some(_), false) => {
                        return self.stop("rooms_meta is stored but its index is short".into());
                    }
                    (None, true) => {
                        return self.stop("rooms_meta is listed but answered no value".into());
                    }
                    _ => {}
                }
                // The rooms `rooms_meta` names that the index does not list: a stored one
                // proves the index short (the web UI's check, freenet/river#592).
                let mut named = VecDeque::new();
                if let Some(meta) = value.and_then(|b| decode_rooms_meta(b).ok()) {
                    for vk in meta.room_order.iter().chain(meta.current_room_key.iter()) {
                        let k = room_storage_key(vk);
                        if !listed.contains(&k) && !named.contains(&k) {
                            named.push_back(k);
                        }
                    }
                }
                let rooms = self.wanted(&listed);
                if named.is_empty() {
                    self.read_rooms(rooms);
                } else {
                    self.phase = Phase::Named { named, rooms };
                }
            }
            Phase::Named { mut named, rooms } => {
                if value.is_some_and(|v| !v.is_empty()) {
                    return self.stop(
                        "a room its rooms_meta names is stored but its index does not list it"
                            .into(),
                    );
                }
                named.pop_front();
                if named.is_empty() {
                    self.read_rooms(rooms);
                } else {
                    self.phase = Phase::Named { named, rooms };
                }
            }
            Phase::Rooms(mut rooms) => {
                let Some(value) = value else {
                    // Listed, with no value: a read fault (River never deletes a stored key).
                    return self.stop("a room key it lists answered no value".into());
                };
                self.decide(key, value);
                rooms.pop_front();
                self.read_rooms(rooms);
            }
            Phase::List => {}
        }
    }

    /// A predecessor's slot for `key`: it decides the key, or holds it back.
    fn decide(&mut self, key: &[u8], value: &[u8]) {
        let pred = &self.preds[self.at];
        let version = pred.version.clone();
        let mut delegate = [0u8; 32];
        delegate.copy_from_slice(&pred.key.bytes()[..32]);
        let item = match decode_room_slot(value) {
            Ok(StoredRoomSlot::Present(room))
                if parse_room_storage_key(key) == Some(room.owner_vk) =>
            {
                let owner = room.owner_vk;
                match carried_slot(*room, value) {
                    Ok(bytes) => Item::Room(Zeroizing::new(bytes), Box::new(owner)),
                    Err(e) => {
                        self.held.insert(key.to_vec(), format!("{version}: {e}"));
                        return;
                    }
                }
            }
            Ok(StoredRoomSlot::Present(_)) => {
                self.held.insert(
                    key.to_vec(),
                    format!("{version}: a slot names another room"),
                );
                return;
            }
            Ok(StoredRoomSlot::Tombstone) => Item::Tombstone,
            Err(e) => {
                self.held.insert(key.to_vec(), format!("{version}: {e}"));
                return;
            }
        };
        self.found.insert(
            key.to_vec(),
            Found {
                version,
                delegate,
                item,
            },
        );
    }

    /// The node does not hold the predecessor being read (it has nothing). True when `key` is
    /// that one.
    pub fn not_installed(&mut self, key: &DelegateKey) -> bool {
        if self.stopped.is_some() || self.preds.get(self.at).map(|p| &p.key) != Some(key) {
            return false;
        }
        if !matches!(self.phase, Phase::List) {
            // It answered before: not installed now is a fault, not an empty store.
            self.stop("it went missing while being read".into());
            return true;
        }
        self.not_installed += 1;
        self.next_pred();
        true
    }

    /// The next write, `(room key, bytes)`, or `None` when all are made.
    pub fn write(&mut self) -> Option<(Vec<u8>, Vec<u8>)> {
        if self.writing.is_none() {
            self.writing = self.writes.pop_front();
        }
        let key = self.writing.clone()?;
        let bytes = match &self.found.get(&key)?.item {
            Item::Room(bytes, _) => bytes.to_vec(),
            Item::Tombstone => tombstone_bytes(),
        };
        Some((key, bytes))
    }

    /// The write being made ended: stored, or found something already there (which wins).
    pub fn written(&mut self, stored: bool) {
        if let Some(key) = self.writing.take()
            && let Some(found) = self.found.remove(&key)
        {
            match found.item {
                Item::Room(_, owner) if stored => {
                    self.carried += 1;
                    eprintln!("River: carried a room over from {}", found.version);
                    self.sources.push((*owner, found.delegate));
                }
                Item::Tombstone if stored => self.tombstones += 1,
                _ => {}
            }
        }
    }

    /// Whether writes remain.
    pub fn writes_left(&self) -> bool {
        self.writing.is_some() || !self.writes.is_empty()
    }

    /// The identity sources to add to `rooms_meta` for the rooms written, if any are owed.
    pub fn sources(&self) -> &[(VerifyingKey, [u8; 32])] {
        &self.sources
    }

    /// `rooms_meta`'s identity sources were written.
    pub fn sources_written(&mut self) {
        self.sources.clear();
    }

    /// Start again (Try again): read every predecessor again. What was written stays written
    /// (the writes check the key again anyway).
    pub fn restart(&mut self) {
        self.at = 0;
        self.phase = Phase::List;
        self.found.clear();
        self.held.clear();
        self.stopped = None;
        self.writes.clear();
        self.writing = None;
        self.not_installed = 0;
        self.began = None;
    }
}

fn tombstone_bytes() -> Vec<u8> {
    // A one-variant enum of no data always encodes.
    super::leave::tombstone_write(None)
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// What a carried room is written as: [`NewRoom`] (the fields this app writes for any room it
/// adds, which the drift tests pin as decodable by the web UI), plus the slot's
/// `previous_contract_key` and `rekey_merge` when they are in a shape the web UI reads, so a
/// room-contract merge the web UI still owes is not dropped. Other fields are not copied (see
/// the module docs).
fn carried_slot(room: StoredRoom, raw: &[u8]) -> Result<Vec<u8>, String> {
    let bytes = encode_new_room(&new_room(room))?;
    let Ok(Value::Map(mut outer)) = ciborium::de::from_reader::<Value, _>(bytes.as_slice()) else {
        return Err("a carried room does not re-encode".into());
    };
    let extra = kept_fields(raw);
    if let Some((_, Value::Map(fields))) = outer.first_mut() {
        fields.extend(extra);
    }
    let mut out = Vec::new();
    ciborium::ser::into_writer(&Value::Map(outer), &mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

/// The slot's `previous_contract_key` and `rekey_merge`, each only when it reads as the web UI
/// reads it.
fn kept_fields(raw: &[u8]) -> Vec<(Value, Value)> {
    let Ok(Value::Map(outer)) = ciborium::de::from_reader::<Value, _>(raw) else {
        return Vec::new();
    };
    let Some((_, Value::Map(fields))) = outer
        .into_iter()
        .find(|(k, _)| k.as_text() == Some("Present"))
    else {
        return Vec::new();
    };
    fields
        .into_iter()
        .filter(|(k, v)| match k.as_text() {
            Some("previous_contract_key") => v.deserialized::<Option<ContractKey>>().is_ok(),
            Some("rekey_merge") => v.deserialized::<RekeyMergeShape>().is_ok(),
            _ => false,
        })
        .collect()
}

/// The room as this app writes a room it adds.
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

/// `rooms_meta` (`current`, as a `GetVersionedResponse` gave it) with `sources` added to its
/// identity sources (the web UI's `RoomsMeta::identity_sources`: for a room whose identity an
/// earlier delegate supplied, that delegate's key, so the web UI's ranked merge still lets a
/// newer generation's copy win). An entry for the same room is replaced. `Ok(None)` when nothing
/// changes. Every other field is kept as CBOR; a value that is not a map, or an identity source
/// list in a shape the web UI never wrote, is an error and never overwritten.
pub fn meta_with_sources(
    current: Option<&[u8]>,
    sources: &[(VerifyingKey, [u8; 32])],
) -> Result<Option<Vec<u8>>, String> {
    if sources.is_empty() {
        return Ok(None);
    }
    let mut entries = match current.filter(|b| !b.is_empty()) {
        None => Vec::new(),
        Some(bytes) => match ciborium::de::from_reader::<Value, _>(bytes) {
            Ok(Value::Map(entries)) => entries,
            Ok(_) => return Err("rooms_meta: not a map".into()),
            Err(e) => return Err(format!("rooms_meta: {e}")),
        },
    };
    let field = match entries
        .iter()
        .position(|(k, _)| k.as_text() == Some("identity_sources"))
    {
        Some(i) => i,
        None => {
            entries.push((
                Value::Text("identity_sources".into()),
                Value::Array(Vec::new()),
            ));
            entries.len() - 1
        }
    };
    let Value::Array(list) = &mut entries[field].1 else {
        return Err("rooms_meta: identity_sources is not a list".into());
    };
    let mut changed = false;
    for (room, delegate) in sources {
        let entry = Value::serialized(&(room, delegate)).map_err(|e| format!("rooms_meta: {e}"))?;
        let names_room = |v: &Value| {
            v.deserialized::<(VerifyingKey, [u8; 32])>()
                .is_ok_and(|(r, _)| r == *room)
        };
        if list.contains(&entry) {
            continue;
        }
        list.retain(|v| !names_room(v));
        list.push(entry);
        changed = true;
    }
    if !changed {
        return Ok(None);
    }
    let mut out = Vec::new();
    ciborium::ser::into_writer(&Value::Map(entries), &mut out)
        .map_err(|e| format!("rooms_meta: {e}"))?;
    Ok(Some(out))
}

#[cfg(test)]
#[path = "carry_tests.rs"]
mod tests;
