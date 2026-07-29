//! SCRATCH probe for freenet/river#422 — measure the REAL invite-tree depth of
//! a live room, since `riverctl` exposes no `invited_by` through any command
//! and the whole O(members x depth) justification turns on that number.
//!
//!   cargo run -p riverctl --example invite_depth_probe -- <ROOM_OWNER_VK>

use anyhow::Result;
use ed25519_dalek::VerifyingKey;
use river_core::room_state::member::MemberId;
use riverctl::api::ApiClient;
use riverctl::config::Config;
use std::collections::HashMap;

#[tokio::main]
async fn main() -> Result<()> {
    let owner_b58 = std::env::args()
        .nth(1)
        .expect("usage: invite_depth_probe <ROOM_OWNER_VK_BASE58>");
    let node_url = std::env::var("RIVER_NODE_URL").unwrap_or_else(|_| {
        "ws://127.0.0.1:7509/v1/contract/command?encodingProtocol=native".into()
    });

    let decoded = bs58::decode(&owner_b58).into_vec()?;
    let bytes: [u8; 32] = decoded.as_slice().try_into()?;
    let owner_vk = VerifyingKey::from_bytes(&bytes)?;
    let owner_id = MemberId::from(&owner_vk);

    let api = ApiClient::new(&node_url, Config::default(), None).await?;
    let state = api.get_room(&owner_vk, false).await?;

    let members = &state.members.members;
    let by_id: HashMap<MemberId, &_> = state.members.members_by_member_id();

    println!("members (vec)          : {}", members.len());
    println!("members (by_id, deduped): {}", by_id.len());
    println!("bans                   : {}", state.bans.0.len());
    println!(
        "messages               : {}",
        state.recent_messages.messages.len()
    );
    println!(
        "member_info            : {}",
        state.member_info.member_info.len()
    );
    println!(
        "max_members            : {}",
        state.configuration.configuration.max_members
    );
    println!(
        "max_user_bans          : {}",
        state.configuration.configuration.max_user_bans
    );
    let mut ser = vec![];
    ciborium::ser::into_writer(&state, &mut ser)?;
    println!("serialized bytes       : {}", ser.len());

    // Depth of each member's chain to the owner, and the total number of links
    // — which IS the Ed25519 verification count MembersV1::verify performed
    // before this PR (and the count it performs after is simply members.len()).
    let mut depths: Vec<usize> = Vec::with_capacity(members.len());
    let mut total_links = 0usize;
    for m in members {
        let mut cur = m;
        let mut seen = std::collections::HashSet::new();
        let mut d = 0usize;
        loop {
            d += 1;
            if !seen.insert(cur.member.id()) {
                break; // cycle
            }
            if cur.member.invited_by == owner_id {
                break;
            }
            match by_id.get(&cur.member.invited_by) {
                Some(next) => cur = next,
                None => break,
            }
        }
        depths.push(d);
        total_links += d;
    }
    depths.sort_unstable();

    let n = depths.len().max(1);
    println!();
    println!(
        "invite depth  max      : {}",
        depths.last().copied().unwrap_or(0)
    );
    println!(
        "invite depth  mean     : {:.2}",
        total_links as f64 / n as f64
    );
    println!("invite depth  median   : {}", depths[depths.len() / 2]);
    println!(
        "invite depth  p90      : {}",
        depths[(depths.len() * 9) / 10]
    );

    let mut hist: HashMap<usize, usize> = HashMap::new();
    for d in &depths {
        *hist.entry(*d).or_default() += 1;
    }
    let mut ks: Vec<_> = hist.keys().copied().collect();
    ks.sort_unstable();
    println!(
        "depth histogram        : {:?}",
        ks.iter().map(|k| (*k, hist[k])).collect::<Vec<_>>()
    );

    println!();
    println!("Ed25519 verifications in MembersV1::verify:");
    println!("  before this PR (sum of chain lengths): {}", total_links);
    println!("  after  this PR (one per member)      : {}", members.len());
    println!(
        "  saving                               : {:.2}x",
        total_links as f64 / n as f64
    );
    Ok(())
}
