use super::*;
use crate::versioning::{apply_cas_store, apply_unconditional_store, read_versioned, CasOutcome};
use ed25519_dalek::{Signer, SigningKey};
use freenet_stdlib::prelude::DelegateCtx;
use river_core::chat_delegate::{CasStoreResult, RequestId, RoomKey};

/// The secret-store operations the key-value handlers use. In the delegate
/// this is the host `DelegateCtx`; natively that context has no storage at all
/// (`get_secret` is always `None`, `set_secret` always `false`), so the
/// handlers' write ORDER and their refusal of an undecodable index
/// (freenet/river#592) could not be tested. A fake store can.
pub(crate) trait SecretStore {
    fn get_secret(&self, key: &[u8]) -> Option<Vec<u8>>;
    fn set_secret(&mut self, key: &[u8], value: &[u8]) -> bool;
    fn remove_secret(&mut self, key: &[u8]);
}

impl SecretStore for DelegateCtx {
    fn get_secret(&self, key: &[u8]) -> Option<Vec<u8>> {
        DelegateCtx::get_secret(self, key)
    }
    /// The host's answer in the delegate. Natively the context stores nothing
    /// and always answers `false`; the handlers' native tests (which never
    /// read back) then behave as before the write checks became uniform.
    fn set_secret(&mut self, key: &[u8], value: &[u8]) -> bool {
        #[cfg(target_family = "wasm")]
        {
            DelegateCtx::set_secret(self, key, value)
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = DelegateCtx::set_secret(self, key, value);
            true
        }
    }
    fn remove_secret(&mut self, key: &[u8]) {
        let _ = DelegateCtx::remove_secret(self, key);
    }
}

/// Handle an application message using the host function API for direct secret access.
pub(crate) fn handle_application_message(
    ctx: &mut DelegateCtx,
    app_msg: ApplicationMessage,
    origin: &Origin,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // Deserialize the request message
    let request: ChatDelegateRequestMsg = ciborium::from_reader(app_msg.payload.as_slice())
        .map_err(|e| DelegateError::Deser(format!("Failed to deserialize request: {e}")))?;

    match request {
        // Key-value storage operations
        ChatDelegateRequestMsg::StoreRequest { key, value } => {
            logging::info(
                format!(
                    "Delegate received StoreRequest key: {key:?}, value_len: {}",
                    value.len()
                )
                .as_str(),
            );
            handle_store_request(ctx, origin, key, value)
        }
        ChatDelegateRequestMsg::GetRequest { key } => {
            logging::info(format!("Delegate received GetRequest key: {key:?}").as_str());
            handle_get_request(ctx, origin, key)
        }
        ChatDelegateRequestMsg::DeleteRequest { key } => {
            logging::info(format!("Delegate received DeleteRequest key: {key:?}").as_str());
            handle_delete_request(ctx, origin, key)
        }
        ChatDelegateRequestMsg::ListRequest => {
            logging::info("Delegate received ListRequest");
            handle_list_request(ctx, origin)
        }
        ChatDelegateRequestMsg::GetVersionedRequest { key } => {
            logging::info(format!("Delegate received GetVersionedRequest key: {key:?}").as_str());
            handle_get_versioned_request(ctx, origin, key)
        }
        ChatDelegateRequestMsg::CasStoreRequest {
            key,
            value,
            expected_generation,
        } => {
            logging::info(
                format!(
                    "Delegate received CasStoreRequest key: {key:?}, value_len: {}, expected_generation: {expected_generation}",
                    value.len()
                )
                .as_str(),
            );
            handle_cas_store_request(ctx, origin, key, value, expected_generation)
        }

        // Signing key management
        ChatDelegateRequestMsg::StoreSigningKey {
            room_key,
            signing_key_bytes,
        } => {
            logging::info(
                format!("Delegate received StoreSigningKey for room: {room_key:?}").as_str(),
            );
            handle_store_signing_key(ctx, origin, room_key, signing_key_bytes)
        }
        ChatDelegateRequestMsg::GetPublicKey { room_key } => {
            logging::info(
                format!("Delegate received GetPublicKey for room: {room_key:?}").as_str(),
            );
            handle_get_public_key(ctx, origin, room_key)
        }

        // Signing operations - all include request_id for correlation
        ChatDelegateRequestMsg::SignMessage {
            room_key,
            request_id,
            message_bytes,
        } => {
            logging::info(format!("Delegate received SignMessage for room: {room_key:?}").as_str());
            handle_sign_request(ctx, origin, room_key, request_id, message_bytes)
        }
        ChatDelegateRequestMsg::SignMember {
            room_key,
            request_id,
            member_bytes,
        } => {
            logging::info(format!("Delegate received SignMember for room: {room_key:?}").as_str());
            handle_sign_request(ctx, origin, room_key, request_id, member_bytes)
        }
        ChatDelegateRequestMsg::SignBan {
            room_key,
            request_id,
            ban_bytes,
        } => {
            logging::info(format!("Delegate received SignBan for room: {room_key:?}").as_str());
            handle_sign_request(ctx, origin, room_key, request_id, ban_bytes)
        }
        ChatDelegateRequestMsg::SignConfig {
            room_key,
            request_id,
            config_bytes,
        } => {
            logging::info(format!("Delegate received SignConfig for room: {room_key:?}").as_str());
            handle_sign_request(ctx, origin, room_key, request_id, config_bytes)
        }
        ChatDelegateRequestMsg::SignMemberInfo {
            room_key,
            request_id,
            member_info_bytes,
        } => {
            logging::info(
                format!("Delegate received SignMemberInfo for room: {room_key:?}").as_str(),
            );
            handle_sign_request(ctx, origin, room_key, request_id, member_info_bytes)
        }
        ChatDelegateRequestMsg::SignSecretVersion {
            room_key,
            request_id,
            record_bytes,
        } => {
            logging::info(
                format!("Delegate received SignSecretVersion for room: {room_key:?}").as_str(),
            );
            handle_sign_request(ctx, origin, room_key, request_id, record_bytes)
        }
        ChatDelegateRequestMsg::SignEncryptedSecret {
            room_key,
            request_id,
            secret_bytes,
        } => {
            logging::info(
                format!("Delegate received SignEncryptedSecret for room: {room_key:?}").as_str(),
            );
            handle_sign_request(ctx, origin, room_key, request_id, secret_bytes)
        }
        ChatDelegateRequestMsg::SignUpgrade {
            room_key,
            request_id,
            upgrade_bytes,
        } => {
            logging::info(format!("Delegate received SignUpgrade for room: {room_key:?}").as_str());
            handle_sign_request(ctx, origin, room_key, request_id, upgrade_bytes)
        }

        ChatDelegateRequestMsg::EnsureRoomSubscription {
            room_owner_vk,
            request_id,
            contract_id,
        } => {
            logging::info(
                format!("Delegate received EnsureRoomSubscription for room: {room_owner_vk:?}")
                    .as_str(),
            );
            crate::subscription::handle_ensure_room_subscription(
                ctx,
                &origin.to_b58(),
                room_owner_vk,
                request_id,
                contract_id,
            )
        }
    }
}

// ============================================================================
// Key-Value Storage Handlers
// ============================================================================

// ============================================================================
// Crash-safe key-value writes (freenet/river#592, #768 review)
// ============================================================================
//
// A store of a NEW key, and a delete of a listed key, each take two writes: the
// value and the key index. A crash (a node restart, a killed process) between
// them must never wedge the store or lose a value. So such an operation first
// records its intent under `<origin>:::kv_intent`, then writes, then clears the
// intent; and every key-value operation first recovers a recorded intent
// ([`open_kv`]): an interrupted add indexes the key if its value landed, an
// interrupted delete finishes. Recovery is idempotent, so a crash during it is
// recovered the next time. Writes to an already-listed key are one write and
// need no intent.
//
// The index itself is created (empty) before the first value is ever written,
// so a stored key can never exist without an index: an index that reads back
// absent beside a stored key is therefore a read fault (freenet-core#5857),
// and is refused rather than rewritten short ([`get_key_index`]).

