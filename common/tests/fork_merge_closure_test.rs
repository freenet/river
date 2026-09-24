//! Randomized fork-merge closure test for freenet/river#423 ("divergent states
//! permanently reject each other's deltas").
//!
//! Two peers start from a common room state and diverge by applying
//! independent, individually valid operation sequences through the REAL
//! `apply_delta` path: joins, messages, member-info/deputy updates, owner
//! configuration changes, owner/member bans, and inactivity pruning driven by
//! a small message cap. Then they gossip their full states to each other.
//!
//! The law checked is closure over time: two VALID states must reach a common
//! state. A merge `Err` leaves a peer on its own state, so if it recurs on
//! every exchange the peers never converge. That permanent fork is #423.
//!
//! The deterministic, minimised cases live in `mutual_rejection_test.rs`.

use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use river_core::room_state::ban::{AuthorizedUserBan, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersDelta};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use std::collections::HashMap;
use std::time::{Duration, SystemTime};

struct PoolMember {
    sk: SigningKey,
    id: MemberId,
    inviter: Option<usize>, // None = invited by owner
    auth: AuthorizedMember,
}

struct World {
    owner_sk: SigningKey,
    owner_id: MemberId,
    params: ChatRoomParametersV1,
    pool: Vec<PoolMember>,
    base_time: SystemTime,
}

fn key(rng: &mut StdRng) -> SigningKey {
    let mut b = [0u8; 32];
    rng.fill(&mut b);
    SigningKey::from_bytes(&b)
}

impl World {
    fn new(rng: &mut StdRng, pool_size: usize) -> Self {
        let owner_sk = key(rng);
        let owner_id: MemberId = owner_sk.verifying_key().into();
        let params = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };
        let mut pool: Vec<PoolMember> = Vec::new();
        for i in 0..pool_size {
            let sk = key(rng);
            let inviter = if i == 0 || rng.gen_bool(0.5) {
                None
            } else {
                Some(rng.gen_range(0..i))
            };
            let (inviter_id, inviter_sk) = match inviter {
                None => (owner_id, &owner_sk),
                Some(j) => (pool[j].id, &pool[j].sk),
            };
            let member = Member {
                owner_member_id: owner_id,
                invited_by: inviter_id,
                member_vk: sk.verifying_key(),
            };
            let auth = AuthorizedMember::new(member, inviter_sk);
            let id = sk.verifying_key().into();
            pool.push(PoolMember {
                sk,
                id,
                inviter,
                auth,
            });
        }
        World {
            owner_sk,
            owner_id,
            params,
            pool,
            base_time: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
        }
    }

    fn initial_state(&self, max_recent_messages: usize, max_user_bans: usize) -> ChatRoomStateV1 {
        ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(
                Configuration {
                    max_members: 50,
                    max_user_bans,
                    max_recent_messages,
                    max_message_size: 1000,
                    ..Default::default()
                },
                &self.owner_sk,
            ),
            ..Default::default()
        }
    }

    /// Pool index chain from the owner down to `i` (ancestors first).
    fn chain(&self, i: usize) -> Vec<usize> {
        let mut c = vec![i];
        let mut cur = i;
        while let Some(p) = self.pool[cur].inviter {
            c.push(p);
            cur = p;
        }
        c.reverse();
        c
    }

    fn msg(&self, author: Option<usize>, t: u64, text: &str) -> AuthorizedMessageV1 {
        let (author_id, sk) = match author {
            None => (self.owner_id, &self.owner_sk),
            Some(i) => (self.pool[i].id, &self.pool[i].sk),
        };
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: self.owner_id,
                author: author_id,
                time: self.base_time + Duration::from_secs(t),
                content: RoomMessageBody::public(text.to_string()),
            },
            sk,
        )
    }
}

#[derive(Default, Clone)]
struct ForkClock {
    info_version: HashMap<usize, u32>,
    config_offset: u32,
}

