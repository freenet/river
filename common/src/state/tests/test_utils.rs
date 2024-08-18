use crate::state::*;
use crate::parameters::ChatRoomParameters;
use crate::logging;
use crate::log;
use ed25519_dalek::{Signature, VerifyingKey, SigningKey};
use std::time::SystemTime;
use rand::prelude::*;

pub fn create_test_parameters() -> ChatRoomParameters {
    let mut rng = rand::thread_rng();
    let mut secret_key = [0u8; 32];
    rng.fill_bytes(&mut secret_key);
    let signing_key = SigningKey::from_bytes(&secret_key);
    ChatRoomParameters {
        owner: signing_key.verifying_key(),
    }
}

pub fn test_delta_commutativity(
    initial_state: ChatRoomState,
    deltas: Vec<ChatRoomDelta>,
    expected_final_state: ChatRoomState,
    parameters: &ChatRoomParameters,
) {
    logging::clear_log();
    let mut rng = thread_rng();
    for i in 0..10 {  // Run 10 random permutations
        log!("Permutation {}", i + 1);
        let mut current_state = initial_state.clone();
        let mut permuted_deltas = deltas.clone();
        permuted_deltas.shuffle(&mut rng);

        for (j, delta) in permuted_deltas.iter().enumerate() {
            log!("Applying delta {}", j + 1);
            log!("Delta: {:?}", delta);
            log!("Before state: {:?}", current_state);
            match current_state.apply_delta(delta.clone(), parameters) {
                Ok(_) => {
                    log!("After state: {:?}", current_state);
                    if let Err(e) = current_state.validate(parameters) {
                        panic!("State became invalid after applying delta {}: {}. Log:\n{}", j + 1, e, logging::get_log());
                    }
                },
                Err(e) => {
                    panic!("Error applying delta {}: {}. Log:\n{}", j + 1, e, logging::get_log());
                }
            }
            log!("");
        }

        assert_eq!(current_state, expected_final_state, "States do not match after applying deltas in permutation {}. Log:\n{}", i + 1, logging::get_log());
        log!("Permutation {} successful", i + 1);
        log!("");
    }

    // Create a summary of the initial state
    let initial_summary = initial_state.summarize();
    log!("Initial state summary: {:?}", initial_summary);

    // Create a delta from the initial state to the expected final state
    let final_delta = expected_final_state.create_delta(&initial_summary);
    log!("Final delta: {:?}", final_delta);

    // Apply the final delta to the initial state
    let mut final_state = initial_state.clone();
    match final_state.apply_delta(final_delta, parameters) {
        Ok(_) => {
            log!("Final state after applying delta: {:?}", final_state);
            if let Err(e) = final_state.validate(parameters) {
                panic!("Final state became invalid after applying delta: {}. Log:\n{}", e, logging::get_log());
            }
        },
        Err(e) => {
            panic!("Error applying final delta: {}. Log:\n{}", e, logging::get_log());
        }
    }

    // Verify that the final state matches the expected final state
    assert_eq!(final_state, expected_final_state, "Final state does not match expected final state after applying single delta. Log:\n{}", logging::get_log());
    log!("Final state matches expected final state");
}

pub fn create_sample_state() -> ChatRoomState {
    ChatRoomState {
        configuration: AuthorizedConfiguration {
            configuration: Configuration {
                configuration_version: 1,
                name: "Test Room".to_string(),
                max_recent_messages: 100,
                max_user_bans: 10,
            },
            signature: Signature::from_bytes(&[0; 64]).unwrap(),
        },
        members: HashSet::new(),
        upgrade: None,
        recent_messages: Vec::new(),
        ban_log: Vec::new(),
    }
}

pub fn create_sample_member(nickname: &str, public_key: [u8; 32], invited_by: VerifyingKey) -> AuthorizedMember {
    AuthorizedMember {
        member: Member {
            public_key: VerifyingKey::from_bytes(&public_key).unwrap(),
            nickname: nickname.to_string(),
        },
        invited_by,
        signature: Signature::from_bytes(&[0; 64]).unwrap(),
    }
}
