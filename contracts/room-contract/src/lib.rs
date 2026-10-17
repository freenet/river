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
                    // The room's own whole-state merge (the trait default is
                    // not commutative for rooms, freenet/river#703), called
                    // without the parent argument it ignores, so the state is
                    // not cloned to supply one.
                    chat_state
                        .merge_from(&parameters, &new_state)
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
                // `UpdateData` became `#[non_exhaustive]` in freenet-stdlib
                // 0.6.0. Unknown variants (including other Related* shapes
                // we don't handle) are rejected rather than panicking with
                // `unreachable!()` — a panic inside contract WASM would
                // kill the runtime for this room and surface as a generic
                // execution error. Returning `InvalidUpdate` is
                // recoverable and gives a clearer diagnostic.
                _ => {
                    return Err(ContractError::InvalidUpdate);
                }
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
        // Message authors' member records are added first, then the whole
        // delta is trimmed to the receiver's work bounds, so a receiver on a
        // lowered cap applies it rather than refusing it (freenet/river#703).
        let delta = chat_state
            .delta(&chat_state, &parameters, &summary)
            .map(|d| ensure_members_for_message_authors(d, &chat_state, &parameters, &summary))
            .map(|mut d| {
                chat_state.trim_network_delta(&parameters, &summary, &mut d);
                d
            });
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

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use river_core::room_state::ban::{AuthorizedUserBan, BansV1, UserBan};
    use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};

    macro_rules! cbor {
        ($v:expr) => {{
            let mut out = Vec::new();
            into_writer($v, &mut out).unwrap();
            out
        }};
    }

    /// The network delta (`get_state_delta`) for a receiver whose summary
    /// shows a newer configuration (a lowered `max_user_bans`) is trimmed to
    /// the work bound one delta may carry, so the receiver applies it rather
    /// than refusing it; the rest follows in later rounds (freenet/river#703).
    #[test]
    fn a_network_delta_is_trimmed_to_the_work_bound() {
        let owner_sk = SigningKey::from_bytes(&[1; 32]);
        let owner_id = MemberId::from(&owner_sk.verifying_key());
        let parameters = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };
        let config = |version: u32, max_user_bans: usize| {
            AuthorizedConfigurationV1::new(
                Configuration {
                    owner_member_id: owner_id,
                    configuration_version: version,
                    max_user_bans,
                    ..Default::default()
                },
                &owner_sk,
            )
        };
        let held = river_core::room_state::ban::MIN_BANS_DELTA_BOUND + 30;
        let bans = (0..held as u64)
            .map(|i| {
                let target = SigningKey::from_bytes(&{
                    let mut b = [9u8; 32];
                    b[..8].copy_from_slice(&i.to_le_bytes());
                    b
                });
                AuthorizedUserBan::new(
                    UserBan {
                        owner_member_id: owner_id,
                        banned_at: std::time::SystemTime::UNIX_EPOCH
                            + std::time::Duration::from_secs(i),
                        banned_user: MemberId::from(&target.verifying_key()),
                    },
                    owner_id,
                    &owner_sk,
                )
            })
            .collect();
        let mut sender = ChatRoomStateV1 {
            configuration: config(1, held),
            bans: BansV1(bans),
            ..Default::default()
        };
        sender.post_apply_cleanup(&parameters).unwrap();
        let receiver = ChatRoomStateV1 {
            configuration: config(2, 2),
            ..Default::default()
        };
        let summary = Contract::summarize_state(
            Parameters::from(cbor!(&parameters)),
            State::from(cbor!(&receiver)),
        )
        .unwrap();
        let delta = Contract::get_state_delta(
            Parameters::from(cbor!(&parameters)),
            State::from(cbor!(&sender)),
            summary,
        )
        .unwrap();
        let out = Contract::update_state(
            Parameters::from(cbor!(&parameters)),
            State::from(cbor!(&receiver)),
            vec![UpdateData::Delta(delta)],
        )
        .expect("the receiver applies the network delta");
        assert!(out.new_state.is_some());
    }

    /// Message authors' member records are added to a network delta before
    /// it is trimmed, so the trimmed delta never carries more members than
    /// the receiver's bound, nor a message whose author it left out: a
    /// receiver on a newer configuration the sender does not know applies it
    /// (freenet/river#703).
    #[test]
    fn a_network_delta_with_message_authors_fits_the_bound() {
        use river_core::room_state::member::{AuthorizedMember, Member, MembersV1};
        use river_core::room_state::message::{
            AuthorizedMessageV1, MessageV1, MessagesV1, RoomMessageBody,
        };
        let owner_sk = SigningKey::from_bytes(&[1; 32]);
        let owner_id = MemberId::from(&owner_sk.verifying_key());
        let parameters = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };
        let config = |version: u32, cap: usize| {
            AuthorizedConfigurationV1::new(
                Configuration {
                    owner_member_id: owner_id,
                    configuration_version: version,
                    max_members: cap,
                    max_recent_messages: cap,
                    ..Default::default()
                },
                &owner_sk,
            )
        };
        let authors: Vec<SigningKey> = (0..1_200u64)
            .map(|i| {
                let mut b = [5u8; 32];
                b[..8].copy_from_slice(&i.to_le_bytes());
                SigningKey::from_bytes(&b)
            })
            .collect();
        let mut messages = MessagesV1 {
            messages: authors
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    AuthorizedMessageV1::new(
                        MessageV1 {
                            room_owner: owner_id,
                            author: MemberId::from(&a.verifying_key()),
                            time: std::time::SystemTime::UNIX_EPOCH
                                + std::time::Duration::from_secs(i as u64),
                            content: RoomMessageBody::public(format!("m{i}")),
                        },
                        a,
                    )
                })
                .collect(),
            ..Default::default()
        };
        messages.rebuild_actions_state();
        let sender = ChatRoomStateV1 {
            configuration: config(1, 5_000),
            members: MembersV1 {
                members: authors
                    .iter()
                    .map(|a| {
                        AuthorizedMember::new(
                            Member {
                                owner_member_id: owner_id,
                                invited_by: owner_id,
                                member_vk: a.verifying_key(),
                            },
                            &owner_sk,
                        )
                    })
                    .collect(),
            },
            recent_messages: messages,
            ..Default::default()
        };
        let receiver = ChatRoomStateV1 {
            configuration: config(2, 2),
            ..Default::default()
        };
        let summary = Contract::summarize_state(
            Parameters::from(cbor!(&parameters)),
            State::from(cbor!(&receiver)),
        )
        .unwrap();
        let delta = Contract::get_state_delta(
            Parameters::from(cbor!(&parameters)),
            State::from(cbor!(&sender)),
            summary,
        )
        .unwrap();
        Contract::update_state(
            Parameters::from(cbor!(&parameters)),
            State::from(cbor!(&receiver)),
            vec![UpdateData::Delta(delta)],
        )
        .expect("the receiver applies the network delta");
    }

    /// A full-state update runs the room's own merge (`merge_from`), not the
    /// trait default. The witness: after the owner lowers `max_user_bans`
    /// from above the bans work bound, merging a peer that still holds the
    /// old ban set applies every ban and keeps what the merged cap keeps,
    /// where the trait default is refused by the per-delta bound
    /// (freenet/river#703).
    #[test]
    fn a_full_state_update_runs_the_rooms_own_merge() {
        let owner_sk = SigningKey::from_bytes(&[1; 32]);
        let owner_id = MemberId::from(&owner_sk.verifying_key());
        let parameters = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };
        let config = |version: u32, max_user_bans: usize| {
            AuthorizedConfigurationV1::new(
                Configuration {
                    owner_member_id: owner_id,
                    configuration_version: version,
                    max_user_bans,
                    ..Default::default()
                },
                &owner_sk,
            )
        };
        let held = river_core::room_state::ban::MIN_BANS_DELTA_BOUND + 3;
        let bans = (0..held as u64)
            .map(|i| {
                let target = SigningKey::from_bytes(&{
                    let mut b = [9u8; 32];
                    b[..8].copy_from_slice(&i.to_le_bytes());
                    b
                });
                AuthorizedUserBan::new(
                    UserBan {
                        owner_member_id: owner_id,
                        banned_at: std::time::SystemTime::UNIX_EPOCH
                            + std::time::Duration::from_secs(i),
                        banned_user: MemberId::from(&target.verifying_key()),
                    },
                    owner_id,
                    &owner_sk,
                )
            })
            .collect();
        let mut stale = ChatRoomStateV1 {
            configuration: config(1, held),
            bans: BansV1(bans),
            ..Default::default()
        };
        stale.post_apply_cleanup(&parameters).unwrap();
        assert_eq!(stale.bans.0.len(), held, "premise: every ban is kept");
        let mut updated = stale.clone();
        updated
            .apply_delta(
                &stale,
                &parameters,
                &Some(ChatRoomStateV1Delta {
                    configuration: Some(config(2, 2)),
                    ..Default::default()
                }),
            )
            .unwrap();

        let mut by_trait = updated.clone();
        assert!(
            ComposableState::merge(&mut by_trait, &updated, &parameters, &stale).is_err(),
            "premise: the trait default fails this merge"
        );
        let mut expected = updated.clone();
        expected.merge_from(&parameters, &stale).unwrap();

        let out = Contract::update_state(
            Parameters::from(cbor!(&parameters)),
            State::from(cbor!(&updated)),
            vec![UpdateData::State(State::from(cbor!(&stale)))],
        )
        .expect("the room's merge trims to the work bound");
        assert_eq!(
            out.new_state.expect("a new state").as_ref(),
            cbor!(&expected).as_slice()
        );
    }
}
