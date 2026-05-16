use crate::components::app::{CURRENT_ROOM, ROOMS, WEB_API};
use dioxus::logger::tracing::{debug, error, info, warn};
use dioxus::prelude::*;
use freenet_stdlib::client_api::ClientRequest::DelegateOp;
use freenet_stdlib::client_api::DelegateRequest;
use freenet_stdlib::prelude::{
    CodeHash, Delegate, DelegateCode, DelegateContainer, DelegateKey, DelegateWasmAPIVersion,
    Parameters,
};
use futures::channel::oneshot;
use futures::future::{select, Either};
use river_core::chat_delegate::{
    ChatDelegateKey, ChatDelegateRequestMsg, ChatDelegateResponseMsg, OutboundDmEntry,
    OutboundDmStore, RequestId, RoomKey,
};
use river_core::room_state::direct_messages::{PurgeToken, MAX_DM_MESSAGES_PER_PAIR};
use river_core::room_state::member::MemberId;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

// Constant for the rooms storage key
pub const ROOMS_STORAGE_KEY: &[u8] = b"rooms_data";

// Re-export so other UI modules don't have to reach into `river_core` for the key.
pub use river_core::chat_delegate::OUTBOUND_DMS_STORAGE_KEY;

// =============================================================================
// LEGACY DELEGATE MIGRATION
// When the delegate WASM changes (dependency updates, code changes), the delegate
// key changes and old secrets become inaccessible. This migration code attempts
// to load room data from known previous delegate keys and migrate it to the
// current delegate.
//
// Legacy entries are defined in legacy_delegates.toml at the repo root.
// The build.rs generates the const array at compile time.
// To add a new entry: cargo make add-migration
// =============================================================================

/// Previous delegate keys for migration, generated from legacy_delegates.toml.
mod generated_legacy {
    include!(concat!(env!("OUT_DIR"), "/legacy_delegates.rs"));
}
use generated_legacy::LEGACY_DELEGATES;

/// Check if a delegate key matches any known legacy delegate
pub fn is_legacy_delegate_key(key_bytes: &[u8]) -> bool {
    LEGACY_DELEGATES
        .iter()
        .any(|(dk, _)| dk.as_slice() == key_bytes)
}

// Prefixes for different pending request types
const SIGNING_KEY_PREFIX: &[u8] = b"__signing_key:";
const PUBLIC_KEY_PREFIX: &[u8] = b"__public_key:";
const SIGN_PREFIX: &[u8] = b"__sign:";

