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

/// Migrate a signing key to the delegate if not already present.
///
/// Returns a `MigrationResult` indicating what happened:
/// - `AlreadyCurrent`: key matched, no action needed
/// - `StaleKeyOverwritten`: old key was replaced (caller should sanitize local messages)
/// - `Stored`: key was stored for the first time
/// - `Failed`: migration failed (fallback to local signing should be used)
pub async fn migrate_signing_key(room_key: RoomKey, signing_key: &SigningKey) -> MigrationResult {
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

/// Sign message bytes with delegate, falling back to local signing if delegate fails.
pub async fn sign_message_with_fallback(
    room_key: RoomKey,
    message_bytes: Vec<u8>,
    fallback_key: &SigningKey,
) -> Signature {
    crate::util::debug_log("[sign] requesting delegate signature...");
    match sign_message(room_key, message_bytes.clone()).await {
        Ok(sig) => {
            crate::util::debug_log("[sign] delegate signed OK");
            sig
        }
        Err(e) => {
            crate::util::debug_log(&format!("[sign] delegate FAILED: {}, using fallback", e));
            warn!("Delegate signing failed, using fallback: {}", e);
            fallback_key.sign(&message_bytes)
        }
    }
}

/// Sign member bytes with delegate, falling back to local signing if delegate fails.
pub async fn sign_member_with_fallback(
    room_key: RoomKey,
    member_bytes: Vec<u8>,
    fallback_key: &SigningKey,
) -> Signature {
    match sign_member(room_key, member_bytes.clone()).await {
        Ok(sig) => sig,
        Err(e) => {
            warn!("Delegate signing failed, using fallback: {}", e);
            fallback_key.sign(&member_bytes)
        }
    }
}

/// Sign ban bytes with delegate, falling back to local signing if delegate fails.
pub async fn sign_ban_with_fallback(
    room_key: RoomKey,
    ban_bytes: Vec<u8>,
    fallback_key: &SigningKey,
) -> Signature {
    match sign_ban(room_key, ban_bytes.clone()).await {
        Ok(sig) => sig,
        Err(e) => {
            warn!("Delegate signing failed, using fallback: {}", e);
            fallback_key.sign(&ban_bytes)
        }
    }
}

/// Sign config bytes with delegate, falling back to local signing if delegate fails.
pub async fn sign_config_with_fallback(
    room_key: RoomKey,
    config_bytes: Vec<u8>,
    fallback_key: &SigningKey,
) -> Signature {
    match sign_config(room_key, config_bytes.clone()).await {
        Ok(sig) => sig,
        Err(e) => {
            warn!("Delegate signing failed, using fallback: {}", e);
            fallback_key.sign(&config_bytes)
        }
    }
}

/// Sign member info bytes with delegate, falling back to local signing if delegate fails.
pub async fn sign_member_info_with_fallback(
    room_key: RoomKey,
    member_info_bytes: Vec<u8>,
    fallback_key: &SigningKey,
) -> Signature {
    match sign_member_info(room_key, member_info_bytes.clone()).await {
        Ok(sig) => sig,
        Err(e) => {
            warn!("Delegate signing failed, using fallback: {}", e);
            fallback_key.sign(&member_info_bytes)
        }
    }
}

/// Sign secret version record bytes with delegate, falling back to local signing if delegate fails.
pub async fn sign_secret_version_with_fallback(
    room_key: RoomKey,
    record_bytes: Vec<u8>,
    fallback_key: &SigningKey,
) -> Signature {
    match sign_secret_version(room_key, record_bytes.clone()).await {
        Ok(sig) => sig,
        Err(e) => {
            warn!("Delegate signing failed, using fallback: {}", e);
            fallback_key.sign(&record_bytes)
        }
    }
}

/// Sign encrypted secret bytes with delegate, falling back to local signing if delegate fails.
pub async fn sign_encrypted_secret_with_fallback(
    room_key: RoomKey,
    secret_bytes: Vec<u8>,
    fallback_key: &SigningKey,
) -> Signature {
    match sign_encrypted_secret(room_key, secret_bytes.clone()).await {
        Ok(sig) => sig,
        Err(e) => {
            warn!("Delegate signing failed, using fallback: {}", e);
            fallback_key.sign(&secret_bytes)
        }
    }
}

/// Sign upgrade bytes with delegate, falling back to local signing if delegate fails.
pub async fn sign_upgrade_with_fallback(
    room_key: RoomKey,
    upgrade_bytes: Vec<u8>,
    fallback_key: &SigningKey,
) -> Signature {
    match sign_upgrade(room_key, upgrade_bytes.clone()).await {
        Ok(sig) => sig,
        Err(e) => {
            warn!("Delegate signing failed, using fallback: {}", e);
            fallback_key.sign(&upgrade_bytes)
        }
    }
}
