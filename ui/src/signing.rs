//! Signing API for delegate-based signing operations.
//!
//! This module provides async wrapper functions that send signing requests
//! to the chat delegate and wait for responses. The delegate holds the signing
//! keys and performs all signing operations, so private keys never leave the delegate.
//!
//! The module also provides fallback functionality that signs locally if the
//! delegate signing fails, for backwards compatibility during migration.

use crate::components::app::chat_delegate::{generate_request_id, send_delegate_request};
use dioxus::logger::tracing::{info, warn};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use river_core::chat_delegate::{ChatDelegateRequestMsg, ChatDelegateResponseMsg, RoomKey};
use river_core::room_state::ChatRoomParametersV1;
use river_core::ChatRoomStateV1;

/// Result of a signing key migration attempt.
#[derive(Debug, PartialEq)]
pub enum MigrationResult {
    /// Key already matched in delegate, no changes needed.
    AlreadyCurrent,
    /// Stale key was overwritten with current key.
    StaleKeyOverwritten,
    /// Key was stored for the first time.
    Stored,
    /// Migration failed.
    Failed,
}

/// Store a signing key in the delegate for a room.
///
/// This should be called when creating a new room or when migrating
/// an existing room's signing key to the delegate.
pub async fn store_signing_key(room_key: RoomKey, signing_key: &SigningKey) -> Result<(), String> {
    let request = ChatDelegateRequestMsg::StoreSigningKey {
        room_key,
        signing_key_bytes: signing_key.to_bytes(),
    };

    match send_delegate_request(request).await {
        Ok(ChatDelegateResponseMsg::StoreSigningKeyResponse { result, .. }) => result,
        Ok(other) => Err(format!("Unexpected response: {:?}", other)),
        Err(e) => Err(e),
    }
}

/// Get the public key for a room from the delegate.
///
/// Returns the VerifyingKey if the signing key exists, None otherwise.
pub async fn get_public_key(room_key: RoomKey) -> Result<Option<VerifyingKey>, String> {
    let request = ChatDelegateRequestMsg::GetPublicKey { room_key };

    match send_delegate_request(request).await {
        Ok(ChatDelegateResponseMsg::GetPublicKeyResponse { public_key, .. }) => {
            if let Some(pk_bytes) = public_key {
                let vk = VerifyingKey::from_bytes(&pk_bytes)
                    .map_err(|e| format!("Invalid public key: {}", e))?;
                Ok(Some(vk))
            } else {
                Ok(None)
            }
        }
        Ok(other) => Err(format!("Unexpected response: {:?}", other)),
        Err(e) => Err(e),
    }
}

/// Sign a message (MessageV1).
pub async fn sign_message(room_key: RoomKey, message_bytes: Vec<u8>) -> Result<Signature, String> {
    let request = ChatDelegateRequestMsg::SignMessage {
        room_key,
        request_id: generate_request_id(),
        message_bytes,
    };

    extract_signature(send_delegate_request(request).await)
}

/// Sign a member invitation (Member).
pub async fn sign_member(room_key: RoomKey, member_bytes: Vec<u8>) -> Result<Signature, String> {
    let request = ChatDelegateRequestMsg::SignMember {
        room_key,
        request_id: generate_request_id(),
        member_bytes,
    };

    extract_signature(send_delegate_request(request).await)
}

/// Sign a ban (BanV1).
pub async fn sign_ban(room_key: RoomKey, ban_bytes: Vec<u8>) -> Result<Signature, String> {
    let request = ChatDelegateRequestMsg::SignBan {
        room_key,
        request_id: generate_request_id(),
        ban_bytes,
    };

    extract_signature(send_delegate_request(request).await)
}

/// Sign a room configuration (Configuration).
pub async fn sign_config(room_key: RoomKey, config_bytes: Vec<u8>) -> Result<Signature, String> {
    let request = ChatDelegateRequestMsg::SignConfig {
        room_key,
        request_id: generate_request_id(),
        config_bytes,
    };

    extract_signature(send_delegate_request(request).await)
}

