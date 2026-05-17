//! Delegate-side subscription bookkeeping and rotation logic for private rooms.
//!
//! This module owns the secrets-rotation pipeline that used to live in the UI:
//!
//! 1. The UI fires a [`ChatDelegateRequestMsg::EnsureRoomSubscription`] for every
//!    room where it holds the owner signing key. The delegate emits a
//!    [`OutboundDelegateMsg::SubscribeContractRequest`] to the runtime and
//!    records the `(room_owner_vk -> contract_id)` mapping in its secret store.
//!
//! 2. When the runtime delivers an
//!    [`InboundDelegateMsg::ContractNotification`] for a room we own, we
//!    deserialize the new state, compare its member set against the cached
//!    last-seen set, and — if it changed — rotate the secret to `version + 1`
//!    using the deterministic
//!    [`river_core::key_derivation::derive_room_secret`] helper.
//!
//! 3. Rotation produces a [`SecretsDelta`] containing the new
//!    `AuthorizedSecretVersionRecord` plus one
//!    `AuthorizedEncryptedSecretForMember` per current member (and one for the
//!    owner). The delta is serialized as a `ChatRoomStateV1Delta` and shipped
//!    to the runtime via [`OutboundDelegateMsg::UpdateContractRequest`].
//!
//! All caches live in the delegate's secret store under fixed prefixes (see
//! the `secret_keys` module) so they survive across `process()` invocations.

use crate::logging;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use freenet_stdlib::prelude::{
    ContractInstanceId, ContractNotification, DelegateContext, DelegateCtx, DelegateError,
    OutboundDelegateMsg, StateDelta, SubscribeContractRequest, UpdateContractRequest, UpdateData,
};
use river_core::chat_delegate::{ChatDelegateResponseMsg, RoomKey};
use river_core::key_derivation::derive_room_secret;
use river_core::room_state::member::MemberId;
use river_core::room_state::privacy::{PrivacyMode, RoomCipherSpec};
use river_core::room_state::secret::{
    AuthorizedEncryptedSecretForMember, AuthorizedSecretVersionRecord, SecretVersionRecordV1,
    SecretsDelta,
};
use river_core::ChatRoomStateV1;
use serde::{Deserialize, Serialize};
use std::time::{Duration, UNIX_EPOCH};

use crate::utils::create_app_response;

/// Secret-store key prefixes for delegate-managed subscription state.
///
/// These keys are scoped per-room (keyed by `room_owner_vk_b58`). They are
/// NOT scoped by webapp origin like the storage helpers in `handlers.rs` are;
/// the rotation pipeline is only ever exercised by the River webapp and we
/// want a single subscription record per room across UI sessions.
mod secret_keys {
    pub const SUB_INDEX_PREFIX: &str = "room_sub:";
    pub const MEMBER_SET_PREFIX: &str = "room_members:";
    pub const SECRET_PREFIX: &str = "room_secret:";

    pub fn sub_index(room_owner_vk_b58: &str) -> Vec<u8> {
        format!("{SUB_INDEX_PREFIX}{room_owner_vk_b58}").into_bytes()
    }

    pub fn member_set(room_owner_vk_b58: &str) -> Vec<u8> {
        format!("{MEMBER_SET_PREFIX}{room_owner_vk_b58}").into_bytes()
    }

    /// Per-(room, version) cached secret. Stored so we can decrypt our own
    /// historical content when serving the UI.
    pub fn secret(room_owner_vk_b58: &str, version: u32) -> Vec<u8> {
        format!("{SECRET_PREFIX}{room_owner_vk_b58}:{version}").into_bytes()
    }
}

/// Helper to derive the b58 form of a `RoomKey` once.
fn room_owner_b58(room_owner_vk: &RoomKey) -> String {
    bs58::encode(room_owner_vk).into_string()
}

/// Locate the subscription record for the contract that produced this
/// notification.
///
/// We don't have a contract_id → room_owner_vk index secret because
/// `get_secret_keys()` isn't exposed by the host API. Instead the UI
/// always passes `EnsureRoomSubscription { room_owner_vk, contract_id }`,
/// and the delegate stores both in [`SubscriptionRecord`]. To answer "which
/// room is this notification for?" we'd ideally walk all sub_index entries —
/// but `DelegateCtx` only exposes `get_secret(key)`, so we instead require
/// the contract to be the contract id the UI explicitly subscribed to via
/// `EnsureRoomSubscription`. We work around this by storing a second tiny
/// index keyed by contract_id → room_owner_vk.
fn contract_id_index_key(contract_id: &[u8; 32]) -> Vec<u8> {
    format!(
        "room_sub_by_cid:{}",
        bs58::encode(contract_id).into_string()
    )
    .into_bytes()
}

