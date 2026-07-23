use crate::config::Config;
use crate::output::OutputFormat;
use crate::storage::Storage;
use anyhow::{anyhow, Result};
use chrono::{DateTime, Local, Utc};
use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_migrate::{NewestFirst, Outcome, ProbeDriver, ProbeStateOps, SelectionPolicy, Step};
use freenet_scaffold::ComposableState;
use freenet_stdlib::client_api::{
    ClientRequest, ContractRequest, ContractResponse, HostResponse, WebApi,
};
use freenet_stdlib::prelude::{
    ContractCode, ContractContainer, ContractInstanceId, ContractKey, ContractWasmAPIVersion,
    Parameters, UpdateData, WrappedContract, WrappedState,
};
use river_core::room_state::ban::{AuthorizedUserBan, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersDelta};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_core::room_state::privacy::{PrivacyMode, RoomDisplayMetadata, SealedBytes};
use river_core::room_state::upgrade::OptionalUpgradeV1;
use river_core::room_state::ChatRoomStateV1Delta;
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_tungstenite::connect_async;
use tracing::{debug, info, warn};

// Load the room contract WASM copied by build.rs
const ROOM_CONTRACT_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/room_contract.wasm"));

/// Timeout for the GET against the current room contract.
const CURRENT_GET_TIMEOUT: Duration = Duration::from_secs(30);
/// Per-probe timeout when searching older contract generations (freenet/river#292).
/// Kept short because a backward search may probe many generations; an existing
/// contract responds quickly, only an absent one runs the timeout down.
const LEGACY_PROBE_TIMEOUT: Duration = Duration::from_secs(8);
/// Timeout for a single hop while following an `OptionalUpgradeV1` pointer chain.
const UPGRADE_HOP_TIMEOUT: Duration = Duration::from_secs(15);
/// Maximum upgrade-pointer hops to follow before giving up — guards against a
/// cyclic or runaway chain.
const MAX_UPGRADE_HOPS: usize = 32;

/// Decide the next contract to follow from `state`'s upgrade pointer.
///
/// Returns `Some(next)` when `state` carries an `OptionalUpgradeV1` pointer to
/// a contract that is a *genuine forward upgrade* and not yet in `visited` —
/// and records it in `visited`. Returns `None` when:
///   * there is no pointer;
///   * the target is a **known** generation for this owner — the current
///     bundled key or any known legacy/backward key (see `known_keys`). A
///     pointer to a generation we already recognize is current-or-older, NOT a
///     real forward upgrade, so following it can only regress to stale state
///     (freenet/river#427); or
///   * the target is an already-visited contract (a self-pointer or a cycle).
///
/// Pure; the network GET is the caller's job. Extracted from
/// `follow_upgrade_chain` so the guards are unit-testable without a node
/// (freenet/river#292, #427).
fn next_upgrade_hop(
    state: &ChatRoomStateV1,
    visited: &mut HashSet<ContractInstanceId>,
    known_keys: &HashSet<ContractInstanceId>,
) -> Option<ContractInstanceId> {
    let authorized_upgrade = state.upgrade.0.as_ref()?;
    let next = ContractInstanceId::new(*authorized_upgrade.upgrade.new_chatroom_address.as_bytes());
    // A pointer to the current bundled key or to any known legacy/backward
    // generation is not a genuine forward upgrade — refuse to follow it
    // (freenet/river#427). Only a pointer to a generation we do NOT recognize
    // (a WASM newer than this build bundles) is a real forward hop.
    if known_keys.contains(&next) {
        return None;
    }
    // `HashSet::insert` returns false if `next` was already present — a cycle.
    visited.insert(next).then_some(next)
}

/// Fold a genuinely-forward upgrade generation's state into the authoritative
/// base state, keeping the base authoritative (freenet/river#427).
///
/// The read path used to REPLACE the base (current-generation) state with the
/// state a pointer led to (`state = next_state`). On a room whose current
/// generation carried a *backward* pointer that poisoning had planted, that
/// dropped every message the current key held. This instead CRDT-merges
/// `next_state` INTO a clone of `base`, mirroring the UI (which merges a
/// followed pointer's state into the authoritative current-key `RoomData`
/// rather than replacing it), so a followed subset can never drop content
/// already present on the base. On a merge failure the base is returned
/// unchanged rather than losing it.
fn fold_forward_upgrade_state(
    base: &ChatRoomStateV1,
    next_state: &ChatRoomStateV1,
    params: &ChatRoomParametersV1,
) -> ChatRoomStateV1 {
    let mut merged = base.clone();
    match merged.merge(base, params, next_state) {
        Ok(()) => {
            merged.recent_messages.rebuild_actions_state();
            merged
        }
        Err(e) => {
            warn!(
                "Failed to merge forward upgrade-pointer state ({e}); keeping the \
                 authoritative base state (freenet/river#427)"
            );
            base.clone()
        }
    }
}

/// Walk an `OptionalUpgradeV1` pointer chain forward from `id`, folding each
/// genuinely-forward generation into the authoritative base. `fetch` supplies a
/// generation's state (a network GET in production, a map lookup in tests), so
/// the walk logic is unit-testable without a node.
///
/// The walk cursor — the state whose upgrade pointer selects the NEXT hop —
/// advances to each **fetched** generation, NOT the merged accumulator. That is
/// the crux of freenet/river#427 P2-1: the accumulator keeps the base's own
/// pointer (a followed generation's onward pointer only merges when its version
/// is strictly higher, and River stamps every pointer version 1), so reading the
/// next hop from the accumulator would re-target the just-visited generation,
/// the visited-set would then stop the walk, and a room 2+ generations stale
/// would miss the newest generation. Reading the next hop from the fetched
/// generation walks base -> gen+1 -> gen+2 -> ... -> newest correctly while the
/// accumulator stays base-authoritative.
///
/// Bounded by [`MAX_UPGRADE_HOPS`] and a visited-set (cycle guard), and gated by
/// `known_keys` so a backward/self pointer is never followed. The returned `id`
/// is always the input `id` (the base generation): following is discovery-only,
/// so we never return or subscribe to a non-base generation.
async fn walk_upgrade_chain<'f>(
    mut state: ChatRoomStateV1,
    id: ContractInstanceId,
    known_keys: &HashSet<ContractInstanceId>,
    params: &ChatRoomParametersV1,
    mut fetch: impl FnMut(
        ContractInstanceId,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<ChatRoomStateV1>> + 'f>,
    >,
) -> (ChatRoomStateV1, ContractInstanceId) {
    let mut visited: HashSet<ContractInstanceId> = HashSet::new();
    visited.insert(id);
    // The state whose pointer selects the NEXT hop. Starts as the base and
    // advances to each fetched generation (freenet/river#427 P2-1).
    let mut walk_cursor = state.clone();
    for _ in 0..MAX_UPGRADE_HOPS {
        // `next_upgrade_hop` carries the no-pointer / known-generation /
        // self-pointer / cycle decision (pure, unit-tested).
        let Some(next) = next_upgrade_hop(&walk_cursor, &mut visited, known_keys) else {
            break;
        };
        match fetch(next).await {
            Some(next_state) => {
                info!(
                    "Followed genuine forward upgrade pointer to newer contract \
                     generation {next}; merging (not replacing) into the base state"
                );
                // Merge the newer generation INTO the base (base stays
                // authoritative; resolved id stays `id`), then advance the walk
                // cursor to the FETCHED generation so the next hop reads ITS
                // pointer, not the base's (freenet/river#427 P2-1).
                state = fold_forward_upgrade_state(&state, &next_state, params);
                walk_cursor = next_state;
            }
            None => break, // Pointer dangles; keep the best state we have.
        }
    }
    (state, id)
}

/// Strip any `OptionalUpgradeV1` forward pointer from `state`, returning the
/// cleaned clone (freenet/river#427).
///
/// The forward pointer is only meaningful on the OLD contract — it tells
/// stragglers still on that generation where the new one lives. Carrying it
/// FORWARD onto the new contract during migration poisons the new generation
/// with a pointer to an older one; a client that later reads the new key then
/// follows that pointer backward and (pre-#427) replaced fresh state with stale
/// state. Stripping the pointer at every forward-migration PUT stops new
/// poisoning at the source. This never removes a legitimate pointer already on
/// the new contract: the room contract's `OptionalUpgradeV1` delta only applies
/// a strictly-higher version, and a PUT carrying `None` emits no upgrade delta
/// at all.
fn strip_upgrade_pointer_for_forward_put(state: &ChatRoomStateV1) -> ChatRoomStateV1 {
    let mut cleaned = state.clone();
    cleaned.upgrade = OptionalUpgradeV1(None);
    cleaned
}

/// App-supplied state semantics for `freenet_migrate`'s backward-probe decision
/// driver (freenet/river#398 phase 2b): the CLI's rules for classifying a
/// legacy-generation response. Everything else (candidate order, single-shot
/// correlation, exhaustion) is owned by the driver; the per-candidate GET plus
/// its forward upgrade-chain walk stays in the I/O adapter (`probe_legacy_bytes`).
struct RiverCliProbeOps {
    /// The owner whose stranded room we're recovering.
    owner_vk: VerifyingKey,
}

impl ProbeStateOps for RiverCliProbeOps {
    type State = ChatRoomStateV1;

    fn decode(&self, bytes: &[u8]) -> Option<ChatRoomStateV1> {
        // The same deserialization as `try_get_state`: ciborium decode, then
        // rebuild the derived actions cache. `None` marks a miss so the driver
        // advances to the next generation rather than adopting garbage.
        let mut state: ChatRoomStateV1 = ciborium::de::from_reader(bytes).ok()?;
        state.recent_messages.rebuild_actions_state();
        Some(state)
    }

    fn is_real(&self, state: &ChatRoomStateV1) -> bool {
        // "Real" == the configuration signature verifies against the owner —
        // the same predicate `try_get_state` and the UI (`is_awaiting_initial_sync`)
        // use. A default / never-initialised contract fails it.
        //
        // Note: in THIS integration `probe_legacy_bytes` calls `try_get_state`,
        // which ALREADY drops non-real states (returns `None`), so bytes ever
        // reaching this `is_real` have already verified. This check is therefore
        // defensively redundant here — kept so the driver stays the sole
        // classifier of record if `try_get_state` ever stops verifying.
        state.configuration.verify_signature(&self.owner_vk).is_ok()
    }

    fn merge_with_local(
        &self,
        recovered: ChatRoomStateV1,
        _local: &ChatRoomStateV1,
    ) -> ChatRoomStateV1 {
        // riverctl recovery does NOT merge device-local state (unlike the UI):
        // it adopts the recovered network state verbatim. Preserved exactly from
        // the pre-driver loop, which returned the fetched state as-is. `_local`
        // is always the empty default handed to the driver.
        recovered
    }

    // `prepare_forward` stays the driver default (identity): `put_room_state`
    // already strips the upgrade pointer on the forward PUT (freenet/river#427),
    // so stripping here too would be redundant and would change what is returned
    // to the caller. Do NOT add a prepare_forward override.
}

/// App-supplied state semantics for the ACTIVE-migration probe
/// (freenet/river#442) — the search that finds the freshest state to migrate
/// forward when a room is stranded on an older contract generation. The
/// decode/classify rules are identical to the read/recovery path, so they
/// delegate to an inner [`RiverCliProbeOps`] (one classifier of record for
/// both paths). The ONE difference is `merge_with_local`: the active path
/// CRDT-merges the recovered network state with the device-local cache (so the
/// migrating PUT carries the freshest of both), whereas the read path adopts
/// network state verbatim.
struct RiverCliMigrationProbeOps {
    /// Supplies decode + is_real (delegated) and the owner for the merge params.
    inner: RiverCliProbeOps,
}

impl ProbeStateOps for RiverCliMigrationProbeOps {
    type State = ChatRoomStateV1;

    fn decode(&self, bytes: &[u8]) -> Option<ChatRoomStateV1> {
        self.inner.decode(bytes)
    }

    fn is_real(&self, state: &ChatRoomStateV1) -> bool {
        self.inner.is_real(state)
    }

    fn merge_with_local(
        &self,
        recovered: ChatRoomStateV1,
        local: &ChatRoomStateV1,
    ) -> ChatRoomStateV1 {
        // The active-migration merge (unlike the read path's pass-through):
        // CRDT-merge the recovered network state with the local cache so
        // neither a fresher network state nor unsynced local edits are lost.
        // Preserved exactly from the pre-driver `get_migration_state`:
        //   let mut merged = net_state.clone();
        //   merged.merge(&net_state, &params, local_state)  // Err => net_state
        let params = ChatRoomParametersV1 {
            owner: self.inner.owner_vk,
        };
        let mut merged = recovered.clone();
        match merged.merge(&recovered, &params, local) {
            Ok(()) => merged,
            Err(e) => {
                info!("Merge with local state failed ({e}); using network state alone");
                recovered
            }
        }
    }

    // `prepare_forward` stays the driver default (identity): the caller
    // (`migrate_room_to_new_contract`) strips the upgrade pointer at PUT time
    // (freenet/river#427), so the state returned to adopt keeps its pointer —
    // exactly the pre-driver behavior. Do NOT add a prepare_forward override.
}

/// Serialize a chain-walk-resolved state into the raw bytes the decision driver
/// consumes. The single production re-encode used by `probe_legacy_bytes`;
/// the round-trip tests encode through this SAME function so they exercise the
/// real production encoding, not a parallel copy. `None` on serialize failure
/// (a miss — the driver advances), matching the old `.ok()?`.
fn encode_probe_state(state: &ChatRoomStateV1) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(state, &mut bytes).ok()?;
    Some(bytes)
}

/// Map a completed backward-probe [`Outcome`] to the recovery result. Pure and
/// I/O-free so both arms are unit-testable without a live node — the async
/// forward migration stays in `fetch_room_state_with_recovery`.
///
/// * `Ok((state, current_id, migrate_forward))` — a generation was recovered;
///   `migrate_forward` reports whether the caller should PUT it onto the current
///   contract.
/// * `Err(..)` — nothing recovered (`SeedLocal` / `NoLegacy` / an already-taken
///   outcome). riverctl NEVER seeds device-local state, so every non-`Recovered`
///   outcome is the not-found error. The error TEXT is a stable contract callers
///   and scripts may match on — keep it verbatim (pinned by
///   `recovery_outcome_seed_local_and_no_legacy_map_to_not_found_err`).
fn resolve_legacy_recovery_outcome(
    outcome: Option<Outcome<ChatRoomStateV1>>,
    current_id: ContractInstanceId,
    legacy_count: usize,
) -> Result<(ChatRoomStateV1, ContractInstanceId, bool)> {
    match outcome {
        Some(Outcome::Recovered { merged, source, .. }) => {
            // `source` is a legacy id, never the current key (the legacy
            // registry excludes the current WASM — pinned by
            // `current_wasm_is_not_in_legacy_registry`), so this guard is
            // unreachable-true today. Kept as defence: if a future change ever
            // probed the current key, it prevents a pointless self-PUT.
            Ok((merged, current_id, source != current_id))
        }
        _ => Err(anyhow!(
            "Room not found on the current contract or any of the {legacy_count} known \
             previous contract generations. The room may never have existed, or its \
             state may have been garbage-collected from the network."
        )),
    }
}

/// The active-migration probe candidates, newest-first: the stored
/// `previous_contract_key` (the immediately-previous generation) FIRST, then
/// every known legacy generation newest-first
/// (`legacy_contract_keys_for_owner` reverses the oldest-first registry). This
/// preserves the pre-driver fast-path-then-deep-path order; under
/// `NewestFirstWins` the stored key short-circuits the deep scan on a hit.
/// Pure (no I/O) so the ordering is unit-testable.
///
/// The stored key is deliberately NOT de-duplicated against the registry: when
/// it equals a registry generation it appears TWICE, and that duplicate is
/// load-bearing. A probe "miss" is not only an absent state — it also covers a
/// transient send failure or an 8s `LEGACY_PROBE_TIMEOUT`. The pre-driver code
/// re-GET the stored key in the deep sweep after a fast-path miss, giving it a
/// second attempt; de-duping would drop that retry, so a single transient
/// timeout on the newest generation would advance to an OLDER one, migrate its
/// stale state forward, and clear `previous_contract_key` — stranding newer
/// network-only state on the skipped generation. `NewestFirstWins` never issues
/// the second GET after a fast-path HIT (the probe ends there), so the duplicate
/// only ever costs a retry after a miss — exactly the pre-driver semantics
/// (freenet/river#442 review finding).
///
/// Intentional consequence of probing the stored key FIRST: if it carries state
/// it is adopted under `NewestFirstWins` even over a NEWER registry generation
/// that also has state. In the normal case this is moot — the stored
/// `previous_contract_key` IS the immediately-previous (newest legacy)
/// generation — but if it were ever an older generation with state, the fast
/// path wins. That is deliberate: it preserves the pre-driver semantics, whose
/// fast path returned the stored-key GET before the deep sweep ran.
fn migration_candidate_ids(
    previous_contract_key_str: &Option<String>,
    room_owner_key: &VerifyingKey,
) -> Vec<ContractInstanceId> {
    let mut ids = Vec::new();
    // Fast path: the immediately-previous contract key recorded in storage.
    if let Some(prev_key_str) = previous_contract_key_str {
        match prev_key_str.parse::<ContractInstanceId>() {
            Ok(prev_id) => ids.push(prev_id),
            Err(e) => warn!("Stored previous_contract_key is not a valid contract id: {e}"),
        }
    }
    // Deep path: every known previous contract generation, newest-first. The
    // stored key is re-listed here when it is a registry generation (NOT
    // de-duped) so a transient miss on the fast-path attempt is retried before
    // the probe advances to an older generation (see the doc comment).
    for legacy_key in river_core::migration::legacy_contract_keys_for_owner(room_owner_key) {
        ids.push(*legacy_key.id());
    }
    ids
}

/// Map a completed active-migration probe [`Outcome`] to the state to migrate
/// forward. Pure and I/O-free so it is unit-testable without a live node.
///
/// Unlike the read path's [`resolve_legacy_recovery_outcome`] (which errors on
/// exhaustion — the CLI never seeds a read), the active path FALLS BACK to the
/// local cache when nothing is reachable on-network, exactly as the pre-driver
/// `get_migration_state` did (`Ok(local_state.clone())`):
///
/// * `Recovered { merged }` — a generation had real state; `merged` is that
///   network state CRDT-merged with the local cache (see
///   [`RiverCliMigrationProbeOps::merge_with_local`]).
/// * `SeedLocal { local }` / `NoLegacy { local }` — every candidate missed (or
///   there were none): migrate the local cache forward.
/// * `None` — defensive (an already-taken outcome): the local cache.
fn resolve_migration_state_outcome(
    outcome: Option<Outcome<ChatRoomStateV1>>,
    local_state: &ChatRoomStateV1,
) -> ChatRoomStateV1 {
    match outcome {
        Some(Outcome::Recovered { merged, .. }) => merged,
        Some(Outcome::SeedLocal { local }) | Some(Outcome::NoLegacy { local }) => local,
        None => local_state.clone(),
    }
}

/// Compute the contract key for a room from its owner verifying key.
/// This uses the current bundled WASM to ensure consistency.
pub fn compute_contract_key(owner_vk: &VerifyingKey) -> ContractKey {
    let params = ChatRoomParametersV1 { owner: *owner_vk };
    let params_bytes = {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&params, &mut buf).expect("Failed to serialize parameters");
        buf
    };
    let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
    ContractKey::from_params_and_code(Parameters::from(params_bytes), &contract_code)
}

/// Resolve a message's human-readable body for display.
///
/// `effective_text` only yields text for `Text`/`Reply` bodies (and any edited
/// content). Other *public* content — notably join events (`content_type = 4`,
/// `EventContentV1`) — is not encrypted but carries no "text" field, so
/// `effective_text` returns `None`. Such content is decoded to its own display
/// string ("joined the room" for a join event) instead of being mislabeled as
/// `<encrypted>`. Only genuinely private (encrypted) bodies fall back to
/// `<encrypted>`.
///
/// Before this helper, riverctl rendered join events as
/// `[nickname]: <encrypted>` because the display path conflated "no text
/// content" with "encrypted".
/// Public-only convenience wrapper (no decryption). Production display paths
/// always thread the room `secrets` via [`message_display_text_with_secrets`];
/// this 2-arg form is retained for the tests that exercise public content and
/// the genuine `<encrypted>` fallback (empty secrets).
#[cfg(test)]
pub(crate) fn message_display_text(
    room_state: &ChatRoomStateV1,
    msg: &river_core::room_state::message::AuthorizedMessageV1,
) -> String {
    message_display_text_with_secrets(room_state, msg, &HashMap::new())
}

/// Like [`message_display_text`], but able to decrypt a **private** (encrypted)
/// message body when the caller supplies the room's `secrets` map
/// (version → 32-byte AES-256-GCM key), as collected by
/// [`crate::private_room::collect_secrets_for_room`].
///
/// riverctl previously rendered every private-room message body as
/// `<encrypted>` because the display path only decoded *public* content — the
/// CLI could send into a private room but not read it back (a river↔XMPP bridge
/// saw `"content":"<encrypted>"` for every message). This mirrors the UI's
/// `decrypt_message_content`: public bodies decode directly; a private
/// Text/Reply body is decrypted with the secret matching its `secret_version`
/// and its plaintext returned. A body whose secret is unavailable (not a
/// member, or an older rotated-past version) still falls back to `<encrypted>`.
///
/// `secrets` is empty for a public room or a room not in local storage, in
/// which case behaviour is identical to the public-only path.
pub(crate) fn message_display_text_with_secrets(
    room_state: &ChatRoomStateV1,
    msg: &river_core::room_state::message::AuthorizedMessageV1,
    secrets: &HashMap<u32, [u8; 32]>,
) -> String {
    let raw = room_state
        .recent_messages
        .effective_text(msg)
        .or_else(|| decrypt_private_body_text(&msg.message.content, secrets))
        .unwrap_or_else(|| {
            msg.message
                .content
                .decode_content()
                .map(|decoded| decoded.to_display_string())
                .unwrap_or_else(|| "<encrypted>".to_string())
        });
    render_mentions_for_terminal(room_state, &raw)
}

/// Decrypt a **private** message body to its display text, mirroring the UI's
/// `decrypt_message_content` private branch. Returns `None` for a public body
/// (the caller decodes those directly), for a body whose `secret_version` has
/// no key in `secrets`, or on any decrypt/decode failure — so the caller falls
/// back to the public decode path / `<encrypted>`.
fn decrypt_private_body_text(
    content: &river_core::room_state::message::RoomMessageBody,
    secrets: &HashMap<u32, [u8; 32]>,
) -> Option<String> {
    use river_core::room_state::content::{
        ReplyContentV1, TextContentV1, CONTENT_TYPE_REPLY, CONTENT_TYPE_TEXT,
    };
    use river_core::room_state::message::RoomMessageBody;

    let RoomMessageBody::Private {
        content_type,
        ciphertext,
        nonce,
        secret_version,
        ..
    } = content
    else {
        return None;
    };
    let secret = secrets.get(secret_version)?;
    let plaintext =
        river_core::ecies::decrypt_with_symmetric_key(secret, ciphertext, nonce).ok()?;
    if *content_type == CONTENT_TYPE_TEXT {
        if let Ok(text) = TextContentV1::decode(&plaintext) {
            return Some(text.text);
        }
    }
    if *content_type == CONTENT_TYPE_REPLY {
        if let Ok(reply) = ReplyContentV1::decode(&plaintext) {
            return Some(reply.text);
        }
    }
    // Decrypted but not a known text-bearing content type: show the raw
    // plaintext rather than falling back to "<encrypted>" (matches the UI).
    Some(String::from_utf8_lossy(&plaintext).to_string())
}

/// Replace `@[name](rv:id)` mention tokens with `@<name>` for terminal display.
/// Prefers each member's *current* public nickname (so the rendered name
/// follows renames); falls back to the token's snapshot name when the member is
/// unknown or their nickname is encrypted (riverctl does not decrypt
/// private-room nicknames). Plain text without tokens is returned unchanged.
pub(crate) fn render_mentions_for_terminal(room_state: &ChatRoomStateV1, text: &str) -> String {
    river_core::mention::render_plaintext(text, |r| {
        // Resolve the mention token to a member_id first (identity lookup,
        // not a content read — safe to leave as a bare `.find()`), then fetch
        // that member's CANONICAL record for the nickname. A bare `.find()`
        // straight to the nickname could return a losing (e.g. stale) duplicate
        // record if the state holds more than one (#411 round 8 item A).
        room_state
            .member_info
            .member_info
            .iter()
            .find(|info| r.matches(info.member_info.member_id))
            .map(|info| info.member_info.member_id)
            .and_then(|id| room_state.member_info.canonical(id))
            .and_then(|info| info.member_info.preferred_nickname.as_public_bytes())
            .map(|bytes| String::from_utf8_lossy(bytes).to_string())
    })
}

/// Resolve a member's `preferred_nickname` [`SealedBytes`] to its display
/// string, decrypting a **private**-room sealed nickname with `secrets`.
///
/// Mirrors the UI (`unseal_bytes_with_secrets` at
/// `ui/src/components/members.rs`): a public nickname yields its plaintext; a
/// private one is decrypted with the version-matched room secret; when the
/// secret is unavailable it falls back to the sealed placeholder
/// (`[Encrypted: N bytes, vN]`) rather than showing raw ciphertext. `secrets`
/// is empty for a public room / a room not in local storage, so public
/// nicknames are unaffected.
///
/// Without this, riverctl rendered every private-room member as
/// `[Encrypted: N bytes, vN]` — and, worse, `send_reply` sealed that
/// placeholder into `ReplyContentV1.target_author_name`, persisting it to
/// contract state for every reader (including the UI).
/// The `author` an outgoing message from `signing_key` carries.
///
/// SINGLE SOURCE OF TRUTH, shared with `riverctl identity whoami`
/// (freenet/river#438) via `storage::self_identity_from`. whoami's entire
/// promise is that the ID it reports equals the `author` on this identity's
/// own messages, so a bridge can filter its own echo. Previously every send
/// site hand-inlined `MemberId::from(&signing_key.verifying_key())` (in three
/// different spellings), and whoami inlined a fourth copy — nothing but a
/// source-scrape kept them in step, and the scrape silently missed the two
/// paths that send actual chat messages.
///
/// Call this from EVERY site that sets `MessageV1::author`. Do not re-inline
/// the derivation: a new send path that derives `author` its own way breaks
/// whoami for that path only, which is invisible until a bridge starts
/// double-posting.
pub(crate) fn author_member_id(signing_key: &SigningKey) -> MemberId {
    MemberId::from(&signing_key.verifying_key())
}

pub(crate) fn unseal_nickname_display(
    nickname: &river_core::room_state::privacy::SealedBytes,
    secrets: &HashMap<u32, [u8; 32]>,
) -> String {
    river_core::ecies::unseal_bytes_with_secrets(nickname, secrets)
        .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
        .unwrap_or_else(|_| nickname.to_string_lossy())
}

/// Convert bare `@nickname` mentions in an outgoing message into full mention
/// tokens, resolving each against the room's current members. Only **public**
/// nicknames are matched (riverctl does not decrypt private-room nicknames) and
/// only an **unambiguous** exact (case-insensitive) match is linked — a name
/// shared by two members, an unknown `@word`, or a private-room nickname is
/// left as plain text. Already-encoded tokens pass through untouched. This is
/// the CLI counterpart to the UI's `@` autocomplete picker.
pub(crate) fn resolve_outgoing_mentions(room_state: &ChatRoomStateV1, text: &str) -> String {
    use std::collections::HashMap;
    // Lowercased nickname -> Some((id, canonical name)) if unique, None if shared.
    let mut by_name: HashMap<String, Option<(MemberId, String)>> = HashMap::new();
    for info in &room_state.member_info.member_info {
        if let Some(bytes) = info.member_info.preferred_nickname.as_public_bytes() {
            let name = String::from_utf8_lossy(bytes).to_string();
            let id = info.member_info.member_id;
            by_name
                .entry(name.to_lowercase())
                .and_modify(|e| {
                    if let Some((eid, _)) = e {
                        if *eid != id {
                            *e = None; // two members share this nickname → ambiguous
                        }
                    }
                })
                .or_insert(Some((id, name)));
        }
    }
    river_core::mention::resolve_typed_mentions(text, |name| {
        by_name.get(&name.to_lowercase()).cloned().flatten()
    })
}

/// Truncate a reply preview to at most [`REPLY_PREVIEW_MAX_CHARS`] characters
/// for display, appending `"..."` **only when characters were actually
/// dropped**. A preview that fits is shown verbatim; a clipped one carries a
/// visible marker so a reader (a terminal user or a JSON-consuming bridge) can
/// tell the quoted text was cut rather than ending there. Operates on `char`s,
/// not bytes, so a multi-byte / emoji preview never panics or splits a
/// codepoint.
///
/// Single source of truth for the truncation marker, so the human and JSON
/// renderings of reply context can't drift on whether/how a preview is clipped.
fn truncate_reply_preview(s: &str) -> String {
    const REPLY_PREVIEW_MAX_CHARS: usize = 50;
    let mut chars = s.chars();
    let mut preview: String = chars.by_ref().take(REPLY_PREVIEW_MAX_CHARS).collect();
    // `chars` is positioned just past the 50th char; a remaining char means we
    // dropped content, so flag the truncation. No remaining char → exact fit,
    // shown verbatim with no misleading ellipsis.
    if chars.next().is_some() {
        preview.push_str("...");
    }
    preview
}

/// Test-only sibling of [`reply_context_display`] that returns the RAW reply
/// preview (no `@[name](rv:id)` mention rendering). Production code never wants
/// this — it would leak raw mention syntax into a preview — so it exists solely
/// to unit-test the truncation boundary behaviour of [`truncate_reply_preview`]
/// on un-rendered input. Returns `None` for non-reply content (or content that
/// fails to decode).
#[cfg(test)]
fn reply_context(
    msg: &river_core::room_state::message::AuthorizedMessageV1,
) -> Option<(String, String)> {
    use river_core::room_state::content::{DecodedContent, CONTENT_TYPE_REPLY};
    if msg.message.content.content_type() != CONTENT_TYPE_REPLY {
        return None;
    }
    match msg.message.content.decode_content() {
        Some(DecodedContent::Reply(reply)) => {
            let preview = truncate_reply_preview(&reply.target_content_preview);
            Some((reply.target_author_name, preview))
        }
        _ => None,
    }
}

/// Like [`reply_context`], but with `@[name](rv:id)` mention tokens in the
/// quoted preview resolved to `@name` for terminal display (mirroring
/// `message_display_text`, so the reply preview and the message body render
/// mentions the same way). Markdown is left as-is, consistent with how the CLI
/// renders message bodies.
///
/// Mentions are resolved on the FULL stored preview *before* the display-length
/// truncation, so a mention token sitting near the cutoff isn't sliced into
/// raw `@[name](rv:..` syntax.
/// Public-only convenience wrapper (no decryption); test counterpart of
/// [`reply_context_display_with_secrets`]. Production paths thread the room
/// `secrets` so a private reply's sealed context decrypts.
#[cfg(test)]
pub(crate) fn reply_context_display(
    room_state: &ChatRoomStateV1,
    msg: &river_core::room_state::message::AuthorizedMessageV1,
) -> Option<(String, String)> {
    reply_context_display_with_secrets(room_state, msg, &HashMap::new())
}

/// Like [`reply_context_display`], but able to decrypt the reply context of a
/// **private** reply when the caller supplies the room's `secrets` map. A
/// private reply seals its `ReplyContentV1` (target author name + quoted
/// preview) alongside the reply text, so without the secret the whole reply
/// context is opaque and no `[reply to …]` prefix could be shown. Public
/// replies decode directly, exactly as before. Returns `None` for a non-reply,
/// or when a private reply's secret is unavailable / undecodable.
pub(crate) fn reply_context_display_with_secrets(
    room_state: &ChatRoomStateV1,
    msg: &river_core::room_state::message::AuthorizedMessageV1,
    secrets: &HashMap<u32, [u8; 32]>,
) -> Option<(String, String)> {
    use river_core::room_state::content::{DecodedContent, ReplyContentV1, CONTENT_TYPE_REPLY};
    use river_core::room_state::message::RoomMessageBody;
    if msg.message.content.content_type() != CONTENT_TYPE_REPLY {
        return None;
    }
    let reply = match msg.message.content.decode_content() {
        // Public reply — decoded directly.
        Some(DecodedContent::Reply(reply)) => reply,
        // Private reply — decrypt then decode the sealed ReplyContentV1.
        _ => {
            let RoomMessageBody::Private {
                ciphertext,
                nonce,
                secret_version,
                ..
            } = &msg.message.content
            else {
                return None;
            };
            let secret = secrets.get(secret_version)?;
            let plaintext =
                river_core::ecies::decrypt_with_symmetric_key(secret, ciphertext, nonce).ok()?;
            ReplyContentV1::decode(&plaintext).ok()?
        }
    };
    let rendered = render_mentions_for_terminal(room_state, &reply.target_content_preview);
    let preview = truncate_reply_preview(&rendered);
    Some((reply.target_author_name, preview))
}

/// Rebuild a room's message `actions_state` (edits / deletes / reactions) using
/// decrypted content for **private** action messages.
///
/// `ChatRoomStateV1`'s `apply_delta`/`merge` end with the *non-decrypting*
/// `rebuild_actions_state()`, which can only decode PUBLIC action messages —
/// every edit / delete / reaction carried by a PRIVATE action message is
/// dropped. So without this, a private-room edit shows the message's ORIGINAL
/// text and a private-room deletion never hides the message. This is the CLI
/// counterpart of the UI's `RoomData::rebuild_private_actions_state`
/// (`ui/src/room_data.rs`): it decrypts each private action body with the
/// version-matched room secret and re-derives `actions_state` from the
/// decrypted actions. No-op for a public room (its public rebuild is already
/// correct) or when `secrets` is empty.
fn rebuild_private_actions_state(
    room_state: &mut ChatRoomStateV1,
    secrets: &HashMap<u32, [u8; 32]>,
) {
    use river_core::room_state::message::{MessageId, RoomMessageBody};

    if secrets.is_empty() {
        return;
    }
    let decrypted: HashMap<MessageId, Vec<u8>> = room_state
        .recent_messages
        .messages
        .iter()
        .filter(|m| m.message.content.is_action())
        .filter_map(|m| match &m.message.content {
            RoomMessageBody::Private {
                ciphertext,
                nonce,
                secret_version,
                ..
            } => secrets
                .get(secret_version)
                .and_then(|s| {
                    river_core::ecies::decrypt_with_symmetric_key(s, ciphertext, nonce).ok()
                })
                .map(|plaintext| (m.id(), plaintext)),
            _ => None,
        })
        .collect();
    room_state
        .recent_messages
        .rebuild_actions_state_with_decrypted(&decrypted);
}

/// Whether a message seen by a monitor stream is brand new, an edit of one
/// already emitted, or unchanged since last emitted.
#[derive(Debug, PartialEq, Eq)]
enum EmitKind {
    New,
    Edited,
    Unchanged,
}

/// Decide how to surface a message in a monitor stream. `seen` maps a message's
/// dedup key to the effective content last emitted for it; a changed content
/// for an already-seen key means the message was edited. This is the core of
/// the monitor's edit detection (it previously keyed on identity only and so
/// never re-emitted an edited message). freenet/river — Rogue Worm report.
fn classify_seen(seen: &HashMap<String, String>, key: &str, content: &str) -> EmitKind {
    match seen.get(key) {
        None => EmitKind::New,
        Some(prev) if prev == content => EmitKind::Unchanged,
        Some(_) => EmitKind::Edited,
    }
}

/// Stable dedup key for a message in a monitor stream: its signature-derived
/// `MessageId`, NOT `author:time`. The id is unique per message and stable
/// across edits (an edit is a separate action message; the original message's
/// signature never changes), so two distinct messages from the same author with
/// an identical timestamp cannot collide. Keying on `author:time` instead would
/// let such a collision flip-flop forever as a spurious "edit" now that we
/// compare effective content. freenet/river — PR #322 review.
fn monitor_seen_key(msg: &river_core::room_state::message::AuthorizedMessageV1) -> String {
    msg.id().0 .0.to_string()
}

/// Whether a monitor stream should emit a deletion event for a now-deleted
/// message. True only if the message was previously surfaced to the stream
/// (`seen`) and a deletion hasn't already been emitted for it
/// (`deleted_emitted`). The caller has already confirmed the message is
/// deleted. Keeping this pure makes the one-shot / only-if-shown semantics
/// unit-testable. freenet/river#323.
fn should_emit_deletion(
    seen: &HashMap<String, String>,
    deleted_emitted: &HashSet<String>,
    key: &str,
) -> bool {
    seen.contains_key(key) && !deleted_emitted.contains(key)
}

/// Keys of pre-existing messages whose deletion must NOT be surfaced as a live
/// event, because the stream did not actually show them at startup. A deletion
/// is surfaced only for a message the stream displayed (or emitted live later);
/// every pre-existing non-action message NOT in `shown_keys` is recorded so its
/// later deletion is suppressed.
///
/// This is necessary because the subscribe path seeds `seen` with ALL
/// pre-existing non-action messages (so old messages aren't re-shown as new —
/// #173) while only DISPLAYING the last `initial_messages`. Without this, a
/// later deletion of a seen-but-never-shown message (e.g. `--subscribe` with the
/// default `initial_messages = 0`, which shows nothing) would emit a spurious
/// `delete` event for a message the user never saw. freenet/river#324 review
/// (external-model pass).
fn deletions_to_suppress_at_start(
    messages: &[river_core::room_state::message::AuthorizedMessageV1],
    shown_keys: &HashSet<String>,
) -> HashSet<String> {
    messages
        .iter()
        .filter(|m| !m.message.content.is_action())
        .map(monitor_seen_key)
        .filter(|key| !shown_keys.contains(key))
        .collect()
}

