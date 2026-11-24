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
/// whether joining is on the way (step 2 of #975); this PR does not implement
/// admission.
pub fn composer_note(state: &ChatRoomStateV1) -> &'static str {
    if state.configuration.configuration.public_admission.is_some() {
        "You are viewing this room read-only. Joining without an invitation is coming soon."
    } else {
        "You are viewing this room read-only. Joining needs an invitation from a member."
    }
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

/// What to send now. Nothing until the user's own rooms have loaded (a link to
/// a room they belong to must open as theirs, not be fetched as a visitor's),
/// and never for a room that is theirs.
pub fn fetch_action(
    connected: bool,
    just_connected: bool,
    already_asked: bool,
    rooms_loaded: bool,
    ours: bool,
    status: &ViewedRoomStatus,
) -> Option<FetchAction> {
    if !connected || !rooms_loaded || ours {
        return None;
    }
    match status {
        ViewedRoomStatus::Loading if just_connected || !already_asked => Some(FetchAction::Get),
        ViewedRoomStatus::Public(_) if just_connected => Some(FetchAction::Subscribe),
        _ => None,
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
    crate::util::defer(|| {
        set_mirror(None);
        let showing = VIEWED_ROOM.try_read().ok().is_some_and(|v| v.is_some());
        if showing {
            *VIEWED_ROOM.write() = None;
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

/// Ask the network for the room (not subscribed: it may be private), and
/// start the timeout after which the viewer offers a retry.
#[cfg(not(feature = "no-sync"))]
pub fn request_fetch(owner_vk: VerifyingKey) {
    use crate::components::app::WEB_API;
    use freenet_stdlib::client_api::{ClientRequest, ContractRequest};
    with_mirror(|m| m.requested = true);
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
        crate::util::sleep(FETCH_TIMEOUT).await;
        if mirror().is_some_and(|m| m.owner_vk == owner_vk && m.status == ViewedRoomStatus::Loading)
        {
            set_status(owner_vk, ViewedRoomStatus::TimedOut);
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
    set_status(owner_vk, ViewedRoomStatus::Loading);
    request_fetch(owner_vk);
}

fn deliver(id: &ContractInstanceId, answer: Answer) -> bool {
    let m = mirror();
    let ours = m.as_ref().is_some_and(|m| is_ours(&m.owner_vk, id));
    let routed = route(m.as_ref(), id, ours, answer);
    if let (Some(status), Some(m)) = (routed.status, m.as_ref()) {
        set_status(m.owner_vk, status);
    }
    if let (Some(follow_up), Some(m)) = (routed.follow_up, m.as_ref()) {
        match follow_up {
            FollowUp::Subscribe => request_subscribe(m.owner_vk),
            FollowUp::Refetch => request_refetch(m.owner_vk),
        }
    }
    routed.claimed
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
        assert!(composer_note(&closed).contains("needs an invitation"));
        assert!(!composer_note(&closed).contains("coming"));
    }

    // ---- claim-first routing -------------------------------------------

    use crate::util::owner_vk_to_contract_key;
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
    fn the_fetch_waits_for_the_users_rooms_and_skips_their_own() {
        let loading = ViewedRoomStatus::Loading;
        let public = ViewedRoomStatus::Public(Box::new(state_with(PrivacyMode::Public)));
        assert_eq!(
            fetch_action(true, true, false, true, false, &loading),
            Some(FetchAction::Get)
        );
        assert_eq!(
            fetch_action(true, false, false, false, false, &loading),
            None,
            "rooms not loaded"
        );
        assert_eq!(
            fetch_action(true, true, false, true, true, &loading),
            None,
            "our own room"
        );
        assert_eq!(
            fetch_action(false, true, false, true, false, &loading),
            None,
            "offline"
        );
        assert_eq!(
            fetch_action(true, false, true, true, false, &loading),
            None,
            "already asked"
        );
        // A reconnect re-subscribes a loaded public room, and only then.
        assert_eq!(
            fetch_action(true, true, true, true, false, &public),
            Some(FetchAction::Subscribe)
        );
        assert_eq!(fetch_action(true, false, true, true, false, &public), None);
        assert_eq!(
            fetch_action(true, true, true, true, false, &ViewedRoomStatus::InviteOnly),
            None
        );
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
                time: std::time::SystemTime::UNIX_EPOCH,
                content: RoomMessageBody::public("hi".to_string()),
            },
            &sk,
        );
        assert_eq!(m.message.content.as_public_string().as_deref(), Some("hi"));
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
