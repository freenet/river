//! Runtime resolution of River's room-contract pointer record.
//!
//! # Why riverctl cannot derive its room key from its own WASM alone
//!
//! A room's contract key is `BLAKE3(BLAKE3(room_contract.wasm) ‖ CBOR{owner})`,
//! so every room-contract WASM change moves the key for every room at once.
//! River has re-keyed the room contract 31 times.
//!
//! riverctl compiles the WASM in and derives keys from it. That is correct at
//! build time and wrong three months later, because riverctl is installed from
//! crates.io and then kept. When the compiled-in generation is older than the
//! live one, riverctl derives the OLD key, finds nothing, and falls into its
//! backward probe, which searches OLDER generations only. The live room is
//! FORWARD of where the probe looks, so the probe can never reach it; worse, it
//! can find an ancient copy and migrate that forward onto a retired key.
//!
//! River publishes a **pointer record** for exactly this: a signed record at a
//! fixed address naming the room contract's current code hash. Resolving it
//! re-anchors riverctl at runtime. See `FREENET.md` and freenet-core#5194.
//!
//! # What lives here
//!
//! Everything in this module is pure and node-free: constants, the
//! classification of a resolved hash against what this binary knows, the
//! mapping from a [`PointerOutcome`] to an anchor, the backward-probe plan, and
//! the on-disk floor representation. The one network-bound piece, the
//! [`freenet_migrate::pointer::PointerIo`] implementation over riverctl's
//! existing node connection, lives in [`crate::api`] next to the connection it
//! borrows.

use anyhow::{anyhow, bail, Result};
use ed25519_dalek::VerifyingKey;
use freenet_migrate::pointer::{PointerFloor, PointerOutcome};
use freenet_stdlib::prelude::{ContractInstanceId, ContractKey};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The `app_id` River publishes the room-contract pointer under.
pub const ROOM_CONTRACT_APP_ID: &[u8] = b"river.room-contract";

/// River's author verifying key, as published in this repository's
/// `FREENET.md`, in the same `river:v1:vk:` presentation form.
///
/// This constant is the entire trust anchor for pointer resolution: a pointer
/// record is only ever accepted if it verifies against this key. It is
/// deliberately spelled exactly as `FREENET.md` spells it so the two can be
/// compared by eye, and pinned by
/// [`tests::author_key_matches_the_documented_pointer_address`].
pub const RIVER_AUTHOR_VK: &str = "river:v1:vk:9Ebskq4y7NvJpTQTrF1FAxU8g6bR4Rhe4TRikXba55EJ";

/// The prefix `FREENET.md` wraps the base58 author key in. Not part of the key.
const AUTHOR_VK_PREFIX: &str = "river:v1:vk:";

/// The room-contract pointer's fixed address, as published in `FREENET.md`.
///
/// Never used to address anything: resolution derives the address from
/// `(author_vk, app_id)`. It exists so a test can prove the derivation lands on
/// the documented address, which is what makes the documented address checkable
/// rather than merely asserted.
pub const ROOM_CONTRACT_POINTER_KEY: &str = "Ai4VLoC2jGdhpcB2UU8VPo3efUoxjm1Ju9VKXqRC63Az";

/// Decode [`RIVER_AUTHOR_VK`] into the 32-byte verifying key.
///
/// Fallible rather than a `LazyLock` panic so a typo in the constant surfaces
/// as a message naming the constant, not as an abort inside an unrelated
/// command.
pub fn river_author_vk() -> Result<VerifyingKey> {
    let b58 = RIVER_AUTHOR_VK
        .strip_prefix(AUTHOR_VK_PREFIX)
        .ok_or_else(|| anyhow!("RIVER_AUTHOR_VK must start with `{AUTHOR_VK_PREFIX}`"))?;
    let bytes = decode_b58_32(b58)
        .ok_or_else(|| anyhow!("RIVER_AUTHOR_VK is not 32 base58-encoded bytes"))?;
    VerifyingKey::from_bytes(&bytes)
        .map_err(|e| anyhow!("RIVER_AUTHOR_VK is not a valid ed25519 verifying key: {e}"))
}

/// Decode a base58 string that must be exactly 32 bytes. `None` otherwise.
fn decode_b58_32(s: &str) -> Option<[u8; 32]> {
    let v = bs58::decode(s)
        .with_alphabet(bs58::Alphabet::BITCOIN)
        .into_vec()
        .ok()?;
    <[u8; 32]>::try_from(v.as_slice()).ok()
}

/// Render a code hash the way Freenet renders hashes in text.
pub fn code_hash_b58(hash: &[u8; 32]) -> String {
    bs58::encode(hash)
        .with_alphabet(bs58::Alphabet::BITCOIN)
        .into_string()
}

/// Where a room-contract code hash sits relative to what THIS binary knows.
///
/// The three cases are the whole point of resolving: they are the difference
/// between "carry on", "the network is behind us" and "we are behind the
/// network", and only the last one is unrecoverable without an upgrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Generation {
    /// The generation this binary bundles as `ROOM_CONTRACT_WASM`.
    Bundled,
    /// A generation this binary knows as a previous one. The index is into
    /// `river_core::migration::LEGACY_ROOM_CONTRACT_CODE_HASHES`, which is
    /// ordered oldest-first.
    Legacy(usize),
    /// Neither the bundled generation nor any this binary knows: River has
    /// re-keyed since this riverctl was built.
    Unknown,
}