/// Stable fingerprint of a message's reactions, used by the monitor stream to
/// detect a reaction added or removed *after* the message was already streamed.
///
/// `classify_seen` keys only on a message's effective text, so a live reaction
/// change does not alter that fingerprint and never re-emits — a bridge would
/// silently miss it (freenet/river#325). Reactions live in
/// `actions_state.reactions` (`emoji -> [MemberId]`), separate from the message
/// body, so they need their own fingerprint.
///
/// The fingerprint is order-independent: both the emoji keys and each emoji's
/// reactor list are sorted before serialising, so a `HashMap`/`Vec` reordering
/// (which carries no semantic meaning) never registers as a change. It captures
/// both the set of emojis AND who reacted with each, so an actor swap that keeps
/// the count constant (A removes 👍, B adds 👍) still fingerprints as changed.
/// `None` (no reactions) and an empty map both yield the empty string, so they
/// compare equal.
///
/// Reaction labels are arbitrary attacker-controlled `String`s (`riverctl
/// message react` passes the CLI argument through unvalidated), so the encoding
/// MUST be unambiguous for ANY label — including ones containing delimiter-like
/// characters. We serialise the sorted `Vec<(label, sorted_ids)>` as JSON, whose
/// string-escaping makes `{"a":[1],"b":[2]}` and `{"a=1|b":[2]}` distinct (a
/// hand-rolled `|`/`=`/`,` separator scheme would collide them and silently drop
/// the change). The JSON is used only for equality comparison, never parsed.
fn reactions_fingerprint(reactions: Option<&HashMap<String, Vec<MemberId>>>) -> String {
    let Some(reactions) = reactions else {
        return String::new();
    };
    if reactions.is_empty() {
        return String::new();
    }
    let mut entries: Vec<(&str, Vec<i64>)> = reactions
        .iter()
        .map(|(emoji, reactors)| {
            let mut ids: Vec<i64> = reactors.iter().map(|m| m.0 .0).collect();
            ids.sort_unstable();
            (emoji.as_str(), ids)
        })
        .collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    // serde_json on a Vec of tuples with string + integer-array elements is
    // infallible; fall back to a Debug rendering only to avoid an unwrap (still
    // unambiguous, just less compact).
    serde_json::to_string(&entries).unwrap_or_else(|_| format!("{:?}", entries))
}

/// What the monitor stream should do with a message's current reactions
/// fingerprint, given what it last recorded for that message.
#[derive(Debug, PartialEq, Eq)]
enum ReactionEmit {
    /// The message has NOT been surfaced to the stream (never shown at start and
    /// never emitted live), so it isn't in `seen_reactions`. Do nothing — a
    /// reaction to a message the user never saw must not surface, matching the
    /// deletion path's "only for messages the stream displayed" rule (#324).
    NotSurfaced,
    /// Reactions changed since last recorded for a surfaced message — emit a
    /// `reaction` event and update the fingerprint.
    Changed,
    /// No change — nothing to do.
    Unchanged,
}

/// Pure decision for the monitor's reaction-change detection. `seen_reactions`
/// contains an entry ONLY for messages the stream has surfaced (shown at start,
/// or emitted live by `emit_new_and_edited`, which seeds the fingerprint as it
/// emits). A key absent from the map is therefore an unsurfaced message →
/// `NotSurfaced`; a present-but-equal fingerprint is `Unchanged`; a present
/// changed fingerprint is `Changed`. Mirrors `classify_seen` (edits) /
/// `should_emit_deletion` (deletions). freenet/river#325.
fn classify_reaction(
    seen_reactions: &HashMap<String, String>,
    key: &str,
    fingerprint: &str,
) -> ReactionEmit {
    match seen_reactions.get(key) {
        None => ReactionEmit::NotSurfaced,
        Some(prev) if prev == fingerprint => ReactionEmit::Unchanged,
        Some(_) => ReactionEmit::Changed,
    }
}

/// Choose the `member_info` nickname to publish when re-adding a member who
/// was pruned for inactivity (see [`ApiClient::build_rejoin_delta`]).
///
/// Restores the member's persisted nickname — sealed for a private room via
/// [`crate::private_room::seal_invitee_nickname`] — falling back to the generic
/// public `"Member"` placeholder when any of these hold:
/// - no nickname was persisted (rooms joined before the `self_nickname` field,
///   or an older `rooms.json`);
/// - a private room has no secret available, so sealing returns `None` (we must
///   never publish a plaintext nickname into a private room);
/// - the stored nickname's byte length exceeds the room's current
///   `max_nickname_size`. The contract's `MemberInfoV1::apply_delta` rejects the
///   ENTIRE rejoin delta (members + member_info together) when
///   `declared_len() > max_nickname_size`, so an over-long restored nickname
///   would block the member from rejoining at all. `declared_len()` is the
///   plaintext byte length for both public and sealed values, so comparing
///   `nick.len()` here matches the contract check exactly. The 6-byte `"Member"`
///   placeholder keeps the rejoin working (regression guard — Codex/skeptical
///   review of PR #321).
fn rejoin_preferred_nickname(
    room_state: &ChatRoomStateV1,
    signing_key: &SigningKey,
    invitation_secrets: &HashMap<u32, [u8; 32]>,
    self_nickname: Option<&str>,
) -> SealedBytes {
    let max_nickname_size = room_state.configuration.configuration.max_nickname_size;
    self_nickname
        .filter(|nick| nick.len() <= max_nickname_size)
        .and_then(|nick| {
            crate::private_room::seal_invitee_nickname(
                room_state,
                signing_key,
                invitation_secrets,
                nick,
            )
        })
        .unwrap_or_else(|| SealedBytes::public("Member".to_string().into_bytes()))
}

/// Build the initial `ChatRoomStateV1` for a brand-new room: the owner-signed
/// configuration and the owner's `member_info`, plus — for a **private** room —
/// the v0 room secret, its version record, and the owner-addressed ECIES secret
/// blob written into contract state so the owner can decrypt later. Returns the
/// state and `Some(secret)` for a private room (`None` for public).
///
/// Pure (no network / no `self`) so the private-room creation crypto is
/// unit-testable. Mirrors the UI's `create_new_room_with_name`
/// (`ui/src/room_data.rs`) field-for-field: `generate_room_secret` (a RANDOM
/// v0, never derived) → `encrypt_secret_for_member` for the owner →
/// `AuthorizedSecretVersionRecord` + `AuthorizedEncryptedSecretForMember`, then
/// name + nickname sealed with `encrypt_with_symmetric_key` under that secret.
fn build_new_room_state(
    signing_key: &SigningKey,
    name: &str,
    nickname: &str,
    private: bool,
) -> (ChatRoomStateV1, Option<[u8; 32]>) {
    let owner_vk = signing_key.verifying_key();
    let mut room_state = ChatRoomStateV1::default();

    let room_secret: Option<[u8; 32]> = if private {
        use river_core::ecies::{encrypt_secret_for_member, generate_room_secret};
        use river_core::room_state::privacy::RoomCipherSpec;
        use river_core::room_state::secret::{
            AuthorizedEncryptedSecretForMember, AuthorizedSecretVersionRecord,
            EncryptedSecretForMemberV1, SecretVersionRecordV1,
        };

        let secret = generate_room_secret();
        let (ciphertext, nonce, ephemeral) = encrypt_secret_for_member(&secret, &owner_vk);

        let version_record = SecretVersionRecordV1 {
            version: 0,
            cipher_spec: RoomCipherSpec::Aes256Gcm,
            created_at: std::time::SystemTime::now(),
        };
        room_state
            .secrets
            .versions
            .push(AuthorizedSecretVersionRecord::new(
                version_record,
                signing_key,
            ));

        let owner_secret = EncryptedSecretForMemberV1 {
            member_id: owner_vk.into(),
            secret_version: 0,
            ciphertext,
            nonce,
            sender_ephemeral_public_key: ephemeral.to_bytes(),
            provider: owner_vk.into(),
        };
        room_state
            .secrets
            .encrypted_secrets
            .push(AuthorizedEncryptedSecretForMember::new(
                owner_secret,
                signing_key,
            ));
        room_state.secrets.current_version = 0;

        Some(secret)
    } else {
        None
    };

    // Seal a metadata/identity field: AES-256-GCM under the v0 room secret for a
    // private room, plaintext-public for a public room.
    let seal = |plaintext: &[u8]| -> SealedBytes {
        match room_secret {
            Some(secret) => {
                use river_core::ecies::encrypt_with_symmetric_key;
                let (ciphertext, nonce) = encrypt_with_symmetric_key(&secret, plaintext);
                SealedBytes::Private {
                    ciphertext,
                    nonce,
                    secret_version: 0,
                    declared_len_bytes: plaintext.len() as u32,
                }
            }
            None => SealedBytes::public(plaintext.to_vec()),
        }
    };

    let config = Configuration {
        owner_member_id: owner_vk.into(),
        privacy_mode: if private {
            PrivacyMode::Private
        } else {
            PrivacyMode::Public
        },
        display: RoomDisplayMetadata {
            name: seal(name.as_bytes()),
            description: None,
        },
        ..Configuration::default()
    };
    room_state.configuration = AuthorizedConfigurationV1::new(config, signing_key);

    let owner_info = MemberInfo {
        member_id: owner_vk.into(),
        version: 0,
        preferred_nickname: seal(nickname.as_bytes()),
        deputies: Vec::new(),
    };
    room_state
        .member_info
        .member_info
        .push(AuthorizedMemberInfo::new(owner_info, signing_key));

    (room_state, room_secret)
}

/// Error returned when `accept_invitation` is asked to join a room the CLI
/// already has stored credentials for (issue freenet/river#308).
///
/// Re-accepting an invitation used to rebuild the `StoredRoomInfo` from
/// scratch and `insert` it, wholesale-clobbering the existing room's
/// `signing_key_bytes`, `self_authorized_member`, `invite_chain`,
/// `previous_contract_key`, and `self_nickname`. The most severe case is a
/// silent identity flip: re-accepting a *different* invitation for the same
/// room replaced the stored signing key, so every subsequent CLI command
/// authenticated with the wrong key. We refuse the re-accept instead — the
/// same posture `commands::identity::import_identity` already takes — and
/// point the user at `riverctl room leave` to opt into a deliberate replace.
///
/// Kept as a free function so the user-facing message is unit-testable
/// without a live Freenet node (the rest of `accept_invitation` requires
/// one). Mirrors the `rejoin_preferred_nickname` extraction discipline above.
fn reaccept_refusal_error(room_owner_vk: &VerifyingKey) -> anyhow::Error {
    let owner_key_str = bs58::encode(room_owner_vk.as_bytes()).into_string();
    anyhow!(
        "You already have an identity for room {owner_key_str}. Accepting this \
         invitation would overwrite your stored signing key, membership, and \
         nickname for that room. Leave it first with `riverctl room leave \
         {owner_key_str}` if you want to replace it."
    )
}

/// On-wire invitation artifact. **MUST stay byte-identical to the UI's
/// `ui::components::members::Invitation`** — both clients exchange these via
/// base58+CBOR and the encoded string is fingerprinted for processed-invite
/// dedup. Any new field here MUST also be added to the UI copy, and vice
/// versa. Filed against issue freenet/river#302 — see point 4 there for a
/// future consolidation pass into a single shared (non-WASM-compiled) type.
#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Invitation {
    pub room: VerifyingKey,
    pub invitee_signing_key: SigningKey,
    pub invitee: AuthorizedMember,
    /// The room's symmetric secrets, one `(version, secret)` per version the
    /// inviting member holds (issue freenet/river#302; the UI counterpart was
    /// added in #301). Lets the invitee decrypt a private room immediately on
    /// join, instead of being stuck on `[Encrypted: N bytes, vN]` until the
    /// room owner's chat-delegate back-fills an `encrypted_secrets` blob.
    /// Works even when a non-owner issues the invitation — the inviter
    /// already holds the secret; the room contract is untouched.
    ///
    /// Carried in plaintext, NOT ECIES-wrapped. That is not a confidentiality
    /// regression: the invitation already carries `invitee_signing_key` in
    /// the clear, so the whole artifact is a bearer credential — anyone who
    /// can read these bytes can already read everything the room secret
    /// protects. Plaintext also avoids decrypting attacker-influenced
    /// ciphertext on the join path (`river_core::ecies::decrypt` panics on a
    /// malformed blob, and the release build is `panic = "abort"`).
    ///
    /// Sorted ascending by version for deterministic CBOR encoding (the
    /// encoded string is fingerprinted for processed-invite dedup, so it must
    /// be stable across decode/re-encode cycles).
    ///
    /// Empty for public rooms and for invitations created before this field
    /// existed (`#[serde(default)]` keeps old links decodable).
    #[serde(default)]
    pub room_secrets: Vec<(u32, [u8; 32])>,
}

/// Hand-written `Debug` that REDACTS `room_secrets`. The derived `Debug` for
/// `[u8; 32]` is fully transparent, so `{:?}`-logging an `Invitation` (e.g.
/// `info!("...{:?}", invitation)`) would print every room-secret byte.
/// `room` and `invitee` are non-sensitive; `SigningKey`'s own `Debug` is
/// already non-exhaustive (it does not print the secret), so it is safe to
/// delegate to. Mirrors the UI's hand-written `Debug` in
/// `ui/src/components/members.rs` — keep the two in sync.
impl std::fmt::Debug for Invitation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Invitation")
            .field("room", &self.room)
            .field("invitee_signing_key", &self.invitee_signing_key)
            .field("invitee", &self.invitee)
            .field(
                "room_secrets",
                &format_args!("<{} room secret(s) redacted>", self.room_secrets.len()),
            )
            .finish()
    }
}

pub struct ApiClient {
    web_api: Arc<Mutex<WebApi>>,
    #[allow(dead_code)]
    config: Config,
    storage: Storage,
}

impl ApiClient {
    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    pub async fn new(node_url: &str, config: Config, config_dir: Option<&str>) -> Result<Self> {
        Self::new_with_signing_key_override(node_url, config, config_dir, None).await
    }

    /// Construct an [`ApiClient`] with an optional in-memory signing-key
    /// override. The override is propagated to [`Storage`] so every
    /// `get_room` resolves the signing key from the override rather than
    /// the per-room `signing_key_bytes`. See [`Storage::signing_key_override`]
    /// for the motivating scenario.
    pub async fn new_with_signing_key_override(
        node_url: &str,
        config: Config,
        config_dir: Option<&str>,
        signing_key_override: Option<SigningKey>,
    ) -> Result<Self> {
        // Use the URL as provided - it should already be in the correct format
        info!("Connecting to Freenet node at: {}", node_url);

        // Connect using tokio-tungstenite
        let (ws_stream, _) = connect_async(node_url)
            .await
            .map_err(|e| anyhow!("Failed to connect to WebSocket: {}", e))?;

        info!("WebSocket connected successfully");

        // Create WebApi instance
        let web_api = WebApi::start(ws_stream);

        let storage = Storage::new_with_override(config_dir, signing_key_override)?;

        Ok(Self {
            web_api: Arc::new(Mutex::new(web_api)),
            config,
            storage,
        })
    }

    pub async fn create_room(
        &self,
        name: String,
        nickname: String,
        private: bool,
    ) -> Result<(VerifyingKey, ContractKey)> {
        info!(
            "Creating {} room: {}",
            if private { "private" } else { "public" },
            name
        );

        // Generate signing key for the room owner
        let signing_key =
            SigningKey::from_bytes(&rand::Rng::gen::<[u8; 32]>(&mut rand::thread_rng()));
        let owner_vk = signing_key.verifying_key();

        // Build the initial room state (owner config + member_info), plus the v0
        // secret for a private room. Extracted as a pure helper so the
        // private-room creation crypto is unit-testable without a live node.
        let (room_state, room_secret) =
            build_new_room_state(&signing_key, &name, &nickname, private);

        // Persist the v0 secret locally (as an invitation secret) so the CLI can
        // decrypt this room's own content immediately, without a round-trip to
        // re-fetch and decrypt the owner blob.
        let created_invitation_secrets: HashMap<u32, [u8; 32]> = match room_secret {
            Some(secret) => HashMap::from([(0u32, secret)]),
            None => HashMap::new(),
        };

        // Generate contract key using ciborium for serialization (matching UI code)
        let parameters = ChatRoomParametersV1 { owner: owner_vk };
        let params_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&parameters, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize parameters: {}", e))?;
            buf
        };

        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        // Use the full ContractKey constructor that includes the code hash
        let contract_key = ContractKey::from_params_and_code(
            Parameters::from(params_bytes.clone()),
            &contract_code,
        );

        // Create contract container
        let contract_container = ContractContainer::from(ContractWasmAPIVersion::V1(
            WrappedContract::new(Arc::new(contract_code), Parameters::from(params_bytes)),
        ));

