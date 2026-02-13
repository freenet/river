//! Convergence tests for River contract state
//!
//! These tests verify that CRDT operations produce the same result regardless
//! of the order in which deltas are applied. For a proper CRDT implementation:
//! - Commutativity: apply(delta_a, apply(delta_b, state)) == apply(delta_b, apply(delta_a, state))
//! - Idempotency: apply(delta, apply(delta, state)) == apply(delta, state)
//!
//! Non-convergence bugs occur when:
//! - Order-dependent truncation (first N items from unordered iteration)
//! - Tie-breaking without deterministic secondary sort keys
//! - Using non-deterministic data structures for selection

use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use rand::rngs::OsRng;
use river_core::room_state::ban::{AuthorizedUserBan, BansV1, UserBan};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersDelta, MembersV1};
use river_core::room_state::message::{
    AuthorizedMessageV1, MessageId, MessageV1, MessagesV1, RoomMessageBody,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1};
use std::time::SystemTime;

/// Helper to create a test member that's invited by a given inviter
fn create_test_member(owner_id: MemberId, invited_by: MemberId) -> (Member, SigningKey) {
    let signing_key = SigningKey::generate(&mut OsRng);
    let verifying_key = signing_key.verifying_key();
    let member = Member {
        owner_member_id: owner_id,
        invited_by,
        member_vk: verifying_key,
    };
    (member, signing_key)
}

/// Helper to create a properly signed authorized member
fn create_authorized_member(member: Member, inviter_signing_key: &SigningKey) -> AuthorizedMember {
    AuthorizedMember::new(member, inviter_signing_key)
}

// =============================================================================
// MEMBER TRUNCATION CONVERGENCE TEST
// =============================================================================
//
// BUG LOCATION: member.rs:144-159
//
// The current implementation processes delta.added in iteration order:
//
//     for member in &delta.added {
//         if self.members.len() < max_members {
//             self.members.push(member.clone());
//         } else {
//             break;
//         }
//     }
//
// This means if we have capacity for 1 more member and receive delta [A, B]:
// - We add A, then break because we're at capacity
//
// If another peer receives delta [B, A]:
// - They add B, then break because they're at capacity
//
// Result: Different final states depending on delta order.
//
// FIX: Sort delta.added by MemberId before processing

#[test]
fn test_member_add_order_convergence() {
    // Create owner
    let owner_signing_key = SigningKey::generate(&mut OsRng);
    let owner_verifying_key = owner_signing_key.verifying_key();
    let owner_id: MemberId = owner_verifying_key.into();

    // Create two members to be added
    let (member_a, _) = create_test_member(owner_id, owner_id);
    let (member_b, _) = create_test_member(owner_id, owner_id);

    let auth_member_a = create_authorized_member(member_a.clone(), &owner_signing_key);
    let auth_member_b = create_authorized_member(member_b.clone(), &owner_signing_key);

    // Create parent state with max_members = 1 (only room for one new member)
    let mut parent_state = ChatRoomStateV1::default();
    parent_state.configuration.configuration.max_members = 1;

    let parameters = ChatRoomParametersV1 {
        owner: owner_verifying_key,
    };

    // State A: Apply delta with order [member_a, member_b]
    let mut state_a = MembersV1::default();
    let delta_ab = MembersDelta::new(vec![auth_member_a.clone(), auth_member_b.clone()]);
    state_a
        .apply_delta(&parent_state, &parameters, &Some(delta_ab))
        .expect("apply_delta should succeed");

    // State B: Apply delta with order [member_b, member_a]
    let mut state_b = MembersV1::default();
    let delta_ba = MembersDelta::new(vec![auth_member_b.clone(), auth_member_a.clone()]);
    state_b
        .apply_delta(&parent_state, &parameters, &Some(delta_ba))
        .expect("apply_delta should succeed");

    // Both states should have exactly 1 member (due to max_members limit)
    assert_eq!(state_a.members.len(), 1, "State A should have 1 member");
    assert_eq!(state_b.members.len(), 1, "State B should have 1 member");

    // CONVERGENCE CHECK: Both states should have the SAME member
    // If this fails, it proves non-convergence due to order-dependent truncation
    let member_a_in_state_a = state_a
        .members
        .iter()
        .any(|m| m.member.id() == member_a.id());
    let member_b_in_state_a = state_a
        .members
        .iter()
        .any(|m| m.member.id() == member_b.id());
    let member_a_in_state_b = state_b
        .members
        .iter()
        .any(|m| m.member.id() == member_a.id());
    let member_b_in_state_b = state_b
        .members
        .iter()
        .any(|m| m.member.id() == member_b.id());

    // For convergence, the same member should be in both states
    assert_eq!(
        member_a_in_state_a, member_a_in_state_b,
        "Member A presence should be the same in both states. \
         State A has member A: {}, State B has member A: {}",
        member_a_in_state_a, member_a_in_state_b
    );
    assert_eq!(
        member_b_in_state_a, member_b_in_state_b,
        "Member B presence should be the same in both states. \
         State A has member B: {}, State B has member B: {}",
        member_b_in_state_a, member_b_in_state_b
    );

    // The actual convergence assertion
    assert_eq!(
        state_a
            .members
            .iter()
            .map(|m| m.member.id())
            .collect::<Vec<_>>(),
        state_b
            .members
            .iter()
            .map(|m| m.member.id())
            .collect::<Vec<_>>(),
        "CONVERGENCE FAILURE: Different delta orders produced different final states!\n\
         State A members: {:?}\n\
         State B members: {:?}",
        state_a
            .members
            .iter()
            .map(|m| m.member.id())
            .collect::<Vec<_>>(),
        state_b
            .members
            .iter()
            .map(|m| m.member.id())
            .collect::<Vec<_>>()
    );
}

// =============================================================================
// MEMBER EXCESS REMOVAL CONVERGENCE TEST
// =============================================================================
//
// BUG LOCATION: member.rs:256-267
//
// The current implementation uses max_by_key to find the member with the longest
// invite chain to remove:
//
//     let member_to_remove = self.members.iter()
//         .max_by_key(|m| self.get_invite_chain(m, parameters).unwrap().len())
//         .unwrap().member.id();
//
// When multiple members have the same invite chain length, max_by_key returns
// an arbitrary one (the last one encountered during iteration).
//
// FIX: Add secondary sort by member ID for deterministic tie-breaking

#[test]
fn test_member_removal_tiebreak_convergence() {
    // Create owner
    let owner_signing_key = SigningKey::generate(&mut OsRng);
    let owner_verifying_key = owner_signing_key.verifying_key();
    let owner_id: MemberId = owner_verifying_key.into();

    // Create three members, all invited directly by owner (same invite chain length = 0)
    let (member_a, _) = create_test_member(owner_id, owner_id);
    let (member_b, _) = create_test_member(owner_id, owner_id);
    let (member_c, _) = create_test_member(owner_id, owner_id);

    let auth_member_a = create_authorized_member(member_a.clone(), &owner_signing_key);
    let auth_member_b = create_authorized_member(member_b.clone(), &owner_signing_key);
    let auth_member_c = create_authorized_member(member_c.clone(), &owner_signing_key);

    // Create parent state with max_members = 2 (need to remove 1 of 3)
    let mut parent_state = ChatRoomStateV1::default();
    parent_state.configuration.configuration.max_members = 2;

    let parameters = ChatRoomParametersV1 {
        owner: owner_verifying_key,
    };

    // State A: Members in order [A, B, C]
    let mut state_a = MembersV1 {
        members: vec![
            auth_member_a.clone(),
            auth_member_b.clone(),
            auth_member_c.clone(),
        ],
    };
    state_a
        .apply_delta(&parent_state, &parameters, &None)
        .expect("apply_delta should succeed");

    // State B: Members in order [C, B, A] (reversed)
    let mut state_b = MembersV1 {
        members: vec![
            auth_member_c.clone(),
            auth_member_b.clone(),
            auth_member_a.clone(),
        ],
    };
    state_b
        .apply_delta(&parent_state, &parameters, &None)
        .expect("apply_delta should succeed");

    // Both should have 2 members after removing excess
    assert_eq!(state_a.members.len(), 2, "State A should have 2 members");
    assert_eq!(state_b.members.len(), 2, "State B should have 2 members");

    // CONVERGENCE CHECK: Both states should have the SAME members
    let mut ids_a: Vec<_> = state_a.members.iter().map(|m| m.member.id()).collect();
    let mut ids_b: Vec<_> = state_b.members.iter().map(|m| m.member.id()).collect();
    ids_a.sort();
    ids_b.sort();

    assert_eq!(
        ids_a, ids_b,
        "CONVERGENCE FAILURE: Different iteration orders produced different member sets!\n\
         State A members: {:?}\n\
         State B members: {:?}\n\
         All members had the same invite chain length, so tie-breaking was needed.",
        ids_a, ids_b
    );
}