/// Classify `code_hash` against the generations this binary knows.
pub fn classify(code_hash: &[u8; 32], bundled: &[u8; 32]) -> Generation {
    if code_hash == bundled {
        return Generation::Bundled;
    }
    match river_core::migration::LEGACY_ROOM_CONTRACT_CODE_HASHES
        .iter()
        .position(|h| h == code_hash)
    {
        Some(i) => Generation::Legacy(i),
        None => Generation::Unknown,
    }
}

/// How a run learned the room-contract code hash it is using.
///
/// Kept separate from [`Generation`] because they answer different questions:
/// `Generation` is "what is this hash", `AnchorSource` is "how much do we trust
/// that it is current". A hash can be the bundled one for two very different
/// reasons (the pointer said so, or nothing answered), and the warning the user
/// deserves differs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorSource {
    /// A signature-verified pointer record named this hash.
    Pointer,
    /// No pointer has ever been published for this `(author, app_id)`, and none
    /// has ever resolved on this install. The one arm in which falling back to
    /// a build-time key is legitimate.
    NeverPublished,
    /// The pointer could not be consulted, or answered something that moves
    /// nothing (unreachable, stale, a competing record at the floor version, a
    /// transport abort). The hash is whatever was last resolved on this install,
    /// or the bundled one if nothing ever was. Carries the reason, which the
    /// caller must make visible: this is exactly today's un-anchored behaviour,
    /// so it is not a regression, but it must not be silent either.
    Unverified(String),
}

/// The room-contract code hash this run will derive keys from, and the
/// provenance that decides what riverctl may do with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomAnchor {
    code_hash: [u8; 32],
    bundled: [u8; 32],
    generation: Generation,
    source: AnchorSource,
}

/// What a caller intends to do with a derived key.
///
/// The anchor gates operations by intent because the three intents fail
/// differently when riverctl is behind the network:
///
/// * [`KeyIntent::Read`] needs only an address. A GET against a generation this
///   binary does not know still returns the right bytes, and a decode failure
///   is visible and harmless. So a stale riverctl can still read.
/// * [`KeyIntent::Write`] sends a delta encoded by THIS binary's `river-core`.
///   A re-key usually happens *because* the state or delta shape changed, so a
///   delta from an older river-core is exactly what a newer contract's
///   `update_state` was re-keyed away from. The failure would be silent and on
///   the network rather than local, so writes refuse.
///
///   **Writes are refused only when the network is AHEAD, never when it is
///   behind**, and the asymmetry is deliberate rather than an oversight. Two
///   reasons:
///
///   The compatibility promise runs in one direction. River's rule is that
///   `ChatRoomStateV1` and its sub-types stay backwards-compatible (new fields
///   `#[serde(default)]`, and on an individually-signed sub-struct also
///   `#[serde(skip_serializing_if)]` so an unset new field serializes
///   byte-identically to the old record). That is a promise that a NEWER
///   river-core can produce bytes an OLDER contract still validates, with the
///   worst case being a field the old contract ignores. There is no promise in
///   the other direction: a newer contract may validate something this binary
///   does not know to send, and we cannot inspect it to find out.
///
///   And only one direction has a remedy. Against an unknown generation the
///   user can upgrade riverctl and the problem is gone. Against an older one
///   there is nothing they can do: they cannot make the network re-key, and
///   telling them to install a riverctl matching a superseded generation is not
///   advice. Refusing there would also break every freshly-released riverctl
///   until the network finished adopting the matching contract, which is the
///   normal state of affairs for a while after each release.
/// * [`KeyIntent::PublishCode`] must PUT the contract's actual WASM bytes.
///   riverctl only has the bytes it bundles, so this is impossible, not merely
///   unwise, whenever the anchor is not the bundled generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyIntent {
    /// GET or SUBSCRIBE: needs an address only.
    Read,
    /// UPDATE: sends a delta this binary encoded.
    Write,
    /// PUT: needs the contract's real WASM bytes.
    PublishCode,
}

impl RoomAnchor {
    /// Build an anchor for a hash that a verified pointer record named.
    fn from_pointer(code_hash: [u8; 32], bundled: [u8; 32]) -> Self {
        Self {
            code_hash,
            bundled,
            generation: classify(&code_hash, &bundled),
            source: AnchorSource::Pointer,
        }
    }

    /// Build an anchor that no pointer vouched for.
    fn unvouched(code_hash: [u8; 32], bundled: [u8; 32], source: AnchorSource) -> Self {
        Self {
            code_hash,
            bundled,
            generation: classify(&code_hash, &bundled),
            source,
        }
    }

    /// The code hash every key this run derives is built from.
    pub fn code_hash(&self) -> &[u8; 32] {
        &self.code_hash
    }

    pub fn generation(&self) -> Generation {
        self.generation
    }

    pub fn source(&self) -> &AnchorSource {
        &self.source
    }

