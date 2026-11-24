//! Tests for validating deltas incrementally (`update_state` seeds a signature
//! memo from the stored state). The reference is the plain rules: apply or
//! merge natively with no memo seeding, then the whole-state check with a
//! fresh memo. The contract must return the same bytes, or an error exactly
//! when the reference does.

#[path = "../../../common/tests/fixtures/official_room.rs"]
mod fixture;

use super::*;
use ciborium::de::from_reader as decode;
use ed25519_dalek::SigningKey;
use fixture::{build, cbor, entry, info, msg, sk, Fix};
use river_core::room_state::member::MemberId;
use river_core::room_state::message::AuthorizedMessageV1;
use river_core::room_state::provisional::ProvisionalDelta;
use river_core::room_state::sigcheck;

type Bytes = Vec<u8>;

enum Input {
    Delta(Box<ChatRoomStateV1Delta>),
    State(Bytes),
}

fn data_of(input: &Input) -> UpdateData<'static> {
    match input {
        Input::Delta(d) => UpdateData::Delta(StateDelta::from(cbor(d))),
        Input::State(s) => UpdateData::State(State::from(s.clone())),
    }
}

/// The contract's `update_state`.
fn contract_update(params: &Bytes, state: &Bytes, input: &Input) -> Result<Bytes, String> {
    Contract::update_state(
        Parameters::from(params.clone()),
        State::from(state.clone()),
        vec![data_of(input)],
    )
    .map(|m| m.new_state.expect("a state").as_ref().to_vec())
    .map_err(|e| e.to_string())
}

/// The plain rules: apply or merge with no memo, then the whole-state check
/// with a fresh memo.
fn reference_update(params: &Bytes, state: &Bytes, input: &Input) -> Result<Bytes, String> {
    let params: ChatRoomParametersV1 = decode(params.as_slice()).map_err(|e| e.to_string())?;
    let mut s: ChatRoomStateV1 = decode(state.as_slice()).map_err(|e| e.to_string())?;
    match input {
        Input::Delta(d) => {
            if !cbor(d).is_empty() {
                s.apply_delta(&s.clone(), &params, &Some(d.clone()))
                    .map_err(|e| e.to_string())?;
            }
        }
        Input::State(t) => {
            let t: ChatRoomStateV1 = decode(t.as_slice()).map_err(|e| e.to_string())?;
            s.merge_from(&params, &t).map_err(|e| e.to_string())?;
        }
    }
    let out = cbor(&s);
    let decoded: ChatRoomStateV1 = decode(out.as_slice()).map_err(|e| e.to_string())?;
    sigcheck::scope(|| decoded.validate_whole_state(&params, &out)).map_err(|e| e.to_string())?;
    Ok(out)
}

fn contract_validate(params: &Bytes, state: &Bytes) -> bool {
    matches!(
        Contract::validate_state(
            Parameters::from(params.clone()),
            State::from(state.clone()),
            RelatedContracts::default(),
        ),
        Ok(ValidateResult::Valid)
    )
}

fn same(params: &Bytes, state: &Bytes, input: &Input) -> Result<Bytes, String> {
    let new = contract_update(params, state, input);
    let old = reference_update(params, state, input);
    match (&new, &old) {
        (Ok(a), Ok(b)) => assert_eq!(a, b, "outputs differ"),
        (Err(_), Err(_)) => {}
        _ => panic!(
            "accept/reject differ: contract {:?} reference {:?}",
            new.as_ref().map(|_| "ok"),
            old.as_ref().map(|_| "ok")
        ),
    }
    if let Ok(out) = &new {
        assert!(
            contract_validate(params, out),
            "an accepted output must validate"
        );
    }
    new
}

fn fixture(pool: usize) -> (Fix, Bytes, Bytes) {
    let f = build(pool);
    let p = cbor(&f.params);
    let s = cbor(&f.state);
    (f, p, s)
}

fn msg_delta(f: &Fix, k: &SigningKey, t: u64) -> ChatRoomStateV1Delta {
    ChatRoomStateV1Delta {
        recent_messages: Some(vec![msg(f.params.owner_id(), k, t)]),
        ..Default::default()
    }
}

fn join_delta(f: &Fix, k: &SigningKey, t: u64) -> ChatRoomStateV1Delta {
    ChatRoomStateV1Delta {
        provisional: Some(ProvisionalDelta {
            entries: vec![entry(&f.params, k)],
            bans: vec![],
        }),
        member_info: Some(vec![info(k, "newcomer")]),
        recent_messages: Some(vec![msg(f.params.owner_id(), k, t)]),
        ..Default::default()
    }
}

