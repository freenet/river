use crate::state::*;
use super::test_utils::*;

#[test]
fn test_add_message() {
    let parameters = create_test_parameters();
    let mut state = create_sample_state();

    let member = create_sample_member("Alice", [1; 32], parameters.owner);
    state.members.insert(member.clone());

    let new_message = AuthorizedMessage {
        time: SystemTime::UNIX_EPOCH,
        content: "Hello, world!".to_string(),
        author: member.member.id(),
        signature: Signature::from_bytes(&[5; 64]).unwrap(),
    };

    let delta = ChatRoomDelta {
        configuration: None,
        members: HashSet::new(),
        upgrade: None,
        recent_messages: vec![new_message.clone()],
        ban_log: Vec::new(),
    };

    state.apply_delta(delta, &parameters).unwrap();

    assert!(state.recent_messages.contains(&new_message));
}

#[test]
fn test_message_from_non_member() {
    let parameters = create_test_parameters();
    let mut state = create_sample_state();

    let non_member = create_sample_member("Eve", [2; 32], parameters.owner);

    let new_message = AuthorizedMessage {
        time: SystemTime::UNIX_EPOCH,
        content: "Hello, world!".to_string(),
        author: non_member.member.id(),
        signature: Signature::from_bytes(&[5; 64]).unwrap(),
    };

    let delta = ChatRoomDelta {
        configuration: None,
        members: HashSet::new(),
        upgrade: None,
        recent_messages: vec![new_message.clone()],
        ban_log: Vec::new(),
    };

    assert!(state.apply_delta(delta, &parameters).is_err());
}
