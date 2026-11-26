//! Resolving a short room link, `?room=<handle>` (freenet/river#973).
//!
//! A handle names a room indirectly: the generation 1 resolver contract for
//! the handle holds the owner key, and [`crate::short_handle`] checks that key
//! against the handle (and, for the Official room, against its known key)
//! before anything else happens. Only a verified key is handed on to
//! [`crate::public_room`], which then treats it exactly as a `?room=<key>`
//! link: the resolver adds one lookup in front of the room's own and nothing
//! else changes.
//!
//! The full link stays the canonical share link. A page opened with a full key
//! never reaches this module (no resolver request is sent: pinned by
//! [`lookups_sent`] and the Playwright spec), and "Copy short link" is offered
//! only after [`request_check`] has found and verified the entry.
//!
//! # Where the answers arrive
//!
//! Like the room lookup, the resolve GET is sent as soon as the socket is
//! connected, not gated on the user's rooms loading, and its answers come back
//! through the ordinary `GetResponse` / `NotFound` / error paths, which offer
//! them here first ([`deliver_state`], [`deliver_not_found`],
//! [`deliver_error`]). A resolver contract is never one of the user's rooms, so
//! no answer is held for the rooms to load.

use crate::public_room::{RoomLinkError, ViewedRoomStatus};
use crate::short_handle::{resolve_state, ShortError, ShortHandle};
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::prelude::ContractInstanceId;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// Same rules as the room lookup: one retry after 7 s, give up after 20 s.
const RESOLVE_RETRY_AFTER: Duration = Duration::from_secs(7);
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(20);
/// A check that found nothing is repeated after this long, when the share
/// dialog is opened again.
const CHECK_RECHECK_AFTER_MS: f64 = 60_000.0;

/// What the page knows about the handle it is resolving.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolveMirror {
    pub handle: ShortHandle,
    pub id: ContractInstanceId,
    /// Set when this page has sent its own resolver GET.
    pub requested: bool,
    /// Resolved, or refused: nothing more is accepted.
    pub done: bool,
    pub status: ViewedRoomStatus,
}

impl ResolveMirror {
    pub fn new(handle: ShortHandle) -> Self {
        Self {
            handle,
            id: handle.resolver_instance_id(),
            requested: false,
            done: false,
            status: ViewedRoomStatus::Resolving,
        }
    }
}

static MIRROR: Mutex<Option<ResolveMirror>> = Mutex::new(None);

fn mirror() -> Option<ResolveMirror> {
    MIRROR.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn with_mirror<R>(f: impl FnOnce(&mut ResolveMirror) -> R) -> Option<R> {
    MIRROR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
        .map(f)
}

/// Start resolving `handle` (a page opened on a short link). Replaces any
/// earlier resolution.
pub fn begin(handle: ShortHandle) {
    note_seen(handle.resolver_instance_id());
    *MIRROR.lock().unwrap_or_else(|e| e.into_inner()) = Some(ResolveMirror::new(handle));
}

/// Stop resolving (the viewer closed).
pub fn end() {
    *MIRROR.lock().unwrap_or_else(|e| e.into_inner()) = None;
    GEN.fetch_add(1, Ordering::SeqCst);
}

/// Resolver GETs this page actually sent, retries included, for the "a full link sends no extra
/// lookup" pin.
static LOOKUPS_SENT: AtomicU32 = AtomicU32::new(0);

#[cfg_attr(
    not(all(feature = "example-data", feature = "no-sync")),
    allow(dead_code)
)]
pub fn lookups_sent() -> u32 {
    LOOKUPS_SENT.load(Ordering::SeqCst)
}

/// A network answer about a resolver contract.
#[derive(Debug, Clone, Copy)]
pub enum Answer<'a> {
    State(&'a [u8]),
    NotFound,
}

