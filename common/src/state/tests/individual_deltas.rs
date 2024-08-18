use super::*;
use crate::util::fast_hash;

#[test]
fn test_member_added_by_owner() {
    let parameters = create_test_parameters();
    let initial_state = ChatRoomState::default();

    let new_member = AuthorizedMember {
        member: Member {
            public_key: VerifyingKey::from_bytes(&[1; 32]).unwrap(),
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

    test_apply_deltas(
        initial_state,
        vec![delta],
        |state: &ChatRoomState| state.members.len() == 1,
        &parameters,
    );
}

#[test]
fn test_member_added_by_existing_member() {
    let parameters = create_test_parameters();
    let mut initial_state = ChatRoomState::default();

    let existing_member = AuthorizedMember {
        member: Member {
            public_key: VerifyingKey::from_bytes(&[1; 32]).expect("Invalid key"),
            nickname: "Alice".to_string(),
        },
        invited_by: parameters.owner,
        signature: Signature::from_bytes(&[0; 64]),
    };
    initial_state.members.insert(existing_member.clone());

    let new_member = AuthorizedMember {
        member: Member {
            public_key: VerifyingKey::from_bytes(&[2; 32]).expect("Invalid key"),
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

    test_apply_deltas(
        initial_state,
        vec![delta],
        |state: &ChatRoomState| state.members.len() == 2,
        &parameters,
    );
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

    let delta = ChatRoomDelta {
        configuration: None,
        members: HashSet::new(),
        upgrade: None,
        recent_messages: vec![message],
        ban_log: Vec::new(),
    };

    test_apply_deltas(
        initial_state,
        vec![delta],
        |state: &ChatRoomState| state.recent_messages.len() == 1,
        &parameters,
    );
}

#[test]
fn test_message_added_by_existing_member() {
    let parameters = create_test_parameters();
    let mut initial_state = ChatRoomState::default();

    let existing_member = AuthorizedMember {
        member: Member {
            public_key: VerifyingKey::from_bytes(&[1; 32]).unwrap(),
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

    test_apply_deltas(
        initial_state,
        vec![delta],
        |state: &ChatRoomState| state.recent_messages.len() == 1,
        &parameters,
    );
}
