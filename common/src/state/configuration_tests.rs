use super::*;
use test_utils::*;

#[test]
fn test_configuration_update() {
    let parameters = create_test_parameters();
    let mut state = create_sample_state();

    let new_config = AuthorizedConfiguration {
        configuration: Configuration {
            configuration_version: 2,
            name: "Updated Room".to_string(),
            max_recent_messages: 150,
            max_user_bans: 15,
        },
        signature: Signature::from_bytes(&[1; 64]),
    };

    let delta = ChatRoomDelta {
        configuration: Some(new_config.clone()),
        members: HashSet::new(),
        upgrade: None,
        recent_messages: Vec::new(),
        ban_log: Vec::new(),
    };

    state.apply_delta(delta, &parameters).unwrap();

    assert_eq!(state.configuration, new_config);
}

#[test]
fn test_configuration_version_conflict() {
    let parameters = create_test_parameters();
    let mut state = create_sample_state();

    let old_config = AuthorizedConfiguration {
        configuration: Configuration {
            configuration_version: 1,
            name: "Old Room".to_string(),
            max_recent_messages: 50,
            max_user_bans: 5,
        },
        signature: Signature::from_bytes(&[1; 64]),
    };

    let delta = ChatRoomDelta {
        configuration: Some(old_config),
        members: HashSet::new(),
        upgrade: None,
        recent_messages: Vec::new(),
        ban_log: Vec::new(),
    };

    assert!(state.apply_delta(delta, &parameters).is_err());
}
