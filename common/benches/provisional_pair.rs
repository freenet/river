//! Cost of the pair the host pays after every chat message on a room at the
//! provisional ceilings (freenet/river#974, design section 5): `update_state`
//! with a one-message delta, then `validate_state` of its output. The room
//! holds the ceilings of provisional entries and bans (16 each) and a full message
//! window; the baseline is the same room (same members, same messages) with no
//! provisional data.
//!
//! Plain `std::time` timing, not criterion: the committed `Cargo.lock` pins the
//! byte-reproducible contract WASM builds, so no dependency is added for a
//! measurement. Run:
//!
//! ```text
//! CARGO_PROFILE_BENCH_OPT_LEVEL=3 cargo bench -p river-core --bench provisional_pair
//! ```
//!
//! A second case times the adversarial worst case one delta may carry: 16 NEW
//! provisional entries, 16 NEW provisional bans, a `member_info` record and a
//! message for each new author, applied to the room at the ceilings (per-delta
//! work bounds are `max(cap, 16)`). Set `BENCH_DUMP_DIR=<dir>` to also write
//! `params.cbor`, `state.cbor`, `delta.cbor` and `adversarial_delta.cbor` for
//! `tools/wasm-cost-bench`, which times the same calls under wasmtime.
//!
//! The contract calls are mirrored exactly: decode the state, decode the delta,
//! `apply_delta`, encode (update); decode the result, `validate_whole_state`
//! (validate). Native numbers; the WASM figure is measured under wasmtime by
//! the same fixture (see the PR).

use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersDelta};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
use river_core::room_state::provisional::{
    AdmissionEvidence, AuthorizedProvisionalBan, AuthorizedProvisionalMember, PowPolicy,
    ProvisionalBan, ProvisionalDelta, ProvisionalMember, PublicRoomPolicy,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use std::time::{Duration, Instant, SystemTime};

const WINDOW: usize = 100;
const MEMBERS: u8 = 26;
/// The ceilings: `MAX_PROVISIONAL_CEILING` entries and as many bans.
/// Overridable with `BENCH_PROVISIONAL=<n>` to measure a candidate cap.
fn provisional() -> usize {
    std::env::var("BENCH_PROVISIONAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(river_core::room_state::provisional::MAX_PROVISIONAL_CEILING as usize)
}
/// What fills the window beside the members' three messages each and the
/// provisional authors' one each.
fn owner_fill() -> usize {
    WINDOW.saturating_sub(MEMBERS as usize * 3 + provisional())
}

fn sk(n: u32) -> SigningKey {
    let mut b = [0u8; 32];
    b[..4].copy_from_slice(&n.to_le_bytes());
    b[31] = 9;
    SigningKey::from_bytes(&b)
}

fn msg(owner_id: MemberId, k: &SigningKey, t: u64) -> AuthorizedMessageV1 {
    AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: owner_id,
            author: k.verifying_key().into(),
            time: (SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000 + t)).into(),
            content: RoomMessageBody::public(format!("benchmark message number {t}")),
        },
        k,
    )
}

