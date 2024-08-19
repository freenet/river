use super::*;
use crate::util::fast_hash;
use crate::state::ban::UserBan;
use ed25519_dalek::SigningKey;
use rand::{thread_rng, RngCore};

fn create_delta(
    members: Option<HashSet<AuthorizedMember>>,
    messages: Option<Vec<AuthorizedMessage>>,
) -> ChatRoomDelta {
    ChatRoomDelta {
        configuration: None,
        members: members.unwrap_or_default(),
        upgrade: None,
        recent_messages: messages.unwrap_or_default(),
        ban_log: Vec::new(),
    }
}

#[test]
fn test_member_added_by_owner() {
    let parameters = create_test_parameters();
    let initial_state = ChatRoomState::default();

    let mut rng = thread_rng();
    let mut secret_key_bytes = [0u8; 32];
    rng.fill_bytes(&mut secret_key_bytes);
    let new_member_key = SigningKey::from_bytes(&secret_key_bytes.into());
    let new_member = AuthorizedMember {
        member: Member {
            public_key: new_member_key.verifying_key(),
            nickname: "Alice".to_string(),
        },
        invited_by: parameters.owner,
        signature: Signature::from_bytes(&[0; 64]),
    };

    let delta = create_delta(Some({
        let mut set = HashSet::new();
        set.insert(new_member);
        set
    }), None);

    assert!(test_apply_deltas(
        initial_state,
        vec![delta],
        |state: &ChatRoomState| state.members.len() == 1,
        &parameters,
    ).is_ok());
}

#[test]
fn test_member_added_by_existing_member() {
    let parameters = create_test_parameters();
    let mut initial_state = ChatRoomState::default();

    let existing_member = AuthorizedMember {
        member: Member {
            public_key: parameters.owner,
            nickname: "Alice".to_string(),
        },
        invited_by: parameters.owner,
        signature: Signature::from_bytes(&[0; 64]),
    };
    initial_state.members.insert(existing_member.clone());

    let mut rng = thread_rng();
    let mut secret_key_bytes = [0u8; 32];
    rng.fill_bytes(&mut secret_key_bytes);
    let new_member_key = SigningKey::from_bytes(&secret_key_bytes.into());
    let new_member = AuthorizedMember {
        member: Member {
            public_key: new_member_key.verifying_key(),
            nickname: "Bob".to_string(),
        },
        invited_by: existing_member.member.public_key,
        signature: Signature::from_bytes(&[0; 64]),
    };

    let delta = ChatRoomDelta {
        configuration: None,
        members: {
            let mut set = HashSet::new();
            set.insert(new_member);
            set
        },
        upgrade: None,
        recent_messages: Vec::new(),
        ban_log: Vec::new(),
    };

    assert!(test_apply_deltas(
        initial_state,
        vec![delta],
        |state: &ChatRoomState| state.members.len() == 2,
        &parameters,
    ).is_ok());
}

#[test]
fn test_message_added_by_owner() {
    let parameters = create_test_parameters();
    let mut initial_state = ChatRoomState::default();

    // Add the owner as a member
    let owner_member = AuthorizedMember {
        member: Member {
            public_key: parameters.owner,
            nickname: "Owner".to_string(),
        },
        invited_by: parameters.owner,
        signature: Signature::from_bytes(&[0; 64]),
    };
    initial_state.members.insert(owner_member);

    let message = AuthorizedMessage {
        time: SystemTime::UNIX_EPOCH,
        content: "Hello from owner".to_string(),
        author: MemberId(fast_hash(&parameters.owner.to_bytes())),
        signature: Signature::from_bytes(&[0; 64]),
    };

    let delta = create_delta(None, Some(vec![message]));

    assert!(test_apply_deltas(
        initial_state,
        vec![delta],
        |state: &ChatRoomState| state.recent_messages.len() == 1,
        &parameters,
    ).is_ok());
}

