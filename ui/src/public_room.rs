//! Opening a room by its public address, `?room=<owner VerifyingKey, base58>`
//! (freenet/river#513, step 1 of #975).
//!
//! The owner key alone determines the room contract
//! ([`crate::util::owner_vk_to_contract_key`]), so a visitor needs no
//! invitation, membership or signing key to read a public room.
//!
//! # Where a viewed room lives
//!
//! In [`VIEWED_ROOM`], a session-only signal that is deliberately NOT part of
//! `ROOMS`. `ROOMS` is persisted to the chat delegate, drives the synchronizer
//! (`NEEDS_SYNC`, `ProcessRooms`, re-PUT of "local" state), the room list, unread
//! counts and notifications, and assumes every entry has a member identity. A
//! viewer has none of that, and an entry there would be written to the delegate
//! as if the user had joined. Keeping it separate means opening a link cannot
//! create or burn an invitation, write to the delegate, or need a signing key,
//! and a reload simply opens the link again. A room the user is already a
//! member of is never viewed this way: the member's own room is selected
//! instead.
//!
//! The viewer's state arrives through the ordinary `GetResponse` /
//! `UpdateNotification` / `NotFound` paths, which route here first
//! ([`deliver_state`], [`deliver_update`], [`deliver_not_found`]), the same
//! way `generation_watch` claims its pointer records.

use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::prelude::{ContractInstanceId, UpdateData};
use river_core::room_state::privacy::PrivacyMode;
use river_core::room_state::ChatRoomStateV1;
use std::sync::Mutex;

use crate::discover_view::DISCOVER_OPEN;

/// The URL query parameter that carries the address.
pub const ROOM_PARAM: &str = "room";

/// An Ed25519 verifying key.
const OWNER_KEY_BYTES: usize = 32;
/// The short handle proposed in #972 (22 base58 characters). Recognised now so
/// a short link says so instead of reading as a corrupt key; resolving one is
/// a later step.
const SHORT_HANDLE_BYTES: usize = 16;

/// Why a `?room=` value was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomLinkError {
    Empty,
    NotBase58,
    /// Decodes to 16 bytes: the future short handle.
    ShortHandle,
    BadLength(usize),
    /// 32 bytes, but not a valid Ed25519 public key.
    NotAKey,
}

impl RoomLinkError {
    pub fn message(&self) -> String {
        match self {
            Self::Empty => "This room link has no room key in it.".to_string(),
            Self::NotBase58 => {
                "This room link is not valid: the room key contains characters a key never has."
                    .to_string()
            }
            Self::ShortHandle => "Short room links are not supported yet.".to_string(),
            Self::BadLength(n) => format!(
                "This room link is not valid: a room key decodes to {OWNER_KEY_BYTES} bytes, this one to {n}."
            ),
            Self::NotAKey => "This room link is not valid: it is not a room key.".to_string(),
        }
    }
}

/// Parse a `?room=` value: the owner's `VerifyingKey` in base58, which is the
/// canonical room id (see the interface decision on freenet/river#972).
///
/// The handle forms are told apart by decoded length, so a 16-byte value is
/// reported as a short handle rather than a bad key.
pub fn parse_room_param(raw: &str) -> Result<VerifyingKey, RoomLinkError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(RoomLinkError::Empty);
    }
    let decoded = bs58::decode(raw)
        .with_alphabet(bs58::Alphabet::BITCOIN)
        .into_vec()
        .map_err(|_| RoomLinkError::NotBase58)?;
    match decoded.len() {
        OWNER_KEY_BYTES => {}
        SHORT_HANDLE_BYTES => return Err(RoomLinkError::ShortHandle),
        n => return Err(RoomLinkError::BadLength(n)),
    }
    let bytes: [u8; OWNER_KEY_BYTES] = decoded
        .try_into()
        .map_err(|_| RoomLinkError::BadLength(0))?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| RoomLinkError::NotAKey)
}

/// The base58 form of a room's owner key: the address in the link.
pub fn owner_key_b58(owner_vk: &VerifyingKey) -> String {
    bs58::encode(owner_vk.as_bytes())
        .with_alphabet(bs58::Alphabet::BITCOIN)
        .into_string()
}

/// The River web container's contract id: the app a public link opens.
const RIVER_WEB_APP: &str = include_str!("../../published-contract/contract-id.txt");

/// `https://freenet.org/open#<River web container id>/?room=<owner key>`: the
/// share-link form, which opens River on the reader's own node whatever host
/// the sharer was on (so it also works from a hosted instance such as
/// try.freenet.org). `ui/src/util/share_link.rs` accepts it as it is.
pub fn public_room_link(owner_vk: &VerifyingKey) -> String {
    format!(
        "https://freenet.org/open#{}/?{ROOM_PARAM}={}",
        RIVER_WEB_APP.trim(),
        owner_key_b58(owner_vk)
    )
}

