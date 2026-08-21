//! Delegate succession: handing this generation's secrets to the next one.
//!
//! # Why this exists
//!
//! A delegate's address is `BLAKE3(BLAKE3(wasm) ‖ params)`, so every rebuild
//! re-keys it and the successor opens an EMPTY secret store. River re-keys
//! roughly weekly. Today the UI recovers the data by asking each of ~30 past
//! generations in turn, which works only while those generations are still
//! installed and still executable, and which pulls every secret through the
//! browser in plaintext. Three generations (V4-V6) are already permanently
//! lost to a wire-format change that made their WASM unrunnable.
//!
//! This module is the other direction: the OUTGOING generation pushes its
//! store straight to the incoming one, inside the node.
//!
//! # The shape, and why it is this shape
//!
//! The predecessor pushes. It is never asked, and it never evaluates a claim
//! from anyone. It acts on exactly three inputs, none of which is an assertion
//! by the caller:
//!
//! 1. a signature it verifies itself, against [`AUTHOR_VK`] compiled in below;
//! 2. [`SUCCESSOR_PARAMS`], also compiled in — NEVER the params it was invoked
//!    with (see the warning on that constant, this is load-bearing);
//! 3. an address it COMPUTES from those two.
//!
//! There is no requester to authenticate because there is no request. A hostile
//! caller's complete set of powers is to withhold the record, or to replay an
//! older genuinely-signed one; [`FLOOR_KEY`] closes the second and the first is
//! a delay, not a compromise.
//!
//! **Direction is the whole security property.** The runtime returns a target
//! delegate's output to whoever drove the SENDER, so a pull ("successor asks
//! predecessor") hands the predecessor's plaintext to the driving app. Pushing
//! avoids that only because `set_secret` is a host function that emits no
//! message — NOT because the hop is private. That makes output discipline an
//! obligation on this code, not a property of the runtime: see
//! [`push_to_successor`], which must never place secret bytes in any outbound
//! message. Pinned by `receipt_carries_no_secret_bytes`.
//!
//! # INVARIANT: data flows old to new. Do not cross this.
//!
//! This design RESOLVES forward — the predecessor looks up who its successor
//! is — but DATA only ever moves old to new. The predecessor reads its own
//! store and an author-signed record; the successor reads a payload written by
//! OLDER code. So the standing requirement is the ordinary one, "new code reads
//! old data", which is the same promise River's contract migration already
//! makes.
//!
//! That is structural, not luck, and it is why this shape works where a
//! consumer that RESOLVED forward and then READ the newer artifact would not:
//! that one needs old code to read new data, which is a far stronger promise
//! and one nothing in River currently makes.
//!
//! **If a future change ever has the predecessor read something the successor
//! produced, or negotiate a shared format with it, the requirement flips to
//! forward compatibility and this stops being safe.** The one successor-produced
//! value here is the receipt, which travels to the driving app: keep it
//! advisory and additively-shaped, never something a predecessor consumes.
//!
//! # What this deliberately does not do
//!
//! * **It does not delete.** The predecessor keeps its copy forever. That is
//!   the entire recovery substrate: if the successor is broken, the author
//!   publishes a further record and every surviving generation hands forward to
//!   whoever it names then.
//! * **It does not run on hosted (multi-user) nodes.** Per-user secrets are
//!   encrypted under a key derived from the user's own session token, which is
//!   not available at rest, so they cannot be re-encrypted for a successor by
//!   anything but a live session. The UI does not trigger succession there and
//!   hosted users keep the session-driven path. A delegate cannot detect its own
//!   scope, so this is a guard in the caller rather than a boundary here.
//! * **It does not claim a complete store.** Host enumeration is best-effort
//!   (see [`EnumerationHealth`]). This reports the condition instead of
//!   pretending otherwise.

use ed25519_dalek::{Signature, VerifyingKey};
use freenet_stdlib::prelude::{
    ApplicationMessage, CodeHash, DelegateCtx, DelegateError, DelegateKey, DelegateMessage,
    OutboundDelegateMsg, Parameters,
};
use river_core::chat_delegate::{
    ChatDelegateResponseMsg, EnumerationHealth, RequestId, SuccessionOutcome, SuccessionRejection,
};

