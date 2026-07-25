//! Pins the contract behaviour the UI's and riverctl's reply-quote handling is
//! built on: an ENFORCED ban removes the member, which makes
//! `post_apply_cleanup` purge BOTH their messages and their `member_info`
//! record.
//!
//! Why this test exists rather than only the client-side ones: a reply embeds a
//! replier-authored snapshot of the message it quotes (`ReplyContentV1`'s
//! `target_author_name` / `target_content_preview`), which the contract does not
//! validate. After a ban that snapshot is the last surviving copy of the banned
//! member's text, so both clients refuse to render a quote they cannot re-read
//! from live state (`resolve_reply_context` in
//! `ui/src/components/conversation.rs`, `reply_context_display_with_secrets` in
//! `cli/src/api.rs`).
//!
//! That refusal keys on ABSENCE, because nothing links the snapshot back to a
//! `MemberId`: the snapshot stores only a name string, `MessageId` is
//! `fast_hash(signature)`, and the purge takes the banned member's nickname with
//! it. So if this sweep ever changed — tombstones, a "soft ban", or scoping the
//! sweep to inactivity-prune only — the banned member's text would start
//! rendering again with every client-side test still green. This is the tripwire
//! for that.

use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use river_core::room_state::ban::{AuthorizedUserBan, BansV1, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersV1};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo, MemberInfoV1};
use river_core::room_state::message::{
    AuthorizedMessageV1, MessageV1, MessagesV1, RoomMessageBody,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1};
use std::time::SystemTime;

struct Peer {
    sk: SigningKey,
    id: MemberId,
}

impl Peer {
    fn new() -> Self {
        let sk = SigningKey::generate(&mut OsRng);
        let id = MemberId::from(&sk.verifying_key());
        Self { sk, id }
    }
}

fn member(
    who: &Peer,
    invited_by: MemberId,
    inviter_sk: &SigningKey,
    owner_id: MemberId,
) -> AuthorizedMember {
    AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by,
            member_vk: who.sk.verifying_key(),
        },
        inviter_sk,
    )
}

fn info(who: &Peer, nickname: &str) -> AuthorizedMemberInfo {
    AuthorizedMemberInfo::new_with_member_key(
        MemberInfo::new_public(who.id, 0, nickname.to_string()),
        &who.sk,
    )
}

fn message(who: &Peer, owner_id: MemberId, text: &str) -> AuthorizedMessageV1 {
    AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: owner_id,
            author: who.id,
            time: SystemTime::UNIX_EPOCH,
            content: RoomMessageBody::public(text.to_string()),
        },
        &who.sk,
    )
}

fn params(owner: &Peer) -> ChatRoomParametersV1 {
    ChatRoomParametersV1 {
        owner: owner.sk.verifying_key(),
    }
}

fn config(owner: &Peer) -> AuthorizedConfigurationV1 {
    AuthorizedConfigurationV1::new(Configuration::default(), &owner.sk)
}

fn ban(target: MemberId, banner: &Peer, owner_id: MemberId) -> AuthorizedUserBan {
    AuthorizedUserBan::new(
        UserBan {
            owner_member_id: owner_id,
            banned_at: SystemTime::UNIX_EPOCH,
            banned_user: target,
        },
        banner.id,
        &banner.sk,
    )
}

/// An owner ban removes the member, sweeps every message they authored, and
/// drops their `member_info` record — so nothing in converged state can map a
/// reply's quoted-author NAME back to the banned `MemberId`.
#[test]
fn owner_ban_purges_the_banned_members_messages_and_member_info() {
    let owner = Peer::new();
    let abuser = Peer::new();
    let bystander = Peer::new();
    let owner_id = owner.id;

    const ABUSE: &str = "you are all worthless, buy my coin at scam.example";

    let abusive = message(&abuser, owner_id, ABUSE);
    // A reply quoting the abusive message, carrying the snapshot of it that
    // survives the purge and that the clients must refuse to render.
    let reply = AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: owner_id,
            author: bystander.id,
            time: SystemTime::UNIX_EPOCH,
            content: RoomMessageBody::reply(
                "please stop".to_string(),
                abusive.id(),
                "Abuser".to_string(),
                ABUSE.to_string(),
            ),
        },
        &bystander.sk,
    );
    let abusive_id = abusive.id();

    let mut state = ChatRoomStateV1 {
        configuration: config(&owner),
        members: MembersV1 {
            members: vec![
                member(&abuser, owner_id, &owner.sk, owner_id),
                member(&bystander, owner_id, &owner.sk, owner_id),
            ],
        },
        member_info: MemberInfoV1 {
            member_info: vec![info(&abuser, "Abuser"), info(&bystander, "Bystander")],
        },
        recent_messages: MessagesV1 {
            messages: vec![abusive, reply.clone()],
            ..Default::default()
        },
        bans: BansV1(vec![ban(abuser.id, &owner, owner_id)]),
        ..Default::default()
    };

    state.post_apply_cleanup(&params(&owner)).unwrap();

    assert!(
        !state
            .members
            .members
            .iter()
            .any(|m| m.member.id() == abuser.id),
        "an owner ban must remove the member"
    );
    assert!(
        !state
            .recent_messages
            .messages
            .iter()
            .any(|m| m.id() == abusive_id),
        "the banned member's messages must be purged — the clients' quote \
         suppression keys on this absence"
    );
    assert!(
        !state
            .member_info
            .member_info
            .iter()
            .any(|i| i.member_info.member_id == abuser.id),
        "the banned member's member_info must be purged too — this is why the \
         quoted-author NAME in a reply snapshot cannot be matched against the \
         ban list"
    );

    // The quoting reply itself survives (its author is still a member), which
    // is exactly why the snapshot outlives the message it quotes.
    assert!(
        state
            .recent_messages
            .messages
            .iter()
            .any(|m| m.id() == reply.id()),
        "the reply that quoted the banned member must survive the purge"
    );
}