/// What the viewer has learned about the room so far.
#[derive(Debug, Clone, PartialEq)]
pub enum ViewedRoomStatus {
    /// Asked the network (or not yet connected).
    Loading,
    /// No peer has the room, or its state could not be read.
    NotFound,
    /// Nothing answered in time.
    TimedOut,
    /// The room is private. Nothing of its content is kept.
    InviteOnly,
    /// The `?room=` value was refused.
    Invalid(RoomLinkError),
    /// A public room's current state. Boxed: it dwarfs the other variants.
    Public(Box<ChatRoomStateV1>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ViewedRoom {
    /// `None` only for an invalid link.
    pub owner_vk: Option<VerifyingKey>,
    pub status: ViewedRoomStatus,
}

/// Initialised from the page address, so a `?room=` link is the very first
/// thing rendered: the conversation pane never mounts and unmounts under it.
pub static VIEWED_ROOM: GlobalSignal<Option<ViewedRoom>> = Global::new(initial_viewed_room);

/// How long a fetch may go unanswered before the viewer offers a retry.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// How long a held answer may wait for the user's rooms to load before it is
/// shown as a visitor's view anyway, counted from the GET being sent. The same
/// length as the rooms-load migration backstop.
const HELD_ANSWER_BACKSTOP: std::time::Duration = std::time::Duration::from_secs(60);

/// How often a held answer is re-checked after the lookup timeout.
const HELD_RECHECK: std::time::Duration = std::time::Duration::from_secs(1);

/// What the lookup timeout does once `FETCH_TIMEOUT` has passed.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(feature = "no-sync", allow(dead_code))]
enum TimeoutAction {
    /// Nothing: the lookup was answered, replaced or closed.
    Nothing,
    /// Offer a retry: nothing has answered.
    TimedOut,
    /// A good answer is held and the rooms are still loading: keep showing
    /// "Loading your rooms" and look again shortly.
    KeepWaiting,
    /// Show the held answer now: the rooms loaded, or the backstop passed.
    Release,
}

/// What to do when the lookup timeout fires or a held answer is re-checked.
/// `waited` is the time since the GET was sent. Pure, so it is unit-tested.
#[cfg_attr(feature = "no-sync", allow(dead_code))]
fn timeout_action(
    still_waiting: bool,
    held: bool,
    rooms_ready: bool,
    waited: std::time::Duration,
) -> TimeoutAction {
    if !still_waiting {
        return TimeoutAction::Nothing;
    }
    if !held {
        return TimeoutAction::TimedOut;
    }
    if rooms_ready || waited >= HELD_ANSWER_BACKSTOP {
        TimeoutAction::Release
    } else {
        TimeoutAction::KeepWaiting
    }
}

/// What the response handlers need to know about the viewed room, kept out of
/// any signal so they can read it from any context. The status is a copy of
/// the signal's, written first.
#[derive(Debug, Clone, PartialEq)]
pub struct Mirror {
    pub owner_vk: VerifyingKey,
    pub id: ContractInstanceId,
    /// Set when this viewer has sent its own request. Only answers to that
    /// request are claimed, so a response meant for a room the user is in can
    /// never be taken before the viewer has asked for anything.
    pub requested: bool,
    pub status: ViewedRoomStatus,
}

static MIRROR: Mutex<Option<Mirror>> = Mutex::new(None);

fn mirror() -> Option<Mirror> {
    MIRROR.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn with_mirror<R>(f: impl FnOnce(&mut Mirror) -> R) -> Option<R> {
    MIRROR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
        .map(f)
}

fn set_mirror(m: Option<Mirror>) {
    *MIRROR.lock().unwrap_or_else(|e| e.into_inner()) = m;
}

/// Whether `id` is the viewer's contract and not one of the user's own rooms.
pub fn is_viewers(id: &ContractInstanceId) -> bool {
    mirror().is_some_and(|m| &m.id == id && !is_ours(&m.owner_vk, id))
}

/// Decide what a room state means for a visitor. Pure, so it is unit-tested.
///
/// A private room's state is not kept at all: no ciphertext and no member list
/// reach the UI model, and nothing here attempts decryption. (The node that
/// answered the GET has fetched and cached it, as it does for any GET; the
/// viewer does not subscribe to a room until it is known to be public.)
pub fn classify_state(state: Option<ChatRoomStateV1>) -> ViewedRoomStatus {
    match state {
        None => ViewedRoomStatus::NotFound,
        Some(s) if s.configuration.configuration.privacy_mode != PrivacyMode::Public => {
            ViewedRoomStatus::InviteOnly
        }
        Some(mut s) => {
            // `actions_state` is not serialized: without this, edits and
            // deletions would not show.
            s.recent_messages.rebuild_actions_state();
            ViewedRoomStatus::Public(Box::new(s))
        }
    }
}

/// What the composer's note says for a viewer. The room's own policy decides
/// whether joining is offered (`public_join::offers_join`).
pub fn composer_note(state: &ChatRoomStateV1) -> &'static str {
    if crate::public_join::offers_join(state) {
        "Your first message joins you."
    } else {
        "Only invited people can write here. Ask a member for an invitation."
    }
}

/// Whether a room opened from a link is shown, and its name once the state has
/// arrived. `None`: no viewer. `Some(None)`: the viewer is open but has no room
/// to name yet. The tab title and the unread and read-marker logic use this so
/// the room selected before the viewer opened is not treated as the open room.
pub fn viewed_now() -> Option<Option<String>> {
    let viewed = match VIEWED_ROOM.try_read() {
        Ok(v) => v,
        Err(_) => {
            crate::util::signal_guard::schedule_nudge();
            return None;
        }
    };
    let viewed = viewed.as_ref()?;
    Some(match &viewed.status {
        ViewedRoomStatus::Public(state) => Some(
            state
                .configuration
                .configuration
                .display
                .name
                .to_string_lossy(),
        ),
        _ => None,
    })
}

/// A request the pane sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchAction {
    /// GET the room (not subscribed: privacy is not known yet).
    Get,
    /// Subscribe to a room already known to be public, to receive edits. Sent
    /// again after a reconnect, which drops subscriptions.
    Subscribe,
}

/// What to send now: as soon as the socket is connected, without waiting for
/// the user's own rooms to load, and never for a room already known to be
/// theirs. The ANSWER is not routed until the rooms have loaded (`deliver`
/// holds it, `release_held` routes it), so a room that turns out to be the
/// user's own is recognised by `route`'s `ours` and opens as theirs through
/// the pane's redirect effect, never as a visitor's view.
pub fn fetch_action(
    connected: bool,
    just_connected: bool,
    already_asked: bool,
    ours: bool,
    status: &ViewedRoomStatus,
) -> Option<FetchAction> {
    if !connected || ours {
        return None;
    }
    match status {
        ViewedRoomStatus::Loading if just_connected || !already_asked => Some(FetchAction::Get),
        ViewedRoomStatus::Public(_) if just_connected => Some(FetchAction::Subscribe),
        _ => None,
    }
}

/// The line shown while the pane is `Loading` and nothing can be shown yet:
/// the socket is not connected, or an answer has arrived and is held until the
/// user's own rooms have loaded. `None` otherwise, so the pane's usual text
/// applies. Pure, so it is unit-tested.
pub fn waiting_hint(connected: bool, answer_held: bool) -> Option<&'static str> {
    if !connected {
        Some("Connecting to Freenet\u{2026}")
    } else if answer_held {
        Some("Loading your rooms\u{2026}")
    } else {
        None
    }
}

/// An answer for the viewed contract that arrived before the user's rooms had
/// loaded, kept so `route`'s `ours` check can run once they have.
enum Held {
    State(Vec<u8>),
    NotFound,
}

static HELD: Mutex<Option<(ContractInstanceId, Held)>> = Mutex::new(None);

/// Whether an answer is being held, for the pane's hint.
pub static ANSWER_HELD: GlobalSignal<bool> = Global::new(|| false);

/// Whether the user's rooms have loaded. A contended signal counts as loaded:
/// `is_ours` then treats the room as theirs, which is the safe error.
fn rooms_ready() -> bool {
    crate::components::viewed_room::rooms_loaded()
}

/// Whether an answer must wait for the rooms: it is the viewer's (asked for,
/// same contract), carries the room's state or absence, and the viewer does not
/// show the room yet.
fn should_hold(
    m: Option<&Mirror>,
    id: &ContractInstanceId,
    rooms_ready: bool,
    ours: bool,
    answer: &Answer,
) -> bool {
    !rooms_ready
        && !ours
        && !matches!(answer, Answer::Delta)
        && m.is_some_and(|m| {
            &m.id == id && m.requested && !matches!(m.status, ViewedRoomStatus::Public(_))
        })
}

/// What to keep when an answer arrives while one is already held: the newest,
/// except that a NotFound never replaces a held State (a retry's NotFound must
/// not undo an answer that already arrived).
fn merge_held(existing: Option<Held>, new: Held) -> Held {
    match (existing, new) {
        (Some(state @ Held::State(_)), Held::NotFound) => state,
        (_, new) => new,
    }
}

