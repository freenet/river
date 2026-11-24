//! Reading Atlas's public room index for Discover Rooms (freenet/river#975,
//! step R1 of #972). There is no UI here: this module resolves, fetches,
//! verifies and decodes the index into [`DISCOVER_INDEX`], and R2 renders it.
//!
//! # Which contract
//!
//! Atlas's index contract re-keys on any change to its `common/` or
//! `contracts/`, and a stale key still answers with an old, frozen index. So
//! the key is never pinned. It is derived each session from Atlas's pointer
//! record (author key [`ATLAS_ROOT_VK_HEX`], app id [`POINTER_APP_ID`]), which
//! names the index's CURRENT code hash, plus the index params
//! `root_vk || "default"`. If the pointer cannot be resolved there is no
//! fallback key: the state is `Unavailable`, never an empty list.
//!
//! # Where the answer lives
//!
//! In [`DISCOVER_INDEX`], a session-only signal. Nothing here writes `ROOMS`,
//! the chat delegate or browser storage (pinned by
//! `the_reader_never_writes_rooms_the_delegate_or_storage`), so browsing
//! creates no state on a hosted instance and a reload starts clean.
//!
//! # Only when asked
//!
//! Nothing is fetched at startup. R2's Discover view calls [`open`], and
//! [`close`] stops the retries when it goes away. Answers arrive through the
//! ordinary `GetResponse` / `UpdateNotification` / `NotFound` paths, which
//! offer them here ([`deliver_state`], [`deliver_update`],
//! [`deliver_not_found`]) the way `public_room` and `generation_watch` claim
//! theirs.
//!
//! # States
//!
//! A pointer failure, an index that nobody has, an index that fails
//! verification, and an index with no listed rooms are four different things
//! and stay four different values ([`Unavailable`], [`DiscoverIndex::Empty`]).
//! A good list is never replaced by a later failure.

pub mod atlas;

use std::sync::Mutex;
use std::time::Duration;

use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use freenet_migrate::{
    contract_id_from_code_hash, PointerError, PointerFloor, PointerOutcome, PointerResolver,
};
use freenet_stdlib::prelude::{ContractInstanceId, Parameters, UpdateData};

use atlas::{search, IndexParams, IndexState, RoomFilter, RoomHit};

/// Atlas's root identity: the trust anchor for its pointer records AND the
/// root key of the index itself (FREENET.md, "The author verifying key").
pub const ATLAS_ROOT_VK_HEX: &str =
    "430b530d2089c82db23df0b6f01e3f7d04069714ad8ee48e3732280f6f31e9f8";
/// The pointer record that names the index contract's current code hash.
pub const POINTER_APP_ID: &[u8] = b"atlas.index-contract";
/// The main index (`IndexParams::slug`).
pub const INDEX_SLUG: &str = "default";

/// How long a request may go unanswered before it counts as failed.
const REQUEST_TIMEOUT_MS: u64 = 20_000;
/// Failed attempts before a retryable failure is shown instead of "Loading".
const SURFACE_AFTER: u32 = 6;
/// Failed attempts after which retrying stops until [`open`] is called again.
const MAX_ATTEMPTS: u32 = 20;

/// Why Discover cannot show a list. Each is a different thing to tell a user,
/// and none is "no rooms".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unavailable {
    /// Atlas has never published its index pointer.
    PointerNeverPublished,
    /// Atlas withdrew the index.
    PointerWithdrawn,
    /// Only an older pointer record than one already seen was served.
    PointerRolledBack,
    /// Two different records at one version; nothing safe to derive from.
    PointerCompeting,
    /// The pointer could not be read (timeout, or nothing answered).
    PointerUnreachable,
    /// The served pointer record failed to decode or verify.
    PointerInvalid(String),
    /// The index contract is not available from any peer asked.
    IndexNotFound,
    /// The index state failed to decode or verify.
    IndexInvalid(String),
    /// This build's Atlas trust anchor is unusable (a build bug).
    BadAnchor,
}

/// What Discover knows about the index right now.
#[derive(Debug, Clone, PartialEq)]
pub enum DiscoverIndex {
    /// Resolving the pointer, fetching, or retrying quietly.
    Loading,
    Unavailable(Unavailable),
    /// Verified, safe-search-filtered room listings, best first.
    Ready(Vec<RoomHit>),
    /// A verified index with no room that passes the filter.
    Empty,
}

/// Session-only. Never persisted.
pub static DISCOVER_INDEX: GlobalSignal<DiscoverIndex> = Global::new(|| DiscoverIndex::Loading);

// ---- pure rules ------------------------------------------------------------

fn atlas_root_vk() -> Option<VerifyingKey> {
    let bytes = bs58_free_hex(ATLAS_ROOT_VK_HEX)?;
    VerifyingKey::from_bytes(&bytes).ok()
}

fn bs58_free_hex(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// The index contract's id for a given code hash: `blake3(code_hash || params)`
/// with the index's own params, NOT the pointer's.
pub fn index_id(code_hash: &[u8; 32], params: &IndexParams) -> ContractInstanceId {
    contract_id_from_code_hash(code_hash, &Parameters::from(params.to_bytes()))
}

/// What a pointer outcome means for which index to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Read the index with this code hash.
    Use([u8; 32]),
    Fail(Unavailable),
}

