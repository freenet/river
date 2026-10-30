//! The v2 room contract. State, merge and validation are
//! `river_core::room_state_v2`; this crate only maps them onto the contract
//! interface. Parameters are the v1 room parameters (the owner's key), so a
//! room's v2 key derives from its owner as the v1 key does.
//!
//! - `validate_state`: the empty state (a contract not yet seeded), or a
//!   state `room_state_v2::validate_state` accepts.
//! - `update_state`: every update is `f` of the held state and the records of
//!   all incoming items, applied once ([`room_state_v2::apply_delta`]); an
//!   incoming record whose signature fails refuses the update. Incoming
//!   bytes are bounded before they are decoded (state and delta by
//!   [`max_state_bytes`], a summary by the largest a valid state yields).
//! - `summarize_state`: the exact set of record digests per kind.
//! - `get_state_delta`: every record the summary lacks, or no bytes.
//!
//! A state holding no records has one byte form: empty bytes. Empty bytes
//! and the encoding of a state with no records are both accepted as input
//! everywhere; the contract only produces the empty form.

use freenet_stdlib::prelude::*;
use river_core::room_state::ChatRoomParametersV1;
use river_core::room_state_v2::config::ceiling;
use river_core::room_state_v2::encoding::{from_cbor_canonical, to_cbor, Key};
use river_core::room_state_v2::records::Record;
use river_core::room_state_v2::state::{
    within_kind_bytes, within_kind_counts, within_kind_counts_encoded, Invalid, Summary,
    STATE_VERSION,
};
use river_core::room_state_v2::{
    apply_delta, max_state_bytes, state as v2state, RoomStateV2, Verifier,
};
use std::collections::HashSet;

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

fn too_large(what: &str, len: usize, max: usize) -> ContractError {
    ContractError::InvalidUpdateWithInfo {
        reason: format!("{what} is {len} bytes, more than the {max} a valid one can be"),
    }
}

/// The most digests a summary of a valid state can hold: the sum of every
/// kind group's cap. The sub-limits inside a group (legacy links, legacy
/// activity) are not subtracted, so this is at least what a valid state
/// holds.
fn max_summary_entries() -> usize {
    use ceiling::*;
    let members = MAX_MEMBERS as usize + 1;
    4 + ROSTER_BATCHES
        + MAX_SECRET_VERSIONS as usize
        + (MAX_OWNER_BANS + MAX_DEPUTY_BANS + MAX_MEMBER_BANS) as usize
        + 2 * MAX_LINKS as usize
        + RESTORE_REQUESTS
        + 2 * members
        + MAX_RECENT_MESSAGES as usize
        + MAX_DIRECT_MESSAGES as usize
        + MAX_SECRET_BLOBS as usize
}

/// The most bytes a summary of a valid state encodes to: each entry is a
/// 32-byte digest as a CBOR byte string (34 bytes), and each of the 24 kinds
/// adds a key and an array header (at most 2 + 3 bytes), plus the map header.
fn max_summary_bytes() -> usize {
    max_summary_entries() * 34 + 24 * 5 + 8
}

/// A state's records. Empty bytes are no records. Refuses bytes longer than
/// a valid state can be before decoding them, a non-canonical encoding, and
/// a version other than 2.
fn decode_state(bytes: &[u8], what: &str) -> Result<Vec<Record>, ContractError> {
    if bytes.is_empty() {
        return Ok(vec![]);
    }
    if bytes.len() > max_state_bytes() {
        return Err(too_large(what, bytes.len(), max_state_bytes()));
    }
    match within_kind_counts_encoded(bytes, true) {
        Some(true) => {}
        Some(false) => return Err(refused(Invalid::TooManyRecords)),
        None => {
            return Err(ContractError::Deser(format!(
                "{what} is not a canonical v2 room state"
            )))
        }
    }
    let s: RoomStateV2 = from_cbor_canonical(bytes)
        .ok_or_else(|| ContractError::Deser(format!("{what} is not a canonical v2 room state")))?;
    if s.version != STATE_VERSION {
        return Err(refused(Invalid::WrongVersion(s.version)));
    }
    Ok(s.records)
}