/// Atomic counter for generating unique request IDs
static REQUEST_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Generate a unique request ID for signing requests
pub fn generate_request_id() -> RequestId {
    REQUEST_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Registry for pending delegate requests.
/// Maps request keys to oneshot senders that will receive the response.
static PENDING_REQUESTS: std::sync::LazyLock<
    Mutex<HashMap<Vec<u8>, oneshot::Sender<ChatDelegateResponseMsg>>>,
> = std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Complete a pending delegate request with the given response.
/// Called by the response handler when a delegate response is received.
pub fn complete_pending_request(key: &ChatDelegateKey, response: ChatDelegateResponseMsg) -> bool {
    let key_bytes = key.as_bytes().to_vec();
    complete_pending_request_bytes(&key_bytes, response)
}

/// Complete a pending signing key store request.
pub fn complete_pending_signing_key_request(
    room_key: &RoomKey,
    response: ChatDelegateResponseMsg,
) -> bool {
    let mut key_bytes = SIGNING_KEY_PREFIX.to_vec();
    key_bytes.extend_from_slice(room_key);
    complete_pending_request_bytes(&key_bytes, response)
}

/// Complete a pending public key request.
pub fn complete_pending_public_key_request(
    room_key: &RoomKey,
    response: ChatDelegateResponseMsg,
) -> bool {
    let mut key_bytes = PUBLIC_KEY_PREFIX.to_vec();
    key_bytes.extend_from_slice(room_key);
    complete_pending_request_bytes(&key_bytes, response)
}

/// Complete a pending signing request using room_key and request_id for correlation.
pub fn complete_pending_sign_request(
    room_key: &RoomKey,
    request_id: RequestId,
    response: ChatDelegateResponseMsg,
) -> bool {
    let mut key_bytes = SIGN_PREFIX.to_vec();
    key_bytes.extend_from_slice(room_key);
    key_bytes.extend_from_slice(&request_id.to_le_bytes());
    complete_pending_request_bytes(&key_bytes, response)
}

/// Internal function to complete a pending request by key bytes.
fn complete_pending_request_bytes(key_bytes: &[u8], response: ChatDelegateResponseMsg) -> bool {
    if let Ok(mut pending) = PENDING_REQUESTS.lock() {
        if let Some(sender) = pending.remove(key_bytes) {
            if sender.send(response).is_ok() {
                info!(
                    "Completed pending request for key: {:?}",
                    String::from_utf8_lossy(key_bytes)
                );
                return true;
            }
        }
    }
    false
}

pub async fn set_up_chat_delegate() -> Result<(), String> {
    let delegate = create_chat_delegate_container();

    // Get a write lock on the API and use it directly
    let api_result = {
        let mut web_api = WEB_API.write();
        if let Some(api) = web_api.as_mut() {
            // Perform the operation while holding the lock
            info!("Registering chat delegate");
            api.send(DelegateOp(DelegateRequest::RegisterDelegate {
                delegate,
                cipher: DelegateRequest::DEFAULT_CIPHER,
                nonce: DelegateRequest::DEFAULT_NONCE,
            }))
            .await
        } else {
            Err(freenet_stdlib::client_api::Error::ConnectionClosed)
        }
    };

    match api_result {
        Ok(_) => {
            info!("Chat delegate registered successfully");
            // NOTE: We don't await load_rooms_from_delegate() here because it would
            // deadlock - it waits for a response that comes through the same message
            // loop that called us. Instead, we fire off the request and let the
            // response be handled by the response_handler through the message loop.
            //
            // The response handler will process GetResponse and populate ROOMS.
            //
            // Legacy migration is NOT fired here. It is gated on the current
            // delegate's response: if the current delegate has data, migration is
            // skipped (current is authoritative); if it is empty, migration is
            // fired then. This prevents legacy responses from racing with the
            // current delegate and clobbering newer state (freenet/river#253).
            fire_load_rooms_request().await;
            fire_load_outbound_dms_request().await;

            Ok(())
        }
        Err(e) => Err(format!("Failed to register chat delegate: {}", e)),
    }
}

/// Per-session dedup set for `EnsureRoomSubscription` calls. The UI may
/// re-fire its load-rooms path on every `rooms_data` reload, but we only
/// need to ask the delegate to (re-)subscribe each room once per session —
/// the delegate's secret store keeps the sub_index across `process()`
/// invocations.
static ENSURE_SUBSCRIPTION_SENT: std::sync::LazyLock<Mutex<HashSet<RoomKey>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashSet::new()));

/// Reset the per-session dedup set. Used in tests; not called in production.
#[cfg(test)]
pub(crate) fn reset_ensure_subscription_dedup() {
    if let Ok(mut s) = ENSURE_SUBSCRIPTION_SENT.lock() {
        s.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Current delegate returned no `rooms_data` value at all — fire migration.
    #[test]
    fn decide_value_absent_fires_migration() {
        assert_eq!(
            decide_legacy_migration_action(false, false, false),
            LegacyMigrationAction::FireMigration,
        );
    }

    /// Current delegate returned `Some(serialized empty Rooms)` (placeholder
    /// written by save_rooms_to_delegate before any room existed). This is
    /// indistinguishable from "user has no data yet" — fire migration so users
    /// with rooms only under a legacy delegate aren't stranded.
    #[test]
    fn decide_value_present_but_empty_fires_migration() {
        assert_eq!(
            decide_legacy_migration_action(true, false, false),
            LegacyMigrationAction::FireMigration,
        );
    }

    /// Current delegate has actual rooms — it is the source of truth.
    /// Block legacy migration permanently to prevent the freenet/river#253
    /// race where stale legacy data overwrites it.
    #[test]
    fn decide_has_rooms_marks_done() {
        assert_eq!(
            decide_legacy_migration_action(true, true, false),
            LegacyMigrationAction::MarkDone,
        );
    }

    /// Current delegate has only tombstones (user left all rooms). Still
    /// authoritative state — the tombstones must be preserved, and a legacy
    /// migration that re-introduces the abandoned rooms would defeat the
    /// freenet/river#247 tombstone fix.
    #[test]
    fn decide_has_tombstones_only_marks_done() {
        assert_eq!(
            decide_legacy_migration_action(true, false, true),
            LegacyMigrationAction::MarkDone,
        );
    }

    /// Both rooms and tombstones present — definitely authoritative.
    #[test]
    fn decide_has_rooms_and_tombstones_marks_done() {
        assert_eq!(
            decide_legacy_migration_action(true, true, true),
            LegacyMigrationAction::MarkDone,
        );
    }
}

/// Idempotent helper: ask the chat delegate to subscribe to a room
/// contract, but only if we haven't already done so this session.
///
/// Returns `Ok(true)` if a request was sent, `Ok(false)` if it was a no-op
/// because the subscription already fired this session.
pub(crate) async fn ensure_room_subscription_once(
    room_owner_vk: RoomKey,
    contract_id: [u8; 32],
) -> Result<bool, String> {
    {
        let mut sent = ENSURE_SUBSCRIPTION_SENT
            .lock()
            .map_err(|e| format!("Failed to lock ensure-subscription dedup set: {e}"))?;
        if !sent.insert(room_owner_vk) {
            return Ok(false);
        }
    }

    let req = ChatDelegateRequestMsg::EnsureRoomSubscription {
        room_owner_vk,
        contract_id,
    };
    fire_ensure_room_subscription(req).await?;
    Ok(true)
}

/// Fire an `EnsureRoomSubscription` request to the chat delegate without
/// awaiting the response. The delegate replies asynchronously via the normal
/// response loop (we just log the outcome there).
///
/// This is the UI-side hook for #228 PR 2: every time we re-load owner-mode
/// rooms from the chat delegate, we ask the delegate to (re-)subscribe to
/// each room contract so it can drive the secret rotation pipeline.
pub(crate) async fn fire_ensure_room_subscription(
    request: ChatDelegateRequestMsg,
) -> Result<(), String> {
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&request, &mut payload)
        .map_err(|e| format!("Failed to serialize EnsureRoomSubscription: {e}"))?;

    let delegate_code =
        DelegateCode::from(include_bytes!("../../../public/contracts/chat_delegate.wasm").to_vec());
    let params = Parameters::from(Vec::<u8>::new());
    let delegate = Delegate::from((&delegate_code, &params));
    let delegate_key = delegate.key().clone();

    let app_msg = freenet_stdlib::prelude::ApplicationMessage::new(payload);
    let delegate_request = DelegateOp(DelegateRequest::ApplicationMessages {
        key: delegate_key,
        params: Parameters::from(Vec::<u8>::new()),
        inbound: vec![freenet_stdlib::prelude::InboundDelegateMsg::ApplicationMessage(app_msg)],
    });

    let api_result = {
        let mut web_api = WEB_API.write();
        if let Some(api) = web_api.as_mut() {
            api.send(delegate_request).await
        } else {
            Err(freenet_stdlib::client_api::Error::ConnectionClosed)
        }
    };

    api_result.map_err(|e| format!("Failed to send EnsureRoomSubscription: {e}"))?;
    Ok(())
}

