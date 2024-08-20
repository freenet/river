use crate::ChatRoomState;
use crate::ChatRoomDelta;
use crate::ChatRoomParameters;
use crate::state::AuthorizedConfiguration;
use crate::state::AuthorizedMember;
use crate::state::AuthorizedMessage;
use crate::state::member::Member;
use crate::state::message::Message;
use crate::state::MemberId;
use std::collections::HashSet;
use std::time::SystemTime;
use ed25519_dalek::{SigningKey, Signature, VerifyingKey};
use rand::thread_rng;
use crate::util::fast_hash;
use crate::state::tests::{create_test_parameters, test_apply_deltas};

#[test]
fn test_multiple_deltas_1() {

    // Create a sample initial state
    let initial_config = crate::state::configuration::Configuration {
        configuration_version: 1,
        name: "Test Room".to_string(),
        max_recent_messages: 100,
        max_user_bans: 10,
        max_message_size: 1000,
        max_nickname_size: 50,
    };
    let initial_signing_key = SigningKey::from_bytes(&[0; 32]);
    let initial_state = ChatRoomState {
        configuration: AuthorizedConfiguration::new(initial_config, &initial_signing_key),
        members: HashSet::new(),
        upgrade: None,
        recent_messages: Vec::new(),
        ban_log: Vec::new(),
    };

    // Create sample deltas
    let delta1_config = crate::state::configuration::Configuration {
        configuration_version: 2,
        name: "Updated Room".to_string(),
        max_recent_messages: 150,
        max_user_bans: 15,
        max_message_size: 1000,
        max_nickname_size: 50,
    };
    let delta1_signing_key = SigningKey::from_bytes(&[1; 32]);
    let delta1 = ChatRoomDelta {
        configuration: Some(AuthorizedConfiguration::new(delta1_config, &delta1_signing_key)),
        members: HashSet::new(),
        upgrade: None,
        recent_messages: Vec::new(),
        ban_log: Vec::new(),
    };

    let mut rng = rand::thread_rng();
    
    let alice_signing_key = SigningKey::generate(&mut rng);
    let owner_signing_key = SigningKey::generate(&mut rng);
    let alice_member = AuthorizedMember::new(
        Member {
            public_key: alice_signing_key.verifying_key(),
            nickname: "Alice".to_string(),
        },
        parameters.owner,
        &owner_signing_key // Use the owner's signing key here
    );

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
        recent_messages: vec![AuthorizedMessage::new(
            Message {
                time: SystemTime::UNIX_EPOCH, // Use a fixed time for deterministic testing
                content: "Hello, world!".to_string(),
            },
            alice_member_id,
            &alice_signing_key
        )],
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

    let result = test_apply_deltas(
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
    assert!(result.is_ok(), "Failed to apply deltas: {:?}", result.err());
}