// =============================================================================
// BAN EXCESS ORDER CONVERGENCE TEST
// =============================================================================
//
// BUG LOCATION: ban.rs:199-205
//
// When bans exceed the maximum, the code removes the oldest bans:
//
//     let mut extra_bans_vec = self.0.clone();
//     extra_bans_vec.sort_by_key(|ban| ban.ban.banned_at);
//     extra_bans_vec.reverse();
//
//     for ban in extra_bans_vec.iter().take(extra_bans as usize) {
//         invalid_bans.insert(ban.id(), BanValidationError::ExceededMaximumBans);
//     }
//
// When multiple bans have the same timestamp, their relative order after sorting
// is undefined, leading to non-deterministic behavior.
//
// FIX: Add secondary sort by ban ID for deterministic ordering

#[test]
fn test_ban_excess_order_convergence() {
    // Create owner
    let owner_signing_key = SigningKey::generate(&mut OsRng);
    let owner_verifying_key = owner_signing_key.verifying_key();
    let owner_id: MemberId = owner_verifying_key.into();

    // Create members to ban
    let (member_a, _) = create_test_member(owner_id, owner_id);
    let (member_b, _) = create_test_member(owner_id, owner_id);
    let (member_c, _) = create_test_member(owner_id, owner_id);

    let auth_member_a = create_authorized_member(member_a.clone(), &owner_signing_key);
    let auth_member_b = create_authorized_member(member_b.clone(), &owner_signing_key);
    let auth_member_c = create_authorized_member(member_c.clone(), &owner_signing_key);

    // Use the SAME timestamp for all bans to trigger the bug
    let same_time = SystemTime::now();

    // Create three bans with identical timestamps
    let ban_a = AuthorizedUserBan::new(
        UserBan {
            owner_member_id: owner_id,
            banned_at: same_time,
            banned_user: member_a.id(),
        },
        owner_id,
        &owner_signing_key,
    );

    let ban_b = AuthorizedUserBan::new(
        UserBan {
            owner_member_id: owner_id,
            banned_at: same_time,
            banned_user: member_b.id(),
        },
        owner_id,
        &owner_signing_key,
    );

    let ban_c = AuthorizedUserBan::new(
        UserBan {
            owner_member_id: owner_id,
            banned_at: same_time,
            banned_user: member_c.id(),
        },
        owner_id,
        &owner_signing_key,
    );

    // Create parent state with max_user_bans = 2 (need to reject 1 of 3)
    let mut parent_state = ChatRoomStateV1::default();
    parent_state.configuration.configuration.max_user_bans = 2;
    parent_state.members = MembersV1 {
        members: vec![auth_member_a, auth_member_b, auth_member_c],
    };

    let parameters = ChatRoomParametersV1 {
        owner: owner_verifying_key,
    };

    // State A: Bans in order [A, B, C]
    let bans_a = BansV1(vec![ban_a.clone(), ban_b.clone(), ban_c.clone()]);

    // State B: Bans in order [C, B, A] (reversed)
    let bans_b = BansV1(vec![ban_c.clone(), ban_b.clone(), ban_a.clone()]);

    // Check which bans are considered invalid (excess) in each state
    // We can't directly call get_invalid_bans since it's private, but we can
    // test via verify() which internally calls it
    let result_a = bans_a.verify(&parent_state, &parameters);
    let result_b = bans_b.verify(&parent_state, &parameters);

    // Both should fail verification (too many bans)
    assert!(result_a.is_err(), "State A should fail verification");
    assert!(result_b.is_err(), "State B should fail verification");

    // The error messages should identify the SAME ban as excess
    // If they identify different bans, we have non-convergence
    let err_a = result_a.unwrap_err();
    let err_b = result_b.unwrap_err();

    // Extract the ban IDs from error messages for comparison
    // This is a heuristic check; a proper fix would expose the invalid ban set
    assert_eq!(
        err_a, err_b,
        "CONVERGENCE FAILURE: Different ban orders identified different excess bans!\n\
         Error A: {}\n\
         Error B: {}\n\
         All bans had the same timestamp, so tie-breaking was needed.",
        err_a, err_b
    );
}

// =============================================================================
// MESSAGE PRUNE ORDER CONVERGENCE TEST
// =============================================================================
//
// BUG LOCATION: message.rs:190-197
//
// Messages are sorted by time and oldest are removed when exceeding max:
//
//     self.messages.sort_by(|a, b| a.message.time.cmp(&b.message.time));
//
//     if self.messages.len() > max_recent_messages {
//         self.messages.drain(0..self.messages.len() - max_recent_messages);
//     }
//
// When multiple messages have the same timestamp, their relative order after
// sorting is undefined (sort_by is not stable in the presence of equal keys
// without additional tie-breaking).
//
// FIX: Add secondary sort by message ID for deterministic ordering

#[test]
fn test_message_prune_order_convergence() {
    // Create owner
    let owner_signing_key = SigningKey::generate(&mut OsRng);
    let owner_verifying_key = owner_signing_key.verifying_key();
    let owner_id: MemberId = owner_verifying_key.into();

    // Use the SAME timestamp for all messages to trigger the bug
    let same_time = SystemTime::now();

    // Create three messages with identical timestamps
    let msg_a = AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: owner_id,
            author: owner_id,
            time: same_time,
            content: RoomMessageBody::public("Message A".to_string()),
        },
        &owner_signing_key,
    );

    let msg_b = AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: owner_id,
            author: owner_id,
            time: same_time,
            content: RoomMessageBody::public("Message B".to_string()),
        },
        &owner_signing_key,
    );

    let msg_c = AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: owner_id,
            author: owner_id,
            time: same_time,
            content: RoomMessageBody::public("Message C".to_string()),
        },
        &owner_signing_key,
    );

    // Create parent state with max_recent_messages = 2 (need to remove 1 of 3)
    let mut parent_state = ChatRoomStateV1::default();
    parent_state.configuration.configuration.max_recent_messages = 2;
    parent_state.configuration.configuration.max_message_size = 1000;

    let parameters = ChatRoomParametersV1 {
        owner: owner_verifying_key,
    };

    // State A: Messages in order [A, B, C]
    let mut state_a = MessagesV1 {
        messages: vec![msg_a.clone(), msg_b.clone(), msg_c.clone()],
        ..Default::default()
    };
    state_a
        .apply_delta(&parent_state, &parameters, &None)
        .expect("apply_delta should succeed");

    // State B: Messages in order [C, B, A] (reversed)
    let mut state_b = MessagesV1 {
        messages: vec![msg_c.clone(), msg_b.clone(), msg_a.clone()],
        ..Default::default()
    };
    state_b
        .apply_delta(&parent_state, &parameters, &None)
        .expect("apply_delta should succeed");

    // Both should have 2 messages after pruning
    assert_eq!(state_a.messages.len(), 2, "State A should have 2 messages");
    assert_eq!(state_b.messages.len(), 2, "State B should have 2 messages");

    // CONVERGENCE CHECK: Both states should have the SAME messages
    let mut ids_a: Vec<_> = state_a.messages.iter().map(|m| m.id()).collect();
    let mut ids_b: Vec<_> = state_b.messages.iter().map(|m| m.id()).collect();
    ids_a.sort();
    ids_b.sort();

    assert_eq!(
        ids_a, ids_b,
        "CONVERGENCE FAILURE: Different message orders produced different message sets!\n\
         State A messages: {:?}\n\
         State B messages: {:?}\n\
         All messages had the same timestamp, so tie-breaking was needed.",
        ids_a, ids_b
    );
}

// =============================================================================
// ADDITIONAL CONVERGENCE TESTS
// =============================================================================

/// Test that applying the same delta multiple times is idempotent
#[test]
fn test_member_delta_idempotency() {
    let owner_signing_key = SigningKey::generate(&mut OsRng);
    let owner_verifying_key = owner_signing_key.verifying_key();
    let owner_id: MemberId = owner_verifying_key.into();

    let (member_a, _) = create_test_member(owner_id, owner_id);
    let auth_member_a = create_authorized_member(member_a.clone(), &owner_signing_key);

    let mut parent_state = ChatRoomStateV1::default();
    parent_state.configuration.configuration.max_members = 10;

    let parameters = ChatRoomParametersV1 {
        owner: owner_verifying_key,
    };

    let mut state = MembersV1::default();
    let delta = MembersDelta::new(vec![auth_member_a.clone()]);

    // Apply delta first time
    state
        .apply_delta(&parent_state, &parameters, &Some(delta.clone()))
        .expect("first apply_delta should succeed");

    let state_after_first = state.clone();

    // Apply same delta again
    state
        .apply_delta(&parent_state, &parameters, &Some(delta))
        .expect("second apply_delta should succeed");

    // State should be unchanged (idempotent)
    assert_eq!(
        state.members.len(),
        state_after_first.members.len(),
        "Applying delta twice should be idempotent"
    );
    assert_eq!(
        state
            .members
            .iter()
            .map(|m| m.member.id())
            .collect::<Vec<_>>(),
        state_after_first
            .members
            .iter()
            .map(|m| m.member.id())
            .collect::<Vec<_>>(),
        "Applying delta twice should produce identical state"
    );
}

