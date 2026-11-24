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
//! fault); every room `rooms_meta` names that the index does not list (`rooms_named_by_meta`:
//! its order, notification modes, open room and identity sources; a stored one proves the index
//! short); the fixed keys `rooms_data` and `outbound_dms`, read even when unlisted
//! (`candidate_keys`: stored but unlisted proves the index short); then the room keys to carry
//! (a listed one with no value is a read fault).
//!
//! **What is carried, at what rank.** As the web UI's ranked merge (`Rooms::merge_from_source`
//! at each generation's lineage rank): for each room key (`room:<owner>`) the current store did
//! not list, the copy with the highest RANK decides. A copy's rank is its generation's, LOWERED
//! to the older generation the predecessor's own `rooms_meta.identity_sources` records for the
//! room's identity, when it has one (`identity_source_ranks`, applied by `flush_predecessor`'s
//! `lower_identity_ranks`): a copy that V40 carried from V38 ranks 38, so a rejoin V39 holds
//! still wins. So a predecessor older than the one that holds a copy is still read for that
//! room while it could outrank it. A tombstone wins over a present copy, and a present copy over
//! a tombstone, only from a strictly higher rank; of two identities the higher rank wins. A
//! tombstone is written as a tombstone, so a room the user left stays left. A room the current
//! store holds in any form is never touched, as the web UI never lets an older generation
//! replace the current one's copy.
//!
//! **A generation that can't be read stops the walk.** An error, no answer, or any fault above
//! ends the walk at that predecessor: it may hold a newer copy of any room (a rejoin under a new
//! identity) or the tombstone of one the user left, so nothing it or an older generation could
//! outrank is decided. A room it proves it holds (named by its `rooms_meta`, stored but left out
//! of its short index) is carried first, as the web UI salvages it (`salvage_short_index_rooms`).
//! What is decided at a rank no unread generation can outrank is still written; the rest waits
//! for Try again. A slot that does not decode (or names another room) holds its room back from
//! older generations the same way. Writing an older copy instead would be permanent: the write
//! never replaces what the current key holds, and the web UI reads a room in the current
//! delegate as authoritative. "Not installed" (a typed `Missing`: the node holds no such
//! delegate) is an empty store and is passed over; the web UI keeps such a predecessor pending,
//! since a node can unregister a delegate without deleting its store, which River never does.
//!
//! **Residual: a permanently unreadable generation.** A generation that can never be read (a
//! frozen short index, an old WASM that traps on the current runtime) stops the walk on every
//! start, after what it proves it holds is carried. A desktop-only user then does not get the
//! rooms only that generation and older ones hold until the web UI runs on the node once: its
//! journaled walk carries older generations past a faulty one under ranks and corrects them
//! later, which needs its room merge (`merge_from_source`); this app does not carry past it.
//! The failure row says to open River in the browser once, then Try again.
//!
//! **Its rank is recorded, before the room.** For each room written, `rooms_meta`'s
//! `identity_sources` gets the generation its rank is (as the web UI persists ranks below the
//! current one, `persisted_identity_sources`), so the web UI's ranked merge still lets a newer
//! generation's copy win when that generation answers later. The rank is written BEFORE the
//! room's slot, so a quit between the two can't leave a carried room unranked (which the web UI
//! would read as authoritative for good); when another writer stored the room first, the rank is
//! taken out again. A rank write that can't be made fails the carry with Try again, before the
//! slot.
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

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
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
    NewRoom, ROOMS_META_KEY, StoredRoom, StoredRoomSlot, decode_room_slot, encode_new_room,
    parse_room_storage_key, room_storage_key,
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
    /// Its place in the registry, oldest first (V1 is 0): the web UI's lineage index, the rank
    /// its merge gives what this generation supplies (`river_delegate_lineage`).
    pub rank: u32,
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
    for (rank, row) in registry.entry.into_iter().enumerate() {
        let key = hex32(&row.delegate_key)?;
        let code_hash = hex32(&row.code_hash)?;
        out.push(Predecessor {
            version: row.version,
            key: DelegateKey::new(key, CodeHash::new(code_hash)),
            rank: rank as u32,
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

/// The web UI's `RoomsMeta` (`ui/src/room_data.rs`), the fields a walk reads, every one
/// defaulted as there.
#[derive(serde::Deserialize)]
struct MetaShape {
    #[serde(default)]
    current_room_key: Option<VerifyingKey>,
    #[serde(default)]
    notification_modes: HashMap<VerifyingKey, Value>,
    #[serde(default)]
    room_order: Vec<VerifyingKey>,
    #[serde(default)]
    identity_sources: Vec<(VerifyingKey, [u8; 32])>,
}

impl MetaShape {
    /// Every room it names (the web UI's `rooms_named_by_meta`): its order, its notification
    /// modes, its cursor and its identity provenance.
    fn named(&self) -> Vec<VerifyingKey> {
        let mut out: Vec<VerifyingKey> = Vec::new();
        for vk in self
            .room_order
            .iter()
            .chain(self.notification_modes.keys())
            .chain(self.current_room_key.iter())
            .chain(self.identity_sources.iter().map(|(vk, _)| vk))
        {
            if !out.contains(vk) {
                out.push(*vk);
            }
        }
        out
    }
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
/// listing, the raw index twice, `rooms_meta`, every room `rooms_meta` names that the index does
/// not list (`rooms_named_by_meta`), the fixed keys `rooms_data` and `outbound_dms`
/// (`candidate_keys`), then the room keys to carry.
enum Phase {
    List,
    /// The first read of the raw index.
    Index,
    /// The second read; `first` is the first read's bytes (`None`: no index stored).
    IndexAgain {
        first: Option<Vec<u8>>,
    },
    Meta,
    /// Named rooms not listed, still to read; `short` once one was found stored.
    Named {
        named: VecDeque<Vec<u8>>,
        short: bool,
    },
    Fixed(VecDeque<Vec<u8>>),
    Rooms(VecDeque<Vec<u8>>),
}

/// The single-blob era's key and the sent-message store: probed even when unlisted, as the web
/// UI's `candidate_keys` probes them (`ROOMS_STORAGE_KEY`, `OUTBOUND_DMS_STORAGE_KEY`).
const FIXED_KEYS: [&[u8]; 2] = [b"rooms_data", b"outbound_dms"];

/// One write of a carry, to the current chat delegate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Write {
    /// Add (room, delegate key) to `rooms_meta`'s identity sources.
    AddRank(VerifyingKey, [u8; 32]),
    /// Store this slot under this room key, where it holds nothing.
    Slot(Vec<u8>, Vec<u8>),
    /// Take the room's identity source out again: another writer stored the room first.
    RemoveRank(VerifyingKey),
}

impl Write {
    /// The delegate key written.
    pub fn key(&self) -> Vec<u8> {
        match self {
            Write::Slot(key, _) => key.clone(),
            Write::AddRank(..) | Write::RemoveRank(_) => ROOMS_META_KEY.to_vec(),
        }
    }

    /// What to store, given what the key holds now (as a `GetVersionedResponse` gave it):
    /// bytes, nothing (already as wanted), or why not.
    pub fn decide(&self, current: Option<&[u8]>) -> Result<Option<Vec<u8>>, String> {
        match self {
            Write::Slot(_, bytes) => Ok(decide(current, bytes)),
            Write::AddRank(room, delegate) => meta_with_sources(current, &[(*room, *delegate)]),
            Write::RemoveRank(room) => meta_without_source(current, room),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WriteStep {
    Rank,
    Slot,
    Unrank,
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
    /// The version of the predecessor it was read from.
    version: String,
    /// Its rank: the generation that supplied it, which is the predecessor's own unless that
    /// predecessor's `rooms_meta` records an older one for the room's identity (the web UI's
    /// `identity_source_ranks` and `lower_identity_ranks`); and that generation's delegate key.
    rank: u32,
    delegate: [u8; 32],
    /// The rank of the predecessor it was read from.
    read_at: u32,
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
    /// Room keys left undecided, with why: a predecessor holds them in a form this app can't
    /// read, or one that could not be read could hold a newer copy. Nothing older decides them,
    /// so they are not carried (the web UI's walk may).
    held: BTreeMap<Vec<u8>, String>,
    /// What the predecessor being read listed, whether it has a key index, and the older ranks
    /// its `rooms_meta` records for rooms' identities (by room).
    listed: Vec<Vec<u8>>,
    indexed: bool,
    provenance: HashMap<[u8; 32], u32>,
    /// The predecessor the walk stopped at, and why: it could not be read (an error, no answer,
    /// an index that does not add up). Nothing older is read, since that one may hold a newer
    /// copy of any room, or the tombstone of one the user left.
    pub stopped: Option<(String, String)>,
    /// After the walk: the writes still to make, in key order.
    writes: VecDeque<Vec<u8>>,
    /// The write being made.
    writing: Option<Vec<u8>>,
    /// Where the write of `writing` stands.
    write_step: WriteStep,
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
            listed: Vec::new(),
            indexed: false,
            provenance: HashMap::new(),
            stopped: None,
            writes: VecDeque::new(),
            writing: None,
            write_step: WriteStep::Rank,
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
            Phase::Index | Phase::IndexAgain { .. } => Ask::Get(key, KEY_INDEX_KEY.to_vec()),
            Phase::Meta => Ask::Get(key, ROOMS_META_KEY.to_vec()),
            Phase::Named { named, .. } => Ask::Get(key, named.front()?.clone()),
            Phase::Fixed(keys) | Phase::Rooms(keys) => Ask::Get(key, keys.front()?.clone()),
        })
    }

    /// Whether the walk is over: every predecessor read, or stopped at one.
    pub fn walked(&self) -> bool {
        self.stopped.is_some() || self.at >= self.preds.len()
    }

    fn next_pred(&mut self) {
        self.at += 1;
        self.phase = Phase::List;
        self.listed.clear();
        self.provenance.clear();
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
        let (version, rank) = (
            self.preds[self.at].version.clone(),
            self.preds[self.at].rank,
        );
        eprintln!("River: an earlier River's storage ({version}) could not be read: {why}");
        // A copy found at a rank some generation not read could outrank is not decided yet:
        // this one (unless the copy is this one's own), and every older one above its rank.
        let unread_above = |f: &Found| {
            (f.read_at != rank && f.rank < rank)
                || self.preds.iter().any(|p| p.rank > f.rank && p.rank < rank)
        };
        let undecided: Vec<Vec<u8>> = self
            .found
            .iter()
            .filter(|(_, f)| unread_above(f))
            .map(|(k, _)| k.clone())
            .collect();
        for k in undecided {
            self.found.remove(&k);
            self.held.insert(
                k,
                format!("{version} could not be read and may hold a newer copy"),
            );
        }
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
    /// the current store does not hold, no newer predecessor held back, and none decided at a
    /// rank this one outranks.
    fn wanted(&self, listed: &[Vec<u8>]) -> VecDeque<Vec<u8>> {
        let rank = self.preds[self.at].rank;
        let mut out = VecDeque::new();
        for k in listed {
            if parse_room_storage_key(k).is_some()
                && !self.current.contains(k)
                && self.found.get(k).is_none_or(|f| f.rank < rank)
                && !self.held.contains_key(k)
                && !out.contains(k)
            {
                out.push_back(k.clone());
            }
        }
        out
    }

    /// Go on to the room keys of the predecessor being read, or to the next one.
    fn read_rooms(&mut self) {
        let rooms = self.wanted(&self.listed.clone());
        if rooms.is_empty() {
            self.next_pred();
        } else {
            self.phase = Phase::Rooms(rooms);
        }
    }

    /// Go on to the fixed keys, then the rooms.
    fn read_fixed(&mut self) {
        let fixed: VecDeque<Vec<u8>> = FIXED_KEYS.iter().map(|k| k.to_vec()).collect();
        self.phase = Phase::Fixed(fixed);
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
                self.listed = keys.iter().map(|k| k.as_bytes().to_vec()).collect();
                self.phase = Phase::Index;
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
            Phase::Index => {
                self.phase = Phase::IndexAgain {
                    first: value.map(<[u8]>::to_vec),
                };
            }
            // `fetch_secrets`: two reads that disagree, an index that does not decode, or one
            // that is not the listing are faults (freenet-core#5857, freenet/river#592).
            Phase::IndexAgain { first } => {
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
                if indexed != self.listed {
                    return self.stop(format!(
                        "it listed {} keys but its index holds {}",
                        self.listed.len(),
                        indexed.len()
                    ));
                }
                self.indexed = value.is_some();
                self.phase = Phase::Meta;
            }
            Phase::Meta => {
                let meta_listed = self.listed.iter().any(|k| k.as_slice() == ROOMS_META_KEY);
                match (value, meta_listed) {
                    (Some(_), _) if !self.indexed => {
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
                let meta = value.and_then(|b| ciborium::de::from_reader::<MetaShape, _>(b).ok());
                // The older generations its `rooms_meta` records for rooms' identities
                // (`identity_source_ranks`: a delegate key the registry does not list is
                // ignored).
                self.provenance = meta
                    .as_ref()
                    .map(|m| {
                        m.identity_sources
                            .iter()
                            .filter_map(|(vk, dk)| {
                                let rank = self.preds.iter().find(|p| p.key.bytes() == dk)?.rank;
                                Some((vk.to_bytes(), rank))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                // Every room it names that the index does not list: a stored one proves the
                // index short (`rooms_named_by_meta`).
                let mut named = VecDeque::new();
                if let Some(meta) = &meta {
                    for vk in meta.named() {
                        let k = room_storage_key(&vk);
                        if !self.listed.contains(&k) && !named.contains(&k) {
                            named.push_back(k);
                        }
                    }
                }
                if named.is_empty() {
                    self.read_fixed();
                } else {
                    self.phase = Phase::Named {
                        named,
                        short: false,
                    };
                }
            }
            Phase::Named { mut named, short } => {
                // A named room stored but not listed proves the index short. Its slot is
                // carried, as the web UI salvages the rooms a short index proves
                // (`salvage_short_index_rooms`): every newer generation was read in full.
                let proven = value.is_some_and(|v| !v.is_empty());
                if let Some(v) = value.filter(|v| !v.is_empty()) {
                    self.decide(key, v);
                }
                named.pop_front();
                let short = short || proven;
                if !named.is_empty() {
                    self.phase = Phase::Named { named, short };
                } else if short {
                    self.stop(
                        "a room its rooms_meta names is stored but its index does not list it"
                            .into(),
                    );
                } else {
                    self.read_fixed();
                }
            }
            Phase::Fixed(mut keys) => {
                let listed = self.listed.iter().any(|k| k.as_slice() == key);
                match value {
                    Some(_) if !listed && self.indexed => {
                        return self.stop(format!(
                            "{:?} is stored but its index does not list it",
                            String::from_utf8_lossy(key)
                        ));
                    }
                    Some(_) if !self.indexed => {
                        return self.stop(format!(
                            "{:?} is stored but it has no key index",
                            String::from_utf8_lossy(key)
                        ));
                    }
                    None if listed => {
                        return self.stop(format!(
                            "{:?} is listed but answered no value",
                            String::from_utf8_lossy(key)
                        ));
                    }
                    _ => {}
                }
                keys.pop_front();
                if keys.is_empty() {
                    self.read_rooms();
                } else {
                    self.phase = Phase::Fixed(keys);
                }
            }
            Phase::Rooms(mut rooms) => {
                let Some(value) = value else {
                    // Listed, with no value: a read fault (River never deletes a stored key).
                    return self.stop("a room key it lists answered no value".into());
                };
                self.decide(key, value);
                rooms.pop_front();
                if rooms.is_empty() {
                    self.next_pred();
                } else {
                    self.phase = Phase::Rooms(rooms);
                }
            }
            Phase::List => {}
        }
    }

    /// Hold `key` back: the predecessor being read holds it in a form this app can't use.
    /// Nothing older decides it; a copy already found at a higher rank still does.
    fn hold(&mut self, key: &[u8], why: String) {
        let rank = self.preds[self.at].rank;
        if self.found.get(key).is_some_and(|f| f.rank > rank) {
            return;
        }
        self.found.remove(key);
        self.held.insert(key.to_vec(), why);
    }

    /// A predecessor's slot for `key`: it decides the key at its rank (lowered to the older
    /// generation its `rooms_meta` records for the room's identity), unless a copy found at a
    /// higher rank already does, or it is held back. As `merge_from_source`: a present copy
    /// survives a tombstone only from a strictly higher rank, a tombstone wins over a present
    /// copy only from a strictly higher rank, and of two identities the higher rank wins.
    fn decide(&mut self, key: &[u8], value: &[u8]) {
        let pred = &self.preds[self.at];
        let version = pred.version.clone();
        let mut delegate = [0u8; 32];
        delegate.copy_from_slice(&pred.key.bytes()[..32]);
        let mut rank = pred.rank;
        let read_at = pred.rank;
        let item = match decode_room_slot(value) {
            Ok(StoredRoomSlot::Present(room))
                if parse_room_storage_key(key) == Some(room.owner_vk) =>
            {
                let owner = room.owner_vk;
                if let Some(older) = self.provenance.get(&owner.to_bytes()).copied()
                    && older < rank
                    && let Some(p) = self.preds.iter().find(|p| p.rank == older)
                {
                    rank = older;
                    delegate.copy_from_slice(&p.key.bytes()[..32]);
                }
                match carried_slot(*room, value) {
                    Ok(bytes) => Item::Room(Zeroizing::new(bytes), Box::new(owner)),
                    Err(e) => return self.hold(key, format!("{version}: {e}")),
                }
            }
            Ok(StoredRoomSlot::Present(_)) => {
                return self.hold(key, format!("{version}: a slot names another room"));
            }
            Ok(StoredRoomSlot::Tombstone) => Item::Tombstone,
            Err(e) => return self.hold(key, format!("{version}: {e}")),
        };
        if self.found.get(key).is_some_and(|f| f.rank >= rank) {
            return;
        }
        self.found.insert(
            key.to_vec(),
            Found {
                version,
                rank,
                delegate,
                read_at,
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

    /// The next write, or `None` when all are made. A room's rank goes to `rooms_meta` BEFORE
    /// its slot, so a quit between the two never leaves a carried room unranked (which the web
    /// UI would read as authoritative for good); a slot another writer got to first has the
    /// rank taken out again.
    pub fn write(&mut self) -> Option<Write> {
        if self.writing.is_none() {
            self.writing = self.writes.pop_front();
            self.write_step = WriteStep::Rank;
        }
        let key = self.writing.clone()?;
        let found = self.found.get(&key)?;
        Some(match (&found.item, self.write_step) {
            (Item::Room(_, owner), WriteStep::Rank) => Write::AddRank(**owner, found.delegate),
            (Item::Room(bytes, _), WriteStep::Slot) => Write::Slot(key, bytes.to_vec()),
            (Item::Room(_, owner), WriteStep::Unrank) => Write::RemoveRank(**owner),
            (Item::Tombstone, _) => Write::Slot(key, tombstone_bytes()),
        })
    }

    /// The write being made ended. For a slot: `stored`, or the key held something already
    /// (which wins). A rank write that found nothing to change has `stored` false too.
    pub fn written(&mut self, stored: bool) {
        let Some(key) = self.writing.clone() else {
            return;
        };
        let room = matches!(self.found.get(&key).map(|f| &f.item), Some(Item::Room(..)));
        match (self.write_step, room) {
            (WriteStep::Rank, true) => {
                self.write_step = WriteStep::Slot;
                return;
            }
            (WriteStep::Slot, true) if !stored => {
                self.write_step = WriteStep::Unrank;
                return;
            }
            _ => {}
        }
        self.writing = None;
        let Some(found) = self.found.remove(&key) else {
            return;
        };
        match (found.item, self.write_step) {
            (Item::Room(..), WriteStep::Slot) => {
                self.carried += 1;
                eprintln!("River: carried a room over from {}", found.version);
            }
            (Item::Tombstone, _) if stored => self.tombstones += 1,
            _ => {}
        }
    }

    /// Whether writes remain.
    pub fn writes_left(&self) -> bool {
        self.writing.is_some() || !self.writes.is_empty()
    }

    /// Start again (Try again): read every predecessor again. What was written stays written
    /// (the writes check the key again anyway).
    pub fn restart(&mut self) {
        self.at = 0;
        self.phase = Phase::List;
        self.found.clear();
        self.held.clear();
        self.listed.clear();
        self.provenance.clear();
        self.stopped = None;
        self.writes.clear();
        self.writing = None;
        self.not_installed = 0;
        self.began = None;
    }
}

fn tombstone_bytes() -> Vec<u8> {
    // The web UI's `RoomSlot::Tombstone`: a unit variant, which CBOR encodes as its name.
    let mut out = Vec::new();
    ciborium::ser::into_writer(&Value::Text("Tombstone".into()), &mut out)
        .expect("CBOR of a short text into a Vec cannot fail");
    out
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

/// `rooms_meta` (`current`) with `room` taken out of its identity sources, every other field
/// kept; `Ok(None)` when it has none for the room.
pub fn meta_without_source(
    current: Option<&[u8]>,
    room: &VerifyingKey,
) -> Result<Option<Vec<u8>>, String> {
    let Some(bytes) = current.filter(|b| !b.is_empty()) else {
        return Ok(None);
    };
    let mut entries = match ciborium::de::from_reader::<Value, _>(bytes) {
        Ok(Value::Map(entries)) => entries,
        Ok(_) => return Err("rooms_meta: not a map".into()),
        Err(e) => return Err(format!("rooms_meta: {e}")),
    };
    let Some(i) = entries
        .iter()
        .position(|(k, _)| k.as_text() == Some("identity_sources"))
    else {
        return Ok(None);
    };
    let Value::Array(list) = &mut entries[i].1 else {
        return Err("rooms_meta: identity_sources is not a list".into());
    };
    let before = list.len();
    list.retain(|v| {
        !v.deserialized::<(VerifyingKey, [u8; 32])>()
            .is_ok_and(|(r, _)| r == *room)
    });
    if list.len() == before {
        return Ok(None);
    }
    // The web UI leaves the field out when it is empty.
    if list.is_empty() {
        entries.remove(i);
    }
    let mut out = Vec::new();
    ciborium::ser::into_writer(&Value::Map(entries), &mut out)
        .map_err(|e| format!("rooms_meta: {e}"))?;
    Ok(Some(out))
}

#[cfg(test)]
#[path = "carry_tests.rs"]
mod tests;