/// Suffix of the per-origin intent record (see the section docs).
const KV_INTENT_SUFFIX: &str = "::kv_intent";

/// A multi-write key-value operation in flight (see the section docs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum KvIntent {
    /// Store a key the index does not list yet.
    Add(ChatDelegateKey),
    /// Delete a key the index lists.
    Delete(ChatDelegateKey),
}

fn create_intent_key(origin: &Origin) -> Vec<u8> {
    format!(
        "{}{}{}",
        origin.to_b58(),
        ORIGIN_KEY_SEPARATOR,
        KV_INTENT_SUFFIX
    )
    .into_bytes()
}

/// Write `value` under `key`, or fail.
fn put_secret(
    ctx: &mut impl SecretStore,
    key: &[u8],
    value: &[u8],
    what: &str,
) -> Result<(), DelegateError> {
    if ctx.set_secret(key, value) {
        Ok(())
    } else {
        Err(DelegateError::Other(format!(
            "Failed to store {what} via host function"
        )))
    }
}

/// Remove `key` and confirm it is gone. The host's `remove_secret` answers
/// `false` both for "did not exist" and for a failed removal, so the value is
/// read back instead.
fn remove_value(ctx: &mut impl SecretStore, key: &[u8]) -> Result<(), DelegateError> {
    ctx.remove_secret(key);
    if ctx.get_secret(key).is_some() {
        return Err(DelegateError::Other(
            "Failed to remove a value via host function".into(),
        ));
    }
    Ok(())
}

fn record_intent(
    ctx: &mut impl SecretStore,
    origin: &Origin,
    intent: &KvIntent,
) -> Result<(), DelegateError> {
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(intent, &mut bytes)
        .map_err(|e| DelegateError::Deser(format!("Failed to serialize intent: {e}")))?;
    put_secret(
        ctx,
        &create_intent_key(origin),
        &bytes,
        "the key-value intent",
    )
}

/// Open the key-value store for a write: read (or first create) the key
/// index, and finish any operation a crash interrupted. Returns the index.
fn open_kv(ctx: &mut impl SecretStore, origin: &Origin) -> Result<KeyIndex, DelegateError> {
    open_kv_with(ctx, origin, CreateIndex::Yes)
}

/// Whether opening the store may create a missing (empty) index.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CreateIndex {
    /// A write path: the index must exist before any value is written.
    Yes,
    /// The read path (`ListRequest`): a fresh delegate answers an empty list
    /// and writes nothing (freenet/river#768 round 3, S4). Crash recovery
    /// still runs, so a listing includes every value that landed.
    No,
}

fn open_kv_with(
    ctx: &mut impl SecretStore,
    origin: &Origin,
    create: CreateIndex,
) -> Result<KeyIndex, DelegateError> {
    let index_key = create_index_key(origin);
    let mut index = get_key_index(ctx, origin, &index_key, create)?;
    let intent_key = create_intent_key(origin);
    if let Some(bytes) = ctx.get_secret(&intent_key) {
        match ciborium::from_reader::<KvIntent, _>(bytes.as_slice()) {
            Ok(KvIntent::Add(key)) => {
                // Index the key if its value landed; if it did not, the store
                // was never acknowledged and there is nothing to keep.
                let landed = ctx.get_secret(&create_origin_key(origin, &key)).is_some();
                if landed && !index.keys.contains(&key) {
                    index.keys.push(key);
                    set_key_index(ctx, &index_key, &index)?;
                }
            }
            Ok(KvIntent::Delete(key)) => {
                if index.keys.contains(&key) {
                    index.keys.retain(|k| k != &key);
                    set_key_index(ctx, &index_key, &index)?;
                }
                // Keep the intent until the value is gone, so the next
                // operation retries the removal. A listing goes ahead (the
                // index no longer lists the key); a write does not, because
                // its own intent record would replace this one and strand the
                // value unlisted.
                if let Err(e) = remove_value(ctx, &create_origin_key(origin, &key)) {
                    logging::info("A deleted value could not be removed yet; retrying later");
                    return match create {
                        CreateIndex::No => Ok(index),
                        CreateIndex::Yes => Err(e),
                    };
                }
            }
            Err(e) => logging::info(&format!(
                "Dropping an undecodable key-value intent record: {e}"
            )),
        }
        ctx.remove_secret(&intent_key);
    }
    Ok(index)
}

/// Write `enveloped` under `key`: one write when the index already lists the
/// key, otherwise intent, value, index, clear (see the section docs).
fn write_value(
    ctx: &mut impl SecretStore,
    origin: &Origin,
    index: &mut KeyIndex,
    key: &ChatDelegateKey,
    enveloped: &[u8],
) -> Result<(), DelegateError> {
    let secret_key = create_origin_key(origin, key);
    if index.keys.contains(key) {
        return put_secret(ctx, &secret_key, enveloped, "secret");
    }
    record_intent(ctx, origin, &KvIntent::Add(key.clone()))?;
    put_secret(ctx, &secret_key, enveloped, "secret")?;
    index.keys.push(key.clone());
    set_key_index(ctx, &create_index_key(origin), index)?;
    ctx.remove_secret(&create_intent_key(origin));
    logging::info(&format!(
        "Added key to index, now has {} keys",
        index.keys.len()
    ));
    Ok(())
}

/// Handle a store request - stores value and updates the index
fn handle_store_request(
    ctx: &mut impl SecretStore,
    origin: &Origin,
    key: ChatDelegateKey,
    value: Vec<u8>,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // Open the store FIRST: an index that exists but does not decode, or reads
    // back absent beside a stored key, fails the request before anything is
    // written (freenet/river#592), and an interrupted earlier write is finished.
    let mut key_index = open_kv(ctx, origin)?;

    // Wrap the value in a versioned envelope, bumping the generation, so
    // that plain (non-CAS) writers stay consistent with CAS readers and
    // never leave an un-versioned value behind (freenet/river#345).
    let secret_key = create_origin_key(origin, &key);
    let enveloped = apply_unconditional_store(ctx.get_secret(&secret_key).as_deref(), &value);
    write_value(ctx, origin, &mut key_index, &key, &enveloped)?;
    logging::info(&format!(
        "Stored secret with key length {}",
        secret_key.len()
    ));

    // Create response for the client
    let response = ChatDelegateResponseMsg::StoreResponse {
        key,
        result: Ok(()),
        value_size: value.len(),
    };

    Ok(vec![create_app_response(&response)?])
}

/// Handle a get request - retrieves value directly
fn handle_get_request(
    ctx: &mut DelegateCtx,
    origin: &Origin,
    key: ChatDelegateKey,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // Create a unique key for this origin's data
    let secret_key = create_origin_key(origin, &key);

    // Get the value directly via host function and strip the versioning
    // envelope so plain `GetRequest` callers see the raw payload exactly
    // as they stored it (back-compat with the outbound-DM cache etc.).
    let (value, _generation) = read_versioned(ctx.get_secret(&secret_key).as_deref());
    logging::info(&format!(
        "Retrieved secret, value present: {}",
        value.is_some()
    ));

    // Create response for the client
    let response = ChatDelegateResponseMsg::GetResponse { key, value };

    Ok(vec![create_app_response(&response)?])
}

/// Handle a delete request - removes value and updates the index.
///
/// Note: this resets the key's CAS generation (a re-created key starts over
/// at 0/1). That is safe today because no CAS-tracked key is ever deleted;
/// see the "generation resets on delete" section in `versioning.rs` before
/// wiring a delete for a key that uses compare-and-swap.
///
/// A listed key is deleted as intent, index, value, clear, so a crash anywhere
/// is finished by the next operation ([`open_kv`]); the index is never left
/// listing a key with no value (a "stale" entry a migration reader cannot tell
/// from a failed read). A delete of an unlisted key never rewrites the index.
fn handle_delete_request(
    ctx: &mut impl SecretStore,
    origin: &Origin,
    key: ChatDelegateKey,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    let secret_key = create_origin_key(origin, &key);
    let index_key = create_index_key(origin);
    let mut key_index = open_kv(ctx, origin)?;
    if key_index.keys.contains(&key) {
        record_intent(ctx, origin, &KvIntent::Delete(key.clone()))?;
        key_index.keys.retain(|k| k != &key);
        set_key_index(ctx, &index_key, &key_index)?;
        // A removal that did not take leaves the intent in place: the next
        // operation finishes the delete ([`open_kv`]) instead of keeping an
        // unlisted value nothing would ever remove.
        remove_value(ctx, &secret_key)?;
        ctx.remove_secret(&create_intent_key(origin));
        logging::info(&format!(
            "Removed key from index, now has {} keys",
            key_index.keys.len()
        ));
    } else {
        ctx.remove_secret(&secret_key);
    }
    logging::info("Removed secret");

    // Create response for the client
    let response = ChatDelegateResponseMsg::DeleteResponse {
        key,
        result: Ok(()),
    };

    Ok(vec![create_app_response(&response)?])
}