/// Whether an answer is currently held (so the lookup counts as answered).
fn answer_is_held() -> bool {
    HELD.lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

fn set_answer_held(held: bool) {
    crate::util::defer(move || {
        if ANSWER_HELD.try_read().map(|v| *v) != Ok(held) {
            *ANSWER_HELD.write() = held;
        }
    });
}

/// Route the answer held while the user's rooms loaded, now that they have. A
/// room that is the user's own is then not claimed, and the pane's redirect
/// effect opens it as theirs. Called from the pane's effect.
pub fn release_held() {
    if !rooms_ready() {
        return;
    }
    release_held_now(false);
}

/// Route the held answer. `as_visitor` shows the room as a visitor's even when
/// the user's rooms have not loaded, for the backstop in [`request_fetch`].
fn release_held_now(as_visitor: bool) {
    let taken = HELD.lock().unwrap_or_else(|e| e.into_inner()).take();
    let Some((id, held)) = taken else { return };
    set_answer_held(false);
    let answer = match &held {
        Held::State(bytes) => Answer::State(bytes),
        Held::NotFound => Answer::NotFound,
    };
    if as_visitor {
        let m = mirror();
        deliver_with(m.as_ref(), false, &id, answer, &mut Live);
    } else {
        deliver_now(&id, answer);
    }
}

/// A network answer about the viewed contract.
pub enum Answer<'a> {
    State(&'a [u8]),
    NotFound,
    /// An update notification carrying a delta.
    Delta,
}

#[derive(Debug, PartialEq)]
pub enum FollowUp {
    Subscribe,
    Refetch,
}

#[derive(Debug, PartialEq)]
pub struct Routed {
    /// The caller must stop: the answer was the viewer's.
    pub claimed: bool,
    pub status: Option<ViewedRoomStatus>,
    pub follow_up: Option<FollowUp>,
}

const NOT_CLAIMED: Routed = Routed {
    claimed: false,
    status: None,
    follow_up: None,
};

/// Route one answer. Pure, so the claim rules are unit-tested.
///
/// An answer is the viewer's only if the viewer asked, it is for the viewed
/// contract, and that contract is not one of the user's own rooms (`ours`: in
/// `ROOMS`, `SYNC_INFO` or `PENDING_INVITES`). A good view is never replaced by
/// a later NotFound or an unreadable state, and a delta only matters for a
/// public room.
pub fn route(
    mirror: Option<&Mirror>,
    id: &ContractInstanceId,
    ours: bool,
    answer: Answer,
) -> Routed {
    let Some(m) = mirror else {
        return NOT_CLAIMED;
    };
    if &m.id != id || !m.requested || ours {
        return NOT_CLAIMED;
    }
    let was_public = matches!(m.status, ViewedRoomStatus::Public(_));
    let (status, follow_up) = match answer {
        Answer::State(bytes) => {
            let decoded = crate::util::try_from_cbor_slice::<ChatRoomStateV1>(bytes);
            if decoded.is_none() && was_public {
                (None, None)
            } else {
                let next = classify_state(decoded);
                let subscribe = !was_public && matches!(next, ViewedRoomStatus::Public(_));
                (Some(next), subscribe.then_some(FollowUp::Subscribe))
            }
        }
        Answer::NotFound if was_public => (None, None),
        Answer::NotFound => (Some(ViewedRoomStatus::NotFound), None),
        Answer::Delta => (None, was_public.then_some(FollowUp::Refetch)),
    };
    Routed {
        claimed: true,
        status,
        follow_up,
    }
}

/// Whether the contract is one of the user's own rooms, or about to be. A
/// contended signal counts as "ours": not claiming is the safe error.
fn is_ours(owner_vk: &VerifyingKey, id: &ContractInstanceId) -> bool {
    use crate::components::app::sync_info::SYNC_INFO;
    use crate::components::app::{PENDING_INVITES, ROOMS};
    let Ok(rooms) = ROOMS.try_read() else {
        return true;
    };
    if rooms.map.contains_key(owner_vk) {
        return true;
    }
    drop(rooms);
    let Ok(pending) = PENDING_INVITES.try_read() else {
        return true;
    };
    if pending.map.contains_key(owner_vk) {
        return true;
    }
    drop(pending);
    let Ok(sync) = SYNC_INFO.try_read() else {
        return true;
    };
    sync.get_owner_vk_for_instance_id(id).is_some()
}

/// Record a new status: the mirror at once, the signal in a deferred write.
fn set_status(owner_vk: VerifyingKey, status: ViewedRoomStatus) {
    let applied = with_mirror(|m| {
        if m.owner_vk == owner_vk {
            m.status = status.clone();
            true
        } else {
            false
        }
    });
    if applied != Some(true) {
        return;
    }
    crate::util::defer(move || {
        let still_viewed = VIEWED_ROOM
            .try_read()
            .ok()
            .is_some_and(|v| v.as_ref().and_then(|r| r.owner_vk) == Some(owner_vk));
        if still_viewed {
            *VIEWED_ROOM.write() = Some(ViewedRoom {
                owner_vk: Some(owner_vk),
                status,
            });
        }
    });
}

/// Test hook support: change the viewed room's state in place. `false` when no
/// public room is being viewed.
#[cfg(all(target_arch = "wasm32", feature = "example-data", feature = "no-sync"))]
pub fn edit_viewed_state(f: impl FnOnce(&mut ChatRoomStateV1)) -> bool {
    let mut guard = VIEWED_ROOM.write();
    match guard.as_mut().map(|v| &mut v.status) {
        Some(ViewedRoomStatus::Public(state)) => {
            f(state);
            true
        }
        _ => false,
    }
}

/// Test hook support: view another room in place, as a link followed inside
/// the page would, without closing the viewer first.
#[cfg(all(target_arch = "wasm32", feature = "example-data", feature = "no-sync"))]
pub fn view_for_test(raw: &str) {
    let viewed = viewed_from_param(raw);
    *VIEWED_ROOM.write() = Some(viewed);
}

/// Open a room read-only from Discover, as following its `?room=` link would
/// (freenet/river#975). A room the user already belongs to is switched to by
/// the viewer pane's own effect. Call from a deferred context.
pub fn open_from_discover(owner_key_b58: &str) {
    crate::public_join::reset();
    set_answer_held(false);
    let viewed = viewed_from_param(owner_key_b58);
    *VIEWED_ROOM.write() = Some(viewed);
}

/// What a `?room=` value opens: the room (waiting for the network) or the
/// reason it was refused. Records what the response handlers match on.
fn viewed_from_param(raw: &str) -> ViewedRoom {
    match parse_room_param(raw) {
        Ok(owner_vk) => {
            // The no-sync example build has no network: answer from fixtures.
            #[cfg(all(feature = "example-data", feature = "no-sync"))]
            let status = classify_state(crate::example_data::viewable_room_state(&owner_vk));
            #[cfg(not(all(feature = "example-data", feature = "no-sync")))]
            let status = ViewedRoomStatus::Loading;
            lookup_begin(&owner_vk);
            *HELD.lock().unwrap_or_else(|e| e.into_inner()) = None;
            set_mirror(Some(Mirror {
                owner_vk,
                id: *crate::util::owner_vk_to_contract_key(&owner_vk).id(),
                requested: false,
                status: status.clone(),
            }));
            ViewedRoom {
                owner_vk: Some(owner_vk),
                status,
            }
        }
        Err(e) => ViewedRoom {
            owner_vk: None,
            status: ViewedRoomStatus::Invalid(e),
        },
    }
}

/// Which `?room=` value to open: none when the URL also carries an
/// invitation, whose flow is unchanged.
pub fn room_param_to_open<'a>(invitation: Option<&str>, room: Option<&'a str>) -> Option<&'a str> {
    if invitation.is_some() {
        None
    } else {
        room
    }
}

/// The room named by the page's `?room=` parameter, if any.
fn initial_viewed_room() -> Option<ViewedRoom> {
    #[cfg(target_arch = "wasm32")]
    {
        let search = web_sys::window()?.location().search().ok()?;
        let params = web_sys::UrlSearchParams::new_with_str(&search).ok()?;
        let invitation = params.get("invitation");
        let room = params.get(ROOM_PARAM);
        room_param_to_open(invitation.as_deref(), room.as_deref()).map(viewed_from_param)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        None
    }
}

/// Stop viewing, and drop `?room=` from the address bar where the page may
/// rewrite it (not in the sandboxed gateway iframe, which has an opaque
/// origin). Called wherever the selected room changes, so a read-only pane
/// never sits over a different room. A node-side subscription made for a
/// public room cannot be cancelled (the client API has no unsubscribe yet) and
/// lasts until the socket closes.
pub fn close() {
    close_inner(None);
}

/// Whether a deferred redirect captured for `captured` still applies: the
/// viewer must still show that very room (freenet/river#1047).
pub fn should_redirect(viewed: Option<VerifyingKey>, captured: VerifyingKey) -> bool {
    viewed == Some(captured)
}

/// The viewed room's owner, or `None` if nothing is viewed or the signal is
/// busy (`Err`).
pub fn viewed_owner() -> Result<Option<VerifyingKey>, ()> {
    VIEWED_ROOM
        .try_peek()
        .map(|v| v.as_ref().and_then(|r| r.owner_vk))
        .map_err(|_| ())
}

/// As [`close`], but only if the viewer still shows `owner` when the deferred
/// body runs: a timer scheduled for one room must not shut another the user
/// opened meanwhile.
pub fn close_if_viewing(owner: VerifyingKey) {
    close_inner(Some(owner));
}

fn close_inner(only_if_viewing: Option<VerifyingKey>) {
    crate::util::defer(move || {
        if let Some(owner) = only_if_viewing {
            if !should_redirect(viewed_owner().ok().flatten(), owner) {
                return;
            }
        }
        crate::public_join::reset();
        set_mirror(None);
        *HELD.lock().unwrap_or_else(|e| e.into_inner()) = None;
        set_answer_held(false);
        let showing = VIEWED_ROOM.try_read().ok().is_some_and(|v| v.is_some());
        if showing {
            *VIEWED_ROOM.write() = None;
        }
        // Choosing a room from the rail, or closing the viewer, closes Discover too.
        if DISCOVER_OPEN.try_read().is_ok_and(|open| *open) {
            *DISCOVER_OPEN.write() = false;
        }
        #[cfg(target_arch = "wasm32")]
        strip_room_param_from_url();
    });
}