        // Create wrapped state using ciborium
        let state_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&room_state, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize room state: {}", e))?;
            buf
        };
        let wrapped_state = WrappedState::new(state_bytes);

        // Create PUT request - subscribe: true so we receive updates to our own room
        let put_request = ContractRequest::Put {
            contract: contract_container,
            state: wrapped_state,
            related_contracts: Default::default(),
            subscribe: true,
            blocking_subscribe: false,
        };

        let client_request = ClientRequest::ContractOp(put_request);

        // Send request
        let mut web_api = self.web_api.lock().await;
        web_api
            .send(client_request)
            .await
            .map_err(|e| anyhow!("Failed to send PUT request: {}", e))?;

        // Wait for response with a more generous timeout to handle network delays
        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(result) => result.map_err(|e| anyhow!("Failed to receive response: {}", e))?,
                Err(_) => return Err(anyhow!("Timeout waiting for PUT response after 60 seconds")),
            };

        match response {
            HostResponse::ContractResponse(contract_response) => {
                match contract_response {
                    ContractResponse::PutResponse { key } => {
                        info!("Room created successfully with contract key: {}", key.id());

                        // Verify the key matches what we expected
                        if key != contract_key {
                            return Err(anyhow!(
                                "Contract key mismatch: expected {}, got {}",
                                contract_key.id(),
                                key.id()
                            ));
                        }

                        // Store room info persistently (with the v0 secret for a
                        // private room so we can decrypt our own room immediately)
                        self.storage.add_room_with_invitation_secrets(
                            &owner_vk,
                            &signing_key,
                            room_state,
                            &contract_key,
                            created_invitation_secrets,
                        )?;

                        Ok((owner_vk, contract_key))
                    }
                    ContractResponse::UpdateNotification { key, .. } => {
                        // When subscribing on PUT, we may receive an UpdateNotification first
                        // This indicates the PUT succeeded and we're now subscribed
                        info!(
                            "Room created (received subscription update) with contract key: {}",
                            key.id()
                        );

                        // Verify the key matches what we expected
                        if key != contract_key {
                            return Err(anyhow!(
                                "Contract key mismatch: expected {}, got {}",
                                contract_key.id(),
                                key.id()
                            ));
                        }

                        // Store room info persistently (with the v0 secret for a
                        // private room so we can decrypt our own room immediately)
                        self.storage.add_room_with_invitation_secrets(
                            &owner_vk,
                            &signing_key,
                            room_state,
                            &contract_key,
                            created_invitation_secrets,
                        )?;

                        Ok((owner_vk, contract_key))
                    }
                    other => Err(anyhow!(
                        "Unexpected contract response type for PUT request: {:?}",
                        other
                    )),
                }
            }
            HostResponse::Ok => {
                // Some versions might return Ok for successful operations
                info!(
                    "Room created (Ok response) with contract key: {}",
                    contract_key.id()
                );

                // Store room info persistently
                self.storage
                    .add_room(&owner_vk, &signing_key, room_state, &contract_key)?;

                Ok((owner_vk, contract_key))
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    /// Republish a room contract to the network
    ///
    /// This re-PUTs the contract with its current state, making this node seed it again.
    /// Use this when the contract exists locally but isn't being served on the network.
    pub async fn republish_room(&self, room_owner_key: &VerifyingKey) -> Result<()> {
        info!(
            "Republishing room owned by: {}",
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get the room state from local storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found in local storage. Cannot republish without local state.")
        })?;
        let (_signing_key, room_state, _contract_key_str) = room_data;

        // Create parameters
        let parameters = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        let params_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&parameters, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize parameters: {}", e))?;
            buf
        };

        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        let contract_key = ContractKey::from_params_and_code(
            Parameters::from(params_bytes.clone()),
            &contract_code,
        );

        // Create contract container
        let contract_container = ContractContainer::from(ContractWasmAPIVersion::V1(
            WrappedContract::new(Arc::new(contract_code), Parameters::from(params_bytes)),
        ));

        // Serialize state. Strip any forward upgrade pointer before this FORWARD
        // PUT onto the current bundled key: `republish` PUTs locally-stored
        // state (which may carry an absorbed backward pointer) directly, without
        // going through `ensure_room_migrated`, so it is another poisoning
        // choke point (freenet/river#427 P2-3).
        let room_state = strip_upgrade_pointer_for_forward_put(&room_state);
        let state_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&room_state, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize room state: {}", e))?;
            buf
        };
        let wrapped_state = WrappedState::new(state_bytes);

        // Create PUT request with subscribe=true to start seeding
        let put_request = ContractRequest::Put {
            contract: contract_container,
            state: wrapped_state,
            related_contracts: Default::default(),
            subscribe: true,
            blocking_subscribe: false,
        };

        let client_request = ClientRequest::ContractOp(put_request);

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(client_request)
            .await
            .map_err(|e| anyhow!("Failed to send PUT request: {}", e))?;

        // Wait for response
        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(result) => result.map_err(|e| anyhow!("Failed to receive response: {}", e))?,
                Err(_) => return Err(anyhow!("Timeout waiting for PUT response after 60 seconds")),
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::PutResponse { key }) => {
                info!(
                    "Room republished successfully with contract key: {}",
                    key.id()
                );
                if key != contract_key {
                    return Err(anyhow!(
                        "Contract key mismatch: expected {}, got {}",
                        contract_key.id(),
                        key.id()
                    ));
                }
                Ok(())
            }
            HostResponse::Ok => {
                info!("Room republished successfully (Ok response)");
                Ok(())
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    /// Prepare a freshly-fetched `room_state` for **display** in a private
    /// room: collect the local member's decryption secrets and, in place,
    /// rebuild the message `actions_state` (edits/deletes/reactions) from the
    /// decrypted private action messages. Returns the secrets map so the caller
    /// can decrypt message *bodies* at render time via
    /// [`message_display_text_with_secrets`] / [`reply_context_display_with_secrets`].
    ///
    /// Returns an empty map (and leaves `room_state` untouched) for a public
    /// room or a room not in local storage — a non-member cannot decrypt, and
    /// the display path then behaves exactly as the pre-existing public-only
    /// path. Secret collection needs the fetched `room_state` (it decrypts the
    /// owner-signed `encrypted_secrets` blobs), so this is called per fetch.
    pub(crate) fn room_display_secrets(
        &self,
        room_owner_key: &VerifyingKey,
        room_state: &mut ChatRoomStateV1,
    ) -> HashMap<u32, [u8; 32]> {
        use river_core::room_state::privacy::PrivacyMode;

        if room_state.configuration.configuration.privacy_mode != PrivacyMode::Private {
            return HashMap::new();
        }
        // The signing key of the identity we joined this room as, from local
        // storage. Absent → we are not a stored member → we cannot decrypt.
        let Some((self_sk, _, _)) = self.storage.get_room(room_owner_key).ok().flatten() else {
            return HashMap::new();
        };
        let invitation_secrets = self
            .storage
            .get_invitation_secrets(room_owner_key)
            .unwrap_or_default();
        let secrets = crate::private_room::collect_secrets_for_room(
            room_state,
            &self_sk,
            &invitation_secrets,
        );
        rebuild_private_actions_state(room_state, &secrets);
        secrets
    }

    pub async fn get_room(
        &self,
        room_owner_key: &VerifyingKey,
        subscribe: bool,
    ) -> Result<ChatRoomStateV1> {
        // Ensure room is migrated to the current contract version before any GET.
        // This handles the case where bundled WASM changed (e.g., after a release)
        // and no other client has migrated the state to the new contract key yet.
        let contract_key = self.ensure_room_migrated(room_owner_key).await?;
        info!("Getting room state for contract: {}", contract_key.id());

        // Fetch the room state, recovering it across older contract-WASM
        // generations if the current contract has no state (freenet/river#292).
        let (room_state, found_id) = self
            .fetch_room_state_with_recovery(room_owner_key, *contract_key.id())
            .await?;

        info!(
            "Retrieved room state with {} messages",
            room_state.recent_messages.messages.len()
        );

        if subscribe {
            // Only ever subscribe to the CURRENT generation. `follow_upgrade_chain`
            // and `fetch_room_state_with_recovery` already resolve `found_id` to
            // the current key, but gate it explicitly so a future change can never
            // subscribe to a stale/older generation reached via an upgrade pointer
            // (freenet/river#427).
            let current_id = *contract_key.id();
            if found_id == current_id {
                self.subscribe_to_contract(found_id).await?;
            } else {
                warn!(
                    "Refusing to subscribe to resolved generation {found_id}: it is not the \
                     current contract {current_id} — subscribing only to the current \
                     generation (freenet/river#427)"
                );
                self.subscribe_to_contract(current_id).await?;
            }
        }

        // Self-heal the "Unknown member" condition (issue freenet/river#304):
        // if this member is in `members` but absent from `member_info`, they
        // render as "Unknown" to every other peer. This is the CLI counterpart
        // of the UI's GET-path `build_member_info_heal` trigger. Best-effort —
        // a heal failure must not fail the read/send command that fetched the
        // state, so on error we warn and fall back to the un-healed state.
        //
        // CRUCIAL: we REBIND `room_state` to the healed state the heal returns,
        // so callers (`send_message`, read commands, etc.) operate on the
        // repaired state. Otherwise a follow-up delta would be applied to the
        // pre-heal state and written back, dropping the just-healed entry
        // locally (Codex review on PR #358).
        let room_state = match self.heal_member_info(room_owner_key, room_state).await {
            Ok(healed) => healed,
            Err((unhealed, e)) => {
                warn!("member_info self-heal (issue #304) did not complete: {e}");
                unhealed
            }
        };

        Ok(room_state)
    }

    /// Detect and remediate the "Unknown member" condition for the current
    /// member (issue freenet/river#304): self present in `state.members` but
    /// absent from `state.member_info`. When detected, publish a standalone
    /// `member_info`-only UPDATE so other peers stop rendering this member as
    /// "Unknown", fold the same entry into the locally-stored state, AND return
    /// the healed state so the caller operates on the repaired copy.
    ///
    /// The CLI counterpart of the UI's `RoomData::build_member_info_heal`
    /// (`ui/src/room_data.rs`), driven from the same place: every GET of an
    /// existing room (see [`Self::get_room`]). The heal-decision logic lives in
    /// the pure [`crate::private_room::build_member_info_heal`] so it is
    /// unit-testable without a node; this method owns the storage lookup and
    /// the network publish.
    ///
    /// Takes `state` by value and returns it (possibly healed). Returns the
    /// state UNCHANGED when there is nothing to heal — the member is the owner,
    /// is not in `members`, already has a `member_info` entry, is a private-room
    /// member with no secret yet available to seal their nickname (in which case
    /// the heal defers rather than leak a plaintext nickname; a later GET retries
    /// once the secret has arrived), the room has no local credentials, or an
    /// active signing-key override selects a different identity than the stored
    /// one. On the success path the returned state already carries the healed
    /// `member_info` entry.
    ///
    /// On error returns `Err((state, error))` — handing the original state back
    /// so the caller can still use it (the heal is best-effort). A network-send
    /// failure is NOT an error here: the local state was already repaired and
    /// stored, so it is logged and the healed state is returned as `Ok`.
    async fn heal_member_info(
        &self,
        room_owner_key: &VerifyingKey,
        state: ChatRoomStateV1,
    ) -> std::result::Result<ChatRoomStateV1, (ChatRoomStateV1, anyhow::Error)> {
        // Load this room's stored identity + nickname + invitation secrets.
        // If the room isn't stored locally we have nothing to heal with.
        let storage = match self.storage.load_rooms() {
            Ok(s) => s,
            Err(e) => return Err((state, e)),
        };
        let key_str = bs58::encode(room_owner_key.as_bytes()).into_string();
        let Some(info) = storage.rooms.get(&key_str) else {
            return Ok(state);
        };
        let signing_key = self.storage.resolve_signing_key(&info.signing_key_bytes);
        let self_nickname = info.self_nickname.clone();
        let invitation_secrets = info.invitation_secrets.clone();

        // The persisted `self_nickname` and `invitation_secrets` belong to the
        // room's STORED identity (`info.signing_key_bytes`). When a
        // `--signing-key-file` / `RIVER_SIGNING_KEY_FILE` override selects a
        // DIFFERENT identity, those fields are not this member's — healing with
        // them would republish another member's nickname / private metadata
        // under the override key (Codex review on PR #358). We have no nickname
        // or secrets for the override identity, so skip the heal in that case.
        // (No override, or an override that matches the stored key, is fine.)
        if signing_key.to_bytes() != info.signing_key_bytes {
            debug!(
                "member_info self-heal (issue #304): skipping for room {key_str} — \
                 active signing-key override does not match the stored identity, so \
                 the persisted nickname/secrets are not this member's"
            );
            return Ok(state);
        }

        let Some(authorized_info) = crate::private_room::build_member_info_heal(
            &state,
            &signing_key,
            room_owner_key,
            &invitation_secrets,
            self_nickname.as_deref(),
        ) else {
            return Ok(state);
        };

        info!(
            "member_info self-heal (issue #304): republishing member_info for self in room {key_str} \
             (was in members but absent from member_info — rendered as \"Unknown\" to peers)"
        );

        // `build_member_info_heal` only returns `Some` for a member who SURVIVES
        // `post_apply_cleanup` (it simulates the cleanup and defers otherwise),
        // so both the network publish and the local fold below are safe from the
        // "heal prunes the member it is repairing" trap (Codex review on PR
        // #358): the standalone `member_info`-only UPDATE carries no
        // `MembersDelta`, and the member is anchored, so neither the network's
        // nor a local cleanup would drop them.
        //
        // We still fold the entry into the local `member_info` sub-state DIRECTLY
        // rather than via a full-state `ChatRoomStateV1::apply_delta`: the entry
        // is already validated (self-signed, length-clamped; contract acceptance
        // pinned by `heal_output_is_accepted_by_member_info_apply_delta`), so a
        // direct insert is correct and avoids re-running cleanup locally for no
        // reason.
        let mut healed_state = state.clone();
        healed_state
            .member_info
            .member_info
            .push(authorized_info.clone());
        healed_state
            .member_info
            .member_info
            .sort_by_key(|i| i.member_info.member_id);
        if let Err(e) = self
            .storage
            .update_room_state(room_owner_key, healed_state.clone())
        {
            return Err((state, e));
        }

        // The network publish is best-effort: the local state is already
        // repaired and stored, so a send failure must not discard the healed
        // state the caller will operate on. Log and return the healed state.
        let delta = ChatRoomStateV1Delta {
            member_info: Some(vec![authorized_info]),
            ..Default::default()
        };
        if let Err(e) = self.send_delta(room_owner_key, delta).await {
            warn!(
                "member_info self-heal (issue #304): local state repaired and stored, \
                 but publishing the member_info UPDATE failed (a later GET will retry): {e}"
            );
        }

        Ok(healed_state)
    }

    /// Fetch a room's state, recovering it across contract-WASM generations.
    ///
    /// The room contract key is `BLAKE3(room_contract.wasm, params)`, so every
    /// WASM upgrade moves the key. A room dormant across one or more upgrades
    /// has its live state stranded under an older-generation key. This:
    ///   1. GETs the current contract (walking any upgrade-pointer chain forward);
    ///   2. if that yields nothing, probes every known previous generation
    ///      newest-to-oldest until one returns state;
    ///   3. migrates a recovered state forward onto the current contract so the
    ///      room is no longer stranded.
    ///
    /// Returns the recovered state and the contract instance it should be
    /// subscribed to.
    async fn fetch_room_state_with_recovery(
        &self,
        room_owner_key: &VerifyingKey,
        current_id: ContractInstanceId,
    ) -> Result<(ChatRoomStateV1, ContractInstanceId)> {
        // 1. Current generation (plus any forward upgrade-pointer chain).
        if let Some((state, id)) = self
            .try_fetch_room(room_owner_key, current_id, CURRENT_GET_TIMEOUT)
            .await
        {
            return Ok((state, id));
        }

        // 2. Backward search across previous contract generations, driven by
        //    freenet_migrate's sans-IO decision driver. Candidates are the
        //    legacy keys, already newest-first BY CONSTRUCTION
        //    (`legacy_contract_keys_for_owner` reverses the oldest-first
        //    registry), so `assume_ordered` is sound; `NewestFirstWins` stops
        //    at the first real generation — the anti-rollback guarantee.
        let legacy_keys = river_core::migration::legacy_contract_keys_for_owner(room_owner_key);
        let legacy_count = legacy_keys.len();
        info!(
            "Room not present on current contract {current_id}; probing {legacy_count} previous \
             contract generation(s)"
        );

        // The raw driver (not the crate's `migrate_contract` pump) fits riverctl
        // best: `migrate_contract` consumes a `ContractLineageEntry` lineage,
        // but riverctl only has newest-first `ContractKey`s from
        // `river_core::migration`, and feeding the raw driver avoids fabricating
        // lineage values just to satisfy that signature. The pump below is the
        // same trivial loop `migrate_contract` runs.
        let candidate_ids: Vec<ContractInstanceId> = legacy_keys.iter().map(|k| *k.id()).collect();
        let mut driver = ProbeDriver::new(
            RiverCliProbeOps {
                owner_vk: *room_owner_key,
            },
            // The CLI never merges device-local state and never seeds it, so the
            // local snapshot is an empty default: `merge_with_local` returns the
            // recovered state verbatim, and exhaustion (SeedLocal) maps to the
            // not-found error below rather than PUTting anything.
            ChatRoomStateV1::default(),
            NewestFirst::assume_ordered(candidate_ids),
            SelectionPolicy::NewestFirstWins,
        )
        // The pre-driver loop probed EVERY legacy key unbounded; the driver's
        // default hop cap (64) would silently strand the oldest generations if
        // the registry ever exceeded it (27 today). Size the cap to fit the whole
        // registry so it only ever guards a runaway registry, never truncates a
        // real one.
        .with_max_hops(legacy_count.max(freenet_migrate::DEFAULT_MAX_PROBE_HOPS));

        // Trivial pump: one awaitable GET (with its forward chain-walk) per
        // candidate. `probe_legacy_bytes` is the I/O adapter — it returns the
        // resolved raw bytes (Ok(None) => a miss => the driver advances); the
        // driver owns the decode + is_real classification.
        // Loop while the driver hands out GETs; it yields `Step::Done` (exiting
        // the loop) once a generation is recovered or all are exhausted.
        let mut probes_run = 0usize;
        while let Step::Get(id) = driver.next_action() {
            probes_run += 1;
            match self.probe_legacy_bytes(room_owner_key, id).await {
                Some(bytes) => driver.on_response(id, &bytes),
                None => driver.on_timeout(id),
            }
        }

        // Pure outcome->result mapping (unit-tested): Recovered => the state +
        // whether to migrate forward; every non-Recovered outcome => the
        // not-found Err (the CLI never seeds).
        let (merged, resolved_id, migrate_forward) =
            resolve_legacy_recovery_outcome(driver.take_outcome(), current_id, legacy_count)?;
        info!(
            "Recovered room from a previous contract generation (probe {probes_run}/{legacy_count})"
        );
        // Migrate the recovered state forward onto the current contract so the
        // room is no longer stranded on an old generation. The current contract
        // was just confirmed empty/absent, so this creates it; the room
        // contract's CRDT merge keeps a concurrent migrator's write safe.
        if migrate_forward {
            match self.put_room_state(room_owner_key, &merged).await {
                Ok(()) => {
                    info!("Migrated recovered room forward onto current contract {current_id}")
                }
                Err(e) => {
                    warn!("Could not migrate recovered room forward (returning it anyway): {e}")
                }
            }
        }
        Ok((merged, resolved_id))
    }

    /// Probe one legacy generation for the decision driver: GET it (with the
    /// legacy timeout), walk any forward `OptionalUpgradeV1` pointer chain, and
    /// return the resolved state's raw bytes for the driver to classify. `None`
    /// = timeout / absent / empty / undecodable (a miss — the driver advances).
    ///
    /// This is `try_fetch_room` restructured to hand the driver RAW BYTES
    /// instead of a decoded state, so the driver owns the decode + is_real
    /// classification (`RiverCliProbeOps`). The chain-walk still happens here in
    /// the I/O adapter — the driver only replaces the newest-first legacy loop.
    async fn probe_legacy_bytes(
        &self,
        room_owner_key: &VerifyingKey,
        id: ContractInstanceId,
    ) -> Option<Vec<u8>> {
        let (state, _base_id) = self
            .try_fetch_room(room_owner_key, id, LEGACY_PROBE_TIMEOUT)
            .await?;
        // Re-serialize the chain-walk-resolved state so the driver decodes and
        // classifies it (a cheap round-trip; recovery is a rare CLI path). The
        // round-trip tests encode via this SAME `encode_probe_state`.
        encode_probe_state(&state)
    }

    /// GET a room state from `id`, then walk any `OptionalUpgradeV1` pointer
    /// chain forward to the newest generation that still has state. Returns
    /// `None` if `id` has no usable state.
    async fn try_fetch_room(
        &self,
        room_owner_key: &VerifyingKey,
        id: ContractInstanceId,
        timeout: Duration,
    ) -> Option<(ChatRoomStateV1, ContractInstanceId)> {
        let state = self.try_get_state(room_owner_key, id, timeout).await?;
        Some(self.follow_upgrade_chain(room_owner_key, state, id).await)
    }

    /// GET a `ChatRoomStateV1` from a contract instance, returning `None` for an
    /// absent contract, a timeout, an empty/default state, or a state whose
    /// bytes do not deserialize (an incompatible older generation).
    ///
    /// "No usable state" is defined as a `configuration` whose signature does
    /// not verify against `owner_vk` — the same predicate the UI uses
    /// (`RoomData::is_awaiting_initial_sync`). A real room always carries an
    /// owner-signed configuration; an absent or never-initialised contract
    /// does not.
    async fn try_get_state(
        &self,
        owner_vk: &VerifyingKey,
        id: ContractInstanceId,
        timeout: Duration,
    ) -> Option<ChatRoomStateV1> {
        let get_request = ContractRequest::Get {
            key: id,
            // Request the contract code: a legacy generation's contract may not
            // be cached on this node, and asking for the code lets the GET
            // resolve / cache it rather than failing. The pre-recovery
            // `get_room` used `true`; the recovery probes need the same.
            return_contract_code: true,
            subscribe: false,
            blocking_subscribe: false,
        };
        let mut web_api = self.web_api.lock().await;
        if web_api
            .send(ClientRequest::ContractOp(get_request))
            .await
            .is_err()
        {
            return None;
        }
        let recv = tokio::time::timeout(timeout, web_api.recv()).await;
        drop(web_api);
        match recv {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::GetResponse {
                state, ..
            }))) => match ciborium::de::from_reader::<ChatRoomStateV1, _>(&state[..]) {
                Ok(mut room_state) => {
                    // A real room always carries an owner-signed configuration;
                    // an absent / never-initialised contract does not.
                    if room_state.configuration.verify_signature(owner_vk).is_err() {
                        return None;
                    }
                    room_state.recent_messages.rebuild_actions_state();
                    Some(room_state)
                }
                Err(e) => {
                    // A state that doesn't deserialize means a genuine
                    // backwards-compat break in an older generation's
                    // `ChatRoomStateV1` — surface it rather than hiding it.
                    warn!("State at {id} did not deserialize ({e}); skipping generation");
                    None
                }
            },
            _ => None,
        }
    }

    /// Follow an `OptionalUpgradeV1` pointer chain forward from `id`, hop by hop,
    /// until a state has no upgrade pointer or a hop's target has no state.
    /// Bounded by [`MAX_UPGRADE_HOPS`] and a visited-set so a cyclic or
    /// self-referential pointer cannot loop forever (freenet/river#292, Part 3).
    ///
    /// Two defences against the stale-backward-pointer bug (freenet/river#427):
    ///   * A pointer is followed only when its target is a genuine forward
    ///     upgrade — NOT the current bundled key and NOT any known legacy
    ///     generation for this owner (`known_keys`). A backward pointer planted
    ///     by migration poisoning (e.g. the current generation carrying a
    ///     pointer to an *older* one) is therefore never chased.
    ///   * When a genuine forward pointer IS followed, the target's state is
    ///     CRDT-**merged** into the authoritative base — mirroring the UI —
    ///     rather than REPLACING it, so a followed subset can never drop content
    ///     already present on the base.
    ///
    /// The returned `id` is always the input `id` (the base generation): like
    /// the UI, pointer-following is discovery-only, so we never return or
    /// subscribe to a non-base generation.
    async fn follow_upgrade_chain(
        &self,
        room_owner_key: &VerifyingKey,
        state: ChatRoomStateV1,
        id: ContractInstanceId,
    ) -> (ChatRoomStateV1, ContractInstanceId) {
        // Generations we already recognize for this owner: the current bundled
        // key plus every known legacy key. A pointer to any of these is
        // current-or-older and must not be followed (freenet/river#427).
        let mut known_keys: HashSet<ContractInstanceId> = HashSet::new();
        known_keys.insert(*self.owner_vk_to_contract_key(room_owner_key).id());
        for legacy_key in river_core::migration::legacy_contract_keys_for_owner(room_owner_key) {
            known_keys.insert(*legacy_key.id());
        }

        let params = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };

        // The pure walk drives the cursor/merge/guard logic; this closure is the
        // only network-bound piece (the per-hop GET).
        walk_upgrade_chain(state, id, &known_keys, &params, |next| {
            Box::pin(self.try_get_state(room_owner_key, next, UPGRADE_HOP_TIMEOUT))
        })
        .await
    }

    /// PUT a room state onto the *current* room contract. Used to migrate a
    /// state recovered from an older generation forward (freenet/river#292).
    async fn put_room_state(
        &self,
        room_owner_key: &VerifyingKey,
        room_state: &ChatRoomStateV1,
    ) -> Result<()> {
        let parameters = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        let mut params_bytes = Vec::new();
        ciborium::ser::into_writer(&parameters, &mut params_bytes)
            .map_err(|e| anyhow!("Failed to serialize parameters: {e}"))?;
        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        let contract_container = ContractContainer::from(ContractWasmAPIVersion::V1(
            WrappedContract::new(Arc::new(contract_code), Parameters::from(params_bytes)),
        ));
        // Strip any forward upgrade pointer before PUTting FORWARD onto the
        // current contract: carrying it forward poisons the current generation
        // with a pointer to an older one (freenet/river#427).
        let state_to_put = strip_upgrade_pointer_for_forward_put(room_state);
        let mut state_bytes = Vec::new();
        ciborium::ser::into_writer(&state_to_put, &mut state_bytes)
            .map_err(|e| anyhow!("Failed to serialize room state: {e}"))?;
        let put_request = ContractRequest::Put {
            contract: contract_container,
            state: WrappedState::new(state_bytes),
            related_contracts: Default::default(),
            subscribe: false,
            blocking_subscribe: false,
        };
        let mut web_api = self.web_api.lock().await;
        web_api
            .send(ClientRequest::ContractOp(put_request))
            .await
            .map_err(|e| anyhow!("Failed to send PUT: {e}"))?;
        match tokio::time::timeout(Duration::from_secs(60), web_api.recv()).await {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::PutResponse { .. })))
            | Ok(Ok(HostResponse::Ok))
            | Ok(Ok(HostResponse::ContractResponse(ContractResponse::UpdateNotification {
                ..
            }))) => Ok(()),
            Ok(Ok(other)) => Err(anyhow!("Unexpected response to PUT: {other:?}")),
            Ok(Err(e)) => Err(anyhow!("Error receiving PUT response: {e}")),
            Err(_) => Err(anyhow!("Timeout during PUT")),
        }
    }

    /// Subscribe to a contract instance and wait for confirmation.
    async fn subscribe_to_contract(&self, id: ContractInstanceId) -> Result<()> {
        info!("Subscribing to contract {id} to receive updates");
        let subscribe_request = ContractRequest::Subscribe {
            key: id,
            summary: None,
        };
        let mut web_api = self.web_api.lock().await;
        web_api
            .send(ClientRequest::ContractOp(subscribe_request))
            .await
            .map_err(|e| anyhow!("Failed to send SUBSCRIBE request: {e}"))?;
        match tokio::time::timeout(Duration::from_secs(5), web_api.recv()).await {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::SubscribeResponse {
                subscribed,
                ..
            }))) => {
                if subscribed {
                    info!("Successfully subscribed to contract");
                    Ok(())
                } else {
                    Err(anyhow!("Failed to subscribe to contract"))
                }
            }
            Ok(Ok(_)) => Err(anyhow!("Unexpected response to SUBSCRIBE request")),
            Ok(Err(e)) => Err(anyhow!("Failed to receive subscription response: {e}")),
            Err(_) => Err(anyhow!(
                "Timeout waiting for SUBSCRIBE response after 5 seconds"
            )),
        }
    }

    pub async fn test_connection(&self) -> Result<()> {
        info!("Testing WebSocket connection...");

        // Send a simple disconnect request to test the connection
        let test_request = ClientRequest::Disconnect { cause: None };

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(test_request)
            .await
            .map_err(|e| anyhow!("Failed to send test request: {}", e))?;

        info!("Connection test successful");
        Ok(())
    }

    pub async fn create_invitation(&self, room_owner_key: &VerifyingKey) -> Result<String> {
        let invitation = self.build_invitation(room_owner_key)?;

        // Encode as base58
        let mut data = Vec::new();
        ciborium::ser::into_writer(&invitation, &mut data)
            .map_err(|e| anyhow!("Failed to serialize invitation: {}", e))?;
        let encoded = bs58::encode(data).into_string();

        Ok(encoded)
    }

    /// Build the [`Invitation`] artifact for `room_owner_key` from local
    /// storage. Shared by [`Self::create_invitation`] (which base58-encodes it
    /// into an `?invitation=…` code) and by `dm invite` (which CBOR-encodes it
    /// into a [`river_core::room_state::dm_body::InvitePayload`]), so both
    /// entry points produce byte-identical invitation payloads.
    ///
    /// The caller must be a member of the room with a stored signing key; the
    /// generated invitee signing key is a fresh bearer credential.
    pub(crate) fn build_invitation(&self, room_owner_key: &VerifyingKey) -> Result<Invitation> {
        info!(
            "Building invitation for room owned by: {}",
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get the room info from persistent storage
        let room_data = self.storage.get_room(room_owner_key)?
            .ok_or_else(|| anyhow!("Room not found in local storage. You must be a member of the room to create invitations."))?;
        let (signing_key, state, _contract_key) = room_data;

        // Generate a new signing key for the invitee
        let invitee_signing_key =
            SigningKey::from_bytes(&rand::Rng::gen::<[u8; 32]>(&mut rand::thread_rng()));
        let invitee_vk = invitee_signing_key.verifying_key();

        // Create the member entry for the invitee
        let member = Member {
            owner_member_id: (*room_owner_key).into(),
            member_vk: invitee_vk,
            invited_by: signing_key.verifying_key().into(),
        };

        // Sign the member entry with the inviter's key (room owner in this case)
        let authorized_member = AuthorizedMember::new(member, &signing_key);

        // Collect every room secret the CLI holds so the invitee can decrypt
        // the room immediately on join — without waiting for the owner
        // chat-delegate to back-fill an `encrypted_secrets` blob (issue
        // freenet/river#302, mirrors UI behavior from #301). Empty for public
        // rooms. The owner addresses an owner-signed blob to themselves at
        // every version, so this path works uniformly for owners and non-
        // owners; see the doc-comment on `collect_secrets_for_room` for why
        // we do NOT derive owner secrets via `derive_room_secret` here.
        //
        // Note: `state` is the LOCAL snapshot from `storage.get_room`, not a
        // fresh network GET. If the room rotated since the CLI last synced,
        // the invitation may omit `current_version`'s secret and the invitee
        // will then defer member_info — a fresh GET before invitation
        // creation is a possible future hardening.
        let invitation_secrets = self.storage.get_invitation_secrets(room_owner_key)?;
        let secrets = crate::private_room::collect_secrets_for_room(
            &state,
            &signing_key,
            &invitation_secrets,
        );
        let room_secrets = crate::private_room::collect_invitation_secrets(&secrets);

        // Create the invitation struct
        Ok(Invitation {
            room: *room_owner_key,
            invitee_signing_key,
            invitee: authorized_member,
            room_secrets,
        })
    }

    pub async fn accept_invitation(
        &self,
        invitation_code: &str,
        nickname: &str,
    ) -> Result<(VerifyingKey, ContractKey)> {
        // Decode the base58 invitation code, then defer to the struct-based
        // path shared with `dm accept` (which already holds a decoded
        // `Invitation` extracted from an invite DM's CBOR payload).
        let decoded = bs58::decode(invitation_code)
            .into_vec()
            .map_err(|e| anyhow!("Failed to decode invitation: {}", e))?;
        let invitation: Invitation = ciborium::de::from_reader(&decoded[..])
            .map_err(|e| anyhow!("Failed to deserialize invitation: {}", e))?;

        self.accept_invitation_struct(invitation, nickname).await
    }

    /// Accept a pre-decoded [`Invitation`]. This is the shared core of
    /// invitation acceptance, called both by [`Self::accept_invitation`]
    /// (which decodes the base58 `?invitation=…` code first) and by the
    /// `dm accept` command (which decodes the CBOR `Invitation` carried
    /// inside a [`river_core::room_state::dm_body::DirectMessageBody::Invite`]
    /// DM). Keeping a single body means the re-accept guard, room GET,
    /// invite-chain walk, secret persistence, and join-delta publish stay
    /// byte-identical across both entry points.
    pub async fn accept_invitation_struct(
        &self,
        invitation: Invitation,
        nickname: &str,
    ) -> Result<(VerifyingKey, ContractKey)> {
        info!("Accepting invitation with nickname: {}", nickname);

        let room_owner_vk = invitation.room;
        let contract_key = self.owner_vk_to_contract_key(&room_owner_vk);

        // Refuse to re-accept an invitation for a room we already have stored
        // credentials for (issue freenet/river#308). Re-accepting rebuilds the
        // `StoredRoomInfo` from scratch, silently clobbering the existing
        // `signing_key_bytes`, `self_authorized_member`, `invite_chain`,
        // `previous_contract_key`, and `self_nickname`. Bail out *before* the
        // network GET — same posture as `import_identity`. The user can opt
        // into a deliberate replace via `riverctl room leave <owner>`.
        if self.storage.get_room(&room_owner_vk)?.is_some() {
            return Err(reaccept_refusal_error(&room_owner_vk));
        }

        info!(
            "Invitation is for room owned by: {}",
            bs58::encode(room_owner_vk.as_bytes()).into_string()
        );
        info!("Contract key: {}", contract_key.id());

        // Perform a GET request to fetch the room state
        let get_request = ContractRequest::Get {
            key: *contract_key.id(),    // GET uses ContractInstanceId
            return_contract_code: true, // Request full contract to enable caching
            subscribe: false,           // We'll subscribe separately after GET succeeds
            blocking_subscribe: false,
        };

        let client_request = ClientRequest::ContractOp(get_request);

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(client_request)
            .await
            .map_err(|e| anyhow!("Failed to send GET request: {}", e))?;

        // Wait for response with timeout
        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(result) => {
                    tracing::info!("ACCEPT: received GET response");
                    result.map_err(|e| anyhow!("Failed to receive response: {}", e))?
                }
                Err(_) => return Err(anyhow!("Timeout waiting for GET response after 60 seconds")),
            };

        match response {
            HostResponse::ContractResponse(contract_response) => {
                match contract_response {
                    ContractResponse::GetResponse { state, .. } => {
                        info!("Successfully retrieved room state");

                        // Parse the actual room state from the response
                        let room_state: ChatRoomStateV1 = ciborium::de::from_reader(&state[..])
                            .map_err(|e| anyhow!("Failed to deserialize room state: {}", e))?;

                        info!(
                            "Room state retrieved: name={}, members={}, messages={}",
                            room_state
                                .configuration
                                .configuration
                                .display
                                .name
                                .to_string_lossy(),
                            room_state.members.members.len(),
                            room_state.recent_messages.messages.len()
                        );

                        // Validate the room state is properly initialized
                        if room_state.configuration.configuration.owner_member_id
                            == river_core::room_state::member::MemberId(
                                freenet_scaffold::util::FastHash(0),
                            )
                        {
                            return Err(anyhow!("Room state has invalid owner_member_id"));
                        }

                        // Compute invite chain before storing (walks up from invitee
                        // to owner through existing members — doesn't require the
                        // invitee to be in the members list)
                        let params = ChatRoomParametersV1 {
                            owner: room_owner_vk,
                        };
                        let invite_chain = room_state
                            .members
                            .get_invite_chain(&invitation.invitee, &params)
                            .unwrap_or_default();

                        // Persist any invitation-carried room secrets (issue
                        // freenet/river#302) alongside the room itself, so the
                        // CLI can decrypt private-room content across
                        // invocations without re-importing the invitation.
                        //
                        // Merge with any previously-persisted entries so a
                        // re-accept of an older invitation does not silently
                        // drop newer versions the CLI already holds — mirrors
                        // the UI's `extend()` semantics (see
                        // `crate::private_room::merge_invitation_secrets`
                        // for the rationale and the round-2 skeptical-review
                        // finding H1 on PR #303).
                        let invitation_secrets_map = crate::private_room::merge_invitation_secrets(
                            self.storage
                                .get_invitation_secrets(&room_owner_vk)
                                .unwrap_or_default(),
                            &invitation.room_secrets,
                        );

                        // Store credentials locally first
                        self.storage.add_room_with_invitation_secrets(
                            &room_owner_vk,
                            &invitation.invitee_signing_key,
                            room_state.clone(),
                            &contract_key,
                            invitation_secrets_map.clone(),
                        )?;

                        self.storage.store_authorized_member(
                            &room_owner_vk,
                            &invitation.invitee,
                            &invite_chain,
                        )?;

                        // Persist our chosen nickname so a later rejoin (after
                        // an inactivity prune) restores it instead of "Member".
                        self.storage
                            .update_self_nickname(&room_owner_vk, nickname)?;

                        // Immediately publish membership + join event atomically.
                        // The join event counts as a message, preventing
                        // post_apply_cleanup from pruning the new member.
                        let signing_key = &invitation.invitee_signing_key;
                        let self_id = author_member_id(signing_key);

                        // Build members delta: invitee + any missing invite chain members
                        let current_member_ids: HashSet<MemberId> = room_state
                            .members
                            .members
                            .iter()
                            .map(|m| m.member.id())
                            .collect();
                        let mut members_to_add = vec![invitation.invitee.clone()];
                        for chain_member in &invite_chain {
                            if !current_member_ids.contains(&chain_member.member.id()) {
                                members_to_add.push(chain_member.clone());
                            }
                        }
                        let members_delta = MembersDelta::new(members_to_add);

                        // Seal the invitee nickname — `SealedBytes::public` for
                        // a public room, AES-GCM at the room's current secret
                        // for a private room. Issue freenet/river#302; mirrors
                        // the UI's `seal_invitee_nickname` (PR #301). Returns
                        // `None` for a private room when neither the
                        // owner-signed contract blob nor the invitation
                        // artifact provides a secret at the room's
                        // `current_secret_version` — in that case we DEFER
                        // `member_info` rather than leak a plaintext nickname
                        // into a private room. The member surfaces as
                        // "Unknown" to other peers until a secret is back-
                        // filled and a future heal re-publishes member_info;
                        // see the UI's `build_member_info_heal` in
                        // `ui/src/room_data.rs` for the eventual remediation
                        // path (CLI counterpart filed as freenet/river#304).
                        let sealed_nickname = crate::private_room::seal_invitee_nickname(
                            &room_state,
                            signing_key,
                            &invitation_secrets_map,
                            nickname,
                        );
                        let member_info_delta = sealed_nickname.map(|sealed| {
                            let member_info = river_core::room_state::member_info::MemberInfo {
                                member_id: self_id,
                                version: 0,
                                preferred_nickname: sealed,
                                deputies: Vec::new(),
                            };
                            let authorized_info = river_core::room_state::member_info::AuthorizedMemberInfo::new_with_member_key(
                                member_info, signing_key,
                            );
                            vec![authorized_info]
                        });

                        if member_info_delta.is_none() {
                            tracing::warn!(
                                "Private room: no secret available at current_version {} \
                                 (owner blob not yet issued and invitation carries no matching \
                                 secret); deferring member_info — your nickname will not appear \
                                 to other members until a heal publishes it.",
                                room_state.secrets.current_version
                            );
                        }

                        // Build join event message
                        let join_message = river_core::room_state::message::MessageV1 {
                            room_owner: params.owner_id(),
                            author: self_id,
                            content: river_core::room_state::message::RoomMessageBody::join_event(),
                            time: std::time::SystemTime::now(),
                        };
                        let auth_join_message =
                            river_core::room_state::message::AuthorizedMessageV1::new(
                                join_message,
                                signing_key,
                            );

                        let delta = ChatRoomStateV1Delta {
                            recent_messages: Some(vec![auth_join_message]),
                            members: Some(members_delta),
                            member_info: member_info_delta,
                            ..Default::default()
                        };

                        // Apply locally for validation
                        let mut local_state = room_state.clone();
                        local_state
                            .apply_delta(&room_state, &params, &Some(delta.clone()))
                            .map_err(|e| anyhow!("Failed to apply join delta: {:?}", e))?;

                        // Update stored state
                        self.storage
                            .update_room_state(&room_owner_vk, local_state)?;

                        // Send delta to network
                        let delta_bytes = {
                            let mut buf = Vec::new();
                            ciborium::ser::into_writer(&delta, &mut buf)
                                .map_err(|e| anyhow!("Failed to serialize delta: {}", e))?;
                            buf
                        };

                        let update_request = ContractRequest::Update {
                            key: contract_key,
                            data: UpdateData::Delta(delta_bytes.into()),
                        };

                        web_api
                            .send(ClientRequest::ContractOp(update_request))
                            .await
                            .map_err(|e| anyhow!("Failed to send join delta: {}", e))?;

                        // Wait for update response
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(60),
                            web_api.recv(),
                        )
                        .await
                        {
                            Ok(Ok(HostResponse::ContractResponse(
                                ContractResponse::UpdateResponse { .. },
                            ))) => {
                                info!("Invitation accepted and membership published");
                            }
                            Ok(Ok(resp)) => {
                                tracing::warn!("Unexpected response after join delta: {:?}", resp);
                            }
                            Ok(Err(e)) => {
                                tracing::warn!("Error receiving join delta response: {}", e);
                            }
                            Err(_) => {
                                tracing::warn!("Timeout waiting for join delta response");
                            }
                        }

                        drop(web_api);

                        Ok((room_owner_vk, contract_key))
                    }
                    _ => Err(anyhow!("Unexpected contract response type")),
                }
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    pub fn owner_vk_to_contract_key(&self, owner_vk: &VerifyingKey) -> ContractKey {
        let parameters = ChatRoomParametersV1 { owner: *owner_vk };
        let params_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&parameters, &mut buf)
                .expect("Serialization should not fail");
            buf
        };
        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        // Use the full ContractKey constructor that includes the code hash
        ContractKey::from_params_and_code(Parameters::from(params_bytes), &contract_code)
    }

    /// Check if a room needs migration to a new contract version and perform it if needed.
    ///
    /// This is called automatically when accessing a room. If the bundled contract WASM
    /// has changed (e.g., bug fixes), this will:
    /// 1. Detect the contract key mismatch
    /// 2. Try GET on the new contract (someone else may have migrated)
    /// 3. If no state on new key, try GET from old contract key (previous_contract_key)
    /// 4. PUT the state to the new contract
    /// 5. Send upgrade pointer on old contract (for old-client compat)
    /// 6. Update local storage
    ///
    /// Any member can perform this migration — not just the owner.
    ///
    /// Returns the current contract key (possibly updated).
    pub async fn ensure_room_migrated(&self, room_owner_key: &VerifyingKey) -> Result<ContractKey> {
        let expected_key = self.owner_vk_to_contract_key(room_owner_key);

        // Check if we have this room locally
        let storage = self.storage.load_rooms()?;
        let owner_key_str = bs58::encode(room_owner_key.as_bytes()).into_string();
        let room_info = match storage.rooms.get(&owner_key_str) {
            Some(info) => info,
            None => {
                // Room not in local storage, no migration needed
                return Ok(expected_key);
            }
        };

        let signing_key = self
            .storage
            .resolve_signing_key(&room_info.signing_key_bytes);
        let room_state = room_info.state.clone();
        let previous_contract_key_str = room_info.previous_contract_key.clone();

        // Check if migration is needed. load_rooms() already regenerates the
        // contract_key to match the current WASM and saves the old key in
        // previous_contract_key. If previous_contract_key is None, the room
        // is already on the current contract version.
        if previous_contract_key_str.is_none() {
            return Ok(expected_key);
        }

        // Safe to unwrap: we returned early above when previous_contract_key_str is None.
        let prev_key_str = previous_contract_key_str.as_deref().unwrap();
        let new_key_display = expected_key.id().to_string();
        info!(
            "Room contract version changed, migrating: {} -> {}",
            &prev_key_str[..12.min(prev_key_str.len())],
            &new_key_display[..12.min(new_key_display.len())]
        );

        // Try to GET from the new contract first - maybe someone else already migrated
        let get_request = ContractRequest::Get {
            key: *expected_key.id(),
            return_contract_code: false,
            subscribe: false,
            blocking_subscribe: false,
        };

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(ClientRequest::ContractOp(get_request))
            .await
            .map_err(|e| anyhow!("Failed to check new contract: {}", e))?;

        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(10), web_api.recv()).await {
                Ok(Ok(resp)) => resp,
                Ok(Err(e)) => return Err(anyhow!("Failed to receive response: {}", e)),
                Err(_) => {
                    // Timeout - assume contract doesn't exist yet, we need to migrate
                    drop(web_api);
                    let state_to_migrate = self
                        .get_migration_state(
                            room_owner_key,
                            &room_state,
                            &previous_contract_key_str,
                        )
                        .await?;
                    let result = self
                        .migrate_room_to_new_contract(
                            room_owner_key,
                            &signing_key,
                            &state_to_migrate,
                            expected_key,
                        )
                        .await?;
                    // Send upgrade pointer on old contract
                    self.send_upgrade_pointer(
                        room_owner_key,
                        &signing_key,
                        &previous_contract_key_str,
                        &result,
                    )
                    .await;
                    self.clear_previous_contract_key(room_owner_key)?;
                    return Ok(result);
                }
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::GetResponse { .. }) => {
                // New contract exists — but may have incomplete state if it was seeded
                // before the old contract's full state was available.
                // Always PUT old state into new contract: the room contract uses CRDT
                // merge (additive only, no data loss), so this is safe and idempotent.
                // Skipping the merge when counts match would miss cases where old and
                // new have different message sets with the same count.
                info!("New contract already exists, merging old contract state");
                drop(web_api);

                if let Some(prev_key_str) = &previous_contract_key_str {
                    match self.get_state_from_contract(prev_key_str).await {
                        Ok(old_state) => {
                            info!("Got old contract state, PUTting into new contract");
                            match self
                                .migrate_room_to_new_contract(
                                    room_owner_key,
                                    &signing_key,
                                    &old_state,
                                    expected_key,
                                )
                                .await
                            {
                                Ok(key) => {
                                    self.storage.update_contract_key(room_owner_key, &key)?;
                                    self.clear_previous_contract_key(room_owner_key)?;
                                    // Upgrade pointer not sent here: the contract already
                                    // exists, so another migrator likely already sent it.
                                    // The CLI cannot send pointers anyway (needs full
                                    // ContractKey, not just instance ID).
                                    return Ok(key);
                                }
                                Err(e) => {
                                    // Don't clear previous_contract_key on failure —
                                    // preserving it allows retry on next run.
                                    warn!("Failed to merge old state into new contract: {}", e);
                                    self.storage
                                        .update_contract_key(room_owner_key, &expected_key)?;
                                    return Ok(expected_key);
                                }
                            }
                        }
                        Err(e) => {
                            warn!(
                                "Could not fetch old contract {} for merge: {}",
                                prev_key_str, e
                            );
                            // Old contract unreachable (GC'd, network issue). Clear
                            // previous_contract_key since we can't merge what doesn't exist.
                        }
                    }
                }

                self.storage
                    .update_contract_key(room_owner_key, &expected_key)?;
                self.clear_previous_contract_key(room_owner_key)?;
                Ok(expected_key)
            }
            _ => {
                // Contract doesn't exist, try to get state from old contract and migrate
                drop(web_api);
                let state_to_migrate = self
                    .get_migration_state(room_owner_key, &room_state, &previous_contract_key_str)
                    .await?;
                let result = self
                    .migrate_room_to_new_contract(
                        room_owner_key,
                        &signing_key,
                        &state_to_migrate,
                        expected_key,
                    )
                    .await?;
                // Send upgrade pointer on old contract
                self.send_upgrade_pointer(
                    room_owner_key,
                    &signing_key,
                    &previous_contract_key_str,
                    &result,
                )
                .await;
                self.clear_previous_contract_key(room_owner_key)?;
                Ok(result)
            }
        }
    }

    /// GET a ChatRoomStateV1 from a contract by instance ID string.
    async fn get_state_from_contract(&self, contract_id: &str) -> Result<ChatRoomStateV1> {
        let id: ContractInstanceId = contract_id
            .parse()
            .map_err(|e| anyhow!("Invalid contract key: {}", e))?;

        let get_request = ContractRequest::Get {
            key: id,
            return_contract_code: false,
            subscribe: false,
            blocking_subscribe: false,
        };

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(ClientRequest::ContractOp(get_request))
            .await
            .map_err(|e| anyhow!("Failed to send GET: {}", e))?;

        match tokio::time::timeout(std::time::Duration::from_secs(30), web_api.recv()).await {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::GetResponse {
                state, ..
            }))) => {
                let mut room_state = ciborium::de::from_reader::<ChatRoomStateV1, _>(&state[..])
                    .map_err(|e| anyhow!("Failed to deserialize state: {}", e))?;
                room_state.recent_messages.rebuild_actions_state();
                Ok(room_state)
            }
            Ok(Ok(other)) => Err(anyhow!("Unexpected response: {:?}", other)),
            Ok(Err(e)) => Err(anyhow!("Error receiving response: {}", e)),
            Err(_) => Err(anyhow!("Timeout getting contract state")),
        }
    }

    /// Find the freshest state to migrate forward, searching the network across
    /// contract generations and merging in the local cache.
    ///
    /// Tries, in order: the immediately-previous contract key recorded in
    /// storage; then every known previous contract generation newest-first
    /// (covers a room dormant across several WASM upgrades — freenet/river#292).
    /// Whatever network state is found is CRDT-merged with the local cache, so
    /// the migrating PUT carries the real network state rather than a possibly
    /// stale local snapshot. Falls back to the local cache only when nothing is
    /// reachable on-network.
    async fn get_migration_state(
        &self,
        room_owner_key: &VerifyingKey,
        local_state: &ChatRoomStateV1,
        previous_contract_key_str: &Option<String>,
    ) -> Result<ChatRoomStateV1> {
        // Candidates newest-first: the stored previous_contract_key first, then
        // the legacy registry — the same fast-path-then-deep-path order the
        // pre-driver search used. The stored key is re-listed (NOT deduped) so a
        // transient fast-path miss is retried before advancing to an older
        // generation (freenet/river#442 review finding; see migration_candidate_ids).
        let candidate_ids = migration_candidate_ids(previous_contract_key_str, room_owner_key);
        let candidate_count = candidate_ids.len();
        info!(
            "Searching {candidate_count} contract generation(s) for the freshest state to \
             migrate forward"
        );

        // The same sans-IO decision driver the read/recovery path runs
        // (freenet/river#398 phase 2b, #437), with the ACTIVE path's semantics:
        // `merge_with_local` is the REAL CRDT merge (`RiverCliMigrationProbeOps`),
        // and exhaustion falls back to the local cache rather than erroring.
        // `NewestFirstWins` stops at the first real generation — so the stored
        // key short-circuits the deep scan on a hit, preserving the fast path.
        let mut driver = ProbeDriver::new(
            RiverCliMigrationProbeOps {
                inner: RiverCliProbeOps {
                    owner_vk: *room_owner_key,
                },
            },
            // Folded into a hit via `merge_with_local`, and handed back verbatim
            // on exhaustion (SeedLocal / NoLegacy) — the pre-driver
            // `Ok(local_state.clone())` fallback.
            local_state.clone(),
            NewestFirst::assume_ordered(candidate_ids),
            SelectionPolicy::NewestFirstWins,
        )
        // Size the hop cap to the candidate set so the driver's default 64-hop
        // cap can never truncate a >64-generation registry (the pre-driver deep
        // path was unbounded), mirroring the read path.
        .with_max_hops(candidate_count.max(freenet_migrate::DEFAULT_MAX_PROBE_HOPS));

        // Trivial pump: one awaitable GET per candidate. `probe_migration_bytes`
        // is the I/O adapter (plain GET, no upgrade-chain walk — matching the
        // pre-driver active path); the driver owns decode + is_real + merge.
        while let Step::Get(id) = driver.next_action() {
            match self.probe_migration_bytes(room_owner_key, id).await {
                Some(bytes) => driver.on_response(id, &bytes),
                None => driver.on_timeout(id),
            }
        }

        // Pure outcome->state mapping (unit-tested): Recovered => the merged
        // state; SeedLocal / NoLegacy => the local cache migrated forward.
        let outcome = driver.take_outcome();
        match &outcome {
            Some(Outcome::Recovered { source, .. }) => {
                info!("Found state on a previous contract generation ({source}) for migration")
            }
            _ => info!("No network state on any contract generation; using local cached state"),
        }
        Ok(resolve_migration_state_outcome(outcome, local_state))
    }

    /// Probe one contract generation for the active-migration decision driver:
    /// GET it (with the legacy timeout) and return the resolved state's raw
    /// bytes for the driver to classify. `None` = timeout / absent / empty /
    /// undecodable (a miss — the driver advances).
    ///
    /// Unlike the read path's `probe_legacy_bytes`, this does NOT walk the
    /// forward upgrade chain: the pre-driver `get_migration_state` used a plain
    /// `try_get_state` per generation, and this preserves that exactly.
    async fn probe_migration_bytes(
        &self,
        room_owner_key: &VerifyingKey,
        id: ContractInstanceId,
    ) -> Option<Vec<u8>> {
        let state = self
            .try_get_state(room_owner_key, id, LEGACY_PROBE_TIMEOUT)
            .await?;
        // Re-serialize so the driver decodes + classifies it (a cheap round-trip;
        // migration is a rare CLI path), through the SAME `encode_probe_state`
        // the read path and the round-trip tests use.
        encode_probe_state(&state)
    }

    /// Send an upgrade pointer to the old contract key for old-client compatibility.
    /// Note: The CLI cannot send upgrade pointers because it only stores the contract
    /// instance ID (not the full ContractKey with code hash). The UI handles upgrade
    /// pointer sending since it has the full ContractKey from the in-memory migration.
    async fn send_upgrade_pointer(
        &self,
        _room_owner_key: &VerifyingKey,
        _signing_key: &SigningKey,
        _previous_contract_key_str: &Option<String>,
        _new_contract_key: &ContractKey,
    ) {
        // Upgrade pointer sending requires a full ContractKey (with code hash),
        // but CLI storage only preserves the contract instance ID string.
        // The UI handles this since it captures the full ContractKey before regeneration.
        // The critical migration path (GET old → PUT new) works without this.
    }

    /// Clear the previous_contract_key after successful migration.
    fn clear_previous_contract_key(&self, owner_vk: &VerifyingKey) -> Result<()> {
        // Single locked load→mutate→save so a concurrent invocation can't
        // clobber this clear (issue freenet/river#307).
        let owner_key_str = bs58::encode(owner_vk.as_bytes()).into_string();
        self.storage.mutate_rooms(|storage| {
            if let Some(room_info) = storage.rooms.get_mut(&owner_key_str) {
                room_info.previous_contract_key = None;
            }
            Ok(())
        })
    }

    /// Migrate a room to a new contract by PUTting the state
    async fn migrate_room_to_new_contract(
        &self,
        room_owner_key: &VerifyingKey,
        _signing_key: &SigningKey, // Kept for potential future use (e.g., signing migration metadata)
        room_state: &ChatRoomStateV1,
        new_contract_key: ContractKey,
    ) -> Result<ContractKey> {
        info!("Migrating room to new contract: {}", new_contract_key.id());

        let parameters = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        let params_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&parameters, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize parameters: {}", e))?;
            buf
        };

        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        let contract_container = ContractContainer::from(ContractWasmAPIVersion::V1(
            WrappedContract::new(Arc::new(contract_code), Parameters::from(params_bytes)),
        ));

        // Strip any forward upgrade pointer before PUTting FORWARD onto the new
        // contract: carrying it forward poisons the new generation with a
        // pointer to an older one (freenet/river#427).
        let state_to_put = strip_upgrade_pointer_for_forward_put(room_state);
        let state_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&state_to_put, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize room state: {}", e))?;
            buf
        };
        let wrapped_state = WrappedState::new(state_bytes);

        let put_request = ContractRequest::Put {
            contract: contract_container,
            state: wrapped_state,
            related_contracts: Default::default(),
            subscribe: true,
            blocking_subscribe: false,
        };

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(ClientRequest::ContractOp(put_request))
            .await
            .map_err(|e| anyhow!("Failed to send PUT for migration: {}", e))?;

        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(Ok(resp)) => resp,
                Ok(Err(e)) => return Err(anyhow!("Failed to receive migration response: {}", e)),
                Err(_) => return Err(anyhow!("Timeout during room migration")),
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::PutResponse { key }) => {
                info!("Room migrated successfully to: {}", key.id());
                // Update local storage with new contract key
                self.storage.update_contract_key(room_owner_key, &key)?;
                Ok(key)
            }
            HostResponse::Ok => {
                info!("Room migrated successfully (Ok response)");
                self.storage
                    .update_contract_key(room_owner_key, &new_contract_key)?;
                Ok(new_contract_key)
            }
            HostResponse::ContractResponse(ContractResponse::UpdateNotification {
                key, ..
            }) => {
                // PUT to an existing contract triggers an UpdateNotification (merge).
                // This is a successful migration.
                info!("Room migrated successfully via merge (UpdateNotification)");
                self.storage.update_contract_key(room_owner_key, &key)?;
                Ok(key)
            }
            _ => Err(anyhow!(
                "Unexpected response during migration: {:?}",
                response
            )),
        }
    }

    pub async fn list_rooms(&self) -> Result<Vec<crate::storage::RoomListing>> {
        self.storage.list_rooms()
    }

    /// [`Self::list_rooms`], reporting each room's `self_identity` as
    /// `signing_key` would sign (the inline `--signing-key` /
    /// `RIVER_SIGNING_KEY` case). `None` uses each room's effective stored key.
    pub async fn list_rooms_as(
        &self,
        signing_key: Option<&SigningKey>,
    ) -> Result<Vec<crate::storage::RoomListing>> {
        self.storage.list_rooms_as(signing_key)
    }

    /// Build a rejoin delta if the user has been pruned from the members list.
    /// Returns (members_delta, member_info_delta) if the user needs to re-add themselves.
    ///
    /// This serves as a fallback for the join event sent at invitation acceptance
    /// time — if the join event ages out of `recent_messages` and the member gets
    /// pruned before sending a regular message, this re-adds them on next send.
    ///
    /// Exposed `pub(crate)` so the `dm` subcommand can bundle the same rejoin
    /// pieces into a DM-bearing delta (Bug #1, reported by Ivvor on Matrix
    /// 2026-05-16) — without this, an invited-but-inactive sender's DM was
    /// silent-dropped by the contract.
    pub(crate) fn build_rejoin_delta(
        &self,
        room_state: &ChatRoomStateV1,
        room_owner_key: &VerifyingKey,
        signing_key: &SigningKey,
    ) -> (Option<MembersDelta>, Option<Vec<AuthorizedMemberInfo>>) {
        let self_vk = signing_key.verifying_key();

        // Owner doesn't need to re-add
        if self_vk == *room_owner_key {
            return (None, None);
        }

        // Already in members list
        if room_state
            .members
            .members
            .iter()
            .any(|m| m.member.member_vk == self_vk)
        {
            return (None, None);
        }

        // Try to get stored authorized member
        let storage = match self.storage.load_rooms() {
            Ok(s) => s,
            Err(_) => return (None, None),
        };
        let key_str = bs58::encode(room_owner_key.as_bytes()).into_string();
        let (authorized_member, invite_chain, self_nickname, invitation_secrets) =
            match storage.rooms.get(&key_str) {
                Some(info) => match &info.self_authorized_member {
                    Some(am) => (
                        am.clone(),
                        info.invite_chain.clone(),
                        info.self_nickname.clone(),
                        info.invitation_secrets.clone(),
                    ),
                    None => return (None, None),
                },
                None => return (None, None),
            };

        // Build members delta - include self and any missing chain members
        let current_member_ids: HashSet<MemberId> = room_state
            .members
            .members
            .iter()
            .map(|m| m.member.id())
            .collect();
        let mut members_to_add = vec![authorized_member.clone()];
        for chain_member in &invite_chain {
            if !current_member_ids.contains(&chain_member.member.id()) {
                members_to_add.push(chain_member.clone());
            }
        }

        // Build member_info delta
        let self_id = MemberId::from(&self_vk);
        // Route through `canonical` (#411 round 8 item A): a bare `.find()`
        // could read a losing (revoked) duplicate record and rebuild the
        // rejoin member_info from its version, understating the true version
        // and letting a stale record win the next apply_delta rank comparison.
        let existing_version = room_state
            .member_info
            .canonical(self_id)
            .map(|i| i.member_info.version)
            .unwrap_or(0);

        // Restore the member's real nickname (persisted on join / set-nickname /
        // import) rather than the generic "Member" placeholder. The selection —
        // public vs sealed, the no-secret fallback, and the max_nickname_size
        // clamp — lives in `rejoin_preferred_nickname` so it is unit-testable
        // without a node connection.
        let preferred_nickname = rejoin_preferred_nickname(
            room_state,
            signing_key,
            &invitation_secrets,
            self_nickname.as_deref(),
        );

        let member_info = MemberInfo {
            member_id: self_id,
            version: existing_version,
            preferred_nickname,
            // A rejoining member was pruned for inactivity, so their previous
            // member_info (and any deputy grants) was already cleaned up; they
            // re-appoint deputies after rejoining if desired. (#410)
            deputies: Vec::new(),
        };
        let authorized_info = AuthorizedMemberInfo::new_with_member_key(member_info, signing_key);

        (
            Some(MembersDelta::new(members_to_add)),
            Some(vec![authorized_info]),
        )
    }

    /// Send a message using an explicit signing key (without requiring local storage)
    ///
    /// This fetches the room state from the network and attempts to re-add the sender
    /// if they were pruned for inactivity. Useful for automation, bots, and CI/CD pipelines.
    pub async fn send_message_with_key(
        &self,
        room_owner_key: &VerifyingKey,
        message_content: String,
        signing_key: &SigningKey,
    ) -> Result<()> {
        info!(
            "Sending message (with explicit key) to room owned by: {}",
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Fetch room state from the network
        let mut room_state = self.get_room(room_owner_key, false).await?;

        let sender_vk = signing_key.verifying_key();
        let sender_member_id = author_member_id(signing_key);

        // Resolve any bare @nickname mentions to full mention tokens.
        let message_content = resolve_outgoing_mentions(&room_state, &message_content);

        // Build the body — plaintext for a public room, AES-256-GCM sealed
        // for a private room. Any persisted invitation-carried secrets (when
        // a config dir holds this room) supplement the contract's per-member
        // `encrypted_secrets` blob.
        //
        // This is the explicit-key / stateless path (bots, CI/CD), documented
        // as NOT requiring local storage, so a missing/corrupt/read-only
        // `rooms.json` must NOT fail the send: `.unwrap_or_default()` degrades
        // to "no invitation secrets", and `build_message_body` then relies on
        // the contract `encrypted_secrets` blob (public sends need no secret at
        // all). Only erroring if NO secret is available anywhere — never on a
        // storage hiccup the send doesn't depend on.
        let invitation_secrets = self
            .storage
            .get_invitation_secrets(room_owner_key)
            .unwrap_or_default();
        let content = crate::private_room::build_message_body(
            &room_state,
            signing_key,
            &invitation_secrets,
            message_content,
        )
        .map_err(|e| anyhow!(e))?;

        // Create the message
        let message = river_core::room_state::message::MessageV1 {
            room_owner: MemberId::from(*room_owner_key),
            author: sender_member_id,
            content,
            time: std::time::SystemTime::now(),
        };

        // Sign the message
        let auth_message =
            river_core::room_state::message::AuthorizedMessageV1::new(message, signing_key);

        // Check if we need to re-add ourselves (pruned for inactivity)
        let is_member = room_state
            .members
            .members
            .iter()
            .any(|m| m.member.member_vk == sender_vk);
        let (members_delta, member_info_delta) =
            self.build_rejoin_delta(&room_state, room_owner_key, signing_key);

        if !is_member && members_delta.is_none() {
            return Err(anyhow!(
                "Signing key is not a current member of this room and no stored membership \
                 credentials were found for automatic rejoin. If you were pruned for inactivity, \
                 ensure you first accepted an invitation via `riverctl invite accept`."
            ));
        }

        // Create a delta with the new message
        let delta = ChatRoomStateV1Delta {
            recent_messages: Some(vec![auth_message.clone()]),
            members: members_delta,
            member_info: member_info_delta,
            ..Default::default()
        };

        // Apply the delta locally for validation
        let params = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        room_state
            .apply_delta(&room_state.clone(), &params, &Some(delta.clone()))
            .map_err(|e| anyhow!("Failed to apply message delta: {:?}", e))?;

        // Send the delta to the network
        let contract_key = self.owner_vk_to_contract_key(room_owner_key);

        let delta_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&delta, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize delta: {}", e))?;
            buf
        };

        let update_request = ContractRequest::Update {
            key: contract_key,
            data: UpdateData::Delta(delta_bytes.into()),
        };

        let client_request = ClientRequest::ContractOp(update_request);

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(client_request)
            .await
            .map_err(|e| anyhow!("Failed to send update request: {}", e))?;

        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(Ok(response)) => response,
                Ok(Err(e)) => return Err(anyhow!("Failed to receive response: {}", e)),
                Err(_) => {
                    return Err(anyhow!(
                        "Timeout waiting for update response after 60 seconds"
                    ))
                }
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::UpdateResponse { key, .. }) => {
                info!("Message sent successfully to contract: {}", key.id());
                Ok(())
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    pub async fn send_message(
        &self,
        room_owner_key: &VerifyingKey,
        message_content: String,
    ) -> Result<()> {
        info!(
            "Sending message to room owned by: {}",
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get signing key from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be a member of the room to send messages.")
        })?;
        let (signing_key, _, _contract_key_str) = room_data;

        // Fetch fresh state from network so build_rejoin_delta can detect pruning
        let mut room_state = self.get_room(room_owner_key, false).await?;

        // Resolve any bare @nickname mentions to full mention tokens.
        let message_content = resolve_outgoing_mentions(&room_state, &message_content);

        // Build the body — plaintext for a public room, AES-256-GCM sealed for
        // a private room (secret resolved from the contract's per-member
        // `encrypted_secrets` blob, or this room's persisted invitation
        // secrets). Unlike `send_message_with_key`, this path already requires
        // local storage (it loaded the signing key from `rooms.json` above), so
        // a `?` here surfaces a corrupt store as a clear error rather than a
        // misleading "no secret available".
        let invitation_secrets = self.storage.get_invitation_secrets(room_owner_key)?;
        let content = crate::private_room::build_message_body(
            &room_state,
            &signing_key,
            &invitation_secrets,
            message_content,
        )
        .map_err(|e| anyhow!(e))?;

        // Create the message
        let message = river_core::room_state::message::MessageV1 {
            room_owner: river_core::room_state::member::MemberId::from(*room_owner_key),
            author: author_member_id(&signing_key),
            content,
            time: std::time::SystemTime::now(),
        };

        // Sign the message
        let auth_message =
            river_core::room_state::message::AuthorizedMessageV1::new(message, &signing_key);

        // Check if we need to re-add ourselves (pruned for inactivity)
        let (members_delta, member_info_delta) =
            self.build_rejoin_delta(&room_state, room_owner_key, &signing_key);

        // Create a delta with the new message
        let delta = river_core::room_state::ChatRoomStateV1Delta {
            recent_messages: Some(vec![auth_message.clone()]),
            members: members_delta,
            member_info: member_info_delta,
            ..Default::default()
        };

        // Apply the delta to our local state for validation
        let params = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        room_state
            .apply_delta(&room_state.clone(), &params, &Some(delta.clone()))
            .map_err(|e| anyhow!("Failed to apply message delta: {:?}", e))?;

        // Update the stored state
        self.storage
            .update_room_state(room_owner_key, room_state.clone())?;

        // Send the delta to the network
        let contract_key = self.owner_vk_to_contract_key(room_owner_key);

        // Serialize the delta
        let delta_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&delta, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize delta: {}", e))?;
            buf
        };

        let update_request = ContractRequest::Update {
            key: contract_key,
            data: UpdateData::Delta(delta_bytes.into()),
        };

        let client_request = ClientRequest::ContractOp(update_request);

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(client_request)
            .await
            .map_err(|e| anyhow!("Failed to send update request: {}", e))?;

        // Wait for response
        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(Ok(response)) => response,
                Ok(Err(e)) => return Err(anyhow!("Failed to receive response: {}", e)),
                Err(_) => {
                    return Err(anyhow!(
                        "Timeout waiting for update response after 60 seconds"
                    ))
                }
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::UpdateResponse { key, .. }) => {
                info!("Message sent successfully to contract: {}", key.id());
                Ok(())
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    /// Send a pre-built `ChatRoomStateV1Delta` for a room. Used by call sites
    /// that build the delta themselves (e.g. `riverctl dm send`/`dm purge`)
    /// so they don't have to duplicate the contract-key + serialize + recv
    /// dance.
    pub async fn send_state_delta(
        &self,
        room_owner_key: &VerifyingKey,
        delta: &ChatRoomStateV1Delta,
    ) -> Result<()> {
        let contract_key = self.owner_vk_to_contract_key(room_owner_key);

        let delta_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(delta, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize delta: {}", e))?;
            buf
        };

        let update_request = ContractRequest::Update {
            key: contract_key,
            data: UpdateData::Delta(delta_bytes.into()),
        };
        let client_request = ClientRequest::ContractOp(update_request);

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(client_request)
            .await
            .map_err(|e| anyhow!("Failed to send update request: {}", e))?;

        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(Ok(response)) => response,
                Ok(Err(e)) => return Err(anyhow!("Failed to receive response: {}", e)),
                Err(_) => {
                    return Err(anyhow!(
                        "Timeout waiting for update response after 60 seconds"
                    ))
                }
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::UpdateResponse { .. }) => Ok(()),
            other => Err(anyhow!("Unexpected response type: {:?}", other)),
        }
    }

    /// Edit a message you sent
    pub async fn edit_message(
        &self,
        room_owner_key: &VerifyingKey,
        target_message_id: river_core::room_state::message::MessageId,
        new_content: String,
    ) -> Result<()> {
        info!(
            "Editing message in room owned by: {}",
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get signing key from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be a member of the room to edit messages.")
        })?;
        let (signing_key, _, _contract_key_str) = room_data;

        // Fetch fresh state from network so build_rejoin_delta can detect pruning
        let mut room_state = self.get_room(room_owner_key, false).await?;

        // Build the action body — plaintext for a public room, AES-256-GCM
        // sealed for a private room (secret resolved from the contract's
        // per-member `encrypted_secrets` blob, or this room's persisted
        // invitation secrets). This path already requires local storage (it
        // loaded the signing key from `rooms.json` above), so `?` surfaces a
        // corrupt store as a clear error rather than a misleading "no secret".
        let invitation_secrets = self.storage.get_invitation_secrets(room_owner_key)?;
        let content = crate::private_room::build_action_body(
            &room_state,
            &signing_key,
            &invitation_secrets,
            river_core::room_state::content::ActionContentV1::edit(target_message_id, new_content),
        )
        .map_err(|e| anyhow!(e))?;

        // Create the edit action message
        let message = river_core::room_state::message::MessageV1 {
            room_owner: MemberId::from(*room_owner_key),
            author: author_member_id(&signing_key),
            content,
            time: std::time::SystemTime::now(),
        };

        // Sign the message
        let auth_message =
            river_core::room_state::message::AuthorizedMessageV1::new(message, &signing_key);

        // Check if we need to re-add ourselves (pruned for inactivity)
        let (members_delta, member_info_delta) =
            self.build_rejoin_delta(&room_state, room_owner_key, &signing_key);

        // Create a delta with the edit action
        let delta = ChatRoomStateV1Delta {
            recent_messages: Some(vec![auth_message]),
            members: members_delta,
            member_info: member_info_delta,
            ..Default::default()
        };

        // Apply the delta to our local state for validation
        let params = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        room_state
            .apply_delta(&room_state.clone(), &params, &Some(delta.clone()))
            .map_err(|e| anyhow!("Failed to apply edit delta: {:?}", e))?;

        // Update the stored state
        self.storage.update_room_state(room_owner_key, room_state)?;

        // Send the delta to the network
        self.send_delta(room_owner_key, delta).await
    }

    /// Delete a message you sent
    pub async fn delete_message(
        &self,
        room_owner_key: &VerifyingKey,
        target_message_id: river_core::room_state::message::MessageId,
    ) -> Result<()> {
        info!(
            "Deleting message in room owned by: {}",
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get signing key from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be a member of the room to delete messages.")
        })?;
        let (signing_key, _, _contract_key_str) = room_data;

        // Fetch fresh state from network so build_rejoin_delta can detect pruning
        let mut room_state = self.get_room(room_owner_key, false).await?;

        // Build the action body — plaintext (public) or AES-256-GCM sealed
        // (private). See `edit_message` for the storage / secret rationale.
        let invitation_secrets = self.storage.get_invitation_secrets(room_owner_key)?;
        let content = crate::private_room::build_action_body(
            &room_state,
            &signing_key,
            &invitation_secrets,
            river_core::room_state::content::ActionContentV1::delete(target_message_id),
        )
        .map_err(|e| anyhow!(e))?;

        // Create the delete action message
        let message = river_core::room_state::message::MessageV1 {
            room_owner: MemberId::from(*room_owner_key),
            author: author_member_id(&signing_key),
            content,
            time: std::time::SystemTime::now(),
        };

        // Sign the message
        let auth_message =
            river_core::room_state::message::AuthorizedMessageV1::new(message, &signing_key);

        // Check if we need to re-add ourselves (pruned for inactivity)
        let (members_delta, member_info_delta) =
            self.build_rejoin_delta(&room_state, room_owner_key, &signing_key);

        // Create a delta with the delete action
        let delta = ChatRoomStateV1Delta {
            recent_messages: Some(vec![auth_message]),
            members: members_delta,
            member_info: member_info_delta,
            ..Default::default()
        };

        // Apply the delta to our local state for validation
        let params = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        room_state
            .apply_delta(&room_state.clone(), &params, &Some(delta.clone()))
            .map_err(|e| anyhow!("Failed to apply delete delta: {:?}", e))?;

        // Update the stored state
        self.storage.update_room_state(room_owner_key, room_state)?;

        // Send the delta to the network
        self.send_delta(room_owner_key, delta).await
    }

    /// Add a reaction to a message
    pub async fn add_reaction(
        &self,
        room_owner_key: &VerifyingKey,
        target_message_id: river_core::room_state::message::MessageId,
        emoji: String,
    ) -> Result<()> {
        info!(
            "Adding reaction '{}' in room owned by: {}",
            emoji,
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get signing key from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be a member of the room to add reactions.")
        })?;
        let (signing_key, _, _contract_key_str) = room_data;

        // Fetch fresh state from network so build_rejoin_delta can detect pruning
        let mut room_state = self.get_room(room_owner_key, false).await?;

        // Build the action body — plaintext (public) or AES-256-GCM sealed
        // (private). See `edit_message` for the storage / secret rationale.
        let invitation_secrets = self.storage.get_invitation_secrets(room_owner_key)?;
        let content = crate::private_room::build_action_body(
            &room_state,
            &signing_key,
            &invitation_secrets,
            river_core::room_state::content::ActionContentV1::reaction(target_message_id, emoji),
        )
        .map_err(|e| anyhow!(e))?;

        // Create the reaction action message
        let message = river_core::room_state::message::MessageV1 {
            room_owner: MemberId::from(*room_owner_key),
            author: author_member_id(&signing_key),
            content,
            time: std::time::SystemTime::now(),
        };

        // Sign the message
        let auth_message =
            river_core::room_state::message::AuthorizedMessageV1::new(message, &signing_key);

        // Check if we need to re-add ourselves (pruned for inactivity)
        let (members_delta, member_info_delta) =
            self.build_rejoin_delta(&room_state, room_owner_key, &signing_key);

        // Create a delta with the reaction action
        let delta = ChatRoomStateV1Delta {
            recent_messages: Some(vec![auth_message]),
            members: members_delta,
            member_info: member_info_delta,
            ..Default::default()
        };

        // Apply the delta to our local state for validation
        let params = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        room_state
            .apply_delta(&room_state.clone(), &params, &Some(delta.clone()))
            .map_err(|e| anyhow!("Failed to apply reaction delta: {:?}", e))?;

        // Update the stored state
        self.storage.update_room_state(room_owner_key, room_state)?;

        // Send the delta to the network
        self.send_delta(room_owner_key, delta).await
    }

    /// Remove a reaction from a message
    pub async fn remove_reaction(
        &self,
        room_owner_key: &VerifyingKey,
        target_message_id: river_core::room_state::message::MessageId,
        emoji: String,
    ) -> Result<()> {
        info!(
            "Removing reaction '{}' in room owned by: {}",
            emoji,
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get signing key from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be a member of the room to remove reactions.")
        })?;
        let (signing_key, _, _contract_key_str) = room_data;

        // Fetch fresh state from network so build_rejoin_delta can detect pruning
        let mut room_state = self.get_room(room_owner_key, false).await?;

        // Build the action body — plaintext (public) or AES-256-GCM sealed
        // (private). See `edit_message` for the storage / secret rationale.
        let invitation_secrets = self.storage.get_invitation_secrets(room_owner_key)?;
        let content = crate::private_room::build_action_body(
            &room_state,
            &signing_key,
            &invitation_secrets,
            river_core::room_state::content::ActionContentV1::remove_reaction(
                target_message_id,
                emoji,
            ),
        )
        .map_err(|e| anyhow!(e))?;

        // Create the remove_reaction action message
        let message = river_core::room_state::message::MessageV1 {
            room_owner: MemberId::from(*room_owner_key),
            author: author_member_id(&signing_key),
            content,
            time: std::time::SystemTime::now(),
        };

        // Sign the message
        let auth_message =
            river_core::room_state::message::AuthorizedMessageV1::new(message, &signing_key);

        // Check if we need to re-add ourselves (pruned for inactivity)
        let (members_delta, member_info_delta) =
            self.build_rejoin_delta(&room_state, room_owner_key, &signing_key);

        // Create a delta with the remove_reaction action
        let delta = ChatRoomStateV1Delta {
            recent_messages: Some(vec![auth_message]),
            members: members_delta,
            member_info: member_info_delta,
            ..Default::default()
        };

        // Apply the delta to our local state for validation
        let params = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        room_state
            .apply_delta(&room_state.clone(), &params, &Some(delta.clone()))
            .map_err(|e| anyhow!("Failed to apply remove_reaction delta: {:?}", e))?;

        // Update the stored state
        self.storage.update_room_state(room_owner_key, room_state)?;

        // Send the delta to the network
        self.send_delta(room_owner_key, delta).await
    }

    /// Reply to a message
    pub async fn send_reply(
        &self,
        room_owner_key: &VerifyingKey,
        target_message_id: river_core::room_state::message::MessageId,
        reply_text: String,
    ) -> Result<()> {
        info!(
            "Sending reply in room owned by: {}",
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get signing key from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be a member of the room to send replies.")
        })?;
        let (signing_key, _, _contract_key_str) = room_data;

        // Fetch fresh state from network so build_rejoin_delta can detect pruning
        let mut room_state = self.get_room(room_owner_key, false).await?;

        // Decrypt the room's private content BEFORE selecting the reply target:
        // this rebuilds `actions_state` from the decrypted private edit/delete
        // actions, so a target the user already deleted is correctly excluded
        // (`display_messages()` hides it) and an edited target's quoted preview
        // reflects the edit rather than the stale original. Returns the secrets
        // used to decrypt the preview body below (empty map / no-op for a public
        // room, so behaviour there is unchanged).
        let secrets = self.room_display_secrets(room_owner_key, &mut room_state);

        // Find the target message to extract author name and content preview
        let target_msg = room_state
            .recent_messages
            .display_messages()
            .find(|m| m.id() == target_message_id)
            .ok_or_else(|| {
                anyhow!("Target message not found in recent messages. Cannot reply to expired messages via CLI.")
            })?;

        // Decrypt the target author's nickname with the room secrets — in a
        // private room this is sealed, and `build_reply_body` seals it into
        // `ReplyContentV1.target_author_name` and PERSISTS it to contract state.
        // Without decrypting here, the reply's quoted author would read
        // "[Encrypted: N bytes, vN]" to every reader (including the UI) forever.
        let target_author_name = room_state
            .member_info
            .canonical(target_msg.message.author)
            .map(|info| unseal_nickname_display(&info.member_info.preferred_nickname, &secrets))
            .unwrap_or_else(|| target_msg.message.author.to_string());

        // Quote the target's plaintext. A PRIVATE target body is decrypted via
        // `secrets`; without this the CLI sealed "<encrypted>" into the reply's
        // `ReplyContentV1`, so the quoted context read "<encrypted>" to every
        // reader forever.
        let target_content_preview: String =
            message_display_text_with_secrets(&room_state, target_msg, &secrets)
                .chars()
                .take(100)
                .collect();

        // Resolve any bare @nickname mentions in the reply body.
        let reply_text = resolve_outgoing_mentions(&room_state, &reply_text);

        // Build the reply body — plaintext (public) or AES-256-GCM sealed
        // (private). See `edit_message` for the storage / secret rationale. The
        // target author name and content preview are sealed alongside the reply
        // text in a private room (they are part of `ReplyContentV1`), so the
        // reply context is not leaked in the clear.
        let invitation_secrets = self.storage.get_invitation_secrets(room_owner_key)?;
        let content = crate::private_room::build_reply_body(
            &room_state,
            &signing_key,
            &invitation_secrets,
            reply_text,
            target_message_id,
            target_author_name,
            target_content_preview,
        )
        .map_err(|e| anyhow!(e))?;

        // Create the reply message
        let message = river_core::room_state::message::MessageV1 {
            room_owner: MemberId::from(*room_owner_key),
            author: author_member_id(&signing_key),
            content,
            time: std::time::SystemTime::now(),
        };

        // Sign the message
        let auth_message =
            river_core::room_state::message::AuthorizedMessageV1::new(message, &signing_key);

        // Check if we need to re-add ourselves (pruned for inactivity)
        let (members_delta, member_info_delta) =
            self.build_rejoin_delta(&room_state, room_owner_key, &signing_key);

        // Create a delta with the reply message
        let delta = ChatRoomStateV1Delta {
            recent_messages: Some(vec![auth_message]),
            members: members_delta,
            member_info: member_info_delta,
            ..Default::default()
        };

        // Apply the delta to our local state for validation
        let params = ChatRoomParametersV1 {
            owner: *room_owner_key,
        };
        room_state
            .apply_delta(&room_state.clone(), &params, &Some(delta.clone()))
            .map_err(|e| anyhow!("Failed to apply reply delta: {:?}", e))?;

        // Update the stored state
        self.storage.update_room_state(room_owner_key, room_state)?;

        // Send the delta to the network
        self.send_delta(room_owner_key, delta).await
    }

    /// Helper to send a delta to the network.
    /// Assumes migration has already been triggered by the caller (via get_room
    /// or ensure_room_migrated), so owner_vk_to_contract_key returns the correct key.
    async fn send_delta(
        &self,
        room_owner_key: &VerifyingKey,
        delta: ChatRoomStateV1Delta,
    ) -> Result<()> {
        let contract_key = self.owner_vk_to_contract_key(room_owner_key);

        // Serialize the delta
        let delta_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&delta, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize delta: {}", e))?;
            buf
        };

        let update_request = ContractRequest::Update {
            key: contract_key,
            data: UpdateData::Delta(delta_bytes.into()),
        };

        let client_request = ClientRequest::ContractOp(update_request);

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(client_request)
            .await
            .map_err(|e| anyhow!("Failed to send update request: {}", e))?;

        // Wait for response
        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(Ok(response)) => response,
                Ok(Err(e)) => return Err(anyhow!("Failed to receive response: {}", e)),
                Err(_) => {
                    return Err(anyhow!(
                        "Timeout waiting for update response after 60 seconds"
                    ))
                }
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::UpdateResponse { key, .. }) => {
                info!("Action sent successfully to contract: {}", key.id());
                Ok(())
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    /// Stream messages from a room by polling for updates
    pub async fn stream_messages(
        &self,
        room_owner_key: &VerifyingKey,
        poll_interval_ms: u64,
        timeout_secs: u64,
        max_messages: usize,
        initial_messages: usize,
        format: OutputFormat,
    ) -> Result<()> {
        // Get the contract key for the room
        let room = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found in local storage. You may need to create or join it first.")
        })?;

        let (_signing_key, _room_state, contract_key_str) = room;
        let _contract_key = contract_key_str.clone();

        // Print header for human format
        if matches!(format, OutputFormat::Human) {
            eprintln!(
                "Streaming messages from room {} (press Ctrl+C to stop)...\n",
                bs58::encode(room_owner_key.as_bytes()).into_string()
            );
        }

        // Track seen messages: key -> last-emitted effective content, so a later
        // edit (content change) is detected and re-emitted, not just new ids.
        let mut seen_messages: HashMap<String, String> = HashMap::new();
        // Messages for which a deletion has already been emitted (one-shot). The
        // polling path needs NO startup pre-seed (unlike subscribe): it only ever
        // inserts into `seen` via `display_messages()` (initial window + each
        // poll's emit_new_and_edited), which excludes deleted messages — so a
        // pre-existing deletion is never in `seen` and `should_emit_deletion`
        // returns false for it. (A future change that seeds `seen` from raw
        // `messages` here would need a pre-seed like the subscribe path's.)
        let mut deleted_emitted: HashSet<String> = HashSet::new();
        // Reactions fingerprint per SURFACED message, so a reaction added/removed
        // AFTER the message was streamed surfaces as a `reaction` event
        // (freenet/river#325). Seeded for messages shown at start (below) and for
        // messages emitted live by emit_new_and_edited (which seeds as it emits);
        // emit_reaction_changes only acts on messages already in this map, so a
        // brand-new message's initial reactions are not re-emitted as a change.
        let mut seen_reactions: HashMap<String, String> = HashMap::new();
        let mut new_message_count = 0;
        let start_time = std::time::Instant::now();

        // Show initial messages if requested
        if initial_messages > 0 {
            let mut room_state = self.get_room(room_owner_key, false).await?;
            // Decrypt private-room content for display (no-op for public rooms).
            let secrets = self.room_display_secrets(room_owner_key, &mut room_state);

            // Use display_messages() to filter out action/deleted messages (matches `message list`)
            let all_msgs: Vec<_> = room_state.recent_messages.display_messages().collect();
            let start = all_msgs.len().saturating_sub(initial_messages);

            for msg in &all_msgs[start..] {
                let key = monitor_seen_key(msg);
                seen_messages.insert(
                    key.clone(),
                    message_display_text_with_secrets(&room_state, msg, &secrets),
                );
                // Seed the reactions fingerprint for shown messages so reactions
                // already present at startup aren't re-emitted as a live change;
                // only later changes to them surface.
                seen_reactions.insert(
                    key,
                    reactions_fingerprint(room_state.recent_messages.reactions(&msg.id())),
                );

                Self::output_message(&room_state, msg, room_owner_key, &format, false, &secrets)?;
            }
        }

        // Set up Ctrl+C handler
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel(1);

        // Spawn task to handle Ctrl+C
        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            let _ = shutdown_tx.send(()).await;
        });

        // Main polling loop
        loop {
            // Check for shutdown signal
            if shutdown_rx.try_recv().is_ok() {
                if matches!(format, OutputFormat::Human) {
                    eprintln!("\nStopped monitoring.");
                }
                return Ok(());
            }

            // Check timeout
            if timeout_secs > 0 && start_time.elapsed().as_secs() >= timeout_secs {
                debug!("Timeout reached, exiting stream");
                return Ok(());
            }

            // Check max messages
            if max_messages > 0 && new_message_count >= max_messages {
                debug!("Maximum message count reached, exiting stream");
                return Ok(());
            }

            // Poll for new + edited messages. emit_new_and_edited re-emits a
            // message whose effective content changed (an edit) and emits ones
            // not seen before; it respects max_messages for NEW messages.
            match self.get_room(room_owner_key, false).await {
                Ok(mut room_state) => {
                    // Decrypt private-room content for display (no-op for public rooms).
                    let secrets = self.room_display_secrets(room_owner_key, &mut room_state);
                    Self::emit_new_and_edited(
                        &room_state,
                        &mut seen_messages,
                        &mut deleted_emitted,
                        &mut seen_reactions,
                        room_owner_key,
                        &format,
                        max_messages,
                        &mut new_message_count,
                        &secrets,
                    )?;
                    Self::emit_deletions(
                        &room_state,
                        &seen_messages,
                        &mut deleted_emitted,
                        room_owner_key,
                        &format,
                        &secrets,
                    )?;
                    // Surface reactions added/removed since a message was already
                    // streamed. Runs AFTER emit_new_and_edited so a brand-new
                    // message is seeded (not re-emitted) on the same poll.
                    Self::emit_reaction_changes(
                        &room_state,
                        &mut seen_reactions,
                        room_owner_key,
                        &format,
                        &secrets,
                    )?;
                    if max_messages > 0 && new_message_count >= max_messages {
                        return Ok(());
                    }
                }
                Err(e) => {
                    // Log error but continue polling
                    debug!("Error fetching room state: {}", e);
                }
            }

            // Wait for next poll interval
            tokio::time::sleep(std::time::Duration::from_millis(poll_interval_ms)).await;
        }
    }

    /// Scan the room's display messages and emit any that are NEW or whose
    /// effective content changed (an EDIT) since last seen. `seen` maps each
    /// message's dedup key to the content last emitted for it, so a later edit
    /// is detected as a content change. `new_count` is incremented only for new
    /// messages (edits don't count toward `max_new`); when `max_new > 0` the
    /// scan stops once that many new messages have been emitted this session.
    ///
    /// This is the shared core of both monitor paths (polling `stream_messages`
    /// and subscription-based `monitor`); before it, edits never surfaced in
    /// either stream because dedup keyed on identity alone. freenet/river —
    /// Rogue Worm report.
    // The monitor's per-message tracking state (seen content, emitted deletions,
    // seeded reaction fingerprints) is passed as separate &mut maps rather than a
    // bundled struct to keep the data-flow of each tracking dimension explicit at
    // the call sites — matching the existing edit/deletion design.
    #[allow(clippy::too_many_arguments)]
    fn emit_new_and_edited(
        room_state: &ChatRoomStateV1,
        seen: &mut HashMap<String, String>,
        deleted_emitted: &mut HashSet<String>,
        seen_reactions: &mut HashMap<String, String>,
        room_owner_key: &VerifyingKey,
        format: &OutputFormat,
        max_new: usize,
        new_count: &mut usize,
        secrets: &HashMap<u32, [u8; 32]>,
    ) -> Result<()> {
        for msg in room_state.recent_messages.display_messages() {
            let key = monitor_seen_key(msg);
            let content = message_display_text_with_secrets(room_state, msg, secrets);
            let is_edit = match classify_seen(seen, &key, &content) {
                EmitKind::Unchanged => continue,
                EmitKind::Edited => true,
                EmitKind::New => false,
            };
            Self::output_message(room_state, msg, room_owner_key, format, is_edit, secrets)?;
            // Surfacing a message (showing it new OR as an edit) lifts any
            // start-time delete-suppression: a now-surfaced message's later
            // deletion MUST be reportable. Without this, an unshown pre-existing
            // message edited then deleted live would emit the edit but silently
            // swallow the delete (#324 re-review).
            deleted_emitted.remove(&key);
            // Surfacing a message also seeds its reactions fingerprint, so a
            // reaction added AFTER this point surfaces (emit_reaction_changes only
            // acts on surfaced messages), while the reactions carried by the
            // message/edit event just emitted are NOT re-emitted as a change. The
            // current fingerprint is the right seed: the reactions output here are
            // current, so emit_reaction_changes sees no change on this pass.
            // freenet/river#325.
            seen_reactions.insert(
                key.clone(),
                reactions_fingerprint(room_state.recent_messages.reactions(&msg.id())),
            );
            seen.insert(key, content);
            if !is_edit {
                *new_count += 1;
                if max_new > 0 && *new_count >= max_new {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Emit a deletion event for any previously-surfaced message that has since
    /// been deleted (once per message). Deleted messages are excluded from
    /// `display_messages`, so `emit_new_and_edited` never sees them — this is
    /// the only path that surfaces a deletion to the stream.
    ///
    /// `deleted_emitted` doubles as the suppression set: it is pre-seeded with
    /// every pre-existing message NOT shown at start, and `emit_new_and_edited`
    /// removes a key when it later surfaces that message (so its deletion
    /// becomes reportable). A key is added here once its deletion is emitted, so
    /// the event fires at most once. freenet/river#323 (#324 review).
    fn emit_deletions(
        room_state: &ChatRoomStateV1,
        seen: &HashMap<String, String>,
        deleted_emitted: &mut HashSet<String>,
        room_owner_key: &VerifyingKey,
        format: &OutputFormat,
        secrets: &HashMap<u32, [u8; 32]>,
    ) -> Result<()> {
        // We can only report a deletion while the original message is still in
        // the recent-messages window. If a message is pruned out of the window
        // (max_recent_messages) and only then deleted, no event is emitted —
        // acceptable for a bounded live stream.
        for msg in &room_state.recent_messages.messages {
            if !room_state.recent_messages.is_deleted(&msg.id()) {
                continue;
            }
            let key = monitor_seen_key(msg);
            if should_emit_deletion(seen, deleted_emitted, &key) {
                Self::output_deletion(room_state, msg, room_owner_key, format, secrets)?;
                deleted_emitted.insert(key);
            }
        }
        Ok(())
    }

    /// Emit a `reaction` event for any SURFACED message whose reactions changed
    /// since last recorded (a reaction added or removed live, after the message
    /// was already streamed). `seen_reactions` holds a fingerprint ONLY for
    /// surfaced messages — those shown at start, or emitted live by
    /// `emit_new_and_edited` (which seeds the fingerprint as it emits). A message
    /// absent from the map was never surfaced (e.g. room history outside the
    /// `--subscribe` initial window), so a reaction to it is NOT emitted — the
    /// same "only for messages the stream displayed" rule the deletion path
    /// follows (#324). Only a *change* to a surfaced message emits.
    ///
    /// This is the reaction counterpart of `emit_new_and_edited` (edits) and
    /// `emit_deletions` (deletions). It must run AFTER `emit_new_and_edited` so a
    /// brand-new message that path just emitted (and seeded) is already in
    /// `seen_reactions`, and the reactions it carried are never re-emitted as a
    /// spurious change. freenet/river#325.
    fn emit_reaction_changes(
        room_state: &ChatRoomStateV1,
        seen_reactions: &mut HashMap<String, String>,
        room_owner_key: &VerifyingKey,
        format: &OutputFormat,
        secrets: &HashMap<u32, [u8; 32]>,
    ) -> Result<()> {
        for msg in room_state.recent_messages.display_messages() {
            let key = monitor_seen_key(msg);
            let fingerprint =
                reactions_fingerprint(room_state.recent_messages.reactions(&msg.id()));
            match classify_reaction(seen_reactions, &key, &fingerprint) {
                // Not surfaced by the stream → never seed or emit here.
                ReactionEmit::NotSurfaced => continue,
                ReactionEmit::Unchanged => continue,
                ReactionEmit::Changed => {
                    Self::output_reaction_change(room_state, msg, room_owner_key, format, secrets)?;
                }
            }
            seen_reactions.insert(key, fingerprint);
        }
        Ok(())
    }

    /// Emit a `reaction` event carrying the message's *current* reactions map
    /// (`emoji -> count`), so a downstream relay can reconcile the new state
    /// without tracking per-reactor deltas. JSON `type: "reaction"`; the human
    /// line is `[reaction]`-prefixed. The map is empty when the last reaction was
    /// removed. freenet/river#325.
    fn output_reaction_change(
        room_state: &ChatRoomStateV1,
        msg: &river_core::room_state::message::AuthorizedMessageV1,
        room_owner_key: &VerifyingKey,
        format: &OutputFormat,
        secrets: &HashMap<u32, [u8; 32]>,
    ) -> Result<()> {
        let msg_id = msg.id();
        let reactions = room_state.recent_messages.reactions(&msg_id);
        let author_str = msg.message.author.to_string();
        let nickname = room_state
            .member_info
            .canonical(msg.message.author)
            .map(|info| unseal_nickname_display(&info.member_info.preferred_nickname, secrets));
        let datetime: DateTime<Utc> = msg.message.time.into();

        match format {
            OutputFormat::Human => {
                let local_time: DateTime<Local> = datetime.into();
                let display_name = nickname
                    .clone()
                    .unwrap_or_else(|| author_str.chars().take(8).collect());
                let reactions_str = reactions
                    .map(|r| {
                        if r.is_empty() {
                            "(none)".to_string()
                        } else {
                            let parts: Vec<_> = r
                                .iter()
                                .map(|(emoji, reactors)| format!("{}×{}", emoji, reactors.len()))
                                .collect();
                            parts.join(" ")
                        }
                    })
                    .unwrap_or_else(|| "(none)".to_string());
                println!(
                    "[reaction] [{} - {}]: {}",
                    local_time.format("%H:%M:%S"),
                    display_name,
                    reactions_str
                );
            }
            OutputFormat::Json => {
                let reactions_map: std::collections::HashMap<String, usize> = reactions
                    .map(|r| r.iter().map(|(k, v)| (k.clone(), v.len())).collect())
                    .unwrap_or_default();
                let json_msg = json!({
                    "type": "reaction",
                    "message_id": msg_id.0 .0.to_string(),
                    "room": bs58::encode(room_owner_key.as_bytes()).into_string(),
                    "author": author_str,
                    "nickname": nickname,
                    "timestamp": datetime.to_rfc3339(),
                    "reactions": reactions_map,
                });
                println!("{}", serde_json::to_string(&json_msg)?);
            }
        }
        std::io::stdout().flush()?;
        Ok(())
    }

    /// Emit a deletion event (the message's content is gone, so only its
    /// identity/author/time are reported). JSON `type: "delete"`; human line is
    /// `[deleted]`-prefixed. freenet/river#323.
    fn output_deletion(
        room_state: &ChatRoomStateV1,
        msg: &river_core::room_state::message::AuthorizedMessageV1,
        room_owner_key: &VerifyingKey,
        format: &OutputFormat,
        secrets: &HashMap<u32, [u8; 32]>,
    ) -> Result<()> {
        let msg_id = msg.id();
        let author_str = msg.message.author.to_string();
        let nickname = room_state
            .member_info
            .canonical(msg.message.author)
            .map(|info| unseal_nickname_display(&info.member_info.preferred_nickname, secrets));
        let datetime: DateTime<Utc> = msg.message.time.into();

        match format {
            OutputFormat::Human => {
                let local_time: DateTime<Local> = datetime.into();
                let display_name = nickname
                    .clone()
                    .unwrap_or_else(|| author_str.chars().take(8).collect());
                println!(
                    "[deleted] [{} - {}]: (message deleted)",
                    local_time.format("%H:%M:%S"),
                    display_name
                );
            }
            OutputFormat::Json => {
                let json_msg = json!({
                    "type": "delete",
                    "message_id": msg_id.0 .0.to_string(),
                    "room": bs58::encode(room_owner_key.as_bytes()).into_string(),
                    "author": author_str,
                    "nickname": nickname,
                    "timestamp": datetime.to_rfc3339(),
                });
                println!("{}", serde_json::to_string(&json_msg)?);
            }
        }
        std::io::stdout().flush()?;
        Ok(())
    }

    /// Helper function to output a message in the requested format.
    ///
    /// `is_edit` marks a re-emission of a message whose content changed since it
    /// was first streamed (the monitor's edit detection): the JSON `type`
    /// becomes `"edit"` and the human line is prefixed so a downstream relay can
    /// tell an edit from a fresh message.
    ///
    /// Note `type: "edit"` differs from the `edited` boolean: `edited` is true
    /// whenever an edit action exists for the message (so a message already
    /// edited *before* the stream first saw it is emitted once as
    /// `type: "message"` with `edited: true`), whereas `type: "edit"` marks a
    /// re-emission triggered by a content change observed live.
    fn output_message(
        room_state: &ChatRoomStateV1,
        msg: &river_core::room_state::message::AuthorizedMessageV1,
        room_owner_key: &VerifyingKey,
        format: &OutputFormat,
        is_edit: bool,
        secrets: &HashMap<u32, [u8; 32]>,
    ) -> Result<()> {
        // Get display content (handles edits and non-text public content like
        // join events; `secrets` decrypts private-room bodies — only a body
        // whose secret is unavailable renders as "<encrypted>")
        let content = message_display_text_with_secrets(room_state, msg, secrets);

        // Get message ID for checking edited status and reactions
        let msg_id = msg.id();
        let edited = room_state.recent_messages.is_edited(&msg_id);
        let reactions = room_state.recent_messages.reactions(&msg_id);
        let reply = reply_context_display_with_secrets(room_state, msg, secrets);

        match format {
            OutputFormat::Human => {
                let author_str = msg.message.author.to_string();
                let author_short = author_str.chars().take(8).collect::<String>();

                // Get nickname if available (decrypted for a private room)
                let nickname = room_state
                    .member_info
                    .canonical(msg.message.author)
                    .map(|info| {
                        unseal_nickname_display(&info.member_info.preferred_nickname, secrets)
                    })
                    .unwrap_or(author_short);

                let datetime: DateTime<Utc> = msg.message.time.into();
                let local_time: DateTime<Local> = datetime.into();

                let edited_indicator = if edited { " (edited)" } else { "" };
                // Re-emission of an edited message — distinguish it from a fresh
                // one for a downstream relay reading the human stream.
                let edit_prefix = if is_edit { "[edit] " } else { "" };
                let reply_prefix = reply
                    .as_ref()
                    .map(|(author, preview)| format!("[reply to {}: {}] ", author, preview))
                    .unwrap_or_default();
                let reactions_str = reactions
                    .map(|r| {
                        if r.is_empty() {
                            String::new()
                        } else {
                            let parts: Vec<_> = r
                                .iter()
                                .map(|(emoji, reactors)| format!("{}×{}", emoji, reactors.len()))
                                .collect();
                            format!(" [{}]", parts.join(" "))
                        }
                    })
                    .unwrap_or_default();

                println!(
                    "{}[{} - {}]: {}{}{}{}",
                    edit_prefix,
                    local_time.format("%H:%M:%S"),
                    nickname,
                    reply_prefix,
                    content,
                    edited_indicator,
                    reactions_str
                );
            }
            OutputFormat::Json => {
                let author_str = msg.message.author.to_string();

                let nickname = room_state
                    .member_info
                    .canonical(msg.message.author)
                    .map(|info| {
                        unseal_nickname_display(&info.member_info.preferred_nickname, secrets)
                    });

                let datetime: DateTime<Utc> = msg.message.time.into();

                let reactions_map: std::collections::HashMap<String, usize> = reactions
                    .map(|r| r.iter().map(|(k, v)| (k.clone(), v.len())).collect())
                    .unwrap_or_default();

                let message_id_str = msg_id.0 .0.to_string();

                // Reply context (null for non-replies) so a relay can thread the
                // message; previously absent from the monitor's JSON output.
                let reply_to = reply
                    .as_ref()
                    .map(|(author, preview)| json!({ "author": author, "preview": preview }));

                // Output as JSONL (one JSON object per line). `type` is "edit"
                // for a re-emitted message whose content changed, else "message".
                let json_msg = json!({
                    "type": if is_edit { "edit" } else { "message" },
                    "message_id": message_id_str,
                    "room": bs58::encode(room_owner_key.as_bytes()).into_string(),
                    "author": author_str,
                    "nickname": nickname,
                    "content": content,
                    "timestamp": datetime.to_rfc3339(),
                    "edited": edited,
                    "reply_to": reply_to,
                    "reactions": reactions_map,
                });

                println!("{}", serde_json::to_string(&json_msg)?);
            }
        }

        // Flush stdout immediately for real-time output
        std::io::stdout().flush()?;
        Ok(())
    }

    /// Set the current user's nickname in a room
    pub async fn set_nickname(
        &self,
        room_owner_key: &VerifyingKey,
        new_nickname: String,
    ) -> Result<()> {
        info!(
            "Setting nickname to '{}' in room owned by: {}",
            new_nickname,
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get signing key from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be a member of the room to change your nickname.")
        })?;
        let (signing_key, _, _contract_key_str) = room_data;

        // Fetch fresh state from network so build_rejoin_delta can detect pruning
        let mut room_state = self.get_room(room_owner_key, false).await?;

        let my_member_id = signing_key.verifying_key().into();

        // Seal the nickname for the room's privacy mode. In a PRIVATE room the
        // nickname MUST be AES-256-GCM sealed under the room secret — sending
        // it as plaintext (the previous unconditional `SealedBytes::public`)
        // silently deanonymised the member: the contract's `member_info`
        // validation only checks signature + declared length, so the plaintext
        // was accepted and published into the private room's state for every
        // peer to read. Errors (rather than leaks) if the secret isn't available
        // yet.
        let invitation_secrets = self.storage.get_invitation_secrets(room_owner_key)?;
        let secrets = crate::private_room::collect_secrets_for_room(
            &room_state,
            &signing_key,
            &invitation_secrets,
        );
        let sealed_nickname = crate::private_room::seal_field_for_room(
            &room_state,
            &secrets,
            new_nickname.as_bytes(),
        )
        .map_err(|e| anyhow!(e))?;

        // Find our current member info to get the version AND our existing
        // deputy grants — republishing member_info replaces the whole signed
        // record, so we must carry `deputies` forward or a nickname change would
        // silently revoke every deputy we appointed (#410). Routes through the
        // shared `resolve_own_member_info_base` (canonical, #411 round 8 item A)
        // so a duplicate-holding state can't resurrect a revoked record.
        let current_self_info = resolve_own_member_info_base(&room_state, my_member_id);
        let current_version = current_self_info
            .as_ref()
            .map(|info| info.version)
            .unwrap_or(0);
        let existing_deputies = current_self_info
            .map(|info| info.deputies)
            .unwrap_or_default();

        // Create new member info with incremented version
        let new_member_info = MemberInfo {
            member_id: my_member_id,
            version: current_version + 1,
            preferred_nickname: sealed_nickname,
            deputies: existing_deputies,
        };

        // Sign with our member key
        let authorized_member_info =
            AuthorizedMemberInfo::new_with_member_key(new_member_info, &signing_key);

        // Update local state first. Drop ALL existing records for our own
        // member_id (not just the first match) before pushing the new one — the
        // new record's version (`current_version + 1`) strictly outranks any
        // existing duplicate, so this collapses to the same single canonical
        // record `dedup_to_canonical` would converge on, rather than leaving a
        // stale duplicate sitting in local state alongside the new republish.
        room_state
            .member_info
            .member_info
            .retain(|info| info.member_info.member_id != my_member_id);
        room_state
            .member_info
            .member_info
            .push(authorized_member_info.clone());

        // Save the updated state locally
        self.storage
            .update_room_state(room_owner_key, room_state.clone())?;

        // Persist our chosen nickname so a later rejoin (after an inactivity
        // prune) restores it instead of "Member".
        self.storage
            .update_self_nickname(room_owner_key, &new_nickname)?;

        // Check if we need to re-add ourselves (pruned for inactivity)
        let (members_delta, _) = self.build_rejoin_delta(&room_state, room_owner_key, &signing_key);

        // Create delta with member info update (and members delta if needed)
        let delta = ChatRoomStateV1Delta {
            member_info: Some(vec![authorized_member_info]),
            members: members_delta,
            ..Default::default()
        };

        // Serialize the delta
        let delta_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&delta, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize delta: {}", e))?;
            buf
        };

        // Get contract key and send the update
        let contract_key = self.owner_vk_to_contract_key(room_owner_key);

        let update_request = ContractRequest::Update {
            key: contract_key,
            data: UpdateData::Delta(delta_bytes.into()),
        };

        let client_request = ClientRequest::ContractOp(update_request);

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(client_request)
            .await
            .map_err(|e| anyhow!("Failed to send update request: {}", e))?;

        // Wait for response
        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(Ok(response)) => response,
                Ok(Err(e)) => return Err(anyhow!("Failed to receive response: {}", e)),
                Err(_) => {
                    return Err(anyhow!(
                        "Timeout waiting for update response after 60 seconds"
                    ))
                }
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::UpdateResponse { key, .. }) => {
                info!("Nickname updated successfully for contract: {}", key.id());
                Ok(())
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    /// Ban a member from the room
    ///
    /// The banning member must be either the room owner or an upstream member in the
    /// invite chain of the member being banned.
    pub async fn ban_member(
        &self,
        room_owner_key: &VerifyingKey,
        member_id_short: &str,
    ) -> Result<()> {
        info!(
            "Banning member '{}' from room owned by: {}",
            member_id_short,
            bs58::encode(room_owner_key.as_bytes()).into_string()
        );

        // Get the signing key from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be a member of the room to ban members.")
        })?;
        let (signing_key, _stored_state, _contract_key_str) = room_data;

        // Fetch fresh room state from the network
        let room_state = self.get_room(room_owner_key, false).await?;

        let my_member_id: MemberId = signing_key.verifying_key().into();
        let owner_member_id: MemberId = room_owner_key.into();

        // Find the member to ban by their short ID (first 8 chars of member_id string)
        let target_member = room_state
            .member_info
            .member_info
            .iter()
            .find(|info| {
                let member_id_str = info.member_info.member_id.to_string();
                member_id_str.starts_with(member_id_short)
                    || member_id_str[..8.min(member_id_str.len())]
                        .eq_ignore_ascii_case(member_id_short)
            })
            .ok_or_else(|| {
                anyhow!(
                    "Member '{}' not found. Use 'member list' to see member IDs.",
                    member_id_short
                )
            })?;

        let banned_member_id = target_member.member_info.member_id;

        // Prevent self-banning
        if banned_member_id == my_member_id {
            return Err(anyhow!("Cannot ban yourself"));
        }

        // Prevent banning the room owner
        if banned_member_id == owner_member_id {
            return Err(anyhow!("Cannot ban the room owner"));
        }

        // Verify authorization using the SAME `is_ban_authorized` predicate the
        // contract enforces (owner OR strict-ancestor-of-target OR
        // deputy-of-an-ancestor, including the "can't ban your deputizer"
        // guardrail), so client-side rejection stays in lockstep with
        // on-contract enforcement (#410).
        if my_member_id != owner_member_id {
            let members_by_id = room_state.members.members_by_member_id();
            let authorized = river_core::room_state::member::MembersV1::is_ban_authorized(
                my_member_id,
                banned_member_id,
                &members_by_id,
                &room_state.member_info,
                owner_member_id,
            );
            if !authorized {
                return Err(anyhow!(
                    "Not authorized to ban this member. You can ban members you invited \
                     (directly or indirectly), or members within a subtree you have been \
                     deputized over."
                ));
            }
        }

        info!("Banning member with ID: {}", banned_member_id.to_string());

        // Create the ban
        let user_ban = UserBan {
            owner_member_id,
            banned_at: std::time::SystemTime::now(),
            banned_user: banned_member_id,
        };

        let authorized_ban = AuthorizedUserBan::new(user_ban, my_member_id, &signing_key);

        // Create delta with just the ban
        let delta = ChatRoomStateV1Delta {
            bans: Some(vec![authorized_ban.clone()]),
            ..Default::default()
        };

        // Serialize the delta
        let delta_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&delta, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize delta: {}", e))?;
            buf
        };

        // Get contract key and send the update
        let contract_key = self.owner_vk_to_contract_key(room_owner_key);

        let update_request = ContractRequest::Update {
            key: contract_key,
            data: UpdateData::Delta(delta_bytes.into()),
        };

        let client_request = ClientRequest::ContractOp(update_request);

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(client_request)
            .await
            .map_err(|e| anyhow!("Failed to send update request: {}", e))?;

        // Wait for response
        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(Ok(response)) => response,
                Ok(Err(e)) => return Err(anyhow!("Failed to receive response: {}", e)),
                Err(_) => {
                    return Err(anyhow!(
                        "Timeout waiting for update response after 60 seconds"
                    ))
                }
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::UpdateResponse { key, .. }) => {
                info!("Ban applied successfully for contract: {}", key.id());
                Ok(())
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    /// Deputize a member (#410): grant them authority to ban within the
    /// caller's invite subtree. Implemented by republishing the caller's own
    /// `MemberInfo` at `version + 1` with the target added to `deputies`.
    pub async fn deputize(
        &self,
        room_owner_key: &VerifyingKey,
        member_id_short: &str,
    ) -> Result<()> {
        self.update_own_deputies(room_owner_key, member_id_short, true)
            .await
    }

    /// Revoke a member's deputy authority (#410). Their prior bans stop
    /// enforcing on the contract once this republish converges. Implemented by
    /// republishing the caller's own `MemberInfo` at `version + 1` with the
    /// target removed from `deputies`.
    pub async fn revoke_deputy(
        &self,
        room_owner_key: &VerifyingKey,
        member_id_short: &str,
    ) -> Result<()> {
        self.update_own_deputies(room_owner_key, member_id_short, false)
            .await
    }

    /// Shared implementation for [`Self::deputize`] / [`Self::revoke_deputy`].
    /// Republishes the caller's own signed `MemberInfo` at `version + 1` with
    /// `target` added (`add = true`) or removed (`add = false`) from the
    /// `deputies` list, preserving the existing sealed nickname, and sends it as
    /// a `member_info`-only delta.
    async fn update_own_deputies(
        &self,
        room_owner_key: &VerifyingKey,
        member_id_short: &str,
        add: bool,
    ) -> Result<()> {
        use river_core::room_state::member_info::MAX_DEPUTIES;

        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be a member of the room to manage deputies.")
        })?;
        let (signing_key, _stored_state, _contract_key_str) = room_data;

        let room_state = self.get_room(room_owner_key, false).await?;

        let my_member_id: MemberId = signing_key.verifying_key().into();
        let owner_member_id: MemberId = room_owner_key.into();

        // Load the caller's current signed member_info FIRST: we must preserve the
        // (already-sealed) nickname and version-continuity when republishing, and
        // the revoke path resolves the target against the caller's OWN deputies
        // list (below). Routes through the shared `resolve_own_member_info_base`
        // (canonical, #411 round 8 item A) so a duplicate-holding state can't
        // resurrect a revoked record's deputies at a higher rank.
        let current_self_info = resolve_own_member_info_base(&room_state, my_member_id)
            .ok_or_else(|| {
                anyhow!(
                    "You don't have a member_info entry in this room yet. \
                     Set your nickname first (`member set-nickname`), then retry."
                )
            })?;
        let current_version = current_self_info.version;
        let preferred_nickname = current_self_info.preferred_nickname.clone();
        let mut deputies = current_self_info.deputies.clone();

        // Resolve the target's full MemberId from the short id. Primary: the
        // member_info list (a present member), same lookup as `ban_member`.
        // Revoke fallback (#411 round 4 C): a deputy pruned for inactivity has no
        // member_info but still sits in the caller's signed `deputies` list, so
        // the primary lookup can't find them — match the short id against the
        // caller's own deputies too, so the lingering grant can be revoked
        // (otherwise it silently reactivates if the deputy's membership is later
        // replayed).
        let target = resolve_deputy_target(
            &room_state.member_info.member_info,
            &deputies,
            member_id_short,
            !add, // allow the own-deputies fallback only when revoking
        )
        .ok_or_else(|| {
            anyhow!(
                "Member '{}' not found. Use 'member list' to see member IDs.",
                member_id_short
            )
        })?;

        if target == my_member_id {
            return Err(anyhow!("You cannot deputize yourself"));
        }
        if target == owner_member_id {
            return Err(anyhow!(
                "The room owner already has full authority; deputizing them is a no-op"
            ));
        }

        if add {
            if deputies.contains(&target) {
                info!("Member is already a deputy; nothing to do");
                return Ok(());
            }
            if deputies.len() >= MAX_DEPUTIES {
                return Err(anyhow!(
                    "You already have the maximum of {} deputies",
                    MAX_DEPUTIES
                ));
            }
            deputies.push(target);
        } else if let Some(pos) = deputies.iter().position(|d| *d == target) {
            deputies.remove(pos);
        } else {
            info!("Member is not currently a deputy; nothing to do");
            return Ok(());
        }

        let new_member_info = MemberInfo {
            member_id: my_member_id,
            version: current_version + 1,
            preferred_nickname,
            deputies,
        };
        let authorized_member_info =
            AuthorizedMemberInfo::new_with_member_key(new_member_info, &signing_key);

        let delta = ChatRoomStateV1Delta {
            member_info: Some(vec![authorized_member_info]),
            ..Default::default()
        };
        self.send_delta(room_owner_key, delta).await
    }

    /// Update room configuration. Only the room owner can do this.
    pub async fn update_config(
        &self,
        room_owner_key: &VerifyingKey,
        modify: impl FnOnce(&mut Configuration),
    ) -> Result<()> {
        // Get the signing key from storage
        let room_data = self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found. You must be the room owner to update configuration.")
        })?;
        let (signing_key, _stored_state, _contract_key_str) = room_data;

        // Verify we are the room owner
        let my_vk = signing_key.verifying_key();
        if my_vk != *room_owner_key {
            return Err(anyhow!("Only the room owner can update configuration"));
        }

        // Fetch fresh room state from the network
        let room_state = self.get_room(room_owner_key, false).await?;

        // Clone current config and apply modifications
        let mut new_config = room_state.configuration.configuration.clone();
        new_config.configuration_version += 1;
        modify(&mut new_config);

        // Sign the new configuration
        let authorized_config = AuthorizedConfigurationV1::new(new_config, &signing_key);

        // Create delta with just the configuration change
        let delta = ChatRoomStateV1Delta {
            configuration: Some(authorized_config),
            ..Default::default()
        };

        // Serialize and send
        let delta_bytes = {
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&delta, &mut buf)
                .map_err(|e| anyhow!("Failed to serialize delta: {}", e))?;
            buf
        };

        let contract_key = self.owner_vk_to_contract_key(room_owner_key);

        let update_request = ContractRequest::Update {
            key: contract_key,
            data: UpdateData::Delta(delta_bytes.into()),
        };

        let client_request = ClientRequest::ContractOp(update_request);

        let mut web_api = self.web_api.lock().await;
        web_api
            .send(client_request)
            .await
            .map_err(|e| anyhow!("Failed to send update request: {}", e))?;

        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(60), web_api.recv()).await {
                Ok(Ok(response)) => response,
                Ok(Err(e)) => return Err(anyhow!("Failed to receive response: {}", e)),
                Err(_) => {
                    return Err(anyhow!(
                        "Timeout waiting for update response after 60 seconds"
                    ))
                }
            };

        match response {
            HostResponse::ContractResponse(ContractResponse::UpdateResponse { key, .. }) => {
                info!(
                    "Configuration updated successfully for contract: {}",
                    key.id()
                );
                Ok(())
            }
            _ => Err(anyhow!("Unexpected response type: {:?}", response)),
        }
    }

    /// Subscribe to a room and stream updates using Freenet subscriptions
    ///
    /// Unlike `stream_messages` which polls, this method subscribes to the contract
    /// and receives push notifications when the contract state changes.
    pub async fn subscribe_and_stream(
        &self,
        room_owner_key: &VerifyingKey,
        timeout_secs: u64,
        max_messages: usize,
        initial_messages: usize,
        format: OutputFormat,
    ) -> Result<()> {
        // Verify room exists in local storage before attempting to subscribe
        self.storage.get_room(room_owner_key)?.ok_or_else(|| {
            anyhow!("Room not found in local storage. You may need to create or join it first.")
        })?;

        // Print header for human format
        if matches!(format, OutputFormat::Human) {
            eprintln!(
                "Subscribing to room {} (press Ctrl+C to stop)...",
                bs58::encode(room_owner_key.as_bytes()).into_string()
            );
        }

        // Track seen messages: key -> last-emitted effective content, so a later
        // edit (content change) is detected and re-emitted, not just new ids.
        let mut seen_messages: HashMap<String, String> = HashMap::new();
        // Messages for which a deletion has already been emitted (one-shot).
        // Pre-seeded below with deletions that existed at stream start, so only
        // deletions observed live are surfaced.
        let mut deleted_emitted: HashSet<String> = HashSet::new();
        // Track each shown message's reactions fingerprint so a reaction
        // added/removed AFTER it was streamed surfaces as a `reaction` event
        // (freenet/river#325). Seeded ONLY for messages actually shown at start
        // (below) and for messages surfaced live thereafter (lazily by
        // emit_reaction_changes). A reaction on a pre-existing message the stream
        // never showed is therefore NOT surfaced — the same "only for messages
        // the stream displayed" rule the deletion path follows (#324 review).
        let mut seen_reactions: HashMap<String, String> = HashMap::new();
        let mut new_message_count = 0;
        let start_time = std::time::Instant::now();

        // Fetch current room state to pre-populate seen_messages and trigger
        // migration if needed (get_room calls ensure_room_migrated internally).
        let contract_key = self.owner_vk_to_contract_key(room_owner_key);
        let contract_instance_id = *contract_key.id();
        {
            let mut room_state = self.get_room(room_owner_key, false).await?;
            // Decrypt private-room content for display (no-op for public rooms).
            // Must run before the immutable `display_msgs` borrow below.
            let secrets = self.room_display_secrets(room_owner_key, &mut room_state);

            // Determine which messages will be displayed initially (the last N
            // non-deleted). Only these count as "shown" for deletion purposes.
            let display_msgs: Vec<_> = room_state.recent_messages.display_messages().collect();
            let display_start = if initial_messages > 0 {
                display_msgs.len().saturating_sub(initial_messages)
            } else {
                display_msgs.len() // display nothing
            };
            let shown_keys: HashSet<String> = display_msgs[display_start..]
                .iter()
                .map(|m| monitor_seen_key(m))
                .collect();

            // Mark ALL non-action messages as seen (key -> effective content),
            // including deleted ones, so old messages aren't re-shown as new
            // (https://github.com/freenet/river/issues/173) and later edits are
            // detected as content changes.
            for msg in &room_state.recent_messages.messages {
                if !msg.message.content.is_action() {
                    seen_messages.insert(
                        monitor_seen_key(msg),
                        message_display_text_with_secrets(&room_state, msg, &secrets),
                    );
                }
            }

            // Suppress live deletion events for every pre-existing message we are
            // NOT showing now (already-deleted ones, and history outside the
            // initial window — including the default initial_messages == 0 which
            // shows nothing). Only deletions of messages the stream actually
            // showed — or emits live later — are surfaced. #324 review.
            deleted_emitted.extend(deletions_to_suppress_at_start(
                &room_state.recent_messages.messages,
                &shown_keys,
            ));

            // Show the last N display messages.
            for (i, msg) in display_msgs.iter().enumerate() {
                if i >= display_start {
                    // Seed the reactions fingerprint for each shown message so
                    // reactions already present at startup aren't re-emitted as a
                    // live change; only later changes to them surface.
                    seen_reactions.insert(
                        monitor_seen_key(msg),
                        reactions_fingerprint(room_state.recent_messages.reactions(&msg.id())),
                    );
                    Self::output_message(
                        &room_state,
                        msg,
                        room_owner_key,
                        &format,
                        false,
                        &secrets,
                    )?;
                }
            }
        }

        // Subscribe to the contract
        {
            let subscribe_request = ContractRequest::Subscribe {
                key: contract_instance_id, // Subscribe uses ContractInstanceId
                summary: None,
            };

            let client_request = ClientRequest::ContractOp(subscribe_request);

            let mut web_api = self.web_api.lock().await;
            web_api
                .send(client_request)
                .await
                .map_err(|e| anyhow!("Failed to send SUBSCRIBE request: {}", e))?;

            // Wait for subscription response (30s to accommodate slow gateways)
            let response = match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                web_api.recv(),
            )
            .await
            {
                Ok(result) => result.map_err(|e| anyhow!("Failed to receive response: {}", e))?,
                Err(_) => return Err(anyhow!("Timeout waiting for SUBSCRIBE response")),
            };

            match response {
                HostResponse::ContractResponse(ContractResponse::SubscribeResponse {
                    subscribed,
                    ..
                }) => {
                    if subscribed {
                        if matches!(format, OutputFormat::Human) {
                            eprintln!("Successfully subscribed. Waiting for updates...\n");
                        }
                    } else {
                        return Err(anyhow!("Failed to subscribe to contract"));
                    }
                }
                _ => return Err(anyhow!("Unexpected response to SUBSCRIBE request")),
            }
        }

        // Set up Ctrl+C handler
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel(1);

        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            let _ = shutdown_tx.send(()).await;
        });

        // Main loop: wait for UpdateNotification messages
        loop {
            // Check for shutdown signal
            if shutdown_rx.try_recv().is_ok() {
                if matches!(format, OutputFormat::Human) {
                    eprintln!("\nStopped monitoring.");
                }
                return Ok(());
            }

            // Check timeout
            if timeout_secs > 0 && start_time.elapsed().as_secs() >= timeout_secs {
                debug!("Timeout reached, exiting subscription stream");
                return Ok(());
            }

            // Check max messages
            if max_messages > 0 && new_message_count >= max_messages {
                debug!("Maximum message count reached, exiting subscription stream");
                return Ok(());
            }

            // Wait for next message with a short timeout to allow checking shutdown
            let mut web_api = self.web_api.lock().await;
            let recv_result =
                tokio::time::timeout(std::time::Duration::from_millis(500), web_api.recv()).await;

            match recv_result {
                Ok(Ok(HostResponse::ContractResponse(ContractResponse::UpdateNotification {
                    key,
                    update,
                }))) => {
                    // We received an update notification
                    debug!("Received update notification for contract: {}", key.id());

                    // Any notification — a delta (INCLUDING edit/delete/reaction
                    // action deltas) or a full-state update — can change what
                    // should be shown. Rather than parse the delta and skip
                    // actions (which made the stream oblivious to edits), re-fetch
                    // the authoritative full state and emit any NEW or EDITED
                    // messages. Deleted messages are excluded by display_messages
                    // and stay marked seen, so #173 (phantom deleted messages)
                    // still holds. The delta payload itself is advisory here.
                    let _ = update;
                    drop(web_api); // get_room needs the web_api lock
                    match self.get_room(room_owner_key, false).await {
                        Ok(mut room_state) => {
                            // Decrypt private-room content for display (no-op for public rooms).
                            let secrets =
                                self.room_display_secrets(room_owner_key, &mut room_state);
                            Self::emit_new_and_edited(
                                &room_state,
                                &mut seen_messages,
                                &mut deleted_emitted,
                                &mut seen_reactions,
                                room_owner_key,
                                &format,
                                max_messages,
                                &mut new_message_count,
                                &secrets,
                            )?;
                            Self::emit_deletions(
                                &room_state,
                                &seen_messages,
                                &mut deleted_emitted,
                                room_owner_key,
                                &format,
                                &secrets,
                            )?;
                            // Surface reactions added/removed since a message was
                            // already streamed. Runs AFTER emit_new_and_edited so a
                            // brand-new message is seeded (not re-emitted) here.
                            Self::emit_reaction_changes(
                                &room_state,
                                &mut seen_reactions,
                                room_owner_key,
                                &format,
                                &secrets,
                            )?;
                        }
                        Err(e) => {
                            debug!("Failed to fetch room state after notification: {}", e);
                        }
                    }
                    if max_messages > 0 && new_message_count >= max_messages {
                        return Ok(());
                    }
                }
                Ok(Ok(other)) => {
                    // Other message type, log and continue
                    debug!("Received unexpected message: {:?}", other);
                }
                Ok(Err(e)) => {
                    // WebSocket error
                    return Err(anyhow!("WebSocket error: {}", e));
                }
                Err(_) => {
                    // Timeout, continue loop (allows checking shutdown signal)
                }
            }
        }
    }
}

