use ciborium::{de::from_reader, ser::into_writer};
use freenet_stdlib::prelude::*;

use freenet_scaffold::ComposableState;
use freenet_stdlib::prelude::ContractError;
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta, ChatRoomStateV1Summary};
use river_core::ChatRoomStateV1;

// NOTE: Crypto helper modules intentionally not compiled by default.
// They are retained under examples/docs to avoid accidental inclusion.

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

        // `verify`, and then that cleanup leaves the state unchanged, so every
        // state a peer holds is canonical (`validate_whole_state` explains the
        // byte comparison and the shared signature memo). Every merge and
        // delta apply ends in cleanup, so a state a peer produced already is;
        // clients send `ChatRoomStateV1::normalized` in whole-state PUTs. Not
        // in `ComposableState::verify`, which also runs on partial states
        // while a delta's fields are applied.
        chat_state
            .validate_whole_state(&parameters, bytes)
            .map_err(|e| ContractError::InvalidUpdateWithInfo {
                reason: e.to_string(),
            })?;
        Ok(ValidateResult::Valid)
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
        // Trimmed to the receiver's work bounds, so a receiver on a lowered
        // cap applies it rather than refusing it (freenet/river#703). The
        // delta carries the record of every member the receiver's summary
        // lacks, message authors included, and the trim keeps the authors of
        // the messages it keeps (freenet/river#145).
        let delta = chat_state.delta_within_work_bounds(&parameters, &summary);
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
    use river_core::room_state::member::MemberId;

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
    /// trait default. The witness: an incoming configuration raises
    /// `max_recent_messages`; the room's merge keeps every message the raised
    /// cap keeps, where the trait default reads the receiver's old retention
    /// horizon and withholds the older ones (freenet/river#703).
    #[test]
    fn a_full_state_update_runs_the_rooms_own_merge() {
        use river_core::room_state::message::{
            AuthorizedMessageV1, MessageV1, MessagesV1, RoomMessageBody,
        };
        let owner_sk = SigningKey::from_bytes(&[1; 32]);
        let owner_id = MemberId::from(&owner_sk.verifying_key());
        let parameters = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };
        let config = |version: u32, max_recent_messages: usize| {
            AuthorizedConfigurationV1::new(
                Configuration {
                    owner_member_id: owner_id,
                    configuration_version: version,
                    max_recent_messages,
                    ..Default::default()
                },
                &owner_sk,
            )
        };
        let messages = |range: std::ops::Range<u64>| {
            let mut m = MessagesV1 {
                messages: range
                    .map(|i| {
                        AuthorizedMessageV1::new(
                            MessageV1 {
                                room_owner: owner_id,
                                author: owner_id,
                                time: std::time::SystemTime::UNIX_EPOCH
                                    + std::time::Duration::from_secs(1_000 + i),
                                content: RoomMessageBody::public(format!("m{i}")),
                            },
                            &owner_sk,
                        )
                    })
                    .collect(),
                ..Default::default()
            };
            m.rebuild_actions_state();
            m
        };
        let receiver = ChatRoomStateV1 {
            configuration: config(1, 3),
            recent_messages: messages(7..10),
            ..Default::default()
        };
        let incoming = ChatRoomStateV1 {
            configuration: config(2, 10),
            recent_messages: messages(0..10),
            ..Default::default()
        };

        let mut by_trait = receiver.clone();
        ComposableState::merge(&mut by_trait, &receiver, &parameters, &incoming).unwrap();
        let mut expected = receiver.clone();
        expected.merge_from(&parameters, &incoming).unwrap();
        assert_eq!(expected.recent_messages.messages.len(), 10);
        assert!(
            by_trait.recent_messages.messages.len() < 10,
            "premise: the trait default withholds the older messages"
        );

        let out = Contract::update_state(
            Parameters::from(cbor!(&parameters)),
            State::from(cbor!(&receiver)),
            vec![UpdateData::State(State::from(cbor!(&incoming)))],
        )
        .expect("the room's merge applies it");
        assert_eq!(
            out.new_state.expect("a new state").as_ref(),
            cbor!(&expected).as_slice()
        );
    }

    fn hex32(s: &str) -> [u8; 32] {
        let b: Vec<u8> = (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect();
        b.try_into().unwrap()
    }

    fn validate(parameters: &ChatRoomParametersV1, bytes: Vec<u8>) -> Result<(), String> {
        Contract::validate_state(
            Parameters::from(cbor!(parameters)),
            State::from(bytes),
            RelatedContracts::default(),
        )
        .map(|_| ())
        .map_err(|e| format!("{e:?}"))
    }

    /// Captured room states pass `validate_state`, cleanup-fixpoint rule
    /// included. Reads `RIVER_V1_STATES` (comma-separated CBOR state files)
    /// and `RIVER_V1_OWNER_HEX` (the room owner's verifying key, hex); skips
    /// when they are unset, so it runs by hand against fetched live states.
    #[test]
    fn captured_states_pass_validate() {
        let (Ok(paths), Ok(owner)) = (
            std::env::var("RIVER_V1_STATES"),
            std::env::var("RIVER_V1_OWNER_HEX"),
        ) else {
            eprintln!("skipped: RIVER_V1_STATES / RIVER_V1_OWNER_HEX unset");
            return;
        };
        let parameters = ChatRoomParametersV1 {
            owner: ed25519_dalek::VerifyingKey::from_bytes(&hex32(&owner)).unwrap(),
        };
        for path in paths.split(',') {
            let bytes = std::fs::read(path).unwrap();
            let state: ChatRoomStateV1 = from_reader(&bytes[..]).unwrap();
            let t = std::time::Instant::now();
            let r = validate(&parameters, bytes.clone());
            println!(
                "{path}: {} bytes, {} members, {} messages, validate {:?} in {:.1} ms",
                bytes.len(),
                state.members.members.len(),
                state.recent_messages.messages.len(),
                r,
                t.elapsed().as_secs_f64() * 1e3
            );
            assert!(r.is_ok(), "{path}: {r:?}");
        }
    }

    /// Two owner messages, which `verify` accepts in either order, while
    /// cleanup sorts them by `(time, id)`.
    fn room_with_two_messages() -> (ChatRoomParametersV1, ChatRoomStateV1) {
        use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
        let owner_sk = SigningKey::from_bytes(&[7; 32]);
        let owner_id = MemberId::from(&owner_sk.verifying_key());
        let parameters = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };
        let mut state = ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(
                Configuration {
                    owner_member_id: owner_id,
                    ..Default::default()
                },
                &owner_sk,
            ),
            ..Default::default()
        };
        for secs in [20, 10] {
            state
                .recent_messages
                .messages
                .push(AuthorizedMessageV1::new(
                    MessageV1 {
                        room_owner: owner_id,
                        author: owner_id,
                        content: RoomMessageBody::public(format!("at {secs}")),
                        time: std::time::SystemTime::UNIX_EPOCH
                            + std::time::Duration::from_secs(1_700_000_000 + secs),
                    },
                    &owner_sk,
                ));
        }
        (parameters, state)
    }

    /// A state `verify` accepts but cleanup would still change is refused,
    /// and its normalized form, as a client sends it, is accepted.
    #[test]
    fn a_state_cleanup_would_change_is_refused() {
        let (parameters, raw) = room_with_two_messages();
        raw.verify(&raw, &parameters)
            .expect("verify accepts the unsorted messages");
        let err = validate(&parameters, cbor!(&raw)).unwrap_err();
        assert!(err.contains("not normalized"), "{err}");
        let normalized = raw.normalized(&parameters).unwrap();
        assert_ne!(cbor!(&normalized), cbor!(&raw));
        validate(&parameters, cbor!(&normalized)).expect("the normalized state is accepted");
    }

    /// The comparison is on bytes: a canonical state whose `#[serde(skip)]`
    /// message-actions cache is empty (as every decoded state's is) passes,
    /// even though cleanup rebuilds that cache. Struct `==` would refuse it.
    #[test]
    fn the_fixpoint_rule_ignores_the_skipped_actions_cache() {
        use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
        let (parameters, raw) = room_with_two_messages();
        let mut state = raw.normalized(&parameters).unwrap();
        let owner_sk = SigningKey::from_bytes(&[7; 32]);
        let owner_id = MemberId::from(&owner_sk.verifying_key());
        let target = state.recent_messages.messages[0].id();
        state
            .recent_messages
            .messages
            .push(AuthorizedMessageV1::new(
                MessageV1 {
                    room_owner: owner_id,
                    author: owner_id,
                    content: RoomMessageBody::edit(target, "edited".to_string()),
                    time: std::time::SystemTime::UNIX_EPOCH
                        + std::time::Duration::from_secs(1_700_000_030),
                },
                &owner_sk,
            ));
        let state = state.normalized(&parameters).unwrap();
        let bytes = cbor!(&state);
        let decoded: ChatRoomStateV1 = from_reader(&bytes[..]).unwrap();
        let cleaned = decoded.normalized(&parameters).unwrap();
        assert_ne!(
            cleaned, decoded,
            "cleanup rebuilds the skipped cache, so the structs differ"
        );
        validate(&parameters, bytes).expect("the bytes are a fixpoint");
    }

    /// A non-canonical encoding of a canonical state is refused.
    #[test]
    fn a_non_canonical_encoding_is_refused() {
        let (parameters, raw) = room_with_two_messages();
        let canonical = cbor!(&raw.normalized(&parameters).unwrap());
        validate(&parameters, canonical.clone()).unwrap();
        // ciborium writes a map header as a single byte for short maps; the
        // same map with a one-byte-length header (0xb8 n) decodes to the same
        // state and is not the canonical encoding.
        assert!((0xa0..0xb8).contains(&canonical[0]));
        let mut longer = vec![0xb8, canonical[0] - 0xa0];
        longer.extend_from_slice(&canonical[1..]);
        let decoded: ChatRoomStateV1 = from_reader(&longer[..]).unwrap();
        assert_eq!(cbor!(&decoded), canonical);
        assert!(validate(&parameters, longer).is_err());
    }
}
