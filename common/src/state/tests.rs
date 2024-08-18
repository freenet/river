use super::*;
use crate::delta::ChatRoomDelta;
use ed25519_dalek::Signature;
use std::collections::HashSet;
use std::time::SystemTime;

mod test_utils;
mod configuration_tests;
mod member_tests;
mod message_tests;
mod ban_tests;
mod upgrade_tests;

use test_utils::*;

#[test]
fn test_delta_application_order() {
    let parameters = create_test_parameters();
    let initial_state = create_sample_state();

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

    let alice_member = create_sample_member("Alice", [
        215, 90, 152, 1, 130, 177, 10, 183, 213, 75, 254, 211, 201, 100, 7, 58,
        14, 225, 114, 243, 218, 166, 35, 37, 175, 2, 26, 104, 247, 7, 81, 26
    ], parameters.owner);

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
        members: HashSet::new(),
        upgrade: None,
        recent_messages: vec![AuthorizedMessage {
            time: SystemTime::UNIX_EPOCH,
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
        match expected_final_state.apply_delta(delta.clone(), &parameters) {
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
        expected_final_state.clone(),
        &parameters,
    );
}
