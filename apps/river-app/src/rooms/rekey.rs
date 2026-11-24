//! Following a room across a room-contract re-key (freenet/river#889).
//!
//! A room's contract key is a hash of the room contract's WASM and its owner, so every River
//! release that changes that WASM moves every room to a new key. The web UI follows: on load it
//! derives the key from the WASM it carries (`RoomData::regenerate_contract_key`), and when the
//! new key has no state on the network it carries the room there from an older key
//! (`start_backward_probe` and `start_rekey_merge` in `ui/src/components/app/freenet_api/
//! backward_probe.rs`). This app does the same for what it needs:
//!
//! - **The key followed.** A stored room whose contract key is one of this owner's earlier keys
//!   (`river_core::migration::legacy_contract_keys_for_owner`, compiled in from
//!   `common/legacy_room_contracts.toml`, never from the network) is followed under the key this
//!   build's WASM gives it ([`current_key`]). Any other key is kept: it is the current one, or
//!   one from a NEWER River, which this build must not move back.
//! - **Carrying the state.** Only when the current key answers `NotFound` (never on another
//!   failure). A stored room is then followed under its earlier key again while it is moved:
//!   every earlier key down to the one it was stored under (the web UI's `rekey_candidates`) is
//!   folded into the copy the user holds ([`Policy::Fold`], the web UI's `start_rekey_merge`),
//!   and the result PUT under the current key; when every earlier key answered with nothing, the
//!   user's copy is PUT, as the web UI seeds it. A move that did not end well is tried again
//!   after a wait that doubles, and keys that did not answer are folded in later (a PUT of a
//!   contract that exists merges). A join or an ID import, which hold no copy, reads every
//!   earlier key newest first for the newest real room ([`Policy::Newest`], the web UI's
//!   backward probe), for at most [`super::work::PROBE_BUDGET`]. The PUT's state has its upgrade
//!   pointer stripped and is normalized, as the web UI's `put_state_to_current_key` sends it.
//!   freenet-migrate's [`ProbeDriver`] orders the reads and decides, as it does for the web UI. A
//!   read that goes unanswered is passed over without a new connection: its answer names a key
//!   nothing else asks for. `NotFound` is not proof of absence; the PUT is safe because the room
//!   contract merges.
//! - **What is left to the web UI.** Folding earlier keys into a current key that already had
//!   state when this app first followed it (the web UI's owed re-key merge, `rekey_merge`, which
//!   a carried slot keeps). An owner who uses only this app does not publish the upgrade pointer
//!   on the old contract.

use std::sync::Arc;

use ed25519_dalek::VerifyingKey;
use freenet_migrate::{
    FoldAllAck, NewestFirst, Outcome, ProbeDriver, ProbeStateOps, RollbackRiskAck, SelectionPolicy,
    Step,
};
use freenet_stdlib::client_api::ContractRequest;
use freenet_stdlib::prelude::{
    ContractCode, ContractContainer, ContractInstanceId, ContractKey, ContractWasmAPIVersion,
    Parameters, WrappedContract, WrappedState,
};
use river_core::migration::legacy_contract_keys_for_owner;
use river_core::room_state::upgrade::OptionalUpgradeV1;
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1};
use river_core::stored_rooms::StoredRoom;

use super::create::{self, ROOM_CONTRACT_WASM, params_bytes};

/// The key to follow `room` under, and the earlier key it was stored under when that is one of
/// its owner's earlier keys (`None` when the stored key is kept).
pub fn current_key(room: &StoredRoom) -> (ContractKey, Option<ContractKey>) {
    let stored = room.contract_key;
    let current = create::contract_key(&room.owner_vk);
    // The current key is never an earlier one, even if a registry row named it by mistake.
    if stored.id() == current.id() {
        return (stored, None);
    }
    if legacy_contract_keys_for_owner(&room.owner_vk)
        .iter()
        .any(|k| k.id() == stored.id())
    {
        (current, Some(stored))
    } else {
        (stored, None)
    }
}

/// The earlier keys to read for a room stored under `stored`, newest first, down to it (all of
/// them when it is not one of them).
pub fn candidates_down_to(
    owner: &VerifyingKey,
    stored: Option<&ContractKey>,
) -> Vec<ContractInstanceId> {
    let all = legacy_contract_keys_for_owner(owner);
    let current = *create::contract_key(owner).id();
    let mut out = Vec::new();
    for k in all.iter().filter(|k| *k.id() != current) {
        out.push(*k.id());
        if stored.is_some_and(|s| s.id() == k.id()) {
            break;
        }
    }
    out
}

/// River's state semantics for the probe, as the web UI's `RiverProbeOps`.
struct RiverOps {
    owner: VerifyingKey,
}

impl ProbeStateOps for RiverOps {
    type State = ChatRoomStateV1;

    fn decode(&self, bytes: &[u8]) -> Option<ChatRoomStateV1> {
        // A very old generation may hold a layout this build can't read: a miss, not a panic.
        ciborium::de::from_reader(bytes).ok()
    }

    fn is_real(&self, state: &ChatRoomStateV1) -> bool {
        state.configuration.verify_signature(&self.owner).is_ok()
    }

    fn merge_with_local(
        &self,
        recovered: ChatRoomStateV1,
        local: &ChatRoomStateV1,
    ) -> ChatRoomStateV1 {
        // A join or import holds no copy (a default state, which no owner signed): nothing to
        // keep. Otherwise what the user's copy holds is merged in, so nothing they had is lost;
        // on a failed merge the recovered state alone goes forward, as in the web UI.
        if !self.is_real(local) {
            return recovered;
        }
        let params = ChatRoomParametersV1 { owner: self.owner };
        let mut merged = recovered.clone();
        match merged.merge_from(&params, local) {
            Ok(()) => merged,
            Err(e) => {
                eprintln!(
                    "River: the copy of a room could not be merged into its earlier state: {e}"
                );
                recovered
            }
        }
    }