/// Resolve the caller's own CANONICAL `member_info` record to republish from —
/// the shared base for `set_nickname`'s nickname change and
/// `update_own_deputies`'s deputy add/revoke. Returns a clone of the winning
/// `MemberInfo` (version, nickname, deputies) so callers can freely rebuild
/// one field.
///
/// Routes through `MemberInfoV1::canonical`, NOT a bare `.find()` (#411 round
/// 8 item A): `verify` deliberately ACCEPTS a state carrying more than one
/// `member_info` record for the same member (migration-safety), so a client
/// can hold such a duplicate-containing state before cleanup runs. A bare
/// first-match `.find()` could then read a LOSING (e.g. revoked) record, and
/// republishing it at `version + 1` would resurrect that losing record at a
/// HIGHER rank — reactivating revoked deputy authority. `canonical` always
/// selects the highest-`member_info_rank` record, matching what
/// `apply_delta` / `summarize` converge on.
fn resolve_own_member_info_base(
    room_state: &ChatRoomStateV1,
    member_id: MemberId,
) -> Option<MemberInfo> {
    room_state
        .member_info
        .canonical(member_id)
        .map(|info| info.member_info.clone())
}

/// Resolve a `member set-deputy` / `revoke-deputy` short id to a full `MemberId`.
///
/// Primary: a present member, matched via their `member_info` (same rule as
/// `ban_member` — full-string prefix, or a case-insensitive first-8-chars match).
/// Revoke fallback (#411 round 4 C): when `allow_deputy_fallback` is set (the
/// revoke path), also match the caller's OWN `deputies`, so a deputy who was
/// pruned for inactivity — their `member_info` is gone but their id still sits in
/// the caller's signed deputies list — can still be revoked.
fn resolve_deputy_target(
    member_infos: &[AuthorizedMemberInfo],
    own_deputies: &[MemberId],
    member_id_short: &str,
    allow_deputy_fallback: bool,
) -> Option<MemberId> {
    let matches = |id: &MemberId| {
        let s = id.to_string();
        s.starts_with(member_id_short) || s[..8.min(s.len())].eq_ignore_ascii_case(member_id_short)
    };
    member_infos
        .iter()
        .map(|info| info.member_info.member_id)
        .find(|id| matches(id))
        .or_else(|| {
            if allow_deputy_fallback {
                own_deputies.iter().copied().find(|id| matches(id))
            } else {
                None
            }
        })
}

