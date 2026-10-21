//! States written by the room contract generation before freenet/river#703
//! must keep working under this one: the migration PUT carries them forward
//! and runs only `verify`, and they then meet new-code states in merges.
//!
//! `main_era_room_states.hex` holds two such states, one per line, produced
//! by `origin/main` (118d64a4) code: each holds a member-issued ban whose
//! target left (so ban evidence), member info and messages, and a
//! configuration at version 2; the two configurations differ, which `main`
//! could never reconcile. The generator is the private example
//! `gen_703_fixture.rs`, run against that tree; it is not in the repository
//! because it only compiles against the old API.

use freenet_scaffold::ComposableState;
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1};

fn states() -> (ChatRoomParametersV1, Vec<ChatRoomStateV1>) {
    let owner = ed25519_dalek::SigningKey::from_bytes(&[21; 32]);
    let params = ChatRoomParametersV1 {
        owner: owner.verifying_key(),
    };
    let states = include_str!("main_era_room_states.hex")
        .lines()
        .map(|line| {
            let bytes: Vec<u8> = (0..line.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&line[i..i + 2], 16).unwrap())
                .collect();
            ciborium::de::from_reader(bytes.as_slice()).expect("main-era state decodes")
        })
        .collect();
    (params, states)
}

fn ser(s: &ChatRoomStateV1) -> Vec<u8> {
    let mut v = Vec::new();
    ciborium::ser::into_writer(s, &mut v).unwrap();
    v
}

#[test]
fn main_era_states_verify_and_reserialize_identically() {
    let (params, states) = states();
    assert_eq!(states.len(), 2);
    let raw: Vec<&str> = include_str!("main_era_room_states.hex").lines().collect();
    for (s, line) in states.iter().zip(raw) {
        s.verify(s, &params)
            .expect("verify accepts a main-era state");
        assert!(!s.ban_evidence.members.is_empty(), "premise: ban evidence");
        let hex: String = ser(s).iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex, line,
            "a main-era state must re-serialize byte for byte"
        );
    }
}

#[test]
fn a_main_era_state_is_a_fixpoint_of_the_new_merge() {
    let (params, states) = states();
    for s in &states {
        let mut m = s.clone();
        m.merge(s, &params, s).unwrap();
        assert!(
            ser(&m) == ser(s),
            "merge(S, S) must be S (state_idempotence)"
        );
    }
}

#[test]
fn main_era_states_with_tied_configurations_now_converge() {
    let (params, states) = states();
    let (a, b) = (&states[0], &states[1]);
    assert_eq!(
        a.configuration.configuration.configuration_version,
        b.configuration.configuration.configuration_version
    );
    assert_ne!(a.configuration, b.configuration, "premise: a tie");
    let mut ab = a.clone();
    ab.merge(a, &params, b).unwrap();
    let mut ba = b.clone();
    ba.merge(b, &params, a).unwrap();
    assert!(ser(&ab) == ser(&ba), "merge(A, B) must equal merge(B, A)");
    ab.verify(&ab, &params).unwrap();
}
