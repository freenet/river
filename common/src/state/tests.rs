use super::*;
use crate::parameters::ChatRoomParameters;
use crate::state::configuration::Configuration;
use crate::state::member::Member;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use rand::prelude::*;
use std::sync::Mutex;
use std::time::SystemTime;

lazy_static::lazy_static! {
    static ref LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
}

macro_rules! log {
    ($($arg:tt)*) => {
        LOG.lock().unwrap().push(format!($($arg)*));
    };
}

fn create_test_parameters() -> ChatRoomParameters {
    let mut rng = rand::thread_rng();
    let mut secret_key = [0u8; 32];
    rng.fill_bytes(&mut secret_key);
    let signing_key = SigningKey::from_bytes(&secret_key);
    ChatRoomParameters {
        owner: signing_key.verifying_key(),
    }
}

fn test_delta_commutativity<F>(
    initial_state: ChatRoomState,
    deltas: Vec<ChatRoomDelta>,
    state_validator: F,
    parameters: &ChatRoomParameters,
) where
    F: Fn(&ChatRoomState) -> bool,
{
    LOG.lock().unwrap().clear();
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
            match current_state.apply_delta(delta, parameters) {
                Ok(_) => {
                    log!("After state: {:?}", current_state);
                    if let Err(e) = current_state.validate(parameters) {
                        panic!("State became invalid after applying delta {}: {}. Log:\n{}", j + 1, e, LOG.lock().unwrap().join("\n"));
                    }
                },
                Err(e) => {
                    panic!("Error applying delta {}: {}. Log:\n{}", j + 1, e, LOG.lock().unwrap().join("\n"));
                }
            }
            log!("");
        }

        assert!(state_validator(&current_state), "State does not meet expected criteria after applying deltas in permutation {}. Log:\n{}", i + 1, LOG.lock().unwrap().join("\n"));
        log!("Permutation {} successful", i + 1);
        log!("");
    }

    log!("All permutations successful");

    // Create a delta from one of the valid final states relative to the initial state
    let mut final_state = initial_state.clone();
    for delta in deltas.iter() {
        final_state.apply_delta(delta, parameters).unwrap();
    }
    let initial_summary = initial_state.summarize();
    let final_delta = final_state.create_delta(&initial_summary);

    // Apply this delta to the initial state
    let mut new_state = initial_state.clone();
    new_state.apply_delta(&final_delta, parameters).unwrap();

    // Verify that the new state passes the state_validator
    assert!(
        state_validator(&new_state),
        "State created from delta does not meet expected criteria. Log:\n{}",
        LOG.lock().unwrap().join("\n")
    );

    log!("Delta creation and application successful");
}

#[test]
fn test_delta_application_order() {
    let parameters = create_test_parameters();

    // Create a sample initial state
    let initial_state = ChatRoomState {
        configuration: AuthorizedConfiguration {
            configuration: Configuration {
                configuration_version: 1,
                name: "Test Room".to_string(),
                max_recent_messages: 100,
                max_user_bans: 10,
            },
            signature: Signature::from_bytes(&[0; 64]),
        },
        members: HashSet::new(),
        upgrade: None,
        recent_messages: Vec::new(),
        ban_log: Vec::new(),
    };

    // Create sample deltas
    let delta1 = ChatRoomDelta {
        configuration: Some(AuthorizedConfiguration {
            configuration: Configuration {
                configuration_version: 2,
                name: "Updated Room".to_string(),
                max_recent_messages: 150,
                max_user_bans: 15,
            },
            signature: Signature::from_bytes(&[1; 64]),
        }),
        members: HashSet::new(),
        upgrade: None,
        recent_messages: Vec::new(),
        ban_log: Vec::new(),
    };

    let alice_member = AuthorizedMember {
        member: Member {
            public_key: VerifyingKey::from_bytes(&[
                215, 90, 152, 1, 130, 177, 10, 183, 213, 75, 254, 211, 201, 100, 7, 58, 
                14, 225, 114, 243, 218, 166, 35, 37, 175, 2, 26, 104, 247, 7, 81, 26
            ]).unwrap(),
            nickname: "Alice".to_string(),
        },
        invited_by: parameters.owner,
        signature: Signature::from_bytes(&[0; 64]),
    };

    let delta2 = ChatRoomDelta {
        configuration: None,
        members: {
            let mut set = HashSet::new();
            set.insert(alice_member.clone());
            set
        },
        upgrade: None,
        recent_messages: Vec::new(),
        ban_log: Vec::new(),
    };

    let alice_member_id = alice_member.member.id();
    let delta3 = ChatRoomDelta {
        configuration: None,
        members: {
            let mut set = HashSet::new();
            set.insert(alice_member);
            set
        },
        upgrade: None,
        recent_messages: vec![AuthorizedMessage {
            time: SystemTime::UNIX_EPOCH, // Use a fixed time for deterministic testing
            content: "Hello, world!".to_string(),
            author: alice_member_id,
            signature: Signature::from_bytes(&[5; 64]),
        }],
        ban_log: Vec::new(),
    };

    // Create the expected final state
    let mut expected_final_state = initial_state.clone();
    let deltas = vec![delta1.clone(), delta2.clone(), delta3.clone()];

    for (i, delta) in deltas.iter().enumerate() {
        let before_summary = expected_final_state.summarize();
        println!("Applying delta{}", i + 1);
        println!("Delta: {:?}", delta);
        println!("Before summary: {:?}", before_summary);
        match expected_final_state.apply_delta(&delta, &parameters) {
            Ok(_) => {
                let after_summary = expected_final_state.summarize();
                let diff = expected_final_state.create_delta(&before_summary);
                println!("After summary: {:?}", after_summary);
                println!("Diff: {:?}", diff);
            },
            Err(e) => {
                panic!("Error applying delta{}: {}", i + 1, e);
            }
        }
        println!();
    }

    // Test commutativity
    test_delta_commutativity(
        initial_state.clone(),
        deltas,
        |state: &ChatRoomState| {
            // Define your state validation logic here
            state.configuration.configuration.name == "Updated Room" &&
            state.configuration.configuration.max_recent_messages == 150 &&
            state.configuration.configuration.max_user_bans == 15 &&
            state.members.len() == 1 &&
            state.recent_messages.len() == 1
        },
        &parameters,
    );
}
