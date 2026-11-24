//! An Official-room-shaped fixture: 187 members, 400 owner bans with their
//! evidence, a 200-message window and a full provisional pool. Shared by the
//! incremental-validation tests and the cost bench (`#[path]` include).
#![allow(dead_code)]
use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use river_core::room_state::ban::{AuthorizedUserBan, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersDelta};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
use river_core::room_state::provisional::{
    AdmissionEvidence, AuthorizedProvisionalBan, AuthorizedProvisionalMember, PowPolicy,
    ProvisionalBan, ProvisionalDelta, ProvisionalMember, PublicRoomPolicy,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use std::time::{Duration, SystemTime};

pub const WINDOW: usize = 200;
pub const MEMBERS: usize = 187;
pub const BANS: usize = 400;
pub const PROV_BANS: usize = 16;

pub fn sk(n: u32) -> SigningKey {
    let mut b = [0u8; 32];
    b[..4].copy_from_slice(&n.to_le_bytes());
    b[31] = 9;
    SigningKey::from_bytes(&b)
}
pub fn t0() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000)
}
pub fn msg(owner_id: MemberId, k: &SigningKey, t: u64) -> AuthorizedMessageV1 {
    AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: owner_id,
            author: k.verifying_key().into(),
            time: t0() + Duration::from_secs(t),
            content: RoomMessageBody::public(format!(
                "benchmark message number {t} with a typical sentence of chat text in it"
            )),
        },
        k,
    )
}
pub fn info(k: &SigningKey, name: &str) -> AuthorizedMemberInfo {
    AuthorizedMemberInfo::new_with_member_key(
        MemberInfo::new_public(k.verifying_key().into(), 1, name.to_string()),
        k,
    )
}
pub fn entry(params: &ChatRoomParametersV1, k: &SigningKey) -> AuthorizedProvisionalMember {
    AuthorizedProvisionalMember::new(
        &params.owner,
        ProvisionalMember {
            member_vk: k.verifying_key(),
            evidence: AdmissionEvidence::Pow { nonce: 0 },
        },
        k,
    )
}
pub fn cbor<T: serde::Serialize>(v: &T) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(v, &mut out).unwrap();
    out
}

pub struct Fix {
    pub state: ChatRoomStateV1,
    pub params: ChatRoomParametersV1,
    pub members: Vec<SigningKey>,
    pub pool: Vec<SigningKey>,
}