/// Build one random, individually valid operation for `state` as a delta.
///
/// The ORDER of RNG draws here is load-bearing for the seeds pinned in
/// `adversarial_forks_never_permanently_reject_each_other`: reordering a draw,
/// adding one, or changing a probability redraws every seed. Re-validate the
/// pinned seeds against the pre-fix code after any such edit.
fn random_op(
    w: &World,
    rng: &mut StdRng,
    state: &ChatRoomStateV1,
    clock: &mut ForkClock,
    t: u64,
    mode: Mode,
) -> ChatRoomStateV1Delta {
    let present: Vec<usize> = (0..w.pool.len())
        .filter(|&i| {
            state
                .members
                .members
                .iter()
                .any(|m| m.member.id() == w.pool[i].id)
        })
        .collect();
    let n = w.pool.len();
    match rng.gen_range(0..100) {
        // Join (or rejoin) pool member i with its whole invite chain, a
        // self-signed member_info, and a message so it survives cleanup.
        0..=34 => {
            let i = rng.gen_range(0..n);
            let chain = w.chain(i);
            let members: Vec<AuthorizedMember> =
                chain.iter().map(|&c| w.pool[c].auth.clone()).collect();
            let mut infos = Vec::new();
            let mut msgs = Vec::new();
            for &c in &chain {
                let v = clock.info_version.entry(c).or_insert(0);
                *v += 1;
                let mut info = MemberInfo::new_public(w.pool[c].id, *v, format!("m{c}"));
                if rng.gen_bool(0.3) {
                    let d = rng.gen_range(0..n);
                    info.deputies = vec![w.pool[d].id];
                }
                infos.push(AuthorizedMemberInfo::new_with_member_key(
                    info,
                    &w.pool[c].sk,
                ));
                msgs.push(w.msg(Some(c), t, &format!("join {c}@{t}")));
            }
            ChatRoomStateV1Delta {
                members: Some(MembersDelta::new(members)),
                member_info: Some(infos),
                recent_messages: Some(msgs),
                ..Default::default()
            }
        }
        // Message from a present member, or the owner (owner messages push
        // older ones out of the small cap, which drives inactivity pruning).
        35..=64 => {
            let author = if present.is_empty() || rng.gen_bool(0.4) {
                None
            } else {
                Some(present[rng.gen_range(0..present.len())])
            };
            ChatRoomStateV1Delta {
                recent_messages: Some(vec![w.msg(author, t, &format!("hi@{t}"))]),
                ..Default::default()
            }
        }
        // member_info update by a present member, possibly naming a deputy.
        65..=79 => {
            if present.is_empty() {
                return ChatRoomStateV1Delta::default();
            }
            let i = present[rng.gen_range(0..present.len())];
            let v = clock.info_version.entry(i).or_insert(0);
            *v += 1;
            let mut info = MemberInfo::new_public(w.pool[i].id, *v, format!("m{i}v{v}"));
            if rng.gen_bool(0.6) {
                let d = rng.gen_range(0..n);
                info.deputies = vec![w.pool[d].id];
            }
            ChatRoomStateV1Delta {
                member_info: Some(vec![AuthorizedMemberInfo::new_with_member_key(
                    info,
                    &w.pool[i].sk,
                )]),
                ..Default::default()
            }
        }
        // The owner changes the room's limits (a real owner action that
        // removes members / messages / bans on every peer that applies it).
        80..=84 => {
            let mut c = state.configuration.configuration.clone();
            // Fork B's owner device uses a disjoint version stream. Two owner
            // devices bumping the SAME version concurrently is a separate
            // defect with a different cause (equal-version configurations are
            // never exchanged); it is tracked on its own issue, not here.
            let bump = if clock.config_offset > 0 {
                clock.config_offset
            } else {
                1
            };
            c.configuration_version += bump;
            c.max_members = rng.gen_range(2..8);
            c.max_recent_messages = rng.gen_range(1..6);
            c.max_user_bans = rng.gen_range(1..5);
            ChatRoomStateV1Delta {
                configuration: Some(AuthorizedConfigurationV1::new(c, &w.owner_sk)),
                ..Default::default()
            }
        }
        // A ban. HONEST mode (the default): issued by the owner or by a
        // present member, against a present member that banner is authorized
        // to ban IN THIS FORK'S VIEW -- what an honest client would send.
        // ADVERSARIAL mode: any pool member may sign a ban on anyone.
        _ => {
            let honest = mode == Mode::Honest;
            let by_id = state.members.members_by_member_id();
            let (banner, target) = if honest {
                let mut cands: Vec<(Option<usize>, usize)> = Vec::new();
                for &t in &present {
                    if river_core::room_state::member::MembersV1::is_ban_authorized(
                        w.owner_id,
                        w.pool[t].id,
                        &by_id,
                        &state.member_info,
                        w.owner_id,
                    ) {
                        cands.push((None, t));
                    }
                    for &b in &present {
                        if b != t
                            && river_core::room_state::member::MembersV1::is_ban_authorized(
                                w.pool[b].id,
                                w.pool[t].id,
                                &by_id,
                                &state.member_info,
                                w.owner_id,
                            )
                        {
                            cands.push((Some(b), t));
                        }
                    }
                }
                if cands.is_empty() {
                    return ChatRoomStateV1Delta::default();
                }
                // Prefer member-issued bans so deputy/ancestor authority is exercised.
                let member_cands: Vec<_> =
                    cands.iter().filter(|c| c.0.is_some()).cloned().collect();
                if !member_cands.is_empty() && rng.gen_bool(0.7) {
                    member_cands[rng.gen_range(0..member_cands.len())]
                } else {
                    cands[rng.gen_range(0..cands.len())]
                }
            } else {
                let b = if rng.gen_bool(0.4) {
                    None
                } else {
                    Some(rng.gen_range(0..n))
                };
                (b, rng.gen_range(0..n))
            };
            let ban = UserBan {
                owner_member_id: w.owner_id,
                banned_at: w.base_time + Duration::from_secs(t),
                banned_user: w.pool[target].id,
            };
            let auth = match banner {
                None => AuthorizedUserBan::new(ban, w.owner_id, &w.owner_sk),
                Some(b) => AuthorizedUserBan::new(ban, w.pool[b].id, &w.pool[b].sk),
            };
            ChatRoomStateV1Delta {
                bans: Some(vec![auth]),
                ..Default::default()
            }
        }
    }
}