fn build(
    with_provisional: bool,
) -> (
    ChatRoomStateV1,
    ChatRoomParametersV1,
    SigningKey,
    SigningKey,
) {
    let owner = sk(1_000_000);
    let params = ChatRoomParametersV1 {
        owner: owner.verifying_key(),
    };
    let owner_id = params.owner_id();
    let policy = with_provisional.then_some(PublicRoomPolicy {
        listed: false,
        max_provisional: provisional() as u32,
        max_provisional_messages: 500,
        max_provisional_bans: provisional() as u32,
        pow: Some(PowPolicy {
            difficulty_bits: 0,
            epoch: 1,
        }),
    });
    let mut state = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: owner_id,
                max_recent_messages: WINDOW,
                max_members: 100,
                public_admission: policy,
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
    // Members with three messages each, and the owner with twenty.
    let members: Vec<SigningKey> = (0..MEMBERS as u32).map(sk).collect();
    let mut t = 0u64;
    let mut msgs = Vec::new();
    for k in &members {
        for _ in 0..3 {
            t += 1;
            msgs.push(msg(owner_id, k, t));
        }
    }
    for _ in 0..owner_fill() {
        t += 1;
        msgs.push(msg(owner_id, &owner, t));
    }
    apply(
        &mut state,
        ChatRoomStateV1Delta {
            members: Some(MembersDelta::new(
                members
                    .iter()
                    .map(|k| {
                        AuthorizedMember::new(
                            Member {
                                owner_member_id: owner_id,
                                invited_by: owner_id,
                                member_vk: k.verifying_key(),
                            },
                            &owner,
                        )
                    })
                    .collect(),
            )),
            recent_messages: Some(msgs),
            ..Default::default()
        },
    );
    if with_provisional {
        // 16 (the ceiling) provisional authors, each with an entry and one message.
        let authors: Vec<SigningKey> = (0..provisional() as u32).map(|i| sk(500_000 + i)).collect();
        let entries: Vec<AuthorizedProvisionalMember> = authors
            .iter()
            .map(|k| {
                // Difficulty 0: any nonce admits.
                let nonce = 0u64;
                AuthorizedProvisionalMember::new(
                    &params.owner,
                    ProvisionalMember {
                        member_vk: k.verifying_key(),
                        evidence: AdmissionEvidence::Pow { nonce },
                    },
                    k,
                )
            })
            .collect();
        let bans: Vec<AuthorizedProvisionalBan> = (0..provisional() as u32)
            .map(|i| {
                AuthorizedProvisionalBan::new(
                    &params.owner,
                    ProvisionalBan {
                        banned_by: owner_id,
                        banned_at: (SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000))
                            .into(),
                        target: sk(900_000 + i).verifying_key().into(),
                    },
                    &owner,
                )
            })
            .collect();
        t += 1;
        let msgs: Vec<AuthorizedMessageV1> = authors
            .iter()
            .enumerate()
            .map(|(i, k)| msg(owner_id, k, t + i as u64))
            .collect();
        apply(
            &mut state,
            ChatRoomStateV1Delta {
                provisional: Some(ProvisionalDelta { entries, bans }),
                recent_messages: Some(msgs),
                ..Default::default()
            },
        );
        assert_eq!(
            state.provisional.entries.len(),
            provisional(),
            "fixture holds the ceiling of entries"
        );
        assert_eq!(
            state.provisional.bans.len(),
            provisional(),
            "fixture holds the ceiling of bans"
        );
    }
    assert_eq!(
        state.recent_messages.messages.len(),
        if with_provisional {
            WINDOW
        } else {
            MEMBERS as usize * 3 + owner_fill()
        }
    );
    (state, params, members[0].clone(), owner)
}

fn cbor<T: serde::Serialize>(v: &T) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(v, &mut out).unwrap();
    out
}

