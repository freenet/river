//! Measures what ONE `ChatRoomStateV1` clone costs in resident memory.
//!
//! Motivation: the conversation view clones the whole room state per rendered
//! message row (two event-handler closures, each capturing `RoomData` by
//! value). That makes the UI's memory O(messages x state_size). This binary
//! prices the multiplicand so the blow-up can be predicted from a room's shape.
//!
//! Run: `cargo run -p river-core --release --example room_state_clone_cost`

use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use rand::rngs::OsRng;
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersV1};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo, MemberInfoV1};
use river_core::room_state::message::{
    AuthorizedMessageV1, MessageV1, MessagesV1, RoomMessageBody,
};
use river_core::room_state::privacy::SealedBytes;
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1};
use std::time::{Duration, SystemTime};

const BASE_SECS: u64 = 1_700_000_000;

/// Resident set size in bytes, straight from the kernel. `statm` field 2 is
/// resident pages.
fn rss_bytes() -> usize {
    let statm = std::fs::read_to_string("/proc/self/statm").expect("read /proc/self/statm");
    let pages: usize = statm
        .split_whitespace()
        .nth(1)
        .expect("resident field")
        .parse()
        .expect("resident pages parse");
    pages * 4096
}

/// A room shaped like River's "Off Topic": `members` members, each with a
/// signed nickname record, and `messages` retained public messages.
fn build_room(members: usize, messages: usize, msg_len: usize) -> ChatRoomStateV1 {
    let owner_sk = SigningKey::generate(&mut OsRng);
    let owner_vk = owner_sk.verifying_key();
    let owner_id = MemberId::from(&owner_vk);
    let params = ChatRoomParametersV1 { owner: owner_vk };

    let member_sks: Vec<SigningKey> = (0..members)
        .map(|_| SigningKey::generate(&mut OsRng))
        .collect();

    let authorized_members: Vec<AuthorizedMember> = member_sks
        .iter()
        .map(|sk| {
            AuthorizedMember::new(
                Member {
                    owner_member_id: owner_id,
                    invited_by: owner_id,
                    member_vk: sk.verifying_key(),
                },
                &owner_sk,
            )
        })
        .collect();

    let member_info: Vec<AuthorizedMemberInfo> = member_sks
        .iter()
        .enumerate()
        .map(|(i, sk)| {
            AuthorizedMemberInfo::new_with_member_key(
                MemberInfo {
                    member_id: MemberId::from(&sk.verifying_key()),
                    version: 0,
                    preferred_nickname: SealedBytes::public(
                        format!("Member Nickname {i}").into_bytes(),
                    ),
                    deputies: Vec::new(),
                },
                sk,
            )
        })
        .collect();

    let config = Configuration {
        max_recent_messages: messages,
        max_message_size: 10_000,
        max_members: members + 1,
        ..Default::default()
    };

    let mut state = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(config, &owner_sk),
        members: MembersV1 {
            members: authorized_members,
        },
        member_info: MemberInfoV1 { member_info },
        ..Default::default()
    };

    // Realistic body: a sentence's worth of text, authored round-robin so the
    // grouping in the UI matches a busy room rather than one long monologue.
    let body = "x".repeat(msg_len);
    let delta: Vec<AuthorizedMessageV1> = (0..messages)
        .map(|i| {
            let sk = &member_sks[i % member_sks.len()];
            AuthorizedMessageV1::new(
                MessageV1 {
                    room_owner: owner_id,
                    author: MemberId::from(&sk.verifying_key()),
                    time: SystemTime::UNIX_EPOCH + Duration::from_secs(BASE_SECS + i as u64),
                    content: RoomMessageBody::public(format!("{body} {i}")),
                },
                sk,
            )
        })
        .collect();

    let mut msgs = MessagesV1::default();
    msgs.apply_delta(&state, &params, &Some(delta))
        .expect("fixture messages must apply");
    state.recent_messages = msgs;
    state
}

fn main() {
    // Shape taken from the live "Off Topic" room profiled on 2026-07-26:
    // ~1133 rendered messages, ~136 members.
    let members = 136;
    let messages = 1133;
    let msg_len = 80;

    let state = build_room(members, messages, msg_len);
    println!(
        "room: {} members, {} messages, ~{}B bodies",
        members,
        state.recent_messages.messages.len(),
        msg_len
    );

    // Warm the allocator so the measured delta is clone cost, not arena growth.
    let warmup: Vec<ChatRoomStateV1> = (0..8).map(|_| state.clone()).collect();
    std::hint::black_box(&warmup);
    drop(warmup);

    const N: usize = 200;
    let before = rss_bytes();
    let clones: Vec<ChatRoomStateV1> = (0..N).map(|_| state.clone()).collect();
    let after = rss_bytes();
    std::hint::black_box(&clones);

    let per_clone = (after - before) as f64 / N as f64;
    println!(
        "RSS before {:.1} MB, after {:.1} MB, {N} clones held",
        before as f64 / 1e6,
        after as f64 / 1e6
    );
    println!("=> {:.0} KB per room-state clone", per_clone / 1024.0);
    println!(
        "=> a conversation holding 2 clones per rendered row costs {:.2} GB at {} rows",
        per_clone * 2.0 * messages as f64 / 1e9,
        messages
    );
    drop(clones);
}