/// Map a finished pointer resolution. `last` is the code hash this session
/// already resolved from a verified record; `floor_withdrawn` says the
/// session floor is a withdrawal.
///
/// Only a verified record gives a hash. The outcomes that learned nothing
/// (`Stale`, `Unavailable`, `CompetingRecord`) keep a hash this session already
/// verified, and fail otherwise. `Withdrawn` always stops. `NeverPublished`
/// is a failure, not a licence to guess a key.
pub fn map_pointer_outcome(
    result: Option<Result<PointerOutcome, PointerError>>,
    last: Option<[u8; 32]>,
    floor_withdrawn: bool,
) -> Resolution {
    let keep = |reason: Unavailable| match last {
        Some(h) if !floor_withdrawn => Resolution::Use(h),
        _ => Resolution::Fail(reason),
    };
    match result {
        None => Resolution::Fail(Unavailable::PointerUnreachable),
        Some(Err(e)) => Resolution::Fail(Unavailable::PointerInvalid(e.to_string())),
        Some(Ok(outcome)) => match outcome {
            PointerOutcome::Resolved(r) | PointerOutcome::Unchanged(r) => {
                Resolution::Use(r.code_hash())
            }
            PointerOutcome::Withdrawn { .. } => Resolution::Fail(Unavailable::PointerWithdrawn),
            PointerOutcome::NeverPublished => Resolution::Fail(Unavailable::PointerNeverPublished),
            PointerOutcome::Stale { .. } => keep(Unavailable::PointerRolledBack),
            PointerOutcome::CompetingRecord { .. } => {
                if floor_withdrawn {
                    Resolution::Fail(Unavailable::PointerWithdrawn)
                } else {
                    keep(Unavailable::PointerCompeting)
                }
            }
            PointerOutcome::Unavailable => keep(Unavailable::PointerUnreachable),
            // `PointerOutcome` is non-exhaustive: an outcome this build does
            // not know carries no record, so it cannot name an index.
            _ => Resolution::Fail(Unavailable::PointerUnreachable),
        },
    }
}

/// Decode, verify and filter one served index state.
pub fn view_from_state(bytes: &[u8], params: &IndexParams) -> Result<DiscoverIndex, Unavailable> {
    let state = IndexState::decode(bytes)
        .map_err(|e| Unavailable::IndexInvalid(format!("cannot decode: {e}")))?;
    state
        .verify(params)
        .map_err(|e| Unavailable::IndexInvalid(format!("not verified: {e}")))?;
    let hits = search(&state, &RoomFilter::default());
    Ok(if hits.is_empty() {
        DiscoverIndex::Empty
    } else {
        DiscoverIndex::Ready(hits)
    })
}

/// A good list is never replaced by a later failure; anything else takes the
/// newest answer.
pub fn merge_view(
    current: &DiscoverIndex,
    incoming: Result<DiscoverIndex, Unavailable>,
) -> DiscoverIndex {
    match incoming {
        Ok(v) => v,
        Err(_) if matches!(current, DiscoverIndex::Ready(_)) => current.clone(),
        Err(reason) => DiscoverIndex::Unavailable(reason),
    }
}

/// Delay before the next try after `attempt` failures: 4 s, 4 s, 8 s, 8 s,
/// 16 s, 16 s, then 30 s.
pub fn retry_delay_ms(attempt: u32) -> u64 {
    (4_000u64 << (attempt / 2).min(3)).min(30_000)
}

// ---- the session -----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timer {
    /// Nothing answered the request made at this epoch.
    Timeout(u64),
    /// Time to ask again for the request that failed at this epoch.
    Retry(u64),
}

/// A network answer about one of the two contracts this module reads.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer<'a> {
    State(&'a [u8]),
    NotFound,
    /// An update notification that carries only a delta.
    Delta,
}

/// Everything the session does to the outside world, so the whole decision is
/// testable without a node or the Dioxus runtime.
pub trait Effects {
    fn get(&mut self, id: ContractInstanceId, subscribe: bool);
    fn set_view(&mut self, view: DiscoverIndex);
    fn after(&mut self, delay_ms: u64, timer: Timer);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Pointer,
    Index,
}

pub struct Session {
    author_vk: VerifyingKey,
    app_id: Vec<u8>,
    params: IndexParams,
    pointer_id: ContractInstanceId,
    resolver: Option<PointerResolver>,
    floor: PointerFloor,
    last_hash: Option<[u8; 32]>,
    index_id: Option<ContractInstanceId>,
    phase: Phase,
    /// Bumped whenever a request is made, an answer lands, or the session
    /// stops, so a timer from before that moment does nothing.
    epoch: u64,
    attempt: u32,
    active: bool,
    view: DiscoverIndex,
}

impl Session {
    pub fn new(author_vk: VerifyingKey, app_id: &[u8], params: IndexParams) -> Option<Self> {
        let resolver =
            PointerResolver::new(&author_vk, app_id, PointerFloor::never_resolved()).ok()?;
        Some(Self {
            author_vk,
            app_id: app_id.to_vec(),
            params,
            pointer_id: resolver.pointer_id(),
            resolver: None,
            floor: PointerFloor::never_resolved(),
            last_hash: None,
            index_id: None,
            phase: Phase::Pointer,
            epoch: 0,
            attempt: 0,
            active: false,
            view: DiscoverIndex::Loading,
        })
    }

    pub fn view(&self) -> &DiscoverIndex {
        &self.view
    }

    /// Whether an answer for `id` belongs to this module.
    pub fn claims(&self, id: &ContractInstanceId) -> bool {
        *id == self.pointer_id || self.index_id.as_ref() == Some(id)
    }

    fn set_view(&mut self, view: DiscoverIndex, fx: &mut impl Effects) {
        if self.view != view {
            self.view = view.clone();
            fx.set_view(view);
        }
    }

    fn arm_timeout(&mut self, fx: &mut impl Effects) {
        self.epoch += 1;
        fx.after(REQUEST_TIMEOUT_MS, Timer::Timeout(self.epoch));
    }

    fn ask_pointer(&mut self, fx: &mut impl Effects) {
        self.phase = Phase::Pointer;
        let Ok(mut resolver) = PointerResolver::new(&self.author_vk, &self.app_id, self.floor)
        else {
            self.fail(Unavailable::BadAnchor, false, fx);
            return;
        };
        // Arms the resolver's event gate; the GET is sent below.
        let _ = resolver.next_action();
        self.resolver = Some(resolver);
        fx.get(self.pointer_id, false);
        self.arm_timeout(fx);
    }

    fn ask_index(&mut self, id: ContractInstanceId, fx: &mut impl Effects) {
        self.phase = Phase::Index;
        self.index_id = Some(id);
        fx.get(id, true);
        self.arm_timeout(fx);
    }