#[test]
fn test_banned_user_removed_from_members() {
    let parameters = create_test_parameters();
    let mut initial_state = ChatRoomState::default();

    // Add a member to the initial state
    let member_to_ban = AuthorizedMember {
        member: Member {
            public_key: parameters.owner,
            nickname: "Alice".to_string(),
        },
        invited_by: parameters.owner,
        signature: Signature::from_bytes(&[0; 64]),
    };
    initial_state.members.insert(member_to_ban.clone());

    // Create a ban for this member
    let ban = AuthorizedUserBan {
        ban: UserBan {
            banned_user: member_to_ban.member.id(),
            banned_at: SystemTime::now(),
        },
        banned_by: parameters.owner,
        signature: Signature::from_bytes(&[0; 64]),
    };

    let delta = ChatRoomDelta {
        configuration: None,
        members: HashSet::new(),
        upgrade: None,
        recent_messages: Vec::new(),
        ban_log: vec![ban],
    };

    let result = test_apply_deltas(
        initial_state,
        vec![delta],
        |state: &ChatRoomState| {
            state.members.is_empty() && state.ban_log.len() == 1
        },
        &parameters,
    );

    assert!(result.is_ok(), "Failed to apply ban delta: {:?}", result.err());
}

#[test]
fn test_member_added_by_non_member() {
    let parameters = create_test_parameters();
    let initial_state = ChatRoomState::default();

    let mut rng = thread_rng();
    let mut secret_key_bytes = [0u8; 32];
    rng.fill_bytes(&mut secret_key_bytes);
    let non_member_key = SigningKey::from_bytes(&secret_key_bytes.into());
    rng.fill_bytes(&mut secret_key_bytes);
    let new_member_key = SigningKey::from_bytes(&secret_key_bytes.into());
    let new_member = AuthorizedMember {
        member: Member {
            public_key: new_member_key.verifying_key(),
            nickname: "Eve".to_string(),
        },
        invited_by: non_member_key.verifying_key(),
        signature: Signature::from_bytes(&[0; 64]),
    };

    let delta = ChatRoomDelta {
        configuration: None,
        members: {
            let mut set = HashSet::new();
            set.insert(new_member);
            set
        },
        upgrade: None,
        recent_messages: Vec::new(),
        ban_log: Vec::new(),
    };

    let result = test_apply_deltas(
        initial_state,
        vec![delta],
        |state: &ChatRoomState| state.members.len() == 1,
        &parameters,
    );
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Invalid invitation chain"));
}

#[test]
fn test_message_added_by_non_member() {
    let parameters = create_test_parameters();
    let initial_state = ChatRoomState::default();

    let mut rng = thread_rng();
    let mut secret_key_bytes = [0u8; 32];
    rng.fill_bytes(&mut secret_key_bytes);
    let non_member_key = SigningKey::from_bytes(&secret_key_bytes.into());
    let message = AuthorizedMessage {
        time: SystemTime::UNIX_EPOCH,
        content: "Hello from non-member".to_string(),
        author: MemberId(fast_hash(&non_member_key.verifying_key().to_bytes())),
        signature: Signature::from_bytes(&[0; 64]),
    };

    let delta = ChatRoomDelta {
        configuration: None,
        members: HashSet::new(),
        upgrade: None,
        recent_messages: vec![message],
        ban_log: Vec::new(),
    };

    let result = test_apply_deltas(
        initial_state,
        vec![delta],
        |state: &ChatRoomState| state.recent_messages.len() == 1,
        &parameters,
    );
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Messages from non-members are present"));
}

#[test]
fn test_message_added_by_existing_member() {
    let parameters = create_test_parameters();
    let mut initial_state = ChatRoomState::default();

    let mut rng = thread_rng();
    let mut secret_key_bytes = [0u8; 32];
    rng.fill_bytes(&mut secret_key_bytes);
    let existing_member_key = SigningKey::from_bytes(&secret_key_bytes.into());
    let existing_member = AuthorizedMember {
        member: Member {
            public_key: existing_member_key.verifying_key(),
            nickname: "Alice".to_string(),
        },
        invited_by: parameters.owner,
        signature: Signature::from_bytes(&[0; 64]),
    };
    initial_state.members.insert(existing_member.clone());

    let message = AuthorizedMessage {
        time: SystemTime::UNIX_EPOCH,
        content: "Hello from Alice".to_string(),
        author: existing_member.member.id(),
        signature: Signature::from_bytes(&[0; 64]),
    };

    let delta = ChatRoomDelta {
        configuration: None,
        members: HashSet::new(),
        upgrade: None,
        recent_messages: vec![message],
        ban_log: Vec::new(),
    };

    assert!(test_apply_deltas(
        initial_state,
        vec![delta],
        |state: &ChatRoomState| state.recent_messages.len() == 1,
        &parameters,
    ).is_ok());
}

