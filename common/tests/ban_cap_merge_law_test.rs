//! Merge laws under `max_user_bans` cap pressure (freenet/river#796, #703).
//!
//! `BansV1::enforce_user_ban_cap` judges each ban against the member set,
//! ban evidence and deputy grants it is handed: a ban is live or dead, and in
//! the owner pool or the other pool, depending on state outside the ban list.
//! Two peers holding full ban lists can therefore classify the same ban
//! differently before they merge. These tests build such pairs from a common
//! base, each side at or over its cap with bans the other side judges
//! differently (an owner ban on a banner kills the banner's bans on one side
//! only, inviter bans and bans on ids held nowhere are live on one side
//! only), and require:
//!
//! - `merge(A, B) == merge(B, A)`, byte for byte;
//! - the merged state verifies;
//! - `merge(M, M) == M` for the merged state, and `post_apply_cleanup` is a
//!   fixpoint on it.

use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use river_core::room_state::ban::{AuthorizedUserBan, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersV1};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo, MemberInfoV1};
use river_core::room_state::message::{
    AuthorizedMessageV1, MessageV1, MessagesV1, RoomMessageBody,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use std::time::{Duration, SystemTime};

fn ser(s: &ChatRoomStateV1) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(s, &mut out).unwrap();
    out
}

fn at(t: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + t)
}

fn key(seed: u64, i: u64) -> SigningKey {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&seed.to_le_bytes());
    b[8..16].copy_from_slice(&i.to_le_bytes());
    b[31] = 0x5a;
    SigningKey::from_bytes(&b)
}

/// The owner; `P` members invited by the owner (P0 a deputy of the owner);
/// `Q` members invited by P1, so P1 may ban them as their inviter. Every
/// member has posted, so no one is pruned for inactivity.
struct World {
    owner_sk: SigningKey,
    owner_id: MemberId,
    params: ChatRoomParametersV1,
    p: Vec<(SigningKey, MemberId)>,
    q: Vec<(SigningKey, MemberId)>,
}

fn world(seed: u64, cap: usize) -> (World, ChatRoomStateV1) {
    let owner_sk = key(seed, 0);
    let owner_id = MemberId::from(&owner_sk.verifying_key());
    let params = ChatRoomParametersV1 {
        owner: owner_sk.verifying_key(),
    };
    let mut members = Vec::new();
    let mut messages = Vec::new();
    let mut add = |sk: &SigningKey, inviter: MemberId, inviter_sk: &SigningKey, t: u64| {
        let m = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: inviter,
                member_vk: sk.verifying_key(),
            },
            inviter_sk,
        );
        let id = m.member.id();
        members.push(m);
        messages.push(AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: id,
                time: at(t),
                content: RoomMessageBody::public(format!("hi {t}")),
            },
            sk,
        ));
        id
    };
    let p: Vec<_> = (0..4)
        .map(|i| {
            let sk = key(seed, 10 + i);
            let id = add(&sk, owner_id, &owner_sk, i);
            (sk, id)
        })
        .collect();
    let q: Vec<_> = (0..4)
        .map(|i| {
            let sk = key(seed, 20 + i);
            let id = add(&sk, p[1].1, &p[1].0, 10 + i);
            (sk, id)
        })
        .collect();
    let mut owner_info = MemberInfo::new_public(owner_id, 1, "owner".into());
    owner_info.deputies = vec![p[0].1];
    let mut state = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: owner_id,
                max_user_bans: cap,
                max_members: 50,
                max_recent_messages: 100,
                ..Default::default()
            },
            &owner_sk,
        ),
        members: MembersV1 { members },
        member_info: MemberInfoV1 {
            member_info: vec![AuthorizedMemberInfo::new_with_member_key(
                owner_info, &owner_sk,
            )],
        },
        recent_messages: MessagesV1 {
            messages,
            ..Default::default()
        },
        ..Default::default()
    };
    state.recent_messages.rebuild_actions_state();
    state.post_apply_cleanup(&params).unwrap();
    state.verify(&state, &params).expect("the base verifies");
    (
        World {
            owner_sk,
            owner_id,
            params,
            p,
            q,
        },
        state,
    )
}

/// One ban an honest or careless client could send, chosen so that sides
/// judge it differently: owner bans (including on banners), the owner's
/// deputy banning anyone, an inviter banning its invitees, members banning
/// ids held nowhere, and repeated (issuer, target) pairs.
fn random_ban(w: &World, rng: &mut StdRng, seed: u64) -> AuthorizedUserBan {
    let t = rng.gen_range(0..200);
    let absent = MemberId::from(&key(seed, 1_000 + rng.gen_range(0..6)).verifying_key());
    let pick_p = |rng: &mut StdRng| w.p[rng.gen_range(0..w.p.len())].1;
    let pick_q = |rng: &mut StdRng| w.q[rng.gen_range(0..w.q.len())].1;
    let ban = |target: MemberId| UserBan {
        owner_member_id: w.owner_id,
        banned_at: at(t),
        banned_user: target,
    };
    match rng.gen_range(0..6) {
        0 => {
            let target = if rng.gen_bool(0.5) {
                pick_p(rng)
            } else {
                pick_q(rng)
            };
            AuthorizedUserBan::new(ban(target), w.owner_id, &w.owner_sk)
        }
        1 => AuthorizedUserBan::new(ban(absent), w.owner_id, &w.owner_sk),
        2 => {
            let target = if rng.gen_bool(0.5) {
                pick_p(rng)
            } else {
                pick_q(rng)
            };
            AuthorizedUserBan::new(ban(target), w.p[0].1, &w.p[0].0)
        }
        3 => AuthorizedUserBan::new(ban(pick_q(rng)), w.p[1].1, &w.p[1].0),
        _ => {
            let i = rng.gen_range(0..w.p.len());
            AuthorizedUserBan::new(ban(absent), w.p[i].1, &w.p[i].0)
        }
    }
}

