//! The member, message and direct-message caps are decided by
//! `post_apply_cleanup`, after bans and pruning, so a member cleanup removes
//! never takes a cap slot and a ban the ban cap does not keep removes nobody.
//! A merged state then absorbs what it merged, on the merge path and on the
//! network-delta path alike.
use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use river_core::room_state::ban::{AuthorizedUserBan, BansV1, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersV1};
use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
use river_core::room_state::{
    ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta, ChatRoomStateV1Summary,
};
use std::time::{Duration, SystemTime};

fn sk(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn at(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + secs)
}

fn ser<T: serde::Serialize>(s: &T) -> Vec<u8> {
    let mut v = Vec::new();
    ciborium::ser::into_writer(s, &mut v).unwrap();
    v
}

fn cbor_rt<T: serde::Serialize + serde::de::DeserializeOwned>(x: &T) -> T {
    ciborium::de::from_reader(ser(x).as_slice()).unwrap()
}

struct Room {
    owner: SigningKey,
    oid: MemberId,
    params: ChatRoomParametersV1,
}

impl Room {
    fn new() -> Self {
        let owner = sk(1);
        Room {
            oid: owner.verifying_key().into(),
            params: ChatRoomParametersV1 {
                owner: owner.verifying_key(),
            },
            owner,
        }
    }

    fn cfg(&self, v: u32, bans: usize, members: usize, msgs: usize) -> AuthorizedConfigurationV1 {
        AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: self.oid,
                configuration_version: v,
                max_user_bans: bans,
                max_members: members,
                max_recent_messages: msgs,
                ..Default::default()
            },
            &self.owner,
        )
    }

    fn member(&self, m: &SigningKey, inviter: &SigningKey) -> AuthorizedMember {
        AuthorizedMember::new(
            Member {
                owner_member_id: self.oid,
                invited_by: inviter.verifying_key().into(),
                member_vk: m.verifying_key(),
            },
            inviter,
        )
    }

    fn msg(&self, author: &SigningKey, secs: u64) -> AuthorizedMessageV1 {
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: self.oid,
                author: author.verifying_key().into(),
                time: at(secs),
                content: RoomMessageBody::public(format!("m{secs}")),
            },
            author,
        )
    }

    fn owner_ban(&self, target: MemberId, secs: u64) -> AuthorizedUserBan {
        AuthorizedUserBan::new(
            UserBan {
                owner_member_id: self.oid,
                banned_at: at(secs),
                banned_user: target,
            },
            self.oid,
            &self.owner,
        )
    }

    fn merge(&self, a: &ChatRoomStateV1, b: &ChatRoomStateV1) -> ChatRoomStateV1 {
        let mut out = a.clone();
        out.merge_from(&self.params, b).expect("merge");
        out
    }

    /// The contract's network path: `get_state_delta` (the sender's delta
    /// against the receiver's summary, trimmed to the receiver's work
    /// bounds), then `update_state` on the receiver.
    fn network_delta(
        &self,
        receiver: &ChatRoomStateV1,
        sender: &ChatRoomStateV1,
    ) -> (Option<ChatRoomStateV1Delta>, ChatRoomStateV1) {
        let summary: ChatRoomStateV1Summary = cbor_rt(&receiver.summarize(receiver, &self.params));
        let d = sender
            .delta_within_work_bounds(&self.params, &summary)
            .map(|d| cbor_rt(&d));
        let mut out = receiver.clone();
        if let Some(dd) = &d {
            out.apply_delta(receiver, &self.params, &Some(dd.clone()))
                .expect("delta applies");
        }
        (d, out)
    }
}

