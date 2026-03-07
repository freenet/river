use std::collections::HashSet;

use ciborium::{de::from_reader, ser::into_writer};
use freenet_stdlib::prelude::*;

use freenet_scaffold::ComposableState;
use freenet_stdlib::prelude::ContractError;
use river_core::room_state::member::{MemberId, MembersDelta};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta, ChatRoomStateV1Summary};
use river_core::ChatRoomStateV1;

// NOTE: Crypto helper modules intentionally not compiled by default.
// They are retained under examples/docs to avoid accidental inclusion.

/// Ensure deltas are self-contained: if a message author is not in the peer's
/// summary AND not already in the members delta, inject their member entry
/// (and invite chain) into the delta. Without this, a peer that pruned an
/// inactive member will silently reject new messages from that member because
/// the message arrives without the member entry. See freenet/river#145.
fn ensure_members_for_message_authors(
    mut delta: ChatRoomStateV1Delta,
    chat_state: &ChatRoomStateV1,
    parameters: &ChatRoomParametersV1,
    summary: &ChatRoomStateV1Summary,
) -> ChatRoomStateV1Delta {
    let msg_delta = match delta.recent_messages {
        Some(ref msgs) => msgs,
        None => return delta,
    };

    let owner_id = MemberId::from(&parameters.owner);

    let msg_authors: HashSet<MemberId> = msg_delta.iter().map(|m| m.message.author).collect();

    let already_in_delta: HashSet<MemberId> = delta
        .members
        .as_ref()
        .map(|m| m.added().iter().map(|a| a.member.id()).collect())
        .unwrap_or_default();

    // Find message authors missing from both the peer's summary and the delta
    let missing: Vec<MemberId> = msg_authors
        .iter()
        .filter(|id| {
            **id != owner_id && !summary.members.contains(id) && !already_in_delta.contains(id)
        })
        .cloned()
        .collect();

    if missing.is_empty() {
        return delta;
    }

    // Walk invite chains upward to include all ancestors (stop at owner or
    // members already known to the peer)
    let members_by_id = chat_state.members.members_by_member_id();
    let mut to_add: HashSet<MemberId> = HashSet::new();
    let mut queue: Vec<MemberId> = missing;
    while let Some(mid) = queue.pop() {
        if to_add.contains(&mid) {
            continue;
        }
        if members_by_id.contains_key(&mid) {
            to_add.insert(mid);
            if let Some(m) = members_by_id.get(&mid) {
                let inviter = m.member.invited_by;
                if inviter != owner_id
                    && !summary.members.contains(&inviter)
                    && !already_in_delta.contains(&inviter)
                    && !to_add.contains(&inviter)
                {
                    queue.push(inviter);
                }
            }
        }
    }

    if to_add.is_empty() {
        return delta;
    }

    // Merge with any existing members delta
    let mut added = delta
        .members
        .take()
        .map(|m| m.into_added())
        .unwrap_or_default();
    for mid in &to_add {
        if let Some(m) = members_by_id.get(mid) {
            added.push((*m).clone());
        }
    }
    delta.members = Some(MembersDelta::new(added));

    delta
}

#[allow(dead_code)]
struct Contract;

#[contract]
impl ContractInterface for Contract {
    fn validate_state(
        parameters: Parameters<'static>,
        state: State<'static>,
        _related: RelatedContracts<'static>,
    ) -> Result<ValidateResult, freenet_stdlib::prelude::ContractError> {
        let bytes = state.as_ref();
        // allow empty room_state
        if bytes.is_empty() {
            return Ok(ValidateResult::Valid);
        }
        let chat_state = from_reader::<ChatRoomStateV1, &[u8]>(bytes)
            .map_err(|e| ContractError::Deser(e.to_string()))?;

        let parameters = from_reader::<ChatRoomParametersV1, &[u8]>(parameters.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;

        chat_state
            .verify(&chat_state, &parameters)
            .map(|_| ValidateResult::Valid)
            .map_err(|e| ContractError::InvalidUpdateWithInfo {
                reason: format!("State verification failed: {}", e),
            })
    }

    fn update_state(
        parameters: Parameters<'static>,
        state: State<'static>,
        data: Vec<UpdateData<'static>>,
    ) -> Result<UpdateModification<'static>, freenet_stdlib::prelude::ContractError> {
        let parameters = from_reader::<ChatRoomParametersV1, &[u8]>(parameters.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;
        let mut chat_state = from_reader::<ChatRoomStateV1, &[u8]>(state.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;

        for update in data {
            match update {
                UpdateData::State(new_state) => {
                    let new_state = from_reader::<ChatRoomStateV1, &[u8]>(new_state.as_ref())
                        .map_err(|e| ContractError::Deser(e.to_string()))?;
                    chat_state
                        .merge(&chat_state.clone(), &parameters, &new_state)
                        .map_err(|e| ContractError::InvalidUpdateWithInfo {
                            reason: e.to_string(),
                        })?;
                }
                UpdateData::Delta(d) => {
                    if d.as_ref().is_empty() {
                        continue;
                    }
                    let delta = from_reader::<ChatRoomStateV1Delta, &[u8]>(d.as_ref())
                        .map_err(|e| ContractError::Deser(e.to_string()))?;
                    chat_state
                        .apply_delta(&chat_state.clone(), &parameters, &Some(delta))
                        .map_err(|e| ContractError::InvalidUpdateWithInfo {
                            reason: e.to_string(),
                        })?;
                }
                UpdateData::RelatedState {
                    related_to: _,
                    state: _,
                } => {
                    // TODO: related room_state handling not needed for river
                }
                _ => unreachable!(),
            }
        }

        let mut updated_state = vec![];
        into_writer(&chat_state, &mut updated_state)
            .map_err(|e| ContractError::Deser(e.to_string()))?;

        Ok(UpdateModification::valid(updated_state.into()))
    }

    fn summarize_state(
        parameters: Parameters<'static>,
        state: State<'static>,
    ) -> Result<StateSummary<'static>, freenet_stdlib::prelude::ContractError> {
        let state = state.as_ref();
        if state.is_empty() {
            return Ok(StateSummary::from(vec![]));
        }
        let parameters = from_reader::<ChatRoomParametersV1, &[u8]>(parameters.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;
        let state = from_reader::<ChatRoomStateV1, &[u8]>(state)
            .map_err(|e| ContractError::Deser(e.to_string()))?;
        let summary = state.summarize(&state, &parameters);
        let mut summary_bytes = vec![];
        into_writer(&summary, &mut summary_bytes)
            .map_err(|e| ContractError::Deser(e.to_string()))?;
        Ok(StateSummary::from(summary_bytes))
    }

    fn get_state_delta(
        parameters: Parameters<'static>,
        state: State<'static>,
        summary: StateSummary<'static>,
    ) -> Result<StateDelta<'static>, freenet_stdlib::prelude::ContractError> {
        let chat_state = from_reader::<ChatRoomStateV1, &[u8]>(state.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;
        let parameters = from_reader::<ChatRoomParametersV1, &[u8]>(parameters.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;
        let summary = from_reader::<ChatRoomStateV1Summary, &[u8]>(summary.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;
        let delta = chat_state.delta(&chat_state, &parameters, &summary);
        let delta = delta
            .map(|d| ensure_members_for_message_authors(d, &chat_state, &parameters, &summary));
        match delta {
            Some(d) => {
                let mut delta_bytes = vec![];
                into_writer(&d, &mut delta_bytes)
                    .map_err(|e| ContractError::Deser(e.to_string()))?;
                Ok(StateDelta::from(delta_bytes))
            }
            None => Ok(StateDelta::from(vec![])),
        }
    }
}