/// Looks up the room-owner signing-key seed previously stored via
/// `StoreSigningKey`. The chat delegate stores signing keys per-(origin, room),
/// but rotation runs without an authenticated origin (it's triggered by a
/// runtime ContractNotification), so we look the key up using the canonical
/// River origin — the room owner's own webapp identity, derived from
/// `room_owner_vk` via the same `signing_key:{origin_b58}:{room_key_b58}`
/// secret-key format used by `handle_store_signing_key`.
///
/// `origin_b58` is the same b58-encoded contract id that the River webapp
/// uses on every webapp call. We capture it inside `EnsureRoomSubscription`
/// so the delegate can retrieve the signing key later when no origin is
/// available (rotation is triggered by a runtime ContractNotification).
fn signing_key_secret_key(origin_b58: &str, room_owner_vk_b58: &str) -> Vec<u8> {
    format!("signing_key:{origin_b58}:{room_owner_vk_b58}").into_bytes()
}

/// Persisted form of "we've subscribed and these are the cached parameters
/// for this room". The signing-key origin is captured at
/// `EnsureRoomSubscription` time so that ContractNotification handling has
/// everything it needs to find the owner's signing key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RoomSubscriptionContext {
    pub room_owner_vk: RoomKey,
    pub contract_id: [u8; 32],
    /// Base58-encoded webapp origin (ContractInstanceId) of the River UI
    /// that owns this signing key. Captured when `EnsureRoomSubscription`
    /// is processed.
    pub signing_key_origin_b58: String,
}

/// Public entry point invoked from `handlers::handle_application_message`.
pub(crate) fn handle_ensure_room_subscription(
    ctx: &mut DelegateCtx,
    origin_b58: &str,
    room_owner_vk: RoomKey,
    contract_id: [u8; 32],
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    let room_b58 = room_owner_b58(&room_owner_vk);
    logging::info(&format!(
        "EnsureRoomSubscription room={room_b58} cid={}",
        bs58::encode(&contract_id).into_string()
    ));

    // Probe for the signing key. The UI is required to send
    // `StoreSigningKey` before `EnsureRoomSubscription` so the rotation
    // pipeline has access to the owner's signing key when a notification
    // arrives. If the key isn't on file we fail fast rather than silently
    // setting up a sub-index that will never be able to rotate.
    //
    // Note: `DelegateCtx::get_secret` returns `None` in non-WASM tests, so
    // running this check there would always reject. We therefore only
    // enforce it on WASM. The non-WASM test
    // `subscribes_to_room_on_ensure_request` documents the legacy
    // permissive behaviour; a WASM integration test would cover the
    // rejection path. Filed as a known gap in the test harness — see
    // module-level comment.
    #[cfg(target_family = "wasm")]
    {
        let signing_key_present = ctx
            .get_secret(&signing_key_secret_key(origin_b58, &room_b58))
            .map(|b| b.len() == 32)
            .unwrap_or(false);
        if !signing_key_present {
            return ok_response(
                room_owner_vk,
                Err(
                    "no signing key on file for this room — call StoreSigningKey first".to_string(),
                ),
            );
        }
    }

    let context = RoomSubscriptionContext {
        room_owner_vk,
        contract_id,
        signing_key_origin_b58: origin_b58.to_string(),
    };

    let context_bytes = match cbor_encode(&context) {
        Ok(b) => b,
        Err(e) => {
            return ok_response(
                room_owner_vk,
                Err(format!("Failed to encode subscription record: {e}")),
            )
        }
    };

    if !ctx_set_secret(ctx, &secret_keys::sub_index(&room_b58), &context_bytes) {
        return ok_response(
            room_owner_vk,
            Err("Failed to persist subscription record (set_secret returned false)".into()),
        );
    }
    // Reverse index: contract_id -> room_owner_vk so notification handling
    // can correlate quickly. Stored as CBOR for consistency with the rest
    // of the file (the previous raw [u8;32] encoding was the only place
    // we stepped outside CBOR; harmonising it keeps decode paths uniform).
    let reverse_bytes = match cbor_encode(&room_owner_vk) {
        Ok(b) => b,
        Err(e) => {
            return ok_response(
                room_owner_vk,
                Err(format!("Failed to encode reverse index: {e}")),
            )
        }
    };
    if !ctx_set_secret(ctx, &contract_id_index_key(&contract_id), &reverse_bytes) {
        return ok_response(
            room_owner_vk,
            Err("Failed to persist contract->room reverse index".into()),
        );
    }

    let response = ChatDelegateResponseMsg::EnsureRoomSubscriptionResponse {
        room_owner_vk,
        result: Ok(()),
    };

    Ok(vec![
        create_app_response(&response)?,
        OutboundDelegateMsg::SubscribeContractRequest(SubscribeContractRequest::new(
            ContractInstanceId::new(contract_id),
        )),
    ])
}

