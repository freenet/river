//! Minimise one commutativity counterexample: report exactly which members /
//! member_info / DM entries differ between merge(A,B) and merge(B,A), and the
//! retention reason for each differing member.
//!
//! Usage: cargo run --release --example merge_law_minimise -- <dir> <a> <b>

use ciborium::{de::from_reader, ser::into_writer};
use freenet_scaffold::ComposableState;
use river_core::room_state::member::MemberId;
use river_core::room_state::ChatRoomParametersV1;
use river_core::ChatRoomStateV1;
use std::collections::HashSet;
use std::fs;

fn load<T: serde::de::DeserializeOwned>(p: &str) -> T {
    let b = fs::read(p).unwrap_or_else(|e| panic!("read {p}: {e}"));
    from_reader::<T, &[u8]>(&b[..]).unwrap_or_else(|e| panic!("decode {p}: {e}"))
}

fn ser(s: &ChatRoomStateV1) -> Vec<u8> {
    let mut v = vec![];
    into_writer(s, &mut v).unwrap();
    v
}

fn merge(a: &ChatRoomStateV1, b: &ChatRoomStateV1, p: &ChatRoomParametersV1) -> ChatRoomStateV1 {
    let mut s = a.clone();
    let parent = s.clone();
    s.merge(&parent, p, b).expect("merge failed");
    s
}

fn ids(s: &ChatRoomStateV1) -> HashSet<MemberId> {
    s.members.members.iter().map(|m| m.member.id()).collect()
}

fn describe(tag: &str, s: &ChatRoomStateV1, who: &HashSet<MemberId>) {
    let authors: HashSet<MemberId> = s
        .recent_messages
        .messages
        .iter()
        .map(|m| m.message.author)
        .collect();
    let dm_parts = s.direct_messages.active_participants();
    for id in who {
        let present = s.members.members.iter().any(|m| m.member.id() == *id);
        let inviter = s
            .members
            .members
            .iter()
            .find(|m| m.member.id() == *id)
            .map(|m| m.member.invited_by);
        println!(
            "  [{tag}] {id:?} present={present} has_msg={} dm_participant={} inviter={:?}",
            authors.contains(id),
            dm_parts.contains(id),
            inviter
        );
    }
}