/// What an answer does to the viewed link.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Nothing to change.
    Ignore,
    /// A verified owner key: open the room as a `?room=<key>` link.
    Resolved(VerifyingKey),
    /// Show this instead.
    Status(ViewedRoomStatus),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Routed {
    /// The caller must stop: the answer was the resolver's.
    pub claimed: bool,
    pub outcome: Outcome,
}

const NOT_CLAIMED: Routed = Routed {
    claimed: false,
    outcome: Outcome::Ignore,
};

/// The refusal a failed verification shows.
pub fn refusal(e: &ShortError) -> RoomLinkError {
    match e {
        ShortError::Mismatch => RoomLinkError::HandleMismatch,
        ShortError::OfficialMismatch => RoomLinkError::OfficialMismatch,
        ShortError::BadHandle => RoomLinkError::BadHandle,
        ShortError::BadState(_) | ShortError::BadKey => RoomLinkError::HandleMalformed,
    }
}

/// Decide what a resolver answer means. Pure, so it is unit-tested.
///
/// The answer is the viewer's only if this page asked and the answer is for the
/// handle's resolver contract. A state is always verified against the handle
/// before it can open anything; a refusal is final for this page.
pub fn route(m: Option<&ResolveMirror>, id: &ContractInstanceId, answer: Answer) -> Routed {
    let Some(m) = m else { return NOT_CLAIMED };
    if &m.id != id || !m.requested {
        return NOT_CLAIMED;
    }
    if m.done {
        // A late duplicate (the retry's answer): swallowed, not a room's.
        return Routed {
            claimed: true,
            outcome: Outcome::Ignore,
        };
    }
    let outcome = match answer {
        Answer::State(bytes) => match resolve_state(&m.handle, bytes) {
            Ok(key) => match VerifyingKey::from_bytes(&key) {
                Ok(vk) => Outcome::Resolved(vk),
                Err(_) => {
                    Outcome::Status(ViewedRoomStatus::Invalid(RoomLinkError::HandleMalformed))
                }
            },
            Err(e) => Outcome::Status(ViewedRoomStatus::Invalid(refusal(&e))),
        },
        Answer::NotFound if m.status == ViewedRoomStatus::HandleNotFound => Outcome::Ignore,
        Answer::NotFound => Outcome::Status(ViewedRoomStatus::HandleNotFound),
    };
    Routed {
        claimed: true,
        outcome,
    }
}

/// What to do with an error from the node.
#[derive(Debug, PartialEq, Eq)]
pub enum ErrorAction {
    /// Not about the resolver lookup.
    Ignore,
    /// A first failure: send the one retry now.
    Retry,
    /// The retry has been sent already: report a transport error.
    GiveUp,
}

/// Classify an error: it counts when this page asked, is still waiting, and the
/// error names the resolver contract. Pure, so it is unit-tested.
pub fn error_action(m: Option<&ResolveMirror>, error_text: &str, retried: bool) -> ErrorAction {
    let for_resolver = m.is_some_and(|m| {
        m.requested
            && !m.done
            && m.status == ViewedRoomStatus::Resolving
            && error_text.contains(&m.id.to_string())
    });
    match (for_resolver, retried) {
        (false, _) => ErrorAction::Ignore,
        (true, false) => ErrorAction::Retry,
        (true, true) => ErrorAction::GiveUp,
    }
}

/// Whether the timer of the resolution numbered `current` should still act.
fn still_waiting(m: Option<&ResolveMirror>, handle: &ShortHandle, current: bool) -> bool {
    current
        && m.is_some_and(|m| {
            &m.handle == handle && m.requested && !m.done && m.status == ViewedRoomStatus::Resolving
        })
}

/// When the pane should send the resolver GET: connected (the no-sync example
/// build has no socket and answers from fixtures), not yet sent for this
/// handle. Pure, so it is unit-tested.
pub fn should_request(
    connected: bool,
    asked_for: Option<ShortHandle>,
    handle: ShortHandle,
    status: &ViewedRoomStatus,
) -> bool {
    connected && asked_for != Some(handle) && *status == ViewedRoomStatus::Resolving
}

