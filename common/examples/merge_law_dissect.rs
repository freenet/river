//! Dissect one commutativity counterexample field-by-field.
//!
//! Reproduces the `#[composable]` macro's `apply_delta` by hand (clone-per-field,
//! composable field order) so we can snapshot the state BEFORE
//! `post_apply_cleanup` runs, then:
//!   * account for every DM lost, separating "dropped inside
//!     DirectMessagesV1::apply_delta" from "dropped by post_apply_cleanup";
//!   * evaluate each `post_apply_cleanup` retention rule for a named member
//!     against the pre-cleanup state and against the returned state.
//!
//! Usage:
//!   cargo run --release --example merge_law_dissect -- <dir> <a.cbor> <b.cbor> [MEMBER_PREFIX]

use ciborium::de::from_reader;
use freenet_scaffold::ComposableState;
use river_core::room_state::ban::BansV1;
use river_core::room_state::direct_messages::{
    AuthorizedDirectMessage, DirectMessagesV1, DmRetentionHorizon,
};
use river_core::room_state::member::MemberId;
use river_core::room_state::ChatRoomParametersV1;
use river_core::ChatRoomStateV1;
use std::collections::{HashMap, HashSet};
use std::fs;

fn load<T: serde::de::DeserializeOwned>(p: &str) -> T {
    let b = fs::read(p).unwrap_or_else(|e| panic!("read {p}: {e}"));
    from_reader::<T, &[u8]>(&b[..]).unwrap_or_else(|e| panic!("decode {p}: {e}"))
}

fn dm_key(m: &AuthorizedDirectMessage) -> String {
    format!(
        "{:?}->{:?}@{:?}",
        m.message.sender, m.message.recipient, m.message.timestamp
    )
}

fn dm_sigs(d: &DirectMessagesV1) -> HashSet<[u8; 64]> {
    d.messages
        .iter()
        .map(|m| m.sender_signature.to_bytes())
        .collect()
}

struct Traced {
    pre_cleanup: ChatRoomStateV1,
    post: ChatRoomStateV1,
    delta_dm_offered: usize,
    dm_after_field_apply: usize,
    dm_in_receiver: usize,
    receiver_horizon: DmRetentionHorizon,
    delta_was_none: bool,
    /// DMs that the delta offered but that were NOT present after the DM field's
    /// apply_delta (silently dropped inside apply_delta, or trimmed by the caps).
    offered_but_absent: Vec<AuthorizedDirectMessage>,
    /// DMs the receiver already held that are gone after the field apply.
    held_but_absent: Vec<AuthorizedDirectMessage>,
    /// Member set as it stood when `direct_messages.apply_delta` ran.
    members_at_dm_apply: HashSet<MemberId>,
}