fn main() {
    let mut a = std::env::args().skip(1);
    let dir = a.next().expect("dir");
    let fa = a.next().expect("state A");
    let fb = a.next().expect("state B");
    let params: ChatRoomParametersV1 = load(&format!("{dir}/params.bin"));
    let sa: ChatRoomStateV1 = load(&format!("{dir}/{fa}"));
    let sb: ChatRoomStateV1 = load(&format!("{dir}/{fb}"));

    println!(
        "A={fa} members={} msgs={} dms={}\nB={fb} members={} msgs={} dms={}",
        sa.members.members.len(),
        sa.recent_messages.messages.len(),
        sa.direct_messages.messages.len(),
        sb.members.members.len(),
        sb.recent_messages.messages.len(),
        sb.direct_messages.messages.len()
    );

    let ab = merge(&sa, &sb, &params);
    let ba = merge(&sb, &sa, &params);
    println!(
        "\nmerge(A,B): members={} msgs={} dms={} bytes={}",
        ab.members.members.len(),
        ab.recent_messages.messages.len(),
        ab.direct_messages.messages.len(),
        ser(&ab).len()
    );
    println!(
        "merge(B,A): members={} msgs={} dms={} bytes={}",
        ba.members.members.len(),
        ba.recent_messages.messages.len(),
        ba.direct_messages.messages.len(),
        ser(&ba).len()
    );
    println!("equal = {}", ser(&ab) == ser(&ba));

    let (ia, ib) = (ids(&ab), ids(&ba));
    let only_ab: HashSet<_> = ia.difference(&ib).cloned().collect();
    let only_ba: HashSet<_> = ib.difference(&ia).cloned().collect();
    println!("\nmembers only in merge(A,B): {}", only_ab.len());
    describe("AB", &ab, &only_ab);
    println!("members only in merge(B,A): {}", only_ba.len());
    describe("BA", &ba, &only_ba);

    // Where does each differing member come from?
    let in_a = ids(&sa);
    let in_b = ids(&sb);
    for id in only_ab.iter().chain(only_ba.iter()) {
        println!(
            "  origin {id:?}: in A={} in B={}",
            in_a.contains(id),
            in_b.contains(id)
        );
    }

    // Is post_apply_cleanup itself non-idempotent on these results?
    let mut ab2 = ab.clone();
    ab2.post_apply_cleanup(&params).unwrap();
    let mut ba2 = ba.clone();
    ba2.post_apply_cleanup(&params).unwrap();
    println!(
        "\ncleanup idempotent on merge(A,B)? {}   on merge(B,A)? {}",
        ser(&ab2) == ser(&ab),
        ser(&ba2) == ser(&ba)
    );
    println!(
        "after one extra cleanup each, equal = {}",
        ser(&ab2) == ser(&ba2)
    );

    // How many cleanup passes until a fixpoint, and do the two orders meet there?
    let fix = |mut s: ChatRoomStateV1| -> (ChatRoomStateV1, usize) {
        for i in 1..20 {
            let before = ser(&s);
            s.post_apply_cleanup(&params).unwrap();
            if ser(&s) == before {
                return (s, i);
            }
        }
        (s, 99)
    };
    let (abf, na) = fix(ab.clone());
    let (baf, nb) = fix(ba.clone());
    println!(
        "\ncleanup fixpoint: merge(A,B) after {na} passes members={} dms={}, merge(B,A) after {nb} passes members={} dms={}",
        abf.members.members.len(), abf.direct_messages.messages.len(),
        baf.members.members.len(), baf.direct_messages.messages.len()
    );
    println!("fixpoints equal = {}", ser(&abf) == ser(&baf));

    // Does a second round of anti-entropy reconcile them (eventual consistency)?
    let r1 = merge(&ab, &ba, &params);
    let r2 = merge(&ba, &ab, &params);
    println!(
        "\nround 2: merge(AB,BA) members={} dms={} bytes={}",
        r1.members.members.len(),
        r1.direct_messages.messages.len(),
        ser(&r1).len()
    );
    println!(
        "round 2: merge(BA,AB) members={} dms={} bytes={}",
        r2.members.members.len(),
        r2.direct_messages.messages.len(),
        ser(&r2).len()
    );
    println!("round-2 equal = {}", ser(&r1) == ser(&r2));

    // DM symmetric difference between the two orders.
    let dm_key = |m: &river_core::room_state::direct_messages::AuthorizedDirectMessage| {
        format!(
            "{:?}->{:?}@{:?}",
            m.message.sender, m.message.recipient, m.message.timestamp
        )
    };
    let dab: HashSet<String> = ab.direct_messages.messages.iter().map(dm_key).collect();
    let dba: HashSet<String> = ba.direct_messages.messages.iter().map(dm_key).collect();
    println!("\nDMs only in merge(A,B): {}", dab.difference(&dba).count());
    for k in dab.difference(&dba).take(8) {
        println!("   {k}");
    }
    println!("DMs only in merge(B,A): {}", dba.difference(&dab).count());
    for k in dba.difference(&dab).take(8) {
        println!("   {k}");
    }

    // MECHANISM: on the non-idempotent side, what changes between cleanup pass
    // 1 and pass 2? Report bans / members / dms after each pass.
    println!("\n--- cleanup passes on merge(B,A) ---");
    let mut s = ba.clone();
    let target: Vec<MemberId> = only_ba.iter().cloned().collect();
    let report = |tag: &str, s: &ChatRoomStateV1| {
        let authors: HashSet<MemberId> = s
            .recent_messages
            .messages
            .iter()
            .map(|m| m.message.author)
            .collect();
        let dmp = s.direct_messages.active_participants();
        let cur = s.secrets.current_version;
        let secr: HashSet<MemberId> = s
            .secrets
            .encrypted_secrets
            .iter()
            .filter(|x| x.secret.secret_version == cur)
            .map(|x| x.secret.member_id)
            .collect();
        let banners: HashSet<MemberId> = s.bans.0.iter().map(|b| b.banned_by).collect();
        println!(
            "  {tag}: members={} bans={} msgs={} dms={} secrets@cur={} distinct_authors={}",
            s.members.members.len(),
            s.bans.0.len(),
            s.recent_messages.messages.len(),
            s.direct_messages.messages.len(),
            secr.len(),
            authors.len()
        );
        for id in &target {
            let present = s.members.members.iter().any(|m| m.member.id() == *id);
            // is this member an ancestor of any current member?
            let is_inviter = s.members.members.iter().any(|m| m.member.invited_by == *id);
            println!(
                "    {id:?} present={present} author={} dm={} secret={} banner={} is_inviter_of_someone={}",
                authors.contains(id), dmp.contains(id), secr.contains(id),
                banners.contains(id), is_inviter
            );
        }
    };
    report("pass0", &s);
    for i in 1..4 {
        s.post_apply_cleanup(&params).unwrap();
        report(&format!("pass{i}"), &s);
    }

    // HYPOTHESIS: the leftover member is an invite-chain ancestor of a DM
    // participant whose DMs step 6 swept in the same pass. Step 1 reads
    // `dm_participants` BEFORE step 6 removes those DMs, so the ancestor is
    // exempted on the strength of a DM that does not survive the pass.
    println!("\n--- ancestry check ---");
    let by_id = ab.members.members_by_member_id();
    let chain = |mut id: MemberId| -> Vec<MemberId> {
        let mut out = vec![id];
        for _ in 0..64 {
            match by_id.get(&id) {
                Some(m) => {
                    id = m.member.invited_by;
                    out.push(id);
                }
                None => break,
            }
        }
        out
    };
    let swept_participants: HashSet<MemberId> = ab
        .direct_messages
        .messages
        .iter()
        .filter(|m| !dba.contains(&dm_key(m)))
        .flat_map(|m| [m.message.sender, m.message.recipient])
        .collect();
    println!(
        "participants of the DMs swept in merge(B,A): {:?}",
        swept_participants
    );
    for pid in &swept_participants {
        let c = chain(*pid);
        let hits: Vec<_> = c.iter().filter(|x| target.contains(x)).collect();
        println!(
            "  chain({pid:?}) len={} contains leftover member: {:?}",
            c.len(),
            hits
        );
        // also: is this participant still a member in each order?
        println!(
            "     member in merge(A,B)={} in merge(B,A)={}",
            ab.members.members.iter().any(|m| m.member.id() == *pid),
            ba.members.members.iter().any(|m| m.member.id() == *pid)
        );
    }
}
