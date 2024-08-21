use crate::{ChatRoomState, ChatRoomDelta, ChatRoomParameters};
use crate::state::{AuthorizedConfiguration, AuthorizedMember, AuthorizedMessage, MemberId};
use crate::state::member::Member;
use crate::state::message::Message;
use std::collections::HashSet;
use std::time::SystemTime;
use ed25519_dalek::SigningKey;
use rand::thread_rng;
use crate::state::tests::test_apply_deltas;

#[test]
fn test_multiple_deltas_1() {

    // Create a sample initial state
    let initial_config = crate::state::configuration::Configuration {
        room_fhash: 0, // Use a dummy value for testing
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
    let mut rng = thread_rng();
    let delta1_signing_key = SigningKey::generate(&mut rng);
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
    let parameters = ChatRoomParameters {
        owner: owner_signing_key.verifying_key(),
    };
    let alice_member = AuthorizedMember::new(
        0, // Use a dummy room_fhash for testing
        Member {
            public_key: alice_signing_key.verifying_key(),
            nickname: "Alice".to_string(),
        },
        parameters.owner,
        &owner_signing_key
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
            0, // Use a dummy room_fhash for testing
            Message {
                time: SystemTime::UNIX_EPOCH, // Use a fixed time for deterministic testing
                content: "Hello, world!".to_string(),
            },
            alice_member_id,
            &alice_signing_key
        )],
        ban_log: Vec::new(),
    };

    let result = test_apply_deltas(
        initial_state.clone(),
        vec![delta1, delta2, delta3],
        |state: &ChatRoomState| {
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