/// Sign member info (MemberInfo).
pub async fn sign_member_info(
    room_key: RoomKey,
    member_info_bytes: Vec<u8>,
) -> Result<Signature, String> {
    let request = ChatDelegateRequestMsg::SignMemberInfo {
        room_key,
        request_id: generate_request_id(),
        member_info_bytes,
    };

    extract_signature(send_delegate_request(request).await)
}

/// Sign a secret version record (SecretVersionRecordV1).
pub async fn sign_secret_version(
    room_key: RoomKey,
    record_bytes: Vec<u8>,
) -> Result<Signature, String> {
    let request = ChatDelegateRequestMsg::SignSecretVersion {
        room_key,
        request_id: generate_request_id(),
        record_bytes,
    };

    extract_signature(send_delegate_request(request).await)
}

/// Sign an encrypted secret for member (EncryptedSecretForMemberV1).
pub async fn sign_encrypted_secret(
    room_key: RoomKey,
    secret_bytes: Vec<u8>,
) -> Result<Signature, String> {
    let request = ChatDelegateRequestMsg::SignEncryptedSecret {
        room_key,
        request_id: generate_request_id(),
        secret_bytes,
    };

    extract_signature(send_delegate_request(request).await)
}

/// Sign a room upgrade (RoomUpgrade).
pub async fn sign_upgrade(room_key: RoomKey, upgrade_bytes: Vec<u8>) -> Result<Signature, String> {
    let request = ChatDelegateRequestMsg::SignUpgrade {
        room_key,
        request_id: generate_request_id(),
        upgrade_bytes,
    };

    extract_signature(send_delegate_request(request).await)
}

/// Extract a signature from a delegate response.
fn extract_signature(
    response: Result<ChatDelegateResponseMsg, String>,
) -> Result<Signature, String> {
    match response {
        Ok(ChatDelegateResponseMsg::SignResponse { signature, .. }) => match signature {
            Ok(sig_bytes) => {
                if sig_bytes.len() != 64 {
                    return Err(format!(
                        "Invalid signature length: {} bytes (expected 64)",
                        sig_bytes.len()
                    ));
                }
                let sig_array: [u8; 64] = sig_bytes
                    .try_into()
                    .map_err(|_| "Failed to convert signature bytes to array".to_string())?;
                Ok(Signature::from_bytes(&sig_array))
            }
            Err(e) => Err(e),
        },
        Ok(other) => Err(format!("Unexpected response: {:?}", other)),
        Err(e) => Err(e),
    }
}

// ============================================================================
// Migration and Fallback Functions
// ============================================================================

/// Per-room async locks serializing [`migrate_signing_key`].
///
/// The migration's get/store/get sequence is NOT atomic. Multiple migrations
/// for the SAME room can run concurrently — startup hydration, the post-GET
/// migration, and a rapid identity replacement (freenet/river#414) — and
/// without serialization a second migration could `store` a different key
/// BETWEEN this task's `store` and its verifying `get`, so the task verifies
/// the wrong key and a completion callback marks the room migrated against a
/// delegate that now holds a different identity. Holding a per-room async lock
/// across the whole sequence makes it atomic w.r.t. other migrations of the
/// same room. Placed inside `migrate_signing_key` so EVERY call site (import,
/// hydration, post-GET) is covered, not just the overwrite path.
///
/// `std::sync::Mutex` guards only the brief map lookup (never held across an
/// `.await`); the per-room `futures::lock::Mutex` is the async lock held across
/// the sequence. Single-threaded WASM, so the std mutex never contends.
static MIGRATION_LOCKS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<RoomKey, std::sync::Arc<futures::lock::Mutex<()>>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn migration_lock_for(room_key: &RoomKey) -> std::sync::Arc<futures::lock::Mutex<()>> {
    let mut locks = MIGRATION_LOCKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    locks
        .entry(*room_key)
        .or_insert_with(|| std::sync::Arc::new(futures::lock::Mutex::new(())))
        .clone()
}