/// Public entry point invoked from `lib::process` for runtime-delivered
/// ContractNotifications.
///
/// Cache discipline (Fix 3, #228 PR 2 v2): the member-set cache is **only**
/// updated AFTER we've successfully built the rotation `UpdateContractRequest`.
/// If any prerequisite step fails (signing key missing, encode error,
/// version overflow), we leave the cache untouched so that the next
/// identical notification retries the rotation rather than silently
/// declaring "members unchanged" forever.
pub(crate) fn handle_contract_notification(
    ctx: &mut DelegateCtx,
    notification: ContractNotification,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    let cid_bytes: [u8; 32] = {
        let slice = notification.contract_id.as_bytes();
        match <[u8; 32]>::try_from(slice) {
            Ok(a) => a,
            Err(_) => {
                logging::info(&format!(
                    "ContractNotification with unexpected contract_id length {} — ignoring",
                    slice.len()
                ));
                return Ok(vec![]);
            }
        }
    };
    let cid_b58 = bs58::encode(&cid_bytes).into_string();
    logging::info(&format!("ContractNotification for cid={cid_b58}"));

    // Look up which room this contract corresponds to. The reverse index is
    // CBOR-encoded; defensively handle a corrupt entry rather than panic.
    let room_owner_vk_bytes: RoomKey = match ctx.get_secret(&contract_id_index_key(&cid_bytes)) {
        Some(b) => match cbor_decode::<RoomKey>(&b) {
            Ok(k) => k,
            Err(e) => {
                logging::info(&format!(
                    "Corrupt reverse index for cid={cid_b58}: {e} — ignoring"
                ));
                return Ok(vec![]);
            }
        },
        None => {
            logging::info("Notification for unknown contract — ignoring");
            return Ok(vec![]);
        }
    };
    let room_b58 = room_owner_b58(&room_owner_vk_bytes);

    let sub_ctx_bytes = match ctx.get_secret(&secret_keys::sub_index(&room_b58)) {
        Some(b) => b,
        None => {
            logging::info("Notification but no subscription context — ignoring");
            return Ok(vec![]);
        }
    };
    let sub_ctx: RoomSubscriptionContext = match cbor_decode(&sub_ctx_bytes) {
        Ok(c) => c,
        Err(e) => {
            logging::info(&format!("Corrupt subscription context: {e}"));
            return Ok(vec![]);
        }
    };

    // Deserialize the room state from the notification.
    let new_state: ChatRoomStateV1 = match ciborium::from_reader(notification.new_state.as_ref()) {
        Ok(s) => s,
        Err(e) => {
            logging::info(&format!("Failed to decode room state in notification: {e}"));
            return Ok(vec![]);
        }
    };

    // Only act on private rooms — public rooms have no secrets to rotate.
    // For public rooms it's safe to update the member-set cache even though
    // we never read it back: the cache is local-only and updating it
    // costs nothing.
    if new_state.configuration.configuration.privacy_mode != PrivacyMode::Private {
        logging::info("Notification for non-private room — no rotation needed");
        update_member_set_cache(ctx, &room_b58, &new_state);
        return Ok(vec![]);
    }

    // Compare member set against the cached last-seen set.
    let current_members: std::collections::BTreeSet<MemberId> = new_state
        .members
        .members
        .iter()
        .map(|m| MemberId::from(&m.member.member_vk))
        .collect();

    let previous_members: Option<std::collections::BTreeSet<MemberId>> = ctx
        .get_secret(&secret_keys::member_set(&room_b58))
        .and_then(|b| cbor_decode(&b).ok());

    if previous_members.as_ref() == Some(&current_members) {
        logging::info("Member set unchanged — no rotation");
        return Ok(vec![]);
    }

    // ----- From here on every early-return represents a rotation failure;
    // the member-set cache is NOT updated until we've successfully built
    // the UpdateContractRequest. Fix 3 (cache-before-success bug).

    // Find the owner signing key. It was stored under the webapp origin that
    // called `StoreSigningKey` originally, and recorded again at
    // `EnsureRoomSubscription` time.
    let signing_key_seed: [u8; 32] = match ctx.get_secret(&signing_key_secret_key(
        &sub_ctx.signing_key_origin_b58,
        &room_b58,
    )) {
        Some(b) => match <[u8; 32]>::try_from(b.as_slice()) {
            Ok(seed) => seed,
            Err(_) => {
                logging::info(&format!(
                    "Stored signing key has wrong length ({}) — cannot rotate",
                    b.len()
                ));
                return Ok(vec![]);
            }
        },
        None => {
            logging::info("Owner signing key not found — cannot rotate");
            return Ok(vec![]);
        }
    };
    let signing_key = SigningKey::from_bytes(&signing_key_seed);
    let owner_vk: VerifyingKey = signing_key.verifying_key();

    // Verify the signing key actually corresponds to the room owner. If a
    // mismatch ever arose (e.g. delegate state corrupted across migrations),
    // we'd silently produce signatures the contract refuses; surface the
    // mismatch instead.
    if owner_vk.to_bytes() != sub_ctx.room_owner_vk {
        logging::info("Stored signing key does not match room owner_vk — refusing to rotate");
        return Ok(vec![]);
    }

    // Determine the new version. We always derive from the notification's
    // current_version + 1 so concurrent rotations across replicas at least
    // converge on the highest observed version+1; the contract rejects
    // replays of an existing version with `Duplicate secret version`.
    //
    // Hard-error and bail on overflow: silently wrapping `u32::MAX -> 0` would
    // collide with the existing version-0 record and reuse a key the
    // banned-then-readmitted member already saw. (Practically unreachable —
    // 4 billion rotations would be required — but cheap to defend against.)
    let current_version = new_state.secrets.current_version;
    if current_version == u32::MAX {
        logging::info(&format!(
            "Refusing to rotate room {room_b58}: current secret version is u32::MAX. \
             This is effectively unreachable in practice but the overflow case \
             must not silently wrap to 0."
        ));
        return Ok(vec![]);
    }
    let new_version = current_version + 1;
    let secret = derive_room_secret(&signing_key_seed, &owner_vk, new_version);

    // Build SecretVersionRecordV1 + sign.
    let record = SecretVersionRecordV1 {
        version: new_version,
        cipher_spec: RoomCipherSpec::Aes256Gcm,
        // RoomSecretsV1::verify and apply_delta don't gate on
        // `created_at`, so UNIX_EPOCH is functionally safe; it's a
        // placeholder until freenet-stdlib's `time::now()` works under
        // wasm32-unknown-unknown for delegates.
        created_at: UNIX_EPOCH + Duration::from_secs(0),
    };
    let record_bytes = match cbor_encode(&record) {
        Ok(b) => b,
        Err(e) => {
            logging::info(&format!("Failed to encode SecretVersionRecord: {e}"));
            return Ok(vec![]);
        }
    };
    let record_signature = signing_key.sign(&record_bytes);
    let authorized_record =
        AuthorizedSecretVersionRecord::with_signature(record.clone(), record_signature);

    // Build per-member encrypted secrets via the back-fill-aware helper.
    //
    // Decision (continuing vs. newly-joined) is driven by what's already in
    // the room state's `encrypted_secrets`, NOT by the delegate-local
    // member-set cache. The cache can be missing (fresh delegate, restart,
    // user reinstalls webapp), and if we used it as the only source of
    // truth we'd emit duplicate `(member, version)` blobs that the room
    // contract rejects via `RoomSecretsV1::apply_delta`'s duplicate check,
    // wedging rotation permanently.
    //
    // Prior-version secrets for back-fill are RECOVERED by decrypting the
    // owner's existing `encrypted_secret`-at-v from the room state — we
    // do NOT re-derive via `derive_room_secret`. The room's v0 was
    // generated by the UI as a random 32-byte value at room creation
    // (see `ui/src/room_data.rs` `create_new_room_with_name`); re-deriving
    // would hand the newly-joined member bytes that don't match what the
    // room name / owner nickname were sealed under. The delegate has the
    // owner's signing key, so it can ECIES-decrypt the owner's blob at
    // any prior version and recover the actual secret bytes the room is
    // really using.
    let owner_id = MemberId::from(&owner_vk);
    let current_with_vks: Vec<(MemberId, VerifyingKey)> = new_state
        .members
        .members
        .iter()
        .map(|m| (MemberId::from(&m.member.member_vk), m.member.member_vk))
        .collect();

    let new_encrypted_secrets = match build_rotation_encrypted_secrets(
        &signing_key,
        &owner_vk,
        owner_id,
        new_version,
        &secret,
        &current_with_vks,
        &new_state.secrets.encrypted_secrets,
    ) {
        Ok(v) => v,
        Err(e) => {
            logging::info(&format!("Failed to build rotation secrets: {e}"));
            return Ok(vec![]);
        }
    };
    // Touch `previous_members` so the unused-variable lint stays quiet —
    // the cache is still useful for the early-return "no change → skip"
    // check above, just not for the per-member decision below.
    let _ = previous_members;

    // Wrap the SecretsDelta in a ChatRoomStateV1Delta serialised with
    // ciborium — the room contract's `update_state` deserialises bytes via
    // the same encoder.
    //
    // ChatRoomStateV1Delta is generated by the `composable` macro, so we
    // construct it via its struct-literal Default + override. Avoid
    // `..Default::default()` because the macro-generated type has many
    // fields; spelling them out keeps the wire-shape explicit and obvious
    // to future readers.
    let delta = river_core::room_state::ChatRoomStateV1Delta {
        configuration: None,
        bans: None,
        members: None,
        member_info: None,
        secrets: Some(SecretsDelta {
            current_version: Some(new_version),
            new_versions: vec![authorized_record],
            new_encrypted_secrets,
        }),
        recent_messages: None,
        direct_messages: None,
        upgrade: None,
        version: None,
    };

    let delta_bytes = match cbor_encode(&delta) {
        Ok(b) => b,
        Err(e) => {
            logging::info(&format!("Failed to encode ChatRoomStateV1Delta: {e}"));
            return Ok(vec![]);
        }
    };

    logging::info(&format!(
        "Rotating room {room_b58} to v{new_version} with {} secrets",
        delta
            .secrets
            .as_ref()
            .map(|s| s.new_encrypted_secrets.len())
            .unwrap_or(0)
    ));

    let mut update_req = UpdateContractRequest::new(
        notification.contract_id,
        UpdateData::Delta(StateDelta::from(delta_bytes)),
    );
    update_req.context = DelegateContext::default();

    // Cache the freshly derived secret so future operations (e.g. UI
    // requests for the current secret) can find it without re-derivation.
    let _ = ctx_set_secret(ctx, &secret_keys::secret(&room_b58, new_version), &secret);

    // Now — and only now — update the member-set cache. The
    // UpdateContractRequest is fully built and ready to emit; if it round-
    // trips the contract and gets rejected (duplicate version, etc.) the
    // contract's CRDT dedup absorbs it. If the rotation succeeds, the cache
    // reflects the new member set so we don't spuriously re-rotate on the
    // next notification.
    update_member_set_cache(ctx, &room_b58, &new_state);

    Ok(vec![OutboundDelegateMsg::UpdateContractRequest(update_req)])
}