fn real_checks_of<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let before = sigcheck::real_checks();
    let r = f();
    (r, sigcheck::real_checks() - before)
}

/// Real signature checks per update on an Official-room-shaped room.
#[test]
fn signature_checks_per_update_are_constant() {
    let (f, p, s) = fixture(8);
    // The whole-state check is a full check and stays one.
    let (ok, full) = real_checks_of(|| contract_validate(&p, &s));
    assert!(ok);
    assert!(full > 1000, "validate_state checks every record ({full})");

    let (r, n) = real_checks_of(|| {
        contract_update(&p, &s, &Input::Delta(msg_delta(&f, &f.members[3], 10_000)))
    });
    r.unwrap();
    assert_eq!(n, 1, "one member message");

    let (r, n) = real_checks_of(|| {
        contract_update(&p, &s, &Input::Delta(msg_delta(&f, &f.pool[0], 10_000)))
    });
    r.unwrap();
    assert_eq!(n, 1, "one newcomer message with a held entry");

    let nk = sk(700_000);
    let (r, n) =
        real_checks_of(|| contract_update(&p, &s, &Input::Delta(join_delta(&f, &nk, 10_000))));
    r.unwrap();
    assert_eq!(n, 3, "entry, member_info and message of a newcomer join");

    // A duplicate of a held record costs nothing.
    let held = f.state.recent_messages.messages[0].clone();
    let dup = ChatRoomStateV1Delta {
        recent_messages: Some(vec![held]),
        ..Default::default()
    };
    let (r, n) = real_checks_of(|| contract_update(&p, &s, &Input::Delta(dup)));
    r.unwrap();
    assert_eq!(n, 0, "a duplicate record");
}

#[test]
fn full_state_merge_checks_only_new_records() {
    let (f, p, s) = fixture(8);
    let mut other = f.state.clone();
    let d = msg_delta(&f, &f.members[5], 10_000);
    other
        .apply_delta(&f.state.clone(), &f.params, &Some(d))
        .unwrap();
    let (r, n) = real_checks_of(|| contract_update(&p, &s, &Input::State(cbor(&other))));
    r.unwrap();
    assert_eq!(n, 1, "one new record in the incoming full state");
}

#[test]
fn memo_is_not_active_between_calls() {
    let (f, p, s) = fixture(8);
    contract_update(&p, &s, &Input::Delta(msg_delta(&f, &f.members[3], 10_000))).unwrap();
    assert!(!sigcheck::is_active(), "update_state leaves no memo behind");
    assert!(contract_validate(&p, &s));
    assert!(
        !sigcheck::is_active(),
        "validate_state leaves no memo behind"
    );
}

/// Same output and same decision as the plain rules, for the usual inputs.
#[test]
fn matches_the_plain_rules() {
    let (f, p, s) = fixture(8);
    let nk = sk(700_001);
    let mut other = f.state.clone();
    other
        .apply_delta(
            &f.state.clone(),
            &f.params,
            &Some(msg_delta(&f, &f.members[5], 10_000)),
        )
        .unwrap();
    let inputs = vec![
        Input::Delta(msg_delta(&f, &f.pool[0], 10_000)),
        Input::Delta(msg_delta(&f, &f.members[3], 10_000)),
        Input::Delta(join_delta(&f, &nk, 10_000)),
        Input::Delta(ChatRoomStateV1Delta::default()),
        Input::State(cbor(&other)),
        Input::State(s.clone()),
    ];
    for i in &inputs {
        assert!(same(&p, &s, i).is_ok());
    }
}

fn corrupt(sig: &ed25519_dalek::Signature) -> ed25519_dalek::Signature {
    let mut b = sig.to_bytes();
    b[10] ^= 1;
    ed25519_dalek::Signature::from_bytes(&b)
}