/// Test that message delta application is idempotent
#[test]
fn test_message_delta_idempotency() {
    let owner_signing_key = SigningKey::generate(&mut OsRng);
    let owner_verifying_key = owner_signing_key.verifying_key();
    let owner_id: MemberId = owner_verifying_key.into();

    let msg = AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: owner_id,
            author: owner_id,
            time: SystemTime::now(),
            content: RoomMessageBody::public("Test message".to_string()),
        },
        &owner_signing_key,
    );

    let mut parent_state = ChatRoomStateV1::default();
    parent_state.configuration.configuration.max_recent_messages = 100;
    parent_state.configuration.configuration.max_message_size = 1000;

    let parameters = ChatRoomParametersV1 {
        owner: owner_verifying_key,
    };

    let mut state = MessagesV1::default();
    let delta = vec![msg.clone()];

    // Apply delta first time
    state
        .apply_delta(&parent_state, &parameters, &Some(delta.clone()))
        .expect("first apply_delta should succeed");

    let state_after_first = state.clone();

    // Apply same delta again
    state
        .apply_delta(&parent_state, &parameters, &Some(delta))
        .expect("second apply_delta should succeed");

    // State should be unchanged (idempotent)
    assert_eq!(
        state.messages.len(),
        state_after_first.messages.len(),
        "Applying delta twice should be idempotent"
    );
}

/// Test convergence with interleaved delta applications
#[test]
fn test_member_interleaved_deltas_convergence() {
    let owner_signing_key = SigningKey::generate(&mut OsRng);
    let owner_verifying_key = owner_signing_key.verifying_key();
    let owner_id: MemberId = owner_verifying_key.into();

    // Create four members
    let (member_a, _) = create_test_member(owner_id, owner_id);
    let (member_b, _) = create_test_member(owner_id, owner_id);
    let (member_c, _) = create_test_member(owner_id, owner_id);
    let (member_d, _) = create_test_member(owner_id, owner_id);

    let auth_a = create_authorized_member(member_a.clone(), &owner_signing_key);
    let auth_b = create_authorized_member(member_b.clone(), &owner_signing_key);
    let auth_c = create_authorized_member(member_c.clone(), &owner_signing_key);
    let auth_d = create_authorized_member(member_d.clone(), &owner_signing_key);

    let mut parent_state = ChatRoomStateV1::default();
    parent_state.configuration.configuration.max_members = 2;

    let parameters = ChatRoomParametersV1 {
        owner: owner_verifying_key,
    };

    // Peer 1: Apply [A, B] then [C, D]
    let mut state_1 = MembersV1::default();
    let delta_1 = MembersDelta::new(vec![auth_a.clone(), auth_b.clone()]);
    state_1
        .apply_delta(&parent_state, &parameters, &Some(delta_1))
        .expect("apply_delta should succeed");
    let delta_2 = MembersDelta::new(vec![auth_c.clone(), auth_d.clone()]);
    state_1
        .apply_delta(&parent_state, &parameters, &Some(delta_2))
        .expect("apply_delta should succeed");

    // Peer 2: Apply [C, D] then [A, B]
    let mut state_2 = MembersV1::default();
    let delta_1 = MembersDelta::new(vec![auth_c.clone(), auth_d.clone()]);
    state_2
        .apply_delta(&parent_state, &parameters, &Some(delta_1))
        .expect("apply_delta should succeed");
    let delta_2 = MembersDelta::new(vec![auth_a.clone(), auth_b.clone()]);
    state_2
        .apply_delta(&parent_state, &parameters, &Some(delta_2))
        .expect("apply_delta should succeed");

    // Both should converge to the same state
    let mut ids_1: Vec<_> = state_1.members.iter().map(|m| m.member.id()).collect();
    let mut ids_2: Vec<_> = state_2.members.iter().map(|m| m.member.id()).collect();
    ids_1.sort();
    ids_2.sort();

    assert_eq!(
        ids_1, ids_2,
        "CONVERGENCE FAILURE: Different delta application orders produced different states!\n\
         Peer 1 applied [A,B] then [C,D]: {:?}\n\
         Peer 2 applied [C,D] then [A,B]: {:?}",
        ids_1, ids_2
    );
}

// =============================================================================
// STRESS TESTS WITH REALISTIC SIZES
// =============================================================================

/// Stress test: 50+ members with various invite chain depths
/// Tests that convergence holds under realistic member counts
#[test]
fn test_member_convergence_stress_50_members() {
    let owner_signing_key = SigningKey::generate(&mut OsRng);
    let owner_verifying_key = owner_signing_key.verifying_key();
    let owner_id: MemberId = owner_verifying_key.into();

    // Create a hierarchy of members with varying invite chain depths
    // Level 0: members invited directly by owner
    // Level 1: members invited by level 0
    // Level 2: members invited by level 1
    // etc.

    let mut all_members: Vec<(AuthorizedMember, SigningKey)> = Vec::new();
    let mut level_0_members: Vec<(AuthorizedMember, SigningKey)> = Vec::new();

    // Create 15 level-0 members (invited by owner)
    for _ in 0..15 {
        let (member, signing_key) = create_test_member(owner_id, owner_id);
        let auth_member = create_authorized_member(member, &owner_signing_key);
        level_0_members.push((auth_member.clone(), signing_key.clone()));
        all_members.push((auth_member, signing_key));
    }

    // Create 20 level-1 members (invited by level-0 members)
    let mut level_1_members: Vec<(AuthorizedMember, SigningKey)> = Vec::new();
    for i in 0..20 {
        let inviter_idx = i % level_0_members.len();
        let inviter = &level_0_members[inviter_idx];
        let (member, signing_key) = create_test_member(owner_id, inviter.0.member.id());
        let auth_member = create_authorized_member(member, &inviter.1);
        level_1_members.push((auth_member.clone(), signing_key.clone()));
        all_members.push((auth_member, signing_key));
    }

    // Create 15 level-2 members (invited by level-1 members)
    for i in 0..15 {
        let inviter_idx = i % level_1_members.len();
        let inviter = &level_1_members[inviter_idx];
        let (member, signing_key) = create_test_member(owner_id, inviter.0.member.id());
        let auth_member = create_authorized_member(member, &inviter.1);
        all_members.push((auth_member, signing_key));
    }

    assert_eq!(all_members.len(), 50);

    // Set max_members to 30 (need to remove 20)
    let mut parent_state = ChatRoomStateV1::default();
    parent_state.configuration.configuration.max_members = 30;

    let parameters = ChatRoomParametersV1 {
        owner: owner_verifying_key,
    };

    // Create multiple permutations of the member list
    let auth_members: Vec<AuthorizedMember> = all_members.iter().map(|(m, _)| m.clone()).collect();

    // State A: Original order
    let mut state_a = MembersV1 {
        members: auth_members.clone(),
    };
    state_a
        .apply_delta(&parent_state, &parameters, &None)
        .expect("apply_delta should succeed");

    // State B: Reversed order
    let mut reversed = auth_members.clone();
    reversed.reverse();
    let mut state_b = MembersV1 { members: reversed };
    state_b
        .apply_delta(&parent_state, &parameters, &None)
        .expect("apply_delta should succeed");

    // State C: Shuffled order (rotate by 17)
    let mut rotated = auth_members.clone();
    rotated.rotate_left(17);
    let mut state_c = MembersV1 { members: rotated };
    state_c
        .apply_delta(&parent_state, &parameters, &None)
        .expect("apply_delta should succeed");

    // All states should have exactly 30 members
    assert_eq!(state_a.members.len(), 30, "State A should have 30 members");
    assert_eq!(state_b.members.len(), 30, "State B should have 30 members");
    assert_eq!(state_c.members.len(), 30, "State C should have 30 members");

    // All states should have the SAME members
    let mut ids_a: Vec<_> = state_a.members.iter().map(|m| m.member.id()).collect();
    let mut ids_b: Vec<_> = state_b.members.iter().map(|m| m.member.id()).collect();
    let mut ids_c: Vec<_> = state_c.members.iter().map(|m| m.member.id()).collect();
    ids_a.sort();
    ids_b.sort();
    ids_c.sort();

    assert_eq!(
        ids_a, ids_b,
        "CONVERGENCE FAILURE: State A and B have different members"
    );
    assert_eq!(
        ids_b, ids_c,
        "CONVERGENCE FAILURE: State B and C have different members"
    );
}

