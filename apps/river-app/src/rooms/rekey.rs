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
//! - **Carrying the state.** When the current key has no state on the network, the earlier keys
//!   are read newest first and the newest that holds a real room (its configuration verifies
//!   against the owner) is PUT under the current key, merged with the copy the user holds, its
//!   upgrade pointer stripped and normalized as the web UI's `put_state_to_current_key` sends it.
//!   freenet-migrate's [`ProbeDriver`] orders the reads and decides, as it does for the web UI.
//!   For a stored room the reads go down to the key it was stored under (the web UI's
//!   `rekey_candidates`); for a join or an ID import, which hold no key, down to the oldest.
//! - **What is left to the web UI.** Folding the OTHER earlier keys' states into a current key
//!   that already has state (the web UI's owed re-key merge, `rekey_merge`), and seeding the
//!   user's own copy when no key has any state.

use std::sync::Arc;

use ed25519_dalek::VerifyingKey;
use freenet_migrate::{
    NewestFirst, Outcome, ProbeDriver, ProbeStateOps, RollbackRiskAck, SelectionPolicy, Step,
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
    if legacy_contract_keys_for_owner(&room.owner_vk)
        .iter()
        .any(|k| k.id() == stored.id())
    {
        (create::contract_key(&room.owner_vk), Some(stored))
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
    let mut out = Vec::new();
    for k in &all {
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
}

/// Reading a room's earlier keys, newest first, for the newest real state.
pub struct Probe {
    owner: VerifyingKey,
    driver: ProbeDriver<RiverOps>,
}

/// What a probe came to.
pub enum Found {
    /// A real state, merged with the user's copy: to PUT under the current key.
    State(Box<ChatRoomStateV1>),
    /// No earlier key holds a real room (or none answered).
    Nothing,
}

impl Probe {
    /// A probe of `candidates` (newest first) for `owner`'s room, with the user's copy `local`
    /// (a default state when there is none).
    pub fn new(
        owner: VerifyingKey,
        candidates: Vec<ContractInstanceId>,
        local: ChatRoomStateV1,
    ) -> Probe {
        let driver = ProbeDriver::new(
            RiverOps { owner },
            local,
            NewestFirst::assume_ordered(candidates),
            SelectionPolicy::NewestFirstWins,
        )
        // As the web UI's backward probe: a key that never answers is passed over, so one
        // unreachable generation does not hide an older one that holds the room.
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
                Some(Outcome::Recovered { merged, .. }) => Found::State(Box::new(merged)),
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
