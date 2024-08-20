
use crate::{ChatRoomState, ChatRoomDelta};
use crate::state::tests::{create_test_parameters, test_apply_deltas};
use crate::state::configuration::{AuthorizedConfiguration, Configuration};
use ed25519_dalek::SigningKey;
use std::collections::HashSet;
#[test]
fn test_max_user_bans_limit() {
    let parameters = create_test_parameters();
    let initial_state = ChatRoomState::default();
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

#[test]
fn test_max_nickname_size_limit() {
    let parameters = create_test_parameters();
    let mut initial_state = ChatRoomState::default();
    
    // Set max_nickname_size to 10
    let new_config = AuthorizedConfiguration::new(
        Configuration {
            max_nickname_size: 10,
            ..initial_state.configuration.configuration.clone()
        },
        &SigningKey::from_bytes(&[0; 32])
    );
    
    let config_delta = ChatRoomDelta {
        configuration: Some(new_config),
        members: HashSet::new(),
        upgrade: None,
        recent_messages: Vec::new(),
        ban_log: Vec::new(),
    };
    
    // Apply the configuration change
    let result = test_apply_deltas(
        initial_state.clone(),
        vec![config_delta],
        |state: &ChatRoomState| {
            state.configuration.configuration.max_nickname_size == 10
        },
        &parameters,
    );
    assert!(result.is_ok(), "Failed to apply configuration delta: {:?}", result.err());
    
    // Now test adding members with different nickname sizes
    let valid_member = AuthorizedMember::new(
        Member {
            public_key: parameters.owner,
            nickname: "Valid".to_string(),
        },
        parameters.owner,
        &SigningKey::from_bytes(&[0; 32])
    );
    
    let invalid_member = AuthorizedMember::new(
        Member {
            public_key: parameters.owner,
            nickname: "TooLongNickname".to_string(),
        },
        parameters.owner,
        &SigningKey::from_bytes(&[0; 32])
    );
    
    let mut valid_members = HashSet::new();
    valid_members.insert(valid_member);
    
    let mut invalid_members = HashSet::new();
    invalid_members.insert(invalid_member);
    
    let valid_delta = ChatRoomDelta {
        configuration: None,
        members: valid_members,
        upgrade: None,
        recent_messages: Vec::new(),
        ban_log: Vec::new(),
    };
    
    let invalid_delta = ChatRoomDelta {
        configuration: None,
        members: invalid_members,
        upgrade: None,
        recent_messages: Vec::new(),
        ban_log: Vec::new(),
    };
    
    // Test adding a valid member
    let result = test_apply_deltas(
        initial_state.clone(),
        vec![valid_delta],
        |state: &ChatRoomState| {
            state.members.len() == 1 && state.members.iter().next().unwrap().member.nickname == "Valid"
        },
        &parameters,
    );
    assert!(result.is_ok(), "Failed to add valid member: {:?}", result.err());
    
    // Test adding an invalid member
    let result = test_apply_deltas(
        initial_state,
        vec![invalid_delta],
        |state: &ChatRoomState| {
            state.members.is_empty()
        },
        &parameters,
    );
    assert!(result.is_ok(), "Failed to reject invalid member: {:?}", result.err());
}