/// The line shown while the link is being resolved.
pub fn waiting_hint(connected: bool) -> &'static str {
    if connected {
        "Looking up this short link. This can take a few seconds."
    } else {
        "Connecting to Freenet\u{2026}"
    }
}

/// Resolver ids this page has asked about. A late answer for one of them (after
/// the viewer closed or moved on, or for a check that timed out) is swallowed
/// here instead of falling through to the room handlers as an unknown contract.
static SEEN: Mutex<Option<std::collections::HashSet<ContractInstanceId>>> = Mutex::new(None);

fn note_seen(id: ContractInstanceId) {
    SEEN.lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(Default::default)
        .insert(id);
}

fn was_seen(id: &ContractInstanceId) -> bool {
    SEEN.lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|s| s.contains(id))
}

static GEN: AtomicU64 = AtomicU64::new(0);
static RETRIED: AtomicBool = AtomicBool::new(false);

fn log(event: &str) {
    crate::public_room::lookup_log(&format!("short link: {event}"));
}

/// Record a new status for the link being resolved and show it.
fn set_status(handle: ShortHandle, status: ViewedRoomStatus, done: bool) {
    let applied = with_mirror(|m| {
        if m.handle == handle {
            m.status = status.clone();
            m.done = done;
            true
        } else {
            false
        }
    });
    if applied != Some(true) {
        return;
    }
    crate::public_room::show_handle_status(handle, status);
}

/// Apply what an answer decided. `handle` is the mirror's.
fn apply(handle: ShortHandle, outcome: Outcome) {
    match outcome {
        Outcome::Ignore => {}
        Outcome::Resolved(vk) => {
            log("resolved and verified");
            let applied = with_mirror(|m| {
                if m.handle == handle {
                    m.done = true;
                    true
                } else {
                    false
                }
            });
            if applied == Some(true) {
                crate::public_room::open_resolved(handle, vk);
            }
        }
        Outcome::Status(status) => {
            let done = matches!(status, ViewedRoomStatus::Invalid(_));
            log(&format!("answer: {status:?}"));
            set_status(handle, status, done);
        }
    }
}

fn deliver(id: &ContractInstanceId, answer: Answer) -> bool {
    let m = mirror();
    let routed = route(m.as_ref(), id, answer);
    if let Some(m) = m.as_ref() {
        if routed.claimed {
            apply(m.handle, routed.outcome);
        }
    }
    // The share-link check may be waiting on the same contract.
    let checked = check_deliver(id, answer);
    // Anything else for a resolver this page asked about is a late answer: drop it.
    routed.claimed || checked || was_seen(id)
}

/// Route a GET response. `true` if it was a resolver's.
pub fn deliver_state(id: &ContractInstanceId, bytes: &[u8]) -> bool {
    deliver(id, Answer::State(bytes))
}

/// Route a `NotFound`. `true` if it was a resolver's.
pub fn deliver_not_found(id: &ContractInstanceId) -> bool {
    deliver(id, Answer::NotFound)
}

/// Route an error the node sent. A first failure sends the retry at once; a
/// failure after it reports a transport error, which says nothing about whether
/// the entry exists.
pub fn deliver_error(error_text: &str) {
    let m = mirror();
    let retried = RETRIED.load(Ordering::SeqCst);
    let Some(m) = m.as_ref() else {
        check_error(error_text);
        return;
    };
    match error_action(Some(m), error_text, retried) {
        ErrorAction::Ignore => check_error(error_text),
        ErrorAction::Retry => {
            if !RETRIED.swap(true, Ordering::SeqCst) {
                log("error (transient), retry sent");
                send_get(m.id);
            }
        }
        ErrorAction::GiveUp => {
            log("error (transport)");
            set_status(m.handle, ViewedRoomStatus::HandleTransportError, false);
        }
    }
}

