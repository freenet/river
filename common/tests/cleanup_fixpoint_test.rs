//! States an apply or merge produces are fixpoints of `merge(S, S)`
//! (freenet/river#703): cleanup leaves nothing a later apply would drop.

use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_scaffold::ComposableState;
use river_core::room_state::ban::{AuthorizedUserBan, BansV1, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersV1};
use river_core::room_state::message::{
    AuthorizedMessageV1, MessageV1, MessagesV1, RoomMessageBody,
};
use river_core::room_state::privacy::RoomCipherSpec;
use river_core::room_state::secret::*;
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use std::time::{Duration, SystemTime};

fn at(t: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + t)
}
fn sk(b: u8) -> SigningKey {
    SigningKey::from_bytes(&[b; 32])
}
fn id(s: &SigningKey) -> MemberId {
    s.verifying_key().into()
}
fn rec(
    owner: MemberId,
    inviter: MemberId,
    vk: VerifyingKey,
    inviter_sk: &SigningKey,
) -> AuthorizedMember {
    AuthorizedMember::new(
        Member {
            owner_member_id: owner,
            invited_by: inviter,
            member_vk: vk,
        },
        inviter_sk,
    )
}
fn msg(owner: MemberId, s: &SigningKey, t: u64) -> AuthorizedMessageV1 {
    AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: owner,
            author: id(s),
            time: at(t),
            content: RoomMessageBody::public(format!("m{t}")),
        },
        s,
    )
}
fn ban(owner: MemberId, by: &SigningKey, target: MemberId, t: u64) -> AuthorizedUserBan {
    AuthorizedUserBan::new(
        UserBan {
            owner_member_id: owner,
            banned_at: at(t),
            banned_user: target,
        },
        id(by),
        by,
    )
}
fn vrec(o: &SigningKey, v: u32, secs: u64) -> AuthorizedSecretVersionRecord {
    AuthorizedSecretVersionRecord::new(
        SecretVersionRecordV1 {
            version: v,
            cipher_spec: RoomCipherSpec::Aes256Gcm,
            created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
        },
        o,
    )
}
fn blob(o: &SigningKey, member: MemberId, v: u32, fill: u8) -> AuthorizedEncryptedSecretForMember {
    AuthorizedEncryptedSecretForMember::new(
        EncryptedSecretForMemberV1 {
            member_id: member,
            secret_version: v,
            ciphertext: vec![fill; 48],
            nonce: [fill; 12],
            sender_ephemeral_public_key: [fill; 32],
            provider: id(o),
        },
        o,
    )
}
fn merge(p: &ChatRoomParametersV1, a: &ChatRoomStateV1, b: &ChatRoomStateV1) -> ChatRoomStateV1 {
    let mut x = a.clone();
    x.merge_from(p, b).unwrap();
    x
}

/// A member removed by `post_apply_cleanup` (here by their inviter's ban,
/// which only cleanup resolves) loses their secret blob in the same apply,
/// so the state an apply produces is a fixpoint of `merge(S, S)`, as on the
/// whole-state merge path (freenet/river#703).
#[test]
fn a_member_removed_in_cleanup_loses_their_secret_blob_in_the_same_apply() {
    let o = sk(1);
    let owner = id(&o);
    let p = ChatRoomParametersV1 {
        owner: o.verifying_key(),
    };
    let (x, y) = (sk(2), sk(3));
    let mut s = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: owner,
                ..Default::default()
            },
            &o,
        ),
        members: MembersV1 {
            members: vec![
                rec(owner, owner, x.verifying_key(), &o),
                rec(owner, id(&x), y.verifying_key(), &x),
            ],
        },
        recent_messages: MessagesV1 {
            messages: vec![msg(owner, &x, 1), msg(owner, &y, 2)],
            ..Default::default()
        },
        secrets: RoomSecretsV1 {
            current_version: 1,
            versions: vec![vrec(&o, 1, 1)],
            encrypted_secrets: vec![blob(&o, id(&x), 1, 7), blob(&o, id(&y), 1, 9)],
        },
        ..Default::default()
    };
    let s0 = s.clone();
    s = merge(&p, &s0, &s0);
    s.verify(&s, &p).unwrap();
    assert_eq!(s.members.members.len(), 2, "premise");
    assert_eq!(s.secrets.encrypted_secrets.len(), 2, "premise");

    // X, Y's inviter, bans Y in a delta.
    let mut r = s.clone();
    r.apply_delta(
        &s,
        &p,
        &Some(ChatRoomStateV1Delta {
            bans: Some(vec![ban(owner, &x, id(&y), 10)]),
            ..Default::default()
        }),
    )
    .unwrap();
    assert_eq!(r.members.members.len(), 1, "premise: Y is removed");
    assert_eq!(r.secrets.encrypted_secrets.len(), 1, "Y's blob goes with Y");
    assert_eq!(merge(&p, &r, &r), r, "merge(R, R) == R");

    // The same through a whole-state merge.
    let mut t = s.clone();
    t.bans = BansV1(vec![ban(owner, &x, id(&y), 10)]);
    let m = merge(&p, &s, &t);
    assert_eq!(merge(&p, &m, &m), m);
    assert_eq!(m, r);
}