/// Fire a request to load rooms from delegate storage without waiting for response.
/// The response will be handled by the response_handler through the message loop.
/// This avoids deadlock when called from inside the message loop.
async fn fire_load_rooms_request() {
    info!("Firing request to load rooms from delegate storage");

    let request = ChatDelegateRequestMsg::GetRequest {
        key: ChatDelegateKey::new(ROOMS_STORAGE_KEY.to_vec()),
    };

    // Serialize and send the request without waiting for response
    let mut payload = Vec::new();
    if let Err(e) = ciborium::ser::into_writer(&request, &mut payload) {
        error!("Failed to serialize load rooms request: {}", e);
        return;
    }

    let delegate_code =
        DelegateCode::from(include_bytes!("../../../public/contracts/chat_delegate.wasm").to_vec());
    let params = Parameters::from(Vec::<u8>::new());
    let delegate = Delegate::from((&delegate_code, &params));
    let delegate_key = delegate.key().clone();

    let app_msg = freenet_stdlib::prelude::ApplicationMessage::new(payload);

    let delegate_request = DelegateOp(DelegateRequest::ApplicationMessages {
        key: delegate_key,
        params: Parameters::from(Vec::<u8>::new()),
        inbound: vec![freenet_stdlib::prelude::InboundDelegateMsg::ApplicationMessage(app_msg)],
    });

    // Send without waiting for response
    let api_result = {
        let mut web_api = WEB_API.write();
        if let Some(api) = web_api.as_mut() {
            api.send(delegate_request).await
        } else {
            Err(freenet_stdlib::client_api::Error::ConnectionClosed)
        }
    };

    if let Err(e) = api_result {
        error!("Failed to send load rooms request: {}", e);
    } else {
        info!("Load rooms request sent, response will be handled by message loop");
    }
}

/// Load rooms from the delegate storage (with response waiting - use outside message loop only)
#[allow(dead_code)]
pub async fn load_rooms_from_delegate() -> Result<(), String> {
    info!("Loading rooms from delegate storage");

    // Create a get request for the rooms data
    let request = ChatDelegateRequestMsg::GetRequest {
        key: ChatDelegateKey::new(ROOMS_STORAGE_KEY.to_vec()),
    };

    // Send the request to the delegate
    match send_delegate_request(request).await {
        Ok(_) => {
            info!("Sent request to load rooms from delegate");
            Ok(())
        }
        Err(e) => {
            warn!("Failed to load rooms from delegate: {}", e);
            // Don't fail the app if we can't load rooms
            Ok(())
        }
    }
}