/// Apply `ops` single-ban deltas, keeping each only if it applies and the
/// result verifies, as an honest peer would.
fn grow(
    w: &World,
    mut s: ChatRoomStateV1,
    rng: &mut StdRng,
    seed: u64,
    ops: usize,
) -> ChatRoomStateV1 {
    for _ in 0..ops {
        let delta = ChatRoomStateV1Delta {
            bans: Some(vec![random_ban(w, rng, seed)]),
            ..Default::default()
        };
        let mut next = s.clone();
        if next.apply_delta(&s, &w.params, &Some(delta)).is_ok()
            && next.verify(&next, &w.params).is_ok()
        {
            s = next;
        }
    }
    s
}

fn merged(w: &World, a: &ChatRoomStateV1, b: &ChatRoomStateV1) -> Result<ChatRoomStateV1, String> {
    let mut m = a.clone();
    m.merge(a, &w.params, b)?;
    Ok(m)
}

/// Whether the two sides' ban resolutions remove different members, so a ban
/// one side counts live the other can count dead: the case this file exists
/// for.
fn sides_disagree(w: &World, a: &ChatRoomStateV1, b: &ChatRoomStateV1) -> bool {
    let removed = |s: &ChatRoomStateV1| {
        s.members
            .resolve_bans(&s.ban_evidence, &s.bans, &s.member_info, &w.params)
            .removed
    };
    removed(a) != removed(b)
}

#[test]
fn full_ban_lists_judged_differently_merge_the_same_both_ways() {
    let seeds: u64 = std::env::var("BAN_CAP_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40);
    let mut failures = Vec::new();
    let mut disagreeing = 0;
    let mut over_cap_union = 0;
    for seed in 0..seeds {
        for cap in [2usize, 3, 4, 5] {
            let (w, base) = world(seed, cap);
            let mut rng = StdRng::seed_from_u64(seed * 16 + cap as u64);
            let base = grow(&w, base, &mut rng, seed, 3);
            let a = grow(&w, base.clone(), &mut rng, seed, 6);
            let b = grow(&w, base.clone(), &mut rng, seed, 6);
            if sides_disagree(&w, &a, &b) {
                disagreeing += 1;
            }
            let mut union: Vec<_> = a.bans.0.clone();
            union.extend(b.bans.0.iter().filter(|x| !a.bans.0.contains(x)).cloned());
            if union.len() > cap {
                over_cap_union += 1;
            }
            let (ab, ba) = match (merged(&w, &a, &b), merged(&w, &b, &a)) {
                (Ok(ab), Ok(ba)) => (ab, ba),
                (x, y) => {
                    failures.push(format!(
                        "seed {seed} cap {cap}: merge failed: {:?} / {:?}",
                        x.err(),
                        y.err()
                    ));
                    continue;
                }
            };
            if ser(&ab) != ser(&ba) {
                failures.push(format!(
                    "seed {seed} cap {cap}: merge(A, B) != merge(B, A); bans {:?} vs {:?}",
                    ab.bans
                        .0
                        .iter()
                        .map(|b| b.ban.banned_at)
                        .collect::<Vec<_>>(),
                    ba.bans
                        .0
                        .iter()
                        .map(|b| b.ban.banned_at)
                        .collect::<Vec<_>>(),
                ));
                continue;
            }
            if let Err(e) = ab.verify(&ab, &w.params) {
                failures.push(format!(
                    "seed {seed} cap {cap}: merged state fails verify: {e}"
                ));
            }
            match merged(&w, &ab, &ab) {
                Ok(again) if ser(&again) == ser(&ab) => {}
                _ => failures.push(format!("seed {seed} cap {cap}: merge(M, M) != M")),
            }
            let mut again = ab.clone();
            again.post_apply_cleanup(&w.params).unwrap();
            if ser(&again) != ser(&ab) {
                failures.push(format!("seed {seed} cap {cap}: cleanup is not a fixpoint"));
            }
        }
    }
    let runs = seeds * 4;
    println!("{runs} pairs: {disagreeing} judged bans differently, {over_cap_union} had a ban union over the cap");
    // Floors: the generator must keep producing the cases this file is for.
    assert!(
        disagreeing * 4 > runs,
        "too few disagreeing pairs: {disagreeing} of {runs}"
    );
    assert!(
        over_cap_union * 2 > runs,
        "too few over-cap unions: {over_cap_union} of {runs}"
    );
    assert!(
        failures.is_empty(),
        "{} of {runs} pairs fail:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
