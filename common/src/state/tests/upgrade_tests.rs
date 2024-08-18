use crate::state::*;
use super::test_utils::*;

#[test]
fn test_apply_upgrade() {
    let parameters = create_test_parameters();
    let mut state = create_sample_state();

    let upgrade = AuthorizedUpgrade {
        upgrade: Upgrade {
            version: 1,
            url: "https://example.com/upgrade".to_string(),
        },
        signature: Signature::from_bytes(&[5; 64]).unwrap(),
    };

    let delta = ChatRoomDelta {
        configuration: None,
        members: HashSet::new(),
        upgrade: Some(upgrade.clone()),
        recent_messages: Vec::new(),
        ban_log: Vec::new(),
    };

    state.apply_delta(delta, &parameters).unwrap();

    assert_eq!(state.upgrade, Some(upgrade));
}

#[test]
fn test_upgrade_version_conflict() {
    let parameters = create_test_parameters();
    let mut state = create_sample_state();

    let upgrade1 = AuthorizedUpgrade {
        upgrade: Upgrade {
            version: 1,
            url: "https://example.com/upgrade1".to_string(),
        },
        signature: Signature::from_bytes(&[5; 64]).unwrap(),
    };

    let upgrade2 = AuthorizedUpgrade {
        upgrade: Upgrade {
            version: 1,
            url: "https://example.com/upgrade2".to_string(),
        },
        signature: Signature::from_bytes(&[6; 64]).unwrap(),
    };

    let delta1 = ChatRoomDelta {
        configuration: None,
        members: HashSet::new(),
        upgrade: Some(upgrade1),
        recent_messages: Vec::new(),
        ban_log: Vec::new(),
    };

    let delta2 = ChatRoomDelta {
        configuration: None,
        members: HashSet::new(),
        upgrade: Some(upgrade2),
        recent_messages: Vec::new(),
        ban_log: Vec::new(),
    };

    state.apply_delta(delta1, &parameters).unwrap();
    assert!(state.apply_delta(delta2, &parameters).is_err());
}
use crate::state::*;
use super::test_utils::*;

#[test]
fn test_apply_upgrade() {
    let parameters = create_test_parameters();
    let mut state = create_sample_state();

    let upgrade = AuthorizedUpgrade {
        upgrade: Upgrade {
            version: 1,
            url: "https://example.com/upgrade".to_string(),
        },
        signature: Signature::from_bytes(&[5; 64]).unwrap(),
    };

    let delta = ChatRoomDelta {
        configuration: None,
        members: HashSet::new(),
        upgrade: Some(upgrade.clone()),
        recent_messages: Vec::new(),
        ban_log: Vec::new(),
    };

    state.apply_delta(delta, &parameters).unwrap();

    assert_eq!(state.upgrade, Some(upgrade));
}

#[test]
fn test_upgrade_version_conflict() {
    let parameters = create_test_parameters();
    let mut state = create_sample_state();

    let upgrade1 = AuthorizedUpgrade {
        upgrade: Upgrade {
            version: 1,
            url: "https://example.com/upgrade1".to_string(),
        },
        signature: Signature::from_bytes(&[5; 64]).unwrap(),
    };

    let upgrade2 = AuthorizedUpgrade {
        upgrade: Upgrade {
            version: 1,
            url: "https://example.com/upgrade2".to_string(),
        },
        signature: Signature::from_bytes(&[6; 64]).unwrap(),
    };

    let delta1 = ChatRoomDelta {
        configuration: None,
        members: HashSet::new(),
        upgrade: Some(upgrade1),
        recent_messages: Vec::new(),
        ban_log: Vec::new(),
    };

    let delta2 = ChatRoomDelta {
        configuration: None,
        members: HashSet::new(),
        upgrade: Some(upgrade2),
        recent_messages: Vec::new(),
        ban_log: Vec::new(),
    };

    state.apply_delta(delta1, &parameters).unwrap();
    assert!(state.apply_delta(delta2, &parameters).is_err());
}
