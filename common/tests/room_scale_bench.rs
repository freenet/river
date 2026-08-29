//! Cost of the room contract's two heavy entry points, as a function of room
//! shape. This is the harness that produced the numbers in freenet/river#548;
//! it is committed so they can be re-run and disputed.
//!
//! Native numbers are NOT representative — `opt-level = 'z'` penalises
//! curve25519-dalek far more severely on a native build than in WASM, where
//! Cranelift re-optimises the module. Run it the way production runs:
//!
//! ```text
//! rustup target add wasm32-wasip1
//! CARGO_TARGET_WASM32_WASIP1_RUNNER='wasmtime -O opt-level=0' \
//!   cargo test --release --target wasm32-wasip1 -p river-core \
//!   --test room_scale_bench -- --nocapture --ignored
//! ```
//!
//! `-O opt-level=0` matches freenet-core, which sets Cranelift to
//! `OptLevel::None` for untrusted code
//! (`crates/core/src/wasm_runtime/engine/wasmtime_engine.rs`).
//!
//! `#[ignore]`d: it is a measurement, not an assertion, and it is slow. The
//! assertions that actually guard this behaviour live in
//! `signature_verification_cost_test.rs` and count verifications rather than
//! wall-clock time.

use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use river_core::room_state::ban::{AuthorizedUserBan, BansV1, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersV1};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo, MemberInfoV1};
use river_core::room_state::message::{
    AuthorizedMessageV1, MessageV1, MessagesV1, RoomMessageBody,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1};
use std::time::{Duration, Instant, SystemTime};

fn sk(i: u64) -> SigningKey {
    let mut b = [0u8; 32];
    b[0..8].copy_from_slice(&i.to_le_bytes());
    b[8] = 0x77;
    SigningKey::from_bytes(&b)
}

/// Builds a room with `n_members` laid out over an invite tree `depth`
/// generations deep (generation 0 is invited by the owner), plus `n_messages`
/// messages and `n_bans` bans issued by member 0 against non-members.
fn build(
    n_members: usize,
    depth: usize,
    n_messages: usize,
    n_bans: usize,
    bans_target_members: bool,
) -> (ChatRoomStateV1, ChatRoomParametersV1) {
    let owner_sk = sk(0);
    let owner_id = MemberId::from(&owner_sk.verifying_key());
    let params = ChatRoomParametersV1 {
        owner: owner_sk.verifying_key(),
    };
    let keys: Vec<SigningKey> = (1..=(n_members + n_bans) as u64).map(sk).collect();
    let sks = &keys[..n_members];
    let ids: Vec<MemberId> = sks
        .iter()
        .map(|k| MemberId::from(&k.verifying_key()))
        .collect();

    let per_gen = n_members.div_ceil(depth.max(1));
    let members: Vec<AuthorizedMember> = (0..n_members)
        .map(|i| {
            let gen = i / per_gen;
            let (inviter_id, inviter_sk) = if gen == 0 {
                (owner_id, &owner_sk)
            } else {
                let p = ((gen - 1) * per_gen + (i % per_gen)).min(gen * per_gen - 1);
                (ids[p], &sks[p])
            };
            AuthorizedMember::new(
                Member {
                    owner_member_id: owner_id,
                    invited_by: inviter_id,
                    member_vk: sks[i].verifying_key(),
                },
                inviter_sk,
            )
        })
        .collect();

    let member_info: Vec<AuthorizedMemberInfo> = (0..n_members)
        .map(|i| {
            AuthorizedMemberInfo::new_with_member_key(
                MemberInfo::new_public(ids[i], 1, format!("m{i}")),
                &sks[i],
            )
        })
        .collect();

    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let text = "x".repeat(120);
    let messages: Vec<AuthorizedMessageV1> = (0..n_messages)
        .map(|i| {
            let a = i % n_members.max(1);
            AuthorizedMessageV1::new(
                MessageV1 {
                    room_owner: owner_id,
                    author: ids[a],
                    time: base + Duration::from_secs(i as u64),
                    content: RoomMessageBody::public(text.clone()),
                },
                &sks[a],
            )
        })
        .collect();

    let bans: Vec<AuthorizedUserBan> = (0..n_bans)
        .map(|i| {
            // Targeting a real member exercises `get_downstream_members`,
            // which linear-scans the whole member vec per node of the target's
            // subtree. Targeting a non-member (the default, and what a
            // moderated room accumulates) returns immediately.
            let victim = if bans_target_members {
                ids[(i + 1) % n_members.max(1)]
            } else {
                MemberId::from(&keys[n_members + i].verifying_key())
            };
            AuthorizedUserBan::new(
                UserBan {
                    owner_member_id: owner_id,
                    banned_at: base + Duration::from_secs(i as u64),
                    banned_user: victim,
                },
                ids[0],
                &sks[0],
            )
        })
        .collect();

    let mut state = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: owner_id,
                max_members: n_members.max(1),
                max_recent_messages: n_messages.max(1),
                max_user_bans: n_bans.max(1),
                ..Default::default()
            },
            &owner_sk,
        ),
        bans: BansV1(bans),
        members: MembersV1 { members },
        member_info: MemberInfoV1 { member_info },
        recent_messages: MessagesV1 {
            messages,
            ..Default::default()
        },
        ..Default::default()
    };
    state.recent_messages.rebuild_actions_state();
    state.members.members.sort_by_key(|m| m.member.id());
    state
        .member_info
        .member_info
        .sort_by_key(|i| i.member_info.member_id);
    state.bans.0.sort_by(|a, b| {
        a.ban
            .banned_at
            .cmp(&b.ban.banned_at)
            .then_with(|| a.id().cmp(&b.id()))
    });
    (state, params)
}