/// Whether a GET can go out now: the socket is up. The no-sync example build
/// answers from fixtures and always can.
#[cfg(not(feature = "no-sync"))]
fn can_send() -> bool {
    crate::components::app::WEB_API
        .try_peek()
        .map(|w| w.is_some())
        .unwrap_or(false)
}

#[cfg(feature = "no-sync")]
fn can_send() -> bool {
    true
}

#[cfg(not(feature = "no-sync"))]
fn send_get(id: ContractInstanceId) {
    use crate::components::app::WEB_API;
    use freenet_stdlib::client_api::{ClientRequest, ContractRequest};
    crate::util::safe_spawn_local(async move {
        let request = ClientRequest::ContractOp(ContractRequest::Get {
            key: id,
            return_contract_code: false,
            subscribe: false,
            blocking_subscribe: false,
        });
        if let Some(web_api) = WEB_API.write().as_mut() {
            LOOKUPS_SENT.fetch_add(1, Ordering::SeqCst);
            if let Err(e) = web_api.send(request).await {
                dioxus::logger::tracing::warn!("short link: resolver GET failed to send: {e}");
            }
        }
    });
}

/// The no-sync example build has no node: the fixture answers after a pause long
/// enough for the loading state to be seen.
#[cfg(feature = "no-sync")]
fn send_get(id: ContractInstanceId) {
    LOOKUPS_SENT.fetch_add(1, Ordering::SeqCst);
    #[cfg(all(target_arch = "wasm32", feature = "example-data"))]
    crate::util::safe_spawn_local(async move {
        crate::util::sleep(Duration::from_millis(1200)).await;
        let answer = crate::example_data::resolver_fixture(&id);
        crate::util::defer(move || match answer {
            Some(state) => {
                deliver_state(&id, &state);
            }
            None => {
                deliver_not_found(&id);
            }
        });
    });
    #[cfg(not(all(target_arch = "wasm32", feature = "example-data")))]
    let _ = id;
}

/// Send the resolver GET for the link being viewed, retry once if nothing has
/// answered after [`RESOLVE_RETRY_AFTER`], and time out after
/// [`RESOLVE_TIMEOUT`] (the same rules as the room lookup).
pub fn request(handle: ShortHandle) {
    let Some(id) = with_mirror(|m| {
        m.requested = true;
        m.id
    }) else {
        return;
    };
    let generation = GEN.fetch_add(1, Ordering::SeqCst) + 1;
    RETRIED.store(false, Ordering::SeqCst);
    let current = move || GEN.load(Ordering::SeqCst) == generation;
    crate::util::safe_spawn_local(async move {
        send_get(id);
        log("GET sent");
        crate::util::sleep(RESOLVE_RETRY_AFTER).await;
        if still_waiting(mirror().as_ref(), &handle, current())
            && !RETRIED.swap(true, Ordering::SeqCst)
        {
            send_get(id);
            log("retry sent");
        }
        crate::util::sleep(RESOLVE_TIMEOUT - RESOLVE_RETRY_AFTER).await;
        if still_waiting(mirror().as_ref(), &handle, current()) {
            log("timeout");
            set_status(handle, ViewedRoomStatus::HandleTimedOut, false);
        }
    });
}

/// Try again after a not-found, an error or a timeout.
pub fn retry(handle: ShortHandle) {
    set_status(handle, ViewedRoomStatus::Resolving, false);
    request(handle);
}

// ---- the share dialog's "does the short link exist" check ------------------

/// Rooms whose short-link entry was found and verified, for "Copy short link".
pub static PUBLISHED: GlobalSignal<Vec<VerifyingKey>> = Global::new(Vec::new);

struct Pending {
    owner: VerifyingKey,
    handle: ShortHandle,
}

static PENDING: Mutex<Option<HashMap<ContractInstanceId, Pending>>> = Mutex::new(None);
/// When each room was last checked, ms since the epoch.
static CHECKED_AT: Mutex<Option<HashMap<[u8; 32], f64>>> = Mutex::new(None);