use crate::logging;

/// Domain separator the author signs under. Must match
/// `freenet_migrate::pointer::POINTER_SIGNING_DOMAIN` exactly — a mismatch
/// makes every genuine record fail verification. Pinned against the crate's own
/// constant by a test in the UI crate, which is where the two can be compared
/// without linking `freenet-migrate` into wasm.
const POINTER_SIGNING_DOMAIN: &[u8] = b"freenet-pointer/state-v1";

/// The app id this delegate's pointer record is published under. Together with
/// [`AUTHOR_VK`] this forms the record's params, and the params are covered by
/// the signature — which is what stops a record signed for one of the author's
/// apps being replayed into another.
const APP_ID: &[u8] = b"river.chat-delegate";

/// The author key whose signature authorizes a successor.
///
/// This is the single trust anchor of the whole mechanism. A generation
/// compiled with this key can only ever be succeeded under it: there is no way
/// to revoke it, and no higher authority to appeal to.
///
/// **Theft of the corresponding private key is terminal.** The version field is
/// a `u32` with a reserved maximum, so an attacker who signs once at the
/// ceiling names a successor that cannot be outbid — every surviving generation
/// will hand its secrets to them, permanently. That is an accepted, deliberate
/// property, not an oversight. It is the reason an author may choose to keep
/// this key separate from the one they publish releases with.
/// River's published author key, `river:v1:vk:9Ebskq4y7NvJpTQTrF1FAxU8g6bR4Rhe4TRikXba55EJ`
/// (bs58 of the 32 raw bytes below), as recorded in `pointer-records.toml` and
/// `FREENET.md`. Pinned against that file by
/// `author_vk_matches_published_pointer_records`, so the two cannot drift — a
/// delegate trusting a different key than the one records are signed with would
/// reject every genuine succession, silently.
///
/// This is River's PUBLISHING key, used on every release. That is the default
/// an author may choose (see the design doc, decision 9.1), and it carries a
/// stated cost: theft of it is terminal for succession, because a single
/// signature at the version ceiling names a successor that cannot be outbid.
/// An author wanting to separate the two publishes a second pointer record
/// under a different key and compiles that key in here instead.
const AUTHOR_VK: [u8; 32] = [
    122, 89, 122, 181, 53, 61, 153, 178, 47, 31, 157, 54, 101, 54, 88, 79, 164, 68, 35, 60, 127,
    160, 176, 173, 173, 107, 205, 85, 201, 115, 214, 131,
];

/// The params the successor is expected to run under.
///
/// **This MUST be a compile-time constant and MUST NOT be the params this
/// delegate was invoked with.** A delegate's `process()` only ever sees params
/// supplied by the CALLER, and the signed record commits to a code hash only —
/// so deriving the successor's address from invocation params lets a hostile
/// caller choose it. Since all delegate WASM is public, they would pre-register
/// the genuine successor code under params of their choosing, sit at the
/// address this delegate computes, and receive the whole store.
///
/// River's delegate params are empty, so this is empty. An author who changes
/// their delegate's params breaks succession silently — the predecessor derives
/// an address nobody is at and the push goes nowhere. Pinned by
/// `successor_params_are_not_taken_from_the_invocation`.
const SUCCESSOR_PARAMS: &[u8] = b"";

/// Where the highest accepted record version is kept.
///
/// This lives among this delegate's own secrets deliberately: an anti-replay
/// floor must be at least as durable as the data it guards, and co-located with
/// it. A floor that outlives its secrets is merely wasteful; one that dies
/// first is a hole. Storing it here also means a freshly re-keyed generation
/// starts at zero with an empty store — born together, so there is no window
/// where an attacker can replay against a floor that does not yet exist.
const FLOOR_KEY: &[u8] = b"__succession_floor__";

/// A pointer record is `version(u32 BE) ‖ code_hash(32) ‖ signature(64)`.
const RECORD_LEN: usize = 4 + 32 + 64;

/// `u32::MAX` is reserved by the pointer contract, so a record carrying it is
/// malformed rather than merely very new.
const RESERVED_VERSION: u32 = u32::MAX;