fn base(
    o: &SigningKey,
    owner: MemberId,
    members: Vec<AuthorizedMember>,
    msgs: Vec<AuthorizedMessageV1>,
    secrets: RoomSecretsV1,
) -> ChatRoomStateV1 {
    ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: owner,
                ..Default::default()
            },
            o,
        ),
        members: MembersV1 { members },
        recent_messages: MessagesV1 {
            messages: msgs,
            ..Default::default()
        },
        secrets,
        ..Default::default()
    }
}

/// A member pruned for inactivity (step 3: no message, no DM, and a blob only
/// at an OLD secret version, which does not exempt them) loses that blob in
/// the same apply.
#[test]
fn a_member_pruned_for_inactivity_loses_their_stale_secret_blob_in_the_same_apply() {
    let o = sk(1);
    let owner = id(&o);
    let p = ChatRoomParametersV1 {
        owner: o.verifying_key(),
    };
    let (x, y) = (sk(2), sk(3));
    let s0 = base(
        &o,
        owner,
        vec![
            rec(owner, owner, x.verifying_key(), &o),
            rec(owner, owner, y.verifying_key(), &o),
        ],
        vec![msg(owner, &x, 1)],
        RoomSecretsV1 {
            current_version: 2,
            versions: vec![vrec(&o, 1, 1), vrec(&o, 2, 2)],
            encrypted_secrets: vec![
                blob(&o, id(&x), 2, 7),
                blob(&o, id(&y), 1, 9),
                blob(&o, owner, 2, 5),
            ],
        },
    );
    s0.verify(&s0, &p).expect("premise");
    let r = merge(&p, &s0, &s0);
    assert_eq!(
        r.members.members.len(),
        1,
        "premise: Y is pruned for inactivity"
    );
    assert!(
        !r.secrets
            .encrypted_secrets
            .iter()
            .any(|b| b.secret.member_id == id(&y)),
        "Y's blob goes with Y"
    );
    assert_eq!(merge(&p, &r, &r), r, "merge(R, R) == R");
    // And through post_apply_cleanup alone.
    let mut c = s0.clone();
    c.post_apply_cleanup(&p).unwrap();
    assert!(!c
        .secrets
        .encrypted_secrets
        .iter()
        .any(|b| b.secret.member_id == id(&y)));
}

/// The owner's own blob is never dropped by cleanup or by an apply.
#[test]
fn the_owners_secret_blob_survives_cleanup_and_merges() {
    let o = sk(1);
    let owner = id(&o);
    let p = ChatRoomParametersV1 {
        owner: o.verifying_key(),
    };
    let x = sk(2);
    let s0 = base(
        &o,
        owner,
        vec![rec(owner, owner, x.verifying_key(), &o)],
        vec![msg(owner, &x, 1)],
        RoomSecretsV1 {
            current_version: 1,
            versions: vec![vrec(&o, 1, 1)],
            encrypted_secrets: vec![blob(&o, id(&x), 1, 7), blob(&o, owner, 1, 5)],
        },
    );
    s0.verify(&s0, &p).expect("premise");
    let owner_blobs = |s: &ChatRoomStateV1| {
        s.secrets
            .encrypted_secrets
            .iter()
            .filter(|b| b.secret.member_id == owner)
            .count()
    };
    assert_eq!(owner_blobs(&s0), 1, "premise");
    let r = merge(&p, &s0, &s0);
    assert_eq!(owner_blobs(&r), 1, "merge(S, S) keeps the owner's blob");
    let mut c = s0.clone();
    c.post_apply_cleanup(&p).unwrap();
    assert_eq!(owner_blobs(&c), 1, "cleanup keeps the owner's blob");
}
