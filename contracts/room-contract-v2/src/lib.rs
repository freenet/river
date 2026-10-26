//! The v2 room contract. State, merge and validation are
//! `river_core::room_state_v2`; this crate only maps them onto the contract
//! interface. Parameters are the v1 room parameters (the owner's key), so a
//! room's v2 key derives from its owner as the v1 key does.
//!
//! - `validate_state`: the empty state (a contract not yet seeded), or a
//!   state `room_state_v2::validate_state` accepts.
//! - `update_state`: every update is `f` of the held state and the incoming
//!   records ([`room_state_v2::apply_delta`]); an incoming record whose
//!   signature fails refuses the update.
//! - `summarize_state`: the exact set of record digests per kind.
//! - `get_state_delta`: every record the summary lacks, or no bytes.

use freenet_stdlib::prelude::*;
use river_core::room_state::ChatRoomParametersV1;
use river_core::room_state_v2::encoding::{from_cbor_canonical, to_cbor, Key};
use river_core::room_state_v2::records::Record;
use river_core::room_state_v2::state::{Invalid, Summary};
use river_core::room_state_v2::{apply_delta, state as v2state, RoomStateV2, Verifier};

fn owner(parameters: &Parameters) -> Result<Key, ContractError> {
    let p: ChatRoomParametersV1 = ciborium::de::from_reader(parameters.as_ref())
        .map_err(|e| ContractError::Deser(e.to_string()))?;
    Ok(Key::from(&p.owner))
}

fn refused(e: Invalid) -> ContractError {
    ContractError::InvalidUpdateWithInfo {
        reason: format!("{e:?}"),
    }
}

/// The held state, or an empty one for a contract not yet seeded.
fn held(state: &State) -> Result<RoomStateV2, ContractError> {
    if state.as_ref().is_empty() {
        return Ok(RoomStateV2::from_records(vec![]));
    }
    from_cbor_canonical(state.as_ref())
        .ok_or_else(|| ContractError::Deser("state is not a canonical v2 room state".into()))
}

#[allow(dead_code)]
struct Contract;

#[contract]
impl ContractInterface for Contract {
    fn validate_state(
        parameters: Parameters<'static>,
        state: State<'static>,
        _related: RelatedContracts<'static>,
    ) -> Result<ValidateResult, ContractError> {
        if state.as_ref().is_empty() {
            return Ok(ValidateResult::Valid);
        }
        let owner = owner(&parameters)?;
        Ok(
            match v2state::validate_state(&owner, state.as_ref(), &mut Verifier::new()) {
                Ok(_) => ValidateResult::Valid,
                Err(_) => ValidateResult::Invalid,
            },
        )
    }

    fn update_state(
        parameters: Parameters<'static>,
        state: State<'static>,
        data: Vec<UpdateData<'static>>,
    ) -> Result<UpdateModification<'static>, ContractError> {
        let owner = owner(&parameters)?;
        let mut s = held(&state)?;
        let mut v = Verifier::new();
        for update in data {
            let records: Vec<Record> = match update {
                UpdateData::State(st) => {
                    let incoming: RoomStateV2 =
                        from_cbor_canonical(st.as_ref()).ok_or_else(|| {
                            ContractError::Deser("update is not a canonical v2 room state".into())
                        })?;
                    incoming.records
                }
                UpdateData::Delta(d) => {
                    if d.as_ref().is_empty() {
                        continue;
                    }
                    from_cbor_canonical(d.as_ref()).ok_or_else(|| {
                        ContractError::Deser("delta is not a canonical record list".into())
                    })?
                }
                UpdateData::RelatedState { .. } => continue,
                _ => return Err(ContractError::InvalidUpdate),
            };
            s = apply_delta(&owner, &s, records, &mut v).map_err(refused)?;
        }
        Ok(UpdateModification::valid(to_cbor(&s).into()))
    }

    fn summarize_state(
        _parameters: Parameters<'static>,
        state: State<'static>,
    ) -> Result<StateSummary<'static>, ContractError> {
        if state.as_ref().is_empty() {
            return Ok(StateSummary::from(vec![]));
        }
        let s = held(&state)?;
        Ok(StateSummary::from(to_cbor(&s.summarize())))
    }

    fn get_state_delta(
        _parameters: Parameters<'static>,
        state: State<'static>,
        summary: StateSummary<'static>,
    ) -> Result<StateDelta<'static>, ContractError> {
        let s = held(&state)?;
        let summary: Summary = if summary.as_ref().is_empty() {
            Summary::new()
        } else {
            from_cbor_canonical(summary.as_ref())
                .ok_or_else(|| ContractError::Deser("summary is not canonical".into()))?
        };
        Ok(match s.delta(&summary) {
            Some(d) => StateDelta::from(to_cbor(&d)),
            None => StateDelta::from(vec![]),
        })
    }
}