/// The handle of a room: what its short link carries.
pub fn handle_of(owner: &VerifyingKey) -> ShortHandle {
    ShortHandle::derive(owner.as_bytes())
}

/// The room's short link, `https://freenet.org/open#<River>/?room=<handle>`.
pub fn short_link(owner: &VerifyingKey) -> String {
    crate::public_room::link_for(&handle_of(owner).to_base58())
}

/// Whether the room's short link has been found and verified.
pub fn is_published(owner: &VerifyingKey) -> bool {
    PUBLISHED
        .try_read()
        .map(|p| p.contains(owner))
        .unwrap_or(false)
}

/// Look the room's resolver entry up once, with no retry, and record it as
/// published only if it exists and verifies. Anything else (not found, error,
/// timeout) leaves the short link unoffered. Safe to call on every render.
pub fn request_check(owner: VerifyingKey) {
    // Nothing is stamped or sent while the socket is not up: the dialog calls
    // this again on every render, including the one after the connect.
    if !can_send() {
        return;
    }
    let now = crate::public_room::now_ms_pub();
    {
        let mut at = CHECKED_AT.lock().unwrap_or_else(|e| e.into_inner());
        let map = at.get_or_insert_with(HashMap::new);
        let key = *owner.as_bytes();
        if let Some(&t) = map.get(&key) {
            if now - t < CHECK_RECHECK_AFTER_MS || is_published(&owner) {
                return;
            }
        }
        map.insert(key, now);
    }
    let handle = handle_of(&owner);
    let id = handle.resolver_instance_id();
    note_seen(id);
    PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(id, Pending { owner, handle });
    // The entry stays pending until answered, on purpose: a late answer is
    // still verified and still enables the link, and it must not fall through
    // to the room handlers as an unknown contract. Pending ids are bounded by
    // the number of rooms checked.
    send_get(id);
}

fn take_pending(id: &ContractInstanceId) -> Option<Pending> {
    PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
        .and_then(|m| m.remove(id))
}

/// Whether a check answer proves the entry: pure, so it is unit-tested.
pub fn check_proves_entry(handle: &ShortHandle, owner: &VerifyingKey, state: &[u8]) -> bool {
    resolve_state(handle, state).is_ok_and(|k| &k == owner.as_bytes())
}

fn check_deliver(id: &ContractInstanceId, answer: Answer) -> bool {
    let Some(p) = take_pending(id) else {
        return false;
    };
    if let Answer::State(bytes) = answer {
        if check_proves_entry(&p.handle, &p.owner, bytes) {
            let owner = p.owner;
            crate::util::defer(move || {
                let known = PUBLISHED.try_read().is_ok_and(|v| v.contains(&owner));
                if !known {
                    PUBLISHED.write().push(owner);
                }
            });
        }
    }
    true
}

fn check_error(error_text: &str) {
    let hit = {
        let map = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        map.as_ref().and_then(|m| {
            m.keys()
                .find(|id| error_text.contains(&id.to_string()))
                .copied()
        })
    };
    if let Some(id) = hit {
        take_pending(&id);
    }
}