/// X (older configuration, cap 10) holds an old owner ban on M and a newer
/// one on an id held nowhere. The merged cap keeps one owner ban; whichever
/// it keeps, merging Y again changes nothing, and both directions agree.
#[test]
fn a_merged_state_absorbs_what_it_merged_when_the_cap_evicts_an_owner_ban() {
    let r = Room::new();
    let m = sk(0x22);
    let m_id: MemberId = m.verifying_key().into();
    let mut y = ChatRoomStateV1 {
        configuration: r.cfg(2, 1, 10, 100),
        members: MembersV1 {
            members: vec![r.member(&m, &r.owner)],
        },
        ..Default::default()
    };
    y.recent_messages.messages = vec![r.msg(&m, 100)];
    let y = r.merge(&y, &y);
    let x = ChatRoomStateV1 {
        configuration: r.cfg(1, 10, 10, 100),
        bans: BansV1(vec![
            r.owner_ban(m_id, 10),
            r.owner_ban(sk(0x33).verifying_key().into(), 20),
        ]),
        ..Default::default()
    };
    let x = r.merge(&x, &x);
    assert_eq!(x.bans.0.len(), 2, "premise");

    let xy = r.merge(&x, &y);
    assert!(ser(&xy) == ser(&r.merge(&y, &x)), "both directions agree");
    xy.verify(&xy, &r.params).unwrap();
    assert!(
        ser(&r.merge(&xy, &y)) == ser(&xy),
        "merging Y again changes nothing"
    );
    let removed = !xy.members.members.iter().any(|mm| mm.member.id() == m_id);
    let banned = xy.bans.0.iter().any(|b| b.ban.banned_user == m_id);
    assert_eq!(
        removed, banned,
        "M is removed exactly when the ban that removes it is kept"
    );
}

/// The same shape on every path: the network delta lands where the merge
/// lands, merging the sender again changes nothing, and the transitions in
/// both directions are reproduced by merging their base back in.
#[test]
fn an_owner_ban_the_cap_does_not_keep_removes_nobody_on_any_path() {
    let r = Room::new();
    let m2 = sk(0x22);
    let absent = sk(0x33);
    let m2_id: MemberId = m2.verifying_key().into();
    // Y: newer configuration, ban cap 1; M2 is a member with a message.
    let mut y = ChatRoomStateV1 {
        configuration: r.cfg(2, 1, 10, 100),
        ..Default::default()
    };
    y.members = MembersV1 {
        members: vec![r.member(&m2, &r.owner)],
    };
    y.recent_messages.messages = vec![r.msg(&m2, 100)];
    y.verify(&y, &r.params).expect("y valid");
    let y = r.merge(&y, &y);
    // X: older configuration, cap 10; an old owner ban of M2 (absent here)
    // and a newer owner ban of an id known nowhere.
    let mut x = ChatRoomStateV1 {
        configuration: r.cfg(1, 10, 10, 100),
        ..Default::default()
    };
    x.bans = BansV1(vec![
        r.owner_ban(m2_id, 10),
        r.owner_ban(absent.verifying_key().into(), 20),
    ]);
    x.verify(&x, &r.params).expect("x valid");
    let x = r.merge(&x, &x);
    assert_eq!(x.bans.0.len(), 2);
    let has_m2 = |s: &ChatRoomStateV1| s.members.members.iter().any(|m| m.member.id() == m2_id);
    let has_m2_ban = |s: &ChatRoomStateV1| s.bans.0.iter().any(|b| b.ban.banned_user == m2_id);

    let xy = r.merge(&x, &y);
    xy.verify(&xy, &r.params).unwrap();
    assert!(ser(&xy) == ser(&r.merge(&y, &x)), "commutes");
    assert_eq!(
        has_m2(&xy),
        !has_m2_ban(&xy),
        "M2 is removed exactly when the ban that removes it is kept"
    );
    assert!(ser(&r.merge(&xy, &y)) == ser(&xy), "absorbs Y");
    assert!(ser(&r.merge(&xy, &x)) == ser(&xy), "absorbs X");

    // The network path lands where the merge lands, and healing it with
    // the sender changes nothing.
    let (d, delta_path) = r.network_delta(&x, &y);
    assert!(d.is_some());
    assert!(ser(&delta_path) == ser(&xy), "delta path == merge path");
    assert!(
        ser(&r.merge(&delta_path, &y)) == ser(&xy),
        "heals to the merge"
    );
    assert!(
        ser(&r.merge(&x, &delta_path)) == ser(&delta_path),
        "the transition X -> delta path is reproduced by merging X back"
    );
    let (_, y_after) = r.network_delta(&y, &x);
    assert!(
        ser(&r.merge(&y, &y_after)) == ser(&y_after),
        "the transition Y -> delta path is reproduced by merging Y back"
    );
}