#[cfg(test)]
mod deputy_resolve_tests {
    use super::resolve_deputy_target;
    use ed25519_dalek::SigningKey;
    use river_core::room_state::member::MemberId;
    use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};

    fn info(sk: &SigningKey) -> AuthorizedMemberInfo {
        let id: MemberId = sk.verifying_key().into();
        AuthorizedMemberInfo::new_with_member_key(
            MemberInfo::new_public(id, 0, "n".to_string()),
            sk,
        )
    }

    fn short(id: &MemberId) -> String {
        let s = id.to_string();
        s[..8.min(s.len())].to_string()
    }

    /// #411 round 4 C: revoke can resolve a pruned deputy (no member_info) from
    /// the caller's own deputies list; deputize cannot resolve a non-member.
    #[test]
    fn revoke_resolves_pruned_deputy_from_own_deputies_list() {
        // Distinct fixed seeds → distinct keys (the cli crate's dalek does not
        // enable the `rand` `generate` helper; mirrors the other cli tests).
        let present = SigningKey::from_bytes(&[7u8; 32]);
        let pruned_deputy = SigningKey::from_bytes(&[9u8; 32]);
        let present_id: MemberId = present.verifying_key().into();
        let pruned_id: MemberId = pruned_deputy.verifying_key().into();

        // Only the present member has a member_info entry; the pruned deputy's
        // member_info is gone but their id still sits in own_deputies.
        let member_infos = vec![info(&present)];
        let own_deputies = vec![pruned_id];

        // A present member resolves via the primary lookup in both modes.
        assert_eq!(
            resolve_deputy_target(&member_infos, &own_deputies, &short(&present_id), false),
            Some(present_id)
        );

        // The pruned deputy is NOT resolvable when deputizing (no member_info)...
        assert_eq!(
            resolve_deputy_target(&member_infos, &own_deputies, &short(&pruned_id), false),
            None,
            "deputize must not resolve a non-member"
        );
        // ...but IS resolvable when revoking (fallback to the caller's deputies).
        assert_eq!(
            resolve_deputy_target(&member_infos, &own_deputies, &short(&pruned_id), true),
            Some(pruned_id),
            "revoke resolves a pruned deputy from the caller's own deputies list"
        );

        // An unknown short id resolves to nothing even with the fallback.
        assert_eq!(
            resolve_deputy_target(&member_infos, &own_deputies, "zzzzzzzz", true),
            None
        );
    }
}

#[cfg(test)]
mod resolve_own_member_info_base_tests {
    use super::resolve_own_member_info_base;
    use ed25519_dalek::SigningKey;
    use river_core::room_state::member::MemberId;
    use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo, MemberInfoV1};
    use river_core::room_state::ChatRoomStateV1;

    /// #411 round 8 item A (CLI Fix A): `set_nickname` and `update_own_deputies`
    /// republish the caller's OWN `member_info` at `version + 1`, carrying
    /// forward the previous `deputies` list. `MemberInfoV1::verify`
    /// deliberately ACCEPTS a state carrying more than one `member_info`
    /// record for the same member (migration-safety), so a client can hold
    /// such a duplicate-containing state before cleanup runs — e.g. a `grant`
    /// at v1 (deputies=[D]) and a `revoke` at v2 (deputies=[]).
    ///
    /// A bare first-match `.find()` over the raw vector could read the
    /// `grant` (if it happens to come first) and republish `deputies=[D]` at
    /// v3 — RESURRECTING revoked deputy authority at a rank higher than the
    /// revoke. `resolve_own_member_info_base` MUST always select the
    /// CANONICAL (highest-rank) record — the revoke — regardless of vector
    /// order, so the republish base is version 2 / deputies=[], and any
    /// `version + 1` built from it is 3, never resurrecting the grant.
    ///
    /// This test FAILS if `resolve_own_member_info_base` is reverted to a
    /// bare `.find(|info| info.member_info.member_id == member_id)` in the
    /// "grant-then-revoke" vector order (the first match would be the grant).
    #[test]
    fn republish_base_reads_canonical_revoke_not_first_match_grant() {
        let member_sk = SigningKey::from_bytes(&[21u8; 32]);
        let member_id: MemberId = member_sk.verifying_key().into();
        let deputy_id: MemberId = SigningKey::from_bytes(&[22u8; 32]).verifying_key().into();

        // grant @ v1: deputies=[deputy_id].
        let mut grant_mi = MemberInfo::new_public(member_id, 1, "nick".to_string());
        grant_mi.deputies = vec![deputy_id];
        let grant = AuthorizedMemberInfo::new_with_member_key(grant_mi, &member_sk);

        // revoke @ v2 (higher rank): deputies cleared.
        let mut revoke_mi = MemberInfo::new_public(member_id, 2, "nick".to_string());
        revoke_mi.deputies = vec![];
        let revoke = AuthorizedMemberInfo::new_with_member_key(revoke_mi, &member_sk);

        for (label, entries) in [
            ("grant-then-revoke", vec![grant.clone(), revoke.clone()]),
            ("revoke-then-grant", vec![revoke.clone(), grant.clone()]),
        ] {
            let state = ChatRoomStateV1 {
                member_info: MemberInfoV1 {
                    member_info: entries,
                },
                ..Default::default()
            };

            let base = resolve_own_member_info_base(&state, member_id)
                .unwrap_or_else(|| panic!("expected a base record ({label})"));

            assert_eq!(
                base.version, 2,
                "republish base must be the v2 revoke, not the v1 grant ({label})"
            );
            assert!(
                base.deputies.is_empty(),
                "republish base must carry the revoke's EMPTY deputies, never \
                 resurrecting the grant's [deputy] ({label})"
            );
        }
    }

    /// No `member_info` at all for this member → no base (both writers treat
    /// this as version 0 / empty deputies, or an error for
    /// `update_own_deputies`).
    #[test]
    fn no_member_info_returns_none() {
        let state = ChatRoomStateV1::default();
        let member_id: MemberId = SigningKey::from_bytes(&[23u8; 32]).verifying_key().into();
        assert!(resolve_own_member_info_base(&state, member_id).is_none());
    }
}