#[cfg(target_arch = "wasm32")]
fn strip_room_param_from_url() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Ok(search) = window.location().search() else {
        return;
    };
    let Ok(params) = web_sys::UrlSearchParams::new_with_str(&search) else {
        return;
    };
    if !params.has(ROOM_PARAM) {
        return;
    }
    params.delete(ROOM_PARAM);
    let rest = params.to_string().as_string().unwrap_or_default();
    let path = window.location().pathname().unwrap_or_default();
    let url = if rest.is_empty() {
        path
    } else {
        format!("{path}?{rest}")
    };
    if let Ok(history) = window.history() {
        let _ = history.replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&url));
    }
}

/// How long after the first GET a lookup with no answer sends one more.
const LOOKUP_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(7);

/// Bumped by every lookup, so a timer left over from an earlier one (before a
/// "Try again") stops instead of retrying or timing out the new one.
static LOOKUP_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// When the current lookup began, in ms since the epoch (0 before any).
static LOOKUP_START_MS: Mutex<f64> = Mutex::new(0.0);

fn now_ms() -> f64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        0.0
    }
}

/// Start timing a lookup and log its absolute start. Later `lookup_log` lines
/// carry the time since this call.
pub fn lookup_begin(owner_vk: &VerifyingKey) {
    *LOOKUP_START_MS.lock().unwrap_or_else(|e| e.into_inner()) = now_ms();
    #[cfg(target_arch = "wasm32")]
    let iso = String::from(js_sys::Date::new_0().to_iso_string());
    #[cfg(not(target_arch = "wasm32"))]
    let iso = String::new();
    dioxus::logger::tracing::info!(
        "public room: lookup start {iso} (room {})",
        room_prefix(owner_vk)
    );
}

fn room_prefix(owner_vk: &VerifyingKey) -> String {
    owner_key_b58(owner_vk).chars().take(8).collect()
}

/// One timing line for the lookup, relative to `lookup_begin`.
pub fn lookup_log(event: &str) {
    let start = *LOOKUP_START_MS.lock().unwrap_or_else(|e| e.into_inner());
    dioxus::logger::tracing::info!(
        "public room: lookup t+{:.2}s {event}",
        (now_ms() - start) / 1000.0
    );
}

/// Whether the lookup this timer belongs to is still the current one and still
/// waiting for an answer. Pure, so it is unit-tested.
#[cfg_attr(feature = "no-sync", allow(dead_code))]
fn lookup_still_waiting(m: Option<&Mirror>, owner_vk: &VerifyingKey, current: bool) -> bool {
    current
        && m.is_some_and(|m| {
            &m.owner_vk == owner_vk && m.requested && m.status == ViewedRoomStatus::Loading
        })
}

/// What to do with an error from the node.
#[derive(Debug, PartialEq, Eq)]
pub enum ErrorAction {
    /// Not about the viewer's lookup.
    Ignore,
    /// A first failure: send the one retry now instead of waiting for it.
    Retry,
    /// The retry has been sent already: offer Try again.
    GiveUp,
}

/// Classify an error: it counts when the viewer asked, is still waiting, the
/// contract is not the user's own, and the error names the contract. Pure, so
/// it is unit-tested.
pub fn error_action(
    m: Option<&Mirror>,
    error_text: &str,
    ours: bool,
    retried: bool,
    answer_held: bool,
) -> ErrorAction {
    let for_viewer = !ours
        && !answer_held
        && m.is_some_and(|m| {
            m.requested
                && m.status == ViewedRoomStatus::Loading
                && error_text.contains(&m.id.to_string())
        });
    match (for_viewer, retried) {
        (false, _) => ErrorAction::Ignore,
        (true, false) => ErrorAction::Retry,
        (true, true) => ErrorAction::GiveUp,
    }
}

/// Set once the lookup's one retry has been sent, by the timer or by an error.
static LOOKUP_RETRIED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Route an error the node sent. A first failure sends the retry at once; a
/// failure after it shows "No answer yet" with Try again now, not after the
/// timeout. A later successful answer is still accepted.
pub fn deliver_error(error_text: &str) {
    use std::sync::atomic::Ordering;
    let m = mirror();
    let ours = m.as_ref().is_some_and(|m| is_ours(&m.owner_vk, &m.id));
    let retried = LOOKUP_RETRIED.load(Ordering::SeqCst);
    let Some(m) = m.as_ref() else { return };
    match error_action(Some(m), error_text, ours, retried, answer_is_held()) {
        ErrorAction::Ignore => {}
        ErrorAction::Retry => {
            if !LOOKUP_RETRIED.swap(true, Ordering::SeqCst) {
                lookup_log("error (transient), retry sent");
                send_lookup_get(m.owner_vk);
            }
        }
        ErrorAction::GiveUp => {
            lookup_log("answer/error (error)");
            set_status(m.owner_vk, ViewedRoomStatus::TimedOut);
        }
    }
}

#[cfg(not(feature = "no-sync"))]
fn send_lookup_get(owner_vk: VerifyingKey) {
    use crate::components::app::WEB_API;
    use freenet_stdlib::client_api::{ClientRequest, ContractRequest};
    crate::util::safe_spawn_local(async move {
        let request = ClientRequest::ContractOp(ContractRequest::Get {
            key: *crate::util::owner_vk_to_contract_key(&owner_vk).id(),
            return_contract_code: false,
            subscribe: false,
            blocking_subscribe: false,
        });
        if let Some(web_api) = WEB_API.write().as_mut() {
            if let Err(e) = web_api.send(request).await {
                dioxus::logger::tracing::warn!("public room: fetch failed to send: {e}");
            }
        }
    });
}

#[cfg(feature = "no-sync")]
fn send_lookup_get(_owner_vk: VerifyingKey) {}

/// Ask the network for the room (not subscribed: it may be private), retry
/// once if nothing has answered after `LOOKUP_RETRY_AFTER`, and start the
/// timeout after which the viewer offers a retry. The timeout counts from the
/// GET being sent, as it always has.
#[cfg(not(feature = "no-sync"))]
pub fn request_fetch(owner_vk: VerifyingKey) {
    use std::sync::atomic::Ordering;
    with_mirror(|m| m.requested = true);
    let generation = LOOKUP_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    LOOKUP_RETRIED.store(false, Ordering::SeqCst);
    let current = move || LOOKUP_GEN.load(Ordering::SeqCst) == generation;
    crate::util::safe_spawn_local(async move {
        send_lookup_get(owner_vk);
        lookup_log("GET sent");
        crate::util::sleep(LOOKUP_RETRY_AFTER).await;
        if lookup_still_waiting(mirror().as_ref(), &owner_vk, current())
            && !answer_is_held()
            && !LOOKUP_RETRIED.swap(true, Ordering::SeqCst)
        {
            send_lookup_get(owner_vk);
            lookup_log("retry sent");
        }
        crate::util::sleep(FETCH_TIMEOUT - LOOKUP_RETRY_AFTER).await;
        let mut waited = FETCH_TIMEOUT;
        loop {
            let waiting = lookup_still_waiting(mirror().as_ref(), &owner_vk, current());
            match timeout_action(waiting, answer_is_held(), rooms_ready(), waited) {
                TimeoutAction::Nothing => break,
                TimeoutAction::TimedOut => {
                    lookup_log("timeout");
                    set_status(owner_vk, ViewedRoomStatus::TimedOut);
                    break;
                }
                TimeoutAction::Release => {
                    lookup_log("held answer released at the timeout");
                    release_held_now(true);
                    break;
                }
                TimeoutAction::KeepWaiting => {
                    crate::util::sleep(HELD_RECHECK).await;
                    waited += HELD_RECHECK;
                }
            }
        }
    });
}

#[cfg(feature = "no-sync")]
pub fn request_fetch(_owner_vk: VerifyingKey) {
    with_mirror(|m| m.requested = true);
}

/// Subscribe to a room known to be public, for its updates.
#[cfg(not(feature = "no-sync"))]
pub fn request_subscribe(owner_vk: VerifyingKey) {
    use crate::components::app::WEB_API;
    use freenet_stdlib::client_api::{ClientRequest, ContractRequest};
    crate::util::safe_spawn_local(async move {
        let request = ClientRequest::ContractOp(ContractRequest::Subscribe {
            key: *crate::util::owner_vk_to_contract_key(&owner_vk).id(),
            summary: None,
        });
        if let Some(web_api) = WEB_API.write().as_mut() {
            if let Err(e) = web_api.send(request).await {
                dioxus::logger::tracing::warn!("public room: subscribe failed to send: {e}");
            }
        }
    });
}