/// The host stops recording new keys past this many per scope. Enumeration then
/// silently omits the rest — see [`EnumerationHealth::AtCapacity`].
const HOST_ENUMERATION_CAP: u32 = 4096;

/// A parsed, signature-verified pointer record.
struct Record {
    version: u32,
    code_hash: [u8; 32],
}

/// Parse and verify a record. Every failure is a refusal to act, never a
/// partial action.
fn verify(record: &[u8]) -> Result<Record, SuccessionRejection> {
    if record.len() != RECORD_LEN {
        return Err(SuccessionRejection::MalformedRecord);
    }
    let version = u32::from_be_bytes([record[0], record[1], record[2], record[3]]);
    if version == RESERVED_VERSION {
        return Err(SuccessionRejection::MalformedRecord);
    }
    let mut code_hash = [0u8; 32];
    code_hash.copy_from_slice(&record[4..36]);
    let mut sig_bytes = [0u8; 64];
    sig_bytes.copy_from_slice(&record[36..100]);

    let vk = VerifyingKey::from_bytes(&AUTHOR_VK).map_err(|_| SuccessionRejection::BadSignature)?;

    // DOMAIN ‖ params ‖ version_be ‖ code_hash, where params is
    // author_vk ‖ app_id. The whole params blob is covered so a record signed
    // for one of this author's apps cannot be replayed into another.
    let mut msg = Vec::with_capacity(POINTER_SIGNING_DOMAIN.len() + 32 + APP_ID.len() + 4 + 32);
    msg.extend_from_slice(POINTER_SIGNING_DOMAIN);
    msg.extend_from_slice(&AUTHOR_VK);
    msg.extend_from_slice(APP_ID);
    msg.extend_from_slice(&version.to_be_bytes());
    msg.extend_from_slice(&code_hash);

    vk.verify_strict(&msg, &Signature::from_bytes(&sig_bytes))
        .map_err(|_| SuccessionRejection::BadSignature)?;

    if code_hash == [0u8; 32] {
        return Err(SuccessionRejection::Withdrawn);
    }
    Ok(Record { version, code_hash })
}

/// Highest record version this delegate has ever acted on.
fn floor(ctx: &DelegateCtx) -> u32 {
    match ctx.get_secret(FLOOR_KEY) {
        Some(b) if b.len() == 4 => u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
        _ => 0,
    }
}

/// Enumerate this delegate's own keys, and say how much to trust the answer.
///
/// The host's registry is populated only on write and is consulted
/// best-effort: it returns an empty list when unreadable, and stops recording
/// new keys past a per-scope ceiling. Neither is distinguishable from the
/// truth by a delegate, so both are reported rather than smoothed over.
fn enumerate(ctx: &DelegateCtx) -> (Vec<Vec<u8>>, EnumerationHealth) {
    let keys = ctx.list_secrets(b"");
    let health = if keys.is_empty() {
        // Transient and self-correcting: nothing was deleted, so a later
        // attempt sends whatever is visible then.
        EnumerationHealth::EmptyListing
    } else if keys.len() as u32 >= HOST_ENUMERATION_CAP {
        // NOT self-correcting: keys past the ceiling were never recorded.
        EnumerationHealth::AtCapacity {
            cap: HOST_ENUMERATION_CAP,
        }
    } else {
        EnumerationHealth::Ok
    };
    (keys, health)
}