    /// The room-contract key for `owner_vk` under this anchor.
    ///
    /// Derived from the code hash alone, so it works for a generation whose
    /// WASM this binary does not have. That is what lets a stale riverctl still
    /// address a live room.
    pub fn contract_key(&self, owner_vk: &VerifyingKey) -> ContractKey {
        river_core::migration::contract_key_for_code_hash(owner_vk, &self.code_hash)
    }

    /// The key this binary would have derived with no pointer at all: today's
    /// behaviour, kept for the paths that legitimately need the bundled
    /// generation (upgrade-pointer `known_keys`, the backward-probe candidate
    /// list when the network is ahead of us).
    pub fn bundled_contract_key(&self, owner_vk: &VerifyingKey) -> ContractKey {
        river_core::migration::contract_key_for_code_hash(owner_vk, &self.bundled)
    }

    /// Whether `intent` is permitted under this anchor. `Err` carries the
    /// message the user sees, which always names both hashes so a bug report is
    /// actionable without asking for more.
    pub fn authorize(&self, intent: KeyIntent) -> Result<()> {
        match (self.generation, intent) {
            (Generation::Bundled, _) => Ok(()),
            (Generation::Legacy(_), KeyIntent::Read | KeyIntent::Write) => Ok(()),
            (Generation::Legacy(_), KeyIntent::PublishCode) => bail!(
                "This operation has to publish the room contract's code, and riverctl only \
                 carries the bytes it was built with.\n  \
                 riverctl bundles room-contract generation {}\n  \
                 River's pointer record names generation  {}\n\
                 The network is on an OLDER generation than this riverctl, so publishing the \
                 bundled code would create a room at a key nobody is using. Read and message \
                 operations still work.",
                code_hash_b58(&self.bundled),
                code_hash_b58(&self.code_hash),
            ),
            (Generation::Unknown, KeyIntent::Read) => Ok(()),
            (Generation::Unknown, KeyIntent::Write | KeyIntent::PublishCode) => bail!(
                "{}\nRefusing to write with a room-contract generation this riverctl does not \
                 know. Reads still work; run `cargo install riverctl` (or update your package) \
                 to write.",
                self.behind_network_summary()
            ),
        }
    }

    /// The two-hash summary for the case this whole module exists for.
    pub fn behind_network_summary(&self) -> String {
        format!(
            "River's room contract is NEWER than this riverctl.\n  \
             riverctl bundles room-contract generation {}\n  \
             River's pointer record names generation  {}\n\
             This riverctl knows {} previous generation(s) and the resolved one is none of \
             them, so it was published after this binary was built.",
            code_hash_b58(&self.bundled),
            code_hash_b58(&self.code_hash),
            river_core::migration::LEGACY_ROOM_CONTRACT_CODE_HASHES.len(),
        )
    }

    /// The line to put on stderr once, at resolution time, or `None` when there
    /// is nothing worth saying.
    ///
    /// Emitted once per run rather than per derived key: the condition is a
    /// property of the run, and repeating it per operation would train users to
    /// ignore it.
    pub fn advisory(&self) -> Option<String> {
        match (&self.source, self.generation) {
            (AnchorSource::Pointer, Generation::Bundled) => None,
            (AnchorSource::Pointer, Generation::Legacy(_)) => Some(format!(
                "note: River's pointer record names room-contract generation {}, which is OLDER \
                 than the one this riverctl bundles ({}). Trusting the pointer, because it names \
                 what is actually deployed.",
                code_hash_b58(&self.code_hash),
                code_hash_b58(&self.bundled),
            )),
            (AnchorSource::Pointer, Generation::Unknown) => Some(format!(
                "warning: {}\nRead-only commands will work against the live generation. Writing \
                 needs a newer riverctl: `cargo install riverctl`.",
                self.behind_network_summary()
            )),
            (AnchorSource::NeverPublished, _) => None,
            (AnchorSource::Unverified(reason), _) => Some(format!(
                "warning: could not verify that riverctl's room-contract generation is current \
                 ({reason}). Continuing with generation {}. If River has re-keyed since this \
                 build, room operations will address the wrong contract.",
                code_hash_b58(&self.code_hash),
            )),
        }
    }
}

/// What a resolution attempt produced, flattened so the arm-by-arm mapping
/// below is a pure function that needs no node and no generic error type.
#[derive(Debug, Clone)]
pub enum ResolveReport {
    /// `resolve_app_pointer` returned an outcome.
    Outcome(PointerOutcome),
    /// `resolve_app_pointer` returned an error: a transport abort, or a
    /// `PointerError` about what the network served. Neither permits the
    /// baked-in fallback, and both are retryable.
    Failed(String),
}