fn apply(
    state: &mut ChatRoomStateV1,
    params: &ChatRoomParametersV1,
    d: ChatRoomStateV1Delta,
) -> bool {
    let parent = state.clone();
    let mut next = state.clone();
    match next.apply_delta(&parent, params, &Some(d)) {
        Ok(()) if next.verify(&next, params).is_ok() => {
            *state = next;
            true
        }
        _ => false, // a real peer rejects it and keeps its state
    }
}

fn merge(
    a: &ChatRoomStateV1,
    b: &ChatRoomStateV1,
    p: &ChatRoomParametersV1,
) -> Result<ChatRoomStateV1, String> {
    let mut s = a.clone();
    let parent = s.clone();
    s.merge(&parent, p, b)?;
    s.verify(&s, p)
        .map_err(|e| format!("merged state fails verify: {e}"))?;
    Ok(s)
}

fn ser(s: &ChatRoomStateV1) -> Vec<u8> {
    let mut v = vec![];
    ciborium::ser::into_writer(s, &mut v).unwrap();
    v
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    /// Every operation is one an honest client would send in its own view.
    Honest,
    /// Any pool member may also sign bans on anyone, present or not. Every
    /// record is still validly signed by its claimed author: this is what a
    /// current or former member can produce at will, not a forgery.
    Adversarial,
}

/// Shapes: (pool size, common-prefix ops, ops per fork, max_recent_messages,
/// max_user_bans). Small caps force inactivity pruning and ban-cap eviction,
/// which is where divergence comes from.
const SHAPES: [(usize, usize, usize, usize, usize); 3] =
    [(6, 8, 8, 4, 3), (8, 12, 12, 3, 2), (5, 4, 16, 2, 5)];

/// Build two forks from a common prefix.
fn fork_pair(
    seed: u64,
    shape: (usize, usize, usize, usize, usize),
    mode: Mode,
) -> (World, ChatRoomStateV1, ChatRoomStateV1) {
    let (pool, prefix, fork_ops, cap, bans) = shape;
    let mut rng = StdRng::seed_from_u64(seed);
    let w = World::new(&mut rng, pool);
    let mut base = w.initial_state(cap, bans);
    let mut clock = ForkClock::default();
    let mut t = 0u64;
    for _ in 0..prefix {
        t += 1;
        let d = random_op(&w, &mut rng, &base, &mut clock, t, mode);
        apply(&mut base, &w.params, d);
    }
    let (mut a, mut b) = (base.clone(), base);
    let (mut ca, mut cb) = (clock.clone(), clock);
    // Offset fork B's member_info and configuration versions so the two
    // forks' re-signed records differ, as two devices' would. Only members
    // already known at the split get the offset; one first seen after it
    // starts at version 1 on both forks, and the rank tie-break settles it.
    for v in cb.info_version.values_mut() {
        *v += 1000;
    }
    cb.config_offset = 1000;
    for _ in 0..fork_ops {
        t += 1;
        let d = random_op(&w, &mut rng, &a, &mut ca, t, mode);
        apply(&mut a, &w.params, d);
        t += 1;
        let d = random_op(&w, &mut rng, &b, &mut cb, t, mode);
        apply(&mut b, &w.params, d);
    }
    (w, a, b)
}

/// How a fork pair ends after gossiping both ways.
enum Ending {
    Converged,
    /// A merge still fails on the last exchange: a PERMANENT rejection, which
    /// is #423 itself.
    Rejecting(String),
    /// Every merge succeeds but the peers still differ. See the adversarial
    /// test for the one residual shape this is known to take.
    SilentlyDiverged,
}