/// X (newer, max_members 2, max_recent_messages 1) holds P with an old
/// message; Y (older) holds Q and K (invited by Q), K's message newest. In
/// the merged room P's message loses its slot and P is pruned, so the member
/// cap never counts P: applying the same delta twice changes nothing, and
/// the delta path heals to the merge path.
#[test]
fn the_member_cap_counts_only_members_cleanup_keeps() {
    let r = Room::new();
    let p = sk(0x41);
    let q = sk(0x42);
    let k = sk(0x43);
    let mut x = ChatRoomStateV1 {
        configuration: r.cfg(2, 10, 2, 1),
        ..Default::default()
    };
    x.members = MembersV1 {
        members: vec![r.member(&p, &r.owner)],
    };
    x.recent_messages.messages = vec![r.msg(&p, 100)];
    x.verify(&x, &r.params).expect("x valid");
    let x = r.merge(&x, &x);
    let mut y = ChatRoomStateV1 {
        configuration: r.cfg(1, 10, 10, 10),
        ..Default::default()
    };
    y.members = MembersV1 {
        members: vec![r.member(&q, &r.owner), r.member(&k, &q)],
    };
    y.recent_messages.messages = vec![r.msg(&q, 200), r.msg(&k, 300)];
    y.verify(&y, &r.params).expect("y valid");
    let y = r.merge(&y, &y);

    let (d, once) = r.network_delta(&x, &y);
    let mut twice = once.clone();
    twice.apply_delta(&once, &r.params, &d).unwrap();
    assert!(
        ser(&twice) == ser(&once),
        "applying the delta again changes nothing"
    );
    let xy = r.merge(&x, &y);
    assert!(ser(&once) == ser(&xy), "delta path == merge path");
    assert!(ser(&r.merge(&once, &y)) == ser(&xy), "heals to the merge");
    assert!(ser(&r.merge(&xy, &y)) == ser(&xy), "absorbs Y");
    assert!(ser(&xy) == ser(&r.merge(&y, &x)), "commutes");
    let ids: Vec<MemberId> = xy.members.members.iter().map(|m| m.member.id()).collect();
    assert!(
        ids.contains(&k.verifying_key().into()) && ids.contains(&q.verifying_key().into()),
        "K (newest message) and its inviter Q fill the member cap"
    );
}

/// P (owner-invited) and K (three invites down: A1 -> A2 -> K), and a room
/// lowered to `max_members = 2` with one slot for a message or a DM. K's
/// record is newest but K needs three member slots, so the member cap
/// removes K; the record kept is then P's older one, by a member that fits,
/// rather than nothing.
fn deep_and_shallow(r: &Room) -> (SigningKey, SigningKey, Vec<AuthorizedMember>) {
    let (p, a1, a2, k) = (sk(0x51), sk(0x52), sk(0x53), sk(0x54));
    let members = vec![
        r.member(&p, &r.owner),
        r.member(&a1, &r.owner),
        r.member(&a2, &a1),
        r.member(&k, &a2),
    ];
    (p, k, members)
}

fn lowered(
    r: &Room,
    held: &ChatRoomStateV1,
    f: impl FnOnce(&mut Configuration),
) -> ChatRoomStateV1 {
    let mut c = held.configuration.configuration.clone();
    c.configuration_version += 1;
    f(&mut c);
    let delta = ChatRoomStateV1Delta {
        configuration: Some(AuthorizedConfigurationV1::new(c, &r.owner)),
        ..Default::default()
    };
    let mut out = held.clone();
    out.apply_delta(held, &r.params, &Some(delta))
        .expect("apply");
    out.verify(&out, &r.params).expect("verifies");
    let mut again = out.clone();
    again.merge_from(&r.params, &out).expect("self-merge");
    assert!(ser(&again) == ser(&out), "a fixpoint");
    out
}