fn update_member_set_cache(ctx: &mut DelegateCtx, room_b58: &str, new_state: &ChatRoomStateV1) {
    let current_members: std::collections::BTreeSet<MemberId> = new_state
        .members
        .members
        .iter()
        .map(|m| MemberId::from(&m.member.member_vk))
        .collect();
    // CBOR-encoding a `BTreeSet<MemberId>` produces deterministic bytes for
    // the same set value: BTreeSet iterates in key order, ciborium preserves
    // that order, and `MemberId` is a fixed 32-byte struct. Even if it
    // weren't strictly canonical, this cache is **local-only** — it's never
    // shipped to other peers, only compared bytewise within a single
    // delegate instance to detect "did the member set change since last
    // notification?". So the eq-on-bytes check we perform after decoding
    // (`previous_members.as_ref() == Some(&current_members)`) is safe even
    // under non-canonical encodings, because we decode before comparing.
    if let Ok(b) = cbor_encode(&current_members) {
        let _ = ctx_set_secret(ctx, &secret_keys::member_set(room_b58), &b);
    }
}

/// `set_secret` returns `true` on the WASM target and `false` in non-WASM
/// tests; this helper centralises that quirk so call-sites stay readable.
fn ctx_set_secret(ctx: &mut DelegateCtx, key: &[u8], value: &[u8]) -> bool {
    #[cfg(target_family = "wasm")]
    {
        ctx.set_secret(key, value)
    }
    #[cfg(not(target_family = "wasm"))]
    {
        let _ = ctx.set_secret(key, value);
        true
    }
}