/// Test hook support: mark the room's entry as published in the fixture
/// resolver, so the next check finds it.
#[cfg(all(target_arch = "wasm32", feature = "example-data", feature = "no-sync"))]
pub fn forget_checks() {
    *CHECKED_AT.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::short_handle::{minimal_state, OFFICIAL_ROOM_OWNER_KEY};
    use ed25519_dalek::SigningKey;

    fn vk_of(seed: u8) -> VerifyingKey {
        SigningKey::from_bytes(&[seed; 32]).verifying_key()
    }

    fn asked(handle: ShortHandle) -> ResolveMirror {
        let mut m = ResolveMirror::new(handle);
        m.requested = true;
        m
    }

    #[test]
    fn a_matching_state_resolves_to_its_key() {
        let vk = vk_of(4);
        let h = handle_of(&vk);
        let m = asked(h);
        let r = route(
            Some(&m),
            &m.id,
            Answer::State(&minimal_state(vk.as_bytes())),
        );
        assert!(r.claimed);
        assert_eq!(r.outcome, Outcome::Resolved(vk));
    }

    #[test]
    fn a_state_for_a_different_key_is_refused_as_a_mismatch() {
        let h = handle_of(&vk_of(4));
        let m = asked(h);
        let r = route(
            Some(&m),
            &m.id,
            Answer::State(&minimal_state(vk_of(5).as_bytes())),
        );
        assert!(r.claimed);
        assert_eq!(
            r.outcome,
            Outcome::Status(ViewedRoomStatus::Invalid(RoomLinkError::HandleMismatch))
        );
    }

    #[test]
    fn a_state_that_does_not_parse_is_refused_as_malformed() {
        let m = asked(handle_of(&vk_of(4)));
        for bytes in [&b""[..], b"\x02junk", &[1u8; 10]] {
            let r = route(Some(&m), &m.id, Answer::State(bytes));
            assert_eq!(
                r.outcome,
                Outcome::Status(ViewedRoomStatus::Invalid(RoomLinkError::HandleMalformed))
            );
        }
    }

    #[test]
    fn the_official_handle_refuses_a_substituted_key() {
        // The Official handle with another key's state is a hash mismatch; the
        // separate Official comparison is pinned in `short_handle`.
        let official: [u8; 32] = bs58::decode(OFFICIAL_ROOM_OWNER_KEY)
            .into_vec()
            .unwrap()
            .try_into()
            .unwrap();
        let m = asked(ShortHandle::derive(&official));
        let r = route(
            Some(&m),
            &m.id,
            Answer::State(&minimal_state(vk_of(1).as_bytes())),
        );
        assert!(matches!(
            r.outcome,
            Outcome::Status(ViewedRoomStatus::Invalid(_))
        ));
        let ok = route(Some(&m), &m.id, Answer::State(&minimal_state(&official)));
        assert!(matches!(ok.outcome, Outcome::Resolved(_)));
    }

    #[test]
    fn not_found_says_the_link_is_not_registered() {
        let m = asked(handle_of(&vk_of(4)));
        let r = route(Some(&m), &m.id, Answer::NotFound);
        assert_eq!(r.outcome, Outcome::Status(ViewedRoomStatus::HandleNotFound));
        let mut again = m.clone();
        again.status = ViewedRoomStatus::HandleNotFound;
        assert_eq!(
            route(Some(&again), &again.id, Answer::NotFound).outcome,
            Outcome::Ignore
        );
    }

    #[test]
    fn an_answer_is_claimed_only_when_asked_and_for_the_resolver() {
        let h = handle_of(&vk_of(4));
        let unasked = ResolveMirror::new(h);
        assert!(!route(Some(&unasked), &unasked.id, Answer::NotFound).claimed);
        assert!(!route(None, &unasked.id, Answer::NotFound).claimed);
        let m = asked(h);
        let other = handle_of(&vk_of(9)).resolver_instance_id();
        assert!(!route(Some(&m), &other, Answer::NotFound).claimed);
    }

    #[test]
    fn once_done_a_late_answer_is_swallowed_and_changes_nothing() {
        let vk = vk_of(4);
        let mut m = asked(handle_of(&vk));
        m.done = true;
        let r = route(
            Some(&m),
            &m.id,
            Answer::State(&minimal_state(vk.as_bytes())),
        );
        assert!(r.claimed);
        assert_eq!(r.outcome, Outcome::Ignore);
    }

    #[test]
    fn errors_retry_once_then_report_a_transport_error() {
        let m = asked(handle_of(&vk_of(4)));
        let text = format!("get failed for {}", m.id);
        assert_eq!(error_action(Some(&m), &text, false), ErrorAction::Retry);
        assert_eq!(error_action(Some(&m), &text, true), ErrorAction::GiveUp);
        assert_eq!(
            error_action(Some(&m), "unrelated", false),
            ErrorAction::Ignore
        );
        let unasked = ResolveMirror::new(m.handle);
        assert_eq!(
            error_action(Some(&unasked), &text, false),
            ErrorAction::Ignore
        );
        assert_eq!(error_action(None, &text, false), ErrorAction::Ignore);
    }

    #[test]
    fn the_request_goes_out_once_per_handle_and_only_when_connected() {
        let h = handle_of(&vk_of(4));
        let s = ViewedRoomStatus::Resolving;
        assert!(should_request(true, None, h, &s));
        assert!(!should_request(false, None, h, &s));
        assert!(!should_request(true, Some(h), h, &s));
        assert!(should_request(true, Some(handle_of(&vk_of(5))), h, &s));
        assert!(!should_request(
            true,
            None,
            h,
            &ViewedRoomStatus::HandleNotFound
        ));
    }

    #[test]
    fn a_check_proves_the_entry_only_for_the_rooms_own_key() {
        let owner = vk_of(4);
        let h = handle_of(&owner);
        assert!(check_proves_entry(
            &h,
            &owner,
            &minimal_state(owner.as_bytes())
        ));
        assert!(!check_proves_entry(
            &h,
            &owner,
            &minimal_state(vk_of(5).as_bytes())
        ));
        assert!(!check_proves_entry(&h, &owner, b"junk"));
    }

    /// The three call sites that feed real network answers in. The fixture
    /// answers call `deliver_*` directly, so nothing else would notice one
    /// being deleted or moved behind the room handlers (which would treat a
    /// resolver's answer as an unknown contract).
    #[test]
    fn the_response_handlers_offer_answers_to_the_resolver_first() {
        use crate::util::strip_comments;
        let get = strip_comments(include_str!(
            "components/app/freenet_api/response_handler/get_response.rs"
        ));
        for (ours, theirs) in [
            (
                "short_resolve::deliver_state(",
                "public_room::deliver_state(",
            ),
            (
                "short_resolve::deliver_not_found(",
                "public_room::deliver_not_found(",
            ),
        ] {
            let a = get
                .find(ours)
                .unwrap_or_else(|| panic!("{ours} is not called"));
            let b = get
                .find(theirs)
                .unwrap_or_else(|| panic!("{theirs} is not called"));
            assert!(a < b, "{ours} must come before {theirs}");
        }
        let sync = strip_comments(include_str!(
            "components/app/freenet_api/freenet_synchronizer.rs"
        ));
        assert!(
            sync.contains("short_resolve::deliver_error("),
            "node errors must reach the resolver"
        );
    }

    /// An entry proved by a different key must not enable the short link: the
    /// check's delivery has to go through `check_proves_entry`.
    #[test]
    fn the_check_delivery_verifies_before_it_publishes() {
        let src = crate::util::strip_comments(include_str!("short_resolve.rs"));
        let body = &src[src.find("fn check_deliver").unwrap()..];
        let verify = body.find("check_proves_entry(").unwrap();
        let publish = body.find("PUBLISHED.write()").unwrap();
        assert!(verify < publish);
    }

    #[test]
    fn a_late_answer_for_a_resolver_this_page_asked_about_is_swallowed() {
        let h = handle_of(&vk_of(21));
        begin(h);
        end();
        // The viewer is gone, but the answer is still the resolver's.
        assert!(deliver_not_found(&h.resolver_instance_id()));
        // A contract this page never asked about is not.
        assert!(!deliver_not_found(
            &handle_of(&vk_of(22)).resolver_instance_id()
        ));
    }

    #[test]
    fn the_short_link_carries_the_handle_not_the_key() {
        let owner = vk_of(4);
        let link = short_link(&owner);
        assert!(link.ends_with(&format!("/?room={}", handle_of(&owner).to_base58())));
        assert!(!link.contains(&crate::public_room::owner_key_b58(&owner)));
        let target = crate::util::share_link::parse_share_link(&link).expect("a share link");
        assert!(target.local_path().contains("?room="));
    }
}