    fn merge_generations(&self, newer: ChatRoomStateV1, older: ChatRoomStateV1) -> ChatRoomStateV1 {
        // The web UI's `merge_converging`: merge the older generation in; if that refuses, merge
        // each side into the other once, which converges where one side only ranks lower; if
        // that fails too, keep the newer (the older key stays unresolved for a later run only
        // when it did not answer; a fold that fails is logged).
        let params = ChatRoomParametersV1 { owner: self.owner };
        let mut merged = newer.clone();
        if merged.merge_from(&params, &older).is_ok() {
            return merged;
        }
        let mut other = older.clone();
        let mut merged = newer.clone();
        if other.merge_from(&params, &newer).is_ok() && merged.merge_from(&params, &other).is_ok() {
            return merged;
        }
        eprintln!("River: folding an earlier key of a room failed; keeping the newer state");
        newer
    }
}

/// How a probe chooses among the earlier keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// Fold every earlier key down to the one the room was stored under into the user's copy:
    /// a stored room moved to its current key (the web UI's `start_rekey_merge`), so messages
    /// that reached any of those keys are carried.
    Fold,
    /// The newest key that holds a real room: a join or an ID import, which hold no copy (the
    /// web UI's backward probe).
    Newest,
}

/// Reading a room's earlier keys, newest first.
pub struct Probe {
    owner: VerifyingKey,
    driver: ProbeDriver<RiverOps>,
}

/// What a probe came to.
pub enum Found {
    /// A real state from an earlier key, merged with the user's copy: to PUT under the current
    /// key. `unresolved` are earlier keys that did not answer, still to fold in later.
    State(Box<ChatRoomStateV1>, Vec<ContractInstanceId>),
    /// Every earlier key answered, and none holds the room: the user's copy is all there is
    /// (seeded under the current key, as the web UI seeds it).
    Local(Box<ChatRoomStateV1>),
    /// Nothing found, and some earlier keys did not answer: whether there is anything is not
    /// known yet.
    Nothing,
}

impl Probe {
    /// A probe of `candidates` (newest first) for `owner`'s room, with the user's copy `local`
    /// (a default state when there is none).
    pub fn new(
        owner: VerifyingKey,
        candidates: Vec<ContractInstanceId>,
        local: ChatRoomStateV1,
        policy: Policy,
    ) -> Probe {
        let selection = match policy {
            Policy::Fold => SelectionPolicy::FoldAll(
                FoldAllAck::i_understand_fold_all_resurrects_without_tombstones(),
            ),
            Policy::Newest => SelectionPolicy::NewestFirstWins,
        };
        let driver = ProbeDriver::new(
            RiverOps { owner },
            local,
            NewestFirst::assume_ordered(candidates),
            selection,
        )
        // As the web UI's backward probe: a key that never answers is passed over, so one
        // unreachable generation does not hide an older one that holds the room. (FoldAll
        // already goes on; the keys passed over come back as `unresolved`.)
        .continue_past_unknown(
            RollbackRiskAck::i_understand_continuing_past_silence_can_roll_back(),
        );
        Probe { owner, driver }
    }

    /// The key to read next, or what was found.
    pub fn next_read(&mut self) -> Result<ContractInstanceId, Found> {
        match self.driver.next_action() {
            Step::Get(id) => Ok(id),
            _ => Err(match self.driver.take_outcome() {
                Some(Outcome::Recovered {
                    merged, unresolved, ..
                }) => Found::State(Box::new(merged), unresolved),
                Some(Outcome::SeedLocal { local }) | Some(Outcome::NoLegacy { local }) => {
                    Found::Local(Box::new(local))
                }
                _ => Found::Nothing,
            }),
        }
    }

    /// The key `id` answered with `bytes`.
    pub fn state(&mut self, id: ContractInstanceId, bytes: &[u8]) {
        self.driver.on_response(id, bytes);
    }

    /// The key `id` has no state.
    pub fn absent(&mut self, id: ContractInstanceId) {
        self.driver.on_absent(id);
    }

    /// The key `id` did not answer, or failed.
    pub fn unknown(&mut self, id: ContractInstanceId) {
        self.driver.on_unknown(id);
    }

    pub fn owner(&self) -> VerifyingKey {
        self.owner
    }
}

/// The PUT that carries `state` to `owner`'s room under the current key: its upgrade pointer
/// stripped (an earlier key's pointer must never be planted on the current one, freenet/river#427)
/// and normalized, as the room contract accepts only states its cleanup leaves unchanged.
pub fn put_forward(
    owner: &VerifyingKey,
    state: &ChatRoomStateV1,
) -> Result<ContractRequest<'static>, String> {
    let mut stripped = state.clone();
    stripped.upgrade = OptionalUpgradeV1(None);
    let normalized = stripped
        .normalized(&ChatRoomParametersV1 { owner: *owner })
        .map_err(|e| format!("room state cleanup failed: {e}"))?;
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&normalized, &mut bytes).map_err(|e| e.to_string())?;
    let container = ContractContainer::from(ContractWasmAPIVersion::V1(WrappedContract::new(
        Arc::new(ContractCode::from(ROOM_CONTRACT_WASM)),
        Parameters::from(params_bytes(owner)),
    )));
    Ok(ContractRequest::Put {
        contract: container,
        state: WrappedState::new(bytes),
        related_contracts: Default::default(),
        subscribe: false,
        blocking_subscribe: false,
    })
}

#[cfg(test)]
#[path = "rekey_tests.rs"]
mod tests;