/// Tests for the `Invitation` struct's wire format (issue freenet/river#302).
/// The CLI invitation MUST stay byte-identical to the UI's
/// `ui::components::members::Invitation` — the UI's tests
/// (`members::tests::invitation_cbor_*`) pin the same shape on that side; keep
/// the two suites in step.
#[cfg(test)]
mod invitation_tests {
    use super::*;
    use river_core::room_state::member::Member;

    /// Build a deterministic test `Invitation` with the given `room_secrets`.
    fn fixture(room_secrets: Vec<(u32, [u8; 32])>) -> Invitation {
        let inviter = SigningKey::from_bytes(&[1u8; 32]);
        let invitee_signing_key = SigningKey::from_bytes(&[2u8; 32]);
        let owner_vk = SigningKey::from_bytes(&[3u8; 32]).verifying_key();
        let member = Member {
            owner_member_id: owner_vk.into(),
            member_vk: invitee_signing_key.verifying_key(),
            invited_by: inviter.verifying_key().into(),
        };
        Invitation {
            room: owner_vk,
            invitee_signing_key,
            invitee: AuthorizedMember::new(member, &inviter),
            room_secrets,
        }
    }

    /// Frozen cross-side wire-format fixture (issue freenet/river#302/#305).
    ///
    /// A base58(CBOR)-encoded [`Invitation`] with every field populated and
    /// two `room_secrets` entries (non-contiguous versions 0 and 3). The
    /// **same string literal** appears in the UI at
    /// `ui/src/components/members.rs`
    /// (`tests::INVITATION_FIXED_FIXTURE_V302`). Both sides decode it,
    /// assert every field, then re-encode and assert the bytes are
    /// byte-identical — so a `#[serde(rename = …)]` slip, a field reorder,
    /// a serde-attr drift, or a field added to one side but not the other
    /// can no longer compile-and-test-clean while silently breaking the
    /// CLI↔UI invitation exchange.
    ///
    /// **Do NOT regenerate this string casually.** It pins the on-wire
    /// format. If a future change legitimately alters the encoding, both
    /// copies (here and in the UI) must change together and the diff must
    /// be reviewed as a wire-format change. The string was produced once,
    /// deterministically, from the seeds in
    /// [`fixed_fixture_expected_invitation`] (ed25519 signing is
    /// deterministic per RFC 8032, so the bytes are reproducible).
    const INVITATION_FIXED_FIXTURE_V302: &str = "6DdkgteQ42ZdqjP42dauXJKUPV7Pb4YG5wxPzvBDezf3pwCkWX5ENtvTM8Eb9bVzDTG986W4SEY6MVx653EuNkBYhfTx7FM7uFHy3bJng5xoq8S6gfwuau9AgvWEixELwY7Pn9hErx6rymdPeBrpBouZgKkSLCbSqteJL3r1x8adRXkJVfDd8N9P1L9Uorah6J6sxisDuBcT3TZ71zmWaHkWwEptej7DUNUxCruLXjLGcJdWUaYP2YRAP5siqbNUz1rL9Jh5ZK7t8sq2p7WBSJasSyLuSJhDDw2qmRs5nGexupvbcimptn1xQBdzNa6q3bgzt8Qka3Ror5AD7iN6UNpGQPqwgrmvX6g8q2zVMDKh1JeEP9tezNtpmige3WvwRMg2wKk7pFnLNaeGyutEVQrsrd73D9TsB1Mkz86WwxMU8pKvonLgr2TB9yJdiX1BBkDPRZ6yE2bEzxyeo3PZ6t9Nw4WVszSBnFDkAKzAnCoHdo9qpm6n4iY5R6rsANPn75WDiUM16UyqzVsYdWH2JhoVuvpz7D8HUgbGcjTDsMxi33aERdtd7vG24oDMMsKYYNP6VGdXfyRWKm7LUk9M1hFyD1Sf9FZksUxpp924mRNyaJUCniR9pY984jDUrNE3gCuK1PoF9ShtCvEd";

    /// The exact `Invitation` the frozen [`INVITATION_FIXED_FIXTURE_V302`]
    /// string decodes to. Reconstructs it from the same fixed seeds used to
    /// generate the fixture: inviter `[1u8; 32]`, invitee `[2u8; 32]`, owner
    /// `[3u8; 32]`, with the inviter (a non-owner) signing the member. The
    /// UI keeps a byte-identical counterpart; keep the two in step.
    fn fixed_fixture_expected_invitation() -> Invitation {
        let inviter = SigningKey::from_bytes(&[1u8; 32]);
        let invitee_signing_key = SigningKey::from_bytes(&[2u8; 32]);
        let owner_vk = SigningKey::from_bytes(&[3u8; 32]).verifying_key();
        let member = Member {
            owner_member_id: owner_vk.into(),
            invited_by: inviter.verifying_key().into(),
            member_vk: invitee_signing_key.verifying_key(),
        };
        Invitation {
            room: owner_vk,
            invitee_signing_key,
            invitee: AuthorizedMember::new(member, &inviter),
            room_secrets: vec![(0u32, [0xA1u8; 32]), (3u32, [0xB2u8; 32])],
        }
    }

    /// Cross-side fixed-vector test (issue freenet/river#305). Decodes the
    /// frozen [`INVITATION_FIXED_FIXTURE_V302`] string, asserts every field,
    /// then re-encodes and asserts the bytes are byte-identical to the
    /// fixture. The UI runs the identical test against the same string in
    /// `ui/src/components/members.rs`, so the two sides cannot silently
    /// diverge on the invitation wire format.
    #[test]
    fn invitation_decodes_frozen_cross_side_fixture() {
        // Decode using the CLI's exact production wire path:
        // bs58-decode → ciborium-from-reader (see `accept_invitation`).
        let raw = bs58::decode(INVITATION_FIXED_FIXTURE_V302)
            .into_vec()
            .expect("frozen fixture must base58-decode on the CLI side");
        let decoded: Invitation = ciborium::de::from_reader(&raw[..])
            .expect("frozen fixture must CBOR-decode on the CLI side");

        let expected = fixed_fixture_expected_invitation();

        // Assert every field individually so a drift points at the exact
        // field that diverged, not just "the structs differ".
        assert_eq!(decoded.room, expected.room, "room field drifted");
        assert_eq!(
            decoded.invitee_signing_key.to_bytes(),
            expected.invitee_signing_key.to_bytes(),
            "invitee_signing_key field drifted"
        );
        assert_eq!(decoded.invitee, expected.invitee, "invitee field drifted");
        assert_eq!(
            decoded.room_secrets, expected.room_secrets,
            "room_secrets field drifted"
        );
        assert_eq!(
            decoded.room_secrets,
            vec![(0u32, [0xA1u8; 32]), (3u32, [0xB2u8; 32])],
            "room_secrets must carry the two frozen entries exactly"
        );
        assert_eq!(decoded, expected, "decoded invitation must match expected");

        // Re-encode using the CLI's exact production wire path
        // (ciborium-into-writer → bs58-encode, see the invite builder) and
        // assert byte-identical to the frozen string. This is the
        // load-bearing assertion: it proves the CLI's serializer emits the
        // same bytes the fixture was frozen at, so a serde-attr or
        // field-order change would fail here.
        let mut reencoded_bytes = Vec::new();
        ciborium::ser::into_writer(&decoded, &mut reencoded_bytes).expect("re-encode");
        let reencoded = bs58::encode(reencoded_bytes).into_string();
        assert_eq!(
            reencoded, INVITATION_FIXED_FIXTURE_V302,
            "re-encoding the decoded invitation must reproduce the frozen \
             fixture byte-for-byte; the CLI wire format has drifted from the \
             frozen vector (and therefore from the UI)"
        );
    }

    /// CBOR round-trip preserves `room_secrets` byte-for-byte. The encoded
    /// invitation is fingerprinted for processed-invite dedup, so the
    /// encode/decode cycle must be stable.
    #[test]
    fn invitation_cbor_round_trip_with_secrets() {
        let original = fixture(vec![(0, [0xAAu8; 32]), (1, [0xBBu8; 32])]);
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&original, &mut bytes).expect("encode");
        let decoded: Invitation = ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(original, decoded);
        assert_eq!(
            decoded.room_secrets,
            vec![(0, [0xAAu8; 32]), (1, [0xBBu8; 32])]
        );
    }

    /// Backward compatibility: a CBOR-encoded invitation that PRE-dates
    /// `room_secrets` (i.e. lacks the field entirely) must still decode, with
    /// `room_secrets` defaulting to `Vec::new()`. This is the same
    /// `#[serde(default)]` invariant that keeps UI-issued legacy invitations
    /// decodable by post-#302 riverctl.
    #[test]
    fn invitation_cbor_decodes_legacy_invitation_without_secrets_field() {
        // Build a pre-#302 wire shape: same three fields as the original CLI
        // `Invitation`, serialized as a CBOR map. `serde`'s `#[serde(default)]`
        // on `room_secrets` should fill in `vec![]`.
        #[derive(serde::Serialize)]
        struct LegacyInvitation {
            room: VerifyingKey,
            invitee_signing_key: SigningKey,
            invitee: AuthorizedMember,
        }
        let template = fixture(vec![]);
        let legacy = LegacyInvitation {
            room: template.room,
            invitee_signing_key: template.invitee_signing_key.clone(),
            invitee: template.invitee.clone(),
        };
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&legacy, &mut bytes).expect("encode");
        let decoded: Invitation = ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(decoded.room, template.room);
        assert_eq!(decoded.invitee, template.invitee);
        assert!(
            decoded.room_secrets.is_empty(),
            "legacy invitation must decode with empty room_secrets"
        );
    }

    /// `room_secrets` defaults to empty when the inviter holds none — a
    /// public-room invitation must NOT carry any per-version entry, so the
    /// wire bytes stay small.
    #[test]
    fn invitation_with_empty_secrets_round_trips() {
        let original = fixture(vec![]);
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&original, &mut bytes).expect("encode");
        let decoded: Invitation = ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(decoded, original);
        assert!(decoded.room_secrets.is_empty());
    }

    /// The hand-written `Debug` REDACTS `room_secrets` — `{:?}`-logging an
    /// invitation must not print the secret bytes to stdout/logs. Mirrors the
    /// UI's `Debug` for `ui::components::members::Invitation` (added in #301
    /// review). We check both that the redaction text appears AND that the
    /// derived `Debug` form of `[u8; 32]` (`[205, 205, 205, ..., 205]`) is
    /// absent — the literal byte 0xCD repeats 32 times, which would only
    /// appear in a non-redacted print.
    #[test]
    fn invitation_debug_redacts_room_secrets() {
        let secret_bytes = [0xCDu8; 32];
        let inv = fixture(vec![(0, secret_bytes), (1, [0xEFu8; 32])]);
        let debug_output = format!("{:?}", inv);
        assert!(
            debug_output.contains("redacted"),
            "Debug output should mention redaction: {}",
            debug_output
        );
        // The placeholder must still report the COUNT so an operator can
        // tell the field was populated.
        assert!(
            debug_output.contains("2 room secret(s)"),
            "Debug output should report the secret count: {}",
            debug_output
        );
        // The unredacted `[u8; 32]` Debug form would print the byte 32 times
        // in a row separated by ", " — anchor on that exact shape to avoid
        // false positives from unrelated key material that happens to contain
        // the substring "205".
        let unredacted_form = "[205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205, 205]";
        assert!(
            !debug_output.contains(unredacted_form),
            "Debug output must not print secret bytes (32x 0xCD in array form): {}",
            debug_output
        );
        let unredacted_ef =
            "[239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239, 239]";
        assert!(
            !debug_output.contains(unredacted_ef),
            "Debug output must not print secret bytes (32x 0xEF in array form): {}",
            debug_output
        );
    }
}

#[cfg(test)]
mod migration_recovery_tests {
    use super::*;

    /// The legacy registry derives a contract key exactly as the live code path
    /// (`compute_contract_key` / `owner_vk_to_contract_key`) does. If this ever
    /// drifts, every backward probe would target the wrong contract instance
    /// and silently fail to recover any room. (freenet/river#292)
    #[test]
    fn legacy_derivation_matches_live_key_for_current_wasm() {
        // Any valid signing key works; SigningKey::from_bytes treats the bytes
        // as the seed and is infallible for any 32-byte input.
        let owner = SigningKey::from_bytes(&[7u8; 32]).verifying_key();
        let current_code_hash: [u8; 32] = *blake3::hash(ROOM_CONTRACT_WASM).as_bytes();
        let via_registry =
            river_core::migration::contract_key_for_code_hash(&owner, &current_code_hash);
        let via_live = compute_contract_key(&owner);
        assert_eq!(
            via_registry.id(),
            via_live.id(),
            "registry-derived key must match the live owner_vk_to_contract_key derivation"
        );
    }

    /// The current room-contract WASM must NOT be in the legacy registry — the
    /// registry holds only *previous* generations. Listing the current hash
    /// would make a probe redundantly re-fetch the current contract.
    #[test]
    fn current_wasm_is_not_in_legacy_registry() {
        let current_code_hash: [u8; 32] = *blake3::hash(ROOM_CONTRACT_WASM).as_bytes();
        assert!(
            !river_core::migration::LEGACY_ROOM_CONTRACT_CODE_HASHES.contains(&current_code_hash),
            "current room-contract WASM hash {} is listed in legacy_room_contracts.toml; \
             the registry must contain only previous generations",
            blake3::hash(ROOM_CONTRACT_WASM).to_hex()
        );
    }

    /// Build a `ChatRoomStateV1` carrying an upgrade pointer to the contract
    /// instance whose 32-byte id is `target`.
    fn state_pointing_at(target: [u8; 32]) -> ChatRoomStateV1 {
        use river_core::room_state::upgrade::{AuthorizedUpgradeV1, OptionalUpgradeV1, UpgradeV1};
        let sk = SigningKey::from_bytes(&[9u8; 32]);
        let upgrade = UpgradeV1 {
            owner_member_id: MemberId::from(&sk.verifying_key()),
            version: 1,
            new_chatroom_address: blake3::Hash::from(target),
        };
        ChatRoomStateV1 {
            upgrade: OptionalUpgradeV1(Some(AuthorizedUpgradeV1::new(upgrade, &sk))),
            ..Default::default()
        }
    }

    /// `next_upgrade_hop` returns `None` for a state with no upgrade pointer —
    /// the chain walk terminates.
    #[test]
    fn next_upgrade_hop_none_without_pointer() {
        let mut visited = HashSet::new();
        let known = HashSet::new();
        assert!(next_upgrade_hop(&ChatRoomStateV1::default(), &mut visited, &known).is_none());
    }

    /// `next_upgrade_hop` follows a pointer to an unvisited, unknown contract
    /// (a genuine forward upgrade) and records it in the visited-set.
    #[test]
    fn next_upgrade_hop_follows_unvisited_pointer() {
        let target = [5u8; 32];
        let mut visited = HashSet::new();
        let known = HashSet::new();
        let next = next_upgrade_hop(&state_pointing_at(target), &mut visited, &known)
            .expect("a pointer to a fresh contract must be followed");
        assert_eq!(next, ContractInstanceId::new(target));
        assert!(
            visited.contains(&next),
            "the followed target must be recorded"
        );
    }

    /// `next_upgrade_hop` returns `None` when the pointer targets an
    /// already-visited contract — the cycle guard that stops a chain that
    /// loops back on itself.
    #[test]
    fn next_upgrade_hop_stops_on_cycle() {
        let target = [5u8; 32];
        let mut visited = HashSet::new();
        let known = HashSet::new();
        visited.insert(ContractInstanceId::new(target));
        assert!(
            next_upgrade_hop(&state_pointing_at(target), &mut visited, &known).is_none(),
            "a pointer back to an already-visited contract must stop the walk"
        );
    }

    /// Core regression for freenet/river#427: `next_upgrade_hop` REFUSES to
    /// follow a pointer whose target is a KNOWN generation (the current bundled
    /// key or a legacy/backward key). Such a pointer is current-or-older, not a
    /// genuine forward upgrade — following it walked riverctl backward onto a
    /// stale generation and returned the older state.
    #[test]
    fn next_upgrade_hop_refuses_known_backward_key() {
        let target = [5u8; 32];
        let target_id = ContractInstanceId::new(target);
        // The pointer target is a KNOWN key (e.g. an older generation for this
        // owner). Even though it is unvisited, it must not be followed.
        let mut known = HashSet::new();
        known.insert(target_id);
        let mut visited = HashSet::new();
        assert!(
            next_upgrade_hop(&state_pointing_at(target), &mut visited, &known).is_none(),
            "a pointer to a known (current-or-legacy) generation must NOT be followed"
        );
        assert!(
            !visited.contains(&target_id),
            "a refused known-key hop must not be recorded as visited"
        );
    }