/// Map a resolution attempt to the anchor this run will use.
///
/// Pure, so every arm is unit-testable without a node. **Every**
/// [`PointerOutcome`] arm is handled explicitly and on purpose: a bare
/// `if let Some(r) = outcome.resolved()` silently does nothing on the arms that
/// carry no record, which collapses a withdrawal, a refused rollback and a
/// plain timeout into the same no-op.
pub fn anchor_from_report(
    report: &ResolveReport,
    floor: &PointerFloor,
    bundled: [u8; 32],
) -> Result<RoomAnchor> {
    // A withdrawal floor is checked before anything else. A tombstone sorts
    // below every real code hash, so once the floor records a withdrawal, any
    // genuine pre-withdrawal record replayed at that version loses the tiebreak
    // and arrives as `CompetingRecord`, and resuming with the key that floor
    // superseded would resurrect, out of our own memory, exactly the code the
    // author retired.
    if floor.is_withdrawn() {
        bail!(withdrawn_message(floor.version()));
    }

    // What to use when nothing was learned: the last hash this install verified,
    // or the bundled one if it has never verified any. Preferring the floor is
    // strictly better than always falling back to the bundled hash, because it is
    // the "keep what you last resolved" the resolver asks for, and it degrades to
    // exactly today's behaviour on a first run.
    let last_known = || floor.code_hash().unwrap_or(bundled);

    let outcome = match report {
        ResolveReport::Failed(reason) => {
            return Ok(RoomAnchor::unvouched(
                last_known(),
                bundled,
                AnchorSource::Unverified(reason.clone()),
            ));
        }
        ResolveReport::Outcome(o) => o,
    };

    match outcome {
        // A record strictly newer than the floor, or the byte-identical record
        // the floor already holds. Both carry a signature-verified pointer.
        PointerOutcome::Resolved(r) | PointerOutcome::Unchanged(r) => {
            Ok(RoomAnchor::from_pointer(r.code_hash(), bundled))
        }

        // The author says there is no current code. Not "the old code is
        // current again", so there is nothing to fall back to.
        PointerOutcome::Withdrawn { version, .. } => bail!(withdrawn_message(*version)),

        // The only arm in which a build-time key is legitimate.
        PointerOutcome::NeverPublished => Ok(RoomAnchor::unvouched(
            bundled,
            bundled,
            AnchorSource::NeverPublished,
        )),

        // A validly-signed record older than our floor, refused. Routine rather
        // than an attack signal: a freshly-bootstrapped or recently-evicted node
        // can transiently serve an older record.
        PointerOutcome::Stale { served, floor: f } => Ok(RoomAnchor::unvouched(
            last_known(),
            bundled,
            AnchorSource::Unverified(format!(
                "a peer served pointer version {served}, older than the version {f} this install \
                 already verified; the rollback was refused"
            )),
        )),

        // Two valid records at one version; ours won the tiebreak. The resolver
        // deliberately hands back no record, so we keep what we last resolved
        // and do NOT pick between the competitors. Our floor is riverctl's own
        // config directory, written only by our own verified resolutions, which
        // is the provenance the resolver requires before a caller may keep using
        // its floor's hash.
        PointerOutcome::CompetingRecord { version, .. } => Ok(RoomAnchor::unvouched(
            last_known(),
            bundled,
            AnchorSource::Unverified(format!(
                "a second, different pointer record exists at version {version}; keeping the one \
                 already verified here rather than choosing between them"
            )),
        )),

        // Nothing could be learned. This is today's behaviour, so continuing is
        // not a regression, but `advisory()` makes it visible rather than
        // silent.
        PointerOutcome::Unavailable => Ok(RoomAnchor::unvouched(
            last_known(),
            bundled,
            AnchorSource::Unverified(
                "the pointer record could not be fetched (timeout, or the node had no answer)"
                    .to_string(),
            ),
        )),

        // `PointerOutcome` is `#[non_exhaustive]`. A future arm is not something
        // to guess at: treat it as "learned nothing", which is the resolver's
        // own default for every arm that carries no record.
        other => Ok(RoomAnchor::unvouched(
            last_known(),
            bundled,
            AnchorSource::Unverified(format!(
                "the pointer resolver returned an outcome this riverctl does not understand \
                 ({other:?}); upgrade riverctl"
            )),
        )),
    }
}

fn withdrawn_message(version: u32) -> String {
    format!(
        "River has WITHDRAWN its room-contract pointer record (version {version}): the author is \
         saying there is no current room-contract code, not that an older generation is current \
         again. Refusing to derive a room key. If you believe this is wrong, check \
         https://github.com/freenet/river for an announcement."
    )
}

/// What the backward probe should do, given the anchor.
///
/// The probe searches generations OLDER than where the room should be. Which
/// generations those are depends entirely on where the room should be, which is
/// the anchor. Leaving the probe anchored at the bundled generation is the
/// actual bug being fixed, not a detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbePlan {
    /// Probe these generations, newest-first.
    Probe(Vec<ContractInstanceId>),
    /// Do not probe. Carries the message to fail with.
    ///
    /// Reached only when the anchor is a generation this binary does not know.
    /// Every generation riverctl knows is then OLDER than the live one, so a
    /// backward probe would search a set that cannot contain the live room,
    /// could surface an ancient copy as if it were current, and would try to
    /// migrate that copy forward onto a retired key. That is the failure this
    /// change exists to remove, so it must not be reachable by falling through
    /// to the old path.
    Refuse(String),
}