/// Whether a migration of `migrating_key` is STALE and must be skipped.
///
/// A migration is stale when the room is still tracked locally but its CURRENT
/// identity (`current_self_sk`) is a DIFFERENT key than the one being migrated.
/// This is the delayed-old-key case (freenet/river#414): a migration queued
/// from a pre-overwrite `LoadRooms` (whose room `Rooms::merge` then skipped)
/// can acquire the per-room lock AFTER the new identity's migration and store
/// the OLD key, clobbering the delegate. The per-room lock only serializes; it
/// does not stop a late old-key migration from winning, so we reject it here.
///
/// `None` (room not tracked) is NOT stale: a brand-new import can legitimately
/// run its migration before/without a `ROOMS` entry, and there is no newer
/// identity to conflict with.
fn migration_is_stale(current_self_sk: Option<&SigningKey>, migrating_key: &SigningKey) -> bool {
    matches!(current_self_sk, Some(current) if current != migrating_key)
}

/// The room's CURRENT local signing identity, if the room is tracked in
/// `ROOMS`. Read via `try_read()` per the signal-safety rules; `None` when the
/// room isn't tracked, the key bytes are invalid, or `ROOMS` is mid-write.
/// Used by [`migrate_signing_key`] to reject a stale migration (#414).
fn current_room_self_sk(room_key: &RoomKey) -> Option<SigningKey> {
    use dioxus::prelude::ReadableExt;
    let owner_vk = VerifyingKey::from_bytes(room_key).ok()?;
    let rooms = crate::components::app::ROOMS.try_read().ok()?;
    rooms.map.get(&owner_vk).map(|rd| rd.self_sk.clone())
}

/// Migrate a signing key to the delegate if not already present.
///
/// Returns a `MigrationResult` indicating what happened:
/// - `AlreadyCurrent`: key matched, no action needed
/// - `StaleKeyOverwritten`: old key was replaced (caller should sanitize local messages)
/// - `Stored`: key was stored for the first time
/// - `Failed`: migration failed (fallback to local signing should be used)
pub async fn migrate_signing_key(room_key: RoomKey, signing_key: &SigningKey) -> MigrationResult {
    // Serialize concurrent migrations for THIS room so the non-atomic
    // get/store/get below runs to completion before another migration for the
    // same room can start (freenet/river#414).
    let room_lock = migration_lock_for(&room_key);
    let _migration_guard = room_lock.lock().await;

    // Check if key already exists in delegate
    let was_stale = match get_public_key(room_key).await {
        Ok(Some(existing_vk)) => {
            // Verify it matches our key
            if existing_vk == signing_key.verifying_key() {
                info!("Signing key already migrated to delegate for room");
                return MigrationResult::AlreadyCurrent;
            } else {
                // Delegate has a stale key (e.g. from before re-invitation).
                // Overwrite it so delegate signing produces valid signatures.
                warn!("Delegate has stale key for room - overwriting with current key");
                true
            }
        }
        Ok(None) => {
            // Key not in delegate, try to store it
            info!("Migrating signing key to delegate for room");
            false
        }
        Err(e) => {
            warn!(
                "Failed to check delegate for existing key: {} - will try to store",
                e
            );
            false
        }
    };

    // Reject a STALE migration before storing (freenet/river#414): if the
    // room's current local identity is a DIFFERENT key than the one this call
    // is migrating, a newer overwrite already replaced it since this migration
    // was queued. Storing the old key here would clobber the delegate with the
    // wrong identity — the per-room lock serializes migrations but does not stop
    // a late old-key migration from winning. Skip it.
    if migration_is_stale(current_room_self_sk(&room_key).as_ref(), signing_key) {
        warn!(
            "Skipping stale signing-key migration — the room's identity changed \
             since this migration was queued (freenet/river#414)"
        );
        return MigrationResult::Failed;
    }

    // Store the key
    match store_signing_key(room_key, signing_key).await {
        Ok(()) => {
            // Verify the key was stored correctly
            match get_public_key(room_key).await {
                Ok(Some(stored_vk)) if stored_vk == signing_key.verifying_key() => {
                    info!("Successfully migrated signing key to delegate");
                    if was_stale {
                        MigrationResult::StaleKeyOverwritten
                    } else {
                        MigrationResult::Stored
                    }
                }
                Ok(Some(_)) => {
                    warn!("Stored key doesn't match - using local signing");
                    MigrationResult::Failed
                }
                Ok(None) => {
                    warn!("Key not found after storing - using local signing");
                    MigrationResult::Failed
                }
                Err(e) => {
                    warn!("Failed to verify stored key: {} - using local signing", e);
                    MigrationResult::Failed
                }
            }
        }
        Err(e) => {
            warn!(
                "Failed to store signing key in delegate: {} - using local signing",
                e
            );
            MigrationResult::Failed
        }
    }
}