/// Stress test: 100+ messages with mixed timestamps
#[test]
fn test_message_convergence_stress_100_messages() {
    let owner_signing_key = SigningKey::generate(&mut OsRng);
    let owner_verifying_key = owner_signing_key.verifying_key();
    let owner_id: MemberId = owner_verifying_key.into();

    let base_time = SystemTime::now();
    let mut messages: Vec<AuthorizedMessageV1> = Vec::new();

    // Create 100 messages with various timestamps
    // Some will have the same timestamp to test tie-breaking
    for i in 0..100 {
        // Create groups of messages with same timestamp
        let time_offset = (i / 5) as u64; // 5 messages per timestamp
        let time = base_time + std::time::Duration::from_secs(time_offset);

        let msg = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: owner_id,
                time,
                content: RoomMessageBody::public(format!("Message {}", i)),
            },
            &owner_signing_key,
        );
        messages.push(msg);
    }

    // Set max_recent_messages to 50
    let mut parent_state = ChatRoomStateV1::default();
    parent_state.configuration.configuration.max_recent_messages = 50;
    parent_state.configuration.configuration.max_message_size = 1000;

    let parameters = ChatRoomParametersV1 {
        owner: owner_verifying_key,
    };

    // State A: Original order
    let mut state_a = MessagesV1 {
        messages: messages.clone(),
        ..Default::default()
    };
    state_a
        .apply_delta(&parent_state, &parameters, &None)
        .expect("apply_delta should succeed");

    // State B: Reversed order
    let mut reversed = messages.clone();
    reversed.reverse();
    let mut state_b = MessagesV1 {
        messages: reversed,
        ..Default::default()
    };
    state_b
        .apply_delta(&parent_state, &parameters, &None)
        .expect("apply_delta should succeed");

    // State C: Shuffled (interleaved)
    let mut interleaved: Vec<AuthorizedMessageV1> = Vec::new();
    let half = messages.len() / 2;
    for i in 0..half {
        interleaved.push(messages[i].clone());
        interleaved.push(messages[half + i].clone());
    }
    let mut state_c = MessagesV1 {
        messages: interleaved,
        ..Default::default()
    };
    state_c
        .apply_delta(&parent_state, &parameters, &None)
        .expect("apply_delta should succeed");

    // All states should have exactly 50 messages
    assert_eq!(state_a.messages.len(), 50);
    assert_eq!(state_b.messages.len(), 50);
    assert_eq!(state_c.messages.len(), 50);

    // All states should have the SAME messages
    let ids_a: Vec<_> = state_a.messages.iter().map(|m| m.id()).collect();
    let ids_b: Vec<_> = state_b.messages.iter().map(|m| m.id()).collect();
    let ids_c: Vec<_> = state_c.messages.iter().map(|m| m.id()).collect();

    // Messages should be in the same order (sorted by time, then by id)
    assert_eq!(
        ids_a, ids_b,
        "CONVERGENCE FAILURE: State A and B have different message order"
    );
    assert_eq!(
        ids_b, ids_c,
        "CONVERGENCE FAILURE: State B and C have different message order"
    );
}

/// Stress test: Multiple bans with same timestamps
#[test]
fn test_ban_convergence_stress_same_timestamps() {
    use std::collections::HashSet;

    let owner_signing_key = SigningKey::generate(&mut OsRng);
    let owner_verifying_key = owner_signing_key.verifying_key();
    let owner_id: MemberId = owner_verifying_key.into();

    // Create 20 members
    let mut members: Vec<AuthorizedMember> = Vec::new();
    for _ in 0..20 {
        let (member, _) = create_test_member(owner_id, owner_id);
        let auth_member = create_authorized_member(member, &owner_signing_key);
        members.push(auth_member);
    }

    // Create 15 bans with the SAME timestamp
    let same_time = SystemTime::now();
    let mut bans: Vec<AuthorizedUserBan> = Vec::new();
    for member in members.iter().take(15) {
        let ban = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: same_time,
                banned_user: member.member.id(),
            },
            owner_id,
            &owner_signing_key,
        );
        bans.push(ban);
    }

    // Set max_user_bans to 10 (need to reject 5)
    let mut parent_state = ChatRoomStateV1::default();
    parent_state.configuration.configuration.max_user_bans = 10;
    parent_state.members = MembersV1 {
        members: members.clone(),
    };

    let parameters = ChatRoomParametersV1 {
        owner: owner_verifying_key,
    };

    // Helper to extract BanIds from error message
    fn extract_ban_ids(err: &str) -> HashSet<String> {
        // Error format: "Invalid bans: BanId(FastHash(123)): ..., BanId(FastHash(456)): ..."
        err.split("BanId(FastHash(")
            .skip(1)
            .filter_map(|s| s.split("))").next())
            .map(|s| s.to_string())
            .collect()
    }

    // State A: Original order
    let bans_a = BansV1(bans.clone());

    // State B: Reversed order
    let mut reversed = bans.clone();
    reversed.reverse();
    let bans_b = BansV1(reversed);

    // State C: Rotated order
    let mut rotated = bans.clone();
    rotated.rotate_left(7);
    let bans_c = BansV1(rotated);

    // All should fail verification identifying the same excess bans
    let err_a = bans_a.verify(&parent_state, &parameters).unwrap_err();
    let err_b = bans_b.verify(&parent_state, &parameters).unwrap_err();
    let err_c = bans_c.verify(&parent_state, &parameters).unwrap_err();

    // Extract the BanIds from each error (the error message order may vary due to HashMap)
    let ids_a = extract_ban_ids(&err_a);
    let ids_b = extract_ban_ids(&err_b);
    let ids_c = extract_ban_ids(&err_c);

    assert_eq!(
        ids_a, ids_b,
        "CONVERGENCE FAILURE: State A and B identified different excess bans"
    );
    assert_eq!(
        ids_b, ids_c,
        "CONVERGENCE FAILURE: State B and C identified different excess bans"
    );

    // Verify we identified exactly 5 excess bans
    assert_eq!(ids_a.len(), 5, "Should identify exactly 5 excess bans");
}

// =============================================================================
// PROPERTY-BASED STYLE TESTS
// =============================================================================

/// Property test: Any permutation of member additions should produce the same state
#[test]
fn test_member_permutation_convergence() {
    let owner_signing_key = SigningKey::generate(&mut OsRng);
    let owner_verifying_key = owner_signing_key.verifying_key();
    let owner_id: MemberId = owner_verifying_key.into();

    // Create 10 members
    let mut members: Vec<AuthorizedMember> = Vec::new();
    for _ in 0..10 {
        let (member, _) = create_test_member(owner_id, owner_id);
        let auth_member = create_authorized_member(member, &owner_signing_key);
        members.push(auth_member);
    }

    let mut parent_state = ChatRoomStateV1::default();
    parent_state.configuration.configuration.max_members = 5;

    let parameters = ChatRoomParametersV1 {
        owner: owner_verifying_key,
    };

    // Generate multiple permutations
    let permutations: Vec<Vec<AuthorizedMember>> = vec![
        members.clone(),
        members.iter().rev().cloned().collect(),
        {
            let mut p = members.clone();
            p.rotate_left(3);
            p
        },
        {
            let mut p = members.clone();
            p.rotate_right(5);
            p
        },
        {
            // Interleave odd and even indices
            let mut p: Vec<AuthorizedMember> = Vec::new();
            for i in (0..members.len()).step_by(2) {
                p.push(members[i].clone());
            }
            for i in (1..members.len()).step_by(2) {
                p.push(members[i].clone());
            }
            p
        },
    ];

    // Apply each permutation and collect resulting member IDs
    let mut results: Vec<Vec<MemberId>> = Vec::new();
    for perm in permutations {
        let mut state = MembersV1 { members: perm };
        state
            .apply_delta(&parent_state, &parameters, &None)
            .expect("apply_delta should succeed");
        let mut ids: Vec<_> = state.members.iter().map(|m| m.member.id()).collect();
        ids.sort();
        results.push(ids);
    }

    // All results should be identical
    let first = &results[0];
    for (i, result) in results.iter().enumerate().skip(1) {
        assert_eq!(
            first, result,
            "CONVERGENCE FAILURE: Permutation {} produced different result than permutation 0",
            i
        );
    }
}

/// Property test: Random operation sequences should converge
#[test]
fn test_random_operation_sequence_convergence() {
    use rand::seq::SliceRandom;
    use rand::SeedableRng;

    let owner_signing_key = SigningKey::generate(&mut OsRng);
    let owner_verifying_key = owner_signing_key.verifying_key();
    let owner_id: MemberId = owner_verifying_key.into();

    // Create a pool of members
    let mut member_pool: Vec<(AuthorizedMember, SigningKey)> = Vec::new();
    for _ in 0..20 {
        let (member, signing_key) = create_test_member(owner_id, owner_id);
        let auth_member = create_authorized_member(member, &owner_signing_key);
        member_pool.push((auth_member, signing_key));
    }

    let mut parent_state = ChatRoomStateV1::default();
    parent_state.configuration.configuration.max_members = 8;

    let parameters = ChatRoomParametersV1 {
        owner: owner_verifying_key,
    };

    // Create operation sequences (add members in different orders)
    let member_refs: Vec<&AuthorizedMember> = member_pool.iter().map(|(m, _)| m).collect();

    // Use a fixed seed for reproducibility
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);

    let mut sequences: Vec<Vec<AuthorizedMember>> = Vec::new();
    for _ in 0..5 {
        let mut seq: Vec<&AuthorizedMember> = member_refs.clone();
        seq.shuffle(&mut rng);
        sequences.push(seq.into_iter().cloned().collect());
    }

    // Apply each sequence via deltas
    let mut final_states: Vec<Vec<MemberId>> = Vec::new();
    for seq in sequences {
        let mut state = MembersV1::default();
        // Apply in batches of 4
        for chunk in seq.chunks(4) {
            let delta = MembersDelta::new(chunk.to_vec());
            state
                .apply_delta(&parent_state, &parameters, &Some(delta))
                .expect("apply_delta should succeed");
        }
        let mut ids: Vec<_> = state.members.iter().map(|m| m.member.id()).collect();
        ids.sort();
        final_states.push(ids);
    }

    // All final states should be identical
    let first = &final_states[0];
    for (i, state) in final_states.iter().enumerate().skip(1) {
        assert_eq!(
            first, state,
            "CONVERGENCE FAILURE: Sequence {} produced different final state",
            i
        );
    }
}