/// Save rooms to the delegate storage
pub async fn save_rooms_to_delegate() -> Result<(), String> {
    info!("Saving rooms to delegate storage");

    // Get the current rooms data - clone the data to avoid holding the read lock
    let rooms_data = {
        let mut rooms_clone = ROOMS.read().clone();
        // Include the current room selection
        rooms_clone.current_room_key = CURRENT_ROOM.read().owner_key;
        let mut buffer = Vec::new();
        ciborium::ser::into_writer(&rooms_clone, &mut buffer)
            .map_err(|e| format!("Failed to serialize rooms: {}", e))?;
        buffer
    };

    // Create a store request for the rooms data
    let request = ChatDelegateRequestMsg::StoreRequest {
        key: ChatDelegateKey::new(ROOMS_STORAGE_KEY.to_vec()),
        value: rooms_data,
    };

    // Send the request to the delegate
    match send_delegate_request(request).await {
        Ok(ChatDelegateResponseMsg::StoreResponse { result, .. }) => result,
        Ok(other) => Err(format!("Unexpected response: {:?}", other)),
        Err(e) => Err(e),
    }
}

// =============================================================================
// OUTBOUND DM PLAINTEXT CACHE (issue freenet/river#256)
//
// The room contract carries DM bodies as ECIES ciphertext only the recipient
// can decrypt, so without a side-channel the sender's own UI/`riverctl` would
// render their own sent DMs as "sent — ciphertext only". We persist the
// sender's plaintext in the chat delegate (same backing store as room
// secrets / signing keys) so reloads and second devices recover it.
// =============================================================================

/// Fire a GetRequest for the outbound-DM cache without awaiting the
/// response. Mirrors [`fire_load_rooms_request`] — the response is
/// processed in `freenet_api::response_handler` and hydrates the
/// in-memory [`OUTBOUND_DMS`] signal.
async fn fire_load_outbound_dms_request() {
    info!("Firing request to load outbound DMs from delegate storage");

    let request = ChatDelegateRequestMsg::GetRequest {
        key: ChatDelegateKey::new(OUTBOUND_DMS_STORAGE_KEY.to_vec()),
    };

    let mut payload = Vec::new();
    if let Err(e) = ciborium::ser::into_writer(&request, &mut payload) {
        error!("Failed to serialize load outbound-DMs request: {}", e);
        return;
    }

    let delegate_code =
        DelegateCode::from(include_bytes!("../../../public/contracts/chat_delegate.wasm").to_vec());
    let params = Parameters::from(Vec::<u8>::new());
    let delegate = Delegate::from((&delegate_code, &params));
    let delegate_key = delegate.key().clone();

    let app_msg = freenet_stdlib::prelude::ApplicationMessage::new(payload);
    let delegate_request = DelegateOp(DelegateRequest::ApplicationMessages {
        key: delegate_key,
        params: Parameters::from(Vec::<u8>::new()),
        inbound: vec![freenet_stdlib::prelude::InboundDelegateMsg::ApplicationMessage(app_msg)],
    });

    let api_result = {
        let mut web_api = WEB_API.write();
        if let Some(api) = web_api.as_mut() {
            api.send(delegate_request).await
        } else {
            Err(freenet_stdlib::client_api::Error::ConnectionClosed)
        }
    };

    match api_result {
        Ok(_) => info!("Outbound-DMs load request sent"),
        Err(e) => error!("Failed to send outbound-DMs load request: {}", e),
    }
}

/// Hydrate the [`OUTBOUND_DMS`] cache from a `Vec<OutboundDmEntry>`
/// loaded from a delegate. Per-pair caps are NOT re-applied here — we
/// trust the writer (us, on save). Returns the number of entries
/// loaded.
pub fn hydrate_outbound_dms_cache(entries: Vec<OutboundDmEntry>) -> usize {
    use crate::components::direct_messages::OUTBOUND_DMS;
    let count = entries.len();
    crate::util::defer(move || {
        OUTBOUND_DMS.with_mut(|cache| {
            for entry in entries {
                let room_vk = match ed25519_dalek::VerifyingKey::from_bytes(&entry.room_owner_vk) {
                    Ok(vk) => vk,
                    Err(e) => {
                        warn!("Skipping outbound-DM entry with invalid room VK: {}", e);
                        continue;
                    }
                };
                cache
                    .by_token
                    .insert((room_vk, entry.recipient, entry.purge_token), entry);
            }
        });
    });
    count
}

