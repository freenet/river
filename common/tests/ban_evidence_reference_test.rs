//! Ban evidence keeps only what a stored ban references through records
//! cleanup can keep (freenet/river#703 review): deterministic repro of the
//! case FORK_SEEDS=1000 found (extended seed (121, 2)).

use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use river_core::room_state::ban::{AuthorizedUserBan, BansV1, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersV1};
use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1};
use std::time::{Duration, SystemTime};

/// A (absent, ancestor) -> D (owner-banned, also named by P's ban). A is
/// referenced only through the owner-removed D, so it must not be kept as
/// evidence, and the cleaned state must verify.
#[test]
fn an_ancestor_referenced_only_through_an_owner_removed_record_is_not_kept() {
    let key = |b: u8| SigningKey::from_bytes(&[b; 32]);
    let (owner_sk, p_sk, a_sk, d_sk) = (key(1), key(2), key(3), key(4));
    let owner_id: MemberId = owner_sk.verifying_key().into();
    let rec = |sk: &SigningKey, inviter: MemberId, inviter_sk: &SigningKey| {
        AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: inviter,
                member_vk: sk.verifying_key(),
            },
            inviter_sk,
        )
    };
    let p = rec(&p_sk, owner_id, &owner_sk);
    let a = rec(&a_sk, owner_id, &owner_sk);
    let d = rec(&d_sk, a.member.id(), &a_sk);
    let params = ChatRoomParametersV1 {
        owner: owner_sk.verifying_key(),
    };
    let ban = |by_sk: &SigningKey, target: MemberId, t: u64| {
        AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + t),
                banned_user: target,
            },
            by_sk.verifying_key().into(),
            by_sk,
        )
    };
    let mut s = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: owner_id,
                ..Default::default()
            },
            &owner_sk,
        ),
        ..Default::default()
    };
    s.members = MembersV1 {
        members: vec![p.clone(), a.clone(), d.clone()],
    };
    s.recent_messages.messages = vec![AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: owner_id,
            author: p.member.id(),
            time: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_001),
            content: RoomMessageBody::public("hi".into()),
        },
        &p_sk,
    )];
    s.bans = BansV1(vec![
        ban(&p_sk, d.member.id(), 2),
        ban(&owner_sk, d.member.id(), 3),
    ]);
    s.post_apply_cleanup(&params).unwrap();
    s.verify(&s, &params).expect("cleaned state must verify");
    assert!(
        !s.ban_evidence
            .members
            .iter()
            .any(|m| m.member.id() == a.member.id()),
        "A is referenced only through an owner-removed record"
    );
}
