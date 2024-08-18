use crate::state::*;
use super::test_utils::*;

#[test]
fn test_ban_user() {
    let parameters = create_test_parameters();
    let mut state = create_sample_state();

    let member = create_sample_member("Alice", [1; 32], parameters.owner);
    state.members.insert(member.clone());

    let ban = AuthorizedUserBan {
        ban: UserBan {
            banned_user: member.member.id(),
            banned_at: SystemTime::UNIX_EPOCH,
            reason: "Violation of rules".to_string(),
        },
        banned_by: parameters.owner,
        signature: Signature::from_bytes(&[5; 64]).unwrap(),
    };

    let delta = ChatRoomDelta {
        configuration: None,
        members: HashSet::new(),
        upgrade: None,
        recent_messages: Vec::new(),
        ban_log: vec![ban.clone()],
    };

    state.apply_delta(delta, &parameters).unwrap();

    assert!(state.ban_log.contains(&ban));
    assert!(!state.members.contains(&member));
}

#[test]
fn test_ban_non_member() {
    let parameters = create_test_parameters();
    let mut state = create_sample_state();

    let non_member = create_sample_member("Eve", [2; 32], parameters.owner);

    let ban = AuthorizedUserBan {
        ban: UserBan {
            banned_user: non_member.member.id(),
            banned_at: SystemTime::UNIX_EPOCH,
            reason: "Violation of rules".to_string(),
        },
        banned_by: parameters.owner,
        signature: Signature::from_bytes(&[5; 64]).unwrap(),
    };

    let delta = ChatRoomDelta {
        configuration: None,
        members: HashSet::new(),
        upgrade: None,
        recent_messages: Vec::new(),
        ban_log: vec![ban.clone()],
    };

    assert!(state.apply_delta(delta, &parameters).is_err());
}