/// Serialize the current [`OUTBOUND_DMS`] cache and persist it via the
/// chat delegate. Caller is responsible for having already mutated the
/// cache before invoking this. Fire-and-forget at most call sites —
/// the StoreResponse comes back through the normal message loop and is
/// logged but not awaited on the hot path.
pub async fn save_outbound_dms_to_delegate() -> Result<(), String> {
    use crate::components::direct_messages::OUTBOUND_DMS;

    let store = {
        let cache = OUTBOUND_DMS.read();
        let mut entries: Vec<OutboundDmEntry> = cache.by_token.values().cloned().collect();
        // Stable order keeps the saved blob byte-identical across runs
        // when no entries changed, so a save that's a no-op on disk
        // doesn't churn the delegate's "modified" bookkeeping.
        entries.sort_by(|a, b| {
            a.room_owner_vk
                .cmp(&b.room_owner_vk)
                .then_with(|| a.recipient.cmp(&b.recipient))
                .then_with(|| a.purge_token.0.cmp(&b.purge_token.0))
        });
        OutboundDmStore { entries }
    };

    let mut buffer = Vec::new();
    ciborium::ser::into_writer(&store, &mut buffer)
        .map_err(|e| format!("Failed to serialize outbound DMs: {}", e))?;

    let request = ChatDelegateRequestMsg::StoreRequest {
        key: ChatDelegateKey::new(OUTBOUND_DMS_STORAGE_KEY.to_vec()),
        value: buffer,
    };

    match send_delegate_request(request).await {
        Ok(ChatDelegateResponseMsg::StoreResponse { result, .. }) => result,
        Ok(other) => Err(format!("Unexpected response: {:?}", other)),
        Err(e) => Err(e),
    }
}

/// Insert a freshly-sent outbound DM into the in-memory cache and
/// queue a delegate write. Enforces the same per-pair cap the contract
/// applies: if `(room, recipient)` already has
/// `MAX_DM_MESSAGES_PER_PAIR` entries cached, the oldest is evicted so
/// the cache stays bounded.
///
/// All signal mutation goes through [`crate::util::defer`] per the
/// WASM signal-safety rules.
pub fn save_outbound_dm(
    room_owner_vk: ed25519_dalek::VerifyingKey,
    sender: MemberId,
    recipient: MemberId,
    purge_token: PurgeToken,
    timestamp: u64,
    plaintext: String,
) {
    use crate::components::direct_messages::OUTBOUND_DMS;

    let entry = OutboundDmEntry {
        room_owner_vk: room_owner_vk.to_bytes(),
        sender,
        recipient,
        purge_token,
        timestamp,
        plaintext,
    };

    crate::util::defer(move || {
        OUTBOUND_DMS.with_mut(|cache| {
            cache.by_token.insert(
                (room_owner_vk, entry.recipient, entry.purge_token),
                entry.clone(),
            );

            // Per-pair cap eviction: drop the oldest entries for this
            // (room, sender, recipient) tuple until we are back under
            // the contract's cap.
            let room_bytes = room_owner_vk.to_bytes();
            let mut pair_entries: Vec<_> = cache
                .by_token
                .iter()
                .filter(|(_, e)| {
                    e.room_owner_vk == room_bytes
                        && e.sender == entry.sender
                        && e.recipient == entry.recipient
                })
                .map(|(k, e)| (*k, e.timestamp))
                .collect();
            if pair_entries.len() > MAX_DM_MESSAGES_PER_PAIR {
                pair_entries.sort_by_key(|(_, ts)| *ts);
                let drop_count = pair_entries.len() - MAX_DM_MESSAGES_PER_PAIR;
                for (key, _) in pair_entries.into_iter().take(drop_count) {
                    cache.by_token.remove(&key);
                }
            }
        });

        crate::util::safe_spawn_local(async {
            if let Err(e) = save_outbound_dms_to_delegate().await {
                warn!("Failed to persist outbound DM cache: {}", e);
            }
        });
    });
}