#[test]
fn lowering_the_member_and_message_caps_keeps_the_newest_message_whose_author_fits() {
    let r = Room::new();
    let (p, k, members) = deep_and_shallow(&r);
    let mut held = ChatRoomStateV1 {
        configuration: r.cfg(1, 10, 10, 10),
        members: MembersV1 { members },
        ..Default::default()
    };
    held.recent_messages.messages = vec![r.msg(&p, 100), r.msg(&k, 200)];
    let held = r.merge(&held, &held);
    assert_eq!(held.members.members.len(), 4, "premise");
    let out = lowered(&r, &held, |c| {
        c.max_members = 2;
        c.max_recent_messages = 1;
    });
    let ids: Vec<MemberId> = out.members.members.iter().map(|m| m.member.id()).collect();
    assert_eq!(ids, vec![MemberId::from(p.verifying_key())], "P stays");
    assert_eq!(out.recent_messages.messages.len(), 1, "P's message stays");
    assert_eq!(
        out.recent_messages.messages[0].message.author,
        MemberId::from(p.verifying_key())
    );
}

#[test]
fn lowering_the_member_and_dm_caps_keeps_the_newest_dm_whose_sender_fits() {
    use river_core::room_state::direct_messages::sign_direct_message;
    let r = Room::new();
    let (p, k, members) = deep_and_shallow(&r);
    let dm = |from: &SigningKey, ts: u64| {
        sign_direct_message(
            from,
            from.verifying_key().into(),
            r.oid,
            &r.params.owner,
            1_700_000_000 + ts,
            vec![1; 8],
        )
        .expect("sign dm")
    };
    let mut held = ChatRoomStateV1 {
        configuration: r.cfg(1, 10, 10, 10),
        members: MembersV1 { members },
        ..Default::default()
    };
    held.direct_messages.messages = vec![dm(&p, 100), dm(&k, 200)];
    let held = r.merge(&held, &held);
    assert_eq!(held.members.members.len(), 4, "premise");
    assert_eq!(held.direct_messages.messages.len(), 2, "premise");
    let out = lowered(&r, &held, |c| {
        c.max_members = 2;
        c.max_direct_messages = Some(1);
    });
    let ids: Vec<MemberId> = out.members.members.iter().map(|m| m.member.id()).collect();
    assert_eq!(ids, vec![MemberId::from(p.verifying_key())], "P stays");
    assert_eq!(out.direct_messages.messages.len(), 1, "P's DM stays");
    assert_eq!(
        out.direct_messages.messages[0].message.sender,
        MemberId::from(p.verifying_key())
    );
}

/// `MembersV1::verify` refuses a member set over `max_members`. Between the
/// field applies and cleanup the set may exceed it, but nothing verifies
/// there, and every state cleanup returns is within it: here ten members,
/// each with a message, in a room lowered to three, by a configuration delta
/// and by a whole-state merge.
#[test]
fn every_state_cleanup_returns_is_within_max_members() {
    let r = Room::new();
    let keys: Vec<SigningKey> = (0..10u8).map(|i| sk(0x60 + i)).collect();
    let members: Vec<AuthorizedMember> = keys
        .iter()
        .enumerate()
        .map(|(i, k)| r.member(k, if i < 3 { &r.owner } else { &keys[i % 3] }))
        .collect();
    let mut held = ChatRoomStateV1 {
        configuration: r.cfg(1, 10, 20, 20),
        members: MembersV1 { members },
        ..Default::default()
    };
    held.recent_messages.messages = keys
        .iter()
        .enumerate()
        .map(|(i, k)| r.msg(k, 100 + i as u64))
        .collect();
    let held = r.merge(&held, &held);
    assert_eq!(held.members.members.len(), 10, "premise");
    let by_delta = lowered(&r, &held, |c| c.max_members = 3);
    assert!(by_delta.members.members.len() <= 3);
    let lower = ChatRoomStateV1 {
        configuration: r.cfg(2, 10, 3, 20),
        ..Default::default()
    };
    let lower = r.merge(&lower, &lower);
    for merged in [r.merge(&held, &lower), r.merge(&lower, &held)] {
        assert!(merged.members.members.len() <= 3);
        merged.verify(&merged, &r.params).expect("verifies");
    }
}
