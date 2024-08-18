use proptest::prelude::*;
use crate::{ChatRoomState, ChatRoomDelta, ChatRoomParameters};
use crate::configuration::{AuthorizedConfiguration, Configuration};
use crate::member::{AuthorizedMember, Member, MemberId};
use crate::message::AuthorizedMessage;
use crate::ban::AuthorizedUserBan;
use ed25519_dalek::{Signature, VerifyingKey};
use std::collections::HashSet;
use std::time::SystemTime;

prop_compose! {
    fn arb_verifying_key()(bytes in prop::array::uniform32(0u8..)) -> VerifyingKey {
        VerifyingKey::from_bytes(&bytes).unwrap()
    }
}

prop_compose! {
    fn arb_signature()(bytes in prop::array::uniform64(0u8..)) -> Signature {
        Signature::from_bytes(&bytes)
    }
}

prop_compose! {
    fn arb_member()(
        public_key in arb_verifying_key(),
        nickname in "[a-zA-Z0-9]{1,20}"
    ) -> Member {
        Member {
            public_key,
            nickname,
        }
    }
}

prop_compose! {
    fn arb_authorized_member()(
        member in arb_member(),
        invited_by in arb_verifying_key(),
        signature in arb_signature()
    ) -> AuthorizedMember {
        AuthorizedMember {
            member,
            invited_by,
            signature,
        }
    }
}

prop_compose! {
    fn arb_configuration()(
        configuration_version in 0..1000u32,
        name in "[a-zA-Z0-9]{1,20}",
        max_recent_messages in 10..1000u32,
        max_user_bans in 10..1000u32
    ) -> Configuration {
        Configuration {
            configuration_version,
            name,
            max_recent_messages,
            max_user_bans,
        }
    }
}

prop_compose! {
    fn arb_authorized_configuration()(
        configuration in arb_configuration(),
        signature in arb_signature()
    ) -> AuthorizedConfiguration {
        AuthorizedConfiguration {
            configuration,
            signature,
        }
    }
}

prop_compose! {
    fn arb_authorized_message()(
        time in prop::num::u64::ANY.prop_map(|t| SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(t)),
        content in "[a-zA-Z0-9]{1,100}",
        author in prop::num::i32::ANY.prop_map(MemberId),
        signature in arb_signature()
    ) -> AuthorizedMessage {
        AuthorizedMessage {
            time,
            content,
            author,
            signature,
        }
    }
}

prop_compose! {
    fn arb_authorized_user_ban()(
        banned_at in prop::num::u64::ANY.prop_map(|t| SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(t)),
        banned_user in prop::num::i32::ANY.prop_map(MemberId),
        banned_by in arb_verifying_key(),
        signature in arb_signature()
    ) -> AuthorizedUserBan {
        AuthorizedUserBan {
            ban: crate::ban::UserBan {
                banned_at,
                banned_user,
            },
            banned_by,
            signature,
        }
    }
}

prop_compose! {
    fn arb_chat_room_state()(
        configuration in arb_authorized_configuration(),
        members in prop::collection::hash_set(arb_authorized_member(), 0..100),
        recent_messages in prop::collection::vec(arb_authorized_message(), 0..100),
        ban_log in prop::collection::vec(arb_authorized_user_ban(), 0..100)
    ) -> ChatRoomState {
        ChatRoomState {
            configuration,
            members,
            upgrade: None, // For simplicity, we're not generating upgrades in this arbitrary state
            recent_messages,
            ban_log,
        }
    }
}

prop_compose! {
    fn arb_chat_room_delta()(
        configuration in prop::option::of(arb_authorized_configuration()),
        members in prop::collection::hash_set(arb_authorized_member(), 0..10),
        recent_messages in prop::collection::vec(arb_authorized_message(), 0..10),
        ban_log in prop::collection::vec(arb_authorized_user_ban(), 0..10)
    ) -> ChatRoomDelta {
        ChatRoomDelta {
            configuration,
            members,
            upgrade: None, // For simplicity, we're not generating upgrades in this arbitrary delta
            recent_messages,
            ban_log,
        }
    }
}

proptest! {
    #[test]
    fn test_delta_application_is_commutative(
        initial_state in arb_chat_room_state(),
        delta1 in arb_chat_room_delta(),
        delta2 in arb_chat_room_delta()
    ) {
        let mut state1 = initial_state.clone();
        let mut state2 = initial_state;

        state1.apply_delta(delta1.clone());
        state1.apply_delta(delta2.clone());

        state2.apply_delta(delta2);
        state2.apply_delta(delta1);

        prop_assert_eq!(state1, state2);
    }

    #[test]
    fn test_delta_application_preserves_invariants(
        initial_state in arb_chat_room_state(),
        delta in arb_chat_room_delta()
    ) {
        let mut state = initial_state;
        state.apply_delta(delta);

        prop_assert!(state.recent_messages.len() <= state.configuration.configuration.max_recent_messages as usize);
        prop_assert!(state.ban_log.len() <= state.configuration.configuration.max_user_bans as usize);
    }

    #[test]
    fn test_summarize_reflects_state(state in arb_chat_room_state()) {
        let summary = state.summarize();

        prop_assert_eq!(summary.configuration_version, state.configuration.configuration.configuration_version);
        prop_assert_eq!(summary.member_ids, state.members.iter().map(|m| m.member.id()).collect::<HashSet<_>>());
        prop_assert_eq!(summary.recent_message_ids, state.recent_messages.iter().map(|m| m.id()).collect::<HashSet<_>>());
        prop_assert_eq!(summary.ban_ids, state.ban_log.iter().map(|b| b.id()).collect::<Vec<_>>());
    }
}
