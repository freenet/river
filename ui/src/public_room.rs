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

/// The contract id of the room being viewed, mirrored out of the signal so the
/// response handlers can test it without a signal read.
static VIEWED_ID: Mutex<Option<ContractInstanceId>> = Mutex::new(None);

fn set_viewed_id(id: Option<ContractInstanceId>) {
    *VIEWED_ID.lock().unwrap_or_else(|e| e.into_inner()) = id;
}

/// Whether `id` is the contract being viewed.
pub fn is_viewed(id: &ContractInstanceId) -> bool {
    VIEWED_ID
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|v| v == id)
}

/// Decide what a room state means for a visitor. Pure, so it is unit-tested.
///
/// A private room's state is not kept at all: no ciphertext and no member list
/// reach the UI model, and nothing here attempts decryption.
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

/// Whether to (re)send the fetch: connected, and still waiting for an answer.
pub fn should_request(connected: bool, status: &ViewedRoomStatus) -> bool {
    connected && *status == ViewedRoomStatus::Loading
}

fn set_status(owner_vk: VerifyingKey, status: ViewedRoomStatus) {
    crate::util::defer(move || {
        // Ignore an answer for a room that is no longer the one being viewed.
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

fn viewed_owner() -> Option<VerifyingKey> {
    VIEWED_ROOM.try_read().ok()?.as_ref()?.owner_vk
}

/// What a `?room=` value opens: the room (waiting for the network) or the
/// reason it was refused. Records the contract id the response handlers match.
fn viewed_from_param(raw: &str) -> ViewedRoom {
    match parse_room_param(raw) {
        Ok(owner_vk) => {
            set_viewed_id(Some(*crate::util::owner_vk_to_contract_key(&owner_vk).id()));
            // The no-sync example build has no network: answer from fixtures.
            #[cfg(all(feature = "example-data", feature = "no-sync"))]
            let status = classify_state(crate::example_data::viewable_room_state(&owner_vk));
            #[cfg(not(all(feature = "example-data", feature = "no-sync")))]
            let status = ViewedRoomStatus::Loading;
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

/// The room named by the page's `?room=` parameter, if any. An invitation in
/// the same URL wins: that flow is unchanged.
fn initial_viewed_room() -> Option<ViewedRoom> {
    #[cfg(target_arch = "wasm32")]
    {
        let search = web_sys::window()?.location().search().ok()?;
        let params = web_sys::UrlSearchParams::new_with_str(&search).ok()?;
        if params.get("invitation").is_some() {
            return None;
        }
        params.get(ROOM_PARAM).map(|raw| viewed_from_param(&raw))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        None
    }
}

/// Stop viewing, and drop `?room=` from the address bar where the page may
/// rewrite it (not in the sandboxed gateway iframe, which has an opaque origin).
pub fn close() {
    crate::util::defer(|| {
        set_viewed_id(None);
        *VIEWED_ROOM.write() = None;
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

/// Ask the network for the room (and subscribe, so edits arrive).
#[cfg(not(feature = "no-sync"))]
pub fn request_fetch(owner_vk: VerifyingKey) {
    use crate::components::app::freenet_api::room_synchronizer::subscribe_get_request;
    use crate::components::app::WEB_API;
    use freenet_stdlib::client_api::ClientRequest;
    crate::util::safe_spawn_local(async move {
        let request = ClientRequest::ContractOp(subscribe_get_request(&owner_vk));
        if let Some(web_api) = WEB_API.write().as_mut() {
            if let Err(e) = web_api.send(request).await {
                dioxus::logger::tracing::warn!("public room: fetch failed to send: {e}");
            }
        }
    });
}

#[cfg(feature = "no-sync")]
pub fn request_fetch(_owner_vk: VerifyingKey) {}

/// Route a GET response. `true` if `id` is the viewed room, which the caller
/// must then not treat as one of the user's rooms.
pub fn deliver_state(id: &ContractInstanceId, bytes: &[u8]) -> bool {
    if !is_viewed(id) {
        return false;
    }
    if let Some(owner_vk) = viewed_owner() {
        let state = crate::util::try_from_cbor_slice::<ChatRoomStateV1>(bytes);
        set_status(owner_vk, classify_state(state));
    }
    true
}

/// Route a `NotFound`. `true` if `id` is the viewed room.
pub fn deliver_not_found(id: &ContractInstanceId) -> bool {
    if !is_viewed(id) {
        return false;
    }
    if let Some(owner_vk) = viewed_owner() {
        set_status(owner_vk, ViewedRoomStatus::NotFound);
    }
    true
}

/// Route an update notification. `true` if `id` is the viewed room.
///
/// A full state replaces what is shown. A delta is not applied here: the room
/// is fetched again, which is a local read for a subscribed contract and keeps
/// this path free of merge logic.
pub fn deliver_update(id: &ContractInstanceId, update: &UpdateData) -> bool {
    if !is_viewed(id) {
        return false;
    }
    match update {
        UpdateData::State(bytes) => {
            deliver_state(id, bytes.as_ref());
        }
        _ => {
            if let Some(owner_vk) = viewed_owner() {
                request_refetch(owner_vk);
            }
        }
    }
    true
}

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
    fn the_fetch_is_sent_only_when_connected_and_waiting() {
        assert!(should_request(true, &ViewedRoomStatus::Loading));
        assert!(!should_request(false, &ViewedRoomStatus::Loading));
        assert!(!should_request(true, &ViewedRoomStatus::NotFound));
        assert!(!should_request(true, &ViewedRoomStatus::InviteOnly));
    }

    #[test]
    fn the_composer_note_follows_the_rooms_admission_setting() {
        let closed = state_with(PrivacyMode::Public);
        assert!(composer_note(&closed).contains("needs an invitation"));
        assert!(!composer_note(&closed).contains("coming"));
    }
}