/// Handle a list request - returns all keys for this origin
fn handle_list_request(
    ctx: &mut impl SecretStore,
    origin: &Origin,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // An index that exists but does not decode, or reads back absent beside a
    // stored key, is an error, never an empty list (freenet/river#592): a
    // migration reading this delegate as a predecessor would otherwise certify
    // a short export as everything it holds. An interrupted write is finished
    // first, so the listing includes every value that landed.
    let key_index = open_kv_with(ctx, origin, CreateIndex::No)
        .map_err(|e| DelegateError::Other(format!("{LIST_REFUSED_MARKER}: {e}")))?;
    logging::info(&format!(
        "Returning list with {} keys",
        key_index.keys.len()
    ));

    // Create response for the client
    let response = ChatDelegateResponseMsg::ListResponse {
        keys: key_index.keys,
    };

    Ok(vec![create_app_response(&response)?])
}

/// Handle a versioned get - returns the value plus its current generation
/// so the caller can subsequently issue a matching compare-and-swap store.
fn handle_get_versioned_request(
    ctx: &mut DelegateCtx,
    origin: &Origin,
    key: ChatDelegateKey,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    let secret_key = create_origin_key(origin, &key);

    let (value, generation) = read_versioned(ctx.get_secret(&secret_key).as_deref());
    logging::info(&format!(
        "GetVersioned: value present: {}, generation: {generation}",
        value.is_some()
    ));

    let response = ChatDelegateResponseMsg::GetVersionedResponse {
        key,
        value,
        generation,
    };

    Ok(vec![create_app_response(&response)?])
}

/// Handle a compare-and-swap store - the anti-clobber primitive
/// (freenet/river#345). Stores only if `expected_generation` matches the
/// stored generation; otherwise returns the current generation + value so
/// the caller can merge and retry. Concurrent tabs can therefore no longer
/// silently overwrite each other's writes.
fn handle_cas_store_request(
    ctx: &mut impl SecretStore,
    origin: &Origin,
    key: ChatDelegateKey,
    value: Vec<u8>,
    expected_generation: u64,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    let secret_key = create_origin_key(origin, &key);
    // Open the store before writing anything (see `handle_store_request`).
    let mut key_index = open_kv(ctx, origin)?;

    let result = match apply_cas_store(
        ctx.get_secret(&secret_key).as_deref(),
        &value,
        expected_generation,
    ) {
        CasOutcome::Stored { generation, bytes } => {
            // A conflict writes nothing; a match writes as the plain store does
            // (one write for a listed key, else intent, value, index, clear).
            if let Err(e) = write_value(ctx, origin, &mut key_index, &key, &bytes) {
                let response = ChatDelegateResponseMsg::CasStoreResponse {
                    key,
                    result: CasStoreResult::Failed(format!("{e:?}")),
                };
                return Ok(vec![create_app_response(&response)?]);
            }
            logging::info(&format!("CasStore stored at generation {generation}"));
            CasStoreResult::Stored { generation }
        }
        CasOutcome::Conflict {
            current_generation,
            current_value,
        } => {
            logging::info(&format!(
                "CasStore conflict: expected {expected_generation}, current {current_generation}"
            ));
            CasStoreResult::Conflict {
                current_generation,
                current_value,
            }
        }
    };

    let response = ChatDelegateResponseMsg::CasStoreResponse { key, result };
    Ok(vec![create_app_response(&response)?])
}

// ============================================================================
// Signing Operations Handlers
// ============================================================================
//
// SECURITY MODEL:
//
// Signing keys are stored and retrieved using origin attestation to ensure that
// only the contract that stored a key can request signatures with it.
//
// The `attested` parameter passed to the delegate is cryptographically verified
// by Freenet - it contains the ContractInstanceId of the webapp that sent the
// message. This cannot be spoofed.
//
// Keys are stored under: "signing_key:{origin_base58}:{room_key_base58}"
//
// This means:
// - When River (contract A) stores a signing key, it's stored under A's origin
// - When River requests a signature, it looks up using A's origin -> found
// - If malicious contract B requests a signature, it looks up using B's origin -> not found
//
// The private key never leaves the delegate. The UI only receives:
// - Public keys (via GetPublicKey)
// - Signatures (via Sign* operations)
//
// ============================================================================

/// Create a secret key for storing a signing key for a room.
/// Format: "signing_key:{origin_base58}:{room_key_base58}"
fn create_signing_key_secret_key(origin: &Origin, room_key: &RoomKey) -> Vec<u8> {
    let origin_b58 = bs58::encode(&origin.0).into_string();
    let room_key_b58 = bs58::encode(room_key).into_string();
    format!("signing_key:{origin_b58}:{room_key_b58}").into_bytes()
}

/// Handle a store signing key request
fn handle_store_signing_key(
    ctx: &mut DelegateCtx,
    origin: &Origin,
    room_key: RoomKey,
    signing_key_bytes: [u8; 32],
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    let secret_key = create_signing_key_secret_key(origin, &room_key);

    // Store the signing key directly via host function
    // Note: In WASM, set_secret returns true on success. In non-WASM tests, it always returns false.
    #[cfg(target_family = "wasm")]
    if !ctx.set_secret(&secret_key, &signing_key_bytes) {
        return Err(DelegateError::Other(
            "Failed to store signing key via host function".into(),
        ));
    }
    #[cfg(not(target_family = "wasm"))]
    let _ = ctx.set_secret(&secret_key, &signing_key_bytes);

    logging::info("Stored signing key for room");

    // Create response for the client
    let response = ChatDelegateResponseMsg::StoreSigningKeyResponse {
        room_key,
        result: Ok(()),
    };

    Ok(vec![create_app_response(&response)?])
}

/// Handle a get public key request
fn handle_get_public_key(
    ctx: &mut DelegateCtx,
    origin: &Origin,
    room_key: RoomKey,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    let secret_key = create_signing_key_secret_key(origin, &room_key);

    // Get the signing key directly via host function
    let public_key = ctx.get_secret(&secret_key).and_then(|sk_bytes| {
        if sk_bytes.len() == 32 {
            let sk_array: [u8; 32] = sk_bytes.try_into().ok()?;
            let signing_key = SigningKey::from_bytes(&sk_array);
            Some(signing_key.verifying_key().to_bytes())
        } else {
            None
        }
    });

    logging::info(&format!(
        "Retrieved public key for room, key present: {}",
        public_key.is_some()
    ));

    // Create response for the client
    let response = ChatDelegateResponseMsg::GetPublicKeyResponse {
        room_key,
        public_key,
    };

    Ok(vec![create_app_response(&response)?])
}

