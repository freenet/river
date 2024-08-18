use super::*;
use crate::state::configuration::Configuration;
use crate::state::member::{Member, MemberId};
use ed25519_dalek::{Signature, VerifyingKey};
use std::time::SystemTime;
use rand::prelude::*;

fn test_delta_commutativity(
    initial_state: ChatRoomState,
    deltas: Vec<ChatRoomDelta>,
    expected_final_state: ChatRoomState,
) {
    let mut rng = thread_rng();
    for _ in 0..10 {  // Run 10 random permutations
        let mut current_state = initial_state.clone();
        let mut permuted_deltas = deltas.clone();
        permuted_deltas.shuffle(&mut rng);

        for delta in permuted_deltas {
            current_state.apply_delta(delta);
        }

        assert_eq!(current_state, expected_final_state, "States do not match after applying deltas in a random order");
    }
}

#[test]
fn test_delta_application_order() {
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

    let delta2 = ChatRoomDelta {
        configuration: None,
        members: {
            let mut set = HashSet::new();
            set.insert(AuthorizedMember {
                member: Member {
                    public_key: VerifyingKey::from_bytes(&[
                        215, 90, 152, 1, 130, 177, 10, 183, 213, 75, 254, 211, 201, 100, 7, 58, 
                        14, 225, 114, 243, 218, 166, 35, 37, 175, 2, 26, 104, 247, 7, 81, 26
                    ]).unwrap(),
                    nickname: "Alice".to_string(),
                },
                invited_by: VerifyingKey::from_bytes(&[
                    215, 90, 152, 1, 130, 177, 10, 183, 213, 75, 254, 211, 201, 100, 7, 58, 
                    14, 225, 114, 243, 218, 166, 35, 37, 175, 2, 26, 104, 247, 7, 81, 27
                ]).unwrap(),
                signature: Signature::from_bytes(&[0; 64]),
            });
            set
        },
        upgrade: None,
        recent_messages: Vec::new(),
        ban_log: Vec::new(),
    };

    let delta3 = ChatRoomDelta {
        configuration: None,
        members: HashSet::new(),
        upgrade: None,
        recent_messages: vec![AuthorizedMessage {
            time: SystemTime::UNIX_EPOCH, // Use a fixed time for deterministic testing
            content: "Hello, world!".to_string(),
            author: MemberId(1),
            signature: Signature::from_bytes(&[5; 64]),
        }],
        ban_log: Vec::new(),
    };

    // Create the expected final state
    let mut expected_final_state = initial_state.clone();
    expected_final_state.apply_delta(delta1.clone());
    expected_final_state.apply_delta(delta2.clone());
    expected_final_state.apply_delta(delta3.clone());

    // Test commutativity
    test_delta_commutativity(
        initial_state,
        vec![delta1, delta2, delta3],
        expected_final_state,
    );
}