/// Build the backward-probe plan for `owner_vk` under `anchor`.
///
/// Pure so the anchoring is unit-testable without a node.
pub fn probe_plan(owner_vk: &VerifyingKey, anchor: &RoomAnchor) -> ProbePlan {
    let key_for =
        |h: &[u8; 32]| *river_core::migration::contract_key_for_code_hash(owner_vk, h).id();
    match anchor.generation() {
        // The room lives on the bundled generation, so everything in the
        // registry is older: today's candidate list, unchanged.
        Generation::Bundled => ProbePlan::Probe(
            river_core::migration::legacy_contract_keys_for_owner(owner_vk)
                .iter()
                .map(|k| *k.id())
                .collect(),
        ),
        // The room lives on registry entry `i`, so only entries BELOW `i` are
        // older. The registry is oldest-first, so that is `[..i]` reversed.
        // Probing above `i` would search generations forward of the live one,
        // the mirror image of the bug, and a way to migrate live state onto a
        // generation nobody uses.
        Generation::Legacy(i) => ProbePlan::Probe(
            river_core::migration::LEGACY_ROOM_CONTRACT_CODE_HASHES[..i]
                .iter()
                .rev()
                .map(key_for)
                .collect(),
        ),
        Generation::Unknown => ProbePlan::Refuse(format!(
            "{}\nThe room was not found on the live generation. Not searching older generations: \
             every generation this riverctl knows is older than the live one, so a search could \
             only turn up a stale copy and would try to republish it onto a retired key. Run \
             `cargo install riverctl` to pick up the current room contract.",
            anchor.behind_network_summary()
        )),
    }
}

// ---------------------------------------------------------------------------
// Floor persistence
// ---------------------------------------------------------------------------

/// The on-disk form of a [`PointerFloor`], keyed by `(author_vk, app_id)`.
///
/// All three columns are stored, and `withdrawn` is a column of its own rather
/// than an inference from a zeroed hash. A defaulted or half-written hash column
/// looks byte-identical to a tombstone, so inferring would let one bad row turn
/// a healthy app into a permanent "the author withdrew this".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredFloor {
    pub version: u32,
    /// Base58 of the 32-byte code hash. Absent for a withdrawal floor, whose
    /// hash is the all-zero tombstone and is reconstructed from `withdrawn`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_hash: Option<String>,
    #[serde(default)]
    pub withdrawn: bool,
}

/// Every pointer floor this install holds, keyed by `"{author_vk_b58}/{app_id}"`.
///
/// Keyed by the pair, never by app_id alone: a rotation to a new author key
/// means a new pointer address and a fresh version space, so it needs a fresh
/// floor rather than the old one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FloorStore {
    #[serde(default)]
    pub floors: BTreeMap<String, StoredFloor>,
}

/// The storage key for a floor.
pub fn floor_key(author_vk: &VerifyingKey, app_id: &[u8]) -> String {
    format!(
        "{}/{}",
        code_hash_b58(author_vk.as_bytes()),
        String::from_utf8_lossy(app_id)
    )
}

impl StoredFloor {
    pub fn from_floor(floor: &PointerFloor) -> Self {
        if floor.is_withdrawn() {
            return Self {
                version: floor.version(),
                code_hash: None,
                withdrawn: true,
            };
        }
        Self {
            version: floor.version(),
            code_hash: floor.code_hash().as_ref().map(code_hash_b58),
            withdrawn: false,
        }
    }

    /// Rebuild the [`PointerFloor`] this row records.
    ///
    /// **Every failure is an error, never a fallback.** The reflex fix is
    /// `unwrap_or_else(|_| PointerFloor::never_resolved())`, and it is exactly
    /// wrong: `never_resolved` is the one state that unlocks the baked-in
    /// build-time key, so recovering that way converts a corrupt row into a
    /// silent downgrade. Surfacing lets the user delete the file deliberately.
    pub fn to_floor(&self) -> Result<PointerFloor> {
        if self.withdrawn {
            return PointerFloor::withdrawn_at(self.version).map_err(|e| {
                anyhow!("stored pointer floor records a withdrawal that cannot be rebuilt: {e}")
            });
        }
        let encoded = self.code_hash.as_deref().ok_or_else(|| {
            anyhow!(
                "stored pointer floor at version {} has no code hash, and is not marked as a \
                 withdrawal; the file is corrupt",
                self.version
            )
        })?;
        let hash = decode_b58_32(encoded).ok_or_else(|| {
            anyhow!("stored pointer floor code hash `{encoded}` is not 32 base58-encoded bytes")
        })?;
        PointerFloor::at(self.version, hash)
            .map_err(|e| anyhow!("stored pointer floor cannot be rebuilt: {e}"))
    }
}

