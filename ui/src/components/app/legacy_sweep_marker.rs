//! A persistent "swept every legacy delegate generation, found nothing" marker,
//! so the legacy room sweep runs at most once per user and node instead of on
//! every page load of a user with no rooms (freenet/river#972).
//!
//! # Where it lives
//!
//! In the CURRENT chat delegate's key-value store, under
//! [`LEGACY_SWEEP_MARKER_KEY`], through the same plain `GetRequest`,
//! `StoreRequest` and `DeleteRequest` the UI uses for room data. NOT in browser
//! storage: the River UI runs in a sandboxed opaque-origin iframe where every
//! browser-storage access throws, so a browser-side marker can never be written
//! (the first attempt, #1067, was inert for that reason). The delegate store is
//! per node and, on the hosted node, per user, and a delegate re-key starts a
//! new, empty store, so a re-key resets the marker without any bookkeeping.
//!
//! # Why it is safe
//!
//! The marker is a claim that no legacy generation holds data. It is only worth
//! anything if it is never wrong in the dangerous direction (skipping a sweep
//! that would have found rooms), so:
//!
//! * It records the CURRENT delegate key and the whole ordered legacy
//!   generation list. A re-key changes both (the node scope comes from the
//!   store itself).
//! * It is written only when [`SweepLedger::complete_and_empty`] holds: the
//!   sweep sent every wave, every generation answered all three of its requests
//!   with a typed "delegate missing" (or an explicit empty answer), no
//!   generation held data, and no fetch failed. Silence, a timed-out wave, a
//!   refusal that is not a typed "missing", an empty reply, a transport failure
//!   and a superseded sweep all leave it unwritten.
//! * It carries its write time and is ignored after seven days.
//! * Any read or write failure, a timeout, or a value that is not exactly this
//!   build's format and fingerprint reads as "no marker": the sweep runs as it
//!   always did.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use futures::future::{select, Either};
use river_core::chat_delegate::{ChatDelegateKey, ChatDelegateRequestMsg, ChatDelegateResponseMsg};

use super::freenet_api::delegate_migration::is_migration_bookkeeping_key;

use super::chat_delegate::{
    await_delegate_response, current_delegate_key, enqueue_delegate_request, legacy_delegate_pairs,
    send_delegate_request, OUTBOUND_DMS_STORAGE_KEY, ROOMS_STORAGE_KEY,
};

/// The current delegate's storage key for the marker. A bookkeeping key: never
/// migrated, never counted as room data (`is_migration_bookkeeping_key`).
pub(crate) const LEGACY_SWEEP_MARKER_KEY: &[u8] = b"__river_legacy_sweep_empty__";

/// Format version of the stored value (`v3:<fingerprint hex>:<written at, ms>`).
/// A value with any other prefix is ignored.
const VALUE_PREFIX: &str = "v3:";

/// How long a marker is believed. It bounds a wrong marker in time: rooms
/// written later to a legacy generation by an old tab or an old desktop build
/// are found again by the next sweep after this.
const MARKER_MAX_AGE_MS: f64 = 7.0 * 24.0 * 60.0 * 60.0 * 1000.0;
/// A stamp this far ahead of the clock is not believed (clock changed).
const MARKER_FUTURE_SLACK_MS: f64 = 60.0 * 1000.0;

/// The longest the load waits for the marker read. The delegate answers in one
/// round trip when it is up; this only bounds the failure case, after which the
/// sweep runs as it always did.
const MARKER_READ_TIMEOUT_MS: u64 = 2_000;

/// The fingerprint of "what this marker covers": the current delegate and the
/// ordered legacy generation list. Length-prefixed so no two inputs share an
/// encoding.
pub(crate) fn fingerprint(current: &[u8], legacy: &[[u8; 32]]) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"river-legacy-sweep-empty-v3");
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