#[cfg(feature = "no-sync")]
pub fn request_subscribe(_owner_vk: VerifyingKey) {}

#[cfg(not(feature = "no-sync"))]
fn request_refetch(owner_vk: VerifyingKey) {
    use crate::components::app::WEB_API;
    use freenet_stdlib::client_api::{ClientRequest, ContractRequest};
    crate::util::safe_spawn_local(async move {
        let request = ClientRequest::ContractOp(ContractRequest::Get {
            key: *crate::util::owner_vk_to_contract_key(&owner_vk).id(),
            return_contract_code: false,
            subscribe: false,
            blocking_subscribe: false,
        });
        if let Some(web_api) = WEB_API.write().as_mut() {
            let _ = web_api.send(request).await;
        }
    });
}

#[cfg(feature = "no-sync")]
fn request_refetch(_owner_vk: VerifyingKey) {}

/// Try again after a NotFound or a timeout.
pub fn retry(owner_vk: VerifyingKey) {
    lookup_begin(&owner_vk);
    set_status(owner_vk, ViewedRoomStatus::Loading);
    request_fetch(owner_vk);
}

/// What `deliver` does with a routed answer. Separate from the routing so the
/// whole decision (claim, status write, follow-up request) is testable without
/// the Dioxus runtime or a node.
trait Effects {
    fn set_status(&mut self, owner_vk: VerifyingKey, status: ViewedRoomStatus);
    fn subscribe(&mut self, owner_vk: VerifyingKey);
    fn refetch(&mut self, owner_vk: VerifyingKey);
}

struct Live;

impl Effects for Live {
    fn set_status(&mut self, owner_vk: VerifyingKey, status: ViewedRoomStatus) {
        set_status(owner_vk, status)
    }
    fn subscribe(&mut self, owner_vk: VerifyingKey) {
        request_subscribe(owner_vk)
    }
    fn refetch(&mut self, owner_vk: VerifyingKey) {
        request_refetch(owner_vk)
    }
}

fn deliver_with(
    m: Option<&Mirror>,
    ours: bool,
    id: &ContractInstanceId,
    answer: Answer,
    fx: &mut impl Effects,
) -> bool {
    let kind = match &answer {
        Answer::State(_) => "state",
        Answer::NotFound => "not found",
        Answer::Delta => "delta",
    };
    let routed = route(m, id, ours, answer);
    if routed.claimed && !matches!(m.map(|m| &m.status), Some(ViewedRoomStatus::Public(_))) {
        lookup_log(&format!("answer/error ({kind})"));
    }
    if let (Some(status), Some(m)) = (routed.status, m) {
        fx.set_status(m.owner_vk, status);
    }
    if let (Some(follow_up), Some(m)) = (routed.follow_up, m) {
        match follow_up {
            FollowUp::Subscribe => fx.subscribe(m.owner_vk),
            FollowUp::Refetch => fx.refetch(m.owner_vk),
        }
    }
    routed.claimed
}

fn deliver(id: &ContractInstanceId, answer: Answer) -> bool {
    let m = mirror();
    let ours = m.as_ref().is_some_and(|m| is_ours(&m.owner_vk, id));
    if should_hold(m.as_ref(), id, rooms_ready(), ours, &answer) {
        let new = match &answer {
            Answer::State(bytes) => Held::State(bytes.to_vec()),
            _ => Held::NotFound,
        };
        let mut slot = HELD.lock().unwrap_or_else(|e| e.into_inner());
        let existing = slot
            .take()
            .filter(|(held_id, _)| held_id == id)
            .map(|(_, h)| h);
        *slot = Some((*id, merge_held(existing, new)));
        drop(slot);
        set_answer_held(true);
        lookup_log("answer held until your rooms load");
        return true;
    }
    deliver_now(id, answer)
}

fn deliver_now(id: &ContractInstanceId, answer: Answer) -> bool {
    let m = mirror();
    let ours = m.as_ref().is_some_and(|m| is_ours(&m.owner_vk, id));
    deliver_with(m.as_ref(), ours, id, answer, &mut Live)
}

/// Route a GET response. `true` if it was the viewer's, which the caller must
/// then not treat as one of the user's rooms.
pub fn deliver_state(id: &ContractInstanceId, bytes: &[u8]) -> bool {
    deliver(id, Answer::State(bytes))
}

/// Route a `NotFound`. `true` if it was the viewer's.
pub fn deliver_not_found(id: &ContractInstanceId) -> bool {
    deliver(id, Answer::NotFound)
}

