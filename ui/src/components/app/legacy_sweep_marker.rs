//! A persistent "swept every legacy delegate generation, found nothing" marker,
//! so the legacy room sweep runs at most once per browser and node instead of
//! on every page load of a user with no rooms.
//!
//! The marker is a claim that no legacy generation holds data for this
//! browser's identity on this node. It is only worth anything if it is never
//! wrong in the dangerous direction (skipping a sweep that would have found
//! rooms), so:
//!
//! * It is keyed by the node (the WebSocket URL the UI talks to), the CURRENT
//!   delegate key and the whole ordered legacy generation list. A re-key adds a
//!   generation and turns the current delegate into one, which changes the
//!   fingerprint, so a user who was new when the marker was written is swept
//!   after the re-key.
//! * It is written only when [`SweepLedger::complete_and_empty`] holds: the
//!   sweep sent every wave, every generation gave a definitive "nothing here"
//!   answer to all three of its requests, no generation held data, and no
//!   fetch failed. Silence, a timed-out wave, a refusal that is not a typed
//!   "delegate missing", a transport failure and a superseded sweep all leave
//!   it unwritten.
//! * Any storage failure, or a value that is not exactly this build's
//!   fingerprint, reads as "no marker": the sweep runs as it always did.

use std::sync::Mutex;

use river_core::chat_delegate::ChatDelegateResponseMsg;

use super::chat_delegate::{
    current_delegate_key, legacy_delegate_pairs, OUTBOUND_DMS_STORAGE_KEY, ROOMS_STORAGE_KEY,
};
use super::freenet_api::constants::get_websocket_url;

/// The localStorage key holding the marker.
#[cfg(target_arch = "wasm32")]
const STORAGE_KEY: &str = "river.legacy_sweep_empty";
/// Format version of the stored value (`v2:<fingerprint>:<written at, ms>`). A
/// value with any other prefix is ignored.
const VALUE_PREFIX: &str = "v2:";

/// How long a marker is believed. It bounds a wrong marker in time: rooms
/// written later to a legacy generation by an old tab or an old desktop build,
/// or a node whose data changed behind the same URL, are found again by the
/// next sweep after this.
const MARKER_MAX_AGE_MS: f64 = 7.0 * 24.0 * 60.0 * 60.0 * 1000.0;
/// A stamp this far ahead of the clock is not believed (clock changed).
const MARKER_FUTURE_SLACK_MS: f64 = 60.0 * 1000.0;

/// Where the marker lives. Every method can fail (private mode, quota).
pub(crate) trait MarkerStore {
    fn get(&self) -> Result<Option<String>, ()>;
    fn set(&self, value: &str) -> Result<(), ()>;
    fn remove(&self) -> Result<(), ()>;
}

/// The fingerprint of "what this marker covers": node, current delegate and
/// the ordered legacy generation list. Length-prefixed so no two inputs share
/// an encoding.
pub(crate) fn fingerprint(node: &str, current: &[u8], legacy: &[[u8; 32]]) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"river-legacy-sweep-empty-v1");
    h.update(&(node.len() as u64).to_le_bytes());
    h.update(node.as_bytes());
    h.update(&(current.len() as u64).to_le_bytes());
    h.update(current);
    h.update(&(legacy.len() as u64).to_le_bytes());
    for key in legacy {
        h.update(key);
    }
    format!("{VALUE_PREFIX}{}", h.finalize().to_hex())
}

fn stored_value(fingerprint: &str, now_ms: f64) -> String {
    format!("{fingerprint}:{}", now_ms.max(0.0) as u64)
}

/// Whether a stored value is `fingerprint`, written at most
/// [`MARKER_MAX_AGE_MS`] before `now_ms`. Pure.
fn value_is_fresh_match(value: &str, fingerprint: &str, now_ms: f64) -> bool {
    let Some(stamp) = value
        .strip_prefix(fingerprint)
        .and_then(|rest| rest.strip_prefix(':'))
    else {
        return false;
    };
    let Ok(written) = stamp.parse::<u64>() else {
        return false;
    };
    let age = now_ms - written as f64;
    (-MARKER_FUTURE_SLACK_MS..MARKER_MAX_AGE_MS).contains(&age)
}

fn now_ms() -> f64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as f64)
            .unwrap_or(0.0)
    }
}