    /// Discover was opened. A first open, or one after a failure, starts over.
    /// An open that finds a list already showing asks again (a local read that
    /// also re-subscribes, which a reconnect drops). One that finds a request
    /// in flight does nothing.
    pub fn open(&mut self, fx: &mut impl Effects) {
        let was_active = std::mem::replace(&mut self.active, true);
        match (&self.view, self.index_id) {
            (DiscoverIndex::Loading, _) if was_active && self.epoch != 0 => {}
            (DiscoverIndex::Ready(_) | DiscoverIndex::Empty, Some(id))
                if self.phase == Phase::Index =>
            {
                self.ask_index(id, fx)
            }
            _ => {
                self.attempt = 0;
                if matches!(self.view, DiscoverIndex::Unavailable(_)) {
                    self.set_view(DiscoverIndex::Loading, fx);
                }
                self.ask_pointer(fx);
            }
        }
    }

    /// Discover went away: no more retries. The subscription stays, so a
    /// reopen shows a current list at once.
    pub fn close(&mut self) {
        self.active = false;
        self.epoch += 1;
    }

    /// A retryable or terminal failure at the current phase.
    fn fail(&mut self, reason: Unavailable, retryable: bool, fx: &mut impl Effects) {
        self.attempt += 1;
        if !retryable || self.attempt >= SURFACE_AFTER {
            let v = merge_view(&self.view, Err(reason));
            self.set_view(v, fx);
        }
        self.epoch += 1;
        if retryable && self.active && self.attempt < MAX_ATTEMPTS {
            fx.after(retry_delay_ms(self.attempt - 1), Timer::Retry(self.epoch));
        }
    }

    fn finish_pointer(&mut self, fx: &mut impl Effects) {
        let Some(mut resolver) = self.resolver.take() else {
            return;
        };
        let result = resolver.take_outcome();
        if let Some(Ok(outcome)) = &result {
            if let Some(next) = outcome.next_floor() {
                self.floor = next;
            }
        }
        match map_pointer_outcome(result, self.last_hash, self.floor.is_withdrawn()) {
            Resolution::Use(hash) => {
                self.last_hash = Some(hash);
                self.attempt = 0;
                let id = index_id(&hash, &self.params);
                self.ask_index(id, fx);
            }
            Resolution::Fail(reason) => {
                let retryable = !matches!(
                    reason,
                    Unavailable::PointerWithdrawn | Unavailable::BadAnchor
                );
                self.fail(reason, retryable, fx);
            }
        }
    }

    /// One network answer for a contract this session claims.
    pub fn on_answer(&mut self, id: &ContractInstanceId, answer: Answer, fx: &mut impl Effects) {
        if *id == self.pointer_id {
            if self.phase != Phase::Pointer {
                return;
            }
            let Some(resolver) = self.resolver.as_mut() else {
                return;
            };
            let accepted = match answer {
                Answer::State(bytes) => resolver.on_response(self.pointer_id, bytes),
                Answer::NotFound => resolver.on_absent(self.pointer_id),
                Answer::Delta => false,
            };
            if accepted {
                self.finish_pointer(fx);
            }
            return;
        }
        if self.index_id.as_ref() != Some(id) {
            return;
        }
        match answer {
            Answer::State(bytes) => {
                self.epoch += 1;
                self.attempt = 0;
                let next = view_from_state(bytes, &self.params);
                let v = merge_view(&self.view, next);
                self.set_view(v, fx);
            }
            // A shown list stays; the retry loop only matters while there is
            // nothing to show.
            Answer::NotFound => {
                if !matches!(self.view, DiscoverIndex::Ready(_) | DiscoverIndex::Empty) {
                    self.fail(Unavailable::IndexNotFound, true, fx);
                }
            }
            // A delta is not applied here: the state is read again, a local
            // read for a subscribed contract.
            Answer::Delta => fx.get(*id, false),
        }
    }

    pub fn on_timer(&mut self, timer: Timer, fx: &mut impl Effects) {
        if !self.active {
            return;
        }
        match timer {
            Timer::Timeout(e) if e == self.epoch => match self.phase {
                Phase::Pointer => {
                    if let Some(r) = self.resolver.as_mut() {
                        if r.on_unreachable(self.pointer_id) {
                            self.finish_pointer(fx);
                        }
                    }
                }
                Phase::Index => {
                    if !matches!(self.view, DiscoverIndex::Ready(_) | DiscoverIndex::Empty) {
                        self.fail(Unavailable::IndexNotFound, true, fx);
                    }
                }
            },
            Timer::Retry(e) if e == self.epoch => match (self.phase, self.index_id) {
                (Phase::Index, Some(id)) => self.ask_index(id, fx),
                _ => self.ask_pointer(fx),
            },
            _ => {}
        }
    }
}

// ---- live wiring -----------------------------------------------------------

static SESSION: Mutex<Option<Session>> = Mutex::new(None);

fn with_session<R>(f: impl FnOnce(&mut Session, &mut Live) -> R) -> Option<R> {
    let mut guard = SESSION.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        let root = atlas_root_vk()?;
        let params = IndexParams {
            root_vk: root,
            slug: INDEX_SLUG.to_string(),
        };
        *guard = Session::new(root, POINTER_APP_ID, params);
    }
    guard.as_mut().map(|s| f(s, &mut Live))
}

struct Live;

impl Effects for Live {
    #[cfg(not(feature = "no-sync"))]
    fn get(&mut self, id: ContractInstanceId, subscribe: bool) {
        use crate::components::app::WEB_API;
        use freenet_stdlib::client_api::{ClientRequest, ContractRequest};
        crate::util::safe_spawn_local(async move {
            let request = ClientRequest::ContractOp(ContractRequest::Get {
                key: id,
                return_contract_code: false,
                subscribe,
                blocking_subscribe: false,
            });
            if let Some(web_api) = WEB_API.write().as_mut() {
                if let Err(e) = web_api.send(request).await {
                    dioxus::logger::tracing::warn!("discover: GET failed to send: {e}");
                }
            }
        });
    }

    #[cfg(feature = "no-sync")]
    fn get(&mut self, _id: ContractInstanceId, _subscribe: bool) {}