/// Remove messages with invalid signatures from local room state.
///
/// This should be called after overwriting a stale delegate signing key,
/// to purge any messages that were signed with the old (wrong) key.
/// Without this, the invalid messages block all UPDATEs to the contract
/// because the contract verifies all message signatures.
pub fn remove_unverifiable_messages(
    state: &mut ChatRoomStateV1,
    parameters: &ChatRoomParametersV1,
) -> usize {
    let owner_id = parameters.owner_id();
    let members_by_id = state.members.members_by_member_id();
    let before = state.recent_messages.messages.len();

    state.recent_messages.messages.retain(|message| {
        let verifying_key = if message.message.author == owner_id {
            &parameters.owner
        } else if let Some(member) = members_by_id.get(&message.message.author) {
            &member.member.member_vk
        } else {
            // Author not in members list — remove
            return false;
        };
        message.validate(verifying_key).is_ok()
    });

    let removed = before - state.recent_messages.messages.len();
    if removed > 0 {
        warn!(
            "Removed {} message(s) with invalid signatures from local state",
            removed
        );
    }
    removed
}

/// Try delegate signing, verify the signature matches our expected key, fall back to local
/// signing if the delegate fails or returns a signature from a stale key.
///
/// This prevents a class of bugs where the delegate holds an old signing key (e.g., before
/// an identity import migration completes) and produces a valid signature that the contract
/// rejects because it doesn't match the member's current verifying key.
async fn delegate_sign_or_fallback(
    delegate_sign: impl std::future::Future<Output = Result<Signature, String>>,
    data: &[u8],
    fallback_key: &SigningKey,
) -> Signature {
    match delegate_sign.await {
        Ok(sig) => {
            // Verify the delegate signed with OUR key, not a stale one
            if fallback_key
                .verifying_key()
                .verify_strict(data, &sig)
                .is_ok()
            {
                sig
            } else {
                warn!(
                    "Delegate returned signature from wrong key (stale delegate?), using local key"
                );
                fallback_key.sign(data)
            }
        }
        Err(e) => {
            warn!("Delegate signing failed, using fallback: {}", e);
            fallback_key.sign(data)
        }
    }
}

/// Sign message bytes with delegate, falling back to local signing if delegate fails
/// or has a stale key.
pub async fn sign_message_with_fallback(
    room_key: RoomKey,
    message_bytes: Vec<u8>,
    fallback_key: &SigningKey,
) -> Signature {
    crate::util::debug_log("[sign] requesting delegate signature...");
    let sig = delegate_sign_or_fallback(
        sign_message(room_key, message_bytes.clone()),
        &message_bytes,
        fallback_key,
    )
    .await;
    crate::util::debug_log("[sign] signed OK");
    sig
}

/// Sign member bytes with delegate, falling back to local signing if delegate fails
/// or has a stale key.
pub async fn sign_member_with_fallback(
    room_key: RoomKey,
    member_bytes: Vec<u8>,
    fallback_key: &SigningKey,
) -> Signature {
    delegate_sign_or_fallback(
        sign_member(room_key, member_bytes.clone()),
        &member_bytes,
        fallback_key,
    )
    .await
}

/// Sign ban bytes with delegate, falling back to local signing if delegate fails
/// or has a stale key.
pub async fn sign_ban_with_fallback(
    room_key: RoomKey,
    ban_bytes: Vec<u8>,
    fallback_key: &SigningKey,
) -> Signature {
    delegate_sign_or_fallback(
        sign_ban(room_key, ban_bytes.clone()),
        &ban_bytes,
        fallback_key,
    )
    .await
}

/// Sign config bytes with delegate, falling back to local signing if delegate fails
/// or has a stale key.
pub async fn sign_config_with_fallback(
    room_key: RoomKey,
    config_bytes: Vec<u8>,
    fallback_key: &SigningKey,
) -> Signature {
    delegate_sign_or_fallback(
        sign_config(room_key, config_bytes.clone()),
        &config_bytes,
        fallback_key,
    )
    .await
}