/// Hand-rolled equivalent of the generated `ChatRoomStateV1::apply_delta`,
/// stopping just before `post_apply_cleanup`.
fn merge_traced(a: &ChatRoomStateV1, b: &ChatRoomStateV1, p: &ChatRoomParametersV1) -> Traced {
    let parent = a.clone();
    let my_summary = a.summarize(&parent, p);
    let delta_in = b.delta(&parent, p, &my_summary);

    let receiver_horizon = a.direct_messages.global_retention_horizon(
        a.configuration
            .configuration
            .effective_max_direct_messages(),
    );

    let mut s = a.clone();
    let mut delta_dm_offered = 0;
    let mut members_at_dm_apply: HashSet<MemberId> = HashSet::new();
    let mut offered: Vec<AuthorizedDirectMessage> = vec![];
    let delta_was_none = delta_in.is_none();

    if let Some(d) = &delta_in {
        if let Some(dmd) = &d.direct_messages {
            delta_dm_offered = dmd.new_messages.len();
            offered = dmd.new_messages.clone();
        }
        macro_rules! step {
            ($f:ident) => {{
                let c = s.clone();
                s.$f.apply_delta(&c, p, &d.$f).expect(stringify!($f));
            }};
        }
        step!(configuration);
        step!(bans);
        step!(members);
        step!(member_info);
        step!(secrets);
        step!(recent_messages);
        members_at_dm_apply = s.members.members.iter().map(|m| m.member.id()).collect();
        step!(direct_messages);
        step!(upgrade);
        step!(version);
    }

    let pre_cleanup = s.clone();
    let after_sigs = dm_sigs(&s.direct_messages);

    let offered_but_absent: Vec<_> = offered
        .iter()
        .filter(|m| !after_sigs.contains(&m.sender_signature.to_bytes()))
        .cloned()
        .collect();
    let held_but_absent: Vec<_> = a
        .direct_messages
        .messages
        .iter()
        .filter(|m| !after_sigs.contains(&m.sender_signature.to_bytes()))
        .cloned()
        .collect();

    let mut post = s.clone();
    if delta_in.is_some() {
        post.post_apply_cleanup(p).expect("cleanup");
    }

    Traced {
        dm_after_field_apply: pre_cleanup.direct_messages.messages.len(),
        dm_in_receiver: a.direct_messages.messages.len(),
        pre_cleanup,
        post,
        delta_dm_offered,
        receiver_horizon,
        delta_was_none,
        offered_but_absent,
        held_but_absent,
        members_at_dm_apply,
    }
}

/// Replicate `post_apply_cleanup` steps 1+2 against an arbitrary state and
/// report the seed set and the invite-chain closure, so we can ask WHY a
/// particular member was kept.
struct Retention {
    message_authors: HashSet<MemberId>,
    dm_participants: HashSet<MemberId>,
    secret_recipients: HashSet<MemberId>,
    ban_banners: HashSet<MemberId>,
    seeds: HashSet<MemberId>,
    required: HashSet<MemberId>,
    /// member -> the seed(s) whose upward chain reaches it
    chain_witnesses: HashMap<MemberId, Vec<MemberId>>,
}

fn retention(s: &ChatRoomStateV1, p: &ChatRoomParametersV1) -> Retention {
    let owner_id = MemberId::from(&p.owner);
    let members_by_id = s.members.members_by_member_id();

    let message_authors: HashSet<MemberId> = s
        .recent_messages
        .messages
        .iter()
        .map(|m| m.message.author)
        .collect();
    let dm_participants: HashSet<MemberId> = s.direct_messages.active_participants();
    let cur = s.secrets.current_version;
    let secret_recipients: HashSet<MemberId> = s
        .secrets
        .encrypted_secrets
        .iter()
        .filter(|x| x.secret.secret_version == cur)
        .map(|x| x.secret.member_id)
        .collect();

    let mut seeds: HashSet<MemberId> = HashSet::new();
    for id in message_authors
        .iter()
        .chain(dm_participants.iter())
        .chain(secret_recipients.iter())
    {
        if *id != owner_id && members_by_id.contains_key(id) {
            seeds.insert(*id);
        }
    }
    let mut ban_banners = HashSet::new();
    for ban in &s.bans.0 {
        let banner = ban.banned_by;
        if banner != owner_id
            && BansV1::ban_signature_matches_current_key(ban, &members_by_id, owner_id, &p.owner)
        {
            ban_banners.insert(banner);
            seeds.insert(banner);
        }
    }

    // Closure, tracking which seed reached each added ancestor.
    let mut required = seeds.clone();
    let mut witnesses: HashMap<MemberId, Vec<MemberId>> = HashMap::new();
    let mut to_process: Vec<(MemberId, MemberId)> = seeds.iter().map(|s| (*s, *s)).collect();
    while let Some((id, seed)) = to_process.pop() {
        if let Some(m) = members_by_id.get(&id) {
            let inv = m.member.invited_by;
            if inv != owner_id && !required.contains(&inv) {
                required.insert(inv);
                witnesses.entry(inv).or_default().push(seed);
                to_process.push((inv, seed));
            }
        }
    }

    Retention {
        message_authors,
        dm_participants,
        secret_recipients,
        ban_banners,
        seeds,
        required,
        chain_witnesses: witnesses,
    }
}

