
#[test]
fn test_member_added_by_owner() {
    let parameters = create_test_parameters();
    let initial_state = ChatRoomState::default();

    let mut rng = thread_rng();
    let new_member_key = SigningKey::generate(&mut rng);
    let new_member = AuthorizedMember {
        member: Member {
            public_key: new_member_key.verifying_key(),
            nickname: "Alice".to_string(),
        },
        invited_by: parameters.owner,
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
    let new_member_key = SigningKey::generate(&mut rng);
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
    let owner_signing_key = SigningKey::from_bytes(&[0; 32]);
    let owner_member = AuthorizedMember {
        member: Member {
            public_key: owner_signing_key.verifying_key(),
            nickname: "Owner".to_string(),
        },
        invited_by: owner_signing_key.verifying_key(),
        signature: Signature::from_bytes(&[0; 64]),
    };
    initial_state.members.insert(owner_member);
    let parameters = ChatRoomParameters {
        owner: owner_signing_key.verifying_key(),
    };

    let owner_signing_key = SigningKey::from_bytes(&[0; 32]);
    let owner_member_id = MemberId(crate::util::fast_hash(&parameters.owner.to_bytes()));
    let message = AuthorizedMessage::new(
        Message {
            time: SystemTime::UNIX_EPOCH,
            content: "Hello from owner".to_string(),
        },
        owner_member_id,
        &owner_signing_key
    );

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
    assert!(result.is_ok(), "Failed to apply delta: {:?}", result.err());
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
    let non_member_key = SigningKey::generate(&mut rng);
    let new_member_key = SigningKey::generate(&mut rng);
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
    let non_member_key = SigningKey::generate(&mut rng);
    let message = AuthorizedMessage::new(
        Message {
            time: SystemTime::UNIX_EPOCH,
            content: "Hello from non-member".to_string(),
        },
        MemberId(fast_hash(&non_member_key.verifying_key().to_bytes())),
        &non_member_key
    );

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
    let existing_member_key = SigningKey::generate(&mut rng);
    let existing_member = AuthorizedMember {
        member: Member {
            public_key: existing_member_key.verifying_key(),
            nickname: "Alice".to_string(),
        },
        invited_by: parameters.owner,
        signature: Signature::from_bytes(&[0; 64]),
    };
    initial_state.members.insert(existing_member.clone());

    let message = AuthorizedMessage::new(
        Message {
            time: SystemTime::UNIX_EPOCH,
            content: "Hello from Alice".to_string(),
        },
        existing_member.member.id(),
        &existing_member_key
    );

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

#[test]
fn test_max_message_size() {
    let parameters = create_test_parameters();
    let mut initial_state = ChatRoomState::default();
    initial_state.configuration.configuration.max_message_size = 10;

    let mut rng = thread_rng();
    let existing_member_key = SigningKey::generate(&mut rng);
    let existing_member = AuthorizedMember {
        member: Member {
            public_key: existing_member_key.verifying_key(),
            nickname: "Alice".to_string(),
        },
        invited_by: parameters.owner,
        signature: Signature::from_bytes(&[0; 64]),
    };
    initial_state.members.insert(existing_member.clone());

    let short_message = AuthorizedMessage::new(
        Message {
            time: SystemTime::UNIX_EPOCH,
            content: "Short msg".to_string(),
        },
        existing_member.member.id(),
        &existing_member_key
    );

    let long_message = AuthorizedMessage::new(
        Message {
            time: SystemTime::UNIX_EPOCH,
            content: "This message is too long".to_string(),
        },
        existing_member.member.id(),
        &existing_member_key
    );

    let delta = ChatRoomDelta {
        configuration: None,
        members: HashSet::new(),
        upgrade: None,
        recent_messages: vec![short_message, long_message],
        ban_log: Vec::new(),
    };

    assert!(test_apply_deltas(
        initial_state,
        vec![delta],
        |state: &ChatRoomState| state.recent_messages.len() == 1 && state.recent_messages[0].message.content == "Short msg",
        &parameters,
    ).is_ok());
}

mod configuration_limits;

use std::collections::HashSet;
use std::time::SystemTime;
use ed25519_dalek::SigningKey;
use rand::thread_rng;
use crate::{ChatRoomDelta, ChatRoomState, ChatRoomParameters};
use crate::state::member::{AuthorizedMember, Member};
use crate::state::message::{AuthorizedMessage, Message};
use crate::state::ban::{AuthorizedUserBan, UserBan};
use crate::state::tests::{create_test_parameters, test_apply_deltas};
use crate::state::MemberId;
use crate::util::fast_hash;