/// Whether the store holds `fingerprint`, written within the last seven days.
/// Any failure or any other value (older format, other node, other generation
/// set, too old) is `false`.
pub(crate) fn marker_matches(store: &dyn MarkerStore, fingerprint: &str, now_ms: f64) -> bool {
    matches!(store.get(), Ok(Some(v)) if value_is_fresh_match(&v, fingerprint, now_ms))
}

/// Write the marker, stamped `now_ms`; `false` if the store refused.
pub(crate) fn write_marker(store: &dyn MarkerStore, fingerprint: &str, now_ms: f64) -> bool {
    store.set(&stored_value(fingerprint, now_ms)).is_ok()
}

/// Remove the marker (best effort).
pub(crate) fn clear_marker(store: &dyn MarkerStore) {
    let _ = store.remove();
}

// ---------------------------------------------------------------------------
// The sweep ledger: what the running sweep has established so far.
// ---------------------------------------------------------------------------

const ANSWERED_ROOMS: u8 = 0b001;
const ANSWERED_OUTBOUND: u8 = 0b010;
const ANSWERED_LIST: u8 = 0b100;
const ANSWERED_ALL: u8 = 0b111;

/// What one legacy delegate reply says about whether the generation has data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reply {
    /// `rooms_data` GET answered with no value.
    RoomsEmpty,
    /// `outbound_dms` GET answered with no value.
    OutboundEmpty,
    /// `ListRequest` answered with no keys.
    ListEmpty,
    /// Any value or any key: the generation holds data.
    Data,
    /// Not a sweep answer.
    Other,
}

/// Classify a legacy delegate's reply. Pure.
pub(crate) fn classify_reply(response: &ChatDelegateResponseMsg) -> Reply {
    match response {
        ChatDelegateResponseMsg::GetResponse { key, value }
            if key.as_bytes() == ROOMS_STORAGE_KEY =>
        {
            if value.is_some() {
                Reply::Data
            } else {
                Reply::RoomsEmpty
            }
        }
        ChatDelegateResponseMsg::GetResponse { key, value }
            if key.as_bytes() == OUTBOUND_DMS_STORAGE_KEY =>
        {
            if value.is_some() {
                Reply::Data
            } else {
                Reply::OutboundEmpty
            }
        }
        ChatDelegateResponseMsg::ListResponse { keys } => {
            if keys.is_empty() {
                Reply::ListEmpty
            } else {
                Reply::Data
            }
        }
        _ => Reply::Other,
    }
}

/// The evidence one sweep has gathered.
#[derive(Debug, Clone)]
pub(crate) struct SweepLedger {
    epoch: u64,
    attempt: u32,
    /// Per generation, which of its three requests were answered "nothing".
    answered: Vec<u8>,
    found_data: bool,
    /// Every wave was sent (not stopped, superseded or cut short by a drop).
    all_waves_sent: bool,
}

impl SweepLedger {
    pub(crate) fn new(epoch: u64, attempt: u32, generations: usize) -> Self {
        Self {
            epoch,
            attempt,
            answered: vec![0; generations],
            found_data: false,
            all_waves_sent: false,
        }
    }

    pub(crate) fn note_reply(&mut self, generation: usize, reply: Reply) {
        let Some(bits) = self.answered.get_mut(generation) else {
            return;
        };
        match reply {
            Reply::RoomsEmpty => *bits |= ANSWERED_ROOMS,
            Reply::OutboundEmpty => *bits |= ANSWERED_OUTBOUND,
            Reply::ListEmpty => *bits |= ANSWERED_LIST,
            Reply::Data => self.found_data = true,
            Reply::Other => {}
        }
    }

    /// The node said, with a typed `Missing`, that the generation's delegate is
    /// not installed: nothing in it to find, for all three requests. An empty
    /// reply is NOT this: it proves nothing about the delegate, so a node that
    /// answers that way writes no marker.
    pub(crate) fn note_absent(&mut self, generation: usize) {
        if let Some(bits) = self.answered.get_mut(generation) {
            *bits = ANSWERED_ALL;
        }
    }

    pub(crate) fn note_all_waves_sent(&mut self) {
        self.all_waves_sent = true;
    }

    #[cfg(test)]
    pub(crate) fn found_data(&self) -> bool {
        self.found_data
    }