fn best<T>(mut f: impl FnMut() -> T) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        let out = f();
        std::hint::black_box(out);
        best = best.min(t.elapsed().as_secs_f64() * 1e3);
    }
    best
}

#[test]
#[ignore = "measurement, not an assertion; run explicitly under wasmtime"]
fn room_scale() {
    // Warm the JIT.
    {
        let (s, p) = build(20, 2, 10, 2, false);
        for _ in 0..3 {
            s.verify(&s, &p).unwrap();
        }
    }

    println!(
        "\n{:>26} {:>8} {:>6} {:>6} {:>5} {:>9} | {:>13} {:>13}",
        "shape", "members", "depth", "msgs", "bans", "bytes", "validate(ms)", "1msg upd(ms)"
    );

    // The first row is the LIVE Freenet Official room, measured 2026-07-29 with
    // `cli/examples/invite_depth_probe.rs`: 496 members, invite depth max 4 /
    // mean 2.02, 2000 messages, 200 bans, 497 member_info, 1.44 MB. The rest
    // sweep depth to show the shape of the curve this PR flattens; they are
    // SYNTHETIC and no measured room looks like the deep ones.
    for &(label, m, d, msg, b, tgt) in &[
        (
            "official room (measured)",
            496usize,
            2usize,
            2000usize,
            200usize,
            false,
        ),
        ("official, no bans", 496, 2, 2000, 0, false),
        ("official, no messages", 496, 2, 0, 200, false),
        ("official, bans hit members", 496, 2, 2000, 200, true),
        ("synthetic depth 10", 400, 10, 100, 0, false),
        ("synthetic depth 50", 400, 50, 100, 0, false),
        ("synthetic depth 200", 400, 200, 100, 0, false),
    ] {
        let (state, params) = build(m, d, msg, b, tgt);
        let mut bytes = vec![];
        ciborium::ser::into_writer(&state, &mut bytes).unwrap();

        let t_validate = best(|| state.verify(&state, &params).unwrap());

        let summary = state.summarize(&state, &params);
        let mut modified = state.clone();
        modified
            .recent_messages
            .messages
            .push(AuthorizedMessageV1::new(
                MessageV1 {
                    room_owner: params.owner_id(),
                    author: state.members.members[0].member.id(),
                    time: SystemTime::UNIX_EPOCH + Duration::from_secs(1_900_000_000),
                    content: RoomMessageBody::public("hi".into()),
                },
                &sk(1),
            ));
        let delta = modified.delta(&state, &params, &summary);
        let t_update = best(|| {
            let mut target = state.clone();
            let parent = target.clone();
            let _ = target.apply_delta(&parent, &params, &delta);
        });

        println!(
            "{label:>26} {m:>8} {d:>6} {msg:>6} {b:>5} {:>9} | {t_validate:>13.1} {t_update:>13.1}",
            bytes.len()
        );
    }
}