/// Handle a `BeginSuccession` request.
pub(crate) fn handle_begin_succession(
    ctx: &mut DelegateCtx,
    request_id: RequestId,
    record: &[u8],
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    let mut out: Vec<OutboundDelegateMsg> = Vec::new();
    let outcome = match verify(record) {
        Err(reason) => {
            logging::info(&format!("Succession refused: {reason:?}"));
            SuccessionOutcome::Rejected(reason)
        }
        Ok(rec) if rec.version <= floor(ctx) => {
            SuccessionOutcome::Rejected(SuccessionRejection::NotNewer { seen: floor(ctx) })
        }
        // NOTE: there is deliberately no "the record names ME" check here.
        // A delegate has no way to learn its own code hash — nothing in the
        // host surface exposes it — so this cannot be decided in here. The
        // CALLER holds both halves and skips the call when the record already
        // names the running delegate. That is a liveness optimisation, not a
        // security check: if a caller does send it, the push is a self-delivery
        // that re-imports data we already hold, and the floor still advances
        // correctly so the next genuine record is honoured. Harmless, just
        // pointless. `SuccessionRejection::SameGeneration` exists for the
        // caller to report that case, not for this code to detect it.
        Ok(rec) => {
            let (outcome, push) = push_to_successor(ctx, &rec)?;
            out.push(push);
            outcome
        }
    };

    let payload = ciborium_vec(&ChatDelegateResponseMsg::SuccessionResponse {
        request_id,
        outcome,
    })?;
    // Receipt LAST. The push above carries the secrets and is delivered by the
    // runtime to the successor; this one goes back to the driving app and must
    // carry nothing but counts and status.
    out.push(OutboundDelegateMsg::ApplicationMessage(
        ApplicationMessage::new(payload).processed(true),
    ));
    Ok(out)
}

/// Enumerate, send, and record the floor.
///
/// # Output discipline
///
/// Everything this function returns is visible to the app that drove us. It
/// must therefore carry counts and status ONLY. The secrets travel in the
/// `DelegateMessage` payload, which the runtime delivers to the successor
/// without surfacing it to any client, and that is the ONLY reason this is
/// private. Putting so much as a key name in the receipt would leak it.
fn push_to_successor(
    ctx: &mut DelegateCtx,
    rec: &Record,
) -> Result<(SuccessionOutcome, OutboundDelegateMsg), DelegateError> {
    // `from_params` wants the code hash bs58-encoded, which is how `CodeHash`
    // renders itself.
    let successor = DelegateKey::from_params(
        CodeHash::new(rec.code_hash).encode(),
        &Parameters::from(SUCCESSOR_PARAMS.to_vec()),
    )
    .map_err(|e| DelegateError::Other(format!("cannot derive successor: {e}")))?;

    let (keys, health) = enumerate(ctx);
    let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(keys.len());
    for k in &keys {
        // The floor is ours, not the user's data — it must not travel, or a
        // successor would inherit a floor it did not earn.
        if k.as_slice() == FLOOR_KEY {
            continue;
        }
        if let Some(v) = ctx.get_secret(k) {
            pairs.push((k.clone(), v));
        }
    }
    let count = pairs.len() as u32;

    let payload = ciborium_vec(&pairs)?;
    // `sender` is a placeholder: the runtime overwrites it with the key of the
    // delegate that actually ran, which is what makes it unforgeable to the
    // receiver.
    let placeholder = DelegateKey::new([0u8; 32], CodeHash::new([0u8; 32]));
    let msg = DelegateMessage::new(successor, placeholder, payload);

    // Record the floor only after building the push. Re-running is safe and
    // idempotent, so failing before this point simply means the next attempt
    // tries again; failing after means we do not re-push a version we already
    // honoured.
    ctx.set_secret(FLOOR_KEY, &rec.version.to_be_bytes());

    logging::info(&format!(
        "Succession: pushing {count} secrets, health {health:?}"
    ));
    Ok((
        SuccessionOutcome::Pushed { count, health },
        OutboundDelegateMsg::SendDelegateMessage(msg),
    ))
}

fn ciborium_vec<T: serde::Serialize>(v: &T) -> Result<Vec<u8>, DelegateError> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(v, &mut out).map_err(|e| DelegateError::Other(format!("{e}")))?;
    Ok(out)
}

// =====================================================================
// Receiving half
// =====================================================================

/// Delegate keys of every generation this one is willing to inherit from.
///
/// Codegen'd from `legacy_delegates.toml` at build time, same source the UI's
/// sweep uses, so the two cannot drift.
///
/// This is an AUTHENTICATION gate, not an integrity gate: it establishes that
/// the sender is a genuine ancestor, NOT that what it sent was legitimately
/// written. Anyone able to reach this node's client API can already write into
/// any delegate under any app's compartment through the ordinary path, so the
/// import route grants no reach they did not have — but for exactly that reason
/// imported records must be treated as untrusted input, and the import path
/// must not validate LESS than the normal write path.
///
/// The generator emits `(delegate_key, code_hash)` pairs — the same view the
/// UI's sweep consumes. Only the key half is used here.
mod generated {
    include!(concat!(env!("OUT_DIR"), "/predecessors.rs"));
}
use generated::PREDECESSOR_DELEGATE_PAIRS;

