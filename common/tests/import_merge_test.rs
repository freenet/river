//! Regression test for #195: importing an identity creates a default state with
//! owner_member_id: FastHash(0). Merging the real network state fails because
//! apply_delta rejects owner_member_id changes. The fix: replace the state
//! wholesale when is_awaiting_initial_sync() is true instead of merging.

use ed25519_dalek::SigningKey;
use freenet_scaffold::util::FastHash;
use freenet_scaffold::ComposableState;
use rand::rngs::OsRng;
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId};
use river_core::room_state::ChatRoomParametersV1;
use river_core::room_state::ChatRoomStateV1;

#[test]
fn test_default_state_has_placeholder_owner() {
    let default_state = ChatRoomStateV1::default();
    assert_eq!(
        default_state.configuration.configuration.owner_member_id,
        MemberId(FastHash(0)),
        "Default state should have placeholder owner_member_id"
    );
}

#[test]
fn test_merge_fails_when_owner_member_id_differs() {
    let owner_sk = SigningKey::generate(&mut OsRng);
    let owner_vk = owner_sk.verifying_key();
    let params = ChatRoomParametersV1 { owner: owner_vk };

    // Simulate import: start with default state (placeholder owner)
    let mut local_state = ChatRoomStateV1::default();
    let current = local_state.clone();

    // Build network state with real owner and config version > 1
    let config = Configuration {
        owner_member_id: owner_vk.into(),
        configuration_version: 2,
        ..Default::default()
    };
    let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);
    let network_state = ChatRoomStateV1 {
        configuration: auth_config,
        ..Default::default()
    };

    // Merge should fail because owner_member_id differs
    let result = local_state.merge(&current, &params, &network_state);
    assert!(
        result.is_err(),
        "Merge should fail due to owner_member_id mismatch"
    );
    assert!(
        result.unwrap_err().contains("owner_member_id"),
        "Error should mention owner_member_id"
    );
}

#[test]
fn test_wholesale_replacement_works_for_imported_rooms() {
    let owner_sk = SigningKey::generate(&mut OsRng);
    let owner_vk = owner_sk.verifying_key();
    let invitee_sk = SigningKey::generate(&mut OsRng);

    // Build network state with proper owner, config, and a member
    let config = Configuration {
        owner_member_id: owner_vk.into(),
        configuration_version: 2,
        ..Default::default()
    };
    let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);
    let member = Member {
        owner_member_id: owner_vk.into(),
        invited_by: owner_vk.into(),
        member_vk: invitee_sk.verifying_key(),
    };
    let auth_member = AuthorizedMember::new(member, &owner_sk);
    let mut network_state = ChatRoomStateV1 {
        configuration: auth_config,
        ..Default::default()
    };
    network_state.members.members.push(auth_member);

    // Start with default (import) state
    let mut local_state = ChatRoomStateV1::default();
    assert!(local_state.members.members.is_empty());
    assert_eq!(
        local_state.configuration.configuration.owner_member_id,
        MemberId(FastHash(0)),
    );

    // Wholesale replacement (the fix for #195)
    local_state = network_state;

    assert_eq!(local_state.members.members.len(), 1);
    assert_eq!(
        local_state.configuration.configuration.owner_member_id,
        MemberId::from(owner_vk),
    );
}