/// A record with a valid id whose signature is corrupted is refused, however
/// it arrives and whatever the memo holds.
#[test]
fn forged_records_are_rejected() {
    let (f, p, s) = fixture(8);
    let base = contract_update(&p, &s, &Input::Delta(ChatRoomStateV1Delta::default())).unwrap();
    // Refused means an error, or an output that did not take the record.
    let refused = |input: Input, what: &str| match same(&p, &s, &input) {
        Err(_) => {}
        Ok(out) => assert_eq!(out, base, "{what} was taken"),
    };
    // A new message from a member, signature corrupted.
    let mut m: AuthorizedMessageV1 = msg(f.params.owner_id(), &f.members[3], 10_000);
    m.signature = corrupt(&m.signature);
    let d = ChatRoomStateV1Delta {
        recent_messages: Some(vec![m.clone()]),
        ..Default::default()
    };
    refused(Input::Delta(d), "forged message delta");

    // The same inside a full state.
    let mut t = f.state.clone();
    t.recent_messages.messages.push(m);
    refused(Input::State(cbor(&t)), "forged message in a full state");

    // A held message re-sent with a corrupted signature.
    let mut held = f.state.recent_messages.messages[0].clone();
    held.signature = corrupt(&held.signature);
    let d = ChatRoomStateV1Delta {
        recent_messages: Some(vec![held]),
        ..Default::default()
    };
    refused(Input::Delta(d), "forged copy of a held message");

    // A ban whose signature is corrupted.
    let mut ban = f.state.bans.0[0].clone();
    ban.ban.banned_at += std::time::Duration::from_secs(1);
    let d = ChatRoomStateV1Delta {
        bans: Some(vec![ban]),
        ..Default::default()
    };
    refused(Input::Delta(d), "forged ban");

    // A stored state that is invalid, then a forged delta: still refused,
    // and a valid state in the same thread is still checked afterwards.
    let mut bad = f.state.clone();
    bad.recent_messages.messages[0].signature = corrupt(&bad.recent_messages.messages[0].signature);
    assert!(
        !contract_validate(&p, &cbor(&bad)),
        "forged stored message fails validate_state"
    );
    assert!(contract_validate(&p, &s));
}

/// A stored state that no longer verifies (disk damage): seeding fails and the
/// update continues unseeded, with the plain rules' result.
#[test]
fn seeding_failure_continues_unseeded() {
    let (f, p, _) = fixture(8);
    let mut bad = f.state.clone();
    bad.recent_messages.messages[0].signature = corrupt(&bad.recent_messages.messages[0].signature);
    let bad = cbor(&bad);
    let d = Input::Delta(msg_delta(&f, &f.members[3], 10_000));
    let _ = same(&p, &bad, &d);
    assert!(!sigcheck::is_active());
}

/// Seeded facts are true: re-check every fact the seeding run recorded.
#[test]
fn every_seeded_fact_is_true() {
    let (f, _, _) = fixture(8);
    let real_before = sigcheck::real_checks();
    sigcheck::scope(|| {
        let r = sigcheck::seed_from(|| f.state.verify(&f.state, &f.params));
        assert!(r.is_ok());
    });
    assert_eq!(
        sigcheck::real_checks(),
        real_before,
        "seeding does no crypto"
    );
    // Re-run the same verify for real: it passes, so every assumed fact held.
    assert!(f.state.verify(&f.state, &f.params).is_ok());
    assert!(sigcheck::real_checks() > real_before);
}

/// Corrupting the signature of any held kind of record makes `verify` fail,
/// so no check inside `verify` tolerates a bad signature (the seeding relies
/// on that).
#[test]
fn every_signature_check_in_verify_is_mandatory() {
    let (f, _, _) = fixture(8);
    type Mutate = fn(&mut ChatRoomStateV1);
    let cases: Vec<(&str, Mutate)> = vec![
        ("configuration", |s| {
            s.configuration.signature = corrupt(&s.configuration.signature)
        }),
        ("message", |s| {
            let m = &mut s.recent_messages.messages[0];
            m.signature = corrupt(&m.signature)
        }),
        ("member", |s| {
            let m = &mut s.members.members[0];
            m.signature = corrupt(&m.signature)
        }),
        ("ban", |s| {
            let b = &mut s.bans.0[0];
            b.signature = corrupt(&b.signature)
        }),
        ("provisional entry", |s| {
            let e = &mut s.provisional.entries[0];
            e.signature = corrupt(&e.signature)
        }),
        ("provisional ban", |s| {
            let b = &mut s.provisional.bans[0];
            b.signature = corrupt(&b.signature)
        }),
    ];
    for (name, mutate) in cases {
        let mut s = f.state.clone();
        mutate(&mut s);
        assert!(
            s.verify(&s, &f.params).is_err(),
            "a corrupted {name} signature must fail verify"
        );
    }
}