/// Handle a sign request (for any signable type)
fn handle_sign_request(
    ctx: &mut DelegateCtx,
    origin: &Origin,
    room_key: RoomKey,
    request_id: RequestId,
    data_to_sign: Vec<u8>,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    let secret_key = create_signing_key_secret_key(origin, &room_key);

    // Get the signing key and sign directly
    let signature: Result<Vec<u8>, String> = match ctx.get_secret(&secret_key) {
        Some(sk_bytes) => {
            if sk_bytes.len() == 32 {
                // Safe: we just checked length is 32
                let sk_array: [u8; 32] = sk_bytes.try_into().expect("length verified");
                let signing_key = SigningKey::from_bytes(&sk_array);
                let sig = signing_key.sign(&data_to_sign);
                Ok(sig.to_bytes().to_vec())
            } else {
                Err(format!(
                    "Invalid signing key length: expected 32, got {}",
                    sk_bytes.len()
                ))
            }
        }
        None => Err("Signing key not found for this room".to_string()),
    };

    logging::info(&format!(
        "Sign request for room, signature created: {}",
        signature.is_ok()
    ));

    // Create response for the client
    let response = ChatDelegateResponseMsg::SignResponse {
        room_key,
        request_id,
        signature,
    };

    Ok(vec![create_app_response(&response)?])
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Keys River's UI stores early and keeps, read to tell an index that is
/// genuinely absent (a delegate nothing was ever stored in) from an index
/// read that failed (freenet/freenet-core#5857 reports a failed secret read as
/// "not found"). Every store adds its key to the index, so one of these being
/// present while the index reads absent can only be a read fault. (The UI's
/// `ROOMS_META_KEY`, `ROOMS_STORAGE_KEY` and `OUTBOUND_DMS_STORAGE_KEY`, plus
/// the freenet-migrate completeness journal, `freenet_migrate::MIGRATION_JOURNAL_KEY`,
/// which the UI stores on its first load.)
const INDEX_WITNESS_KEYS: [&[u8]; 4] = [
    b"rooms_meta",
    b"rooms_data",
    b"outbound_dms",
    b"\0freenet-migrate-journal/v1",
];

/// Get the key index from secrets: empty when none is stored, an error when
/// one is stored but does not decode (freenet/river#592), and an error when
/// none reads back although a key it must list is stored (a failed read, which
/// would otherwise be rewritten as a one-key index, or listed as empty).
fn get_key_index(
    ctx: &mut impl SecretStore,
    origin: &Origin,
    index_key: &[u8],
    create: CreateIndex,
) -> Result<KeyIndex, DelegateError> {
    let stored = ctx.get_secret(index_key);
    if stored.is_none() {
        for witness in INDEX_WITNESS_KEYS {
            let key = create_origin_key(origin, &ChatDelegateKey::new(witness.to_vec()));
            if ctx.get_secret(&key).is_some() {
                return Err(DelegateError::Other(format!(
                    "the key index read back absent although {:?} is stored; refusing to \
                     treat it as empty (freenet/river#592, freenet-core#5857)",
                    String::from_utf8_lossy(witness)
                )));
            }
        }
        // Genuinely fresh. On a write path, create the (empty) index now,
        // before any value is written, so no stored key can ever exist without
        // one. A crash before this write leaves nothing stored; one after it
        // leaves an empty index. The read path writes nothing.
        let empty = KeyIndex::default();
        if create == CreateIndex::Yes {
            set_key_index(ctx, index_key, &empty)?;
        }
        return Ok(empty);
    }
    decode_key_index(stored.as_deref())
}

/// The stored index bytes as a [`KeyIndex`]. Absent is a legitimately empty
/// index; present but undecodable is an error. Earlier generations turned the
/// second into an empty index, which a `ListResponse` then reported as "no
/// keys" and the next store wrote back over the real one: every key vanished
/// from the listing with no error (freenet/river#592).
fn decode_key_index(stored: Option<&[u8]>) -> Result<KeyIndex, DelegateError> {
    match stored {
        None => Ok(KeyIndex::default()),
        Some(bytes) => ciborium::from_reader::<KeyIndex, _>(bytes)
            .map_err(|e| DelegateError::Deser(format!("Stored key index does not decode: {e}"))),
    }
}

/// Set the key index in secrets
fn set_key_index(
    ctx: &mut impl SecretStore,
    index_key: &[u8],
    key_index: &KeyIndex,
) -> Result<(), DelegateError> {
    let mut index_bytes = Vec::new();
    ciborium::ser::into_writer(key_index, &mut index_bytes)
        .map_err(|e| DelegateError::Deser(format!("Failed to serialize key index: {e}")))?;

    put_secret(ctx, index_key, &index_bytes, "key index")
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use freenet_stdlib::prelude::{ContractInstanceId, DelegateCtx, MessageOrigin};

    /// Helper function to create empty parameters for testing
    fn create_test_parameters() -> Parameters<'static> {
        Parameters::from(vec![])
    }

    /// Helper function to create an application message
    fn create_app_message(request: ChatDelegateRequestMsg) -> ApplicationMessage {
        let mut payload = Vec::new();
        ciborium::ser::into_writer(&request, &mut payload)
            .map_err(|e| panic!("Failed to serialize request: {e}"))
            .unwrap();
        ApplicationMessage::new(payload)
    }

    /// Helper function to extract response from outbound messages
    fn extract_response(messages: Vec<OutboundDelegateMsg>) -> Option<ChatDelegateResponseMsg> {
        for msg in messages {
            if let OutboundDelegateMsg::ApplicationMessage(app_msg) = msg {
                return ciborium::from_reader(app_msg.payload.as_slice())
                    .map_err(|e| panic!("Failed to deserialize response: {e}"))
                    .ok();
            }
        }
        None
    }

    // Test origin - using a fixed ContractInstanceId for testing
    fn get_test_origin() -> Option<MessageOrigin> {
        Some(MessageOrigin::WebApp(ContractInstanceId::new([42u8; 32])))
    }

    #[test]
    fn test_store_request() {
        let key = b"test_key".to_vec();
        let value = b"test_value".to_vec();

        let request = ChatDelegateRequestMsg::StoreRequest {
            key: river_core::chat_delegate::ChatDelegateKey(key.clone()),
            value: value.clone(),
        };
        let app_msg = create_app_message(request);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = crate::ChatDelegate::process(
            &mut DelegateCtx::default(),
            create_test_parameters(),
            get_test_origin(),
            inbound_msg,
        )
        .unwrap();

        // Should have 1 message: app response (secrets are stored via host function)
        assert_eq!(result.len(), 1);

        // Check app response
        let response = extract_response(result).unwrap();
        match response {
            ChatDelegateResponseMsg::StoreResponse {
                key: resp_key,
                result,
                value_size,
            } => {
                assert_eq!(
                    resp_key,
                    river_core::chat_delegate::ChatDelegateKey(key.clone())
                );
                assert!(result.is_ok());
                assert_eq!(value_size, value.len());
            }
            _ => panic!("Expected StoreResponse, got {:?}", response),
        }
    }

    #[test]
    fn test_get_request() {
        let key = b"test_key".to_vec();

        let request = ChatDelegateRequestMsg::GetRequest {
            key: river_core::chat_delegate::ChatDelegateKey(key.clone()),
        };
        let app_msg = create_app_message(request);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = crate::ChatDelegate::process(
            &mut DelegateCtx::default(),
            create_test_parameters(),
            get_test_origin(),
            inbound_msg,
        )
        .unwrap();

        // Should have 1 message: app response with the value (or None if not found)
        assert_eq!(result.len(), 1);

        // Check it's a GetResponse
        let response = extract_response(result).unwrap();
        match response {
            ChatDelegateResponseMsg::GetResponse {
                key: resp_key,
                value: _,
            } => {
                assert_eq!(
                    resp_key,
                    river_core::chat_delegate::ChatDelegateKey(key.clone())
                );
                // Value will be None in test since we didn't store it first
            }
            _ => panic!("Expected GetResponse, got {:?}", response),
        }
    }

    /// A first CAS write (`expected_generation = 0`) against an absent key
    /// dispatches to the CAS handler and reports `Stored { generation: 1 }`.
    /// (Native `set_secret` is a no-op, so this exercises dispatch + the
    /// generation arithmetic, not persistence — the round-trip logic is
    /// covered by the pure tests in `versioning`.)
    #[test]
    fn test_cas_store_first_write_returns_stored() {
        let request = ChatDelegateRequestMsg::CasStoreRequest {
            key: river_core::chat_delegate::ChatDelegateKey(b"rooms_data".to_vec()),
            value: b"snapshot".to_vec(),
            expected_generation: 0,
        };
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(create_app_message(request));
        let result = crate::ChatDelegate::process(
            &mut DelegateCtx::default(),
            create_test_parameters(),
            get_test_origin(),
            inbound_msg,
        )
        .unwrap();
        match extract_response(result).unwrap() {
            ChatDelegateResponseMsg::CasStoreResponse { result, .. } => {
                assert_eq!(result, CasStoreResult::Stored { generation: 1 });
            }
            other => panic!("Expected CasStoreResponse, got {other:?}"),
        }
    }

    /// A CAS write whose `expected_generation` does not match the stored
    /// generation (here: stale `5` vs absent/`0`) is rejected with a
    /// `Conflict` carrying the true current state — the anti-clobber guard.
    #[test]
    fn test_cas_store_stale_generation_returns_conflict() {
        let request = ChatDelegateRequestMsg::CasStoreRequest {
            key: river_core::chat_delegate::ChatDelegateKey(b"rooms_data".to_vec()),
            value: b"stale".to_vec(),
            expected_generation: 5,
        };
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(create_app_message(request));
        let result = crate::ChatDelegate::process(
            &mut DelegateCtx::default(),
            create_test_parameters(),
            get_test_origin(),
            inbound_msg,
        )
        .unwrap();
        match extract_response(result).unwrap() {
            ChatDelegateResponseMsg::CasStoreResponse { result, .. } => {
                assert_eq!(
                    result,
                    CasStoreResult::Conflict {
                        current_generation: 0,
                        current_value: None,
                    }
                );
            }
            other => panic!("Expected CasStoreResponse, got {other:?}"),
        }
    }

    /// `GetVersionedRequest` dispatches and reports generation `0` for an
    /// absent key.
    #[test]
    fn test_get_versioned_absent_returns_generation_zero() {
        let request = ChatDelegateRequestMsg::GetVersionedRequest {
            key: river_core::chat_delegate::ChatDelegateKey(b"rooms_data".to_vec()),
        };
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(create_app_message(request));
        let result = crate::ChatDelegate::process(
            &mut DelegateCtx::default(),
            create_test_parameters(),
            get_test_origin(),
            inbound_msg,
        )
        .unwrap();
        match extract_response(result).unwrap() {
            ChatDelegateResponseMsg::GetVersionedResponse {
                value, generation, ..
            } => {
                assert_eq!(value, None);
                assert_eq!(generation, 0);
            }
            other => panic!("Expected GetVersionedResponse, got {other:?}"),
        }
    }

    #[test]
    fn test_delete_request() {
        let key = b"test_key".to_vec();

        let request = ChatDelegateRequestMsg::DeleteRequest {
            key: river_core::chat_delegate::ChatDelegateKey(key.clone()),
        };
        let app_msg = create_app_message(request);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = crate::ChatDelegate::process(
            &mut DelegateCtx::default(),
            create_test_parameters(),
            get_test_origin(),
            inbound_msg,
        )
        .unwrap();

        // Should have 1 message: app response
        assert_eq!(result.len(), 1);

        // Check response
        let response = extract_response(result).unwrap();
        match response {
            ChatDelegateResponseMsg::DeleteResponse {
                key: resp_key,
                result,
            } => {
                assert_eq!(
                    resp_key,
                    river_core::chat_delegate::ChatDelegateKey(key.clone())
                );
                assert!(result.is_ok());
            }
            _ => panic!("Expected DeleteResponse, got {:?}", response),
        }
    }

    #[test]
    fn test_list_request() {
        let request = ChatDelegateRequestMsg::ListRequest;
        let app_msg = create_app_message(request);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = crate::ChatDelegate::process(
            &mut DelegateCtx::default(),
            create_test_parameters(),
            get_test_origin(),
            inbound_msg,
        )
        .unwrap();

        // Should have 1 message: app response with list
        assert_eq!(result.len(), 1);

        // Check response
        let response = extract_response(result).unwrap();
        match response {
            ChatDelegateResponseMsg::ListResponse { keys } => {
                // Empty list since we haven't stored anything
                assert!(keys.is_empty());
            }
            _ => panic!("Expected ListResponse, got {:?}", response),
        }
    }

    #[test]
    fn test_error_on_processed_message() {
        let request = ChatDelegateRequestMsg::ListRequest;
        let mut payload = Vec::new();
        ciborium::ser::into_writer(&request, &mut payload).unwrap();

        let app_msg = ApplicationMessage::new(payload).processed(true);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = crate::ChatDelegate::process(
            &mut DelegateCtx::default(),
            create_test_parameters(),
            get_test_origin(),
            inbound_msg,
        );

        assert!(result.is_err());
        if let Err(DelegateError::Other(msg)) = result {
            assert!(msg.contains("already processed"));
        } else {
            panic!("Expected DelegateError::Other, got {:?}", result);
        }
    }

    #[test]
    fn test_error_on_missing_origin() {
        let request = ChatDelegateRequestMsg::ListRequest;
        let app_msg = create_app_message(request);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        // Pass None for origin
        let result = crate::ChatDelegate::process(
            &mut DelegateCtx::default(),
            create_test_parameters(),
            None,
            inbound_msg,
        );
        assert!(result.is_err());

        if let Err(DelegateError::Other(msg)) = result {
            assert!(msg.contains("missing message origin"));
        } else {
            panic!("Expected DelegateError::Other, got {:?}", result);
        }
    }

    /// The chat delegate is driven exclusively by the River web app via
    /// `MessageOrigin::WebApp`; inter-delegate invocation is not a supported
    /// authorization path. Regression test for the new rejection arm added
    /// alongside the freenet-stdlib 0.5.0/0.6.0 bump — `MessageOrigin::Delegate`
    /// did not exist at all before that bump, so this test pins the explicit
    /// "reject inter-delegate caller" policy so it cannot regress silently
    /// if a future refactor reorders the `match origin` arms.
    #[test]
    fn test_error_on_inter_delegate_origin() {
        use freenet_stdlib::prelude::{CodeHash, DelegateKey};

        let request = ChatDelegateRequestMsg::ListRequest;
        let app_msg = create_app_message(request);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        // Synthesize a caller DelegateKey — its actual value doesn't matter
        // for the test, only that the variant is `MessageOrigin::Delegate(..)`.
        let caller = DelegateKey::new([0xA1u8; 32], CodeHash::new([0xA2u8; 32]));
        let origin = Some(MessageOrigin::Delegate(caller));

        let result = crate::ChatDelegate::process(
            &mut DelegateCtx::default(),
            create_test_parameters(),
            origin,
            inbound_msg,
        );
        assert!(result.is_err(), "inter-delegate caller must be rejected");

        if let Err(DelegateError::Other(msg)) = result {
            assert!(
                msg.contains("does not accept inter-delegate calls"),
                "expected 'inter-delegate' rejection message, got: {msg}"
            );
        } else {
            panic!("Expected DelegateError::Other, got {:?}", result);
        }
    }

    #[test]
    fn test_store_signing_key() {
        let room_key: river_core::chat_delegate::RoomKey = [7u8; 32];
        let signing_key_bytes: [u8; 32] = [8u8; 32];

        let request = ChatDelegateRequestMsg::StoreSigningKey {
            room_key,
            signing_key_bytes,
        };
        let app_msg = create_app_message(request);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = crate::ChatDelegate::process(
            &mut DelegateCtx::default(),
            create_test_parameters(),
            get_test_origin(),
            inbound_msg,
        )
        .unwrap();

        // Should have 1 message: app response
        assert_eq!(result.len(), 1);

        // Check response
        let response = extract_response(result).unwrap();
        match response {
            ChatDelegateResponseMsg::StoreSigningKeyResponse {
                room_key: resp_room_key,
                result,
            } => {
                assert_eq!(resp_room_key, room_key);
                assert!(result.is_ok());
            }
            _ => panic!("Expected StoreSigningKeyResponse, got {:?}", response),
        }
    }

    #[test]
    fn test_get_public_key_not_found() {
        let room_key: river_core::chat_delegate::RoomKey = [9u8; 32];

        let request = ChatDelegateRequestMsg::GetPublicKey { room_key };
        let app_msg = create_app_message(request);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = crate::ChatDelegate::process(
            &mut DelegateCtx::default(),
            create_test_parameters(),
            get_test_origin(),
            inbound_msg,
        )
        .unwrap();

        // Should have 1 message: app response
        assert_eq!(result.len(), 1);

        // Check response - public key should be None since no key is stored
        let response = extract_response(result).unwrap();
        match response {
            ChatDelegateResponseMsg::GetPublicKeyResponse {
                room_key: resp_room_key,
                public_key,
            } => {
                assert_eq!(resp_room_key, room_key);
                // In non-WASM test environment, get_secret returns None
                assert!(public_key.is_none());
            }
            _ => panic!("Expected GetPublicKeyResponse, got {:?}", response),
        }
    }

    #[test]
    fn test_sign_message_without_key_returns_error() {
        let room_key: river_core::chat_delegate::RoomKey = [10u8; 32];
        let request_id: river_core::chat_delegate::RequestId = 12345;
        let message_bytes = b"test message to sign".to_vec();

        let request = ChatDelegateRequestMsg::SignMessage {
            room_key,
            request_id,
            message_bytes,
        };
        let app_msg = create_app_message(request);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = crate::ChatDelegate::process(
            &mut DelegateCtx::default(),
            create_test_parameters(),
            get_test_origin(),
            inbound_msg,
        )
        .unwrap();

        // Should have 1 message: app response
        assert_eq!(result.len(), 1);

        // Check response - signature should be an error since no key is stored
        let response = extract_response(result).unwrap();
        match response {
            ChatDelegateResponseMsg::SignResponse {
                room_key: resp_room_key,
                request_id: resp_request_id,
                signature,
            } => {
                assert_eq!(resp_room_key, room_key);
                assert_eq!(resp_request_id, request_id);
                // Should be an error because no signing key is stored
                assert!(signature.is_err());
                let err_msg = signature.unwrap_err();
                assert!(
                    err_msg.contains("not found"),
                    "Expected 'not found' error, got: {}",
                    err_msg
                );
            }
            _ => panic!("Expected SignResponse, got {:?}", response),
        }
    }

    /// freenet/river#592: an index that is stored but does not decode is an
    /// error, never an empty index. An empty one was reported by
    /// `ListResponse` as "no keys" and written back over the real index by
    /// the next store.
    #[test]
    fn an_undecodable_index_is_an_error_not_an_empty_index() {
        assert!(decode_key_index(None).unwrap().keys.is_empty());

        let index = KeyIndex {
            keys: vec![ChatDelegateKey::new(b"room:a".to_vec())],
        };
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&index, &mut bytes).unwrap();
        assert_eq!(decode_key_index(Some(&bytes)).unwrap().keys, index.keys);

        assert!(decode_key_index(Some(&[0xFF, 0x00, 0x13])).is_err());
        // Truncated: a valid prefix of a real index.
        assert!(decode_key_index(Some(&bytes[..bytes.len() - 2])).is_err());
    }

    // ---------------------------------------------------------------------
    // Probes added by the testing review of PR #768: the key-value handlers
    // against a fake secret store (the native `DelegateCtx` stores nothing).
    // ---------------------------------------------------------------------

    use std::collections::BTreeMap;

    #[derive(Default)]
    struct FakeSecrets {
        map: BTreeMap<Vec<u8>, Vec<u8>>,
        /// Every mutation, in order: ("set" | "remove", key).
        log: Vec<(&'static str, Vec<u8>)>,
        /// A crash: once this many mutations have run, every later one fails
        /// (a set returns `false`, a remove does nothing), as if the process
        /// had died between two writes.
        crash_after: Option<usize>,
        mutations: usize,
        /// Removing this key "succeeds" without removing it (a host removal
        /// that did not take).
        sticky: Option<Vec<u8>>,
    }

    impl FakeSecrets {
        fn crashed(&mut self) -> bool {
            self.mutations += 1;
            self.crash_after.is_some_and(|n| self.mutations > n)
        }
    }

    impl SecretStore for FakeSecrets {
        fn get_secret(&self, key: &[u8]) -> Option<Vec<u8>> {
            self.map.get(key).cloned()
        }
        fn set_secret(&mut self, key: &[u8], value: &[u8]) -> bool {
            if self.crashed() {
                return false;
            }
            self.log.push(("set", key.to_vec()));
            self.map.insert(key.to_vec(), value.to_vec());
            true
        }
        fn remove_secret(&mut self, key: &[u8]) {
            if self.crashed() {
                return;
            }
            self.log.push(("remove", key.to_vec()));
            if self.sticky.as_deref() != Some(key) {
                self.map.remove(key);
            }
        }
    }

    fn origin() -> Origin {
        Origin(vec![7u8; 32])
    }

    fn ck(k: &str) -> ChatDelegateKey {
        river_core::chat_delegate::ChatDelegateKey(k.as_bytes().to_vec())
    }

    /// A store holding `keys` (each with a value) and a decodable index over them.
    fn store_with(keys: &[&str]) -> FakeSecrets {
        let mut f = FakeSecrets::default();
        for k in keys {
            f.map
                .insert(create_origin_key(&origin(), &ck(k)), b"v".to_vec());
        }
        let index = KeyIndex {
            keys: keys.iter().map(|k| ck(k)).collect(),
        };
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&index, &mut bytes).unwrap();
        f.map.insert(create_index_key(&origin()), bytes);
        f
    }

    /// A store whose index is present but undecodable, beside `room:a`.
    fn store_with_corrupt_index() -> FakeSecrets {
        let mut f = store_with(&["room:a"]);
        f.map
            .insert(create_index_key(&origin()), vec![0xFF, 0x00, 0x13]);
        f
    }

    fn index_of(f: &FakeSecrets) -> Vec<ChatDelegateKey> {
        decode_key_index(
            f.map
                .get(&create_index_key(&origin()))
                .map(|v| v.as_slice()),
        )
        .unwrap()
        .keys
    }

    #[test]
    fn a_store_over_an_undecodable_index_fails_before_writing_anything() {
        let mut f = store_with_corrupt_index();
        let before = f.map.clone();
        assert!(handle_store_request(&mut f, &origin(), ck("room:b"), b"x".to_vec()).is_err());
        assert!(f.log.is_empty(), "nothing may be written: {:?}", f.log);
        assert_eq!(
            f.map, before,
            "the corrupt index must not be replaced by a one-key index"
        );
    }

    #[test]
    fn a_cas_store_over_an_undecodable_index_fails_before_writing_anything() {
        let mut f = store_with_corrupt_index();
        let before = f.map.clone();
        assert!(
            handle_cas_store_request(&mut f, &origin(), ck("room:b"), b"x".to_vec(), 0).is_err()
        );
        assert!(f.log.is_empty(), "nothing may be written: {:?}", f.log);
        assert_eq!(f.map, before);
    }

    #[test]
    fn a_delete_over_an_undecodable_index_removes_nothing() {
        let mut f = store_with_corrupt_index();
        let before = f.map.clone();
        assert!(handle_delete_request(&mut f, &origin(), ck("room:a")).is_err());
        assert!(
            f.log.is_empty(),
            "nothing may be removed or written: {:?}",
            f.log
        );
        assert_eq!(f.map, before);
    }

    #[test]
    fn a_list_over_an_undecodable_index_is_an_error_not_an_empty_list() {
        let mut f = store_with_corrupt_index();
        assert!(handle_list_request(&mut f, &origin()).is_err());
        // And over a decodable one it lists what is indexed.
        let mut ok = store_with(&["room:a", "room:b"]);
        let resp = extract_response(handle_list_request(&mut ok, &origin()).unwrap()).unwrap();
        match resp {
            ChatDelegateResponseMsg::ListResponse { keys } => {
                assert_eq!(keys, vec![ck("room:a"), ck("room:b")])
            }
            other => panic!("expected ListResponse, got {other:?}"),
        }
    }

    /// The index is rewritten BEFORE the value is removed, so a failed index
    /// write leaves the key listed AND present, never listed with no value.
    #[test]
    fn a_delete_updates_the_index_before_removing_the_value() {
        let mut f = store_with(&["room:a", "room:b"]);
        handle_delete_request(&mut f, &origin(), ck("room:a")).unwrap();
        assert_eq!(index_of(&f), vec![ck("room:b")]);
        let intent = create_intent_key(&origin());
        let index = create_index_key(&origin());
        let value = create_origin_key(&origin(), &ck("room:a"));
        let order: Vec<_> = f.log.iter().map(|(op, k)| (*op, k.clone())).collect();
        assert_eq!(
            order,
            vec![
                ("set", intent.clone()),
                ("set", index),
                ("remove", value),
                ("remove", intent)
            ],
            "intent, index, value, clear (crash-safe: see the crash tests)"
        );
    }

    #[test]
    fn deleting_an_unlisted_key_never_rewrites_the_index() {
        let mut f = store_with(&["room:a"]);
        f.map.insert(
            create_origin_key(&origin(), &ck("room:ghost")),
            b"v".to_vec(),
        );
        handle_delete_request(&mut f, &origin(), ck("room:ghost")).unwrap();
        assert!(
            !f.log
                .iter()
                .any(|(op, k)| *op == "set" && *k == create_index_key(&origin())),
            "{:?}",
            f.log
        );
        assert_eq!(index_of(&f), vec![ck("room:a")]);
    }

    #[test]
    fn a_store_lists_the_key_and_both_store_paths_agree() {
        let mut f = store_with(&["room:a"]);
        handle_store_request(&mut f, &origin(), ck("room:b"), b"x".to_vec()).unwrap();
        assert_eq!(index_of(&f), vec![ck("room:a"), ck("room:b")]);
        let mut g = store_with(&["room:a"]);
        handle_cas_store_request(&mut g, &origin(), ck("room:b"), b"x".to_vec(), 0).unwrap();
        assert_eq!(index_of(&g), vec![ck("room:a"), ck("room:b")]);
        // A CAS conflict writes nothing, not even the index.
        let mut h = store_with(&["room:a"]);
        let key_a = create_origin_key(&origin(), &ck("room:a"));
        let _ = key_a;
        handle_cas_store_request(&mut h, &origin(), ck("room:a"), b"y".to_vec(), 99).unwrap();
        assert!(
            h.log.is_empty(),
            "a conflicting CAS must write nothing: {:?}",
            h.log
        );
    }

    /// A `None` index read beside a stored witness key is a read fault
    /// (freenet-core#5857): store, CAS, delete and list must refuse, never
    /// rewrite a one-key index or list nothing (#592 residual, #595 route 2).
    #[test]
    fn an_absent_index_beside_a_stored_witness_key_refuses_every_index_path() {
        let lost_index = || {
            let mut f = store_with(&["rooms_meta", "room:a"]);
            f.map.remove(&create_index_key(&origin()));
            f.log.clear();
            f
        };
        let mut f = lost_index();
        let before = f.map.clone();
        assert!(handle_store_request(&mut f, &origin(), ck("room:b"), b"x".to_vec()).is_err());
        assert!(
            handle_cas_store_request(&mut f, &origin(), ck("room:b"), b"x".to_vec(), 0).is_err()
        );
        assert!(handle_delete_request(&mut f, &origin(), ck("room:a")).is_err());
        assert!(handle_list_request(&mut f, &origin()).is_err());
        assert!(f.log.is_empty(), "nothing may be written: {:?}", f.log);
        assert_eq!(f.map, before);

        // A genuinely fresh delegate (no index, no witness) still stores.
        let mut fresh = FakeSecrets::default();
        handle_store_request(&mut fresh, &origin(), ck("rooms_meta"), b"m".to_vec()).unwrap();
        assert_eq!(index_of(&fresh), vec![ck("rooms_meta")]);
    }

    // ---------------------------------------------------------------------
    // Crash between ANY two writes (#768 review, item 7): fail after each
    // individual write of each operation, restart, and show that the next
    // list, store, CAS and delete all succeed and nothing acknowledged is
    // lost. Atomicity of the interrupted operation itself: it either took
    // effect (its key listed AND readable) or did not (unlisted AND absent),
    // never half (listed with no value, or a value nothing lists).
    // ---------------------------------------------------------------------

    fn listed(f: &mut FakeSecrets) -> Vec<ChatDelegateKey> {
        match extract_response(handle_list_request(f, &origin()).expect("list after a crash"))
            .unwrap()
        {
            ChatDelegateResponseMsg::ListResponse { keys } => keys,
            other => panic!("expected ListResponse, got {other:?}"),
        }
    }

    fn value_of(f: &FakeSecrets, k: &str) -> Option<Vec<u8>> {
        read_versioned(
            f.map
                .get(&create_origin_key(&origin(), &ck(k)))
                .map(|v| v.as_slice()),
        )
        .0
    }

    /// After recovery: every listed key is readable, every readable
    /// (non-bookkeeping) key is listed, and the four operations work.
    fn assert_recovers(mut f: FakeSecrets, must_keep: &[&str], what: &str) {
        f.crash_after = None;
        let keys = listed(&mut f);
        for k in &keys {
            assert!(
                value_of(&f, std::str::from_utf8(&k.0).unwrap()).is_some(),
                "{what}: {k:?} listed with no value"
            );
        }
        for k in must_keep {
            assert!(
                keys.contains(&ck(k)),
                "{what}: acknowledged {k} lost from the listing"
            );
            assert!(value_of(&f, k).is_some(), "{what}: acknowledged {k} lost");
        }
        // Every stored origin key (other than bookkeeping) is listed.
        let prefix = format!("{}{}", origin().to_b58(), ORIGIN_KEY_SEPARATOR).into_bytes();
        for raw in f.map.keys().filter(|k| k.starts_with(&prefix)) {
            let k = &raw[prefix.len()..];
            if k == KEY_INDEX_SUFFIX.as_bytes() || k == KV_INTENT_SUFFIX.as_bytes() {
                continue;
            }
            assert!(
                keys.contains(&ChatDelegateKey(k.to_vec())),
                "{what}: a stored value nothing lists: {:?}",
                String::from_utf8_lossy(k)
            );
        }
        assert!(
            !f.map.contains_key(&create_intent_key(&origin())),
            "{what}: the intent is cleared"
        );
        handle_store_request(&mut f, &origin(), ck("room:new"), b"n".to_vec())
            .unwrap_or_else(|e| panic!("{what}: store after recovery: {e:?}"));
        handle_cas_store_request(&mut f, &origin(), ck("room:cas"), b"c".to_vec(), 0)
            .unwrap_or_else(|e| panic!("{what}: CAS after recovery: {e:?}"));
        handle_delete_request(&mut f, &origin(), ck("room:new"))
            .unwrap_or_else(|e| panic!("{what}: delete after recovery: {e:?}"));
        let keys = listed(&mut f);
        assert!(
            keys.contains(&ck("room:cas")) && !keys.contains(&ck("room:new")),
            "{what}"
        );
        for k in must_keep {
            assert!(
                keys.contains(&ck(k)),
                "{what}: {k} lost after the follow-up ops"
            );
        }
    }

    /// Writes an operation makes on `start`, counted on a crash-free copy.
    fn writes_of(start: &FakeSecrets, op: &dyn Fn(&mut FakeSecrets)) -> usize {
        let mut f = FakeSecrets {
            map: start.map.clone(),
            ..Default::default()
        };
        op(&mut f);
        f.mutations
    }

    fn crash_at_every_write(
        start: FakeSecrets,
        op: &dyn Fn(&mut FakeSecrets),
        keep: &[&str],
        what: &str,
    ) {
        let n = writes_of(&start, op);
        assert!(n > 0, "{what}: the operation must write");
        for crash_after in 0..n {
            let mut f = FakeSecrets {
                map: start.map.clone(),
                crash_after: Some(crash_after),
                ..Default::default()
            };
            op(&mut f);
            assert_recovers(
                f,
                keep,
                &format!("{what}, crash after write {crash_after} of {n}"),
            );
        }
    }

    #[test]
    fn a_crash_between_any_two_writes_of_a_store_wedges_nothing() {
        let op = |f: &mut FakeSecrets| {
            let _ = handle_store_request(f, &origin(), ck("rooms_meta"), b"m".to_vec());
        };
        crash_at_every_write(
            store_with(&["room:a"]),
            &op,
            &["room:a"],
            "store new witness key",
        );
        let op2 = |f: &mut FakeSecrets| {
            let _ = handle_store_request(f, &origin(), ck("room:a"), b"m2".to_vec());
        };
        crash_at_every_write(
            store_with(&["room:a"]),
            &op2,
            &["room:a"],
            "store listed key",
        );
    }

    #[test]
    fn a_crash_between_any_two_writes_of_a_cas_wedges_nothing() {
        let op = |f: &mut FakeSecrets| {
            let _ = handle_cas_store_request(f, &origin(), ck("room:b"), b"b".to_vec(), 0);
        };
        crash_at_every_write(store_with(&["room:a"]), &op, &["room:a"], "CAS new key");
    }

    #[test]
    fn a_crash_between_any_two_writes_of_a_delete_wedges_nothing() {
        let op = |f: &mut FakeSecrets| {
            let _ = handle_delete_request(f, &origin(), ck("room:a"));
        };
        crash_at_every_write(
            store_with(&["room:a", "rooms_meta"]),
            &op,
            &["rooms_meta"],
            "delete listed key",
        );
    }

    /// The first load of a fresh delegate: list (which writes nothing), then
    /// the first store creates the index and writes a witness key (`rooms_meta`). A crash at any
    /// write of that sequence never leaves a witness without an index (the
    /// refusal that used to lock the user out for good).
    #[test]
    fn a_crash_during_the_first_load_and_first_store_wedges_nothing() {
        let op = |f: &mut FakeSecrets| {
            if handle_list_request(f, &origin()).is_ok() {
                let _ = handle_store_request(f, &origin(), ck("rooms_meta"), b"m".to_vec());
            }
        };
        crash_at_every_write(
            FakeSecrets::default(),
            &op,
            &[],
            "first load + first witness store",
        );
        // And a first STORE without a list before it.
        let op2 = |f: &mut FakeSecrets| {
            let _ = handle_store_request(f, &origin(), ck("rooms_meta"), b"m".to_vec());
        };
        crash_at_every_write(FakeSecrets::default(), &op2, &[], "first witness store");
    }

    /// The A5 guarantee holds alongside crash recovery: an index that reads
    /// back ABSENT beside a stored witness (a read fault, not a crash: a crash
    /// cannot produce it, since the index is created before any value) is
    /// refused, never rewritten short, on every path.
    #[test]
    fn recovery_never_rewrites_a_short_index_over_a_read_fault() {
        let mut f = store_with(&["rooms_meta", "room:a"]);
        f.map.remove(&create_index_key(&origin()));
        let before = f.map.clone();
        assert!(handle_list_request(&mut f, &origin()).is_err());
        assert!(handle_store_request(&mut f, &origin(), ck("room:b"), b"x".to_vec()).is_err());
        assert_eq!(f.map, before);
    }
    /// #768 round 3, S4: a `ListRequest` on a fresh delegate answers an empty
    /// list and writes nothing; the first STORE creates the index.
    #[test]
    fn listing_a_fresh_delegate_writes_nothing() {
        let mut f = FakeSecrets::default();
        assert!(listed(&mut f).is_empty());
        assert_eq!(f.mutations, 0, "the read path wrote: {:?}", f.log);
        assert!(f.map.is_empty());
        handle_store_request(&mut f, &origin(), ck("rooms_meta"), b"m".to_vec()).unwrap();
        assert!(f.map.contains_key(&create_index_key(&origin())));
    }

    /// #768 round 3, S4: the migration journal is a witness too. An index that
    /// reads back absent beside a stored journal is a read fault, refused.
    #[test]
    fn an_absent_index_beside_a_stored_journal_is_refused() {
        let mut f = store_with(&["\0freenet-migrate-journal/v1"]);
        f.map.remove(&create_index_key(&origin()));
        let before = f.map.clone();
        assert!(handle_list_request(&mut f, &origin()).is_err());
        assert_eq!(f.map, before);
    }

    /// #768 round 3, S1: a refused listing carries the marker the UI keys on.
    #[test]
    fn a_refused_listing_carries_the_list_refused_marker() {
        let mut f = store_with(&["rooms_meta"]);
        f.map.remove(&create_index_key(&origin()));
        let err = handle_list_request(&mut f, &origin()).unwrap_err();
        assert!(
            format!("{err}").contains(LIST_REFUSED_MARKER),
            "the UI cannot recognise this refusal: {err}"
        );
    }

    /// #768 round 3 nit: a delete whose value removal did not take fails and
    /// keeps the intent, so the next operation finishes it; it never leaves an
    /// unlisted value behind with the intent cleared.
    #[test]
    fn a_delete_whose_removal_does_not_take_is_finished_later() {
        let mut f = store_with(&["room:a", "rooms_meta"]);
        let raw = create_origin_key(&origin(), &ck("room:a"));
        f.sticky = Some(raw.clone());
        assert!(handle_delete_request(&mut f, &origin(), ck("room:a")).is_err());
        assert!(
            f.map.contains_key(&create_intent_key(&origin())),
            "intent kept"
        );
        assert!(
            !listed(&mut f).contains(&ck("room:a")),
            "the listing goes ahead"
        );
        assert!(
            f.map.contains_key(&raw),
            "a recovery that cannot remove keeps trying"
        );
        assert!(
            f.map.contains_key(&create_intent_key(&origin())),
            "intent still kept"
        );
        assert!(
            handle_store_request(&mut f, &origin(), ck("room:c"), b"c".to_vec()).is_err(),
            "a write must not replace the pending delete's intent"
        );
        assert!(
            f.map.contains_key(&create_intent_key(&origin())),
            "intent still kept"
        );
        f.sticky = None;
        assert_recovers(f, &["rooms_meta"], "removal fixed");
    }

    /// #768 round 3, CF5: a crash DURING crash recovery. Crash the original
    /// operation at every write, then crash the recovering operation at every
    /// write of its own, then recover again: nothing is wedged or lost.
    fn crash_during_recovery(
        start: FakeSecrets,
        op: &dyn Fn(&mut FakeSecrets),
        recover: &dyn Fn(&mut FakeSecrets),
        keep: &[&str],
        what: &str,
    ) -> usize {
        let mut cases = 0;
        let n = writes_of(&start, op);
        for first in 0..n {
            let mut crashed = FakeSecrets {
                map: start.map.clone(),
                crash_after: Some(first),
                ..Default::default()
            };
            op(&mut crashed);
            let m = writes_of(&crashed, recover);
            for second in 0..m {
                let mut f = FakeSecrets {
                    map: crashed.map.clone(),
                    crash_after: Some(second),
                    ..Default::default()
                };
                recover(&mut f);
                cases += 1;
                assert_recovers(
                    f,
                    keep,
                    &format!("{what}: crash after write {first} of {n}, then after recovery write {second} of {m}"),
                );
            }
        }
        cases
    }

    #[test]
    fn a_crash_during_recovery_wedges_nothing() {
        let list = |f: &mut FakeSecrets| {
            let _ = handle_list_request(f, &origin());
        };
        let store = |f: &mut FakeSecrets| {
            let _ = handle_store_request(f, &origin(), ck("room:z"), b"z".to_vec());
        };
        let add = |f: &mut FakeSecrets| {
            let _ = handle_store_request(f, &origin(), ck("room:b"), b"b".to_vec());
        };
        let delete = |f: &mut FakeSecrets| {
            let _ = handle_delete_request(f, &origin(), ck("room:a"));
        };
        let mut cases = 0;
        for (op, keep, what) in [
            (&add as &dyn Fn(&mut FakeSecrets), &["room:a"][..], "add"),
            (&delete, &["rooms_meta"][..], "delete"),
        ] {
            for (recover, rname) in [
                (&list as &dyn Fn(&mut FakeSecrets), "list"),
                (&store, "store"),
            ] {
                cases += crash_during_recovery(
                    store_with(&["room:a", "rooms_meta"]),
                    op,
                    recover,
                    keep,
                    &format!("{what}, recovered by {rname}"),
                );
            }
        }
        assert!(cases > 0, "no crash-during-recovery case ran");
    }
}