/// Route an update notification. `true` if it was the viewer's.
///
/// A full state replaces what is shown. A delta is not applied here: the room
/// is fetched again, which is a local read for a subscribed contract and keeps
/// this path free of merge logic.
pub fn deliver_update(id: &ContractInstanceId, update: &UpdateData) -> bool {
    match update {
        UpdateData::State(bytes) => deliver(id, Answer::State(bytes.as_ref())),
        _ => deliver(id, Answer::Delta),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};

    fn key() -> VerifyingKey {
        SigningKey::from_bytes(&[7u8; 32]).verifying_key()
    }

    fn state_with(mode: PrivacyMode) -> ChatRoomStateV1 {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let mut config = Configuration::default();
        config.privacy_mode = mode;
        let mut state = ChatRoomStateV1::default();
        state.configuration = AuthorizedConfigurationV1::new(config, &sk);
        state
    }

    #[test]
    fn a_full_owner_key_round_trips() {
        let vk = key();
        assert_eq!(parse_room_param(&owner_key_b58(&vk)), Ok(vk));
        assert_eq!(
            parse_room_param(&format!("  {}\n", owner_key_b58(&vk))),
            Ok(vk)
        );
    }

    #[test]
    fn an_invalid_value_is_refused_with_its_own_reason() {
        assert_eq!(parse_room_param(""), Err(RoomLinkError::Empty));
        assert_eq!(parse_room_param("   "), Err(RoomLinkError::Empty));
        // 0, O, I and l are not in the alphabet.
        assert_eq!(parse_room_param("0OIl"), Err(RoomLinkError::NotBase58));
        assert_eq!(parse_room_param("abc"), Err(RoomLinkError::BadLength(3)));
        // The 33-byte base58 of a key plus a byte.
        let mut long = key().as_bytes().to_vec();
        long.push(1);
        let long = bs58::encode(long).into_string();
        assert_eq!(parse_room_param(&long), Err(RoomLinkError::BadLength(33)));
    }

    #[test]
    fn sixteen_bytes_is_a_short_handle_not_a_corrupt_key() {
        let handle = bs58::encode([5u8; SHORT_HANDLE_BYTES]).into_string();
        let err = parse_room_param(&handle).unwrap_err();
        assert_eq!(err, RoomLinkError::ShortHandle);
        assert!(err.message().contains("not supported yet"));
    }

    #[test]
    fn thirty_two_bytes_that_are_not_a_curve_point_are_refused() {
        // y = 2 has no x on the curve.
        let mut bytes = [0u8; 32];
        bytes[0] = 2;
        let encoded = bs58::encode(bytes).into_string();
        assert_eq!(parse_room_param(&encoded), Err(RoomLinkError::NotAKey));
    }

    #[test]
    fn the_public_link_is_a_share_link_that_opens_river_with_the_room() {
        use crate::util::share_link::parse_share_link;
        let vk = key();
        let link = public_room_link(&vk);
        let id = RIVER_WEB_APP.trim();
        assert_eq!(
            link,
            format!("https://freenet.org/open#{id}/?room={}", owner_key_b58(&vk))
        );
        let target = parse_share_link(&link).expect("a valid share link");
        let path = target.local_path();
        assert_eq!(
            path,
            format!("/v1/contract/web/{id}/?room={}", owner_key_b58(&vk))
        );
        let key_part = path.split("?room=").nth(1).unwrap();
        assert_eq!(parse_room_param(key_part), Ok(vk));
    }

    #[test]
    fn a_private_room_is_never_kept() {
        assert_eq!(
            classify_state(Some(state_with(PrivacyMode::Private))),
            ViewedRoomStatus::InviteOnly
        );
    }

    #[test]
    fn a_public_room_is_shown_and_no_state_is_not_found() {
        assert!(matches!(
            classify_state(Some(state_with(PrivacyMode::Public))),
            ViewedRoomStatus::Public(_)
        ));
        assert_eq!(classify_state(None), ViewedRoomStatus::NotFound);
    }

    #[test]
    fn the_composer_note_follows_the_rooms_admission_setting() {
        let closed = state_with(PrivacyMode::Public);
        assert!(composer_note(&closed).contains("invitation"));
        assert!(!composer_note(&closed).contains("read-only"));
        assert!(!composer_note(&closed).contains("coming"));
    }

    // ---- claim-first routing -------------------------------------------

    use crate::util::owner_vk_to_contract_key;
    use river_core::room_state::member::MemberId;
    use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};

    fn mirror_for(vk: &VerifyingKey, requested: bool, status: ViewedRoomStatus) -> Mirror {
        Mirror {
            owner_vk: *vk,
            id: *owner_vk_to_contract_key(vk).id(),
            requested,
            status,
        }
    }

    fn state_bytes(mode: PrivacyMode) -> Vec<u8> {
        crate::util::to_cbor_vec(&state_with(mode))
    }

    #[test]
    fn a_viewers_answer_is_claimed_and_classified() {
        let vk = key();
        let m = mirror_for(&vk, true, ViewedRoomStatus::Loading);
        let bytes = state_bytes(PrivacyMode::Public);
        let r = route(Some(&m), &m.id, false, Answer::State(&bytes));
        assert!(r.claimed);
        assert!(matches!(r.status, Some(ViewedRoomStatus::Public(_))));
        // A public room is subscribed only once it is known to be public.
        assert_eq!(r.follow_up, Some(FollowUp::Subscribe));
    }

    #[test]
    fn a_private_room_is_never_subscribed_or_refetched() {
        let vk = key();
        let m = mirror_for(&vk, true, ViewedRoomStatus::Loading);
        let bytes = state_bytes(PrivacyMode::Private);
        let r = route(Some(&m), &m.id, false, Answer::State(&bytes));
        assert!(r.claimed);
        assert_eq!(r.status, Some(ViewedRoomStatus::InviteOnly));
        assert_eq!(r.follow_up, None);
        let held = mirror_for(&vk, true, ViewedRoomStatus::InviteOnly);
        let r = route(Some(&held), &held.id, false, Answer::Delta);
        assert!(r.claimed);
        assert_eq!(
            r.follow_up, None,
            "a delta for a non-public room is not fetched"
        );
    }

    #[test]
    fn a_members_answer_for_the_same_contract_is_not_taken() {
        let vk = key();
        let m = mirror_for(&vk, true, ViewedRoomStatus::Loading);
        let bytes = state_bytes(PrivacyMode::Public);
        for answer in [Answer::State(&bytes), Answer::NotFound, Answer::Delta] {
            let r = route(Some(&m), &m.id, true, answer);
            assert_eq!(
                r, NOT_CLAIMED,
                "ours (ROOMS, SYNC_INFO or PENDING_INVITES) must pass through"
            );
        }
    }

    #[test]
    fn nothing_is_claimed_before_the_viewer_asked_or_for_another_contract() {
        let vk = key();
        let bytes = state_bytes(PrivacyMode::Public);
        let unasked = mirror_for(&vk, false, ViewedRoomStatus::Loading);
        assert_eq!(
            route(Some(&unasked), &unasked.id, false, Answer::State(&bytes)),
            NOT_CLAIMED
        );
        let m = mirror_for(&vk, true, ViewedRoomStatus::Loading);
        let other =
            *owner_vk_to_contract_key(&SigningKey::from_bytes(&[9u8; 32]).verifying_key()).id();
        assert_eq!(
            route(Some(&m), &other, false, Answer::NotFound),
            NOT_CLAIMED
        );
        assert_eq!(route(None, &m.id, false, Answer::NotFound), NOT_CLAIMED);
    }

    #[test]
    fn a_later_not_found_or_unreadable_state_keeps_a_good_view() {
        let vk = key();
        let good = ViewedRoomStatus::Public(Box::new(state_with(PrivacyMode::Public)));
        let m = mirror_for(&vk, true, good);
        let r = route(Some(&m), &m.id, false, Answer::NotFound);
        assert!(r.claimed);
        assert_eq!(r.status, None);
        let r = route(Some(&m), &m.id, false, Answer::State(b"not cbor"));
        assert!(r.claimed);
        assert_eq!(r.status, None);
        // A delta on a public room asks for the room again.
        assert_eq!(
            route(Some(&m), &m.id, false, Answer::Delta).follow_up,
            Some(FollowUp::Refetch)
        );
        // While still loading, NotFound is shown.
        let loading = mirror_for(&vk, true, ViewedRoomStatus::Loading);
        assert_eq!(
            route(Some(&loading), &loading.id, false, Answer::NotFound).status,
            Some(ViewedRoomStatus::NotFound)
        );
    }

    #[test]
    fn the_fetch_waits_for_the_socket_and_skips_their_own() {
        let loading = ViewedRoomStatus::Loading;
        let public = ViewedRoomStatus::Public(Box::new(state_with(PrivacyMode::Public)));
        assert_eq!(
            fetch_action(true, true, false, false, &loading),
            Some(FetchAction::Get)
        );
        assert_eq!(
            fetch_action(true, false, false, false, &loading),
            Some(FetchAction::Get),
            "does not wait for the user's rooms to load"
        );
        assert_eq!(
            fetch_action(true, true, false, true, &loading),
            None,
            "our own room"
        );
        assert_eq!(
            fetch_action(false, true, false, false, &loading),
            None,
            "offline"
        );
        assert_eq!(
            fetch_action(true, false, true, false, &loading),
            None,
            "already asked"
        );
        // A reconnect re-subscribes a loaded public room, and only then.
        assert_eq!(
            fetch_action(true, true, true, false, &public),
            Some(FetchAction::Subscribe)
        );
        assert_eq!(fetch_action(true, false, true, false, &public), None);
        assert_eq!(
            fetch_action(true, true, true, false, &ViewedRoomStatus::InviteOnly),
            None
        );
    }

    #[test]
    fn waiting_hint_names_what_is_held_back() {
        assert_eq!(
            waiting_hint(false, false),
            Some("Connecting to Freenet\u{2026}")
        );
        assert_eq!(
            waiting_hint(false, true),
            Some("Connecting to Freenet\u{2026}")
        );
        assert_eq!(waiting_hint(true, true), Some("Loading your rooms\u{2026}"));
        assert_eq!(waiting_hint(true, false), None);
    }

    #[test]
    fn an_answer_before_the_rooms_load_is_held_not_claimed_as_a_visitors() {
        let vk = key();
        let loading = mirror_for(&vk, true, ViewedRoomStatus::Loading);
        let id = loading.id;
        let bytes = state_bytes(PrivacyMode::Public);
        // Rooms not loaded: held, so `route` never sees a room that may be theirs.
        assert!(should_hold(
            Some(&loading),
            &id,
            false,
            false,
            &Answer::State(&bytes)
        ));
        assert!(should_hold(
            Some(&loading),
            &id,
            false,
            false,
            &Answer::NotFound
        ));
        // Loaded: routed.
        assert!(!should_hold(
            Some(&loading),
            &id,
            true,
            false,
            &Answer::State(&bytes)
        ));
        // Not the viewer's, not asked, a delta, or a view already shown.
        let other = SigningKey::from_bytes(&[9u8; 32]).verifying_key();
        let other_id = *owner_vk_to_contract_key(&other).id();
        assert!(!should_hold(
            Some(&loading),
            &other_id,
            false,
            false,
            &Answer::NotFound
        ));
        let unasked = mirror_for(&vk, false, ViewedRoomStatus::Loading);
        assert!(!should_hold(
            Some(&unasked),
            &id,
            false,
            false,
            &Answer::NotFound
        ));
        assert!(!should_hold(
            Some(&loading),
            &id,
            false,
            false,
            &Answer::Delta
        ));
        let shown = mirror_for(
            &vk,
            true,
            ViewedRoomStatus::Public(Box::new(state_with(PrivacyMode::Public))),
        );
        assert!(!should_hold(
            Some(&shown),
            &id,
            false,
            false,
            &Answer::NotFound
        ));
        // Released after the rooms loaded and the room is theirs: not claimed,
        // nothing written, so the pane's redirect opens the member room.
        let mut fx = Recorder::default();
        assert!(!deliver_with(
            Some(&loading),
            true,
            &id,
            Answer::State(&bytes),
            &mut fx
        ));
        assert!(fx.statuses.is_empty() && fx.subscribed == 0);
        // Released and not theirs: shown as a visitor's view and subscribed.
        let mut fx = Recorder::default();
        assert!(deliver_with(
            Some(&loading),
            false,
            &id,
            Answer::State(&bytes),
            &mut fx
        ));
        assert_eq!(fx.subscribed, 1);
    }

    #[test]
    fn a_lookup_keeps_retrying_only_while_it_is_the_current_loading_one() {
        let vk = key();
        let loading = mirror_for(&vk, true, ViewedRoomStatus::Loading);
        assert!(lookup_still_waiting(Some(&loading), &vk, true));
        assert!(
            !lookup_still_waiting(Some(&loading), &vk, false),
            "superseded"
        );
        assert!(!lookup_still_waiting(None, &vk, true), "closed");
        let other = SigningKey::from_bytes(&[8u8; 32]).verifying_key();
        assert!(
            !lookup_still_waiting(Some(&loading), &other, true),
            "another room"
        );
        for status in [
            ViewedRoomStatus::NotFound,
            ViewedRoomStatus::TimedOut,
            ViewedRoomStatus::Public(Box::new(state_with(PrivacyMode::Public))),
        ] {
            let m = mirror_for(&vk, true, status);
            assert!(!lookup_still_waiting(Some(&m), &vk, true), "answered");
        }
    }

    #[test]
    fn the_retry_comes_before_the_timeout_which_is_unchanged() {
        assert!(LOOKUP_RETRY_AFTER < FETCH_TIMEOUT);
        assert_eq!(FETCH_TIMEOUT, std::time::Duration::from_secs(20));
    }

    #[test]
    fn a_held_answer_is_not_reported_as_a_timeout_while_rooms_load() {
        use std::time::Duration;
        let at_timeout = FETCH_TIMEOUT;
        // Nothing held: the timeout is as it always was.
        assert_eq!(
            timeout_action(true, false, false, at_timeout),
            TimeoutAction::TimedOut
        );
        assert_eq!(
            timeout_action(true, false, true, at_timeout),
            TimeoutAction::TimedOut
        );
        // A good answer is held and the rooms are still loading: no error.
        assert_eq!(
            timeout_action(true, true, false, at_timeout),
            TimeoutAction::KeepWaiting
        );
        // The rooms loaded: show the answer.
        assert_eq!(
            timeout_action(true, true, true, at_timeout),
            TimeoutAction::Release
        );
        // The backstop: show it as a visitor's view even though they have not.
        assert_eq!(
            timeout_action(true, true, false, HELD_ANSWER_BACKSTOP),
            TimeoutAction::Release
        );
        assert_eq!(
            timeout_action(
                true,
                true,
                false,
                HELD_ANSWER_BACKSTOP - Duration::from_secs(1)
            ),
            TimeoutAction::KeepWaiting
        );
        // Answered, replaced or closed meanwhile.
        assert_eq!(
            timeout_action(false, true, false, at_timeout),
            TimeoutAction::Nothing
        );
        assert!(HELD_ANSWER_BACKSTOP > FETCH_TIMEOUT);
    }

    #[test]
    fn an_error_retries_once_then_ends_the_wait() {
        let vk = key();
        let loading = mirror_for(&vk, true, ViewedRoomStatus::Loading);
        let text = format!("request failed for contract {}: refused", loading.id);
        assert_eq!(
            error_action(Some(&loading), &text, false, false, false),
            ErrorAction::Retry
        );
        assert_eq!(
            error_action(Some(&loading), &text, false, true, false),
            ErrorAction::GiveUp
        );
        assert_eq!(
            error_action(Some(&loading), "other", false, false, false),
            ErrorAction::Ignore
        );
        assert_eq!(
            error_action(None, &text, false, false, false),
            ErrorAction::Ignore
        );
        assert_eq!(
            error_action(Some(&loading), &text, true, false, false),
            ErrorAction::Ignore,
            "the user's own room"
        );
        let unasked = mirror_for(&vk, false, ViewedRoomStatus::Loading);
        assert_eq!(
            error_action(Some(&unasked), &text, false, false, false),
            ErrorAction::Ignore
        );
        let shown = mirror_for(
            &vk,
            true,
            ViewedRoomStatus::Public(Box::new(state_with(PrivacyMode::Public))),
        );
        assert_eq!(
            error_action(Some(&shown), &text, false, false, false),
            ErrorAction::Ignore
        );
        // A later good answer is still accepted after the wait ended.
        let timed_out = mirror_for(&vk, true, ViewedRoomStatus::TimedOut);
        let bytes = state_bytes(PrivacyMode::Public);
        assert!(
            route(
                Some(&timed_out),
                &timed_out.id,
                false,
                Answer::State(&bytes)
            )
            .claimed
        );
    }

    #[test]
    fn a_not_found_never_replaces_a_held_state_and_held_answers_stop_retries() {
        let state = || Held::State(state_bytes(PrivacyMode::Public));
        assert!(matches!(
            merge_held(Some(state()), Held::NotFound),
            Held::State(_)
        ));
        assert!(matches!(
            merge_held(Some(Held::NotFound), state()),
            Held::State(_)
        ));
        assert!(matches!(merge_held(None, Held::NotFound), Held::NotFound));
        assert!(matches!(merge_held(Some(state()), state()), Held::State(_)));
        // With an answer held, an error neither retries nor ends the wait.
        let vk = key();
        let loading = mirror_for(&vk, true, ViewedRoomStatus::Loading);
        let text = format!("failed for contract {}", loading.id);
        assert_eq!(
            error_action(Some(&loading), &text, false, false, true),
            ErrorAction::Ignore
        );
    }

    #[test]
    fn the_users_own_room_is_not_held() {
        let vk = key();
        let loading = mirror_for(&vk, true, ViewedRoomStatus::Loading);
        assert!(!should_hold(
            Some(&loading),
            &loading.id,
            false,
            true,
            &Answer::NotFound
        ));
    }

    #[test]
    fn a_reopened_unrequested_lookup_is_not_driven_by_an_old_timer() {
        let vk = key();
        let reopened = mirror_for(&vk, false, ViewedRoomStatus::Loading);
        assert!(!lookup_still_waiting(Some(&reopened), &vk, true));
    }

    #[test]
    fn the_lookup_wiring_is_pinned_by_source() {
        let sync = handler_source("components/app/freenet_api/freenet_synchronizer.rs");
        let err_at = sync
            .find("Received error in API response")
            .expect("error arm");
        let hook_at = sync
            .find("public_room::deliver_error(")
            .expect("deliver_error hook");
        assert!(hook_at > err_at, "deliver_error runs in the error arm");
        let room = handler_source("public_room.rs");
        let fetch = &room[room
            .find("pub fn request_fetch(owner_vk: VerifyingKey) {\n    use std")
            .expect("request_fetch")..];
        let fetch = &fetch[..fetch.find("\n}\n").expect("end")];
        assert!(fetch.contains("LOOKUP_GEN") && fetch.contains("lookup_still_waiting("));
        assert_eq!(
            fetch.matches("lookup_still_waiting(").count(),
            2,
            "retry and timeout both check"
        );
        let deliver = &room[room.find("fn deliver(id").expect("deliver")..];
        assert!(deliver[..deliver.find("fn deliver_now").unwrap()].contains("should_hold("));
        let pane = handler_source("components/viewed_room.rs");
        assert!(pane.contains("waiting_hint(") && pane.contains("release_held()"));
    }

    #[test]
    fn an_invitation_in_the_url_wins_over_room() {
        assert_eq!(room_param_to_open(None, Some("k")), Some("k"));
        assert_eq!(room_param_to_open(Some("inv"), Some("k")), None);
        assert_eq!(room_param_to_open(Some("inv"), None), None);
        assert_eq!(room_param_to_open(None, None), None);
    }

    #[test]
    fn a_message_with_unreadable_author_still_has_text() {
        // Guards the `lines` input shape used by the pane: public text only.
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let m = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: MemberId::from(&sk.verifying_key()),
                author: MemberId::from(&sk.verifying_key()),
                time: std::time::SystemTime::UNIX_EPOCH.into(),
                content: RoomMessageBody::public("hi".to_string()),
            },
            &sk,
        );
        assert_eq!(m.message.content.as_public_string().as_deref(), Some("hi"));
    }

    #[derive(Default)]
    struct Recorder {
        statuses: Vec<ViewedRoomStatus>,
        subscribed: u32,
        refetched: u32,
    }

    impl Effects for Recorder {
        fn set_status(&mut self, _: VerifyingKey, status: ViewedRoomStatus) {
            self.statuses.push(status);
        }
        fn subscribe(&mut self, _: VerifyingKey) {
            self.subscribed += 1;
        }
        fn refetch(&mut self, _: VerifyingKey) {
            self.refetched += 1;
        }
    }

    #[test]
    fn delivering_a_viewers_state_claims_it_sets_the_status_and_subscribes() {
        let vk = key();
        let m = mirror_for(&vk, true, ViewedRoomStatus::Loading);
        let mut fx = Recorder::default();
        let bytes = state_bytes(PrivacyMode::Public);
        assert!(deliver_with(
            Some(&m),
            false,
            &m.id,
            Answer::State(&bytes),
            &mut fx
        ));
        assert!(matches!(fx.statuses[..], [ViewedRoomStatus::Public(_)]));
        assert_eq!((fx.subscribed, fx.refetched), (1, 0));
        let mut fx = Recorder::default();
        assert!(deliver_with(
            Some(&m),
            false,
            &m.id,
            Answer::NotFound,
            &mut fx
        ));
        assert_eq!(fx.statuses, vec![ViewedRoomStatus::NotFound]);
    }

    #[test]
    fn delivering_a_private_state_leaves_only_invite_only_and_sends_nothing() {
        let vk = key();
        let m = mirror_for(&vk, true, ViewedRoomStatus::Loading);
        let mut fx = Recorder::default();
        let bytes = state_bytes(PrivacyMode::Private);
        assert!(deliver_with(
            Some(&m),
            false,
            &m.id,
            Answer::State(&bytes),
            &mut fx
        ));
        assert_eq!(fx.statuses, vec![ViewedRoomStatus::InviteOnly]);
        assert_eq!((fx.subscribed, fx.refetched), (0, 0));
    }

    #[test]
    fn a_member_or_pending_invitee_is_never_swallowed_and_nothing_is_written() {
        // `ours` is true for ROOMS, SYNC_INFO and PENDING_INVITES alike, and
        // for a contended signal.
        let vk = key();
        let m = mirror_for(&vk, true, ViewedRoomStatus::Loading);
        let bytes = state_bytes(PrivacyMode::Public);
        for answer in [Answer::State(&bytes), Answer::NotFound, Answer::Delta] {
            let mut fx = Recorder::default();
            assert!(!deliver_with(Some(&m), true, &m.id, answer, &mut fx));
            assert!(fx.statuses.is_empty() && fx.subscribed == 0 && fx.refetched == 0);
        }
        // Before the viewer has asked (e.g. before ROOMS hydrates), an answer
        // for the same contract is not the viewer's either.
        let unasked = mirror_for(&vk, false, ViewedRoomStatus::Loading);
        let mut fx = Recorder::default();
        assert!(!deliver_with(
            Some(&unasked),
            false,
            &unasked.id,
            Answer::State(&bytes),
            &mut fx
        ));
        assert!(fx.statuses.is_empty());
    }

    #[test]
    fn a_delta_for_a_public_view_refetches_and_a_non_viewed_id_is_not_claimed() {
        let vk = key();
        let public = ViewedRoomStatus::Public(Box::new(state_with(PrivacyMode::Public)));
        let m = mirror_for(&vk, true, public);
        let mut fx = Recorder::default();
        assert!(deliver_with(Some(&m), false, &m.id, Answer::Delta, &mut fx));
        assert_eq!(fx.refetched, 1);
        let other =
            *owner_vk_to_contract_key(&SigningKey::from_bytes(&[9u8; 32]).verifying_key()).id();
        let mut fx = Recorder::default();
        assert!(!deliver_with(
            Some(&m),
            false,
            &other,
            Answer::Delta,
            &mut fx
        ));
        assert_eq!(fx.refetched, 0);
    }

    #[test]
    fn the_viewer_never_writes_to_the_delegate_or_rooms() {
        let src = handler_source("public_room.rs");
        for forbidden in [
            "ROOMS.write",
            "SYNC_INFO.write",
            "PENDING_INVITES.write",
            "chat_delegate",
        ] {
            assert!(
                !src.contains(forbidden),
                "viewer code must not touch {forbidden}"
            );
        }
    }

    // ---- the hooks stay in place ---------------------------------------
    //
    // The handlers cannot be driven natively (they need the Dioxus runtime and
    // a node), so each claim-first hook is pinned by source: deleting one
    // fails here. The routing rules they call are tested above.

    fn handler_source(rel: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join(rel);
        crate::util::source_scan::production_only(&std::fs::read_to_string(path).expect("readable"))
    }

    #[test]
    fn every_response_path_offers_the_answer_to_the_viewer_first() {
        let get = handler_source("components/app/freenet_api/response_handler/get_response.rs");
        let state_at = get.find("public_room::deliver_state(").expect("GET hook");
        let notfound_at = get
            .find("public_room::deliver_not_found(")
            .expect("NotFound hook");
        // Before any lookup by SYNC_INFO or ROOMS.
        assert!(state_at < get.find("SYNC_INFO.read()").expect("SYNC_INFO lookup"));
        assert!(
            notfound_at
                < get
                    .find("get_owner_vk_for_instance_id(&instance_id)")
                    .expect("lookup")
        );
        let upd =
            handler_source("components/app/freenet_api/response_handler/update_notification.rs");
        assert!(
            upd.find("public_room::deliver_update(")
                .expect("update hook")
                < upd
                    .find("get_owner_vk_for_instance_id(key.id())")
                    .expect("lookup")
        );
        let sub =
            handler_source("components/app/freenet_api/response_handler/subscribe_response.rs");
        assert!(
            sub.contains("public_room::is_viewers("),
            "subscribe responses for the viewer are not a room's"
        );
        let handler = handler_source("components/app/freenet_api/response_handler.rs");
        assert!(
            handler.contains("public_room::is_viewers("),
            "the post-subscribe GET is not sent for the viewed room"
        );
    }

    #[test]
    fn the_address_is_read_before_the_invitation_is_removed_from_it() {
        // `App` deletes `?invitation=` from the URL; VIEWED_ROOM judges the
        // address on first access, so App must touch it first or a link with
        // both opens the visitor view.
        let app = handler_source("components/app.rs");
        let read_at = app.find("VIEWED_ROOM.peek()").expect("early read");
        let delete_at = app
            .find("params.delete(\"invitation\")")
            .expect("invitation removal");
        assert!(read_at < delete_at);
    }

    #[test]
    fn a_redirect_applies_only_while_the_viewer_shows_the_captured_room() {
        let a = VerifyingKey::from(&ed25519_dalek::SigningKey::from_bytes(&[1; 32]));
        let b = VerifyingKey::from(&ed25519_dalek::SigningKey::from_bytes(&[2; 32]));
        assert!(should_redirect(Some(a), a));
        assert!(!should_redirect(None, a), "viewer already closed");
        assert!(!should_redirect(Some(b), a), "another room opened since");
    }

    #[test]
    fn the_viewer_closes_wherever_the_selected_room_changes() {
        for rel in [
            "components/room_list.rs",
            "components/app/notifications.rs",
            "components/app/freenet_api/response_handler/get_response.rs",
            "components/members.rs",
            "components/room_list/edit_room_modal.rs",
            "components/room_list/create_room_modal.rs",
        ] {
            assert!(
                handler_source(rel).contains("public_room::close()"),
                "{rel} changes the selected room and must close the read-only viewer"
            );
        }
    }
}