/// Whether the delegate's reply to the marker `GetRequest` is a fresh marker
/// for `fingerprint`. A failed request, any other reply, an absent value, bytes
/// that are not UTF-8 and every unrecognised or stale value read as no marker.
/// Pure.
pub(crate) fn marker_from_read(
    reply: Result<ChatDelegateResponseMsg, String>,
    fingerprint: &str,
    now_ms: f64,
) -> bool {
    match reply {
        Ok(ChatDelegateResponseMsg::GetResponse {
            value: Some(bytes), ..
        }) => {
            std::str::from_utf8(&bytes).is_ok_and(|v| value_is_fresh_match(v, fingerprint, now_ms))
        }
        _ => false,
    }
}

fn marker_get_request() -> ChatDelegateRequestMsg {
    ChatDelegateRequestMsg::GetRequest {
        key: ChatDelegateKey::new(LEGACY_SWEEP_MARKER_KEY.to_vec()),
    }
}

/// The request that writes the marker, stamped `now_ms`.
pub(crate) fn marker_store_request(fingerprint: &str, now_ms: f64) -> ChatDelegateRequestMsg {
    ChatDelegateRequestMsg::StoreRequest {
        key: ChatDelegateKey::new(LEGACY_SWEEP_MARKER_KEY.to_vec()),
        value: stored_value(fingerprint, now_ms).into_bytes(),
    }
}