/// Property test: Messages with varying max limits should converge
#[test]
fn test_message_varying_limits_convergence() {
    let owner_signing_key = SigningKey::generate(&mut OsRng);
    let owner_verifying_key = owner_signing_key.verifying_key();
    let owner_id: MemberId = owner_verifying_key.into();

    // Create 30 messages with distinct timestamps
    let base_time = SystemTime::now();
    let mut messages: Vec<AuthorizedMessageV1> = Vec::new();
    for i in 0..30 {
        let msg = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: owner_id,
                time: base_time + std::time::Duration::from_secs(i as u64),
                content: RoomMessageBody::public(format!("Message {}", i)),
            },
            &owner_signing_key,
        );
        messages.push(msg);
    }

    let parameters = ChatRoomParametersV1 {
        owner: owner_verifying_key,
    };

    // Test with different max_recent_messages limits
    for max_messages in [5, 10, 15, 20, 25] {
        let mut parent_state = ChatRoomStateV1::default();
        parent_state.configuration.configuration.max_recent_messages = max_messages;
        parent_state.configuration.configuration.max_message_size = 1000;

        // Apply messages in different orders
        let mut state_forward = MessagesV1 {
            messages: messages.clone(),
            ..Default::default()
        };
        state_forward
            .apply_delta(&parent_state, &parameters, &None)
            .expect("apply_delta should succeed");

        let mut reversed = messages.clone();
        reversed.reverse();
        let mut state_backward = MessagesV1 {
            messages: reversed,
            ..Default::default()
        };
        state_backward
            .apply_delta(&parent_state, &parameters, &None)
            .expect("apply_delta should succeed");

        // Verify convergence
        assert_eq!(
            state_forward.messages.len(),
            max_messages,
            "Forward state should have {} messages",
            max_messages
        );
        assert_eq!(
            state_backward.messages.len(),
            max_messages,
            "Backward state should have {} messages",
            max_messages
        );

        let ids_forward: Vec<_> = state_forward.messages.iter().map(|m| m.id()).collect();
        let ids_backward: Vec<_> = state_backward.messages.iter().map(|m| m.id()).collect();

        assert_eq!(
            ids_forward, ids_backward,
            "CONVERGENCE FAILURE: Different orders with max_messages={} produced different states",
            max_messages
        );
    }
}

// =============================================================================
// EDGE CASE TESTS
// =============================================================================

/// Edge case: Exactly at capacity (max_members)
#[test]
fn test_member_exactly_at_capacity() {
    let owner_signing_key = SigningKey::generate(&mut OsRng);
    let owner_verifying_key = owner_signing_key.verifying_key();
    let owner_id: MemberId = owner_verifying_key.into();

    // Create exactly 5 members
    let mut members: Vec<AuthorizedMember> = Vec::new();
    for _ in 0..5 {
        let (member, _) = create_test_member(owner_id, owner_id);
        let auth_member = create_authorized_member(member, &owner_signing_key);
        members.push(auth_member);
    }

    let mut parent_state = ChatRoomStateV1::default();
    parent_state.configuration.configuration.max_members = 5;

    let parameters = ChatRoomParametersV1 {
        owner: owner_verifying_key,
    };

    // State A: Original order
    let mut state_a = MembersV1 {
        members: members.clone(),
    };
    state_a
        .apply_delta(&parent_state, &parameters, &None)
        .expect("apply_delta should succeed");

    // State B: Reversed order
    let mut reversed = members.clone();
    reversed.reverse();
    let mut state_b = MembersV1 { members: reversed };
    state_b
        .apply_delta(&parent_state, &parameters, &None)
        .expect("apply_delta should succeed");

    // Both should have exactly 5 members and the same members
    assert_eq!(state_a.members.len(), 5);
    assert_eq!(state_b.members.len(), 5);

    let mut ids_a: Vec<_> = state_a.members.iter().map(|m| m.member.id()).collect();
    let mut ids_b: Vec<_> = state_b.members.iter().map(|m| m.member.id()).collect();
    ids_a.sort();
    ids_b.sort();

    assert_eq!(ids_a, ids_b, "At capacity, all members should be preserved");
}

/// Edge case: One over capacity
#[test]
fn test_member_one_over_capacity() {
    let owner_signing_key = SigningKey::generate(&mut OsRng);
    let owner_verifying_key = owner_signing_key.verifying_key();
    let owner_id: MemberId = owner_verifying_key.into();

    // Create 6 members (one over capacity of 5)
    let mut members: Vec<AuthorizedMember> = Vec::new();
    for _ in 0..6 {
        let (member, _) = create_test_member(owner_id, owner_id);
        let auth_member = create_authorized_member(member, &owner_signing_key);
        members.push(auth_member);
    }

    let mut parent_state = ChatRoomStateV1::default();
    parent_state.configuration.configuration.max_members = 5;

    let parameters = ChatRoomParametersV1 {
        owner: owner_verifying_key,
    };

    // State A: Original order
    let mut state_a = MembersV1 {
        members: members.clone(),
    };
    state_a
        .apply_delta(&parent_state, &parameters, &None)
        .expect("apply_delta should succeed");

    // State B: Reversed order
    let mut reversed = members.clone();
    reversed.reverse();
    let mut state_b = MembersV1 { members: reversed };
    state_b
        .apply_delta(&parent_state, &parameters, &None)
        .expect("apply_delta should succeed");

    // Both should have exactly 5 members
    assert_eq!(state_a.members.len(), 5);
    assert_eq!(state_b.members.len(), 5);

    let mut ids_a: Vec<_> = state_a.members.iter().map(|m| m.member.id()).collect();
    let mut ids_b: Vec<_> = state_b.members.iter().map(|m| m.member.id()).collect();
    ids_a.sort();
    ids_b.sort();

    assert_eq!(
        ids_a, ids_b,
        "One over capacity: same member should be removed regardless of order"
    );
}

/// Edge case: All messages with identical timestamps
#[test]
fn test_messages_all_identical_timestamps() {
    let owner_signing_key = SigningKey::generate(&mut OsRng);
    let owner_verifying_key = owner_signing_key.verifying_key();
    let owner_id: MemberId = owner_verifying_key.into();

    let same_time = SystemTime::now();

    // Create 10 messages all with the same timestamp
    let mut messages: Vec<AuthorizedMessageV1> = Vec::new();
    for i in 0..10 {
        let msg = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: owner_id,
                time: same_time,
                content: RoomMessageBody::public(format!("Message {}", i)),
            },
            &owner_signing_key,
        );
        messages.push(msg);
    }

    let mut parent_state = ChatRoomStateV1::default();
    parent_state.configuration.configuration.max_recent_messages = 5;
    parent_state.configuration.configuration.max_message_size = 1000;

    let parameters = ChatRoomParametersV1 {
        owner: owner_verifying_key,
    };

    // Multiple orderings
    let orderings: Vec<Vec<AuthorizedMessageV1>> = vec![
        messages.clone(),
        messages.iter().rev().cloned().collect(),
        {
            let mut r = messages.clone();
            r.rotate_left(3);
            r
        },
        {
            let mut r = messages.clone();
            r.rotate_right(7);
            r
        },
    ];

    let mut results: Vec<Vec<MessageId>> = Vec::new();
    for ordering in orderings {
        let mut state = MessagesV1 {
            messages: ordering,
            ..Default::default()
        };
        state
            .apply_delta(&parent_state, &parameters, &None)
            .expect("apply_delta should succeed");

        // Messages should be in deterministic order (by MessageId since timestamps are equal)
        let ids: Vec<_> = state.messages.iter().map(|m| m.id()).collect();
        results.push(ids);
    }

    // All results should be identical
    let first = &results[0];
    for (i, result) in results.iter().enumerate().skip(1) {
        assert_eq!(
            first, result,
            "CONVERGENCE FAILURE: Ordering {} produced different result with identical timestamps",
            i
        );
    }
}