    /// Build a minimal owner-signed `ChatRoomStateV1` carrying the given
    /// owner-authored public messages. Owner-authored messages survive
    /// `post_apply_cleanup` (`author == owner_id`), so no explicit membership is
    /// needed for them to persist through a merge.
    fn owner_state_with_messages(owner_sk: &SigningKey, texts: &[&str]) -> ChatRoomStateV1 {
        use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
        use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
        let owner_vk = owner_sk.verifying_key();
        let config = Configuration {
            owner_member_id: owner_vk.into(),
            ..Default::default()
        };
        let mut state = ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(config, owner_sk),
            ..Default::default()
        };
        for (i, text) in texts.iter().enumerate() {
            let message = MessageV1 {
                room_owner: owner_vk.into(),
                author: MemberId::from(&owner_vk),
                content: RoomMessageBody::public((*text).to_string()),
                time: std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(i as u64),
            };
            state
                .recent_messages
                .messages
                .push(AuthorizedMessageV1::new(message, owner_sk));
        }
        state.recent_messages.rebuild_actions_state();
        state
    }

    /// Following a genuine forward pointer MERGES the followed state into the
    /// authoritative base instead of REPLACING it (freenet/river#427): a message
    /// present on the base survives even when the followed state lacks it. The
    /// pre-#427 `state = next_state` would have dropped it.
    #[test]
    fn fold_forward_upgrade_state_keeps_base_messages() {
        let owner_sk = SigningKey::from_bytes(&[42u8; 32]);
        let params = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };

        // Base (current generation) holds a message the followed state lacks.
        let base = owner_state_with_messages(&owner_sk, &["on the current key"]);
        // Followed (pointer target) has the same owner/config but NO messages.
        let followed = owner_state_with_messages(&owner_sk, &[]);

        let merged = fold_forward_upgrade_state(&base, &followed, &params);

        let texts: Vec<String> = merged
            .recent_messages
            .messages
            .iter()
            .map(|m| message_display_text(&merged, m))
            .collect();
        assert!(
            texts.iter().any(|t| t == "on the current key"),
            "merge must preserve the base-key message the followed state lacked; got {texts:?}"
        );
    }

    /// The merge also ADDS content the followed (newer) generation carries that
    /// the base lacks — a genuine forward upgrade's new messages are not lost.
    #[test]
    fn fold_forward_upgrade_state_adds_followed_messages() {
        let owner_sk = SigningKey::from_bytes(&[43u8; 32]);
        let params = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };
        let base = owner_state_with_messages(&owner_sk, &["base msg"]);
        let followed = owner_state_with_messages(&owner_sk, &["base msg", "followed msg"]);

        let merged = fold_forward_upgrade_state(&base, &followed, &params);

        let texts: Vec<String> = merged
            .recent_messages
            .messages
            .iter()
            .map(|m| message_display_text(&merged, m))
            .collect();
        assert!(
            texts.iter().any(|t| t == "base msg"),
            "merge must keep the base message; got {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t == "followed msg"),
            "merge must add the followed generation's new message; got {texts:?}"
        );
    }

    /// The forward-migration PUT strips the upgrade pointer (freenet/river#427):
    /// the state PUT onto the new/current contract must carry `upgrade = None`,
    /// so migration never carries a stale forward/backward pointer forward.
    #[test]
    fn strip_upgrade_pointer_for_forward_put_clears_pointer() {
        let poisoned = state_pointing_at([5u8; 32]);
        assert!(
            poisoned.upgrade.0.is_some(),
            "precondition: the source state carries an upgrade pointer"
        );
        let cleaned = strip_upgrade_pointer_for_forward_put(&poisoned);
        assert!(
            cleaned.upgrade.0.is_none(),
            "the forward-migration PUT payload must carry no upgrade pointer"
        );
    }

    /// Stripping is a no-op on a state that already has no pointer — the rest of
    /// the state is untouched.
    #[test]
    fn strip_upgrade_pointer_for_forward_put_noop_without_pointer() {
        let owner_sk = SigningKey::from_bytes(&[44u8; 32]);
        let state = owner_state_with_messages(&owner_sk, &["hi"]);
        assert!(state.upgrade.0.is_none());
        let cleaned = strip_upgrade_pointer_for_forward_put(&state);
        assert!(cleaned.upgrade.0.is_none());
        assert_eq!(
            cleaned.recent_messages.messages.len(),
            1,
            "stripping must not disturb the rest of the state"
        );
    }

    /// Build an owner-signed upgrade pointer to `target`.
    fn pointer_to(owner_sk: &SigningKey, target: [u8; 32]) -> OptionalUpgradeV1 {
        use river_core::room_state::upgrade::{AuthorizedUpgradeV1, UpgradeV1};
        let upgrade = UpgradeV1 {
            owner_member_id: MemberId::from(&owner_sk.verifying_key()),
            version: 1,
            new_chatroom_address: blake3::Hash::from(target),
        };
        OptionalUpgradeV1(Some(AuthorizedUpgradeV1::new(upgrade, owner_sk)))
    }

    /// P2-1 (freenet/river#427): a multi-generation forward chain
    /// base -> gen+1 -> gen+2 is walked ALL the way to the newest generation,
    /// merging every generation's content into the authoritative base. The
    /// pre-P2-1 bug read the next hop from the merged accumulator (which keeps
    /// the base's own pointer, since a lower-version onward pointer does not
    /// merge in), so the visited-set stopped the walk after gen+1 and gen+2's
    /// content was lost — the same stale-list symptom on a 2+-generation-stale
    /// riverctl. Advancing the cursor from the FETCHED generation fixes it.
    #[tokio::test]
    async fn walk_upgrade_chain_follows_multi_generation_forward_chain() {
        let owner_sk = SigningKey::from_bytes(&[71u8; 32]);
        let params = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };

        let base_bytes = [10u8; 32];
        let gen1_bytes = [11u8; 32];
        let gen2_bytes = [12u8; 32];
        let base_id = ContractInstanceId::new(base_bytes);
        let gen1_id = ContractInstanceId::new(gen1_bytes);
        let gen2_id = ContractInstanceId::new(gen2_bytes);

        // base -> gen1 -> gen2 (gen2 terminates). Each carries its own message.
        let mut base = owner_state_with_messages(&owner_sk, &["base msg"]);
        base.upgrade = pointer_to(&owner_sk, gen1_bytes);
        let mut gen1 = owner_state_with_messages(&owner_sk, &["gen1 msg"]);
        gen1.upgrade = pointer_to(&owner_sk, gen2_bytes);
        let gen2 = owner_state_with_messages(&owner_sk, &["gen2 msg"]);

        let states: HashMap<ContractInstanceId, ChatRoomStateV1> =
            HashMap::from([(gen1_id, gen1), (gen2_id, gen2)]);

        // gen1/gen2 are unknown, newer-than-current generations → genuine
        // forward hops. base_id is the starting cursor.
        let known_keys: HashSet<ContractInstanceId> = HashSet::new();

        let (merged, resolved_id) =
            walk_upgrade_chain(base, base_id, &known_keys, &params, |next| {
                let s = states.get(&next).cloned();
                Box::pin(std::future::ready(s))
            })
            .await;

        // Discovery-only: the resolved id stays the base generation.
        assert_eq!(
            resolved_id, base_id,
            "the resolved id must stay the base generation (discovery-only follow)"
        );

        let texts: Vec<String> = merged
            .recent_messages
            .messages
            .iter()
            .map(|m| message_display_text(&merged, m))
            .collect();
        assert!(
            texts.iter().any(|t| t == "base msg"),
            "base content must be kept; got {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t == "gen1 msg"),
            "gen+1 content must be merged in; got {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t == "gen2 msg"),
            "gen+2 content must be merged in — the walk must reach the newest \
             generation, not stall after gen+1; got {texts:?}"
        );
    }

    /// A backward pointer at the head of the chain is still refused even by the
    /// multi-hop walk: `known_keys` (current + legacy) gates every hop.
    #[tokio::test]
    async fn walk_upgrade_chain_refuses_backward_head_pointer() {
        let owner_sk = SigningKey::from_bytes(&[72u8; 32]);
        let params = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };
        let base_bytes = [20u8; 32];
        let legacy_bytes = [21u8; 32];
        let base_id = ContractInstanceId::new(base_bytes);
        let legacy_id = ContractInstanceId::new(legacy_bytes);

        // base carries a BACKWARD pointer to a known legacy generation.
        let mut base = owner_state_with_messages(&owner_sk, &["fresh current msg"]);
        base.upgrade = pointer_to(&owner_sk, legacy_bytes);
        // The legacy generation holds only stale/older content.
        let legacy = owner_state_with_messages(&owner_sk, &["stale older msg"]);
        let states: HashMap<ContractInstanceId, ChatRoomStateV1> =
            HashMap::from([(legacy_id, legacy)]);

        // legacy_id is a KNOWN generation for this owner.
        let known_keys: HashSet<ContractInstanceId> = HashSet::from([legacy_id]);

        let (result, resolved_id) =
            walk_upgrade_chain(base, base_id, &known_keys, &params, |next| {
                let s = states.get(&next).cloned();
                Box::pin(std::future::ready(s))
            })
            .await;

        assert_eq!(resolved_id, base_id);
        let texts: Vec<String> = result
            .recent_messages
            .messages
            .iter()
            .map(|m| message_display_text(&result, m))
            .collect();
        assert!(
            texts.iter().any(|t| t == "fresh current msg"),
            "the fresh current-key content must be returned; got {texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t == "stale older msg"),
            "the backward pointer must NOT be followed to the stale generation; got {texts:?}"
        );
    }

    /// P2-3 + root-fix pin (freenet/river#427): every FORWARD-PUT choke point
    /// strips the upgrade pointer before its PUT. Guards against a future PUT
    /// site (or a refactor) silently dropping the strip and re-introducing
    /// poisoning of the current generation.
    #[test]
    fn forward_put_sites_strip_upgrade_pointer() {
        let src = include_str!("api.rs");
        for fn_name in [
            "async fn migrate_room_to_new_contract",
            "async fn put_room_state",
            "pub async fn republish_room",
        ] {
            let fn_start = src
                .find(fn_name)
                .unwrap_or_else(|| panic!("could not find `{fn_name}` in api.rs"));
            let rest = &src[fn_start..];
            let put_at = rest
                .find("ContractRequest::Put")
                .unwrap_or_else(|| panic!("`{fn_name}` has no ContractRequest::Put"));
            assert!(
                rest[..put_at].contains("strip_upgrade_pointer_for_forward_put"),
                "`{fn_name}` must call strip_upgrade_pointer_for_forward_put before its \
                 forward PUT (freenet/river#427)"
            );
        }
    }

    // --- freenet/river#398 phase 2b: the legacy recovery search now runs on
    // freenet_migrate's decision driver via `RiverCliProbeOps`. ---

    // Encode through the SAME production function `probe_legacy_bytes` uses, so
    // the round-trip tests exercise the real production encoding.
    fn encode_state(state: &ChatRoomStateV1) -> Vec<u8> {
        encode_probe_state(state).expect("a valid state serializes")
    }

    fn cli_id(n: u8) -> ContractInstanceId {
        ContractInstanceId::new([n; 32])
    }

    /// `RiverCliProbeOps` classification matches the pre-driver `try_get_state`:
    /// an owner-signed state is real (a hit), a default state is not (a miss),
    /// undecodable bytes decode to `None` (a miss), and `merge_with_local` is a
    /// pass-through — the CLI adopts the recovered network state verbatim and
    /// NEVER folds in device-local state (unlike the UI).
    #[test]
    fn river_cli_probe_ops_classifies_and_never_merges() {
        let owner_sk = SigningKey::from_bytes(&[71u8; 32]);
        let owner_vk = owner_sk.verifying_key();
        let ops = RiverCliProbeOps { owner_vk };

        let real = owner_state_with_messages(&owner_sk, &["hi"]);
        assert!(ops.is_real(&real), "an owner-signed state must be real");
        assert!(
            !ops.is_real(&ChatRoomStateV1::default()),
            "a default state must NOT be real (a miss)"
        );
        assert!(
            ops.decode(&encode_state(&real)).is_some(),
            "valid CBOR must decode"
        );
        assert!(
            ops.decode(&[0xffu8; 8]).is_none(),
            "undecodable bytes must map to None (a miss)"
        );

        // merge_with_local returns the recovered state untouched, ignoring the
        // local snapshot entirely. `real` and `local` carry distinct messages,
        // so a mistaken fold would change the message set.
        let local = owner_state_with_messages(&owner_sk, &["local-only message"]);
        let merged = ops.merge_with_local(real.clone(), &local);
        let real_ids: HashSet<_> = real
            .recent_messages
            .messages
            .iter()
            .map(|m| m.id())
            .collect();
        assert_eq!(
            merged.recent_messages.messages.len(),
            real.recent_messages.messages.len(),
            "the CLI must adopt the recovered state verbatim — no local merge"
        );
        assert!(
            merged
                .recent_messages
                .messages
                .iter()
                .all(|m| real_ids.contains(&m.id())),
            "no device-local message may appear in the merged result"
        );
    }

    /// `prepare_forward` must be identity: the CLI strips the upgrade pointer in
    /// `put_room_state` (freenet/river#427), NOT in the driver hook. Stripping
    /// here would change what is returned to the caller.
    #[test]
    fn river_cli_probe_ops_prepare_forward_is_identity() {
        let owner_sk = SigningKey::from_bytes(&[72u8; 32]);
        let ops = RiverCliProbeOps {
            owner_vk: owner_sk.verifying_key(),
        };
        // A state carrying a forward upgrade pointer (built via the module's
        // pointer helper) must come back through prepare_forward untouched.
        let mut state = owner_state_with_messages(&owner_sk, &["m"]);
        state.upgrade = pointer_to(&owner_sk, [9u8; 32]);
        let forwarded = ops.prepare_forward(state.clone());
        assert_eq!(
            forwarded.upgrade, state.upgrade,
            "prepare_forward must NOT strip the upgrade pointer (stripping happens \
             only in put_room_state — freenet/river#427)"
        );
    }

    /// End-to-end at the CLI level: two real generations, the driver adopts the
    /// NEWEST and never reads the older (anti-rollback), wiring `RiverCliProbeOps`
    /// into a real `ProbeDriver`.
    #[test]
    fn cli_driver_adopts_newest_real_generation() {
        let owner_sk = SigningKey::from_bytes(&[73u8; 32]);
        let owner_vk = owner_sk.verifying_key();
        let mut driver = ProbeDriver::new(
            RiverCliProbeOps { owner_vk },
            ChatRoomStateV1::default(),
            NewestFirst::assume_ordered(vec![cli_id(9), cli_id(5)]),
            SelectionPolicy::NewestFirstWins,
        );
        let Step::Get(newest) = driver.next_action() else {
            panic!("expected a GET")
        };
        assert_eq!(newest, cli_id(9), "the newest generation is probed first");
        driver.on_response(
            newest,
            &encode_state(&owner_state_with_messages(&owner_sk, &["newest"])),
        );
        assert_eq!(driver.next_action(), Step::Done);
        let Some(Outcome::Recovered { source, merged, .. }) = driver.take_outcome() else {
            panic!("expected recovery");
        };
        assert_eq!(
            source,
            cli_id(9),
            "an older generation must not shadow the newer"
        );
        assert!(merged.configuration.verify_signature(&owner_vk).is_ok());
    }

    /// End-to-end: every generation misses (a timeout then an undecodable
    /// response), so the driver reports SeedLocal — which
    /// `fetch_room_state_with_recovery` maps to the not-found error. The CLI
    /// never seeds device-local state.
    #[test]
    fn cli_driver_exhausts_to_seed_local() {
        let owner_sk = SigningKey::from_bytes(&[74u8; 32]);
        let mut driver = ProbeDriver::new(
            RiverCliProbeOps {
                owner_vk: owner_sk.verifying_key(),
            },
            ChatRoomStateV1::default(),
            NewestFirst::assume_ordered(vec![cli_id(2), cli_id(1)]),
            SelectionPolicy::NewestFirstWins,
        );
        let Step::Get(a) = driver.next_action() else {
            panic!()
        };
        driver.on_timeout(a); // absent generation
        let Step::Get(b) = driver.next_action() else {
            panic!()
        };
        driver.on_response(b, &[0xffu8; 8]); // undecodable → miss
        assert_eq!(driver.next_action(), Step::Done);
        assert!(
            matches!(driver.take_outcome(), Some(Outcome::SeedLocal { .. })),
            "exhaustion must be SeedLocal — which the CLI maps to the not-found Err, never a seed"
        );
    }

    /// Source-grep pin: `fetch_room_state_with_recovery` must drive the legacy
    /// search through the decision driver (newest-first), route the outcome
    /// through the pure `resolve_legacy_recovery_outcome` mapping, AND contain
    /// exactly ONE forward PUT — the CLI-never-seeds guarantee. A refactor that
    /// re-introduced a hand-rolled loop, or added a seed-arm PUT, would fail
    /// this pin.
    #[test]
    fn legacy_recovery_routes_through_decision_driver() {
        let full = include_str!("api.rs");
        let fn_start = full
            .find("async fn fetch_room_state_with_recovery")
            .expect("fetch_room_state_with_recovery must exist");
        // Bound the slice at the next fn so we only inspect this function body.
        let rest = &full[fn_start..];
        let body_end = rest
            .find("async fn probe_legacy_bytes")
            .expect("probe_legacy_bytes must follow fetch_room_state_with_recovery");
        let body = &rest[..body_end];

        // Match "NewestFirstWins" bare so a `use` that drops the
        // `SelectionPolicy::` prefix cannot false-fail this pin.
        assert!(
            body.contains("NewestFirstWins"),
            "the legacy recovery search must use the newest-first decision driver"
        );
        // The hop cap must be sized to the registry so the driver's default
        // 64-hop cap never silently truncates a >64-generation registry (the old
        // loop was unbounded).
        assert!(
            body.contains("with_max_hops"),
            "the driver must set with_max_hops (sized to the registry) so the default hop cap \
             can never strand the oldest legacy generations"
        );
        assert!(
            body.contains("resolve_legacy_recovery_outcome"),
            "the outcome->result mapping must go through the pure, unit-tested resolver, \
             not a hand-rolled match on Outcome"
        );
        // CLI-never-seeds: EXACTLY ONE forward PUT in the whole function. A
        // seed-arm PUT (or any second migration PUT) would make this 2 and slip
        // past a first-match `.find`. Exhaustion (SeedLocal / NoLegacy) maps to
        // the not-found Err inside the resolver (unit-tested separately) with no
        // PUT at all.
        assert_eq!(
            body.matches("put_room_state").count(),
            1,
            "fetch_room_state_with_recovery must contain exactly one put_room_state call — a \
             second would mean a seed-arm PUT slipped in (the CLI must never seed)"
        );
    }

    /// Build a `ChatRoomStateV1` with EVERY field non-default and owner-signed:
    /// configuration, members, member_info, bans, secrets, direct_messages,
    /// upgrade, version, and recent_messages carrying an edit + reaction +
    /// delete so the derived `actions_state` is non-trivially rebuilt.
    fn fully_populated_state(owner_sk: &SigningKey) -> ChatRoomStateV1 {
        use river_core::room_state::ban::{AuthorizedUserBan, UserBan};
        use river_core::room_state::direct_messages::sign_direct_message;
        use river_core::room_state::member::{AuthorizedMember, Member};
        use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
        use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
        use river_core::room_state::privacy::RoomCipherSpec;
        use river_core::room_state::secret::{
            AuthorizedEncryptedSecretForMember, AuthorizedSecretVersionRecord,
            EncryptedSecretForMemberV1, SecretVersionRecordV1,
        };
        use river_core::room_state::upgrade::{AuthorizedUpgradeV1, OptionalUpgradeV1, UpgradeV1};
        use river_core::room_state::version::StateVersion;

        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        // A distinct member (a fixed, owner-independent seed).
        let member_sk = SigningKey::from_bytes(&[201u8; 32]);
        let member_vk = member_sk.verifying_key();
        let member_id = MemberId::from(&member_vk);

        let mut state = ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(
                Configuration {
                    owner_member_id: owner_id,
                    ..Default::default()
                },
                owner_sk,
            ),
            ..Default::default()
        };

        state.members.members.push(AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk,
            },
            owner_sk,
        ));

        state
            .member_info
            .member_info
            .push(AuthorizedMemberInfo::new(
                MemberInfo::new_public(member_id, 1, "Nacho".to_string()),
                owner_sk,
            ));

        state.bans.0.push(AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(500),
                banned_user: member_id,
            },
            owner_id,
            owner_sk,
        ));

        state.secrets.current_version = 1;
        state
            .secrets
            .versions
            .push(AuthorizedSecretVersionRecord::new(
                SecretVersionRecordV1 {
                    version: 1,
                    cipher_spec: RoomCipherSpec::Aes256Gcm,
                    created_at: std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(10),
                },
                owner_sk,
            ));
        state
            .secrets
            .encrypted_secrets
            .push(AuthorizedEncryptedSecretForMember::new(
                EncryptedSecretForMemberV1 {
                    member_id,
                    secret_version: 1,
                    ciphertext: vec![1, 2, 3, 4],
                    nonce: [7u8; 12],
                    sender_ephemeral_public_key: [9u8; 32],
                    provider: owner_id,
                },
                owner_sk,
            ));

        state.direct_messages.messages.push(
            sign_direct_message(
                owner_sk,
                owner_id,
                member_id,
                &owner_vk,
                1234,
                vec![9, 9, 9],
            )
            .expect("owner->member DM signs"),
        );

        // recent_messages: two texts, then an edit + reaction on the first and a
        // delete of the second, so actions_state has edited_content, reactions,
        // and deleted entries after rebuild.
        let authored = |i: u64, body: RoomMessageBody| {
            AuthorizedMessageV1::new(
                MessageV1 {
                    room_owner: owner_id,
                    author: owner_id,
                    content: body,
                    time: std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(i),
                },
                owner_sk,
            )
        };
        let msg_a = authored(1, RoomMessageBody::public("hello".to_string()));
        let msg_b = authored(2, RoomMessageBody::public("to delete".to_string()));
        let a_id = msg_a.id();
        let b_id = msg_b.id();
        state.recent_messages.messages.push(msg_a);
        state.recent_messages.messages.push(msg_b);
        state.recent_messages.messages.push(authored(
            3,
            RoomMessageBody::edit(a_id.clone(), "hello (edited)".to_string()),
        ));
        state.recent_messages.messages.push(authored(
            4,
            RoomMessageBody::reaction(a_id, "👍".to_string()),
        ));
        state
            .recent_messages
            .messages
            .push(authored(5, RoomMessageBody::delete(b_id)));

        state.upgrade = OptionalUpgradeV1(Some(AuthorizedUpgradeV1::new(
            UpgradeV1 {
                owner_member_id: owner_id,
                version: 2,
                new_chatroom_address: blake3::Hash::from([5u8; 32]),
            },
            owner_sk,
        )));

        state.version = StateVersion(7);

        // Rebuild the derived actions cache so the fixture's actions_state
        // matches what `decode` rebuilds — the round-trip's fidelity anchor.
        state.recent_messages.rebuild_actions_state();
        state
    }

    /// The re-encode -> re-decode surface `probe_legacy_bytes` +
    /// `RiverCliProbeOps::decode` introduces MUST be lossless across every field,
    /// including the `#[serde(skip)]` `actions_state` (rebuilt in `decode`). This
    /// encodes a fully-populated state exactly as `probe_legacy_bytes` does and
    /// asserts the driver decodes back an identical value.
    #[test]
    fn river_cli_probe_ops_roundtrips_fully_populated_state() {
        let owner_sk = SigningKey::from_bytes(&[200u8; 32]);
        let ops = RiverCliProbeOps {
            owner_vk: owner_sk.verifying_key(),
        };
        let original = fully_populated_state(&owner_sk);

        assert!(
            ops.is_real(&original),
            "the fully-populated owner-signed state must classify as real"
        );
        // `encode_state` uses the same ciborium::ser::into_writer as
        // `probe_legacy_bytes`.
        let decoded = ops
            .decode(&encode_state(&original))
            .expect("a fully-populated state must decode");
        assert_eq!(
            decoded, original,
            "the re-encode -> re-decode round-trip (incl. rebuild_actions_state) must be \
             lossless across every field"
        );
        // Guard the fixture itself: it must actually exercise the actions rebuild.
        let actions = &original.recent_messages.actions_state;
        assert!(
            !actions.edited_content.is_empty()
                && !actions.deleted.is_empty()
                && !actions.reactions.is_empty(),
            "the fixture must populate actions_state (edit + delete + reaction) so the \
             rebuild is genuinely exercised"
        );
    }

    /// End-to-end: a full `ProbeDriver` + `RiverCliProbeOps` run recovers a
    /// fully-populated room VERBATIM — `merge_with_local` is a pass-through and
    /// `prepare_forward` is identity, so `Outcome::Recovered.merged` equals the
    /// original byte-for-byte.
    #[test]
    fn cli_driver_recovers_fully_populated_room_verbatim() {
        let owner_sk = SigningKey::from_bytes(&[202u8; 32]);
        let owner_vk = owner_sk.verifying_key();
        let original = fully_populated_state(&owner_sk);

        let mut driver = ProbeDriver::new(
            RiverCliProbeOps { owner_vk },
            ChatRoomStateV1::default(),
            NewestFirst::assume_ordered(vec![cli_id(3), cli_id(2)]),
            SelectionPolicy::NewestFirstWins,
        );
        let Step::Get(newest) = driver.next_action() else {
            panic!("expected a GET")
        };
        // Feed the bytes exactly as `probe_legacy_bytes` would.
        driver.on_response(newest, &encode_state(&original));
        assert_eq!(driver.next_action(), Step::Done);
        let Some(Outcome::Recovered { merged, .. }) = driver.take_outcome() else {
            panic!("expected recovery");
        };
        assert_eq!(
            merged, original,
            "the driver must recover the fully-populated room verbatim (no local merge, \
             no forward-strip)"
        );
    }

    /// The pure `resolve_legacy_recovery_outcome`, Recovered arm: returns the
    /// state under the CURRENT id and reports the forward-PUT decision. A legacy
    /// source (!= current) reports a forward PUT; a source == current (the
    /// defensive, unreachable-today guard) reports none.
    #[test]
    fn recovery_outcome_recovered_returns_state_and_reports_forward_put() {
        let owner_sk = SigningKey::from_bytes(&[203u8; 32]);
        let state = owner_state_with_messages(&owner_sk, &["recovered"]);
        let current = cli_id(1);

        let (out_state, out_id, migrate_forward) = resolve_legacy_recovery_outcome(
            Some(Outcome::Recovered {
                merged: state.clone(),
                source: cli_id(9),
                truncated_fold: false,
            }),
            current,
            5,
        )
        .expect("Recovered maps to Ok");
        assert_eq!(
            out_state, state,
            "the recovered state is returned unchanged"
        );
        assert_eq!(out_id, current, "recovery returns the CURRENT contract id");
        assert!(
            migrate_forward,
            "a legacy source (!= current) must report forward-PUT = true"
        );

        let (_, _, migrate_same) = resolve_legacy_recovery_outcome(
            Some(Outcome::Recovered {
                merged: state,
                source: current,
                truncated_fold: false,
            }),
            current,
            5,
        )
        .expect("Recovered maps to Ok");
        assert!(
            !migrate_same,
            "source == current must report forward-PUT = false (the defensive guard)"
        );
    }

    /// The pure `resolve_legacy_recovery_outcome`, non-Recovered arms: SeedLocal,
    /// NoLegacy, and an already-taken (None) outcome all map to the not-found
    /// Err — the CLI never seeds. The error TEXT is a stable contract, so it is
    /// pinned verbatim here.
    #[test]
    fn recovery_outcome_seed_local_and_no_legacy_map_to_not_found_err() {
        let current = cli_id(1);
        let default = ChatRoomStateV1::default();
        for outcome in [
            Some(Outcome::SeedLocal {
                local: default.clone(),
            }),
            Some(Outcome::NoLegacy {
                local: default.clone(),
            }),
            None,
        ] {
            let err = resolve_legacy_recovery_outcome(outcome, current, 5)
                .expect_err("every non-Recovered outcome must be the not-found Err (never a seed)");
            let msg = err.to_string();
            assert!(
                msg.contains(
                    "Room not found on the current contract or any of the 5 known previous \
                     contract generations."
                ),
                "the not-found Err text is a stable contract callers may match; got: {msg}"
            );
            assert!(
                msg.contains("garbage-collected from the network."),
                "the full not-found error text must be preserved; got: {msg}"
            );
        }
    }

    // --- freenet/river#442: the ACTIVE room-migration search
    // (`get_migration_state`) now runs on freenet_migrate's decision driver via
    // `RiverCliMigrationProbeOps`, completing what #437 did for the read path.
    // The two active-path differences from the read path pinned here: the merge
    // with local is a REAL CRDT merge, and exhaustion falls back to the local
    // cache (never an error). ---

    /// The pure `resolve_migration_state_outcome`: Recovered returns the merged
    /// state; SeedLocal and NoLegacy both fall back to the carried local
    /// snapshot (the active path migrates the local cache forward when nothing
    /// is on-network — it does NOT error like the read path); a defensive None
    /// falls back to the passed-in local cache.
    #[test]
    fn migration_outcome_recovered_returns_merged_others_seed_local() {
        let owner_sk = SigningKey::from_bytes(&[91u8; 32]);
        let merged = owner_state_with_messages(&owner_sk, &["merged network+local"]);
        let local = owner_state_with_messages(&owner_sk, &["local only"]);

        let out = resolve_migration_state_outcome(
            Some(Outcome::Recovered {
                merged: merged.clone(),
                source: cli_id(9),
                truncated_fold: false,
            }),
            &local,
        );
        assert_eq!(
            out, merged,
            "Recovered must return the merged state verbatim"
        );

        // SeedLocal / NoLegacy both migrate the outcome's carried local forward.
        let seed = owner_state_with_messages(&owner_sk, &["seeded"]);
        assert_eq!(
            resolve_migration_state_outcome(
                Some(Outcome::SeedLocal {
                    local: seed.clone()
                }),
                &local
            ),
            seed,
            "SeedLocal must migrate the (carried) local snapshot forward"
        );
        let no_legacy = owner_state_with_messages(&owner_sk, &["nolegacy"]);
        assert_eq!(
            resolve_migration_state_outcome(
                Some(Outcome::NoLegacy {
                    local: no_legacy.clone()
                }),
                &local
            ),
            no_legacy,
            "NoLegacy must migrate the (carried) local snapshot forward"
        );
        assert_eq!(
            resolve_migration_state_outcome(None, &local),
            local,
            "an already-taken outcome falls back to the local cache (never an error)"
        );
    }

    /// The active-migration ops delegate decode + is_real to the shared
    /// `RiverCliProbeOps` classifier (one classifier of record for both paths):
    /// an owner-signed state is real, a default is not, undecodable bytes miss.
    #[test]
    fn migration_probe_ops_classifies_via_shared_classifier() {
        let owner_sk = SigningKey::from_bytes(&[93u8; 32]);
        let ops = RiverCliMigrationProbeOps {
            inner: RiverCliProbeOps {
                owner_vk: owner_sk.verifying_key(),
            },
        };
        assert!(ops.is_real(&owner_state_with_messages(&owner_sk, &["hi"])));
        assert!(
            !ops.is_real(&ChatRoomStateV1::default()),
            "a default state must NOT be real (a miss)"
        );
        assert!(
            ops.decode(&encode_state(&owner_state_with_messages(
                &owner_sk,
                &["hi"]
            )))
            .is_some(),
            "valid CBOR must decode"
        );
        assert!(
            ops.decode(&[0xffu8; 8]).is_none(),
            "undecodable bytes must map to None (a miss)"
        );
    }

    /// THE distinguishing active-path behavior: `merge_with_local` is the REAL
    /// CRDT merge, NOT the read path's pass-through. Recovered and local carry
    /// DISJOINT owner-authored messages; the merge must contain BOTH — a
    /// pass-through would silently drop the device-local message.
    #[test]
    fn migration_probe_ops_merge_with_local_is_the_real_merge() {
        let owner_sk = SigningKey::from_bytes(&[92u8; 32]);
        let ops = RiverCliMigrationProbeOps {
            inner: RiverCliProbeOps {
                owner_vk: owner_sk.verifying_key(),
            },
        };
        let recovered = owner_state_with_messages(&owner_sk, &["network-only"]);
        let local = owner_state_with_messages(&owner_sk, &["local-only"]);

        let merged = ops.merge_with_local(recovered, &local);
        let texts: Vec<String> = merged
            .recent_messages
            .messages
            .iter()
            .map(|m| message_display_text(&merged, m))
            .collect();
        assert!(
            texts.iter().any(|t| t == "network-only"),
            "the recovered network message must survive the merge; got {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t == "local-only"),
            "the device-local message must be folded in (the ACTIVE path's real \
             merge, not the read path's pass-through); got {texts:?}"
        );
    }

    /// `migration_candidate_ids` puts the stored previous_contract_key FIRST,
    /// then the legacy registry newest-first. When the stored key IS a registry
    /// generation it is deliberately re-listed (present TWICE), NOT de-duped:
    /// the duplicate is the retry a transient fast-path miss (send failure / 8s
    /// timeout) needs before the probe advances to an older generation
    /// (freenet/river#442 review finding). Stored-key-first + the retry preserve
    /// the pre-driver fast-path-then-deep-path semantics under `NewestFirstWins`.
    #[test]
    fn migration_candidates_put_stored_key_first_and_keep_retry_duplicate() {
        let owner = SigningKey::from_bytes(&[94u8; 32]).verifying_key();
        let legacy: Vec<ContractInstanceId> =
            river_core::migration::legacy_contract_keys_for_owner(&owner)
                .iter()
                .map(|k| *k.id())
                .collect();
        assert!(
            !legacy.is_empty(),
            "precondition: the owner has at least one legacy generation to probe"
        );

        // A distinct stored key (not in the registry) is prepended; registry follows.
        let distinct = cli_id(200);
        assert!(
            !legacy.contains(&distinct),
            "fixture: the distinct stored key must not collide with the registry"
        );
        let with_distinct = migration_candidate_ids(&Some(distinct.to_string()), &owner);
        assert_eq!(
            with_distinct.first(),
            Some(&distinct),
            "the stored key must be probed first (the fast path)"
        );
        assert_eq!(
            &with_distinct[1..],
            &legacy[..],
            "the full registry follows the stored key, in newest-first order"
        );

        // A stored key that IS a registry generation is RE-LISTED (present
        // twice), NOT de-duped: it is probed first AND again in its registry
        // slot, so the fast-path attempt has a deep-sweep retry.
        let dup = legacy[0];
        let with_dup = migration_candidate_ids(&Some(dup.to_string()), &owner);
        assert_eq!(
            with_dup.first(),
            Some(&dup),
            "the stored key is still probed first"
        );
        assert_eq!(
            &with_dup[1..],
            &legacy[..],
            "the full registry (including the stored key's own slot) follows it — the \
             stored key is re-listed so a transient fast-path miss is retried"
        );
        assert_eq!(
            with_dup.iter().filter(|&&id| id == dup).count(),
            2,
            "a stored key that is also a registry generation must appear TWICE (the \
             retry duplicate), never de-duped to once"
        );
        assert_eq!(
            with_dup.len(),
            legacy.len() + 1,
            "the retry duplicate adds exactly one candidate ahead of the registry"
        );

        // No stored key, or an unparseable one → just the registry.
        assert_eq!(migration_candidate_ids(&None, &owner), legacy);
        assert_eq!(
            migration_candidate_ids(&Some("not-a-contract-id".to_string()), &owner),
            legacy,
            "an unparseable stored key is skipped, leaving the registry"
        );
    }

    /// The retry duplicate in action at the driver level: a transient timeout on
    /// the newest generation (the fast-path attempt) does NOT strand the probe
    /// on an older generation — the re-listed stored key is retried and recovers
    /// the NEWEST state. Without the duplicate, the timeout would advance to
    /// `cli_id(5)` and migrate stale state forward, clearing the pointer (the
    /// freenet/river#442 review finding).
    #[test]
    fn migration_retry_after_transient_timeout_recovers_newest() {
        let owner_sk = SigningKey::from_bytes(&[97u8; 32]);
        let owner_vk = owner_sk.verifying_key();
        let local = owner_state_with_messages(&owner_sk, &["local"]);
        // Candidates as migration_candidate_ids builds them when the stored key
        // is the newest registry generation: [stored, stored(retry), older].
        let mut driver = ProbeDriver::new(
            RiverCliMigrationProbeOps {
                inner: RiverCliProbeOps { owner_vk },
            },
            local.clone(),
            NewestFirst::assume_ordered(vec![cli_id(9), cli_id(9), cli_id(5)]),
            SelectionPolicy::NewestFirstWins,
        );
        // Fast-path attempt on the newest generation transiently times out.
        let Step::Get(first) = driver.next_action() else {
            panic!("expected a GET")
        };
        assert_eq!(first, cli_id(9));
        driver.on_timeout(first);
        // The re-listed stored key is retried (NOT an older generation) and hits.
        let Step::Get(retry) = driver.next_action() else {
            panic!("expected the retry GET")
        };
        assert_eq!(
            retry,
            cli_id(9),
            "a transient timeout must retry the newest generation, not advance to an older one"
        );
        driver.on_response(
            retry,
            &encode_state(&owner_state_with_messages(&owner_sk, &["newest"])),
        );
        assert_eq!(driver.next_action(), Step::Done);
        let Some(Outcome::Recovered { source, merged, .. }) = driver.take_outcome() else {
            panic!("the retry must recover the newest generation")
        };
        assert_eq!(
            source,
            cli_id(9),
            "the newest generation is recovered by the retry, not the older cli_id(5)"
        );
        let texts: Vec<String> = merged
            .recent_messages
            .messages
            .iter()
            .map(|m| message_display_text(&merged, m))
            .collect();
        assert!(
            texts.iter().any(|t| t == "newest"),
            "the recovered state is the newest generation's; got {texts:?}"
        );
    }

    /// End-to-end at the driver level: `RiverCliMigrationProbeOps` + a real
    /// `ProbeDriver` adopt the NEWEST real generation (anti-rollback) AND fold
    /// the local cache into it (the active path's real merge), then
    /// `resolve_migration_state_outcome` returns that merged state.
    #[test]
    fn cli_migration_driver_adopts_newest_and_merges_local() {
        let owner_sk = SigningKey::from_bytes(&[95u8; 32]);
        let owner_vk = owner_sk.verifying_key();
        let local = owner_state_with_messages(&owner_sk, &["local"]);
        let mut driver = ProbeDriver::new(
            RiverCliMigrationProbeOps {
                inner: RiverCliProbeOps { owner_vk },
            },
            local.clone(),
            NewestFirst::assume_ordered(vec![cli_id(9), cli_id(5)]),
            SelectionPolicy::NewestFirstWins,
        );
        let Step::Get(newest) = driver.next_action() else {
            panic!("expected a GET")
        };
        assert_eq!(newest, cli_id(9), "the newest generation is probed first");
        driver.on_response(
            newest,
            &encode_state(&owner_state_with_messages(&owner_sk, &["network"])),
        );
        assert_eq!(
            driver.next_action(),
            Step::Done,
            "NewestFirstWins stops at the first real generation"
        );
        let state = resolve_migration_state_outcome(driver.take_outcome(), &local);
        let texts: Vec<String> = state
            .recent_messages
            .messages
            .iter()
            .map(|m| message_display_text(&state, m))
            .collect();
        assert!(
            texts.iter().any(|t| t == "network"),
            "the adopted network state must be kept; got {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t == "local"),
            "the local cache must be folded into the migrated state; got {texts:?}"
        );
    }

    /// End-to-end exhaustion: every candidate misses, so the driver reports
    /// SeedLocal — which the active path migrates FORWARD as the local cache
    /// (never an error, unlike the read path's not-found Err).
    #[test]
    fn cli_migration_driver_exhausts_to_local_cache() {
        let owner_sk = SigningKey::from_bytes(&[96u8; 32]);
        let local = owner_state_with_messages(&owner_sk, &["only local"]);
        let mut driver = ProbeDriver::new(
            RiverCliMigrationProbeOps {
                inner: RiverCliProbeOps {
                    owner_vk: owner_sk.verifying_key(),
                },
            },
            local.clone(),
            NewestFirst::assume_ordered(vec![cli_id(2), cli_id(1)]),
            SelectionPolicy::NewestFirstWins,
        );
        let Step::Get(a) = driver.next_action() else {
            panic!()
        };
        driver.on_timeout(a); // absent generation
        let Step::Get(b) = driver.next_action() else {
            panic!()
        };
        driver.on_response(b, &[0xffu8; 8]); // undecodable → miss
        assert_eq!(driver.next_action(), Step::Done);
        let state = resolve_migration_state_outcome(driver.take_outcome(), &local);
        assert_eq!(
            state, local,
            "on exhaustion the active path migrates the local cache forward, never errors"
        );
    }

    /// Source-grep pin: `get_migration_state` must drive the generation search
    /// through the decision driver (newest-first) with the active-path MERGE ops
    /// (`RiverCliMigrationProbeOps`), size its hop cap to the candidate set,
    /// build candidates via `migration_candidate_ids`, and route the outcome
    /// through the pure `resolve_migration_state_outcome`. A refactor that
    /// re-introduced the hand-rolled fast-path/deep-path loop would fail this.
    #[test]
    fn active_migration_routes_through_decision_driver() {
        let full = include_str!("api.rs");
        let fn_start = full
            .find("async fn get_migration_state")
            .expect("get_migration_state must exist");
        // Bound the slice at the next fn so we only inspect this function body.
        let rest = &full[fn_start..];
        let body_end = rest
            .find("async fn probe_migration_bytes")
            .expect("probe_migration_bytes must follow get_migration_state");
        let body = &rest[..body_end];

        // Match "NewestFirstWins" bare so a `use` that drops the
        // `SelectionPolicy::` prefix cannot false-fail this pin.
        assert!(
            body.contains("NewestFirstWins"),
            "the active-migration search must use the newest-first decision driver"
        );
        assert!(
            body.contains("RiverCliMigrationProbeOps"),
            "the active path must use the merge-with-local ops, NOT the read path's pass-through"
        );
        assert!(
            body.contains("with_max_hops"),
            "the driver must size its hop cap to the candidate set (the pre-driver deep path \
             was unbounded)"
        );
        assert!(
            body.contains("migration_candidate_ids"),
            "candidates must be built by the shared stored-key-first + dedupe helper"
        );
        assert!(
            body.contains("resolve_migration_state_outcome"),
            "the outcome->state mapping must go through the pure, unit-tested resolver"
        );
    }
}

#[cfg(test)]
mod create_room_tests {
    use super::*;

    /// Creating a PRIVATE room seals its name + owner nickname under a v0 room
    /// secret, records the version, and writes an owner-addressed secret blob so
    /// the OWNER can decrypt their own room from contract state alone (the
    /// load-bearing property). The sealed metadata must decrypt back to
    /// plaintext — and must NOT be stored as plaintext.
    #[test]
    fn build_new_room_state_private_is_encrypted_and_owner_decryptable() {
        let owner = SigningKey::from_bytes(&[13u8; 32]);
        let (state, secret) = build_new_room_state(&owner, "Secret Room", "Alice", true);
        let secret = secret.expect("private room yields a secret");

        assert_eq!(
            state.configuration.configuration.privacy_mode,
            PrivacyMode::Private
        );
        assert_eq!(state.secrets.current_version, 0);
        assert_eq!(state.secrets.versions.len(), 1);
        assert_eq!(state.secrets.encrypted_secrets.len(), 1);

        let name_field = &state.configuration.configuration.display.name;
        let nick_field = &state.member_info.member_info[0]
            .member_info
            .preferred_nickname;
        assert!(name_field.is_private(), "private room name must be sealed");
        assert!(nick_field.is_private(), "owner nickname must be sealed");

        // The owner recovers v0 from its own contract blob — no local secret.
        let recovered =
            crate::private_room::collect_secrets_for_room(&state, &owner, &HashMap::new());
        assert_eq!(
            recovered.get(&0),
            Some(&secret),
            "owner must recover v0 from its own contract blob"
        );

        // Sealed metadata decrypts back to the plaintext.
        let secrets = HashMap::from([(0u32, secret)]);
        assert_eq!(
            river_core::ecies::unseal_bytes_with_secrets(name_field, &secrets).unwrap(),
            b"Secret Room"
        );
        assert_eq!(
            river_core::ecies::unseal_bytes_with_secrets(nick_field, &secrets).unwrap(),
            b"Alice"
        );
    }

    /// Creating a PUBLIC room is unchanged: no secret, public metadata.
    #[test]
    fn build_new_room_state_public_has_no_secret_and_public_metadata() {
        let owner = SigningKey::from_bytes(&[14u8; 32]);
        let (state, secret) = build_new_room_state(&owner, "Open Room", "Bob", false);

        assert!(secret.is_none());
        assert_eq!(
            state.configuration.configuration.privacy_mode,
            PrivacyMode::Public
        );
        assert!(state.secrets.encrypted_secrets.is_empty());
        assert_eq!(
            state
                .configuration
                .configuration
                .display
                .name
                .as_public_bytes(),
            Some(b"Open Room".as_ref())
        );
        assert!(state.member_info.member_info[0]
            .member_info
            .preferred_nickname
            .is_public());
    }
}

#[cfg(test)]
mod display_text_tests {
    use super::*;
    use river_core::room_state::message::{
        AuthorizedMessageV1, MessageId, MessageV1, RoomMessageBody,
    };
    use std::time::SystemTime;

    /// Build a `ChatRoomStateV1` whose `recent_messages` holds a single
    /// authored message with `body`.
    fn state_with_message(body: RoomMessageBody) -> (ChatRoomStateV1, AuthorizedMessageV1) {
        let author_sk = SigningKey::from_bytes(&[11u8; 32]);
        let owner_vk = SigningKey::from_bytes(&[12u8; 32]).verifying_key();
        let message = MessageV1 {
            room_owner: MemberId::from(owner_vk),
            author: MemberId::from(&author_sk.verifying_key()),
            content: body,
            time: SystemTime::UNIX_EPOCH,
        };
        let authored = AuthorizedMessageV1::new(message, &author_sk);
        let mut state = ChatRoomStateV1::default();
        state.recent_messages.messages.push(authored.clone());
        (state, authored)
    }

    /// Regression: a join event is a *public* `content_type = 4` message, not
    /// encrypted. riverctl previously rendered it as "<encrypted>" because the
    /// display path fell back to that literal whenever `effective_text` (which
    /// only yields text/reply bodies) returned `None`. It must now read
    /// "joined the room".
    #[test]
    fn join_event_renders_as_joined_not_encrypted() {
        let (state, msg) = state_with_message(RoomMessageBody::join_event());
        assert_eq!(message_display_text(&state, &msg), "joined the room");
    }

    /// A genuinely private (encrypted) body still renders as "<encrypted>" —
    /// the fix must not leak ciphertext details or mislabel real encryption.
    #[test]
    fn private_body_still_renders_as_encrypted() {
        let body = RoomMessageBody::private(1, 1, vec![0xDE, 0xAD, 0xBE, 0xEF], [0u8; 12], 0);
        let (state, msg) = state_with_message(body);
        assert_eq!(message_display_text(&state, &msg), "<encrypted>");
    }

    /// A public text message is unaffected — it renders its plaintext.
    #[test]
    fn public_text_renders_plaintext() {
        let (state, msg) = state_with_message(RoomMessageBody::public("hello world".to_string()));
        assert_eq!(message_display_text(&state, &msg), "hello world");
    }

    /// An unrecognized *public* content type (a future content_type this CLI
    /// doesn't understand) is not encrypted, so it renders the "please upgrade"
    /// placeholder rather than "<encrypted>". Pins that the fallback narrowing
    /// applies to all public content, not just join events.
    #[test]
    fn unknown_public_content_renders_upgrade_placeholder() {
        let (state, msg) = state_with_message(RoomMessageBody::public_raw(99, 1, vec![0x01, 0x02]));
        assert_eq!(
            message_display_text(&state, &msg),
            "[Unsupported message type 99.1 - please upgrade]"
        );
    }

    /// Seal `text` as a private (AES-256-GCM) `TextContentV1` body under
    /// `secret`/`version`, mirroring the wire bytes the UI and
    /// `private_room::build_message_body` produce.
    fn private_text_body(secret: &[u8; 32], version: u32, text: &str) -> RoomMessageBody {
        use river_core::room_state::content::TextContentV1;
        let bytes = TextContentV1::new(text.to_string()).encode();
        let (ciphertext, nonce) = river_core::ecies::encrypt_with_symmetric_key(secret, &bytes);
        RoomMessageBody::private_text(ciphertext, nonce, version)
    }

    /// Core regression for the reported bug (riverctl `message list` on an
    /// encrypted room showed `"content":"<encrypted>"`): a private text body
    /// must decrypt to its plaintext when the room secret is supplied, while
    /// still falling back to "<encrypted>" when it is not.
    #[test]
    fn private_text_decrypts_with_secret() {
        let secret = [7u8; 32];
        let (state, msg) = state_with_message(private_text_body(&secret, 0, "secret hello"));

        // No secrets (non-member / pre-fix behaviour) → still "<encrypted>".
        assert_eq!(message_display_text(&state, &msg), "<encrypted>");

        // Correct secret for the body's version → plaintext.
        let secrets = HashMap::from([(0u32, secret)]);
        assert_eq!(
            message_display_text_with_secrets(&state, &msg, &secrets),
            "secret hello"
        );

        // A secrets map lacking this body's version (e.g. rotated past) →
        // "<encrypted>", never a panic or a wrong-key garble.
        let other_version = HashMap::from([(1u32, secret)]);
        assert_eq!(
            message_display_text_with_secrets(&state, &msg, &other_version),
            "<encrypted>"
        );

        // Wrong key at the right version → decrypt fails → "<encrypted>".
        let wrong_key = HashMap::from([(0u32, [8u8; 32])]);
        assert_eq!(
            message_display_text_with_secrets(&state, &msg, &wrong_key),
            "<encrypted>"
        );
    }

    /// A private reply seals its whole `ReplyContentV1` (target author + quoted
    /// preview + reply text). Without the secret the reply context is opaque
    /// (no `[reply to …]` prefix); with it, both the context and the body
    /// decrypt.
    #[test]
    fn private_reply_context_decrypts_with_secret() {
        use river_core::room_state::content::{
            ReplyContentV1, CONTENT_TYPE_REPLY, REPLY_CONTENT_VERSION,
        };
        let secret = [9u8; 32];
        let reply = ReplyContentV1::new(
            "my reply".to_string(),
            MessageId(freenet_scaffold::util::FastHash(0)),
            "Alice".to_string(),
            "quoted original".to_string(),
        );
        let (ciphertext, nonce) =
            river_core::ecies::encrypt_with_symmetric_key(&secret, &reply.encode());
        let body = RoomMessageBody::private(
            CONTENT_TYPE_REPLY,
            REPLY_CONTENT_VERSION,
            ciphertext,
            nonce,
            0,
        );
        let (state, msg) = state_with_message(body);

        // Opaque without the secret.
        assert_eq!(reply_context_display(&state, &msg), None);

        // Author + quoted preview decrypt with the secret.
        let secrets = HashMap::from([(0u32, secret)]);
        let (author, preview) = reply_context_display_with_secrets(&state, &msg, &secrets)
            .expect("private reply context decrypts");
        assert_eq!(author, "Alice");
        assert_eq!(preview, "quoted original");

        // And the reply body itself decrypts to the reply text.
        assert_eq!(
            message_display_text_with_secrets(&state, &msg, &secrets),
            "my reply"
        );
    }

    /// A member nickname is `SealedBytes`: public in a public room, AES-256-GCM
    /// sealed in a private room. `unseal_nickname_display` must decrypt the
    /// private case with the room secret and fall back to the placeholder (never
    /// raw ciphertext) when the secret is unavailable — this is what stops
    /// `message list` / `member list` / `send_reply` from showing (and, for
    /// replies, persisting) "[Encrypted: N bytes, vN]" as the author.
    #[test]
    fn unseal_nickname_display_decrypts_private_and_falls_back() {
        use river_core::ecies::seal_bytes;
        use river_core::room_state::privacy::SealedBytes;

        let secret = [4u8; 32];
        let sealed = seal_bytes(b"Alice", &secret, 0);

        // With the matching secret → plaintext nickname.
        let secrets = HashMap::from([(0u32, secret)]);
        assert_eq!(unseal_nickname_display(&sealed, &secrets), "Alice");

        // Without the secret → the "[Encrypted: …]" placeholder, never raw
        // ciphertext.
        assert_eq!(
            unseal_nickname_display(&sealed, &HashMap::new()),
            "[Encrypted: 5 bytes, v0]"
        );

        // Wrong key at the right version → placeholder (decrypt fails cleanly).
        let wrong = HashMap::from([(0u32, [9u8; 32])]);
        assert_eq!(
            unseal_nickname_display(&sealed, &wrong),
            "[Encrypted: 5 bytes, v0]"
        );

        // A public nickname is unaffected (public room / empty secrets).
        let public = SealedBytes::public(b"Bob".to_vec());
        assert_eq!(unseal_nickname_display(&public, &HashMap::new()), "Bob");
    }

    /// A private-room EDIT is an encrypted action message. The public-only
    /// `rebuild_actions_state` that `apply_delta` runs drops it, so the body
    /// decrypts to its ORIGINAL text; only after
    /// [`rebuild_private_actions_state`] (which the display paths now run) does
    /// the edited text surface.
    #[test]
    fn private_edit_shows_edited_text_after_rebuild() {
        use river_core::room_state::content::ActionContentV1;
        let author_sk = SigningKey::from_bytes(&[11u8; 32]);
        let owner_vk = SigningKey::from_bytes(&[12u8; 32]).verifying_key();
        let secret = [5u8; 32];

        let author = |content: RoomMessageBody, secs: u64| {
            AuthorizedMessageV1::new(
                MessageV1 {
                    room_owner: MemberId::from(owner_vk),
                    author: MemberId::from(&author_sk.verifying_key()),
                    content,
                    time: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs),
                },
                &author_sk,
            )
        };

        let orig = author(private_text_body(&secret, 0, "before edit"), 0);
        let action = ActionContentV1::edit(orig.id(), "after edit".to_string());
        let (ciphertext, nonce) =
            river_core::ecies::encrypt_with_symmetric_key(&secret, &action.encode());
        let edit = author(RoomMessageBody::private_action(ciphertext, nonce, 0), 1);

        let mut state = ChatRoomStateV1::default();
        state.recent_messages.messages.push(orig.clone());
        state.recent_messages.messages.push(edit);

        let secrets = HashMap::from([(0u32, secret)]);

        // Before the decrypt-aware rebuild: original text (the private edit
        // action was dropped by the public-only rebuild).
        assert!(!state.recent_messages.is_edited(&orig.id()));
        assert_eq!(
            message_display_text_with_secrets(&state, &orig, &secrets),
            "before edit"
        );

        // After it: the edit applies.
        rebuild_private_actions_state(&mut state, &secrets);
        assert!(state.recent_messages.is_edited(&orig.id()));
        assert_eq!(
            message_display_text_with_secrets(&state, &orig, &secrets),
            "after edit"
        );
    }
}

#[cfg(test)]
mod rejoin_nickname_tests {
    use super::*;
    use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
    use river_core::room_state::privacy::PrivacyMode;

    fn member_key() -> SigningKey {
        SigningKey::from_bytes(&[11u8; 32])
    }

    /// Build a `ChatRoomStateV1` with the given privacy mode, nickname-size
    /// limit, and current secret version.
    fn state_with(
        privacy: PrivacyMode,
        max_nickname_size: usize,
        current_version: u32,
    ) -> ChatRoomStateV1 {
        let owner_sk = SigningKey::from_bytes(&[3u8; 32]);
        let config = Configuration {
            owner_member_id: owner_sk.verifying_key().into(),
            privacy_mode: privacy,
            max_nickname_size,
            ..Default::default()
        };
        let mut state = ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(config, &owner_sk),
            ..Default::default()
        };
        state.secrets.current_version = current_version;
        state
    }

    /// Public room → the real nickname is restored as public plaintext.
    #[test]
    fn public_room_restores_real_nickname() {
        let state = state_with(PrivacyMode::Public, 50, 0);
        let out = rejoin_preferred_nickname(&state, &member_key(), &HashMap::new(), Some("Alice"));
        assert!(out.is_public());
        assert_eq!(out.to_string_lossy(), "Alice");
    }

    /// No persisted nickname → generic "Member" placeholder.
    #[test]
    fn no_stored_nickname_falls_back_to_member() {
        let state = state_with(PrivacyMode::Public, 50, 0);
        let out = rejoin_preferred_nickname(&state, &member_key(), &HashMap::new(), None);
        assert!(out.is_public());
        assert_eq!(out.to_string_lossy(), "Member");
    }

    /// A nickname longer than the room's current `max_nickname_size` must NOT
    /// be published (the contract would reject the whole rejoin delta) — fall
    /// back to "Member" so the member can still rejoin. Regression guard for the
    /// PR #321 Codex/skeptical finding.
    #[test]
    fn over_long_nickname_falls_back_to_member() {
        // max 8: "Member" (6) fits, but the stored nickname (20) does not.
        let state = state_with(PrivacyMode::Public, 8, 0);
        let out = rejoin_preferred_nickname(
            &state,
            &member_key(),
            &HashMap::new(),
            Some("this_is_way_too_long"),
        );
        assert_eq!(out.to_string_lossy(), "Member");
    }

    /// Private room with a secret available → the nickname is SEALED
    /// (ciphertext), never published as plaintext.
    #[test]
    fn private_room_with_secret_seals_nickname() {
        let state = state_with(PrivacyMode::Private, 50, 1);
        let mut secrets = HashMap::new();
        secrets.insert(1u32, [7u8; 32]);
        let out = rejoin_preferred_nickname(&state, &member_key(), &secrets, Some("Alice"));
        assert!(out.is_private(), "private-room nickname must be sealed");
        // Declared plaintext length is preserved even though the bytes are sealed.
        assert_eq!(out.declared_len(), "Alice".len());
    }

    /// Private room with NO secret available → must fall back to the generic
    /// public "Member" placeholder, NEVER leak the real nickname as plaintext.
    #[test]
    fn private_room_without_secret_does_not_leak_real_nickname() {
        let state = state_with(PrivacyMode::Private, 50, 1);
        let out = rejoin_preferred_nickname(&state, &member_key(), &HashMap::new(), Some("Alice"));
        assert!(out.is_public());
        assert_eq!(out.to_string_lossy(), "Member");
        assert_ne!(
            out.to_string_lossy(),
            "Alice",
            "real nickname must never be published as plaintext in a private room"
        );
    }
}