    fn set_view(&mut self, view: DiscoverIndex) {
        crate::util::defer(move || {
            *DISCOVER_INDEX.write() = view;
        });
    }

    fn after(&mut self, delay_ms: u64, timer: Timer) {
        crate::util::safe_spawn_local(async move {
            crate::util::sleep(Duration::from_millis(delay_ms)).await;
            with_session(|s, fx| s.on_timer(timer, fx));
        });
    }
}

/// The Discover view was opened (R2 calls this). Idempotent.
pub fn open() {
    with_session(|s, fx| s.open(fx));
}

/// The Discover view was closed: stop retrying.
pub fn close() {
    with_session(|s, _| s.close());
}

/// Whether `id` is the pointer or the index this module reads.
pub fn is_discover(id: &ContractInstanceId) -> bool {
    SESSION
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|s| s.claims(id))
}

fn deliver(id: &ContractInstanceId, answer: Answer) -> bool {
    if !is_discover(id) {
        return false;
    }
    with_session(|s, fx| s.on_answer(id, answer, fx));
    true
}

/// Route a GET response. `true` if it was ours, which the caller must then not
/// treat as one of the user's rooms.
pub fn deliver_state(id: &ContractInstanceId, bytes: &[u8]) -> bool {
    deliver(id, Answer::State(bytes))
}

/// Route a `NotFound`. `true` if it was ours.
pub fn deliver_not_found(id: &ContractInstanceId) -> bool {
    deliver(id, Answer::NotFound)
}