    /// Whether the marker may be written. Every condition is required; any
    /// doubt is `false`.
    ///
    /// * `latest_epoch`: the newest sweep's epoch (this one must be it).
    /// * `current_attempt`: the current load attempt (this sweep's must be it).
    /// * `saw_fetch_failure`: the attempt's `SAW_FETCH_FAILURE`.
    /// * `rooms_empty`: the room map is empty (the load is a genuine Empty).
    pub(crate) fn complete_and_empty(
        &self,
        latest_epoch: u64,
        current_attempt: u32,
        saw_fetch_failure: bool,
        rooms_empty: bool,
    ) -> bool {
        self.epoch == latest_epoch
            && self.attempt == current_attempt
            && self.all_waves_sent
            && !self.found_data
            && !saw_fetch_failure
            && rooms_empty
            && !self.answered.is_empty()
            && self.answered.iter().all(|b| *b == ANSWERED_ALL)
    }
}

static LEDGER: Mutex<Option<SweepLedger>> = Mutex::new(None);

fn with_ledger(f: impl FnOnce(&mut SweepLedger)) {
    if let Some(ledger) = LEDGER.lock().unwrap().as_mut() {
        f(ledger);
    }
}

fn generation_of(delegate: &[u8]) -> Option<usize> {
    legacy_delegate_pairs()
        .iter()
        .position(|(key, _)| key.as_slice() == delegate)
}

/// A sweep starts: forget the previous one's evidence.
pub(crate) fn begin_sweep(epoch: u64, attempt: u32) {
    *LEDGER.lock().unwrap() = Some(SweepLedger::new(
        epoch,
        attempt,
        legacy_delegate_pairs().len(),
    ));
}

/// The sweep sent every wave it plans to.
pub(crate) fn note_all_waves_sent(epoch: u64) {
    with_ledger(|l| {
        if l.epoch == epoch {
            l.note_all_waves_sent();
        }
    });
}

/// A legacy delegate replied. Data clears the marker at once.
pub(crate) fn note_legacy_reply(delegate: &[u8], response: &ChatDelegateResponseMsg) {
    let Some(generation) = generation_of(delegate) else {
        return;
    };
    let reply = classify_reply(response);
    if reply == Reply::Data {
        clear_marker_now();
    }
    with_ledger(|l| l.note_reply(generation, reply));
}

/// A legacy delegate is absent on the node (typed `Missing` only).
pub(crate) fn note_legacy_absent(delegate: &[u8]) {
    if let Some(generation) = generation_of(delegate) {
        with_ledger(|l| l.note_absent(generation));
    }
}

/// Whether the stored marker covers this node, current delegate and
/// generation list.
pub(crate) fn marker_matches_now() -> bool {
    marker_matches(&BrowserStore, &current_fingerprint(), now_ms())
}

/// Remove the marker: a Retry, an interrupted-migration recovery, found data.
pub(crate) fn clear_marker_now() {
    clear_marker(&BrowserStore);
}

/// Write the marker if the latest sweep proved every generation empty.
/// Called only where the load resolves to a genuine Empty.
pub(crate) fn write_marker_if_sweep_proved_empty(
    latest_epoch: u64,
    current_attempt: u32,
    saw_fetch_failure: bool,
    rooms_empty: bool,
) -> bool {
    let proved = LEDGER.lock().unwrap().as_ref().is_some_and(|l| {
        l.complete_and_empty(
            latest_epoch,
            current_attempt,
            saw_fetch_failure,
            rooms_empty,
        )
    });
    proved && write_marker(&BrowserStore, &current_fingerprint(), now_ms())
}

fn current_fingerprint() -> String {
    fingerprint_for(
        &get_websocket_url(),
        current_delegate_key().bytes(),
        legacy_delegate_pairs(),
    )
}

/// [`fingerprint`] over the generated registry (`(key, code hash)` pairs).
fn fingerprint_for(node: &str, current: &[u8], registry: &[([u8; 32], [u8; 32])]) -> String {
    let legacy: Vec<[u8; 32]> = registry.iter().map(|(k, _)| *k).collect();
    fingerprint(node, current, &legacy)
}

/// The browser's localStorage. Natively (unit tests) every call fails, which
/// reads as "no marker".
struct BrowserStore;

#[cfg(target_arch = "wasm32")]
impl BrowserStore {
    fn storage() -> Result<web_sys::Storage, ()> {
        web_sys::window()
            .ok_or(())?
            .local_storage()
            .map_err(|_| ())?
            .ok_or(())
    }
}

