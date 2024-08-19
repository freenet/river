use super::*;

#[test]
fn test_multiple_deltas_1() {
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

    test_apply_deltas(
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