/// Gossip both ways until the two peers agree. A peer whose merge fails keeps
/// its own state, exactly as a node does.
///
/// One-shot commutativity is deliberately NOT asserted here. Under these small
/// caps a single exchange can legitimately leave the peers differing (the
/// message retention horizon trades an extra round for never re-offering a
/// message the receiver would prune, freenet/river#703), and every such case
/// converges on the next exchange.
fn ending(w: &World, a: &ChatRoomStateV1, b: &ChatRoomStateV1) -> Ending {
    let (mut x, mut y) = (a.clone(), b.clone());
    let mut last_err = None;
    for _ in 0..6 {
        if ser(&x) == ser(&y) {
            return Ending::Converged;
        }
        last_err = None;
        let nx = match merge(&x, &y, &w.params) {
            Ok(s) => s,
            Err(e) => {
                last_err = Some(e);
                x.clone()
            }
        };
        let ny = match merge(&y, &x, &w.params) {
            Ok(s) => s,
            Err(e) => {
                last_err = Some(e);
                y.clone()
            }
        };
        x = nx;
        y = ny;
    }
    if ser(&x) == ser(&y) {
        Ending::Converged
    } else {
        match last_err {
            Some(e) => Ending::Rejecting(e),
            None => Ending::SilentlyDiverged,
        }
    }
}

fn seeds_from_env(default: u64) -> u64 {
    std::env::var("FORK_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn default_seeds(n: u64) -> Vec<(u64, usize)> {
    (0..n)
        .flat_map(|s| (0..SHAPES.len()).map(move |sh| (s, sh)))
        .collect()
}

/// Returns (permanent rejections, silent divergences), each as a readable line.
fn endings(mode: Mode, seeds: &[(u64, usize)]) -> (Vec<String>, Vec<String>) {
    let (mut rejecting, mut silent) = (Vec::new(), Vec::new());
    for &(seed, shape) in seeds {
        let (w, a, b) = fork_pair(seed, SHAPES[shape], mode);
        match ending(&w, &a, &b) {
            Ending::Converged => {}
            Ending::Rejecting(e) => rejecting.push(format!(
                "seed {seed} shape {shape}: {}",
                &e[..e.len().min(200)]
            )),
            Ending::SilentlyDiverged => silent.push(format!("seed {seed} shape {shape}")),
        }
    }
    (rejecting, silent)
}

/// Honest divergence: every fork pair must fully converge. `FORK_SEEDS=N` runs
/// more (0 of 3000 failed at N=1000).
#[test]
fn honest_forks_always_converge() {
    let seeds = default_seeds(seeds_from_env(3));
    let (rejecting, silent) = endings(Mode::Honest, &seeds);
    assert!(
        rejecting.is_empty() && silent.is_empty(),
        "honest fork pairs that never converge, of {}:\nrejecting:\n{}\nsilent:\n{}",
        seeds.len(),
        rejecting.join("\n"),
        silent.join("\n")
    );
}

/// Adversarial-but-validly-signed divergence must never end in a PERMANENT
/// REJECTION (#423). The pinned seeds are pairs that did exactly that before
/// the fix, each with "Banning member not found in member list", so this test
/// fails on that code even at the default seed count.
///
/// The pinned seeds are only meaningful while `random_op` draws from the RNG
/// in exactly the order it does today (`StdRng` under the committed
/// `Cargo.lock`). If you change the generator or bump `rand`, re-check that
/// they still fail on the pre-fix code, or pick new ones: `FORK_SEEDS=1000`
/// against `a3e63c8c`'s `ban.rs` lists them.
///
/// Silent divergence (every merge succeeds, the states differ) is reported
/// but not asserted. With the apply-time orphaned-ban drop (#702 review
/// rounds 1-3) it was 6 of 3006 pairs at `FORK_SEEDS=1000`: a member kept on
/// one peer only by the banner prune exemption, dropped as orphaned on the
/// other. Since bans are resolved from converged state
/// (`MembersV1::resolve_bans`) and nothing is dropped at apply time, the
/// same run finds 0. Tracked in freenet/river#703.
#[test]
fn adversarial_forks_never_permanently_reject_each_other() {
    let mut seeds = default_seeds(seeds_from_env(3));
    seeds.extend([(40, 1), (60, 2), (83, 2), (201, 2), (252, 2), (292, 2)]);
    let (rejecting, silent) = endings(Mode::Adversarial, &seeds);
    if !silent.is_empty() {
        eprintln!(
            "silent divergence (#703): {} of {} adversarial pairs:\n{}",
            silent.len(),
            seeds.len(),
            silent.join("\n")
        );
    }
    assert!(
        rejecting.is_empty(),
        "{} of {} adversarial fork pairs PERMANENTLY REJECT each other (#423):\n{}",
        rejecting.len(),
        seeds.len(),
        rejecting.join("\n")
    );
}