/// Edge case: Deep invite chains (10+ levels)
#[test]
fn test_deep_invite_chains() {
    let owner_signing_key = SigningKey::generate(&mut OsRng);
    let owner_verifying_key = owner_signing_key.verifying_key();
    let owner_id: MemberId = owner_verifying_key.into();

    // Create a chain of 12 members, each inviting the next
    let mut chain: Vec<(AuthorizedMember, SigningKey)> = Vec::new();

    // First member invited by owner
    let (first_member, first_sk) = create_test_member(owner_id, owner_id);
    let first_auth = create_authorized_member(first_member.clone(), &owner_signing_key);
    chain.push((first_auth, first_sk));

    // Each subsequent member invited by the previous
    for _ in 1..12 {
        let (prev_auth, prev_sk) = chain.last().unwrap();
        let (member, signing_key) = create_test_member(owner_id, prev_auth.member.id());
        let auth_member = create_authorized_member(member, prev_sk);
        chain.push((auth_member, signing_key));
    }

    // Also create 3 members at depth 0 (invited by owner)
    let mut depth_0_members: Vec<AuthorizedMember> = Vec::new();
    for _ in 0..3 {
        let (member, _) = create_test_member(owner_id, owner_id);
        let auth_member = create_authorized_member(member, &owner_signing_key);
        depth_0_members.push(auth_member);
    }

    // Combine all members
    let mut all_members: Vec<AuthorizedMember> = chain.iter().map(|(m, _)| m.clone()).collect();
    all_members.extend(depth_0_members);

    // Set max_members to 10 (need to remove 5)
    let mut parent_state = ChatRoomStateV1::default();
    parent_state.configuration.configuration.max_members = 10;

    let parameters = ChatRoomParametersV1 {
        owner: owner_verifying_key,
    };

    // The deepest members (end of chain) should be removed first
    let mut state_a = MembersV1 {
        members: all_members.clone(),
    };
    state_a
        .apply_delta(&parent_state, &parameters, &None)
        .expect("apply_delta should succeed");

    let mut reversed = all_members.clone();
    reversed.reverse();
    let mut state_b = MembersV1 { members: reversed };
    state_b
        .apply_delta(&parent_state, &parameters, &None)
        .expect("apply_delta should succeed");

    assert_eq!(state_a.members.len(), 10);
    assert_eq!(state_b.members.len(), 10);

    let mut ids_a: Vec<_> = state_a.members.iter().map(|m| m.member.id()).collect();
    let mut ids_b: Vec<_> = state_b.members.iter().map(|m| m.member.id()).collect();
    ids_a.sort();
    ids_b.sort();

    assert_eq!(
        ids_a, ids_b,
        "Deep invite chains: same members should be kept"
    );

    // Verify that the deepest chain members were removed (chain indices 7-11 have depth 8-12)
    let deep_chain_ids: Vec<MemberId> = chain[7..12].iter().map(|(m, _)| m.member.id()).collect();
    for deep_id in &deep_chain_ids {
        assert!(
            !ids_a.contains(deep_id),
            "Deep chain member should have been removed"
        );
    }
}

/// Edge case: Concurrent adds and removals (via bans)
#[test]
fn test_concurrent_adds_and_bans() {
    let owner_signing_key = SigningKey::generate(&mut OsRng);
    let owner_verifying_key = owner_signing_key.verifying_key();
    let owner_id: MemberId = owner_verifying_key.into();

    // Create 8 members
    let mut members: Vec<AuthorizedMember> = Vec::new();
    for _ in 0..8 {
        let (member, _) = create_test_member(owner_id, owner_id);
        let auth_member = create_authorized_member(member, &owner_signing_key);
        members.push(auth_member);
    }

    // Ban 2 of them
    let ban_time = SystemTime::now();
    let bans = BansV1(vec![
        AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: ban_time,
                banned_user: members[0].member.id(),
            },
            owner_id,
            &owner_signing_key,
        ),
        AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: ban_time,
                banned_user: members[1].member.id(),
            },
            owner_id,
            &owner_signing_key,
        ),
    ]);

    let mut parent_state = ChatRoomStateV1::default();
    parent_state.configuration.configuration.max_members = 5;
    parent_state.configuration.configuration.max_user_bans = 10;
    parent_state.bans = bans;

    let parameters = ChatRoomParametersV1 {
        owner: owner_verifying_key,
    };

    // Apply members in different orders
    let mut state_a = MembersV1 {
        members: members.clone(),
    };
    state_a
        .apply_delta(&parent_state, &parameters, &None)
        .expect("apply_delta should succeed");

    let mut reversed = members.clone();
    reversed.reverse();
    let mut state_b = MembersV1 { members: reversed };
    state_b
        .apply_delta(&parent_state, &parameters, &None)
        .expect("apply_delta should succeed");

    // Both should have 5 members (8 - 2 banned = 6, then capped to 5)
    assert_eq!(state_a.members.len(), 5);
    assert_eq!(state_b.members.len(), 5);

    // Banned members should not be present
    let ids_a: Vec<_> = state_a.members.iter().map(|m| m.member.id()).collect();
    assert!(!ids_a.contains(&members[0].member.id()));
    assert!(!ids_a.contains(&members[1].member.id()));

    let mut ids_a_sorted = ids_a.clone();
    let mut ids_b: Vec<_> = state_b.members.iter().map(|m| m.member.id()).collect();
    ids_a_sorted.sort();
    ids_b.sort();

    assert_eq!(ids_a_sorted, ids_b, "States should converge after bans");
}

// =============================================================================
// REGRESSION TESTS
// =============================================================================

/// Regression test: Member truncation bug
/// Before fix: First N members from delta were added based on iteration order
/// After fix: All members added, then excess removed deterministically
#[test]
fn test_regression_member_truncation_order_dependent() {
    let owner_signing_key = SigningKey::generate(&mut OsRng);
    let owner_verifying_key = owner_signing_key.verifying_key();
    let owner_id: MemberId = owner_verifying_key.into();

    // Create 5 members with same invite chain length
    let mut members: Vec<AuthorizedMember> = Vec::new();
    for _ in 0..5 {
        let (member, _) = create_test_member(owner_id, owner_id);
        let auth_member = create_authorized_member(member, &owner_signing_key);
        members.push(auth_member);
    }

    let mut parent_state = ChatRoomStateV1::default();
    parent_state.configuration.configuration.max_members = 2;

    let parameters = ChatRoomParametersV1 {
        owner: owner_verifying_key,
    };

    // OLD BEHAVIOR (before fix):
    // - Delta [A, B, C, D, E] with max_members=2 would add A, B (first 2)
    // - Delta [E, D, C, B, A] with max_members=2 would add E, D (first 2)
    // Result: Different states!

    // NEW BEHAVIOR (after fix):
    // - All members are added first, then excess removed by longest chain + highest ID
    // - Since all have same chain length, the 2 members with LOWEST MemberIds are kept

    let mut state_a = MembersV1::default();
    let delta_a = MembersDelta::new(members.clone());
    state_a
        .apply_delta(&parent_state, &parameters, &Some(delta_a))
        .expect("apply_delta should succeed");

    let mut reversed = members.clone();
    reversed.reverse();
    let mut state_b = MembersV1::default();
    let delta_b = MembersDelta::new(reversed);
    state_b
        .apply_delta(&parent_state, &parameters, &Some(delta_b))
        .expect("apply_delta should succeed");

    // Both should have exactly 2 members
    assert_eq!(state_a.members.len(), 2);
    assert_eq!(state_b.members.len(), 2);

    // Both should have the SAME 2 members (the ones with lowest MemberIds)
    let mut ids_a: Vec<_> = state_a.members.iter().map(|m| m.member.id()).collect();
    let mut ids_b: Vec<_> = state_b.members.iter().map(|m| m.member.id()).collect();
    ids_a.sort();
    ids_b.sort();

    assert_eq!(
        ids_a, ids_b,
        "REGRESSION: Member truncation is still order-dependent!\n\
         This would have failed before the fix was applied."
    );

    // Verify the kept members have the lowest IDs among all 5
    let mut all_ids: Vec<_> = members.iter().map(|m| m.member.id()).collect();
    all_ids.sort();
    let expected_kept: Vec<MemberId> = all_ids[0..2].to_vec();

    assert_eq!(
        ids_a, expected_kept,
        "The members with lowest IDs should be kept"
    );
}