/// Drop cached outbound-DM entries whose ciphertext is no longer
/// present in any room's `direct_messages` (recipient has purged
/// them, or per-pair-cap eviction in the contract dropped them).
/// Called from a `use_effect` in `App()` that subscribes to ROOMS,
/// so this fires after every room-state mutation. Persists the
/// trimmed cache if anything was actually removed.
///
/// Critically, this function does NOT call `with_mut` on the cache
/// unless there is something to remove — otherwise the `App()`
/// effect that triggers it would re-fire endlessly on its own writes
/// even when steady-state.
pub fn prune_outbound_dms_for_purges() {
    use crate::components::direct_messages::OUTBOUND_DMS;

    // Build the "live" key set from current ROOMS state. `try_read`
    // so we cooperate with any in-flight write.
    let live_keys: std::collections::HashSet<(ed25519_dalek::VerifyingKey, MemberId, PurgeToken)> = {
        let Ok(rooms) = ROOMS.try_read() else {
            return;
        };
        let mut keys = std::collections::HashSet::new();
        for (owner_vk, room_data) in &rooms.map {
            for msg in &room_data.room_state.direct_messages.messages {
                keys.insert((*owner_vk, msg.message.recipient, msg.purge_token()));
            }
        }
        keys
    };

    // Compute the diff against the cache via a read-only borrow so we
    // can skip the entire defer/with_mut path when nothing changed.
    let to_remove: Vec<(ed25519_dalek::VerifyingKey, MemberId, PurgeToken)> = {
        let Ok(cache) = OUTBOUND_DMS.try_read() else {
            return;
        };
        cache
            .by_token
            .keys()
            .filter(|k| !live_keys.contains(k))
            .copied()
            .collect()
    };
    if to_remove.is_empty() {
        return;
    }

    let removed = to_remove.len();
    crate::util::defer(move || {
        OUTBOUND_DMS.with_mut(|cache| {
            for key in &to_remove {
                cache.by_token.remove(key);
            }
        });
        info!(
            "Pruned {} outbound-DM cache entries against purges",
            removed
        );
        crate::util::safe_spawn_local(async {
            if let Err(e) = save_outbound_dms_to_delegate().await {
                warn!("Failed to persist pruned outbound DM cache: {}", e);
            }
        });
    });
}

fn create_chat_delegate_container() -> DelegateContainer {
    let delegate_bytes = include_bytes!("../../../public/contracts/chat_delegate.wasm");
    let delegate_code = DelegateCode::from(delegate_bytes.to_vec());
    let params = Parameters::from(Vec::<u8>::new());
    let delegate = Delegate::from((&delegate_code, &params));
    DelegateContainer::Wasm(DelegateWasmAPIVersion::V1(delegate))
}

/// Extract the key from a request message for tracking purposes.
fn get_request_key(request: &ChatDelegateRequestMsg) -> Vec<u8> {
    match request {
        // Key-value storage operations
        ChatDelegateRequestMsg::StoreRequest { key, .. } => key.as_bytes().to_vec(),
        ChatDelegateRequestMsg::GetRequest { key } => key.as_bytes().to_vec(),
        ChatDelegateRequestMsg::DeleteRequest { key } => key.as_bytes().to_vec(),
        ChatDelegateRequestMsg::ListRequest => b"__list_request__".to_vec(),

        // Signing key management
        ChatDelegateRequestMsg::StoreSigningKey { room_key, .. } => {
            let mut key = SIGNING_KEY_PREFIX.to_vec();
            key.extend_from_slice(room_key);
            key
        }
        ChatDelegateRequestMsg::GetPublicKey { room_key } => {
            let mut key = PUBLIC_KEY_PREFIX.to_vec();
            key.extend_from_slice(room_key);
            key
        }

        // Signing operations - use prefix + room_key + request_id for uniqueness
        ChatDelegateRequestMsg::SignMessage {
            room_key,
            request_id,
            ..
        }
        | ChatDelegateRequestMsg::SignMember {
            room_key,
            request_id,
            ..
        }
        | ChatDelegateRequestMsg::SignBan {
            room_key,
            request_id,
            ..
        }
        | ChatDelegateRequestMsg::SignConfig {
            room_key,
            request_id,
            ..
        }
        | ChatDelegateRequestMsg::SignMemberInfo {
            room_key,
            request_id,
            ..
        }
        | ChatDelegateRequestMsg::SignSecretVersion {
            room_key,
            request_id,
            ..
        }
        | ChatDelegateRequestMsg::SignEncryptedSecret {
            room_key,
            request_id,
            ..
        }
        | ChatDelegateRequestMsg::SignUpgrade {
            room_key,
            request_id,
            ..
        } => {
            let mut key = SIGN_PREFIX.to_vec();
            key.extend_from_slice(room_key);
            key.extend_from_slice(&request_id.to_le_bytes());
            key
        }

        // EnsureRoomSubscription is fire-and-forget from the UI's point of
        // view — the response is logged but not awaited. We give it a stable
        // tracking key per-room anyway in case a future caller wants to
        // await the response via `send_delegate_request`.
        ChatDelegateRequestMsg::EnsureRoomSubscription { room_owner_vk, .. } => {
            let mut key = b"__room_subscription:".to_vec();
            key.extend_from_slice(room_owner_vk);
            key
        }
    }
}

