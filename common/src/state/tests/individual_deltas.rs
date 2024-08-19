use super::*;
use crate::util::fast_hash;
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

    let delta = create_delta(None, Some(vec![message]));

    test_apply_deltas(
        initial_state,
        vec![delta],
        |state: &ChatRoomState| state.recent_messages.len() == 1,
        &parameters,
    );
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

    test_apply_deltas(
        initial_state,
        vec![delta],
        |state: &ChatRoomState| state.recent_messages.len() == 1,
        &parameters,
    );
}