/// Handle a push from a predecessor.
///
/// Refuses anything whose runtime-attested sender is not a known ancestor. The
/// sender field is overwritten by the runtime with the key of the delegate that
/// actually ran, so it cannot be forged by the sender's own code.
pub(crate) fn handle_inherited_secrets(
    ctx: &mut DelegateCtx,
    sender: &DelegateKey,
    payload: &[u8],
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    let sender_bytes = sender.bytes();
    if !PREDECESSOR_DELEGATE_PAIRS
        .iter()
        .any(|(key, _code_hash)| key.as_slice() == sender_bytes)
    {
        logging::info("Refusing inherited secrets: sender is not a known predecessor");
        return Err(DelegateError::Other(
            "sender is not a known predecessor of this delegate".into(),
        ));
    }

    let pairs: Vec<(Vec<u8>, Vec<u8>)> = ciborium::from_reader(payload)
        .map_err(|e| DelegateError::Deser(format!("inherited payload: {e}")))?;

    let mut imported = 0usize;
    let mut merged_indexes = 0usize;
    for (key, value) in pairs {
        if is_index_key(&key) {
            // MUST merge, never replace or skip. This is the failure ghostkeys
            // measured: an index is a single record listing what exists, so a
            // copier that skips it when one is already present leaves recovered
            // keys in storage that nothing will ever list. They are present and
            // permanently invisible. Thirteen of fourteen of their scenarios
            // diverged on this class, and no copy policy reproduces the right
            // answer — only merging through the app's own understanding does.
            merge_index(ctx, &key, &value)?;
            merged_indexes += 1;
        } else if ctx.get_secret(&key).is_none() {
            // Absent: take it. Present: the successor's own value is NEWER
            // than anything a predecessor holds, so it wins.
            let _ = ctx.set_secret(&key, &value);
            imported += 1;
        }
    }

    logging::info(&format!(
        "Inherited {imported} secrets, merged {merged_indexes} indexes"
    ));
    // Emits NOTHING. Output from here travels to whoever drove the PREDECESSOR,
    // so anything returned is client-visible. The predecessor reports the count
    // in its own receipt; this side stays silent rather than risk echoing a key
    // name or a value into that channel.
    Ok(vec![])
}

/// Is this the app's per-origin key index rather than user data?
fn is_index_key(key: &[u8]) -> bool {
    core::str::from_utf8(key).is_ok_and(|s| s.ends_with(crate::KEY_INDEX_SUFFIX))
}

/// Union an inherited index into the local one, preserving both sides.
fn merge_index(ctx: &mut DelegateCtx, key: &[u8], incoming: &[u8]) -> Result<(), DelegateError> {
    let inherited: crate::KeyIndex = match ciborium::from_reader(incoming) {
        Ok(v) => v,
        Err(e) => {
            // A predecessor's index that will not decode is NOT proof it held
            // nothing. Skip the merge and keep ours intact rather than
            // clobbering it with an empty one — the values themselves were
            // still imported above, and a later generation may decode it.
            logging::info(&format!("Inherited index did not decode, keeping ours: {e}"));
            return Ok(());
        }
    };
    let mut local: crate::KeyIndex = ctx
        .get_secret(key)
        .and_then(|d| ciborium::from_reader(d.as_slice()).ok())
        .unwrap_or_default();
    let before = local.keys.len();
    for k in inherited.keys {
        if !local.keys.contains(&k) {
            local.keys.push(k);
        }
    }
    if local.keys.len() != before {
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&local, &mut bytes)
            .map_err(|e| DelegateError::Deser(format!("index: {e}")))?;
        let _ = ctx.set_secret(key, &bytes);
    }
    Ok(())
}