mod configuration_limits {
    use super::*;

    #[test]
    fn test_max_recent_messages_limit() {
        let parameters = create_test_parameters();
        let mut initial_state = ChatRoomState::default();
        initial_state.configuration.configuration.max_recent_messages = 5;

        let member = AuthorizedMember {
            member: Member {
                public_key: parameters.owner,
                nickname: "Alice".to_string(),
            },
            invited_by: parameters.owner,
            signature: Signature::from_bytes(&[0; 64]),
        };
        initial_state.members.insert(member.clone());

        let create_message = |content: &str| AuthorizedMessage {
            time: SystemTime::UNIX_EPOCH,
            content: content.to_string(),
            author: member.member.id(),
            signature: Signature::from_bytes(&[0; 64]),
        };

        let messages = (0..10).map(|i| create_message(&format!("Message {}", i))).collect::<Vec<_>>();

        let delta = ChatRoomDelta {
            configuration: None,
            members: HashSet::new(),
            upgrade: None,
            recent_messages: messages.clone(),
            ban_log: Vec::new(),
        };

        println!("Initial state max_recent_messages: {}", initial_state.configuration.configuration.max_recent_messages);
        println!("Delta messages count: {}", delta.recent_messages.len());

        let result = test_apply_deltas(
            initial_state.clone(),
            vec![delta.clone()],
            |state: &ChatRoomState| {
                println!("Final state max_recent_messages: {}", state.configuration.configuration.max_recent_messages);
                println!("Recent messages: {:?}", state.recent_messages);
                println!("Recent messages count: {}", state.recent_messages.len());
                
                let condition_met = state.recent_messages.len() == 5 && 
                    state.recent_messages.iter().all(|m| m.content.starts_with("Message ")) &&
                    state.recent_messages.iter().any(|m| m.content == "Message 9") &&
                    state.recent_messages.iter().all(|m| m.content != "Message 4");
                
                println!("Condition met: {}", condition_met);
                condition_met
            },
            &parameters,
        );

        // If the test fails, print more detailed information
        if result.is_err() {
            println!("Test failed. Debugging information:");
            println!("Initial state: {:?}", initial_state);
            println!("Delta: {:?}", delta);
            
            // Manually apply the delta to see what's happening
            let mut debug_state = initial_state.clone();
            match debug_state.apply_delta(&delta, &parameters) {
                Ok(_) => {
                    println!("Manual delta application succeeded");
                    println!("Resulting state: {:?}", debug_state);
                },
                Err(e) => println!("Manual delta application failed: {:?}", e),
            }
        }

        // If the test fails, print more detailed information
        if result.is_err() {
            println!("Test failed. Debugging information:");
            println!("Initial state: {:?}", initial_state);
            println!("Delta: {:?}", delta);
            
            // Manually apply the delta to see what's happening
            let mut debug_state = initial_state.clone();
            match debug_state.apply_delta(&delta, &parameters) {
                Ok(_) => {
                    println!("Manual delta application succeeded");
                    println!("Resulting state: {:?}", debug_state);
                },
                Err(e) => println!("Manual delta application failed: {:?}", e),
            }
        }

        assert!(result.is_ok(), "Failed to apply delta: {:?}", result.err());
    }

    #[test]
    fn test_max_user_bans_limit() {
        let parameters = create_test_parameters();
        let mut initial_state = ChatRoomState::default();
        initial_state.configuration.configuration.max_user_bans = 3;

        let create_ban = |user_id: i32| AuthorizedUserBan {
            ban: UserBan {
                banned_user: MemberId(user_id),
                banned_at: SystemTime::now(),
            },
            banned_by: parameters.owner,
            signature: Signature::from_bytes(&[0; 64]),
        };

        let bans = (0..5).map(|i| create_ban(i)).collect::<Vec<_>>();

        let delta = ChatRoomDelta {
            configuration: None,
            members: HashSet::new(),
            upgrade: None,
            recent_messages: Vec::new(),
            ban_log: bans,
        };

        let result = test_apply_deltas(
            initial_state,
            vec![delta],
            |state: &ChatRoomState| {
                state.ban_log.len() == 3 &&
                state.ban_log.iter().all(|b| b.ban.banned_user.0 < 3)
            },
            &parameters,
        );

        assert!(result.is_ok(), "Failed to apply delta: {:?}", result.err());
    }
}