pub async fn send_delegate_request(
    request: ChatDelegateRequestMsg,
) -> Result<ChatDelegateResponseMsg, String> {
    debug!("Sending delegate request: {:?}", request);

    // Get the key bytes for tracking this request
    let key_bytes = get_request_key(&request);

    // Create a oneshot channel to receive the response
    let (sender, receiver) = oneshot::channel();

    // Register the pending request
    {
        let mut pending = PENDING_REQUESTS
            .lock()
            .map_err(|e| format!("Failed to lock pending requests: {}", e))?;
        pending.insert(key_bytes.clone(), sender);
    }

    // Serialize the request
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&request, &mut payload)
        .map_err(|e| format!("Failed to serialize request: {}", e))?;

    info!("Serialized request payload size: {} bytes", payload.len());

    let delegate_code =
        DelegateCode::from(include_bytes!("../../../public/contracts/chat_delegate.wasm").to_vec());
    let params = Parameters::from(Vec::<u8>::new());
    let delegate = Delegate::from((&delegate_code, &params));
    let delegate_key = delegate.key().clone(); // Get the delegate key for targeting the delegate request

    let app_msg = freenet_stdlib::prelude::ApplicationMessage::new(payload);

    // Prepare the delegate request, targeting the delegate using its key
    let delegate_request = DelegateOp(DelegateRequest::ApplicationMessages {
        key: delegate_key, // Target the delegate instance
        params: Parameters::from(Vec::<u8>::new()),
        inbound: vec![freenet_stdlib::prelude::InboundDelegateMsg::ApplicationMessage(app_msg)],
    });

    // Get the API and send the request, releasing the lock before awaiting
    let api_result = {
        let mut web_api = WEB_API.write();
        if let Some(api) = web_api.as_mut() {
            // Send the request while holding the lock
            api.send(delegate_request).await
        } else {
            Err(freenet_stdlib::client_api::Error::ConnectionClosed)
        }
    };

    // Handle send errors - remove the pending request if send failed
    if let Err(e) = api_result {
        if let Ok(mut pending) = PENDING_REQUESTS.lock() {
            pending.remove(&key_bytes);
        }
        return Err(format!("Failed to send delegate request: {}", e));
    }

    info!("Request sent, waiting for response...");

    // Wait for the response with a timeout
    // Use WASM-compatible sleep from util module
    // Delegate runs locally on the same node, so responses should be near-instant.
    // However, first-time WASM compilation on slow mobile devices can take several
    // seconds. 10s balances responsiveness with device compatibility.
    let timeout = Box::pin(crate::util::sleep(std::time::Duration::from_secs(10)));

    match select(receiver, timeout).await {
        Either::Left((response, _)) => match response {
            Ok(resp) => {
                debug!("Received delegate response: {:?}", resp);
                Ok(resp)
            }
            Err(_) => Err("Response channel was cancelled".to_string()),
        },
        Either::Right((_, _)) => {
            // Timeout occurred - remove the pending request
            if let Ok(mut pending) = PENDING_REQUESTS.lock() {
                pending.remove(&key_bytes);
            }
            Err("Timeout waiting for delegate response".to_string())
        }
    }
}

// =============================================================================
// LEGACY DELEGATE MIGRATION IMPLEMENTATION
// =============================================================================

/// The action to take after the **current** delegate's `GetResponse` for
/// `rooms_data` has been observed. See `decide_legacy_migration_action`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LegacyMigrationAction {
    /// Current delegate has authoritative state (rooms or tombstones). Mark
    /// legacy migration done so it never fires for this user.
    MarkDone,
    /// Current delegate is empty (no record, or a placeholder serialization
    /// with no rooms and no tombstones). Fire the legacy probes — there is
    /// nothing in current to clobber.
    FireMigration,
}

/// Decide what to do with legacy migration based on the **current** delegate's
/// `GetResponse` for `rooms_data`.
///
/// This encodes the gating invariant from freenet/river#253:
/// - If the current delegate holds rooms or tombstones, it is the source of
///   truth and legacy migration must be blocked permanently — a stale legacy
///   snapshot would otherwise overwrite this newer state.
/// - If the current delegate is empty (no record, or just a placeholder
///   serialization with no rooms and no tombstones), legacy migration is
///   safe to fire because there is nothing to clobber.
///
/// `current_has_rooms` is `true` if the current delegate returned at least
/// one entry in `Rooms::map`. `current_has_tombstones` is `true` if at least
/// one entry exists in `Rooms::removed_rooms`. `value_present` is `true` if
/// the `GetResponse` carried `Some(_)` (regardless of whether the bytes
/// deserialize to a non-empty `Rooms`).
pub(crate) fn decide_legacy_migration_action(
    value_present: bool,
    current_has_rooms: bool,
    current_has_tombstones: bool,
) -> LegacyMigrationAction {
    if value_present && (current_has_rooms || current_has_tombstones) {
        LegacyMigrationAction::MarkDone
    } else {
        LegacyMigrationAction::FireMigration
    }
}

/// localStorage key to track whether legacy migration has been attempted
#[allow(dead_code)]
const LEGACY_MIGRATION_FLAG: &str = "river_legacy_migration_done";