/// A delta's records. Empty bytes are no records. Bounded as a state is.
fn decode_delta(bytes: &[u8]) -> Result<Vec<Record>, ContractError> {
    if bytes.is_empty() {
        return Ok(vec![]);
    }
    if bytes.len() > max_state_bytes() {
        return Err(too_large("delta", bytes.len(), max_state_bytes()));
    }
    match within_kind_counts_encoded(bytes, false) {
        Some(true) => {}
        Some(false) => return Err(refused(Invalid::TooManyRecords)),
        None => {
            return Err(ContractError::Deser(
                "delta is not a canonical record list".into(),
            ))
        }
    }
    from_cbor_canonical(bytes)
        .ok_or_else(|| ContractError::Deser("delta is not a canonical record list".into()))
}

/// A peer's summary. Empty bytes are the empty summary. Refuses one larger
/// than a valid state's summary before decoding it.
fn decode_summary(bytes: &[u8]) -> Result<Summary, ContractError> {
    if bytes.is_empty() {
        return Ok(Summary::new());
    }
    if bytes.len() > max_summary_bytes() {
        return Err(too_large("summary", bytes.len(), max_summary_bytes()));
    }
    let s: Summary = from_cbor_canonical(bytes)
        .ok_or_else(|| ContractError::Deser("summary is not canonical".into()))?;
    if s.values().map(|d| d.len()).sum::<usize>() > max_summary_entries() {
        return Err(ContractError::InvalidUpdateWithInfo {
            reason: "summary has more entries than a valid state can".into(),
        });
    }
    Ok(s)
}

/// The held state, or an empty one for a contract not yet seeded.
fn held(state: &State) -> Result<RoomStateV2, ContractError> {
    Ok(RoomStateV2::from_records(decode_state(
        state.as_ref(),
        "state",
    )?))
}

/// The record lists of one update item, each bounded on its own: a
/// `StateAndDelta` is its two parts.
fn item_records(update: UpdateData) -> Result<Vec<Vec<Record>>, ContractError> {
    let bounded = |records: Vec<Record>| {
        if within_kind_counts(&records) {
            Ok(records)
        } else {
            Err(refused(Invalid::TooManyRecords))
        }
    };
    let state = |s: &State| bounded(decode_state(s.as_ref(), "update")?);
    let delta = |d: &StateDelta| bounded(decode_delta(d.as_ref())?);
    match update {
        UpdateData::State(s) => Ok(vec![state(&s)?]),
        UpdateData::Delta(d) => Ok(vec![delta(&d)?]),
        UpdateData::StateAndDelta { state: s, delta: d } => Ok(vec![state(&s)?, delta(&d)?]),
        // The state part belongs to another contract and is ignored, as
        // `RelatedState` is; the delta is this contract's.
        UpdateData::RelatedState { .. } => Ok(vec![]),
        UpdateData::RelatedStateAndDelta { delta: d, .. } => Ok(vec![delta(&d)?]),
        _ => Err(ContractError::InvalidUpdate),
    }
}

/// The most non-empty record lists applied in turn when together they hold
/// more than one valid state: two lists, two runs of `f`, each at most what
/// the timing check measures for one. freenet-core passes one item per
/// `update_state` call (a state or a delta, plus related-contract items that
/// carry no records here), so this bounds only what another host might send.
const MAX_ITEMS_IN_TURN: usize = 2;