/// One `update_state` then `validate_state`, as the contract does them.
fn pair(
    state_bytes: &[u8],
    delta_bytes: &[u8],
    params: &ChatRoomParametersV1,
) -> (Duration, Duration, usize) {
    let t0 = Instant::now();
    let mut state: ChatRoomStateV1 = ciborium::de::from_reader(state_bytes).unwrap();
    let delta: ChatRoomStateV1Delta = ciborium::de::from_reader(delta_bytes).unwrap();
    state
        .apply_delta(&state.clone(), params, &Some(delta))
        .unwrap();
    let out = cbor(&state);
    let update = t0.elapsed();
    let t1 = Instant::now();
    let decoded: ChatRoomStateV1 = ciborium::de::from_reader(out.as_slice()).unwrap();
    decoded.validate_whole_state(params, &out).unwrap();
    (update, t1.elapsed(), out.len())
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn main() {
    let iterations: usize = std::env::var("BENCH_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);
    println!(
        "room at the ceilings: {} entries, {} provisional bans, window {WINDOW}",
        provisional(),
        provisional()
    );
    let mut medians = Vec::new();
    for with in [false, true] {
        let (state, params, member, _owner) = build(with);
        let owner_id = params.owner_id();
        let delta = ChatRoomStateV1Delta {
            recent_messages: Some(vec![msg(owner_id, &member, 5_000_000)]),
            ..Default::default()
        };
        let (state_bytes, delta_bytes) = (cbor(&state), cbor(&delta));
        let summary_bytes = cbor(&state.summarize(&state, &params)).len();
        pair(&state_bytes, &delta_bytes, &params); // warm up
        let (mut u, mut v) = (Vec::new(), Vec::new());
        let mut out_len = 0;
        for _ in 0..iterations {
            let (a, b, l) = pair(&state_bytes, &delta_bytes, &params);
            u.push(a);
            v.push(b);
            out_len = l;
        }
        let (mu, mv) = (median(u), median(v));
        println!(
            "{:<22} state {:>7} B  summary {:>6} B  update_state {:>8.2?}  validate_state {:>8.2?}  pair {:>8.2?}",
            if with { "with provisional data" } else { "baseline (none)" },
            out_len,
            summary_bytes,
            mu,
            mv,
            mu + mv
        );
        medians.push(mu + mv);
    }
    println!(
        "added by provisional data: {:.2?} per message",
        medians[1].saturating_sub(medians[0])
    );

    // Adversarial worst case: one delta with 16 new entries, 16 new bans, and a
    // member_info record plus a message for each new author.
    {
        let (state, params, member, owner) = build(true);
        let owner_id = params.owner_id();
        let authors: Vec<SigningKey> = (0..provisional() as u32).map(|i| sk(600_000 + i)).collect();
        let entries = authors
            .iter()
            .map(|k| {
                AuthorizedProvisionalMember::new(
                    &params.owner,
                    ProvisionalMember {
                        member_vk: k.verifying_key(),
                        evidence: AdmissionEvidence::Pow { nonce: 0 },
                    },
                    k,
                )
            })
            .collect();
        let bans = (0..provisional() as u32)
            .map(|i| {
                AuthorizedProvisionalBan::new(
                    &params.owner,
                    ProvisionalBan {
                        banned_by: owner_id,
                        banned_at: (SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000))
                            .into(),
                        target: sk(950_000 + i).verifying_key().into(),
                    },
                    &owner,
                )
            })
            .collect();
        let infos = authors
            .iter()
            .map(|k| {
                AuthorizedMemberInfo::new_with_member_key(
                    MemberInfo::new_public(
                        k.verifying_key().into(),
                        1,
                        "provisional author".to_string(),
                    ),
                    k,
                )
            })
            .collect();
        let msgs = authors
            .iter()
            .enumerate()
            .map(|(i, k)| msg(owner_id, k, 6_000_000 + i as u64))
            .collect();
        let delta = ChatRoomStateV1Delta {
            provisional: Some(ProvisionalDelta { entries, bans }),
            member_info: Some(infos),
            recent_messages: Some(msgs),
            ..Default::default()
        };
        let (state_bytes, delta_bytes) = (cbor(&state), cbor(&delta));
        pair(&state_bytes, &delta_bytes, &params);
        let (mut u, mut v) = (Vec::new(), Vec::new());
        let mut out_len = 0;
        for _ in 0..iterations {
            let (a, b, l) = pair(&state_bytes, &delta_bytes, &params);
            u.push(a);
            v.push(b);
            out_len = l;
        }
        let (mu, mv) = (median(u), median(v));
        println!(
            "{:<22} state {:>7} B  delta {:>6} B  update_state {:>8.2?}  validate_state {:>8.2?}  pair {:>8.2?}",
            "adversarial delta",
            out_len,
            delta_bytes.len(),
            mu,
            mv,
            mu + mv
        );
        if let Ok(dir) = std::env::var("BENCH_DUMP_DIR") {
            let one = ChatRoomStateV1Delta {
                recent_messages: Some(vec![msg(owner_id, &member, 5_000_000)]),
                ..Default::default()
            };
            let dir = std::path::Path::new(&dir);
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("params.cbor"), cbor(&params)).unwrap();
            std::fs::write(dir.join("state.cbor"), &state_bytes).unwrap();
            std::fs::write(dir.join("delta.cbor"), cbor(&one)).unwrap();
            std::fs::write(dir.join("adversarial_delta.cbor"), &delta_bytes).unwrap();
        }
    }

    // Where `validate_state` spends it, per field, on the room with provisional
    // data (each field's `verify`, then the cleanup the whole-state check runs).
    let (state, params, _member, _owner) = build(true);
    let time = |name: &str, f: &dyn Fn()| {
        f();
        let t = Instant::now();
        for _ in 0..iterations {
            f();
        }
        println!("  {name:<24} {:>9.2?}", t.elapsed() / iterations as u32);
    };
    println!("validate_state, by field:");
    time("provisional.verify", &|| {
        state.provisional.verify(&state, &params).unwrap()
    });
    time("recent_messages.verify", &|| {
        state.recent_messages.verify(&state, &params).unwrap()
    });
    time("member_info.verify", &|| {
        state.member_info.verify(&state, &params).unwrap()
    });
    time("members.verify", &|| {
        state.members.verify(&state, &params).unwrap()
    });
    time("cleanup (normalized)", &|| {
        state.normalized(&params).unwrap();
    });
    time("decode state", &|| {
        let _: ChatRoomStateV1 = ciborium::de::from_reader(cbor(&state).as_slice()).unwrap();
    });
}
