use crate::state::*;
use super::test_utils::*;

#[test]
fn test_add_member() {
    let parameters = create_test_parameters();
    let mut state = create_sample_state();

    let new_member = create_sample_member("Alice", [1; 32], parameters.owner);

    let delta = ChatRoomDelta {
        configuration: None,
        members: {
            let mut set = HashSet::new();
            set.insert(new_member.clone());
            set
        },
        upgrade: None,
        recent_messages: Vec::new(),
        ban_log: Vec::new(),
    };

    state.apply_delta(delta, &parameters).unwrap();

    assert!(state.members.contains(&new_member));
}

#[test]
fn test_remove_member() {
    let parameters = create_test_parameters();
    let mut state = create_sample_state();

    let member = create_sample_member("Alice", [1; 32], parameters.owner);
    state.members.insert(member.clone());

    let delta = ChatRoomDelta {
        configuration: None,
        members: HashSet::new(),
        upgrade: None,
        recent_messages: Vec::new(),
        ban_log: Vec::new(),
    };

    state.apply_delta(delta, &parameters).unwrap();

    assert!(!state.members.contains(&member));
}
use crate::state::*;
use super::test_utils::*;

#[test]
fn test_add_member() {
    let parameters = create_test_parameters();
    let mut state = create_sample_state();

    let new_member = create_sample_member("Alice", [1; 32], parameters.owner);

    let delta = ChatRoomDelta {
        configuration: None,
        members: {
            let mut set = HashSet::new();
            set.insert(new_member.clone());
            set
        },
        upgrade: None,
        recent_messages: Vec::new(),
        ban_log: Vec::new(),
    };

    state.apply_delta(delta, &parameters).unwrap();

    assert!(state.members.contains(&new_member));
}

#[test]
fn test_remove_member() {
    let parameters = create_test_parameters();
    let mut state = create_sample_state();

    let member = create_sample_member("Alice", [1; 32], parameters.owner);
    state.members.insert(member.clone());

    let delta = ChatRoomDelta {
        configuration: None,
        members: HashSet::new(),
        upgrade: None,
        recent_messages: Vec::new(),
        ban_log: Vec::new(),
    };

    state.apply_delta(delta, &parameters).unwrap();

    assert!(!state.members.contains(&member));
}