/// Check if legacy migration has already been done (via localStorage)
fn is_legacy_migration_done() -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            if let Ok(Some(storage)) = window.local_storage() {
                return storage
                    .get_item(LEGACY_MIGRATION_FLAG)
                    .ok()
                    .flatten()
                    .is_some();
            }
        }
        false
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        // Non-WASM: assume migration not needed (testing environment)
        true
    }
}

/// Mark legacy migration as done in localStorage
pub fn mark_legacy_migration_done() {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            if let Ok(Some(storage)) = window.local_storage() {
                if let Err(e) = storage.set_item(LEGACY_MIGRATION_FLAG, "true") {
                    warn!("Failed to set legacy migration flag: {:?}", e);
                } else {
                    info!("Legacy migration marked as done");
                }
            }
        }
    }
}

/// Guard to prevent repeated legacy migration attempts within the same session.
/// The migration is fire-and-forget: if the legacy delegates don't exist on the node,
/// the node returns "delegate not found" errors that never reach the response handler's
/// success path, so `mark_legacy_migration_done()` is never called. Without this guard,
/// every WebSocket reconnect would re-fire all 3 legacy delegate requests, creating a
/// tight error spam loop (~9 errors every 3 seconds).
static LEGACY_MIGRATION_ATTEMPTED: AtomicBool = AtomicBool::new(false);

/// Fire requests to load rooms from all known legacy delegates (fire and forget).
/// If any legacy delegate has room data, the response handler will migrate it.
///
/// **Must only be called once the current delegate has confirmed it has no
/// rooms_data** (see freenet/river#253). Firing this while the current delegate
/// may have data is unsafe because a legacy response can trigger a save that
/// overwrites the current delegate's storage before its GET response arrives,
/// destroying rooms the user created after the last legacy snapshot.
pub(crate) async fn fire_legacy_migration_request() {
    // Check if migration has already been done (persistent across sessions)
    if is_legacy_migration_done() {
        info!("Legacy migration already done, skipping");
        return;
    }

    // Only attempt migration once per session to avoid error spam on reconnect.
    // If the legacy delegates aren't installed, retrying won't help.
    if LEGACY_MIGRATION_ATTEMPTED.swap(true, Ordering::Relaxed) {
        info!("Legacy migration already attempted this session, skipping");
        return;
    }

    info!(
        "Attempting to migrate data from {} legacy delegate(s)",
        LEGACY_DELEGATES.len()
    );

    for (i, (key_bytes, code_hash_bytes)) in LEGACY_DELEGATES.iter().enumerate() {
        let legacy_code_hash = CodeHash::new(*code_hash_bytes);
        let legacy_delegate_key = DelegateKey::new(*key_bytes, legacy_code_hash);

        // Send a GetRequest for each storage key we want to migrate.
        // rooms_data is the original and gates the migration; outbound_dms
        // (#256) is mirrored so a delegate rebuild doesn't orphan the
        // sender's own DM plaintext.
        let storage_keys: [&[u8]; 2] = [ROOMS_STORAGE_KEY, OUTBOUND_DMS_STORAGE_KEY];
        for storage_key in storage_keys {
            let request = ChatDelegateRequestMsg::GetRequest {
                key: ChatDelegateKey::new(storage_key.to_vec()),
            };

            let mut payload = Vec::new();
            if let Err(e) = ciborium::ser::into_writer(&request, &mut payload) {
                error!(
                    "Failed to serialize legacy migration request #{} for key {:?}: {}",
                    i,
                    String::from_utf8_lossy(storage_key),
                    e
                );
                continue;
            }

            let app_msg = freenet_stdlib::prelude::ApplicationMessage::new(payload);

            let delegate_request = DelegateOp(DelegateRequest::ApplicationMessages {
                key: legacy_delegate_key.clone(),
                params: Parameters::from(Vec::<u8>::new()),
                inbound: vec![
                    freenet_stdlib::prelude::InboundDelegateMsg::ApplicationMessage(app_msg),
                ],
            });

            let api_result = {
                let mut web_api = WEB_API.write();
                if let Some(api) = web_api.as_mut() {
                    api.send(delegate_request).await
                } else {
                    Err(freenet_stdlib::client_api::Error::ConnectionClosed)
                }
            };

            match api_result {
                Ok(_) => info!(
                    "Legacy migration request #{} sent for key {:?}",
                    i,
                    String::from_utf8_lossy(storage_key)
                ),
                Err(e) => {
                    info!(
                        "Could not send legacy migration request #{} for key {:?} \
                         (expected if delegate not present): {}",
                        i,
                        String::from_utf8_lossy(storage_key),
                        e
                    );
                }
            }
        }
    }
}
