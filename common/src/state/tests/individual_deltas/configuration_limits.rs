use super::*;
use crate::{ChatRoomState, ChatRoomDelta};
use crate::state::{AuthorizedUserBan, AuthorizedMember};
use crate::state::member::{Member, MemberId};
use crate::state::ban::UserBan;
use ed25519_dalek::Signature;
use std::time::SystemTime;
use std::collections::HashSet;
#[test]
fn test_max_user_bans_limit() {
    let parameters = create_test_parameters();
    let mut initial_state = ChatRoomState::default();
    initial_state.configuration.configuration.max_user_bans = 3;

    let create_ban = |user_id: i32| AuthorizedUserBan {
        ban: UserBan {
            banned_user: MemberId(user_id),
            banned_at: SystemTime::now(),
        },
        banned_by: parameters.owner,
        signature: Signature::from_bytes(&[0; 64]),
    };

    let bans = (0..5).map(|i| create_ban(i)).collect::<Vec<_>>();

    let delta = ChatRoomDelta {
        configuration: None,
        members: HashSet::new(),
        upgrade: None,
        recent_messages: Vec::new(),
        ban_log: bans,
    };

    let result = test_apply_deltas(
        initial_state,
        vec![delta],
        |state: &ChatRoomState| {
            state.ban_log.len() == 3 &&
            state.ban_log.iter().all(|b| b.ban.banned_user.0 < 3)
        },
        &parameters,
    );

    assert!(result.is_ok(), "Failed to apply delta: {:?}", result.err());
}