/// Regression test: Member excess removal tie-breaking
/// Before fix: max_by_key returned arbitrary member when chain lengths tied
/// After fix: Secondary sort by MemberId provides deterministic tie-breaking
#[test]
fn test_regression_member_excess_removal_tiebreak() {
    let owner_signing_key = SigningKey::generate(&mut OsRng);
    let owner_verifying_key = owner_signing_key.verifying_key();
    let owner_id: MemberId = owner_verifying_key.into();

    // Create 10 members all at same depth (invited by owner)
    let mut members: Vec<AuthorizedMember> = Vec::new();
    for _ in 0..10 {
        let (member, _) = create_test_member(owner_id, owner_id);
        let auth_member = create_authorized_member(member, &owner_signing_key);
        members.push(auth_member);
    }

    let mut parent_state = ChatRoomStateV1::default();
    parent_state.configuration.configuration.max_members = 5;

    let parameters = ChatRoomParametersV1 {
        owner: owner_verifying_key,
    };

    // OLD BEHAVIOR (before fix):
    // max_by_key would return "the last element" when multiple had same chain length,
    // which depends on iteration order. Different orderings would remove different members.

    // NEW BEHAVIOR (after fix):
    // When chain lengths are equal, the member with highest MemberId is removed first.
    // This is deterministic regardless of iteration order.

    // Test 50 different orderings to increase confidence
    let mut first_result: Option<Vec<MemberId>> = None;

    for rotation in 0..10 {
        let mut rotated = members.clone();
        rotated.rotate_left(rotation);

        let mut state = MembersV1 { members: rotated };
        state
            .apply_delta(&parent_state, &parameters, &None)
            .expect("apply_delta should succeed");

        let mut ids: Vec<_> = state.members.iter().map(|m| m.member.id()).collect();
        ids.sort();

        if let Some(ref first) = first_result {
            assert_eq!(
                first, &ids,
                "REGRESSION: Member excess removal tie-breaking is non-deterministic!\n\
                 Rotation {} produced different result. This would have failed before the fix.",
                rotation
            );
        } else {
            first_result = Some(ids);
        }
    }

    // Verify the 5 members with lowest IDs are kept
    let mut all_ids: Vec<_> = members.iter().map(|m| m.member.id()).collect();
    all_ids.sort();
    let expected_kept: Vec<MemberId> = all_ids[0..5].to_vec();

    assert_eq!(
        first_result.unwrap(),
        expected_kept,
        "The 5 members with lowest IDs should be kept"
    );
}

/// Regression test: Ban excess identification
/// Before fix: sort_by_key on timestamp was unstable for equal timestamps
/// After fix: Secondary sort by BanId provides deterministic ordering
#[test]
fn test_regression_ban_excess_identification() {
    use std::collections::HashSet;

    let owner_signing_key = SigningKey::generate(&mut OsRng);
    let owner_verifying_key = owner_signing_key.verifying_key();
    let owner_id: MemberId = owner_verifying_key.into();

    // Create members to ban
    let mut members: Vec<AuthorizedMember> = Vec::new();
    for _ in 0..10 {
        let (member, _) = create_test_member(owner_id, owner_id);
        let auth_member = create_authorized_member(member, &owner_signing_key);
        members.push(auth_member);
    }

    // Create 8 bans with identical timestamps
    let same_time = SystemTime::now();
    let mut bans: Vec<AuthorizedUserBan> = Vec::new();
    for member in members.iter().take(8) {
        let ban = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: same_time,
                banned_user: member.member.id(),
            },
            owner_id,
            &owner_signing_key,
        );
        bans.push(ban);
    }

    let mut parent_state = ChatRoomStateV1::default();
    parent_state.configuration.configuration.max_user_bans = 5;
    parent_state.members = MembersV1 {
        members: members.clone(),
    };

    let parameters = ChatRoomParametersV1 {
        owner: owner_verifying_key,
    };

    // OLD BEHAVIOR (before fix):
    // sort_by_key with equal timestamps would produce unstable ordering,
    // meaning different orderings of the same bans could identify different
    // bans as "excess".

    // NEW BEHAVIOR (after fix):
    // Secondary sort by BanId ensures deterministic identification of excess bans.

    // Helper to extract BanIds from error message
    fn extract_ban_ids(err: &str) -> HashSet<String> {
        err.split("BanId(FastHash(")
            .skip(1)
            .filter_map(|s| s.split("))").next())
            .map(|s| s.to_string())
            .collect()
    }

    // Collect the sets of identified excess bans from multiple orderings
    let mut ban_id_sets: Vec<HashSet<String>> = Vec::new();

    for rotation in 0..8 {
        let mut rotated = bans.clone();
        rotated.rotate_left(rotation);
        let bans_state = BansV1(rotated);
        let err = bans_state.verify(&parent_state, &parameters).unwrap_err();
        ban_id_sets.push(extract_ban_ids(&err));
    }

    // All sets should identify the same BanIds as excess
    let first = &ban_id_sets[0];
    for (i, ban_ids) in ban_id_sets.iter().enumerate().skip(1) {
        assert_eq!(
            first, ban_ids,
            "REGRESSION: Ban excess identification is non-deterministic!\n\
             Rotation {} identified different excess bans. This would have failed before the fix.",
            i
        );
    }

    // Verify we identified exactly 3 excess bans (8 - 5 = 3)
    assert_eq!(first.len(), 3, "Should identify exactly 3 excess bans");
}

/// Regression test: Message pruning order
/// Before fix: sort_by on timestamp without secondary key was order-dependent
/// After fix: Secondary sort by MessageId provides deterministic pruning
#[test]
fn test_regression_message_pruning_order() {
    let owner_signing_key = SigningKey::generate(&mut OsRng);
    let owner_verifying_key = owner_signing_key.verifying_key();
    let owner_id: MemberId = owner_verifying_key.into();

    // Create 20 messages in groups of 4 with same timestamp
    let base_time = SystemTime::now();
    let mut messages: Vec<AuthorizedMessageV1> = Vec::new();
    for i in 0..20 {
        let time_offset = (i / 4) as u64; // 4 messages per timestamp
        let time = base_time + std::time::Duration::from_secs(time_offset);
        let msg = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: owner_id,
                time,
                content: RoomMessageBody::public(format!("Message {}", i)),
            },
            &owner_signing_key,
        );
        messages.push(msg);
    }

    let mut parent_state = ChatRoomStateV1::default();
    parent_state.configuration.configuration.max_recent_messages = 10;
    parent_state.configuration.configuration.max_message_size = 1000;

    let parameters = ChatRoomParametersV1 {
        owner: owner_verifying_key,
    };

    // OLD BEHAVIOR (before fix):
    // Messages with same timestamp would have undefined relative order after sorting,
    // leading to non-deterministic pruning.

    // NEW BEHAVIOR (after fix):
    // Messages with same timestamp are sorted by MessageId as secondary key,
    // ensuring deterministic pruning.

    let mut first_result: Option<Vec<MessageId>> = None;

    for rotation in 0..10 {
        let mut rotated = messages.clone();
        rotated.rotate_left(rotation * 2);

        let mut state = MessagesV1 {
            messages: rotated,
            ..Default::default()
        };
        state
            .apply_delta(&parent_state, &parameters, &None)
            .expect("apply_delta should succeed");

        let ids: Vec<_> = state.messages.iter().map(|m| m.id()).collect();

        if let Some(ref first) = first_result {
            assert_eq!(
                first,
                &ids,
                "REGRESSION: Message pruning is non-deterministic!\n\
                 Rotation {} produced different message order. This would have failed before the fix.",
                rotation
            );
        } else {
            first_result = Some(ids);
        }
    }
}

// =============================================================================
// SERIALIZED COMMUTATIVITY TEST
// =============================================================================
//
// This test verifies that merge(A,B) == merge(B,A) at the serialized byte level.
// This is the ultimate convergence test: two peers starting from different initial
// states must produce identical serialized output after merging the other's state.

use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};