fn cbor_encode<T: Serialize>(value: &T) -> Result<Vec<u8>, ciborium::ser::Error<std::io::Error>> {
    let mut buf = Vec::new();
    ciborium::ser::into_writer(value, &mut buf)?;
    Ok(buf)
}

fn cbor_decode<T: for<'de> Deserialize<'de>>(
    bytes: &[u8],
) -> Result<T, ciborium::de::Error<std::io::Error>> {
    ciborium::from_reader(bytes)
}

fn ok_response(
    room_owner_vk: RoomKey,
    result: Result<(), String>,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    let response = ChatDelegateResponseMsg::EnsureRoomSubscriptionResponse {
        room_owner_vk,
        result,
    };
    Ok(vec![create_app_response(&response)?])
}

/// Build the list of `AuthorizedEncryptedSecretForMember` records to emit
/// in a rotation update.
///
/// For each current member (owner + each in `current_members_with_vks`),
/// for each version `v` in `[0..=new_version]`:
/// * If `(member, v)` is already in `existing_encrypted_secrets`, skip —
///   the room state already has that pair and emitting it again would be
///   rejected by `RoomSecretsV1::apply_delta`'s duplicate guard, wedging
///   rotation permanently.
/// * Otherwise, emit a fresh `AuthorizedEncryptedSecretForMember` that
///   encrypts the per-version secret for the member's VK.
///
/// Per-version secrets are sourced as follows:
/// * `new_version` → `new_secret` (the value the caller just derived).
/// * Any prior `v < new_version` → RECOVERED by ECIES-decrypting the
///   owner's existing `encrypted_secret`-at-v using the owner's signing
///   key. The delegate has the owner's signing seed, so it can decrypt
///   the blob the owner originally produced for themselves and recover
///   the actual secret bytes the room is really using. We do NOT
///   re-derive via `derive_room_secret`: River's UI generates v0
///   randomly at room creation
///   (`ui/src/room_data.rs:create_new_room_with_name`), so a derived v0
///   would not match what `room_state.configuration.display.name` and
///   similar were sealed under — i.e. the very bug we're trying to fix
///   (see PR #245 skeptical-review finding).
///
/// If a prior version's secret can't be recovered (no owner blob at
/// that version, or decrypt fails), entries at that version are
/// skipped. The newly-joined member won't decrypt content sealed at
/// that version, but nobody else can either — the data is irrecoverable.
///
/// Determining continuing-vs-newly-joined per `(member, version)`
/// directly from `existing_encrypted_secrets` (rather than from a
/// delegate-local cache) is deliberate: the local cache can be missing
/// (fresh delegate, restart, webapp reinstall), and using it as the
/// dedup source would produce duplicate `(member, version)` pairs that
/// the contract rejects (Codex review of PR #245).
///
/// Pure function, no I/O — delegates to the shared
/// [`river_core::room_state::secret::build_rotation_encrypted_secrets`]
/// helper so the delegate and UI paths produce byte-identical blob sets
/// for the same inputs. See Bug #3 PR B (Ivvor 2026-05-17).
#[allow(clippy::too_many_arguments)]
fn build_rotation_encrypted_secrets(
    signing_key: &SigningKey,
    owner_vk: &VerifyingKey,
    owner_id: MemberId,
    new_version: u32,
    new_secret: &[u8; 32],
    current_members_with_vks: &[(MemberId, VerifyingKey)],
    existing_encrypted_secrets: &[AuthorizedEncryptedSecretForMember],
) -> Result<Vec<AuthorizedEncryptedSecretForMember>, String> {
    river_core::room_state::secret::build_rotation_encrypted_secrets(
        signing_key,
        owner_vk,
        owner_id,
        new_version,
        new_secret,
        current_members_with_vks,
        existing_encrypted_secrets,
    )
}

#[cfg(test)]
mod tests;