fn marker_delete_request() -> ChatDelegateRequestMsg {
    ChatDelegateRequestMsg::DeleteRequest {
        key: ChatDelegateKey::new(LEGACY_SWEEP_MARKER_KEY.to_vec()),
    }
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
            // Keys that are only this delegate's own bookkeeping (the marker,
            // the walk's markers, cadence and journal records): once the
            // delegate is a legacy generation they must not count as room data.
            if keys
                .iter()
                .all(|k| is_migration_bookkeeping_key(k.as_bytes()))
            {
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
        // Found data: forget any marker rather than wait for it to age out.
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

/// Set by a Retry or an interrupted-migration recovery: the next sweep must
/// not trust (or wait for a read of) the marker.
static FRESH_SWEEP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// The user asked to look again (Retry, "Look for rooms from older versions")
/// or a migration was interrupted: the next sweep ignores the marker and
/// removes it. Synchronous, so it cannot race the read it replaces.
pub(crate) fn request_fresh_sweep() {
    FRESH_SWEEP_REQUESTED.store(true, Ordering::Relaxed);
}

/// Consume [`request_fresh_sweep`]; when it was set, also remove the stored
/// marker (best effort, in the background).
pub(crate) fn take_fresh_sweep_request() -> bool {
    let requested = FRESH_SWEEP_REQUESTED.swap(false, Ordering::Relaxed);
    if requested {
        clear_marker_now();
    }
    requested
}

/// Whether the current delegate holds a fresh marker covering this delegate
/// and generation list. Costs one delegate round trip, bounded by
/// [`MARKER_READ_TIMEOUT_MS`]; any failure or timeout is `false`.
pub(crate) async fn marker_matches_now() -> bool {
    let fingerprint = current_fingerprint();
    let read = Box::pin(async {
        let pending = enqueue_delegate_request(marker_get_request()).await?;
        await_delegate_response(pending).await
    });
    let timeout = Box::pin(crate::util::sleep(std::time::Duration::from_millis(
        MARKER_READ_TIMEOUT_MS,
    )));
    match select(read, timeout).await {
        Either::Left((reply, _)) => marker_from_read(reply, &fingerprint, now_ms()),
        Either::Right(_) => false,
    }
}

/// Remove the marker from the current delegate, in the background.
pub(crate) fn clear_marker_now() {
    crate::util::safe_spawn_local(async {
        let _ = send_delegate_request(marker_delete_request()).await;
    });
}

/// Write the marker if the latest sweep proved every generation empty. Called
/// only where the load resolves to a genuine Empty. The write itself runs in
/// the background; its failure leaves no marker.
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
    if proved {
        let request = marker_store_request(&current_fingerprint(), now_ms());
        crate::util::safe_spawn_local(async move {
            let _ = send_delegate_request(request).await;
        });
    }
    proved
}

fn current_fingerprint() -> String {
    fingerprint_for(current_delegate_key().bytes(), legacy_delegate_pairs())
}

/// [`fingerprint`] over the generated registry (`(key, code hash)` pairs).
fn fingerprint_for(current: &[u8], registry: &[([u8; 32], [u8; 32])]) -> String {
    let legacy: Vec<[u8; 32]> = registry.iter().map(|(k, _)| *k).collect();
    fingerprint(current, &legacy)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: f64 = 1_800_000_000_000.0;
    const DAY: f64 = 24.0 * 60.0 * 60.0 * 1000.0;

    fn gens(n: u8) -> Vec<[u8; 32]> {
        (0..n).map(|i| [i; 32]).collect()
    }

    fn get_reply(value: Option<&str>) -> Result<ChatDelegateResponseMsg, String> {
        Ok(ChatDelegateResponseMsg::GetResponse {
            key: ChatDelegateKey::new(LEGACY_SWEEP_MARKER_KEY.to_vec()),
            value: value.map(|v| v.as_bytes().to_vec()),
        })
    }

    #[test]
    fn fingerprint_changes_when_a_generation_is_added() {
        let a = fingerprint(&[9; 32], &gens(3));
        assert_ne!(a, fingerprint(&[9; 32], &gens(4)));
        assert_eq!(a, fingerprint(&[9; 32], &gens(3)));
    }

    #[test]
    fn fingerprint_changes_with_generation_order() {
        let mut swapped = gens(3);
        swapped.swap(0, 1);
        assert_ne!(
            fingerprint(&[9; 32], &gens(3)),
            fingerprint(&[9; 32], &swapped)
        );
    }

    #[test]
    fn fingerprint_changes_when_the_current_delegate_changes() {
        assert_ne!(
            fingerprint(&[9; 32], &gens(3)),
            fingerprint(&[8; 32], &gens(3))
        );
    }

    /// A re-key turns the old current delegate into the newest legacy one and
    /// installs a new current: both inputs move, and the marker stops matching.
    #[test]
    fn a_rekey_invalidates_the_marker() {
        let before = fingerprint(&[9; 32], &gens(3));
        let mut after_legacy = gens(3);
        after_legacy.push([9; 32]);
        assert_ne!(before, fingerprint(&[10; 32], &after_legacy));
    }

    #[test]
    fn a_read_matches_only_the_exact_fresh_fingerprint() {
        let fp = fingerprint(&[9; 32], &gens(3));
        let stored = stored_value(&fp, T0);
        assert!(marker_from_read(get_reply(Some(&stored)), &fp, T0 + 1.0));
        assert!(!marker_from_read(
            get_reply(Some(&stored)),
            &fingerprint(&[9; 32], &gens(4)),
            T0 + 1.0
        ));
        assert!(!marker_from_read(get_reply(None), &fp, T0), "no value");
    }

    #[test]
    fn an_unrecognised_value_is_no_marker() {
        let fp = fingerprint(&[9; 32], &gens(3));
        for junk in ["", "1", "v2:abc", "true", &fp[VALUE_PREFIX.len()..]] {
            assert!(
                !marker_from_read(get_reply(Some(junk)), &fp, T0),
                "{junk:?}"
            );
        }
        let not_utf8 = Ok(ChatDelegateResponseMsg::GetResponse {
            key: ChatDelegateKey::new(LEGACY_SWEEP_MARKER_KEY.to_vec()),
            value: Some(vec![0xff, 0xfe]),
        });
        assert!(!marker_from_read(not_utf8, &fp, T0));
    }

    /// A failed or odd reply is "no marker": the sweep runs.
    #[test]
    fn a_failed_read_means_sweep() {
        let fp = fingerprint(&[9; 32], &gens(3));
        assert!(!marker_from_read(Err("Timeout".into()), &fp, T0));
        assert!(!marker_from_read(
            Ok(ChatDelegateResponseMsg::ListResponse { keys: vec![] }),
            &fp,
            T0
        ));
    }

    #[test]
    fn a_marker_older_than_seven_days_is_absent() {
        let fp = fingerprint(&[9; 32], &gens(3));
        let stored = stored_value(&fp, T0);
        assert!(marker_from_read(
            get_reply(Some(&stored)),
            &fp,
            T0 + 6.9 * DAY
        ));
        assert!(!marker_from_read(
            get_reply(Some(&stored)),
            &fp,
            T0 + 7.0 * DAY
        ));
        assert!(!marker_from_read(
            get_reply(Some(&stored)),
            &fp,
            T0 + 30.0 * DAY
        ));
    }

    #[test]
    fn a_marker_stamped_in_the_future_is_absent() {
        let fp = fingerprint(&[9; 32], &gens(3));
        let stored = stored_value(&fp, T0 + DAY);
        assert!(!marker_from_read(get_reply(Some(&stored)), &fp, T0));
    }

    #[test]
    fn a_marker_without_a_valid_stamp_is_absent() {
        let fp = fingerprint(&[9; 32], &gens(3));
        for junk in [
            fp.clone(),
            format!("{fp}:"),
            format!("{fp}:x"),
            format!("{fp}:-5"),
        ] {
            assert!(
                !marker_from_read(get_reply(Some(&junk)), &fp, T0),
                "{junk:?}"
            );
        }
    }

    /// What is written is what is read back, under the bookkeeping key.
    #[test]
    fn the_store_request_round_trips_through_the_read() {
        let fp = fingerprint(&[9; 32], &gens(3));
        let ChatDelegateRequestMsg::StoreRequest { key, value } = marker_store_request(&fp, T0)
        else {
            panic!("the marker is written with a plain StoreRequest");
        };
        assert_eq!(key.as_bytes(), LEGACY_SWEEP_MARKER_KEY);
        let reply = Ok(ChatDelegateResponseMsg::GetResponse {
            key,
            value: Some(value),
        });
        assert!(marker_from_read(reply, &fp, T0 + 1.0));
        assert!(matches!(
            marker_get_request(),
            ChatDelegateRequestMsg::GetRequest { key } if key.as_bytes() == LEGACY_SWEEP_MARKER_KEY
        ));
        assert!(matches!(
            marker_delete_request(),
            ChatDelegateRequestMsg::DeleteRequest { key } if key.as_bytes() == LEGACY_SWEEP_MARKER_KEY
        ));
    }

    /// The real key must cover the real registry and the current delegate:
    /// dropping either would let a marker outlive a re-key.
    #[test]
    fn the_real_fingerprint_covers_the_registry_and_the_current_delegate() {
        let registry = legacy_delegate_pairs();
        assert!(!registry.is_empty(), "the generated registry is not empty");
        let current = [7u8; 32];
        let full = fingerprint_for(&current, registry);
        assert_ne!(
            full,
            fingerprint_for(&current, &registry[1..]),
            "dropping a generation changes the key"
        );
        assert_ne!(
            full,
            fingerprint_for(&[8u8; 32], registry),
            "changing the current delegate changes the key"
        );
        assert_ne!(full, fingerprint_for(&[], registry));
        let keys: Vec<[u8; 32]> = registry.iter().map(|(k, _)| *k).collect();
        assert_eq!(full, fingerprint(&current, &keys));
        // And the production entry point goes through it.
        let src = include_str!("legacy_sweep_marker.rs");
        let body = fn_body(src, "fn current_fingerprint() -> String {");
        assert!(body.contains("current_delegate_key().bytes()"));
        assert!(body.contains("legacy_delegate_pairs()"));
        let body = fn_body(src, "fn fingerprint_for(");
        assert!(body.contains("registry.iter().map(|(k, _)| *k)"));
        assert!(body.contains("fingerprint(current, &legacy)"));
    }

    /// The marker lives in the delegate. The UI iframe has no browser storage,
    /// so any use of it here would be inert (#1067).
    #[test]
    fn the_marker_never_touches_browser_storage() {
        let src = include_str!("legacy_sweep_marker.rs");
        let production = src.split("#[cfg(test)]\nmod tests {").next().unwrap();
        for needle in [
            ["local", "_storage"].concat(),
            ["local", "Storage"].concat(),
            ["session", "_storage"].concat(),
            ["session", "Storage"].concat(),
            ["web_sys", "::Storage"].concat(),
            ["Indexed", "Db"].concat(),
            ["document", ".cookie"].concat(),
        ] {
            assert!(
                !production.contains(&needle),
                "browser storage use: {needle}"
            );
        }
    }

    /// The new key is bookkeeping: the walk never copies it, the room load
    /// swallows its reply, and a listing of only this key is not room data.
    #[test]
    fn the_marker_key_is_bookkeeping() {
        assert!(is_migration_bookkeeping_key(LEGACY_SWEEP_MARKER_KEY));
        let only_marker = ChatDelegateResponseMsg::ListResponse {
            keys: vec![ChatDelegateKey::new(LEGACY_SWEEP_MARKER_KEY.to_vec())],
        };
        assert_eq!(classify_reply(&only_marker), Reply::ListEmpty);
        let with_room = ChatDelegateResponseMsg::ListResponse {
            keys: vec![
                ChatDelegateKey::new(LEGACY_SWEEP_MARKER_KEY.to_vec()),
                ChatDelegateKey::new(b"room:x".to_vec()),
            ],
        };
        assert_eq!(classify_reply(&with_room), Reply::Data);
    }

    /// Once this generation is a legacy one, its walk markers, cadence and
    /// journal records are not room data either; real data keys still are.
    #[test]
    fn a_listing_of_only_bookkeeping_keys_is_not_data() {
        use super::super::freenet_api::delegate_migration::{
            KEY_INDEX_STORAGE_KEY, KV_INTENT_STORAGE_KEY, MIGRATE_PRED_DONE_PREFIX,
            SHORT_INDEX_KEY, TOPUP_WINDOW_KEY, WALK_CADENCE_KEY,
        };
        let list = |keys: Vec<Vec<u8>>| ChatDelegateResponseMsg::ListResponse {
            keys: keys.into_iter().map(ChatDelegateKey::new).collect(),
        };
        let mut pred_done = MIGRATE_PRED_DONE_PREFIX.to_vec();
        pred_done.extend_from_slice(b"abc");
        let bookkeeping = vec![
            LEGACY_SWEEP_MARKER_KEY.to_vec(),
            WALK_CADENCE_KEY.to_vec(),
            TOPUP_WINDOW_KEY.to_vec(),
            SHORT_INDEX_KEY.to_vec(),
            KEY_INDEX_STORAGE_KEY.to_vec(),
            KV_INTENT_STORAGE_KEY.to_vec(),
            pred_done,
        ];
        assert_eq!(classify_reply(&list(bookkeeping.clone())), Reply::ListEmpty);
        for data in [
            ROOMS_STORAGE_KEY.to_vec(),
            OUTBOUND_DMS_STORAGE_KEY.to_vec(),
            b"rooms_meta".to_vec(),
            b"room:x".to_vec(),
        ] {
            let mut keys = bookkeeping.clone();
            keys.push(data.clone());
            assert_eq!(
                classify_reply(&list(keys)),
                Reply::Data,
                "{:?}",
                String::from_utf8_lossy(&data)
            );
        }
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
            .find("legacy_sweep_marker::marker_matches_now().await")
            .expect("fire_legacy_migration_request must consult the marker");
        let fresh = body
            .find("legacy_sweep_marker::take_fresh_sweep_request()")
            .expect("a Retry or recovery bypasses the marker");
        assert!(fresh < skip, "the bypass is decided before the read");
        let swap = body
            .find("LEGACY_MIGRATION_ATTEMPTED.swap(")
            .expect("guard swap");
        let epoch = body
            .find("SWEEP_EPOCH.fetch_add(")
            .expect("the sweep's epoch");
        assert!(
            swap < skip && skip < epoch,
            "the marker is read when a sweep would start, before it does"
        );
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

    /// A Retry or reconnect that begins a new attempt while the marker read is
    /// pending must not let the old call start or supersede a sweep.
    #[test]
    fn a_new_attempt_during_the_marker_read_stops_the_old_call() {
        let body = fn_body(
            include_str!("chat_delegate.rs"),
            "pub(crate) async fn fire_legacy_migration_request() -> bool {",
        );
        let capture = body
            .find("let attempt_at_read = LOAD_ATTEMPT_GEN.load(")
            .expect("the attempt is captured before the read");
        let read = body
            .find("legacy_sweep_marker::marker_matches_now().await")
            .expect("the read");
        let recheck = body
            .find("if !load_attempt_is_current(attempt_at_read) {")
            .expect("the attempt is rechecked after the read");
        let epoch = body.find("SWEEP_EPOCH.fetch_add(").expect("sweep start");
        assert!(capture < read && read < recheck && recheck < epoch);
        let stale = &body[recheck..epoch];
        assert!(
            stale.contains("return false;"),
            "a stale call starts nothing"
        );
        // The session guard this call took is handed back, but only when no
        // sweep started meanwhile.
        assert!(stale.contains("!already && SWEEP_EPOCH.load(Ordering::Relaxed) == epoch_at_read"));
        assert!(stale.contains("LEGACY_MIGRATION_ATTEMPTED.store(false"));
    }

    /// `note_legacy_absent` may be reached only from the typed "delegate
    /// missing for a legacy generation" arm, never from the generic error path
    /// that also sees refusals and execution errors.
    #[test]
    fn absence_is_noted_only_for_a_typed_missing_legacy_delegate() {
        let sync = include_str!("freenet_api/freenet_synchronizer.rs");
        let production = sync.split("#[cfg(test)]\nmod tests {").next().unwrap();
        assert_eq!(
            production
                .matches("legacy_sweep_marker::note_legacy_absent(")
                .count(),
            1
        );
        let arm = production
            .find("if let Some(key) = legacy_not_installed_key(&e) {")
            .expect("the typed legacy-missing arm");
        let call = production
            .find("legacy_sweep_marker::note_legacy_absent(")
            .unwrap();
        let arm_end = arm + production[arm..].find("} else if ").expect("arm end");
        assert!(arm < call && call < arm_end, "inside the typed-missing arm");
    }

    #[test]
    fn retry_and_recovery_ask_for_a_fresh_sweep() {
        let src = include_str!("chat_delegate.rs");
        for sig in [
            "pub(crate) fn retry_rooms_load() {",
            "pub(crate) fn arm_legacy_migration_recovery() -> bool {",
        ] {
            assert!(
                fn_body(src, sig).contains("legacy_sweep_marker::request_fresh_sweep()"),
                "{sig}"
            );
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
        // `retry_rooms_load` asks for a fresh sweep (ignore and remove the
        // marker), clears the session guard, then re-runs the load.
        let body = fn_body(
            include_str!("chat_delegate.rs"),
            "pub(crate) fn retry_rooms_load() {",
        );
        assert!(body.contains("LEGACY_MIGRATION_ATTEMPTED.store(false"));
        assert!(body.contains("request_fresh_sweep()"));
        assert!(body.contains("set_up_chat_delegate_registering()"));
        // And the sweep consumes the request and removes the stored marker.
        let take = fn_body(
            include_str!("legacy_sweep_marker.rs"),
            "pub(crate) fn take_fresh_sweep_request() -> bool {",
        );
        assert!(take.contains("clear_marker_now()"));
    }
}