#[test]
fn test_full_state_merge_commutativity() {
    let owner_signing_key = SigningKey::generate(&mut OsRng);
    let owner_verifying_key = owner_signing_key.verifying_key();
    let owner_id: MemberId = owner_verifying_key.into();

    let parameters = ChatRoomParametersV1 {
        owner: owner_verifying_key,
    };

    // Create members
    let (member_a, member_a_sk) = create_test_member(owner_id, owner_id);
    let (member_b, member_b_sk) = create_test_member(owner_id, owner_id);
    let (member_c, _member_c_sk) = create_test_member(owner_id, owner_id);

    let auth_member_a = create_authorized_member(member_a.clone(), &owner_signing_key);
    let auth_member_b = create_authorized_member(member_b.clone(), &owner_signing_key);
    let auth_member_c = create_authorized_member(member_c.clone(), &owner_signing_key);

    // Create member info
    let info_a = AuthorizedMemberInfo::new_with_member_key(
        MemberInfo::new_public(member_a.id(), 1, "Alice".to_string()),
        &member_a_sk,
    );
    let info_b = AuthorizedMemberInfo::new_with_member_key(
        MemberInfo::new_public(member_b.id(), 1, "Bob".to_string()),
        &member_b_sk,
    );
    let owner_info = AuthorizedMemberInfo::new(
        MemberInfo::new_public(owner_id, 1, "Owner".to_string()),
        &owner_signing_key,
    );

    // Create messages with different timestamps
    let time_1 = SystemTime::now();
    let time_2 = time_1 + std::time::Duration::from_secs(1);
    let time_3 = time_1 + std::time::Duration::from_secs(2);

    let msg_1 = AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: owner_id,
            author: owner_id,
            time: time_1,
            content: RoomMessageBody::public("Hello from owner".to_string()),
        },
        &owner_signing_key,
    );
    let msg_2 = AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: owner_id,
            author: member_a.id(),
            time: time_2,
            content: RoomMessageBody::public("Hello from Alice".to_string()),
        },
        &member_a_sk,
    );
    let msg_3 = AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: owner_id,
            author: member_b.id(),
            time: time_3,
            content: RoomMessageBody::public("Hello from Bob".to_string()),
        },
        &member_b_sk,
    );

    // ---- State A: has members [A, C], messages [1, 2], info [owner, A] ----
    let mut state_a = ChatRoomStateV1::default();
    state_a.configuration.configuration.max_members = 10;
    state_a.configuration.configuration.max_recent_messages = 100;
    state_a.configuration.configuration.max_message_size = 1000;
    state_a.members.members.push(auth_member_a.clone());
    state_a.members.members.push(auth_member_c.clone());
    state_a.recent_messages.messages.push(msg_1.clone());
    state_a.recent_messages.messages.push(msg_2.clone());
    state_a.member_info.member_info.push(owner_info.clone());
    state_a.member_info.member_info.push(info_a.clone());

    // ---- State B: has members [B, C], messages [1, 3], info [owner, B] ----
    let mut state_b = ChatRoomStateV1::default();
    state_b.configuration.configuration.max_members = 10;
    state_b.configuration.configuration.max_recent_messages = 100;
    state_b.configuration.configuration.max_message_size = 1000;
    state_b.members.members.push(auth_member_b.clone());
    state_b.members.members.push(auth_member_c.clone());
    state_b.recent_messages.messages.push(msg_1.clone());
    state_b.recent_messages.messages.push(msg_3.clone());
    state_b.member_info.member_info.push(owner_info.clone());
    state_b.member_info.member_info.push(info_b.clone());

    // ---- merge(A, B): start from A, merge in B ----
    let mut merged_ab = state_a.clone();
    merged_ab
        .merge(&state_a, &parameters, &state_b)
        .expect("merge A+B should succeed");

    // ---- merge(B, A): start from B, merge in A ----
    let mut merged_ba = state_b.clone();
    merged_ba
        .merge(&state_b, &parameters, &state_a)
        .expect("merge B+A should succeed");

    // Serialize both results
    let mut bytes_ab = Vec::new();
    ciborium::ser::into_writer(&merged_ab, &mut bytes_ab).expect("serialize merged_ab");
    let mut bytes_ba = Vec::new();
    ciborium::ser::into_writer(&merged_ba, &mut bytes_ba).expect("serialize merged_ba");

    // The serialized bytes must be identical
    assert_eq!(
        bytes_ab,
        bytes_ba,
        "COMMUTATIVITY FAILURE: merge(A,B) != merge(B,A) at byte level!\n\
         merged_ab members: {:?}\n\
         merged_ba members: {:?}\n\
         merged_ab messages: {:?}\n\
         merged_ba messages: {:?}\n\
         merged_ab member_info: {:?}\n\
         merged_ba member_info: {:?}",
        merged_ab
            .members
            .members
            .iter()
            .map(|m| m.member.id())
            .collect::<Vec<_>>(),
        merged_ba
            .members
            .members
            .iter()
            .map(|m| m.member.id())
            .collect::<Vec<_>>(),
        merged_ab
            .recent_messages
            .messages
            .iter()
            .map(|m| m.id())
            .collect::<Vec<_>>(),
        merged_ba
            .recent_messages
            .messages
            .iter()
            .map(|m| m.id())
            .collect::<Vec<_>>(),
        merged_ab
            .member_info
            .member_info
            .iter()
            .map(|i| i.member_info.member_id)
            .collect::<Vec<_>>(),
        merged_ba
            .member_info
            .member_info
            .iter()
            .map(|i| i.member_info.member_id)
            .collect::<Vec<_>>(),
    );
}

// =============================================================================
// REGRESSION TESTS
// =============================================================================

/// Regression test: Combined scenario testing all fixes together
/// This test exercises all the convergence fixes in a realistic scenario
#[test]
fn test_regression_combined_scenario() {
    let owner_signing_key = SigningKey::generate(&mut OsRng);
    let owner_verifying_key = owner_signing_key.verifying_key();
    let owner_id: MemberId = owner_verifying_key.into();

    // Create a realistic room state with:
    // - 30 members with varying invite depths
    // - 50 messages with some having same timestamps
    // - 8 bans with some having same timestamps

    // Create member hierarchy
    let mut members: Vec<(AuthorizedMember, SigningKey)> = Vec::new();
    let mut level_0: Vec<(AuthorizedMember, SigningKey)> = Vec::new();

    // 10 level-0 members
    for _ in 0..10 {
        let (member, signing_key) = create_test_member(owner_id, owner_id);
        let auth_member = create_authorized_member(member, &owner_signing_key);
        level_0.push((auth_member.clone(), signing_key.clone()));
        members.push((auth_member, signing_key));
    }

    // 15 level-1 members
    for i in 0..15 {
        let inviter = &level_0[i % level_0.len()];
        let (member, signing_key) = create_test_member(owner_id, inviter.0.member.id());
        let auth_member = create_authorized_member(member, &inviter.1);
        members.push((auth_member, signing_key));
    }

    // 5 level-2 members (these should be removed first when over capacity)
    for i in 0..5 {
        let inviter = &members[10 + (i % 15)]; // Pick from level-1
        let (member, signing_key) = create_test_member(owner_id, inviter.0.member.id());
        let auth_member = create_authorized_member(member, &inviter.1);
        members.push((auth_member, signing_key));
    }

    assert_eq!(members.len(), 30);

    // Create messages
    let base_time = SystemTime::now();
    let mut messages: Vec<AuthorizedMessageV1> = Vec::new();
    for i in 0..50 {
        let time_offset = (i / 3) as u64; // Groups of 3 with same timestamp
        let time = base_time + std::time::Duration::from_secs(time_offset);
        let author_idx = i % members.len();
        let msg = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: members[author_idx].0.member.id(),
                time,
                content: RoomMessageBody::public(format!("Message {}", i)),
            },
            &members[author_idx].1,
        );
        messages.push(msg);
    }

    // Create bans (ban some level-2 members)
    let ban_time = SystemTime::now();
    let mut bans: Vec<AuthorizedUserBan> = Vec::new();
    for i in 0..3 {
        let ban = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: ban_time,                        // Same timestamp
                banned_user: members[25 + i].0.member.id(), // Ban level-2 members
            },
            owner_id,
            &owner_signing_key,
        );
        bans.push(ban);
    }

    // Set up parent state with limits
    let mut parent_state = ChatRoomStateV1::default();
    parent_state.configuration.configuration.max_members = 20;
    parent_state.configuration.configuration.max_recent_messages = 30;
    parent_state.configuration.configuration.max_message_size = 1000;
    parent_state.configuration.configuration.max_user_bans = 10;
    parent_state.bans = BansV1(bans);

    let parameters = ChatRoomParametersV1 {
        owner: owner_verifying_key,
    };

    // Test convergence with multiple orderings
    let member_list: Vec<AuthorizedMember> = members.iter().map(|(m, _)| m.clone()).collect();

    let orderings = vec![
        (member_list.clone(), messages.clone()),
        (
            member_list.iter().rev().cloned().collect(),
            messages.iter().rev().cloned().collect(),
        ),
        (
            {
                let mut m = member_list.clone();
                m.rotate_left(11);
                m
            },
            {
                let mut msgs = messages.clone();
                msgs.rotate_left(17);
                msgs
            },
        ),
    ];

    let mut final_states: Vec<(Vec<MemberId>, Vec<MessageId>)> = Vec::new();

    for (member_ordering, msg_ordering) in orderings {
        // Update parent state with ordered members for message validation
        let mut local_parent = parent_state.clone();
        local_parent.members = MembersV1 {
            members: member_ordering.clone(),
        };
        local_parent
            .members
            .apply_delta(&parent_state, &parameters, &None)
            .expect("members apply_delta should succeed");

        // Apply messages
        let mut msg_state = MessagesV1 {
            messages: msg_ordering,
            ..Default::default()
        };
        msg_state
            .apply_delta(&local_parent, &parameters, &None)
            .expect("messages apply_delta should succeed");

        let mut member_ids: Vec<_> = local_parent
            .members
            .members
            .iter()
            .map(|m| m.member.id())
            .collect();
        member_ids.sort();

        let msg_ids: Vec<_> = msg_state.messages.iter().map(|m| m.id()).collect();

        final_states.push((member_ids, msg_ids));
    }

    // All final states should be identical
    let (first_members, first_messages) = &final_states[0];
    for (i, (members_result, messages_result)) in final_states.iter().enumerate().skip(1) {
        assert_eq!(
            first_members, members_result,
            "REGRESSION: Combined scenario - members don't converge for ordering {}",
            i
        );
        assert_eq!(
            first_messages, messages_result,
            "REGRESSION: Combined scenario - messages don't converge for ordering {}",
            i
        );
    }
}