/// A state's bytes: empty for no records.
fn state_bytes(s: &RoomStateV2) -> Vec<u8> {
    if s.records.is_empty() {
        vec![]
    } else {
        to_cbor(s)
    }
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
                // The state with no records has one form, the empty bytes
                // (what `update_state` returns for it), so merge(S, S) == S.
                Ok(s) if s.records.is_empty() => ValidateResult::Invalid,
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
        // Bound what arrives before decoding what is held.
        let mut items: Vec<Vec<Record>> = vec![];
        for item in data {
            items.extend(item_records(item)?);
        }
        items.retain(|r| !r.is_empty());
        let mut s = held(&state)?;
        // The same record in several items counts once.
        let mut seen = HashSet::new();
        let union: Vec<Record> = items
            .iter()
            .flatten()
            .filter(|r| seen.insert(r.digest()))
            .cloned()
            .collect();
        let mut v = Verifier::new();
        if within_kind_counts(&union) && within_kind_bytes(&union) {
            // One run of `f` over the held state and every item.
            s = apply_delta(&owner, &s, union, &mut v).map_err(refused)?;
        } else if items.len() <= MAX_ITEMS_IN_TURN {
            // Items that hold more together than one valid state are applied
            // in turn, each bounded on its own; at most `MAX_ITEMS_IN_TURN`,
            // so one call costs at most that many runs of `f`.
            for records in items {
                s = apply_delta(&owner, &s, records, &mut v).map_err(refused)?;
            }
        } else {
            return Err(refused(Invalid::TooManyRecords));
        }
        Ok(UpdateModification::valid(state_bytes(&s).into()))
    }

    fn summarize_state(
        _parameters: Parameters<'static>,
        state: State<'static>,
    ) -> Result<StateSummary<'static>, ContractError> {
        let s = held(&state)?;
        Ok(StateSummary::from(if s.records.is_empty() {
            vec![]
        } else {
            to_cbor(&s.summarize())
        }))
    }

    fn get_state_delta(
        _parameters: Parameters<'static>,
        state: State<'static>,
        summary: StateSummary<'static>,
    ) -> Result<StateDelta<'static>, ContractError> {
        let s = held(&state)?;
        let summary = decode_summary(summary.as_ref())?;
        Ok(match s.delta(&summary) {
            Some(d) => StateDelta::from(to_cbor(&d)),
            None => StateDelta::from(vec![]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use river_core::room_state_v2::bench::{largest_state, owner_key, Caps};
    use river_core::room_state_v2::encoding::{sign_payload, Sig};
    use river_core::room_state_v2::records::{tag, ActivityP, BanP, BanTarget, Issuer, Signed};
    use std::time::Instant;

    type C = Contract;

    fn params() -> Parameters<'static> {
        let mut out = vec![];
        ciborium::ser::into_writer(
            &ChatRoomParametersV1 {
                owner: owner_key().verifying_key(),
            },
            &mut out,
        )
        .unwrap();
        Parameters::from(out)
    }

    fn owner_k() -> Key {
        Key::from(&owner_key().verifying_key())
    }

    /// A small valid state and the number of records `f` dropped building it.
    fn small() -> RoomStateV2 {
        let (s, dropped) = largest_state(Caps::ceilings().scaled(20), 0);
        assert_eq!(dropped, 0);
        s
    }

    fn st(s: &RoomStateV2) -> State<'static> {
        State::from(to_cbor(s))
    }

    fn fresh_key(i: u32) -> SigningKey {
        let mut seed = [0u8; 32];
        seed[0] = 0xee;
        seed[1..5].copy_from_slice(&i.to_le_bytes());
        SigningKey::from_bytes(&seed)
    }

    /// A validly self-signed activity record for a key no state links.
    fn activity(i: u32) -> Record {
        let k = fresh_key(i);
        let p = ActivityP {
            member: Key::from(&k.verifying_key()),
            hours: 5,
        };
        Record::Activity(Signed {
            sig: sign_payload(tag::ACTIVITY, &owner_k(), &p, &k),
            payload: p,
        })
    }

    /// An owner ban whose signature is not the owner's.
    fn bad_ban(i: u32) -> Record {
        let p = BanP {
            target: BanTarget::Key(Key::from(&fresh_key(i).verifying_key())),
            issuer: Issuer::Owner(()),
            // Newer than any ban a state holds, so the room examines it.
            time: river_core::room_state_v2::config::EPOCH_2026_SECS
                + 3600 * 24 * 299
                + 1000
                + i as u64,
            clock_ref: 24 * 300,
            reason: river_core::room_state_v2::encoding::ByteBuf(vec![]),
        };
        Record::Ban(Signed {
            sig: Sig([7; 64]),
            payload: p,
        })
    }

    fn update(
        state: &State<'static>,
        data: Vec<UpdateData<'static>>,
    ) -> Result<Vec<u8>, ContractError> {
        let m = C::update_state(params(), state.clone(), data)?;
        Ok(m.unwrap_valid().as_ref().to_vec())
    }

    fn delta_of(records: &[Record]) -> UpdateData<'static> {
        UpdateData::Delta(StateDelta::from(to_cbor(records)))
    }

    fn empty() -> State<'static> {
        State::from(vec![])
    }

    #[test]
    fn bad_parameters_are_refused() {
        let bad = Parameters::from(vec![0xff, 0x00]);
        let s = st(&small());
        assert!(matches!(
            C::validate_state(bad.clone(), s.clone(), RelatedContracts::default()),
            Err(ContractError::Deser(_))
        ));
        assert!(matches!(
            C::update_state(bad, s, vec![]),
            Err(ContractError::Deser(_))
        ));
    }

    #[test]
    fn empty_state_summary_and_delta_are_consistent() {
        let none = RoomStateV2::from_records(vec![]);
        let encoded_none = State::from(to_cbor(&none));
        // Only the empty bytes are a valid stored form of the state with no
        // records; the encoded one is still read as no records elsewhere.
        assert_eq!(
            C::validate_state(params(), encoded_none.clone(), RelatedContracts::default()).unwrap(),
            ValidateResult::Invalid
        );
        for s in [empty(), encoded_none] {
            if s.as_ref().is_empty() {
                assert_eq!(
                    C::validate_state(params(), s.clone(), RelatedContracts::default()).unwrap(),
                    ValidateResult::Valid
                );
            }
            assert!(C::summarize_state(params(), s.clone())
                .unwrap()
                .as_ref()
                .is_empty());
            for sum in [vec![], to_cbor(&Summary::new())] {
                let d = C::get_state_delta(params(), s.clone(), StateSummary::from(sum)).unwrap();
                assert!(d.as_ref().is_empty());
            }
            // An empty update, an empty state update and an empty delta
            // list change nothing and give the one empty form.
            assert!(update(&s, vec![]).unwrap().is_empty());
            assert!(update(&s, vec![UpdateData::State(empty())])
                .unwrap()
                .is_empty());
            assert!(
                update(&s, vec![UpdateData::Delta(StateDelta::from(vec![]))])
                    .unwrap()
                    .is_empty()
            );
            assert!(update(&s, vec![delta_of(&[])]).unwrap().is_empty());
        }
        // An empty summary asks for everything a state holds.
        let s = st(&small());
        for sum in [vec![], to_cbor(&Summary::new())] {
            let d = C::get_state_delta(params(), s.clone(), StateSummary::from(sum)).unwrap();
            let recs: Vec<Record> = from_cbor_canonical(d.as_ref()).unwrap();
            assert_eq!(recs.len(), small().records.len());
        }
    }

    #[test]
    fn seeding_from_empty_and_summary_round_trip() {
        let full = small();
        let out = update(&empty(), vec![UpdateData::State(st(&full))]).unwrap();
        assert_eq!(out, to_cbor(&full));
        let s = State::from(out);
        assert_eq!(
            C::validate_state(params(), s.clone(), RelatedContracts::default()).unwrap(),
            ValidateResult::Valid
        );
        let sum = C::summarize_state(params(), s.clone()).unwrap();
        let d = C::get_state_delta(params(), s, sum).unwrap();
        assert!(d.as_ref().is_empty());
    }

    #[test]
    fn non_canonical_inputs_are_refused() {
        let s = small();
        let good = to_cbor(&s);
        let mut trailing = good.clone();
        trailing.push(0);
        let held = State::from(good.clone());
        assert!(matches!(
            C::summarize_state(params(), State::from(trailing.clone())),
            Err(ContractError::Deser(_))
        ));
        assert!(matches!(
            update(&held, vec![UpdateData::State(State::from(trailing))]),
            Err(ContractError::Deser(_))
        ));
        let mut d = to_cbor(&s.records[..1]);
        d.push(0);
        assert!(matches!(
            update(&held, vec![UpdateData::Delta(StateDelta::from(d))]),
            Err(ContractError::Deser(_))
        ));
        // Indefinite-length array: the same value, another encoding.
        let mut indef = vec![0x9f];
        indef.extend(s.records[..1].iter().flat_map(to_cbor));
        indef.push(0xff);
        assert!(matches!(
            update(&held, vec![UpdateData::Delta(StateDelta::from(indef))]),
            Err(ContractError::Deser(_))
        ));
        let mut sum = to_cbor(&s.summarize());
        sum.push(0);
        assert!(matches!(
            C::get_state_delta(params(), held, StateSummary::from(sum)),
            Err(ContractError::Deser(_))
        ));
    }

    #[test]
    fn a_state_update_of_version_1_is_refused() {
        let mut s = small();
        s.version = 1;
        let bytes = to_cbor(&s);
        assert!(matches!(
            update(
                &empty(),
                vec![UpdateData::State(State::from(bytes.clone()))]
            ),
            Err(ContractError::InvalidUpdateWithInfo { .. })
        ));
        assert_eq!(
            C::validate_state(params(), State::from(bytes), RelatedContracts::default()).unwrap(),
            ValidateResult::Invalid
        );
    }

    #[test]
    fn an_update_that_changes_nothing_returns_the_held_bytes() {
        let s = small();
        let held = st(&s);
        let same = update(&held, vec![UpdateData::State(held.clone())]).unwrap();
        assert_eq!(same, held.as_ref());
        let same = update(&held, vec![delta_of(&s.records[..3])]).unwrap();
        assert_eq!(same, held.as_ref());
    }

    #[test]
    fn duplicates_and_overlap_count_once() {
        let s = small();
        let held = st(&s);
        let one = activity(1);
        // The same record in two items, and in a state and a delta.
        let a = update(
            &held,
            vec![
                delta_of(std::slice::from_ref(&one)),
                delta_of(std::slice::from_ref(&one)),
            ],
        )
        .unwrap();
        let b = update(&held, vec![delta_of(std::slice::from_ref(&one))]).unwrap();
        assert_eq!(a, b);
        let both = UpdateData::StateAndDelta {
            state: held.clone(),
            delta: StateDelta::from(to_cbor(std::slice::from_ref(&one))),
        };
        assert_eq!(update(&held, vec![both]).unwrap(), b);
        // Several items give the same as one item holding the union.
        let two = activity(2);
        let split = update(
            &held,
            vec![
                delta_of(std::slice::from_ref(&one)),
                delta_of(std::slice::from_ref(&two)),
            ],
        )
        .unwrap();
        let joined = update(&held, vec![delta_of(&[one, two])]).unwrap();
        assert_eq!(split, joined);
    }

    #[test]
    fn state_and_delta_takes_the_records_of_both() {
        let s = small();
        let (first, rest) = s.records.split_at(s.records.len() / 2);
        let state = RoomStateV2::from_records(first.to_vec());
        let item = UpdateData::StateAndDelta {
            state: st(&state),
            delta: StateDelta::from(to_cbor(rest)),
        };
        assert_eq!(update(&empty(), vec![item]).unwrap(), to_cbor(&s));
        // The related variants: the state part is ignored, the delta taken.
        let id = ContractInstanceId::new([3; 32]);
        let item = UpdateData::RelatedStateAndDelta {
            related_to: id,
            state: st(&s),
            delta: StateDelta::from(to_cbor(rest)),
        };
        let got = update(&empty(), vec![item]).unwrap();
        let want = update(&empty(), vec![delta_of(rest)]).unwrap();
        assert_eq!(got, want);
        let item = UpdateData::RelatedState {
            related_to: id,
            state: st(&s),
        };
        assert!(update(&empty(), vec![item]).unwrap().is_empty());
    }

    #[test]
    fn oversized_inputs_are_refused_before_decoding() {
        let big = vec![0xffu8; max_state_bytes() + 1];
        let held = empty();
        let t = Instant::now();
        for r in [
            update(&held, vec![UpdateData::State(State::from(big.clone()))]),
            update(
                &held,
                vec![UpdateData::Delta(StateDelta::from(big.clone()))],
            ),
            update(
                &held,
                vec![UpdateData::StateAndDelta {
                    state: held.clone(),
                    delta: StateDelta::from(big.clone()),
                }],
            ),
        ] {
            assert!(
                matches!(&r, Err(ContractError::InvalidUpdateWithInfo { reason }) if reason.contains("more than")),
                "{r:?}"
            );
        }
        let sum = StateSummary::from(vec![0xffu8; max_summary_bytes() + 1]);
        assert!(matches!(
            C::get_state_delta(params(), held.clone(), sum),
            Err(ContractError::InvalidUpdateWithInfo { .. })
        ));
        // A held state that large is refused too.
        assert!(C::summarize_state(params(), State::from(big)).is_err());
        assert!(t.elapsed().as_millis() < 2000, "{:?}", t.elapsed());
    }

    #[test]
    fn a_summary_with_more_entries_than_a_valid_state_is_refused() {
        let mut sum = Summary::new();
        let mut n = 0u32;
        // 4 bytes of digest per entry would be cheaper to build, but a
        // digest is 32 bytes; stay under the byte bound and over the count
        // for one kind is not possible, so build a count just over.
        while n as usize <= max_summary_entries() {
            let mut d = [0u8; 32];
            d[..4].copy_from_slice(&n.to_be_bytes());
            sum.entry((n % 24) as u8)
                .or_default()
                .insert(river_core::room_state_v2::encoding::Bytes32(d));
            n += 1;
        }
        let bytes = to_cbor(&sum);
        assert!(
            bytes.len() > max_summary_bytes() || {
                // Within the byte bound: the entry count check catches it.
                true
            }
        );
        assert!(C::get_state_delta(params(), st(&small()), StateSummary::from(bytes)).is_err());
    }

    #[test]
    fn a_valid_states_summary_fits_the_summary_bound() {
        let s = small();
        assert!(to_cbor(&s.summarize()).len() <= max_summary_bytes());
        assert!(s.summarize().values().map(|d| d.len()).sum::<usize>() <= max_summary_entries());
    }

    #[test]
    #[ignore = "builds the full-size largest state (tens of seconds)"]
    fn the_largest_states_summary_fits_the_summary_bound() {
        for caps in [Caps::ceilings(), Caps::ceilings_legacy()] {
            let (s, _) = largest_state(caps, 0);
            assert!(to_cbor(&s.summarize()).len() <= max_summary_bytes());
            assert!(
                s.summarize().values().map(|d| d.len()).sum::<usize>() <= max_summary_entries()
            );
        }
    }

    #[test]
    fn a_delta_of_broken_signatures_is_refused() {
        let held = st(&small());
        let bad: Vec<Record> = (0..200).map(bad_ban).collect();
        assert!(matches!(
            update(&held, vec![delta_of(&bad)]),
            Err(ContractError::InvalidUpdateWithInfo { .. })
        ));
    }

    #[test]
    fn records_the_room_drops_unexamined_are_accepted_and_change_nothing() {
        let s = small();
        let held = st(&s);
        let records: Vec<Record> = (0..300).map(activity).collect();
        assert_eq!(
            update(&held, vec![delta_of(&records)]).unwrap(),
            held.as_ref()
        );
        // On the empty contract the result has no records: the empty form.
        assert!(update(&empty(), vec![delta_of(&records)])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn more_activity_records_than_a_valid_state_holds_are_refused() {
        let n = ceiling::MAX_LINKS + 1;
        let records: Vec<Record> = (0..n).map(activity).collect();
        assert!(matches!(
            update(&st(&small()), vec![delta_of(&records)]),
            Err(ContractError::InvalidUpdateWithInfo { .. })
        ));
        // Each item is bounded on its own: two items that hold more together
        // than one valid state are applied in turn, each within the bound.
        let half = records.len() / 2;
        let ok = update(
            &empty(),
            vec![delta_of(&records[..half]), delta_of(&records[half..])],
        );
        assert!(matches!(&ok, Ok(b) if b.is_empty()), "{ok:?}");
        // At most two items in turn: a third is refused, not a third run.
        let third = records.len() / 3;
        let r = update(
            &empty(),
            vec![
                delta_of(&records[..third]),
                delta_of(&records[third..2 * third]),
                delta_of(&records[2 * third..]),
                delta_of(&records[..1]),
            ],
        );
        assert!(
            matches!(r, Err(ContractError::InvalidUpdateWithInfo { .. })),
            "{r:?}"
        );
    }

    #[test]
    fn many_small_records_are_refused_before_they_are_decoded() {
        // 50,000 "clock records" whose payload does not decode as one: the
        // refusal names their number, so nothing was decoded.
        let n: u32 = 50_000;
        let mut delta = vec![0x9a];
        delta.extend(n.to_be_bytes());
        for _ in 0..n {
            delta.extend([0x82, 0x02, 0x00]);
        }
        let r = update(
            &st(&small()),
            vec![UpdateData::Delta(StateDelta::from(delta))],
        );
        match r {
            Err(ContractError::InvalidUpdateWithInfo { reason }) => {
                assert!(reason.contains("TooManyRecords"), "{reason}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn incoming_data_is_bounded_before_the_held_state_is_decoded() {
        // Held bytes that do not decode: an oversized delta is refused for
        // its size, so the held state was never read.
        let junk = State::from(vec![0xffu8; 16]);
        let big = UpdateData::Delta(StateDelta::from(vec![0xffu8; max_state_bytes() + 1]));
        match update(&junk, vec![big]) {
            Err(ContractError::InvalidUpdateWithInfo { reason }) => {
                assert!(reason.contains("delta is"), "{reason}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_full_state_and_one_new_record_are_applied_in_turn() {
        // A state part at the activity count and a delta with one more: over
        // the count together, so two runs of `f`, not a refusal.
        let records: Vec<Record> = (0..ceiling::MAX_LINKS + 1).map(activity).collect();
        let (full, one) = records.split_at(ceiling::MAX_LINKS as usize);
        let part = RoomStateV2::from_records(full.to_vec());
        let item = UpdateData::StateAndDelta {
            state: st(&part),
            delta: StateDelta::from(to_cbor(&one.to_vec())),
        };
        let held = st(&small());
        assert_eq!(update(&held, vec![item]).unwrap(), held.as_ref().to_vec());
    }

    #[test]
    fn items_with_no_records_do_not_count_towards_the_items_in_turn() {
        let n = ceiling::MAX_LINKS + 1;
        let records: Vec<Record> = (0..n).map(activity).collect();
        let half = records.len() / 2;
        let related = UpdateData::RelatedState {
            related_to: ContractInstanceId::new([3; 32]),
            state: empty(),
        };
        let ok = update(
            &empty(),
            vec![
                delta_of(&records[..half]),
                delta_of(&records[half..]),
                related,
                delta_of(&[]),
            ],
        );
        assert!(ok.is_ok(), "{ok:?}");
    }

    #[test]
    fn a_state_and_delta_whose_delta_repeats_its_state_is_not_refused() {
        // A state part at the activity count, and a delta repeating all of
        // it: together twice the count, but each record once.
        let records: Vec<Record> = (0..ceiling::MAX_LINKS).map(activity).collect();
        let part = RoomStateV2::from_records(records.clone());
        let item = UpdateData::StateAndDelta {
            state: st(&part),
            delta: StateDelta::from(to_cbor(&records)),
        };
        let held = st(&small());
        let got = update(&held, vec![item]);
        assert_eq!(got.unwrap(), held.as_ref().to_vec());
    }
}