/// Regression tests for the re-accept guard (issue freenet/river#308).
///
/// `accept_invitation` must refuse to re-accept an invitation for a room the
/// CLI already has stored credentials for — re-accepting used to rebuild the
/// `StoredRoomInfo` and `insert` it, wholesale-clobbering the existing
/// `signing_key_bytes` / `self_authorized_member` / `invite_chain` /
/// `previous_contract_key` / `self_nickname`. The substantive
/// "clobber is actually prevented" assertion lives in `storage.rs`
/// (`reaccept_guard_prevents_clobber`, which can run without a live node);
/// here we pin the guard's wiring and its user-facing error.
#[cfg(test)]
mod reaccept_guard_tests {
    use super::*;

    /// The refusal error names the room and points at `riverctl room leave`,
    /// mirroring `import_identity`'s message so the recovery path is the same
    /// across both entry points.
    #[test]
    fn refusal_error_names_room_and_points_to_leave() {
        let owner_vk = SigningKey::from_bytes(&[9u8; 32]).verifying_key();
        let owner_key_str = bs58::encode(owner_vk.as_bytes()).into_string();
        let msg = reaccept_refusal_error(&owner_vk).to_string();
        assert!(
            msg.contains(&owner_key_str),
            "refusal must name the room owner key so the user knows which room: {msg}"
        );
        assert!(
            msg.contains(&format!("riverctl room leave {owner_key_str}")),
            "refusal must point the user at `riverctl room leave <owner>`: {msg}"
        );
    }

    /// Source-grep pin: the shared `accept_invitation_struct` core MUST
    /// consult `get_room` and bail via `reaccept_refusal_error` BEFORE doing
    /// the network GET that rebuilds the `StoredRoomInfo`. A refactor that
    /// drops this guard would silently reintroduce the #308 clobber, which no
    /// pure unit test can catch because the accept path requires a live
    /// Freenet node end-to-end. The guard lives in `accept_invitation_struct`
    /// (shared by the base58 `invite accept` path and the `dm accept` path),
    /// so pinning it there protects BOTH entry points at once.
    #[test]
    fn accept_invitation_has_reaccept_guard() {
        let api_src = include_str!("api.rs");
        // Pin the guard within the shared accept body: find the function,
        // then assert the guard appears before the network GET request.
        let accept_idx = api_src
            .find("pub async fn accept_invitation_struct(")
            .expect("accept_invitation_struct must exist");
        let body = &api_src[accept_idx..];
        let guard_idx = body
            .find("if self.storage.get_room(&room_owner_vk)?.is_some() {")
            .expect(
                "accept_invitation must guard on `get_room(&room_owner_vk)` to refuse re-accept \
                 (issue #308) — if you refactored the guard, update this pin",
            );
        assert!(
            body[guard_idx..].contains("return Err(reaccept_refusal_error(&room_owner_vk));"),
            "the re-accept guard must return `reaccept_refusal_error` so the user gets the \
             `riverctl room leave` recovery path"
        );
        // The guard must precede the GET that rebuilds StoredRoomInfo, so we
        // never touch the network (or local storage) on a refused re-accept.
        let get_idx = body
            .find("let get_request = ContractRequest::Get {")
            .expect("accept_invitation must perform a GET");
        assert!(
            guard_idx < get_idx,
            "the re-accept guard must run BEFORE the network GET so a refused re-accept \
             does no network or storage work"
        );
    }
}

#[cfg(test)]
mod monitor_tests {
    use super::*;
    use river_core::room_state::message::{
        AuthorizedMessageV1, MessageId, MessageV1, RoomMessageBody,
    };
    use std::time::SystemTime;

    fn authored(body: RoomMessageBody) -> AuthorizedMessageV1 {
        let sk = SigningKey::from_bytes(&[5u8; 32]);
        let owner = SigningKey::from_bytes(&[6u8; 32]).verifying_key();
        let m = MessageV1 {
            room_owner: MemberId::from(owner),
            author: MemberId::from(&sk.verifying_key()),
            content: body,
            time: SystemTime::UNIX_EPOCH,
        };
        AuthorizedMessageV1::new(m, &sk)
    }

    /// A reply message yields its target author and a preview truncated to 50
    /// chars — the context the monitor stream now renders (it previously didn't).
    /// A clipped preview keeps 50 chars of content and gains a trailing `"..."`
    /// so a reader can tell it was cut.
    #[test]
    fn reply_context_extracts_author_and_truncated_preview() {
        let long_preview = "x".repeat(80);
        let msg = authored(RoomMessageBody::reply(
            "my reply".to_string(),
            MessageId(freenet_scaffold::util::FastHash(0)),
            "Alice".to_string(),
            long_preview,
        ));
        let (author, preview) = reply_context(&msg).expect("should detect a reply");
        assert_eq!(author, "Alice");
        assert!(
            preview.ends_with("..."),
            "clipped preview gets a truncation marker: {preview}"
        );
        assert_eq!(
            preview.chars().count(),
            53,
            "50 content chars + 3-char ellipsis"
        );
        assert_eq!(&preview[..50], &"x".repeat(50), "50 chars of content kept");
    }

    /// A plain (non-reply) message has no reply context.
    #[test]
    fn reply_context_none_for_plain_message() {
        let msg = authored(RoomMessageBody::public("hello".to_string()));
        assert!(reply_context(&msg).is_none());
    }

    /// A join event (public, non-text, non-reply) has no reply context.
    #[test]
    fn reply_context_none_for_event() {
        let msg = authored(RoomMessageBody::join_event());
        assert!(reply_context(&msg).is_none());
    }

    fn reply_with_preview(preview: &str) -> AuthorizedMessageV1 {
        authored(RoomMessageBody::reply(
            "my reply".to_string(),
            MessageId(freenet_scaffold::util::FastHash(0)),
            "Alice".to_string(),
            preview.to_string(),
        ))
    }

    /// Preview truncation boundaries: a short preview is returned whole (no
    /// ellipsis), an exactly-50 preview is untouched (no ellipsis — it wasn't
    /// clipped), and a multi-byte/emoji preview is truncated by CHARACTERS (not
    /// bytes), so `.chars().take(50)` never panics or splits a codepoint. The
    /// `"..."` marker appears only when content was actually dropped.
    #[test]
    fn reply_context_preview_boundaries() {
        // Shorter than 50 → returned whole, no truncation marker.
        let (_, short) = reply_context(&reply_with_preview("hi")).unwrap();
        assert_eq!(short, "hi");

        // Empty preview → still a reply, empty body, no marker.
        let (author, empty) = reply_context(&reply_with_preview("")).unwrap();
        assert_eq!(author, "Alice");
        assert_eq!(empty, "");

        // Exactly 50 → unchanged, NO ellipsis (nothing was dropped).
        let exactly = "a".repeat(50);
        let (_, p50) = reply_context(&reply_with_preview(&exactly)).unwrap();
        assert_eq!(p50.chars().count(), 50);
        assert_eq!(p50, exactly);
        assert!(!p50.ends_with("..."), "exact fit gets no marker: {p50}");

        // 51 chars → clipped to 50 content chars + "...".
        let just_over = "a".repeat(51);
        let (_, p51) = reply_context(&reply_with_preview(&just_over)).unwrap();
        assert_eq!(p51, format!("{}...", "a".repeat(50)));

        // 60 emoji (multi-byte) → 50 content chars + "...", no panic / no split.
        let emojis = "🦀".repeat(60);
        let (_, pe) = reply_context(&reply_with_preview(&emojis)).unwrap();
        assert!(
            pe.ends_with("..."),
            "clipped emoji preview gets a marker: {pe}"
        );
        assert_eq!(pe.chars().count(), 53, "50 emoji + 3-char ellipsis");
        assert_eq!(&pe.chars().take(50).collect::<String>(), &"🦀".repeat(50));
    }

    /// Regression guard for PR #322 review finding #1: two DIFFERENT messages
    /// from the same author with an identical timestamp must get DIFFERENT
    /// monitor dedup keys (keyed on the signature-derived id, not author:time),
    /// or they would flip-flop forever as spurious "edit" re-emissions. The same
    /// message yields a stable key.
    #[test]
    fn monitor_seen_key_distinct_for_same_author_and_time_different_content() {
        let sk = SigningKey::from_bytes(&[8u8; 32]);
        let owner = SigningKey::from_bytes(&[9u8; 32]).verifying_key();
        let make = |text: &str| {
            let m = MessageV1 {
                room_owner: MemberId::from(owner),
                author: MemberId::from(&sk.verifying_key()),
                content: RoomMessageBody::public(text.to_string()),
                time: SystemTime::UNIX_EPOCH, // identical timestamp
            };
            AuthorizedMessageV1::new(m, &sk)
        };
        let a = make("first");
        let b = make("second");
        assert_ne!(
            monitor_seen_key(&a),
            monitor_seen_key(&b),
            "same author + identical timestamp but different content must not collide"
        );
        // Same message → stable key.
        assert_eq!(monitor_seen_key(&a), monitor_seen_key(&make("first")));
    }

    /// The monitor edit-detection: a key never seen is New; the same content is
    /// Unchanged; a changed content for a seen key is Edited.
    #[test]
    fn classify_seen_detects_new_unchanged_edited() {
        let mut seen: HashMap<String, String> = HashMap::new();
        assert_eq!(classify_seen(&seen, "k1", "hello"), EmitKind::New);
        seen.insert("k1".to_string(), "hello".to_string());
        assert_eq!(classify_seen(&seen, "k1", "hello"), EmitKind::Unchanged);
        assert_eq!(
            classify_seen(&seen, "k1", "hello, world"),
            EmitKind::Edited,
            "a changed effective content for a seen message is an edit"
        );
        assert_eq!(classify_seen(&seen, "k2", "other"), EmitKind::New);
    }

    /// Deletion is emitted only for a message the stream previously surfaced,
    /// and only once. A message never shown (not in `seen`) — e.g. deleted
    /// before the stream started — produces no deletion event. freenet/river#323.
    #[test]
    fn should_emit_deletion_only_for_seen_and_unreported() {
        let mut seen: HashMap<String, String> = HashMap::new();
        let mut emitted: HashSet<String> = HashSet::new();

        // Never surfaced → no deletion event.
        assert!(!should_emit_deletion(&seen, &emitted, "k1"));

        // Surfaced → emit once.
        seen.insert("k1".to_string(), "hi".to_string());
        assert!(should_emit_deletion(&seen, &emitted, "k1"));

        // Already reported → don't repeat.
        emitted.insert("k1".to_string());
        assert!(!should_emit_deletion(&seen, &emitted, "k1"));

        // Completes the truth table: not-seen + already-emitted → false.
        let only_emitted: HashSet<String> = ["k9".to_string()].into_iter().collect();
        assert!(!should_emit_deletion(&HashMap::new(), &only_emitted, "k9"));
    }

    /// A pre-existing message NOT shown at stream start has its later deletion
    /// suppressed; a shown message does not. Regression guard for the
    /// subscribe-path bug (#324 review): `--subscribe` with the default
    /// `initial_messages = 0` seeds every message into `seen` but shows none, so
    /// without this every later deletion would spuriously emit.
    #[test]
    fn deletions_to_suppress_excludes_only_shown_messages() {
        let shown_msg = authored(RoomMessageBody::public("shown".to_string()));
        let hidden_msg = authored(RoomMessageBody::public("hidden".to_string()));
        let messages = vec![shown_msg.clone(), hidden_msg.clone()];

        let shown_keys: HashSet<String> = [monitor_seen_key(&shown_msg)].into_iter().collect();
        let suppress = deletions_to_suppress_at_start(&messages, &shown_keys);
        assert!(
            !suppress.contains(&monitor_seen_key(&shown_msg)),
            "a shown message's deletion must NOT be suppressed"
        );
        assert!(
            suppress.contains(&monitor_seen_key(&hidden_msg)),
            "an unshown pre-existing message's deletion must be suppressed"
        );

        // Nothing shown (e.g. --subscribe with initial_messages == 0) → suppress all.
        let none_shown = deletions_to_suppress_at_start(&messages, &HashSet::new());
        assert_eq!(none_shown.len(), 2);
    }

    // ---- Reaction-change detection (freenet/river#325) ----

    use river_core::room_state::message::MessagesV1;
    use river_core::room_state::ChatRoomStateV1;

    /// Build a signed reaction (or remove-reaction) action message from a fixed
    /// per-`actor` signing key, targeting `target`.
    fn reaction_action(
        actor: u8,
        target: &MessageId,
        emoji: &str,
        remove: bool,
    ) -> AuthorizedMessageV1 {
        let sk = SigningKey::from_bytes(&[actor; 32]);
        let owner = SigningKey::from_bytes(&[6u8; 32]).verifying_key();
        let content = if remove {
            RoomMessageBody::remove_reaction(target.clone(), emoji.to_string())
        } else {
            RoomMessageBody::reaction(target.clone(), emoji.to_string())
        };
        let m = MessageV1 {
            room_owner: MemberId::from(owner),
            author: MemberId::from(&sk.verifying_key()),
            content,
            // Distinct non-UNIX-EPOCH time so reaction actions don't collide.
            time: SystemTime::UNIX_EPOCH + Duration::from_secs(actor as u64),
        };
        AuthorizedMessageV1::new(m, &sk)
    }

    /// A `ChatRoomStateV1` whose `recent_messages` contains `original` plus the
    /// given reaction action messages, with `actions_state` rebuilt so
    /// `reactions()` reflects them.
    fn state_with_reactions(
        original: &AuthorizedMessageV1,
        reaction_actions: Vec<AuthorizedMessageV1>,
    ) -> ChatRoomStateV1 {
        let mut messages = vec![original.clone()];
        messages.extend(reaction_actions);
        let mut recent = MessagesV1 {
            messages,
            ..Default::default()
        };
        recent.rebuild_actions_state();
        ChatRoomStateV1 {
            recent_messages: recent,
            ..Default::default()
        }
    }

    /// The reactions fingerprint is independent of `HashMap`/`Vec` iteration
    /// order: the same set of (emoji, reactors) yields the same string however
    /// the underlying collections happen to be ordered. Without this, the
    /// monitor would emit phantom `reaction` events every time the map reordered.
    #[test]
    fn reactions_fingerprint_is_order_independent() {
        let a = MemberId(freenet_scaffold::util::FastHash(10));
        let b = MemberId(freenet_scaffold::util::FastHash(20));
        let mut m1: HashMap<String, Vec<MemberId>> = HashMap::new();
        m1.insert("👍".to_string(), vec![a, b]);
        m1.insert("❤️".to_string(), vec![b]);
        let mut m2: HashMap<String, Vec<MemberId>> = HashMap::new();
        // Different insertion order + reversed reactor order — same semantic set.
        m2.insert("❤️".to_string(), vec![b]);
        m2.insert("👍".to_string(), vec![b, a]);
        assert_eq!(
            reactions_fingerprint(Some(&m1)),
            reactions_fingerprint(Some(&m2)),
            "reordering emojis/reactors must not change the fingerprint"
        );
    }

    /// `None` (no reactions) and an empty map fingerprint identically, and any
    /// non-empty reaction set differs from them — so adding the first reaction is
    /// detected as a change and removing the last reaction is too.
    #[test]
    fn reactions_fingerprint_none_empty_and_nonempty() {
        let empty: HashMap<String, Vec<MemberId>> = HashMap::new();
        assert_eq!(
            reactions_fingerprint(None),
            reactions_fingerprint(Some(&empty))
        );
        assert_eq!(reactions_fingerprint(None), "");

        let a = MemberId(freenet_scaffold::util::FastHash(10));
        let mut one: HashMap<String, Vec<MemberId>> = HashMap::new();
        one.insert("👍".to_string(), vec![a]);
        assert_ne!(
            reactions_fingerprint(Some(&one)),
            reactions_fingerprint(None)
        );
    }

    /// Codex-review regression: reaction labels are arbitrary unvalidated
    /// strings, so the fingerprint MUST distinguish label sets that a naive
    /// delimiter scheme would collide. `{"a":[1], "b":[2]}` and `{"a=1|b":[2]}`
    /// both render as `a=1|b=2` under a `|`/`=`/`,` scheme — they MUST get
    /// different fingerprints, or a live reaction change using such labels would
    /// be classified Unchanged and silently dropped.
    #[test]
    fn reactions_fingerprint_distinguishes_delimiter_colliding_labels() {
        let one = MemberId(freenet_scaffold::util::FastHash(1));
        let two = MemberId(freenet_scaffold::util::FastHash(2));
        let mut m1: HashMap<String, Vec<MemberId>> = HashMap::new();
        m1.insert("a".to_string(), vec![one]);
        m1.insert("b".to_string(), vec![two]);
        let mut m2: HashMap<String, Vec<MemberId>> = HashMap::new();
        m2.insert("a=1|b".to_string(), vec![two]);
        assert_ne!(
            reactions_fingerprint(Some(&m1)),
            reactions_fingerprint(Some(&m2)),
            "delimiter-colliding labels must not produce equal fingerprints"
        );
    }

    /// An actor swap that keeps the count constant (A removes 👍, B adds 👍)
    /// still changes the fingerprint — the fingerprint captures WHO reacted, not
    /// just the count, so a bridge sees the change.
    #[test]
    fn reactions_fingerprint_detects_actor_swap_at_constant_count() {
        let a = MemberId(freenet_scaffold::util::FastHash(10));
        let b = MemberId(freenet_scaffold::util::FastHash(20));
        let mut before: HashMap<String, Vec<MemberId>> = HashMap::new();
        before.insert("👍".to_string(), vec![a]);
        let mut after: HashMap<String, Vec<MemberId>> = HashMap::new();
        after.insert("👍".to_string(), vec![b]);
        assert_ne!(
            reactions_fingerprint(Some(&before)),
            reactions_fingerprint(Some(&after)),
            "same emoji + same count but different reactor must register as a change"
        );
    }

    /// The pure reaction decision: a key NOT in `seen_reactions` (an unsurfaced
    /// message) is NotSurfaced; an unchanged fingerprint is Unchanged; a changed
    /// fingerprint for a surfaced (seeded) message is Changed. Mirrors
    /// `classify_seen_detects_new_unchanged_edited`.
    #[test]
    fn classify_reaction_notsurfaced_unchanged_changed() {
        let mut seen: HashMap<String, String> = HashMap::new();
        assert_eq!(
            classify_reaction(&seen, "k1", "👍=1"),
            ReactionEmit::NotSurfaced,
            "a message never surfaced to the stream must not emit reaction events"
        );
        seen.insert("k1".to_string(), "👍=1".to_string());
        assert_eq!(
            classify_reaction(&seen, "k1", "👍=1"),
            ReactionEmit::Unchanged
        );
        assert_eq!(
            classify_reaction(&seen, "k1", "👍=1|❤=2"),
            ReactionEmit::Changed,
            "a changed reactions fingerprint for a surfaced message is a reaction event"
        );
    }

    /// END-TO-END root-cause regression for #325: a reaction added AFTER a
    /// surfaced message was streamed must surface as a change, even though the
    /// message's effective text is unchanged (so the old text-only
    /// `classify_seen` returned Unchanged and emitted nothing).
    ///
    /// `seen_reactions` is pre-seeded for the (surfaced) message — exactly what
    /// the startup display loop / `emit_new_and_edited` do when they surface it.
    /// `emit_reaction_changes` over the post-reaction state then advances the
    /// stored fingerprint (and prints the event); the text fingerprint is
    /// identical across both states, proving text-only detection misses it.
    #[test]
    fn live_reaction_change_is_detected_when_text_is_unchanged() {
        let original = authored(RoomMessageBody::public("hello".to_string()));
        let target = original.id();

        // State A: message present, no reactions yet (as first streamed).
        let state_a = state_with_reactions(&original, vec![]);
        // State B: same message, now with a 👍 reaction added live.
        let state_b =
            state_with_reactions(&original, vec![reaction_action(7, &target, "👍", false)]);

        let key = monitor_seen_key(&original);

        // The message's effective TEXT is identical in both states — this is why
        // the text-only `classify_seen` path never re-emitted (the #325 bug).
        let text_a = message_display_text(&state_a, &original);
        let text_b = message_display_text(&state_b, &original);
        assert_eq!(text_a, text_b, "text unchanged by adding a reaction");
        assert_eq!(
            classify_seen(
                &[(key.clone(), text_a.clone())].into_iter().collect(),
                &key,
                &text_b
            ),
            EmitKind::Unchanged,
            "text-only detection misses the reaction — the bug #325 fixes"
        );

        // Surface the message: seed its reactions fingerprint from state A, as the
        // stream does when it first shows/emits the message.
        let owner_vk = SigningKey::from_bytes(&[6u8; 32]).verifying_key();
        let mut seen_reactions: HashMap<String, String> = HashMap::new();
        seen_reactions.insert(
            key.clone(),
            reactions_fingerprint(state_a.recent_messages.reactions(&target)),
        );

        // The reactions fingerprint changed → the new path flags Changed.
        let fp_b = reactions_fingerprint(state_b.recent_messages.reactions(&target));
        assert_eq!(
            classify_reaction(&seen_reactions, &key, &fp_b),
            ReactionEmit::Changed,
            "a reaction added after the message was surfaced must be detected"
        );

        // emit_reaction_changes over state B advances the stored fingerprint.
        let before = seen_reactions.get(&key).cloned();
        ApiClient::emit_reaction_changes(
            &state_b,
            &mut seen_reactions,
            &owner_vk,
            &OutputFormat::Json,
            &HashMap::new(),
        )
        .unwrap();
        let after = seen_reactions.get(&key).cloned();
        assert_ne!(
            before, after,
            "the stored fingerprint must advance on a live reaction change"
        );
        assert_eq!(after.as_deref(), Some(fp_b.as_str()));
    }

    /// Codex-review regression: a reaction to a message the stream NEVER surfaced
    /// (room history outside the `--subscribe` initial window) must NOT emit. The
    /// message is absent from `seen_reactions`, so `emit_reaction_changes` leaves
    /// it absent (does not seed it) and emits nothing — matching the deletion
    /// path's "only for messages the stream displayed" rule.
    #[test]
    fn reaction_to_unsurfaced_message_is_suppressed() {
        let original = authored(RoomMessageBody::public("old, never shown".to_string()));
        let target = original.id();
        let state = state_with_reactions(&original, vec![reaction_action(7, &target, "👍", false)]);
        let key = monitor_seen_key(&original);
        let owner_vk = SigningKey::from_bytes(&[6u8; 32]).verifying_key();

        // seen_reactions is EMPTY: this message was never surfaced (not shown at
        // start, not emitted live).
        let mut seen: HashMap<String, String> = HashMap::new();
        let fp = reactions_fingerprint(state.recent_messages.reactions(&target));
        assert_ne!(fp, "", "the message carries a reaction");
        assert_eq!(
            classify_reaction(&seen, &key, &fp),
            ReactionEmit::NotSurfaced
        );

        // emit_reaction_changes must NOT seed it (which would let a *later*
        // reaction flip-flop to Changed and emit — the codex-found bug).
        ApiClient::emit_reaction_changes(
            &state,
            &mut seen,
            &owner_vk,
            &OutputFormat::Json,
            &HashMap::new(),
        )
        .unwrap();
        assert!(
            !seen.contains_key(&key),
            "an unsurfaced message must not be seeded by emit_reaction_changes; \
             otherwise a subsequent reaction to it would spuriously emit"
        );
    }

    /// A reaction ALREADY present when a SURFACED message was streamed must NOT
    /// re-emit as a live change — it was already reported on the message event
    /// (the issue's "reactions present at first emit are already reported"). With
    /// the fingerprint pre-seeded (as surfacing does), a pass over the SAME state
    /// is Unchanged and emits nothing.
    #[test]
    fn preexisting_reaction_on_surfaced_message_is_not_reemitted() {
        let original = authored(RoomMessageBody::public("hi".to_string()));
        let target = original.id();
        let state = state_with_reactions(&original, vec![reaction_action(7, &target, "👍", false)]);
        let key = monitor_seen_key(&original);
        let owner_vk = SigningKey::from_bytes(&[6u8; 32]).verifying_key();

        // Surface the message WITH its current reaction (as emit_new_and_edited /
        // the startup display loop do).
        let fp = reactions_fingerprint(state.recent_messages.reactions(&target));
        assert_ne!(fp, "", "the message does carry a reaction");
        let mut seen: HashMap<String, String> = HashMap::new();
        seen.insert(key.clone(), fp.clone());

        // A pass over the SAME state is Unchanged — no spurious re-emit.
        assert_eq!(
            classify_reaction(&seen, &key, &fp),
            ReactionEmit::Unchanged,
            "an unchanged reaction set must not re-emit on every poll"
        );
        ApiClient::emit_reaction_changes(
            &state,
            &mut seen,
            &owner_vk,
            &OutputFormat::Json,
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(
            seen.get(&key).cloned(),
            Some(fp),
            "fingerprint stays put across an unchanged pass"
        );
    }

    /// Removing the last reaction (count → 0) is also a change: the fingerprint
    /// goes from non-empty back to empty, so a bridge learns the reaction was
    /// retracted.
    #[test]
    fn live_reaction_removal_is_detected() {
        let original = authored(RoomMessageBody::public("hi".to_string()));
        let target = original.id();
        let with_reaction =
            state_with_reactions(&original, vec![reaction_action(7, &target, "👍", false)]);
        let after_removal = state_with_reactions(
            &original,
            vec![
                reaction_action(7, &target, "👍", false),
                reaction_action(7, &target, "👍", true),
            ],
        );
        let key = monitor_seen_key(&original);
        let mut seen: HashMap<String, String> = HashMap::new();
        seen.insert(
            key.clone(),
            reactions_fingerprint(with_reaction.recent_messages.reactions(&target)),
        );
        let fp_after = reactions_fingerprint(after_removal.recent_messages.reactions(&target));
        assert_eq!(
            classify_reaction(&seen, &key, &fp_after),
            ReactionEmit::Changed,
            "removing the last reaction must surface as a change"
        );
        assert_eq!(fp_after, "", "no reactions left → empty fingerprint");
    }

    /// `emit_new_and_edited` SEEDS `seen_reactions` for every message it surfaces
    /// (new or edited). This is the wiring that makes the suppression rule work:
    /// a brand-new message becomes eligible for later reaction events the moment
    /// it's emitted, while a message it does NOT surface stays absent (so a
    /// reaction to an unsurfaced message is suppressed). Without this seeding the
    /// reaction path would silently never fire for live messages, or (the
    /// codex-found bug) fire for unshown history.
    #[test]
    fn emit_new_and_edited_seeds_reactions_for_surfaced_messages() {
        let original = authored(RoomMessageBody::public("brand new".to_string()));
        let target = original.id();
        // New message carrying a 👍 at the moment it's first surfaced.
        let state = state_with_reactions(&original, vec![reaction_action(7, &target, "👍", false)]);
        let key = monitor_seen_key(&original);
        let owner_vk = SigningKey::from_bytes(&[6u8; 32]).verifying_key();

        let mut seen: HashMap<String, String> = HashMap::new();
        let mut deleted_emitted: HashSet<String> = HashSet::new();
        let mut seen_reactions: HashMap<String, String> = HashMap::new();
        let mut new_count = 0usize;

        ApiClient::emit_new_and_edited(
            &state,
            &mut seen,
            &mut deleted_emitted,
            &mut seen_reactions,
            &owner_vk,
            &OutputFormat::Json,
            0,
            &mut new_count,
            &HashMap::new(),
        )
        .unwrap();

        assert_eq!(new_count, 1, "the new message was emitted");
        let expected_fp = reactions_fingerprint(state.recent_messages.reactions(&target));
        assert_eq!(
            seen_reactions.get(&key).map(String::as_str),
            Some(expected_fp.as_str()),
            "emit_new_and_edited must seed the surfaced message's reactions \
             fingerprint, so a later reaction to it is reported (and its initial \
             reaction is not re-emitted as a change)"
        );

        // The just-surfaced message's CURRENT reactions are Unchanged (already
        // reported on the message event) — emit_reaction_changes won't re-emit.
        assert_eq!(
            classify_reaction(&seen_reactions, &key, &expected_fp),
            ReactionEmit::Unchanged
        );
    }
}

#[cfg(test)]
mod mention_cli_tests {
    use super::*;
    use river_core::mention::encode_mention;
    use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
    use std::time::SystemTime;

    fn member_id(sk: &SigningKey) -> MemberId {
        MemberId::from(&sk.verifying_key())
    }

    /// Build a room state whose `member_info` carries the given (key, nickname)
    /// entries.
    fn state_with_members(members: &[(SigningKey, SealedBytes)]) -> ChatRoomStateV1 {
        let mut state = ChatRoomStateV1::default();
        for (i, (sk, nickname)) in members.iter().enumerate() {
            let info = MemberInfo {
                member_id: member_id(sk),
                version: i as u32,
                preferred_nickname: nickname.clone(),
                deputies: Vec::new(),
            };
            state
                .member_info
                .member_info
                .push(AuthorizedMemberInfo::new_with_member_key(info, sk));
        }
        state
    }

    fn msg_with_text(text: String) -> AuthorizedMessageV1 {
        let author = SigningKey::from_bytes(&[200u8; 32]);
        let m = MessageV1 {
            room_owner: MemberId::from(author.verifying_key()),
            author: member_id(&author),
            content: RoomMessageBody::public(text),
            time: SystemTime::UNIX_EPOCH,
        };
        AuthorizedMessageV1::new(m, &author)
    }

    // --- resolve_outgoing_mentions (send path) ---

    #[test]
    fn resolves_unambiguous_public_nickname() {
        let alice = SigningKey::from_bytes(&[1u8; 32]);
        let state = state_with_members(&[(alice.clone(), SealedBytes::public(b"alice".to_vec()))]);
        let out = resolve_outgoing_mentions(&state, "hi @alice!");
        assert_eq!(
            out,
            format!("hi {}!", encode_mention(member_id(&alice), "alice"))
        );
    }

    #[test]
    fn resolved_outgoing_mention_uses_base32_ref_not_hex() {
        // The CLI send path must emit the truncated-base32 ref, never hex —
        // this pins the property directly (not transitively via encode_mention).
        let alice = SigningKey::from_bytes(&[1u8; 32]);
        let state = state_with_members(&[(alice.clone(), SealedBytes::public(b"alice".to_vec()))]);
        let out = resolve_outgoing_mentions(&state, "hi @alice!");
        let id = member_id(&alice);
        assert!(
            out.contains(&format!(
                "rv:{}",
                river_core::mention::member_id_to_short(id)
            )),
            "CLI send path emits the base32 ref: {out}"
        );
        assert!(
            !out.contains(&river_core::mention::member_id_to_hex(id)),
            "CLI send path must not emit hex: {out}"
        );
    }

    #[test]
    fn leaves_ambiguous_nickname_as_plain_text() {
        // Two members share the nickname "alice" → cannot disambiguate.
        let a = SigningKey::from_bytes(&[1u8; 32]);
        let b = SigningKey::from_bytes(&[2u8; 32]);
        let state = state_with_members(&[
            (a, SealedBytes::public(b"alice".to_vec())),
            (b, SealedBytes::public(b"alice".to_vec())),
        ]);
        assert_eq!(resolve_outgoing_mentions(&state, "hi @alice"), "hi @alice");
    }

    #[test]
    fn leaves_unknown_name_as_plain_text() {
        let alice = SigningKey::from_bytes(&[1u8; 32]);
        let state = state_with_members(&[(alice, SealedBytes::public(b"alice".to_vec()))]);
        assert_eq!(resolve_outgoing_mentions(&state, "hi @bob"), "hi @bob");
    }

    #[test]
    fn does_not_match_private_nickname() {
        // A private (encrypted) nickname has no public bytes → cannot match.
        let alice = SigningKey::from_bytes(&[1u8; 32]);
        let state = state_with_members(&[(
            alice,
            SealedBytes::private(vec![0xDE, 0xAD], [0u8; 12], 0, 5),
        )]);
        assert_eq!(resolve_outgoing_mentions(&state, "hi @alice"), "hi @alice");
    }

    #[test]
    fn leaves_already_encoded_token_untouched() {
        let alice = SigningKey::from_bytes(&[1u8; 32]);
        let state = state_with_members(&[(alice.clone(), SealedBytes::public(b"alice".to_vec()))]);
        let token = encode_mention(member_id(&alice), "alice");
        assert_eq!(resolve_outgoing_mentions(&state, &token), token);
    }

    // --- render_mentions_for_terminal (display path) ---

    #[test]
    fn render_uses_current_nickname_over_snapshot() {
        let alice = SigningKey::from_bytes(&[1u8; 32]);
        let state =
            state_with_members(&[(alice.clone(), SealedBytes::public(b"NewName".to_vec()))]);
        let text = format!("hey {}", encode_mention(member_id(&alice), "OldName"));
        assert_eq!(render_mentions_for_terminal(&state, &text), "hey @NewName");
    }

    #[test]
    fn render_falls_back_to_snapshot_for_unknown_member() {
        let ghost = SigningKey::from_bytes(&[9u8; 32]);
        let state = state_with_members(&[]); // ghost not present
        let text = format!("hey {}", encode_mention(member_id(&ghost), "Ghost"));
        assert_eq!(render_mentions_for_terminal(&state, &text), "hey @Ghost");
    }

    #[test]
    fn message_display_text_renders_mentions() {
        let alice = SigningKey::from_bytes(&[1u8; 32]);
        let state = state_with_members(&[(alice.clone(), SealedBytes::public(b"Alice".to_vec()))]);
        let msg = msg_with_text(format!("hi {}", encode_mention(member_id(&alice), "Alice")));
        // The full display path wraps render_mentions_for_terminal.
        assert_eq!(message_display_text(&state, &msg), "hi @Alice");
    }

    #[test]
    fn reply_context_display_renders_mention_in_preview() {
        use river_core::room_state::message::MessageId;
        let alice = SigningKey::from_bytes(&[1u8; 32]);
        let state = state_with_members(&[(alice.clone(), SealedBytes::public(b"Alice".to_vec()))]);
        // A reply whose quoted preview snapshot contains a mention token.
        let preview = format!("re: {}", encode_mention(member_id(&alice), "Alice"));
        let sender = SigningKey::from_bytes(&[200u8; 32]);
        let reply = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: MemberId::from(sender.verifying_key()),
                author: member_id(&sender),
                content: RoomMessageBody::reply(
                    "ok".to_string(),
                    MessageId(freenet_scaffold::util::FastHash(0)),
                    "Bob".to_string(),
                    preview,
                ),
                time: SystemTime::UNIX_EPOCH,
            },
            &sender,
        );
        let (_, rendered) = reply_context_display(&state, &reply).expect("is a reply");
        assert!(
            rendered.contains("@Alice"),
            "mention rendered in preview: {rendered}"
        );
        assert!(!rendered.contains("rv:"), "no raw token syntax: {rendered}");
    }

    #[test]
    fn reply_context_display_does_not_slice_a_boundary_mention_token() {
        use river_core::room_state::message::MessageId;
        let alice = SigningKey::from_bytes(&[1u8; 32]);
        let state = state_with_members(&[(alice.clone(), SealedBytes::public(b"Alice".to_vec()))]);
        // Pad so the raw token would straddle the 50-char display cutoff; the
        // token must be resolved before truncation, never sliced into raw syntax.
        let preview = format!(
            "{}{}",
            "x".repeat(45),
            encode_mention(member_id(&alice), "Alice")
        );
        let sender = SigningKey::from_bytes(&[200u8; 32]);
        let reply = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: MemberId::from(sender.verifying_key()),
                author: member_id(&sender),
                content: RoomMessageBody::reply(
                    "ok".to_string(),
                    MessageId(freenet_scaffold::util::FastHash(0)),
                    "Bob".to_string(),
                    preview,
                ),
                time: SystemTime::UNIX_EPOCH,
            },
            &sender,
        );
        let (_, rendered) = reply_context_display(&state, &reply).expect("is a reply");
        assert!(
            !rendered.contains("rv:") && !rendered.contains("@["),
            "no raw/partial token in boundary preview: {rendered}"
        );
    }

    /// The display path (which feeds both the `riverctl message list --format
    /// json` and the monitor-stream JSON `reply_to.preview`) appends the
    /// truncation marker when it clips, and omits it when the preview fits.
    /// Pins the requested behaviour: a cut quoted message is visibly marked,
    /// rather than silently ending mid-word. freenet/river XMPP-bridge request.
    #[test]
    fn reply_context_display_marks_truncation() {
        use river_core::room_state::message::MessageId;
        let alice = SigningKey::from_bytes(&[1u8; 32]);
        let state = state_with_members(&[(alice.clone(), SealedBytes::public(b"Alice".to_vec()))]);
        let sender = SigningKey::from_bytes(&[200u8; 32]);
        let reply_with = |preview: &str| {
            AuthorizedMessageV1::new(
                MessageV1 {
                    room_owner: MemberId::from(sender.verifying_key()),
                    author: member_id(&sender),
                    content: RoomMessageBody::reply(
                        "ok".to_string(),
                        MessageId(freenet_scaffold::util::FastHash(0)),
                        "Bob".to_string(),
                        preview.to_string(),
                    ),
                    time: SystemTime::UNIX_EPOCH,
                },
                &sender,
            )
        };

        // Long preview (no mention tokens) → clipped + marked.
        let long = "y".repeat(120);
        let (_, clipped) = reply_context_display(&state, &reply_with(&long)).expect("is a reply");
        assert!(
            clipped.ends_with("..."),
            "long preview is marked as truncated: {clipped}"
        );
        assert_eq!(clipped.chars().count(), 53, "50 content chars + ellipsis");

        // Short preview → shown verbatim, no marker.
        let (_, whole) = reply_context_display(&state, &reply_with("short")).expect("is a reply");
        assert_eq!(whole, "short");
    }
}