/// The advice attached to any floor-store failure. Split out so the read path
/// and the tests share one wording.
pub fn floor_corruption_hint(path: &std::path::Path) -> String {
    format!(
        "riverctl will not treat an unreadable anti-rollback floor as \"never resolved\", because \
         that is the one state in which it would trust its own build-time key again. Inspect or \
         delete {} to continue.",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use freenet_migrate::pointer::{
        pointer_contract_id, pointer_params, pointer_signing_message, PointerRecord,
        PointerResolver,
    };
    use freenet_migrate::Step;

    fn bundled() -> [u8; 32] {
        [0xAA; 32]
    }

    fn owner() -> VerifyingKey {
        SigningKey::from_bytes(&[7u8; 32]).verifying_key()
    }

    /// Drive a real resolver against a canned record so the tests below act on
    /// genuine `PointerOutcome` values rather than hand-built ones. Hand-built
    /// outcomes would let a test pass against an outcome the resolver can never
    /// actually produce, and `Withdrawn` / `CompetingRecord` cannot be built
    /// by hand at all, being `#[non_exhaustive]` variants, which is exactly the
    /// boundary this helper respects rather than works around.
    fn outcome_for(
        author: &SigningKey,
        floor: PointerFloor,
        version: u32,
        code_hash: [u8; 32],
    ) -> PointerOutcome {
        let vk = author.verifying_key();
        let params = pointer_params(&vk, ROOM_CONTRACT_APP_ID).unwrap();
        let sig = author.sign(&pointer_signing_message(&params, version, &code_hash));
        let state = PointerRecord {
            version,
            code_hash,
            signature: sig.to_bytes(),
        }
        .encode()
        .to_vec();
        let mut r = PointerResolver::new(&vk, ROOM_CONTRACT_APP_ID, floor).unwrap();
        let Step::Get(id) = r.next_action() else {
            panic!("a fresh resolver asks for the pointer");
        };
        assert!(r.on_response(id, &state));
        r.take_outcome().unwrap().unwrap()
    }

    /// The documented pointer address in `FREENET.md` must be exactly what the
    /// documented author key and app_id derive to. If this fails, one of the two
    /// constants was edited without the other and every resolution would GET the
    /// wrong address, an error that is otherwise invisible, because a wrong
    /// address simply never answers.
    #[test]
    fn author_key_matches_the_documented_pointer_address() {
        let vk = river_author_vk().expect("RIVER_AUTHOR_VK must decode");
        let derived = pointer_contract_id(&vk, ROOM_CONTRACT_APP_ID).unwrap();
        assert_eq!(
            derived.to_string(),
            ROOM_CONTRACT_POINTER_KEY,
            "the author key and app_id in this module no longer derive the pointer address \
             published in FREENET.md"
        );
    }

    #[test]
    fn classify_recognises_bundled_legacy_and_unknown() {
        let b = bundled();
        assert_eq!(classify(&b, &b), Generation::Bundled);
        let legacy = river_core::migration::LEGACY_ROOM_CONTRACT_CODE_HASHES[3];
        assert_eq!(classify(&legacy, &b), Generation::Legacy(3));
        assert_eq!(classify(&[0x11; 32], &b), Generation::Unknown);
    }

    /// The case the change exists for: a resolved hash this binary does not
    /// know must produce the upgrade message, must refuse writes, and must NOT
    /// hand the backward probe a candidate list.
    #[test]
    fn unknown_generation_refuses_writes_and_refuses_to_probe() {
        let author = SigningKey::from_bytes(&[3u8; 32]);
        let live = [0x11; 32];
        let outcome = outcome_for(&author, PointerFloor::never_resolved(), 4, live);
        let anchor = anchor_from_report(
            &ResolveReport::Outcome(outcome),
            &PointerFloor::never_resolved(),
            bundled(),
        )
        .expect("an unknown generation is a usable anchor, not an error");

        assert_eq!(anchor.generation(), Generation::Unknown);
        // The key is derived from the LIVE hash, which is the whole win: a
        // stale riverctl can still address the room it could not reach before.
        assert_eq!(
            anchor.contract_key(&owner()).id(),
            river_core::migration::contract_key_for_code_hash(&owner(), &live).id()
        );

        anchor.authorize(KeyIntent::Read).expect("reads proceed");
        let write_err = anchor.authorize(KeyIntent::Write).unwrap_err().to_string();
        assert!(
            write_err.contains("NEWER than this riverctl"),
            "{write_err}"
        );
        assert!(write_err.contains(&code_hash_b58(&live)), "{write_err}");
        assert!(
            write_err.contains(&code_hash_b58(&bundled())),
            "{write_err}"
        );
        assert!(write_err.contains("cargo install riverctl"), "{write_err}");
        anchor
            .authorize(KeyIntent::PublishCode)
            .expect_err("publishing code needs bytes we do not have");

        let advisory = anchor.advisory().expect("this must be announced");
        assert!(advisory.contains("NEWER than this riverctl"), "{advisory}");

        match probe_plan(&owner(), &anchor) {
            ProbePlan::Refuse(msg) => {
                assert!(msg.contains("Not searching older generations"), "{msg}");
                assert!(msg.contains(&code_hash_b58(&live)), "{msg}");
            }
            ProbePlan::Probe(c) => panic!("must not probe from a stale anchor; got {c:?}"),
        }
    }

    /// The normal case must be indistinguishable from today: same key, no
    /// advisory, no gating, and the full legacy registry as probe candidates.
    #[test]
    fn resolved_equals_bundled_behaves_exactly_as_before() {
        let author = SigningKey::from_bytes(&[3u8; 32]);
        let outcome = outcome_for(&author, PointerFloor::never_resolved(), 1, bundled());
        let anchor = anchor_from_report(
            &ResolveReport::Outcome(outcome),
            &PointerFloor::never_resolved(),
            bundled(),
        )
        .unwrap();

        assert_eq!(anchor.generation(), Generation::Bundled);
        assert_eq!(anchor.advisory(), None);
        for intent in [KeyIntent::Read, KeyIntent::Write, KeyIntent::PublishCode] {
            anchor.authorize(intent).expect("nothing is gated");
        }
        assert_eq!(
            anchor.contract_key(&owner()).id(),
            anchor.bundled_contract_key(&owner()).id()
        );
        let expected: Vec<_> = river_core::migration::legacy_contract_keys_for_owner(&owner())
            .iter()
            .map(|k| *k.id())
            .collect();
        assert_eq!(probe_plan(&owner(), &anchor), ProbePlan::Probe(expected));
    }

    /// A pointer naming an older generation is trusted, because it names what
    /// is actually deployed, but the probe must then search only generations
    /// older than THAT, and code publishing must refuse.
    #[test]
    fn legacy_generation_is_trusted_and_reanchors_the_probe() {
        let author = SigningKey::from_bytes(&[3u8; 32]);
        let legacy = river_core::migration::LEGACY_ROOM_CONTRACT_CODE_HASHES[5];
        let outcome = outcome_for(&author, PointerFloor::never_resolved(), 2, legacy);
        let anchor = anchor_from_report(
            &ResolveReport::Outcome(outcome),
            &PointerFloor::never_resolved(),
            bundled(),
        )
        .unwrap();

        assert_eq!(anchor.generation(), Generation::Legacy(5));
        anchor.authorize(KeyIntent::Read).unwrap();
        anchor.authorize(KeyIntent::Write).unwrap();
        anchor.authorize(KeyIntent::PublishCode).unwrap_err();
        assert!(anchor.advisory().unwrap().contains("OLDER"));

        let ProbePlan::Probe(candidates) = probe_plan(&owner(), &anchor) else {
            panic!("a known generation is probeable");
        };
        // Strictly older only: five entries (indices 4..=0), newest-first.
        assert_eq!(candidates.len(), 5);
        let newest_older = river_core::migration::contract_key_for_code_hash(
            &owner(),
            &river_core::migration::LEGACY_ROOM_CONTRACT_CODE_HASHES[4],
        );
        assert_eq!(candidates[0], *newest_older.id());
        let anchor_id = *anchor.contract_key(&owner()).id();
        assert!(
            !candidates.contains(&anchor_id),
            "the probe must not re-probe the generation it is anchored on"
        );
    }

    /// Unreachable must not change the derived key, and must not be silent.
    #[test]
    fn unavailable_falls_back_to_bundled_with_a_visible_warning() {
        let anchor = anchor_from_report(
            &ResolveReport::Outcome(PointerOutcome::Unavailable),
            &PointerFloor::never_resolved(),
            bundled(),
        )
        .unwrap();
        assert_eq!(*anchor.code_hash(), bundled());
        assert_eq!(anchor.generation(), Generation::Bundled);
        // Not gated: this is exactly today's behaviour, so gating it would break
        // every user whose node cannot reach the pointer.
        anchor.authorize(KeyIntent::PublishCode).unwrap();
        let advisory = anchor.advisory().expect("must warn");
        assert!(advisory.starts_with("warning:"), "{advisory}");
        assert!(advisory.contains("could not verify"), "{advisory}");
    }

    /// A transport abort is the same story as `Unavailable`, and must reach the
    /// same place rather than aborting the command.
    #[test]
    fn transport_failure_falls_back_to_bundled_with_a_visible_warning() {
        let anchor = anchor_from_report(
            &ResolveReport::Failed("websocket send failed".to_string()),
            &PointerFloor::never_resolved(),
            bundled(),
        )
        .unwrap();
        assert_eq!(*anchor.code_hash(), bundled());
        assert!(anchor.advisory().unwrap().contains("websocket send failed"));
    }

    /// When something WAS resolved before, "keep what you last resolved" beats
    /// falling back to the build-time key: falling back would be the downgrade
    /// an attacker gets for free by making the pointer briefly unreachable.
    #[test]
    fn unavailable_keeps_the_last_resolved_hash_over_the_bundled_one() {
        let live = [0x11; 32];
        let floor = PointerFloor::at(9, live).unwrap();
        let anchor = anchor_from_report(
            &ResolveReport::Outcome(PointerOutcome::Unavailable),
            &floor,
            bundled(),
        )
        .unwrap();
        assert_eq!(*anchor.code_hash(), live);
        assert_eq!(anchor.generation(), Generation::Unknown);
        anchor
            .authorize(KeyIntent::Write)
            .expect_err("still an unknown generation, so writes still refuse");
    }

    #[test]
    fn stale_and_competing_record_keep_the_last_resolved_hash() {
        let author = SigningKey::from_bytes(&[3u8; 32]);
        let live = [0x11; 32];
        let floor = PointerFloor::at(9, live).unwrap();
        for outcome in [
            // A peer serving an older-but-validly-signed record: refused as a
            // rollback.
            outcome_for(&author, floor, 3, [0x44; 32]),
            // A second, different record at the floor's own version. The floor
            // holds `live`, and `0xFF..` sorts above it, so the served record
            // loses the tiebreak.
            outcome_for(&author, floor, 9, [0xFF; 32]),
        ] {
            let anchor =
                anchor_from_report(&ResolveReport::Outcome(outcome), &floor, bundled()).unwrap();
            assert_eq!(*anchor.code_hash(), live);
            assert!(anchor.advisory().is_some());
        }
    }

    #[test]
    fn never_published_falls_back_to_bundled_silently() {
        let anchor = anchor_from_report(
            &ResolveReport::Outcome(PointerOutcome::NeverPublished),
            &PointerFloor::never_resolved(),
            bundled(),
        )
        .unwrap();
        assert_eq!(*anchor.code_hash(), bundled());
        assert_eq!(anchor.source(), &AnchorSource::NeverPublished);
        assert_eq!(
            anchor.advisory(),
            None,
            "the one legitimate baked-in fallback needs no warning"
        );
    }

    #[test]
    fn withdrawn_refuses() {
        let author = SigningKey::from_bytes(&[3u8; 32]);
        // A withdrawal IS a signed record; its code hash is the all-zero
        // tombstone. Built through the resolver, so the test acts on the same
        // value production would see.
        let withdrawn = outcome_for(&author, PointerFloor::never_resolved(), 12, [0u8; 32]);
        assert!(matches!(withdrawn, PointerOutcome::Withdrawn { .. }));
        let err = anchor_from_report(
            &ResolveReport::Outcome(withdrawn),
            &PointerFloor::never_resolved(),
            bundled(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("WITHDRAWN"), "{err}");
        assert!(err.contains("version 12"), "{err}");
    }

    /// A withdrawal already recorded in the floor must refuse before any record
    /// is consulted. Otherwise a peer replaying a genuine pre-withdrawal record
    /// arrives as `CompetingRecord` and "keep your last key" resurrects exactly
    /// the code the withdrawal retired.
    #[test]
    fn a_withdrawn_floor_refuses_every_arm() {
        let author = SigningKey::from_bytes(&[3u8; 32]);
        let floor = PointerFloor::withdrawn_at(12).unwrap();
        // A genuine pre-withdrawal record replayed at the withdrawal's version:
        // the tombstone sorts below every real hash, so it loses the tiebreak
        // and arrives as `CompetingRecord`. This is the arm that would
        // otherwise resurrect the retired code out of our own floor.
        let replayed = outcome_for(&author, floor, 12, [0x44; 32]);
        assert!(matches!(replayed, PointerOutcome::CompetingRecord { .. }));
        for report in [
            ResolveReport::Outcome(PointerOutcome::Unavailable),
            ResolveReport::Outcome(replayed),
            ResolveReport::Outcome(PointerOutcome::NeverPublished),
            ResolveReport::Failed("boom".to_string()),
        ] {
            let err = anchor_from_report(&report, &floor, bundled())
                .unwrap_err()
                .to_string();
            assert!(err.contains("WITHDRAWN"), "{err}");
        }
    }

    #[test]
    fn floor_round_trips_through_its_stored_form() {
        let floor = PointerFloor::at(7, [0x22; 32]).unwrap();
        let stored = StoredFloor::from_floor(&floor);
        assert_eq!(stored.to_floor().unwrap(), floor);

        let withdrawn = PointerFloor::withdrawn_at(8).unwrap();
        let stored = StoredFloor::from_floor(&withdrawn);
        assert!(stored.withdrawn);
        assert_eq!(stored.to_floor().unwrap(), withdrawn);
    }

    /// A corrupt row must surface, never silently become `never_resolved`,
    /// which is the one state that unlocks the build-time key again.
    #[test]
    fn a_corrupt_stored_floor_surfaces_rather_than_resetting() {
        let cases = [
            StoredFloor {
                version: 0,
                code_hash: Some(code_hash_b58(&[0x22; 32])),
                withdrawn: false,
            },
            StoredFloor {
                version: 3,
                code_hash: Some("not base58!!".to_string()),
                withdrawn: false,
            },
            StoredFloor {
                version: 3,
                code_hash: Some(code_hash_b58(&[0u8; 32])),
                withdrawn: false,
            },
            StoredFloor {
                version: 3,
                code_hash: None,
                withdrawn: false,
            },
            StoredFloor {
                version: 0,
                code_hash: None,
                withdrawn: true,
            },
        ];
        for case in cases {
            assert!(
                case.to_floor().is_err(),
                "corrupt floor {case:?} must not rebuild"
            );
        }
    }

    #[test]
    fn floor_keys_are_scoped_to_the_author_key() {
        let a = SigningKey::from_bytes(&[1u8; 32]).verifying_key();
        let b = SigningKey::from_bytes(&[2u8; 32]).verifying_key();
        assert_ne!(
            floor_key(&a, ROOM_CONTRACT_APP_ID),
            floor_key(&b, ROOM_CONTRACT_APP_ID),
            "rotating the author key must not inherit the old key's floor"
        );
        assert!(floor_key(&a, ROOM_CONTRACT_APP_ID).ends_with("/river.room-contract"));
    }
}
