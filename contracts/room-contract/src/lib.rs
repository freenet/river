use ciborium::{de::from_reader, ser::into_writer};
use freenet_stdlib::prelude::*;

use freenet_scaffold::ComposableState;
use freenet_stdlib::prelude::ContractError;
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta, ChatRoomStateV1Summary};
use river_core::ChatRoomStateV1;

// NOTE: Crypto helper modules intentionally not compiled by default.
// They are retained under examples/docs to avoid accidental inclusion.

/// A stored or PUT state must be one `post_apply_cleanup` leaves as it is.
/// Every merge and delta apply ends in that cleanup, so an honest state
/// already is one; a state that is not would be changed by the next merge on
/// some peers and not others. Compared as bytes: the cleaned state must
/// re-encode to exactly the bytes given, which also refuses a non-canonical
/// encoding (and ignores `#[serde(skip)]` caches, which cleanup rebuilds).
/// Checked here rather than in `ComposableState::verify`, which also runs on
/// partial states while a delta's fields are applied.
pub fn require_cleanup_fixpoint(
    bytes: &[u8],
    state: &ChatRoomStateV1,
    parameters: &ChatRoomParametersV1,
) -> Result<(), String> {
    let mut cleaned = state.clone();
    cleaned
        .post_apply_cleanup(parameters)
        .map_err(|e| format!("State cleanup failed: {e}"))?;
    let mut out = Vec::with_capacity(bytes.len());
    into_writer(&cleaned, &mut out).map_err(|e| e.to_string())?;
    if out != bytes {
        return Err("State is not normalized: cleanup would change it".to_string());
    }
    Ok(())
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

        chat_state.verify(&chat_state, &parameters).map_err(|e| {
            ContractError::InvalidUpdateWithInfo {
                reason: format!("State verification failed: {}", e),
            }
        })?;
        require_cleanup_fixpoint(bytes, &chat_state, &parameters)
            .map_err(|reason| ContractError::InvalidUpdateWithInfo { reason })?;
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

    /// Spike: captured live v1 states (RIVER_V1_STATES, comma separated;
    /// owner key RIVER_V1_OWNER_HEX) pass `verify` and the cleanup-fixpoint
    /// rule, through the contract's own `validate_state`.
    #[test]
    #[ignore = "reads RIVER_V1_STATES; run by hand"]
    fn live_states_pass_the_fixpoint_rule() {
        let owner = ed25519_dalek::VerifyingKey::from_bytes(&hex32(
            &std::env::var("RIVER_V1_OWNER_HEX").unwrap(),
        ))
        .unwrap();
        let parameters = ChatRoomParametersV1 { owner };
        for path in std::env::var("RIVER_V1_STATES").unwrap().split(',') {
            let bytes = std::fs::read(path).unwrap();
            let s: ChatRoomStateV1 = from_reader(&bytes[..]).unwrap();
            let verify = s.verify(&s, &parameters);
            let mut cleaned = s.clone();
            let t = std::time::Instant::now();
            cleaned.post_apply_cleanup(&parameters).unwrap();
            let cleanup_ms = t.elapsed().as_secs_f64() * 1e3;
            let mut diff = vec![];
            macro_rules! d {
                ($f:ident) => {
                    if cleaned.$f != s.$f {
                        diff.push(stringify!($f));
                    }
                };
            }
            d!(configuration);
            d!(bans);
            d!(members);
            d!(member_info);
            d!(secrets);
            d!(recent_messages);
            d!(upgrade);
            d!(ban_evidence);
            d!(direct_messages);
            let r = Contract::validate_state(
                Parameters::from(cbor!(&parameters)),
                State::from(bytes.clone()),
                RelatedContracts::default(),
            );
            let bytes_fixpoint = cbor!(&cleaned) == bytes;
            println!(
                "{path}: {} bytes members={} msgs={} verify={:?} bytes_fixpoint={bytes_fixpoint} struct_diff(incl. serde-skip caches)={diff:?} native_cleanup_ms={cleanup_ms:.1} validate={r:?}",
                bytes.len(),
                s.members.members.len(),
                s.recent_messages.messages.len(),
                verify.is_ok()
            );
            assert!(verify.is_ok());
            assert!(r.is_ok(), "{path} refused: {r:?}");
        }
    }

    /// Spike: the raw fuzz pairs that do not converge under gossip
    /// (RIVER_RAW_DIR/<pair>/{a,b,params}.cbor) are refused by
    /// `validate_state`, a merge of them is accepted, and their normalized
    /// forms converge in a few exchanges.
    #[test]
    #[ignore = "reads RIVER_RAW_DIR; run by hand"]
    fn raw_non_fixpoint_states_are_refused() {
        let dir = std::env::var("RIVER_RAW_DIR").unwrap();
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        let mut refused = 0;
        let mut total = 0;
        for n in names {
            let rd = |f: &str| std::fs::read(format!("{dir}/{n}/{f}.cbor")).unwrap();
            let pbytes = rd("params");
            let parameters: ChatRoomParametersV1 = from_reader(&pbytes[..]).unwrap();
            let validate = |b: &[u8]| {
                Contract::validate_state(
                    Parameters::from(pbytes.clone()),
                    State::from(b.to_vec()),
                    RelatedContracts::default(),
                )
            };
            let merge = |x: &[u8], y: &[u8]| -> Vec<u8> {
                Contract::update_state(
                    Parameters::from(pbytes.clone()),
                    State::from(x.to_vec()),
                    vec![UpdateData::State(State::from(y.to_vec()))],
                )
                .unwrap()
                .new_state
                .unwrap()
                .as_ref()
                .to_vec()
            };
            let (a, b) = (rd("a"), rd("b"));
            let mut line = format!("{n}:");
            for (name, x) in [("a", &a), ("b", &b)] {
                total += 1;
                let st: ChatRoomStateV1 = from_reader(&x[..]).unwrap();
                let v = st.verify(&st, &parameters).is_ok();
                let r = validate(x);
                if r.is_err() {
                    refused += 1;
                }
                line += &format!(
                    " {name}: verify_ok={v} validate={}",
                    if r.is_ok() { "Valid" } else { "REFUSED" }
                );
            }
            // A merge output is always accepted.
            let ab = merge(&a, &b);
            let ba = merge(&b, &a);
            line += &format!(
                " | merge(a,b) valid={} merge(b,a) valid={}",
                validate(&ab).is_ok(),
                validate(&ba).is_ok()
            );
            // Raw gossip (what two peers holding the raw states would do).
            let gossip = |mut x: Vec<u8>, mut y: Vec<u8>| -> Option<usize> {
                for round in 1..=8 {
                    let nx = merge(&x, &y);
                    let ny = merge(&y, &nx);
                    x = nx;
                    y = ny;
                    if x == y {
                        return Some(round);
                    }
                }
                None
            };
            let norm = |x: &[u8]| -> Vec<u8> {
                let mut s: ChatRoomStateV1 = from_reader(x).unwrap();
                s.post_apply_cleanup(&parameters).unwrap();
                cbor!(&s)
            };
            let (an, bn) = (norm(&a), norm(&b));
            line += &format!(
                " | raw gossip converges at round {:?}; normalized: a'/b' valid={}/{} gossip converges at round {:?}",
                gossip(a.clone(), b.clone()),
                validate(&an).is_ok(),
                validate(&bn).is_ok(),
                gossip(an.clone(), bn.clone())
            );
            println!("{line}");
            assert!(validate(&ab).is_ok() && validate(&ba).is_ok());
        }
        println!("refused {refused} of {total} raw states");
    }
}