/// Route an update notification. `true` if it was ours.
pub fn deliver_update(id: &ContractInstanceId, update: &UpdateData) -> bool {
    match update {
        UpdateData::State(bytes) => deliver(id, Answer::State(bytes.as_ref())),
        UpdateData::StateAndDelta { state, .. } => deliver(id, Answer::State(state.as_ref())),
        _ => deliver(id, Answer::Delta),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Signer;

    // ---- the golden index: bytes made by the real Atlas crates -----------

    const GOLDEN: &str = include_str!("testdata/atlas_index_golden.json");

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    fn golden() -> serde_json::Value {
        serde_json::from_str(GOLDEN).expect("fixture parses")
    }

    fn golden_params(g: &serde_json::Value) -> IndexParams {
        let root: [u8; 32] = unhex(g["root_vk_hex"].as_str().unwrap())
            .try_into()
            .unwrap();
        IndexParams {
            root_vk: VerifyingKey::from_bytes(&root).unwrap(),
            slug: g["slug"].as_str().unwrap().to_string(),
        }
    }

    fn golden_state() -> (IndexState, IndexParams, serde_json::Value) {
        let g = golden();
        let bytes = unhex(g["state_hex"].as_str().unwrap());
        let state = IndexState::decode(&bytes).expect("Atlas's bytes decode");
        (state, golden_params(&g), g)
    }

    fn filter_of(desc: &serde_json::Value) -> RoomFilter {
        RoomFilter {
            query: desc["query"].as_str().map(str::to_string),
            tags: desc["tags"]
                .as_array()
                .map(|a| a.iter().map(|t| t.as_str().unwrap().to_string()).collect())
                .unwrap_or_default(),
            safe_search: desc["safe_search"].as_bool().unwrap_or(true),
            only_live: desc["only_live"].as_bool().unwrap_or(false),
            stale_after_secs: desc["stale_after_secs"].as_u64(),
            now: desc["now"].as_u64().unwrap_or(0),
        }
    }

    fn hit_json(h: &RoomHit) -> serde_json::Value {
        use atlas::{Availability, TextSource};
        serde_json::json!({
            "owner_key": h.owner_key,
            "subject_id": h.subject_id,
            "title": h.title,
            "snippet": h.snippet,
            "snippet_source": h.snippet_source.map(|s| match s {
                TextSource::Curator => "c",
                TextSource::Llm => "l",
            }),
            "tags": h.tags,
            "featured": h.featured,
            "added_at": h.added_at,
            "availability": match h.availability {
                Availability::Live => "live",
                Availability::Unreachable => "unreachable",
                Availability::Stale => "stale",
                Availability::Unknown => "unknown",
            },
            "last_verified_at": h.last_verified_at,
            "landing": h.landing,
            "has_adult_sections": h.has_adult_sections,
            "config_version": h.config_version,
        })
    }

    #[test]
    fn the_golden_state_decodes_and_verifies() {
        let (state, params, _) = golden_state();
        state
            .verify(&params)
            .expect("Atlas's own state verifies here");
        assert!(state.apps.is_some(), "the registry is decoded too");
    }

    /// The mirror's filter must return exactly what `atlas_rooms::search`
    /// returned for the same bytes, in the same order, for every scenario.
    #[test]
    fn the_mirror_filters_exactly_as_atlas_rooms_search_does() {
        let (state, _, g) = golden_state();
        let scenarios = g["scenarios"].as_array().unwrap();
        assert!(scenarios.len() >= 8);
        for sc in scenarios {
            let got: Vec<_> = search(&state, &filter_of(&sc["filter"]))
                .iter()
                .map(hit_json)
                .collect();
            let want = sc["hits"].as_array().unwrap();
            assert_eq!(&got, want, "scenario {}", sc["name"]);
        }
    }

    #[test]
    fn the_golden_default_view_hides_what_safe_search_hides() {
        let (_, params, g) = golden_state();
        let bytes = unhex(g["state_hex"].as_str().unwrap());
        let DiscoverIndex::Ready(hits) = view_from_state(&bytes, &params).unwrap() else {
            panic!("expected rooms");
        };
        let titles: Vec<_> = hits.iter().map(|h| h.title.as_str()).collect();
        assert_eq!(titles[0], "Freenet Official", "featured first");
        for hidden in [
            "Unclassified Room",
            "Adult Room",
            "Explicit Room",
            "Short Key Room",
            "Delta Locator Room",
            "A Site",
            "Old Freenet name",
        ] {
            assert!(!titles.contains(&hidden), "{hidden} must not be listed");
        }
    }

    // ---- malformed input ---------------------------------------------------

    fn golden_bytes() -> (Vec<u8>, IndexParams) {
        let (_, params, g) = golden_state();
        (unhex(g["state_hex"].as_str().unwrap()), params)
    }

    #[test]
    fn garbage_is_an_invalid_index_and_never_an_empty_list() {
        let (_, params) = golden_bytes();
        for bad in [&b""[..], b"\x00", b"not cbor at all", &[0xff; 64]] {
            assert!(
                matches!(
                    view_from_state(bad, &params),
                    Err(Unavailable::IndexInvalid(_))
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_truncated_state_is_invalid() {
        let (bytes, params) = golden_bytes();
        for cut in [1, bytes.len() / 2, bytes.len() - 1] {
            assert!(matches!(
                view_from_state(&bytes[..cut], &params),
                Err(Unavailable::IndexInvalid(_))
            ));
        }
    }

    #[test]
    fn a_state_for_another_root_does_not_verify() {
        let (bytes, params) = golden_bytes();
        let other = IndexParams {
            root_vk: ed25519_dalek::SigningKey::from_bytes(&[42; 32]).verifying_key(),
            slug: params.slug.clone(),
        };
        assert!(matches!(
            view_from_state(&bytes, &other),
            Err(Unavailable::IndexInvalid(m)) if m.contains("key_auth")
        ));
    }

    #[test]
    fn a_registry_signed_for_another_slug_is_refused() {
        let (bytes, params) = golden_bytes();
        let other = IndexParams {
            root_vk: params.root_vk,
            slug: "staging".to_string(),
        };
        assert!(matches!(
            view_from_state(&bytes, &other),
            Err(Unavailable::IndexInvalid(m)) if m.contains("slug")
        ));
    }

    /// A flipped byte inside a signed title must break a signature, not slip
    /// through as a different title.
    #[test]
    fn a_tampered_title_fails_verification() {
        let (bytes, params) = golden_bytes();
        let needle = b"Cooking Club";
        let at = bytes
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("title in state");
        let mut forged = bytes.clone();
        forged[at] = b'X';
        assert!(matches!(
            view_from_state(&forged, &params),
            Err(Unavailable::IndexInvalid(m)) if m.contains("record signature")
        ));
    }

    #[test]
    fn a_record_from_an_unauthorized_key_is_refused() {
        let (mut state, params, _) = golden_state();
        state.key_auth.as_mut().unwrap().body.authorized.clear();
        // The key_auth signature now fails first; either way it is invalid.
        assert!(state.verify(&params).is_err());
    }

    #[test]
    fn an_index_with_no_listed_rooms_is_empty_not_unavailable() {
        let (mut state, params, _) = golden_state();
        state.records.clear();
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&state, &mut bytes).unwrap();
        assert_eq!(view_from_state(&bytes, &params), Ok(DiscoverIndex::Empty));
    }

    // ---- pointer outcomes ----------------------------------------------------

    /// The live Atlas record from its `pointer-records.toml`.
    const ATLAS_RECORD_HEX: &str = "00000001ee6713f87a43f870f8c697f1749b4e367cb3dd2d5affb9d7a51cd27dce12647ba12555d602e821fb61fd06984ad31fe76d3bb794683937278450097fc5957f8ca7af237c8c3bc2223222810fb15bb5fe51c1cd74d3175e6ac67b9cd767b82c0e";

    fn resolve_with(
        record: &[u8],
        floor: PointerFloor,
        author: &VerifyingKey,
        app: &[u8],
    ) -> Option<Result<PointerOutcome, PointerError>> {
        let mut r = PointerResolver::new(author, app, floor).unwrap();
        let _ = r.next_action();
        assert!(r.on_response(r.pointer_id(), record));
        r.take_outcome()
    }

    #[test]
    fn the_real_atlas_pointer_record_resolves_to_its_code_hash() {
        let author = atlas_root_vk().expect("anchor decodes");
        let r =
            PointerResolver::new(&author, POINTER_APP_ID, PointerFloor::never_resolved()).unwrap();
        assert_eq!(
            r.pointer_id().to_string(),
            "BwsKx5iDhjBJGDNAPtZbbC9f6twDAUrnb2Yh1D6Wng2K",
            "the pinned anchor and app id derive Atlas's published pointer address"
        );
        let out = resolve_with(
            &unhex(ATLAS_RECORD_HEX),
            PointerFloor::never_resolved(),
            &author,
            POINTER_APP_ID,
        );
        let want: [u8; 32] =
            unhex("ee6713f87a43f870f8c697f1749b4e367cb3dd2d5affb9d7a51cd27dce12647b")
                .try_into()
                .unwrap();
        assert_eq!(map_pointer_outcome(out, None, false), Resolution::Use(want));
    }

    #[test]
    fn the_index_id_comes_from_the_index_params_not_the_pointers() {
        let hash = [9u8; 32];
        let a = IndexParams {
            root_vk: atlas_root_vk().unwrap(),
            slug: INDEX_SLUG.into(),
        };
        let b = IndexParams {
            slug: "staging".into(),
            ..a.clone()
        };
        assert_eq!(index_id(&hash, &a), index_id(&hash, &a));
        assert_ne!(index_id(&hash, &a), index_id(&hash, &b));
        assert_ne!(index_id(&hash, &a), index_id(&[8u8; 32], &a));
        // Same shape as contract id derivation: blake3(code_hash || params).
        let mut h = blake3::Hasher::new();
        h.update(&hash);
        h.update(&a.to_bytes());
        assert_eq!(index_id(&hash, &a).as_bytes(), h.finalize().as_bytes());
    }

    fn signed_record(sk: &ed25519_dalek::SigningKey, version: u32, hash: [u8; 32]) -> Vec<u8> {
        let params = freenet_migrate::pointer_params(&sk.verifying_key(), POINTER_APP_ID).unwrap();
        let msg = freenet_migrate::pointer_signing_message(&params, version, &hash);
        freenet_migrate::PointerRecord {
            version,
            code_hash: hash,
            signature: sk.sign(&msg).to_bytes(),
        }
        .encode()
        .to_vec()
    }

    fn pointer_sk() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[5; 32])
    }

    #[test]
    fn every_pointer_outcome_maps_to_a_use_or_a_distinct_failure() {
        let sk = pointer_sk();
        let author = sk.verifying_key();
        let h1 = [1u8; 32];
        let h2 = [2u8; 32];
        let map = |out, last, w| map_pointer_outcome(out, last, w);

        // Resolved.
        let out = resolve_with(
            &signed_record(&sk, 2, h2),
            PointerFloor::at(1, h1).unwrap(),
            &author,
            POINTER_APP_ID,
        );
        assert_eq!(map(out, Some(h1), false), Resolution::Use(h2));
        // Unchanged.
        let out = resolve_with(
            &signed_record(&sk, 1, h1),
            PointerFloor::at(1, h1).unwrap(),
            &author,
            POINTER_APP_ID,
        );
        assert_eq!(map(out, Some(h1), false), Resolution::Use(h1));
        // Stale (rollback): keeps what was verified, else a rollback failure.
        let out = || {
            resolve_with(
                &signed_record(&sk, 1, h1),
                PointerFloor::at(2, h2).unwrap(),
                &author,
                POINTER_APP_ID,
            )
        };
        assert_eq!(map(out(), Some(h2), false), Resolution::Use(h2));
        assert_eq!(
            map(out(), None, false),
            Resolution::Fail(Unavailable::PointerRolledBack)
        );
        // Competing record at the floor's own version.
        let out = || {
            resolve_with(
                &signed_record(&sk, 2, [3u8; 32]),
                PointerFloor::at(2, h1).unwrap(),
                &author,
                POINTER_APP_ID,
            )
        };
        assert_eq!(
            map(out(), None, false),
            Resolution::Fail(Unavailable::PointerCompeting)
        );
        assert_eq!(map(out(), Some(h1), false), Resolution::Use(h1));
        // Withdrawn stops, even with a key already derived.
        let out = resolve_with(
            &signed_record(&sk, 3, [0u8; 32]),
            PointerFloor::at(2, h2).unwrap(),
            &author,
            POINTER_APP_ID,
        );
        assert_eq!(
            map(out, Some(h2), false),
            Resolution::Fail(Unavailable::PointerWithdrawn)
        );
        // ... and a withdrawal floor never lets a replayed record resurrect a key.
        let out = || {
            resolve_with(
                &signed_record(&sk, 3, h1),
                PointerFloor::withdrawn_at(3).unwrap(),
                &author,
                POINTER_APP_ID,
            )
        };
        assert_eq!(
            map(out(), Some(h1), true),
            Resolution::Fail(Unavailable::PointerWithdrawn)
        );
        // NeverPublished (a definitive absence) is a failure, not a fallback.
        let mut r =
            PointerResolver::new(&author, POINTER_APP_ID, PointerFloor::never_resolved()).unwrap();
        let _ = r.next_action();
        assert!(r.on_absent(r.pointer_id()));
        assert_eq!(
            map(r.take_outcome(), None, false),
            Resolution::Fail(Unavailable::PointerNeverPublished)
        );
        // Unreachable keeps a verified key, else fails as unreachable.
        let mut r =
            PointerResolver::new(&author, POINTER_APP_ID, PointerFloor::never_resolved()).unwrap();
        let _ = r.next_action();
        assert!(r.on_unreachable(r.pointer_id()));
        assert_eq!(
            map(r.take_outcome(), None, false),
            Resolution::Fail(Unavailable::PointerUnreachable)
        );
        // A forged record fails as invalid.
        let other = ed25519_dalek::SigningKey::from_bytes(&[6; 32]);
        let mut forged = signed_record(&other, 2, h2);
        forged[0..4].copy_from_slice(&2u32.to_be_bytes());
        let out = resolve_with(
            &forged,
            PointerFloor::never_resolved(),
            &author,
            POINTER_APP_ID,
        );
        assert!(matches!(
            map(out, None, false),
            Resolution::Fail(Unavailable::PointerInvalid(_))
        ));
        // No result at all.
        assert_eq!(
            map(None, None, false),
            Resolution::Fail(Unavailable::PointerUnreachable)
        );
    }

    // ---- views ---------------------------------------------------------------

    #[test]
    fn a_good_list_survives_a_later_failure_but_loading_does_not() {
        let hit = RoomHit {
            owner_key: "k".into(),
            subject_id: "s".into(),
            title: "t".into(),
            snippet: String::new(),
            snippet_source: None,
            tags: vec![],
            featured: false,
            added_at: 0,
            availability: atlas::Availability::Unknown,
            last_verified_at: None,
            landing: Some("general".into()),
            has_adult_sections: Some(false),
            config_version: None,
        };
        let ready = DiscoverIndex::Ready(vec![hit]);
        let bad = || Err(Unavailable::IndexInvalid("x".into()));
        assert_eq!(merge_view(&ready, bad()), ready);
        assert_eq!(
            merge_view(&ready, Ok(DiscoverIndex::Empty)),
            DiscoverIndex::Empty
        );
        assert_eq!(
            merge_view(&DiscoverIndex::Loading, bad()),
            DiscoverIndex::Unavailable(Unavailable::IndexInvalid("x".into()))
        );
    }

    #[test]
    fn the_retry_delay_grows_and_is_bounded() {
        let d: Vec<_> = (0..9).map(retry_delay_ms).collect();
        assert_eq!(
            d,
            [4000, 4000, 8000, 8000, 16000, 16000, 30000, 30000, 30000]
        );
    }

    // ---- the session, driven by a recording effect sink -----------------------

    #[derive(Default)]
    struct Rec {
        gets: Vec<(ContractInstanceId, bool)>,
        views: Vec<DiscoverIndex>,
        timers: Vec<(u64, Timer)>,
    }

    impl Effects for Rec {
        fn get(&mut self, id: ContractInstanceId, subscribe: bool) {
            self.gets.push((id, subscribe));
        }
        fn set_view(&mut self, view: DiscoverIndex) {
            self.views.push(view);
        }
        fn after(&mut self, delay_ms: u64, timer: Timer) {
            self.timers.push((delay_ms, timer));
        }
    }

    struct Rig {
        s: Session,
        fx: Rec,
        sk: ed25519_dalek::SigningKey,
        index_hash: [u8; 32],
    }

    fn rig() -> Rig {
        let sk = pointer_sk();
        let (_, params, _) = golden_state();
        let s = Session::new(sk.verifying_key(), POINTER_APP_ID, params).unwrap();
        Rig {
            s,
            fx: Rec::default(),
            sk,
            index_hash: [7u8; 32],
        }
    }

    impl Rig {
        fn pointer_id(&self) -> ContractInstanceId {
            self.s.pointer_id
        }
        fn idx(&self) -> ContractInstanceId {
            index_id(&self.index_hash, &self.s.params)
        }
        fn answer_pointer(&mut self) {
            let rec = signed_record(&self.sk, 1, self.index_hash);
            let id = self.pointer_id();
            self.s.on_answer(&id, Answer::State(&rec), &mut self.fx);
        }
        fn last_timer(&self) -> Timer {
            self.fx.timers.last().unwrap().1
        }
    }

    #[test]
    fn nothing_is_requested_until_opened() {
        let r = rig();
        assert!(r.fx.gets.is_empty());
        assert_eq!(*r.s.view(), DiscoverIndex::Loading);
    }

    #[test]
    fn open_resolves_the_pointer_then_gets_the_derived_index_with_subscribe() {
        let mut r = rig();
        r.s.open(&mut r.fx);
        assert_eq!(r.fx.gets, [(r.pointer_id(), false)]);
        r.answer_pointer();
        let idx = r.idx();
        assert_eq!(
            r.fx.gets[1],
            (idx, true),
            "GET + subscribe on the derived id"
        );
        assert!(r.s.claims(&idx));

        let (bytes, _) = golden_bytes();
        r.s.on_answer(&idx, Answer::State(&bytes), &mut r.fx);
        assert!(matches!(r.fx.views.last(), Some(DiscoverIndex::Ready(h)) if !h.is_empty()));
    }

    #[test]
    fn an_update_state_replaces_the_list_and_a_delta_refetches() {
        let mut r = rig();
        r.s.open(&mut r.fx);
        r.answer_pointer();
        let idx = r.idx();
        let (bytes, _) = golden_bytes();
        r.s.on_answer(&idx, Answer::State(&bytes), &mut r.fx);
        let n = r.fx.gets.len();
        r.s.on_answer(&idx, Answer::Delta, &mut r.fx);
        assert_eq!(r.fx.gets.len(), n + 1);
        assert_eq!(*r.fx.gets.last().unwrap(), (idx, false));
    }

    #[test]
    fn index_not_found_retries_quietly_then_surfaces_and_keeps_trying() {
        let mut r = rig();
        r.s.open(&mut r.fx);
        r.answer_pointer();
        let idx = r.idx();
        let gets_before = r.fx.gets.len();
        for i in 1..=SURFACE_AFTER {
            r.s.on_answer(&idx, Answer::NotFound, &mut r.fx);
            if i < SURFACE_AFTER {
                assert!(r.fx.views.is_empty(), "attempt {i} stays Loading");
            }
            let t = r.last_timer();
            assert!(matches!(t, Timer::Retry(_)));
            r.s.on_timer(t, &mut r.fx);
        }
        assert_eq!(
            r.fx.views.last(),
            Some(&DiscoverIndex::Unavailable(Unavailable::IndexNotFound))
        );
        // Each retry asked for the index again with subscribe.
        assert_eq!(r.fx.gets.len(), gets_before + SURFACE_AFTER as usize);
        assert!(r.fx.gets[gets_before..].iter().all(|g| *g == (idx, true)));
        // First delay is the 4 s Atlas's own UI uses.
        let first_retry =
            r.fx.timers
                .iter()
                .find(|t| matches!(t.1, Timer::Retry(_)))
                .unwrap();
        assert_eq!(first_retry.0, 4000);
    }

    #[test]
    fn retrying_stops_after_the_cap_and_open_starts_over() {
        let mut r = rig();
        r.s.open(&mut r.fx);
        r.answer_pointer();
        let idx = r.idx();
        for _ in 0..MAX_ATTEMPTS + 2 {
            r.s.on_answer(&idx, Answer::NotFound, &mut r.fx);
            if let Some((_, t @ Timer::Retry(_))) = r.fx.timers.last().copied() {
                r.s.on_timer(t, &mut r.fx);
            }
        }
        let n = r.fx.gets.len();
        let last = r.fx.timers.len();
        r.s.on_answer(&idx, Answer::NotFound, &mut r.fx);
        assert_eq!(
            r.fx.timers.len(),
            last,
            "no retry is scheduled past the cap"
        );
        assert_eq!(r.fx.gets.len(), n);
        // Reopening asks again from the pointer.
        r.s.open(&mut r.fx);
        assert_eq!(*r.fx.gets.last().unwrap(), (r.pointer_id(), false));
    }

    #[test]
    fn a_pointer_that_never_answers_times_out_as_unreachable_and_retries() {
        let mut r = rig();
        r.s.open(&mut r.fx);
        let t = r.last_timer();
        assert!(matches!(t, Timer::Timeout(_)));
        r.s.on_timer(t, &mut r.fx);
        assert!(matches!(r.last_timer(), Timer::Retry(_)));
        assert!(r.fx.views.is_empty(), "first failures are quiet");
        // A stale timer from before is ignored.
        let gets = r.fx.gets.len();
        r.s.on_timer(t, &mut r.fx);
        assert_eq!(r.fx.gets.len(), gets);
    }

    #[test]
    fn a_pointer_not_found_is_never_published_and_never_a_fallback_key() {
        let mut r = rig();
        r.s.open(&mut r.fx);
        let id = r.pointer_id();
        for _ in 0..SURFACE_AFTER {
            r.s.on_answer(&id, Answer::NotFound, &mut r.fx);
            if let Timer::Retry(e) = r.last_timer() {
                r.s.on_timer(Timer::Retry(e), &mut r.fx);
            }
        }
        assert_eq!(
            r.fx.views.last(),
            Some(&DiscoverIndex::Unavailable(
                Unavailable::PointerNeverPublished
            ))
        );
        assert!(
            r.fx.gets.iter().all(|(g, _)| *g == id),
            "no index GET without a resolved pointer"
        );
    }

    #[test]
    fn a_withdrawn_pointer_is_terminal() {
        let mut r = rig();
        r.s.open(&mut r.fx);
        let rec = signed_record(&r.sk, 2, [0u8; 32]);
        let id = r.pointer_id();
        r.s.on_answer(&id, Answer::State(&rec), &mut r.fx);
        assert_eq!(
            r.fx.views.last(),
            Some(&DiscoverIndex::Unavailable(Unavailable::PointerWithdrawn))
        );
        assert!(!matches!(r.last_timer(), Timer::Retry(_)));
    }

    #[test]
    fn a_bad_index_state_is_invalid_and_does_not_replace_a_good_list() {
        let mut r = rig();
        r.s.open(&mut r.fx);
        r.answer_pointer();
        let idx = r.idx();
        r.s.on_answer(&idx, Answer::State(b"junk"), &mut r.fx);
        assert!(matches!(
            r.fx.views.last(),
            Some(DiscoverIndex::Unavailable(Unavailable::IndexInvalid(_)))
        ));
        let (bytes, _) = golden_bytes();
        r.s.on_answer(&idx, Answer::State(&bytes), &mut r.fx);
        let good = r.fx.views.last().cloned();
        r.s.on_answer(&idx, Answer::State(b"junk"), &mut r.fx);
        assert_eq!(r.fx.views.last().cloned(), good, "a good list stays");
        r.s.on_answer(&idx, Answer::NotFound, &mut r.fx);
        assert_eq!(r.fx.views.last().cloned(), good);
    }

    #[test]
    fn a_second_open_with_a_list_resubscribes_and_one_in_flight_does_nothing() {
        let mut r = rig();
        r.s.open(&mut r.fx);
        let n = r.fx.gets.len();
        r.s.open(&mut r.fx);
        assert_eq!(r.fx.gets.len(), n, "a request is already in flight");
        r.answer_pointer();
        let idx = r.idx();
        let (bytes, _) = golden_bytes();
        r.s.on_answer(&idx, Answer::State(&bytes), &mut r.fx);
        let n = r.fx.gets.len();
        r.s.open(&mut r.fx);
        assert_eq!(r.fx.gets.len(), n + 1);
        assert_eq!(*r.fx.gets.last().unwrap(), (idx, true));
    }

    #[test]
    fn close_stops_retries_and_timers() {
        let mut r = rig();
        r.s.open(&mut r.fx);
        let t = r.last_timer();
        r.s.close();
        let n = r.fx.gets.len();
        r.s.on_timer(t, &mut r.fx);
        assert_eq!(r.fx.gets.len(), n);
        assert!(r.fx.views.is_empty());
    }

    #[test]
    fn answers_for_other_contracts_are_not_claimed() {
        let mut r = rig();
        r.s.open(&mut r.fx);
        let other = ContractInstanceId::new([1u8; 32]);
        assert!(!r.s.claims(&other));
        assert!(r.s.claims(&r.pointer_id()));
        assert!(!r.s.claims(&r.idx()), "the index is claimed once derived");
    }

    // ---- source pins ------------------------------------------------------------

    fn production_source(rel: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join(rel);
        crate::util::source_scan::production_only(&std::fs::read_to_string(path).expect("readable"))
    }

    #[test]
    fn the_reader_never_writes_rooms_the_delegate_or_storage() {
        for file in ["discover_index/mod.rs", "discover_index/atlas.rs"] {
            let src = production_source(file);
            for forbidden in [
                "ROOMS",
                "SYNC_INFO",
                "PENDING_INVITES",
                "chat_delegate",
                "ContractRequest::Put",
                "ContractRequest::Update",
                "DelegateRequest",
                "localStorage",
                "local_storage",
                "sessionStorage",
                "set_item",
            ] {
                assert!(!src.contains(forbidden), "{file} must not use {forbidden}");
            }
        }
    }

    #[test]
    fn nothing_is_fetched_at_startup() {
        // The only senders are reached from `open`/`on_timer`; no module-level
        // initialiser or app startup refers to them.
        let app = production_source("components/app.rs");
        assert!(
            !app.contains("discover_index::"),
            "Discover must be fetched only when opened"
        );
    }

    fn handler_source(rel: &str) -> String {
        production_source(&format!("components/app/freenet_api/{rel}"))
    }

    #[test]
    fn every_response_path_offers_the_answer_to_discover_before_any_room_lookup() {
        let get = handler_source("response_handler/get_response.rs");
        let at = get
            .find("discover_index::deliver_state(")
            .expect("GET hook");
        assert!(at < get.find("SYNC_INFO.read()").expect("lookup"));
        let nf = get
            .find("discover_index::deliver_not_found(")
            .expect("NotFound hook");
        assert!(
            nf < get
                .find("get_owner_vk_for_instance_id(&instance_id)")
                .expect("lookup")
        );
        let upd = handler_source("response_handler/update_notification.rs");
        assert!(
            upd.find("discover_index::deliver_update(")
                .expect("update hook")
                < upd
                    .find("get_owner_vk_for_instance_id(key.id())")
                    .expect("lookup")
        );
        assert!(handler_source("response_handler/subscribe_response.rs")
            .contains("discover_index::is_discover("));
        assert!(handler_source("response_handler.rs").contains("discover_index::is_discover("));
    }

    #[test]
    fn the_trust_anchor_is_atlas_published_key() {
        assert!(atlas_root_vk().is_some());
        assert_eq!(ATLAS_ROOT_VK_HEX.len(), 64);
        assert_eq!(&ATLAS_ROOT_VK_HEX[..8], "430b530d");
    }
}