#[cfg(target_arch = "wasm32")]
impl MarkerStore for BrowserStore {
    fn get(&self) -> Result<Option<String>, ()> {
        Self::storage()?.get_item(STORAGE_KEY).map_err(|_| ())
    }
    fn set(&self, value: &str) -> Result<(), ()> {
        Self::storage()?
            .set_item(STORAGE_KEY, value)
            .map_err(|_| ())
    }
    fn remove(&self) -> Result<(), ()> {
        Self::storage()?.remove_item(STORAGE_KEY).map_err(|_| ())
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl MarkerStore for BrowserStore {
    fn get(&self) -> Result<Option<String>, ()> {
        Err(())
    }
    fn set(&self, _value: &str) -> Result<(), ()> {
        Err(())
    }
    fn remove(&self) -> Result<(), ()> {
        Err(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct MemStore {
        value: RefCell<Option<String>>,
        fail: bool,
    }
    impl MarkerStore for MemStore {
        fn get(&self) -> Result<Option<String>, ()> {
            if self.fail {
                Err(())
            } else {
                Ok(self.value.borrow().clone())
            }
        }
        fn set(&self, v: &str) -> Result<(), ()> {
            if self.fail {
                return Err(());
            }
            *self.value.borrow_mut() = Some(v.to_string());
            Ok(())
        }
        fn remove(&self) -> Result<(), ()> {
            if self.fail {
                return Err(());
            }
            *self.value.borrow_mut() = None;
            Ok(())
        }
    }

    const T0: f64 = 1_800_000_000_000.0;
    const DAY: f64 = 24.0 * 60.0 * 60.0 * 1000.0;

    const NODE: &str = "wss://node-a.example/v1/contract/command";

    fn gens(n: u8) -> Vec<[u8; 32]> {
        (0..n).map(|i| [i; 32]).collect()
    }

    #[test]
    fn fingerprint_changes_when_a_generation_is_added() {
        let a = fingerprint(NODE, &[9; 32], &gens(3));
        let b = fingerprint(NODE, &[9; 32], &gens(4));
        assert_ne!(a, b);
        assert_eq!(a, fingerprint(NODE, &[9; 32], &gens(3)));
    }

    #[test]
    fn fingerprint_changes_with_generation_order() {
        let mut swapped = gens(3);
        swapped.swap(0, 1);
        assert_ne!(
            fingerprint(NODE, &[9; 32], &gens(3)),
            fingerprint(NODE, &[9; 32], &swapped)
        );
    }

    #[test]
    fn fingerprint_changes_when_the_current_delegate_changes() {
        assert_ne!(
            fingerprint(NODE, &[9; 32], &gens(3)),
            fingerprint(NODE, &[8; 32], &gens(3))
        );
    }

    /// A re-key turns the old current delegate into the newest legacy one and
    /// installs a new current: both inputs move, and the marker stops matching.
    #[test]
    fn a_rekey_invalidates_the_marker() {
        let before = fingerprint(NODE, &[9; 32], &gens(3));
        let mut after_legacy = gens(3);
        after_legacy.push([9; 32]);
        let after = fingerprint(NODE, &[10; 32], &after_legacy);
        assert_ne!(before, after);
    }

    #[test]
    fn fingerprint_is_per_node() {
        assert_ne!(
            fingerprint(NODE, &[9; 32], &gens(3)),
            fingerprint(
                "wss://node-b.example/v1/contract/command",
                &[9; 32],
                &gens(3)
            )
        );
    }

    #[test]
    fn marker_matches_only_the_exact_fingerprint() {
        let store = MemStore::default();
        let fp = fingerprint(NODE, &[9; 32], &gens(3));
        assert!(!marker_matches(&store, &fp, T0), "empty store");
        assert!(write_marker(&store, &fp, T0));
        assert!(marker_matches(&store, &fp, T0 + 1.0));
        assert!(!marker_matches(
            &store,
            &fingerprint(NODE, &[9; 32], &gens(4)),
            T0 + 1.0
        ));
    }

    #[test]
    fn an_unrecognised_value_is_no_marker() {
        let store = MemStore::default();
        let fp = fingerprint(NODE, &[9; 32], &gens(3));
        for junk in ["", "1", "v2:abc", "true", &fp[VALUE_PREFIX.len()..]] {
            *store.value.borrow_mut() = Some(junk.to_string());
            assert!(!marker_matches(&store, &fp, T0), "{junk:?}");
        }
    }

    #[test]
    fn storage_failure_means_sweep() {
        let store = MemStore {
            fail: true,
            ..Default::default()
        };
        let fp = fingerprint(NODE, &[9; 32], &gens(3));
        assert!(!marker_matches(&store, &fp, T0));
        assert!(!write_marker(&store, &fp, T0));
        clear_marker(&store); // must not panic
    }

    #[test]
    fn a_marker_older_than_seven_days_is_absent() {
        let store = MemStore::default();
        let fp = fingerprint(NODE, &[9; 32], &gens(3));
        assert!(write_marker(&store, &fp, T0));
        assert!(marker_matches(&store, &fp, T0 + 6.9 * DAY));
        assert!(!marker_matches(&store, &fp, T0 + 7.0 * DAY));
        assert!(!marker_matches(&store, &fp, T0 + 30.0 * DAY));
    }

    #[test]
    fn a_marker_stamped_in_the_future_is_absent() {
        let store = MemStore::default();
        let fp = fingerprint(NODE, &[9; 32], &gens(3));
        assert!(write_marker(&store, &fp, T0 + DAY));
        assert!(!marker_matches(&store, &fp, T0));
    }

    #[test]
    fn a_marker_without_a_valid_stamp_is_absent() {
        let store = MemStore::default();
        let fp = fingerprint(NODE, &[9; 32], &gens(3));
        for junk in [
            fp.clone(),
            format!("{fp}:"),
            format!("{fp}:x"),
            format!("{fp}:-5"),
        ] {
            *store.value.borrow_mut() = Some(junk.clone());
            assert!(!marker_matches(&store, &fp, T0), "{junk:?}");
        }
    }

    /// The real key must cover the real registry and the current delegate:
    /// dropping either would let a marker outlive a re-key.
    #[test]
    fn the_real_fingerprint_covers_the_registry_and_the_current_delegate() {
        let registry = legacy_delegate_pairs();
        assert!(!registry.is_empty(), "the generated registry is not empty");
        let current = [7u8; 32];
        let full = fingerprint_for(NODE, &current, registry);
        assert_ne!(
            full,
            fingerprint_for(NODE, &current, &registry[1..]),
            "dropping a generation changes the key"
        );
        assert_ne!(
            full,
            fingerprint_for(NODE, &[8u8; 32], registry),
            "changing the current delegate changes the key"
        );
        assert_ne!(full, fingerprint_for(NODE, &[], registry));
        assert_ne!(full, fingerprint_for("wss://other/", &current, registry));
        let keys: Vec<[u8; 32]> = registry.iter().map(|(k, _)| *k).collect();
        assert_eq!(full, fingerprint(NODE, &current, &keys));
        // And the production entry point goes through it.
        let src = include_str!("legacy_sweep_marker.rs");
        let body = fn_body(src, "fn current_fingerprint() -> String {");
        assert!(body.contains("current_delegate_key().bytes()"));
        assert!(body.contains("legacy_delegate_pairs()"));
        assert!(body.contains("get_websocket_url()"));
        let body = fn_body(src, "fn fingerprint_for(");
        assert!(body.contains("registry.iter().map(|(k, _)| *k)"));
        assert!(body.contains("fingerprint(node, current, &legacy)"));
    }

    #[test]
    fn clear_removes_the_marker() {
        let store = MemStore::default();
        let fp = fingerprint(NODE, &[9; 32], &gens(3));
        write_marker(&store, &fp, T0);
        clear_marker(&store);
        assert!(!marker_matches(&store, &fp, T0));
    }

    // ---- when the marker may be written ----

    const N: usize = 3;

    fn clean_ledger() -> SweepLedger {
        let mut l = SweepLedger::new(7, 2, N);
        for g in 0..N {
            l.note_reply(g, Reply::RoomsEmpty);
            l.note_reply(g, Reply::OutboundEmpty);
            l.note_reply(g, Reply::ListEmpty);
        }
        l.note_all_waves_sent();
        l
    }

    fn writable(l: &SweepLedger) -> bool {
        l.complete_and_empty(7, 2, false, true)
    }

    #[test]
    fn a_complete_empty_sweep_writes() {
        assert!(writable(&clean_ledger()));
    }

    #[test]
    fn absent_generations_count_as_answered() {
        let mut l = SweepLedger::new(7, 2, N);
        for g in 0..N {
            l.note_absent(g);
        }
        l.note_all_waves_sent();
        assert!(writable(&l));
    }

    #[test]
    fn an_unanswered_request_leaves_no_marker() {
        // A timed-out wave or a transport failure: one request never answered.
        let mut l = SweepLedger::new(7, 2, N);
        for g in 0..N {
            l.note_reply(g, Reply::RoomsEmpty);
            l.note_reply(g, Reply::OutboundEmpty);
            if g != 1 {
                l.note_reply(g, Reply::ListEmpty);
            }
        }
        l.note_all_waves_sent();
        assert!(!writable(&l));
    }

    #[test]
    fn a_never_answered_generation_leaves_no_marker() {
        let mut l = SweepLedger::new(7, 2, N);
        l.note_absent(0);
        l.note_absent(1);
        l.note_all_waves_sent();
        assert!(!writable(&l));
    }

    #[test]
    fn a_sweep_cut_short_leaves_no_marker() {
        let mut l = clean_ledger();
        l.all_waves_sent = false;
        assert!(!writable(&l));
    }

    #[test]
    fn found_data_leaves_no_marker() {
        let mut l = clean_ledger();
        l.note_reply(1, Reply::Data);
        assert!(l.found_data());
        assert!(!writable(&l));
    }

    #[test]
    fn a_superseded_sweep_leaves_no_marker() {
        let l = clean_ledger();
        assert!(!l.complete_and_empty(8, 2, false, true), "newer sweep");
        assert!(!l.complete_and_empty(7, 3, false, true), "newer attempt");
    }

    #[test]
    fn a_fetch_failure_or_non_empty_load_leaves_no_marker() {
        let l = clean_ledger();
        assert!(!l.complete_and_empty(7, 2, true, true));
        assert!(!l.complete_and_empty(7, 2, false, false));
    }

    #[test]
    fn an_empty_registry_never_writes() {
        let mut l = SweepLedger::new(7, 2, 0);
        l.note_all_waves_sent();
        assert!(!l.complete_and_empty(7, 2, false, true));
    }

    #[test]
    fn replies_classify_by_what_they_prove() {
        use river_core::chat_delegate::ChatDelegateKey;
        let get = |k: &[u8], v: Option<Vec<u8>>| ChatDelegateResponseMsg::GetResponse {
            key: ChatDelegateKey::new(k.to_vec()),
            value: v,
        };
        assert_eq!(
            classify_reply(&get(ROOMS_STORAGE_KEY, None)),
            Reply::RoomsEmpty
        );
        assert_eq!(
            classify_reply(&get(OUTBOUND_DMS_STORAGE_KEY, None)),
            Reply::OutboundEmpty
        );
        assert_eq!(
            classify_reply(&get(ROOMS_STORAGE_KEY, Some(vec![1]))),
            Reply::Data
        );
        assert_eq!(
            classify_reply(&get(OUTBOUND_DMS_STORAGE_KEY, Some(vec![1]))),
            Reply::Data
        );
        assert_eq!(
            classify_reply(&ChatDelegateResponseMsg::ListResponse { keys: vec![] }),
            Reply::ListEmpty
        );
        assert_eq!(
            classify_reply(&ChatDelegateResponseMsg::ListResponse {
                keys: vec![ChatDelegateKey::new(b"room:x".to_vec())]
            }),
            Reply::Data
        );
        assert_eq!(classify_reply(&get(b"other", None)), Reply::Other);
    }

    // ---- wiring pins (source scrape, like the sweep's other pins) ----

    /// The body of the production `fn` whose signature line starts a line (the
    /// pins elsewhere quote signatures inside string literals, which do not).
    fn fn_body<'a>(src: &'a str, sig: &str) -> &'a str {
        let start = src
            .find(&format!("\n{sig}"))
            .unwrap_or_else(|| panic!("fn not found: {sig}"));
        let rest = &src[start + 1..];
        &rest[..rest.find("\n}\n").expect("fn end")]
    }

    #[test]
    fn the_sweep_skips_when_the_marker_matches() {
        let body = fn_body(
            include_str!("chat_delegate.rs"),
            "pub(crate) async fn fire_legacy_migration_request() -> bool {",
        );
        let skip = body
            .find("legacy_sweep_marker::marker_matches_now()")
            .expect("fire_legacy_migration_request must consult the marker");
        let swap = body
            .find("LEGACY_MIGRATION_ATTEMPTED.swap(")
            .expect("guard swap");
        assert!(skip < swap, "the marker is checked before the sweep starts");
        assert!(body.contains("legacy_sweep_marker::begin_sweep("));
        assert_eq!(
            body.matches("legacy_sweep_marker::note_all_waves_sent(")
                .count(),
            2,
            "single-wave end and last-wave end"
        );
        // The last-wave note sits inside the `!cut_short` branch.
        let guard = body
            .find("if !cut_short && load_attempt_is_current(attempt) {")
            .expect("the last-wave note is guarded by !cut_short");
        let last_note = body
            .rfind("legacy_sweep_marker::note_all_waves_sent(")
            .unwrap();
        let close = body[guard..].find("\n        }\n").expect("guard end");
        assert!(
            last_note > guard && last_note < guard + close,
            "note_all_waves_sent must be inside the !cut_short branch"
        );
        // The mid-wave break marks the sweep cut short.
        assert!(body.contains("cut_short = true;\n                    break;"));
    }

    #[test]
    fn the_marker_is_written_only_at_the_genuine_empty_resolution() {
        let whole = include_str!("chat_delegate.rs");
        assert_eq!(
            whole.matches("write_marker_if_sweep_proved_empty(").count(),
            1,
            "no other path may write the marker"
        );
        let idle = fn_body(
            whole,
            "fn schedule_idle_resolution(armed_gen: u32, armed_attempt: u32) {",
        );
        // Real values, not constants: the failure flag and the room map.
        assert!(idle.contains(
            "SAW_FETCH_FAILURE.load(Ordering::Relaxed),\n                    rooms_map_is_empty(),"
        ));
        let failed = idle.find("RoomsLoadState::LoadFailed").unwrap();
        let write = idle
            .find("write_marker_if_sweep_proved_empty(")
            .expect("the idle resolution writes the marker");
        assert!(write > failed, "not on the failure branch");
    }

    #[test]
    fn retry_and_recovery_clear_the_marker() {
        let src = include_str!("chat_delegate.rs");
        for sig in [
            "pub(crate) fn retry_rooms_load() {",
            "pub(crate) fn arm_legacy_migration_recovery() -> bool {",
        ] {
            assert!(fn_body(src, sig).contains("clear_marker_now()"), "{sig}");
        }
    }

    #[test]
    fn replies_and_absences_feed_the_ledger() {
        let handler = include_str!("freenet_api/response_handler.rs");
        assert!(handler.contains("legacy_sweep_marker::note_legacy_reply("));
        let sync = include_str!("freenet_api/freenet_synchronizer.rs");
        assert!(sync.contains("legacy_sweep_marker::note_legacy_absent("));
    }

    /// An empty `values` reply proves nothing about the delegate: it counts as
    /// a pacing answer but never as "answered absent".
    #[test]
    fn an_empty_values_reply_is_not_an_absence() {
        let handler = include_str!("freenet_api/response_handler.rs");
        let start = handler
            .find("if values.is_empty() {")
            .expect("the empty-values branch");
        let branch = &handler[start..];
        let branch = &branch[..branch.find("\n                    }\n").unwrap()];
        assert!(branch.contains("note_sweep_answer("));
        assert!(!branch.contains("note_legacy_absent"));
        // And a ledger given only pacing answers writes nothing.
        let mut l = SweepLedger::new(7, 2, N);
        l.note_all_waves_sent();
        assert!(!l.complete_and_empty(7, 2, false, true));
    }

    #[test]
    fn the_empty_state_offers_a_look_for_older_rooms() {
        let list = include_str!("../room_list.rs");
        let start = list
            .find("RoomListDisplay::Empty => rsx!")
            .expect("empty arm");
        let arm = &list[start..];
        let arm = &arm[..arm.find("RoomListDisplay::LoadFailed").unwrap()];
        assert!(arm.contains("Look for rooms from older versions"));
        assert!(arm.contains("onclick: move |_| retry_rooms_load()"));
        // `retry_rooms_load` clears the marker and the session guard, then
        // re-runs the load, which sweeps.
        let body = fn_body(
            include_str!("chat_delegate.rs"),
            "pub(crate) fn retry_rooms_load() {",
        );
        assert!(body.contains("LEGACY_MIGRATION_ATTEMPTED.store(false"));
        assert!(body.contains("clear_marker_now()"));
        assert!(body.contains("set_up_chat_delegate_registering()"));
    }
}