fn report_member(tag: &str, s: &ChatRoomStateV1, p: &ChatRoomParametersV1, who: MemberId) {
    let r = retention(s, p);
    let present = s.members.members.iter().any(|m| m.member.id() == who);
    println!(
        "  [{tag}] {who:?} present={present} author={} dm_participant={} secret_recipient={} ban_banner={} seed={} required={} chain_witness={:?}",
        r.message_authors.contains(&who),
        r.dm_participants.contains(&who),
        r.secret_recipients.contains(&who),
        r.ban_banners.contains(&who),
        r.seeds.contains(&who),
        r.required.contains(&who),
        r.chain_witnesses.get(&who),
    );
    // Who does this member invite (i.e. is it an ancestor of anybody)?
    let invitees: Vec<MemberId> = s
        .members
        .members
        .iter()
        .filter(|m| m.member.invited_by == who)
        .map(|m| m.member.id())
        .collect();
    println!("     invitees_in_this_state={:?}", invitees);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("dir");
    let fa = args.next().expect("A");
    let fb = args.next().expect("B");
    let target_prefix = args.next();

    let p: ChatRoomParametersV1 = load(&format!("{dir}/params.bin"));
    let sa: ChatRoomStateV1 = load(&format!("{dir}/{fa}"));
    let sb: ChatRoomStateV1 = load(&format!("{dir}/{fb}"));

    let cap = sa
        .configuration
        .configuration
        .effective_max_direct_messages();
    println!("effective_max_direct_messages (A cfg) = {cap}");
    println!(
        "effective_max_direct_messages (B cfg) = {}",
        sb.configuration
            .configuration
            .effective_max_direct_messages()
    );
    println!(
        "A={fa}: members={} msgs={} dms={} purges={} bans={}",
        sa.members.members.len(),
        sa.recent_messages.messages.len(),
        sa.direct_messages.messages.len(),
        sa.direct_messages.purges.len(),
        sa.bans.0.len()
    );
    println!(
        "B={fb}: members={} msgs={} dms={} purges={} bans={}",
        sb.members.members.len(),
        sb.recent_messages.messages.len(),
        sb.direct_messages.messages.len(),
        sb.direct_messages.purges.len(),
        sb.bans.0.len()
    );

    // Raw set relationship between A's and B's DM holdings.
    {
        let ka = dm_sigs(&sa.direct_messages);
        let kb = dm_sigs(&sb.direct_messages);
        println!(
            "\nDM holdings: |A|={} |B|={} |A n B|={} |A\\B|={} |B\\A|={}",
            ka.len(),
            kb.len(),
            ka.intersection(&kb).count(),
            ka.difference(&kb).count(),
            kb.difference(&ka).count()
        );
    }

    let mut results: Vec<(String, ChatRoomStateV1)> = vec![];
    for (tag, x, y) in [("AB", &sa, &sb), ("BA", &sb, &sa)] {
        println!("\n================ merge({tag}) ================");
        let t = merge_traced(x, y, &p);
        println!("delta was None: {}", t.delta_was_none);
        println!(
            "receiver held {} DMs, horizon={:?}",
            t.dm_in_receiver, t.receiver_horizon
        );
        println!("delta offered {} new DMs", t.delta_dm_offered);
        println!(
            "after DM field apply_delta: {} DMs (receiver {} + offered {} = {} before caps/drops)",
            t.dm_after_field_apply,
            t.dm_in_receiver,
            t.delta_dm_offered,
            t.dm_in_receiver + t.delta_dm_offered
        );
        println!(
            "  offered-but-absent after apply: {}",
            t.offered_but_absent.len()
        );
        for m in t.offered_but_absent.iter().take(12) {
            println!(
                "     OFFERED-DROPPED {} sender_in_members_at_apply={} recipient_in_members_at_apply={}",
                dm_key(m),
                t.members_at_dm_apply.contains(&m.message.sender),
                t.members_at_dm_apply.contains(&m.message.recipient),
            );
        }
        for pref in ["WAM6AFJB", "QFNGNRMO"] {
            let hit: Vec<String> = t
                .members_at_dm_apply
                .iter()
                .map(|i| format!("{i:?}"))
                .filter(|s| s.contains(pref))
                .collect();
            println!("     members_at_dm_apply contains {pref}: {:?}", hit);
        }
        println!(
            "     members_at_dm_apply.len()={}",
            t.members_at_dm_apply.len()
        );
        println!(
            "  held-but-absent after apply: {} (receiver DMs the apply removed)",
            t.held_but_absent.len()
        );
        for m in t.held_but_absent.iter().take(12) {
            println!("     HELD-DROPPED {}", dm_key(m));
        }
        println!(
            "post_apply_cleanup: {} DMs -> {} DMs, members {} -> {}",
            t.pre_cleanup.direct_messages.messages.len(),
            t.post.direct_messages.messages.len(),
            t.pre_cleanup.members.members.len(),
            t.post.members.members.len(),
        );
        let pre_k: HashSet<String> = t
            .pre_cleanup
            .direct_messages
            .messages
            .iter()
            .map(dm_key)
            .collect();
        let post_k: HashSet<String> = t.post.direct_messages.messages.iter().map(dm_key).collect();
        for k in pre_k.difference(&post_k).take(12) {
            println!("     CLEANUP-SWEPT {k}");
        }

        // Step-by-step of post_apply_cleanup: where does each interesting id go?
        {
            let owner_id = MemberId::from(&p.owner);
            let s0 = &t.pre_cleanup;
            let watch: Vec<MemberId> = ["WAM6AFJB", "QFNGNRMO"]
                .iter()
                .filter_map(|pref| {
                    s0.members
                        .members
                        .iter()
                        .map(|m| m.member.id())
                        .find(|id| format!("{id:?}").contains(pref))
                })
                .collect();
            let present = |s: &ChatRoomStateV1, id: MemberId| {
                s.members.members.iter().any(|m| m.member.id() == id)
            };
            println!("  --- cleanup step trace (watch {:?}) ---", watch);
            for id in &watch {
                println!("     pre-cleanup present({id:?})={}", present(s0, *id));
            }
            // step 0: ban enforcement
            let mut st = s0.clone();
            let enforced = st.members.banned_member_ids(&st.bans, &st.member_info, &p);
            for id in &watch {
                println!("     enforced_banned({id:?})={}", enforced.contains(id));
            }
            st.members
                .members
                .retain(|m| !enforced.contains(&m.member.id()));
            for id in &watch {
                println!("     after step0 present({id:?})={}", present(&st, *id));
            }
            // steps 1+2 against the post-step-0 state
            let r = retention(&st, &p);
            for id in &watch {
                println!(
                    "     step1/2: dm_participant={} in_members_map={} required={} (id {id:?})",
                    r.dm_participants.contains(id),
                    st.members.members.iter().any(|m| m.member.id() == *id),
                    r.required.contains(id),
                );
            }
            // is the *other* end of each watched DM present?
            for m in s0.direct_messages.messages.iter().filter(|m| {
                watch.contains(&m.message.sender) || watch.contains(&m.message.recipient)
            }) {
                println!(
                    "     DM {} sender_member={} recipient_member={} sender_enforced_banned={} recipient_enforced_banned={}",
                    dm_key(m),
                    present(&st, m.message.sender),
                    present(&st, m.message.recipient),
                    enforced.contains(&m.message.sender),
                    enforced.contains(&m.message.recipient),
                );
            }
            // does the owner count? (owner is implicit, never in members list)
            println!("     owner_id={owner_id:?}");
        }

        if let Some(pref) = &target_prefix {
            let find = |s: &ChatRoomStateV1| -> Option<MemberId> {
                s.members
                    .members
                    .iter()
                    .map(|m| m.member.id())
                    .find(|id| format!("{id:?}").contains(pref.as_str()))
            };
            if let Some(who) = find(&t.pre_cleanup).or_else(|| find(&t.post)) {
                println!("  --- retention for {pref} ---");
                report_member("pre_cleanup", &t.pre_cleanup, &p, who);
                report_member("post", &t.post, &p, who);
            } else {
                println!("  target {pref} not a member in either snapshot of {tag}");
            }
        }
        // Precise membership of the interesting DMs: in A? in B? in the result?
        {
            let ka = dm_sigs(&sa.direct_messages);
            let kb = dm_sigs(&sb.direct_messages);
            let mut interesting: Vec<AuthorizedDirectMessage> = t.offered_but_absent.clone();
            let post_sigs = dm_sigs(&t.post.direct_messages);
            for m in &t.pre_cleanup.direct_messages.messages {
                if !post_sigs.contains(&m.sender_signature.to_bytes()) {
                    interesting.push(m.clone());
                }
            }
            for m in &interesting {
                let sig = m.sender_signature.to_bytes();
                println!(
                    "     LOST {} sig={:02x}{:02x}{:02x}{:02x} in_A={} in_B={}",
                    dm_key(m),
                    sig[0],
                    sig[1],
                    sig[2],
                    sig[3],
                    ka.contains(&sig),
                    kb.contains(&sig),
                );
            }
        }
        // Idempotence check on the returned state.
        {
            let mut again = t.post.clone();
            again.post_apply_cleanup(&p).unwrap();
            println!(
                "  cleanup pass 2: members {} -> {}, dms {} -> {}  (idempotent={})",
                t.post.members.members.len(),
                again.members.members.len(),
                t.post.direct_messages.messages.len(),
                again.direct_messages.messages.len(),
                again == t.post
            );
        }
        results.push((tag.to_string(), t.post.clone()));
    }

    // Do the two orders end with the same DM SET, or merely different counts?
    let (na, ra) = &results[0];
    let (nb, rb) = &results[1];
    let ka = dm_sigs(&ra.direct_messages);
    let kb = dm_sigs(&rb.direct_messages);
    println!(
        "\nresult DM sets: |{na}|={} |{nb}|={} |only {na}|={} |only {nb}|={}",
        ka.len(),
        kb.len(),
        ka.difference(&kb).count(),
        kb.difference(&ka).count()
    );
    {
        let mb_ids: HashSet<MemberId> = rb.members.members.iter().map(|m| m.member.id()).collect();
        let owner_id = MemberId::from(&p.owner);
        for m in &ra.direct_messages.messages {
            if !kb.contains(&m.sender_signature.to_bytes()) {
                println!(
                    "  ONLY-IN-{na}: {} sender_is_member_of_{nb}={} recipient_is_member_of_{nb}={}",
                    dm_key(m),
                    m.message.sender == owner_id || mb_ids.contains(&m.message.sender),
                    m.message.recipient == owner_id || mb_ids.contains(&m.message.recipient),
                );
            }
        }
    }
    let ma: HashSet<MemberId> = ra.members.members.iter().map(|m| m.member.id()).collect();
    let mb: HashSet<MemberId> = rb.members.members.iter().map(|m| m.member.id()).collect();
    println!(
        "result member sets: |{na}|={} |{nb}|={} |only {na}|={:?} |only {nb}|={:?}",
        ma.len(),
        mb.len(),
        ma.difference(&mb).collect::<Vec<_>>(),
        mb.difference(&ma).collect::<Vec<_>>()
    );
}