pub fn build(pool: usize) -> Fix {
    let owner = sk(1_000_000);
    let params = ChatRoomParametersV1 {
        owner: owner.verifying_key(),
    };
    let owner_id = params.owner_id();
    let prov_quota = WINDOW / 5; // 20% share
    let mut state = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: owner_id,
                max_recent_messages: WINDOW,
                max_members: 2000,
                max_user_bans: BANS,
                public_admission: Some(PublicRoomPolicy {
                    listed: false,
                    max_provisional: pool as u32,
                    max_provisional_messages: prov_quota as u32,
                    max_provisional_bans: PROV_BANS as u32,
                    pow: Some(PowPolicy {
                        difficulty_bits: 0,
                        epoch: 1,
                    }),
                }),
                ..Default::default()
            },
            &owner,
        ),
        ..Default::default()
    };
    let apply = |state: &mut ChatRoomStateV1, d: ChatRoomStateV1Delta| {
        let parent = state.clone();
        state
            .apply_delta(&parent, &params, &Some(d))
            .expect("fixture delta applies");
    };
    // Invite tree: 0 = "Invite Bot" (depth 1, invited by the owner); 1..=9 invited by
    // the bot (depth 2); 10..=26 invited by 1..=9 (depth 3); authors 27..MEMBERS invited
    // by 0..=26 round-robin (depth 2..4). Banned keys MEMBERS.. are invited by the bot.
    // A member with no message and no invitee with a message is pruned, so every
    // inviter keeps at least one author below it.
    let all: Vec<SigningKey> = (0..(MEMBERS + BANS) as u32).map(sk).collect();
    let inviter_of = |i: usize| -> usize {
        if i == 0 {
            usize::MAX
        } else if i <= 9 {
            0
        } else if i <= 26 {
            1 + (i % 9)
        } else if i < MEMBERS {
            i % 27
        } else {
            0
        }
    };
    let mk_member = |i: usize| {
        let by = inviter_of(i);
        let (by_id, by_key) = if by == usize::MAX {
            (owner_id, &owner)
        } else {
            (MemberId::from(all[by].verifying_key()), &all[by])
        };
        AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: by_id,
                member_vk: all[i].verifying_key(),
            },
            by_key,
        )
    };
    let members: Vec<SigningKey> = all[..MEMBERS].to_vec();
    let mut t = 0u64;
    // Real members, in groups closed under "inviter present when applied".
    let mut groups: Vec<Vec<usize>> = vec![(0..=9).collect()];
    for j in 10..=26 {
        groups.push(vec![j]);
    }
    // each author joins the group of its inviter: attach authors to the group holding the inviter
    for i in 27..MEMBERS {
        let by = inviter_of(i);
        let g = groups.iter_mut().find(|g| g.contains(&by)).unwrap();
        g.push(i);
    }
    for g in &groups {
        let mut msgs = Vec::new();
        for &i in g.iter().filter(|&&i| i >= 27) {
            t += 1;
            msgs.push(msg(owner_id, &all[i], t));
        }
        apply(
            &mut state,
            ChatRoomStateV1Delta {
                members: Some(MembersDelta::new(g.iter().map(|&i| mk_member(i)).collect())),
                member_info: Some(
                    g.iter()
                        .map(|&i| info(&all[i], &format!("user{i}")))
                        .collect(),
                ),
                recent_messages: Some(msgs),
                ..Default::default()
            },
        );
    }
    // Banned members: join with one message each, then the owner bans them (evidence kept).
    for chunk in (MEMBERS..MEMBERS + BANS).collect::<Vec<_>>().chunks(40) {
        let mut msgs = Vec::new();
        for &i in chunk {
            t += 1;
            msgs.push(msg(owner_id, &all[i], t));
        }
        apply(
            &mut state,
            ChatRoomStateV1Delta {
                members: Some(MembersDelta::new(
                    chunk.iter().map(|&i| mk_member(i)).collect(),
                )),
                member_info: Some(
                    chunk
                        .iter()
                        .map(|&i| info(&all[i], &format!("spam{i}")))
                        .collect(),
                ),
                recent_messages: Some(msgs),
                ..Default::default()
            },
        );
        let bans: Vec<AuthorizedUserBan> = chunk
            .iter()
            .map(|&i| {
                AuthorizedUserBan::new(
                    UserBan {
                        owner_member_id: owner_id,
                        banned_at: t0(),
                        banned_user: all[i].verifying_key().into(),
                    },
                    owner_id,
                    &owner,
                )
            })
            .collect();
        apply(
            &mut state,
            ChatRoomStateV1Delta {
                bans: Some(bans),
                ..Default::default()
            },
        );
    }
    let pool_keys: Vec<SigningKey> = (0..pool as u32).map(|i| sk(500_000 + i)).collect();
    let pbans: Vec<AuthorizedProvisionalBan> = (0..PROV_BANS as u32)
        .map(|i| {
            AuthorizedProvisionalBan::new(
                &params.owner,
                ProvisionalBan {
                    banned_by: owner_id,
                    banned_at: t0(),
                    target: sk(900_000 + i).verifying_key().into(),
                },
                &owner,
            )
        })
        .collect();
    let mut pm = Vec::new();
    for j in 0..prov_quota {
        t += 1;
        pm.push(msg(owner_id, &pool_keys[j % pool], t));
    }
    apply(
        &mut state,
        ChatRoomStateV1Delta {
            provisional: Some(ProvisionalDelta {
                entries: pool_keys.iter().map(|k| entry(&params, k)).collect(),
                bans: pbans,
            }),
            member_info: Some(pool_keys.iter().map(|k| info(k, "newcomer")).collect()),
            recent_messages: Some(pm),
            ..Default::default()
        },
    );
    assert_eq!(state.provisional.entries.len(), pool, "pool full");
    assert_eq!(state.provisional.bans.len(), PROV_BANS, "prov bans full");
    Fix {
        state,
        params,
        members,
        pool: pool_keys,
    }
}