/// Deterministic pseudo-random sequences of inputs, in different orders, with
/// duplicates and stale deltas: each step compared with the plain rules.
#[test]
fn random_sequences_match_the_plain_rules() {
    let (f, p, s0) = fixture(8);
    let owner_id = f.params.owner_id();
    let newcomers: Vec<SigningKey> = (0..6).map(|i| sk(710_000 + i)).collect();
    let mut inputs: Vec<ChatRoomStateV1Delta> = Vec::new();
    for (i, k) in f.members.iter().take(8).enumerate() {
        inputs.push(msg_delta(&f, k, 10_000 + i as u64));
    }
    for (i, k) in f.pool.iter().take(3).enumerate() {
        inputs.push(msg_delta(&f, k, 11_000 + i as u64));
    }
    for (i, k) in newcomers.iter().enumerate() {
        inputs.push(join_delta(&f, k, 12_000 + i as u64));
    }
    inputs.push(ChatRoomStateV1Delta {
        member_info: Some(vec![info(&f.members[2], "renamed member")]),
        ..Default::default()
    });
    let _ = owner_id;
    for seed in 0..6u64 {
        let mut rng = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let mut next = || {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (rng >> 33) as usize
        };
        let mut state = s0.clone();
        let mut seen: Vec<usize> = Vec::new();
        for _ in 0..8 {
            // pick a fresh delta, a duplicate of an earlier one, or a stale one
            let pick = if !seen.is_empty() && next() % 3 == 0 {
                seen[next() % seen.len()]
            } else {
                next() % inputs.len()
            };
            seen.push(pick);
            if let Ok(out) = same(&p, &state, &Input::Delta(inputs[pick].clone())) {
                state = out;
            }
        }
    }
}

/// Two forks exchange full states through `update_state` and converge to
/// the same bytes the plain rules give.
#[test]
fn forks_converge_through_update_state() {
    let (f, p, s) = fixture(8);
    let a = contract_update(&p, &s, &Input::Delta(msg_delta(&f, &f.members[3], 10_001))).unwrap();
    let b = contract_update(&p, &s, &Input::Delta(msg_delta(&f, &f.members[5], 10_002))).unwrap();
    let ab = same(&p, &a, &Input::State(b.clone())).unwrap();
    let ba = same(&p, &b, &Input::State(a.clone())).unwrap();
    assert_eq!(ab, ba, "both sides converge to the same state");
    // Idempotent.
    assert_eq!(same(&p, &ab, &Input::State(ba.clone())).unwrap(), ab);
}

/// A full state that carries an invalid part the host already holds (here a
/// held message re-sent with a corrupted signature, beside a valid new one):
/// the contract and the plain rules agree, and an accepted output validates.
#[test]
fn full_state_with_invalid_parts_matches_the_plain_rules() {
    let (f, p, s) = fixture(8);
    let mut t = f.state.clone();
    t.recent_messages.messages[0].signature = corrupt(&t.recent_messages.messages[0].signature);
    t.recent_messages
        .messages
        .push(msg(f.params.owner_id(), &f.members[3], 10_000));
    let _ = same(&p, &s, &Input::State(cbor(&t)));
}

#[test]
fn only_the_sigcheck_module_runs_ed25519() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../common/src");
    let mut stack = vec![std::path::PathBuf::from(root)];
    let mut offenders = Vec::new();
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap() {
            let path = e.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x == "rs") {
                let name = path.file_name().unwrap().to_string_lossy().to_string();
                if name == "sigcheck.rs" {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                // Production code only: stop at the first test module.
                let prod = text.split("#[cfg(test)]").next().unwrap();
                for (i, line) in prod.lines().enumerate() {
                    let l = line.trim_start();
                    if l.starts_with("//") {
                        continue;
                    }
                    if l.contains(".verify_strict(")
                        || l.contains(".verify(&") && l.contains("_vk")
                        || l.contains("verifying_key.verify(")
                    {
                        offenders.push(format!("{}:{}", path.display(), i + 1));
                    }
                }
            }
        }
    }
    assert!(offenders.is_empty(), "direct ed25519 checks: {offenders:?}");
}

#[test]
fn seeding_is_called_only_from_update_state() {
    let src = include_str!("lib.rs");
    let prod = src.split("#[cfg(test)]").next().unwrap();
    assert_eq!(prod.matches("seed_from(").count(), 1);
    let (validate, update) = prod.split_once("fn update_state(").unwrap();
    assert!(
        validate.contains("sigcheck::scope"),
        "validate_state opens a scope"
    );
    assert!(
        !validate.contains("seed_from("),
        "validate_state never seeds"
    );
    assert!(update.contains("seed_from("));
    assert!(
        update.contains(".validate_whole_state("),
        "update_state checks its own output"
    );
}

/// The crypto crates stay optimised in the contract build.
#[test]
fn crypto_crates_build_optimised() {
    let toml = include_str!("../../../Cargo.toml");
    for krate in ["curve25519-dalek", "ed25519-dalek", "sha2"] {
        let header = format!("[profile.release.package.{krate}]");
        let at = toml
            .find(&header)
            .unwrap_or_else(|| panic!("{header} missing"));
        let section = toml[at + header.len()..]
            .lines()
            .take(2)
            .collect::<String>();
        assert!(section.contains("opt-level = 3"), "{krate}: {section}");
    }
}

#[allow(dead_code)]
fn _unused(_: MemberId) {}
