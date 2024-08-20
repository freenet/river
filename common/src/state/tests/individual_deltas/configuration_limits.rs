use super::*;
/*
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
*/
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