/// Sign member info bytes with delegate, falling back to local signing if delegate fails
/// or has a stale key.
pub async fn sign_member_info_with_fallback(
    room_key: RoomKey,
    member_info_bytes: Vec<u8>,
    fallback_key: &SigningKey,
) -> Signature {
    delegate_sign_or_fallback(
        sign_member_info(room_key, member_info_bytes.clone()),
        &member_info_bytes,
        fallback_key,
    )
    .await
}

/// Sign secret version record bytes with delegate, falling back to local signing if delegate fails
/// or has a stale key.
pub async fn sign_secret_version_with_fallback(
    room_key: RoomKey,
    record_bytes: Vec<u8>,
    fallback_key: &SigningKey,
) -> Signature {
    delegate_sign_or_fallback(
        sign_secret_version(room_key, record_bytes.clone()),
        &record_bytes,
        fallback_key,
    )
    .await
}

/// Sign encrypted secret bytes with delegate, falling back to local signing if delegate fails
/// or has a stale key.
pub async fn sign_encrypted_secret_with_fallback(
    room_key: RoomKey,
    secret_bytes: Vec<u8>,
    fallback_key: &SigningKey,
) -> Signature {
    delegate_sign_or_fallback(
        sign_encrypted_secret(room_key, secret_bytes.clone()),
        &secret_bytes,
        fallback_key,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Signer;
    use river_core::room_state::member::{AuthorizedMember, Member, MemberId};
    use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};

    /// freenet/river#414 (Codex round 4): `migrate_signing_key`'s non-atomic
    /// get/store/get must be serialized PER ROOM so a concurrent migration for
    /// the same room can't store a different key between this task's store and
    /// its verify. Pin the lock is (a) per-room and (b) actually mutually
    /// exclusive for the same room while leaving other rooms free.
    #[test]
    fn migration_lock_is_per_room_and_serializes_same_room() {
        let room_a: RoomKey = [1u8; 32];
        let room_b: RoomKey = [2u8; 32];

        // Same room → one shared lock; different rooms → distinct locks.
        let a1 = migration_lock_for(&room_a);
        let a2 = migration_lock_for(&room_a);
        let b1 = migration_lock_for(&room_b);
        assert!(
            std::sync::Arc::ptr_eq(&a1, &a2),
            "the same room must share one migration lock"
        );
        assert!(
            !std::sync::Arc::ptr_eq(&a1, &b1),
            "different rooms must have distinct migration locks"
        );

        // Holding room A's lock blocks a second acquire for room A (the
        // get/store/get sequence is serialized) but NOT for room B.
        let held = a1
            .try_lock()
            .expect("first acquire for room A must succeed");
        assert!(
            a2.try_lock().is_none(),
            "a concurrent migration for the SAME room must wait for the lock"
        );
        assert!(
            b1.try_lock().is_some(),
            "a migration for a DIFFERENT room must not be blocked"
        );
        drop(held);
        assert!(
            a2.try_lock().is_some(),
            "releasing the lock must let the next same-room migration proceed"
        );
    }

    /// freenet/river#414 (Codex round 5): serializing migrations is not enough —
    /// a delayed OLD-key migration can still acquire the lock after the new one.
    /// `migrate_signing_key` rejects a migration whose key no longer matches the
    /// room's CURRENT identity. Pin that decision.
    #[test]
    fn migration_is_stale_rejects_superseded_key() {
        let new_key = SigningKey::from_bytes(&[9u8; 32]);
        let old_key = SigningKey::from_bytes(&[8u8; 32]);
        assert_ne!(new_key.to_bytes(), old_key.to_bytes());

        // Room now holds `new_key`: a migration for the OLD key is stale.
        assert!(
            migration_is_stale(Some(&new_key), &old_key),
            "an old key must be rejected once the room's identity has moved on"
        );
        // Migrating the CURRENT key is fine.
        assert!(
            !migration_is_stale(Some(&new_key), &new_key),
            "migrating the room's current identity is never stale"
        );
        // Untracked room (brand-new import): never stale.
        assert!(
            !migration_is_stale(None, &new_key),
            "a not-yet-tracked room has no newer identity to conflict with"
        );
    }

    /// Source-grep pin (freenet/river#414, Codex round 5): `migrate_signing_key`
    /// must actually consult `migration_is_stale` (against the room's current
    /// identity) and abort before storing — the pure-helper test above does not
    /// prove the reject is wired into the store path.
    #[test]
    fn migrate_signing_key_wires_staleness_reject() {
        let src = include_str!("signing.rs");
        let marker = "#[cfg(test)]";
        let prod = &src[..src.find(marker).expect("signing.rs has a test module")];
        assert!(
            prod.contains("if migration_is_stale(current_room_self_sk(&room_key).as_ref()"),
            "migrate_signing_key must reject a stale migration before storing (#414)"
        );
    }

    fn make_signed_message(author_sk: &SigningKey, owner_vk: &VerifyingKey) -> AuthorizedMessageV1 {
        let msg = MessageV1 {
            room_owner: MemberId::from(owner_vk),
            author: MemberId::from(&author_sk.verifying_key()),
            content: RoomMessageBody::public("test".to_string()),
            time: std::time::SystemTime::UNIX_EPOCH,
        };
        let mut msg_bytes = Vec::new();
        ciborium::ser::into_writer(&msg, &mut msg_bytes).unwrap();
        let signature = author_sk.sign(&msg_bytes);
        AuthorizedMessageV1::with_signature(msg, signature)
    }

    #[test]
    fn test_remove_unverifiable_messages() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let owner_vk = owner_sk.verifying_key();
        let member_sk = SigningKey::generate(&mut rng);
        let wrong_sk = SigningKey::generate(&mut rng);

        let params = ChatRoomParametersV1 { owner: owner_vk };

        // Add member to the room
        let member = Member {
            owner_member_id: owner_vk.into(),
            invited_by: owner_vk.into(),
            member_vk: member_sk.verifying_key(),
        };
        let auth_member = AuthorizedMember::new(member, &owner_sk);

        let mut state = ChatRoomStateV1::default();
        state.members.members.push(auth_member);

        // Valid message from owner
        let owner_msg = make_signed_message(&owner_sk, &owner_vk);
        // Valid message from member
        let member_msg = make_signed_message(&member_sk, &owner_vk);
        // Message signed with wrong key (stale delegate key scenario)
        let mut bad_msg = make_signed_message(&member_sk, &owner_vk);
        let wrong_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&bad_msg.message, &mut buf).unwrap();
            buf
        };
        bad_msg.signature = wrong_sk.sign(&wrong_bytes);

        state
            .recent_messages
            .messages
            .extend([owner_msg, member_msg, bad_msg]);

        assert_eq!(state.recent_messages.messages.len(), 3);

        let removed = remove_unverifiable_messages(&mut state, &params);
        assert_eq!(removed, 1);
        assert_eq!(state.recent_messages.messages.len(), 2);
    }

    #[test]
    fn test_remove_unverifiable_messages_unknown_author() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let owner_vk = owner_sk.verifying_key();
        let unknown_sk = SigningKey::generate(&mut rng);

        let params = ChatRoomParametersV1 { owner: owner_vk };
        let mut state = ChatRoomStateV1::default();

        // Message from unknown author (not in members list)
        let unknown_msg = make_signed_message(&unknown_sk, &owner_vk);
        state.recent_messages.messages.push(unknown_msg);

        let removed = remove_unverifiable_messages(&mut state, &params);
        assert_eq!(removed, 1);
        assert_eq!(state.recent_messages.messages.len(), 0);
    }

    #[test]
    fn test_remove_unverifiable_messages_empty() {
        let owner_sk = SigningKey::generate(&mut rand::thread_rng());
        let params = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };
        let mut state = ChatRoomStateV1::default();

        let removed = remove_unverifiable_messages(&mut state, &params);
        assert_eq!(removed, 0);
    }
}

/// Sign upgrade bytes with delegate, falling back to local signing if delegate fails
/// or has a stale key.
pub async fn sign_upgrade_with_fallback(
    room_key: RoomKey,
    upgrade_bytes: Vec<u8>,
    fallback_key: &SigningKey,
) -> Signature {
    delegate_sign_or_fallback(
        sign_upgrade(room_key, upgrade_bytes.clone()),
        &upgrade_bytes,
        fallback_key,
    )
    .await
}
