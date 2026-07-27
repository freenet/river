//! Regression tests for the GLOBAL direct-message retention cap
//! (freenet/river#519).
//!
//! # The bug
//!
//! `DirectMessagesV1` bounded each ordered `(sender, recipient)` pair at
//! `MAX_DM_MESSAGES_PER_PAIR`, but nothing bounded the set as a whole. Because
//! `ChatRoomStateV1::post_apply_cleanup` exempts every DM sender and recipient
//! from inactivity-prune (via `DirectMessagesV1::active_participants`), an
//! unbounded DM set pinned an unbounded MEMBER set: anyone who had ever
//! exchanged a single DM stayed a member forever. Measured on the live official
//! room: 181 members, 499 DMs, 129 of those members held zero messages and were
//! kept solely by a DM, and `remove_excess_members` could prune none of them
//! because it early-returns under `max_members`.
//!
//! # The fix, and the two things that could break production
//!
//! A global cap (`Configuration::effective_max_direct_messages`) trimmed in
//! `apply_delta`. Two hazards, each pinned below:
//!
//! 1. **The resend loop.** Trimming makes the merge non-monotonic, so `delta`
//!    must not offer a DM the receiver would discard on arrival — otherwise the
//!    pair re-offers it forever at up to 32 KiB a time. The per-pair
//!    `DmPairHorizon` cannot describe this axis: a DM can sit comfortably
//!    inside its own pair's window and still be the oldest DM in the room. Hence
//!    `DmRetentionHorizon`.
//!
//! 2. **Bricking every existing room.** `Configuration` is owner-signed by
//!    re-serializing the struct, so a naively-added field changes the signed
//!    bytes and makes `verify` — which also gates the #292 migration PUT — fail
//!    for every room created before the field existed. See
//!    `legacy_configuration_bytes_still_verify_after_adding_the_field`.

use ed25519_dalek::{Signature, SigningKey};
use freenet_scaffold::ComposableState;
use river_core::room_state::configuration::{
    AuthorizedConfigurationV1, Configuration, DEFAULT_MAX_DIRECT_MESSAGES,
};
use river_core::room_state::direct_messages::{
    sign_direct_message, AuthorizedDirectMessage, DirectMessagesDelta, DirectMessagesV1,
    DmRetentionHorizon, MAX_DM_MESSAGES_PER_PAIR,
};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersV1};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1};

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// Epoch offset so generated timestamps read as small integers.
const BASE_TS: u64 = 1_700_000_000;

struct Room {
    params: ChatRoomParametersV1,
    state: ChatRoomStateV1,
    owner_sk: SigningKey,
    member_sks: Vec<SigningKey>,
}

impl Room {
    fn id(&self, i: usize) -> MemberId {
        MemberId::from(&self.member_sks[i].verifying_key())
    }

    /// The same room re-signed at a different global DM cap, so a test can
    /// model two peers of one room running different configurations.
    fn state_with_cap(&self, cap: usize) -> ChatRoomStateV1 {
        let mut cfg = self.state.configuration.configuration.clone();
        cfg.max_direct_messages = Some(cap);
        ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(cfg, &self.owner_sk),
            ..self.state.clone()
        }
    }
}

/// A room with `n_members` members (all invited directly by the owner, so no
/// invite chains complicate `post_apply_cleanup`) and the given global DM cap.
///
/// Deterministic keys: `SigningKey::from_bytes` on a seed rather than `OsRng`,
/// so a failure reproduces exactly. Signatures — and therefore the tiebreak
/// half of `DmOrderKey` — are then identical on every run.
fn room(n_members: usize, max_direct_messages: Option<usize>) -> Room {
    let owner_sk = SigningKey::from_bytes(&[1u8; 32]);
    let owner_vk = owner_sk.verifying_key();
    let owner_id = MemberId::from(&owner_vk);

    let member_sks: Vec<SigningKey> = (0..n_members)
        .map(|i| SigningKey::from_bytes(&[(i + 10) as u8; 32]))
        .collect();

    let members: Vec<AuthorizedMember> = member_sks
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

    let config = Configuration {
        max_members: 1000,
        max_direct_messages,
        ..Default::default()
    };

    Room {
        params: ChatRoomParametersV1 { owner: owner_vk },
        state: ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(config, &owner_sk),
            members: MembersV1 { members },
            ..Default::default()
        },
        owner_sk,
        member_sks,
    }
}

/// A signed DM from member `from` to member `to` at `BASE_TS + offset`.
fn dm(r: &Room, from: usize, to: usize, offset: u64) -> AuthorizedDirectMessage {
    sign_direct_message(
        &r.member_sks[from],
        r.id(from),
        r.id(to),
        &r.params.owner,
        BASE_TS + offset,
        vec![(offset % 251) as u8; 8],
    )
    .expect("sign dm")
}

/// A DM universe spread across `n_pairs` disjoint ordered pairs, one DM per
/// `offset`. Pairs are `(2k, 2k+1)`, so each pair stays far below
/// `MAX_DM_MESSAGES_PER_PAIR` and the PER-PAIR horizon is absent — isolating
/// the global axis under test.
fn spread(
    r: &Room,
    offsets: impl IntoIterator<Item = u64>,
    n_pairs: usize,
) -> Vec<AuthorizedDirectMessage> {
    offsets
        .into_iter()
        .map(|o| {
            let k = (o as usize) % n_pairs;
            dm(r, 2 * k, 2 * k + 1, o)
        })
        .collect()
}

/// `DirectMessagesV1` holding the given DMs, normalised through `apply_delta`
/// against `parent` exactly as a real peer's stored state always is.
fn dms_holding_under(
    parent: &ChatRoomStateV1,
    params: &ChatRoomParametersV1,
    msgs: Vec<AuthorizedDirectMessage>,
) -> DirectMessagesV1 {
    let mut d = DirectMessagesV1::default();
    apply(&mut d, parent, params, msgs);
    d
}

fn apply(
    d: &mut DirectMessagesV1,
    parent: &ChatRoomStateV1,
    params: &ChatRoomParametersV1,
    msgs: Vec<AuthorizedDirectMessage>,
) {
    d.apply_delta(
        parent,
        params,
        &Some(DirectMessagesDelta {
            new_messages: msgs,
            advanced_purges: vec![],
        }),
    )
    .expect("dms must apply");
}

/// The timestamp offsets a `DirectMessagesV1` currently retains, ascending.
fn held(d: &DirectMessagesV1) -> Vec<u64> {
    let mut v: Vec<u64> = d
        .messages
        .iter()
        .map(|m| m.message.timestamp - BASE_TS)
        .collect();
    v.sort_unstable();
    v
}

// ---------------------------------------------------------------------------
// The bound itself
// ---------------------------------------------------------------------------

/// The cap is enforced across pairs, and it keeps the NEWEST — not an
/// arrival-order-dependent subset.
///
/// Phrased as an exact equality against "the newest `cap` offsets" so a change
/// that raises or removes the cap fails rather than passing vacuously.
#[test]
fn global_cap_bounds_the_whole_dm_set_keeping_the_newest() {
    let r = room(6, Some(40));

    // 150 DMs over 3 pairs: 50 per pair, so no pair is anywhere near
    // MAX_DM_MESSAGES_PER_PAIR and only the global cap can bite.
    let input = spread(&r, 0..150, 3);

    // Premise measured on the INPUT, not on the trimmed result. Folding over
    // the post-trim state would be unfalsifiable: it holds at most `cap` (40)
    // messages in total, so no pair could ever reach 100 whatever the fixture
    // did, and the guard would pass vacuously.
    let busiest_pair = input
        .iter()
        .fold(
            std::collections::HashMap::<_, usize>::new(),
            |mut acc, m| {
                *acc.entry((m.message.sender, m.message.recipient))
                    .or_default() += 1;
                acc
            },
        )
        .into_values()
        .max()
        .expect("state holds DMs");
    assert!(
        busiest_pair < MAX_DM_MESSAGES_PER_PAIR,
        "test premise: no pair may reach the per-pair cap ({busiest_pair}), or \
         the per-pair trim would be doing this test's work"
    );

    let d = dms_holding_under(&r.state, &r.params, input);
    assert_eq!(
        d.messages.len(),
        40,
        "the global cap must bound the whole DM set"
    );
    assert_eq!(
        held(&d),
        (110..150).collect::<Vec<u64>>(),
        "the trim must keep the newest `cap` DMs by (timestamp, signature)"
    );
}

/// The default applies to rooms whose owner never set the field — which is
/// every room that exists today. Without this, the fix would be inert in
/// production until each owner re-signed their configuration.
#[test]
fn unset_cap_falls_back_to_the_default_rather_than_being_unbounded() {
    // Enough pairs that ~525 DMs stay under MAX_DM_MESSAGES_PER_PAIR each,
    // so the DEFAULT global cap is the only thing that can trim.
    const PAIRS: usize = 8;
    let r = room(2 * PAIRS, None);
    assert_eq!(
        r.state
            .configuration
            .configuration
            .effective_max_direct_messages(),
        DEFAULT_MAX_DIRECT_MESSAGES,
        "a configuration signed before the field existed must still be bounded"
    );

    let over = DEFAULT_MAX_DIRECT_MESSAGES + 25;
    assert!(
        over.div_ceil(PAIRS) < MAX_DM_MESSAGES_PER_PAIR,
        "test premise: no pair may hit its own cap, or the per-pair trim would \
         be doing this test's work"
    );
    let d = dms_holding_under(&r.state, &r.params, spread(&r, 0..over as u64, PAIRS));
    assert_eq!(
        d.messages.len(),
        DEFAULT_MAX_DIRECT_MESSAGES,
        "the default cap must actually trim"
    );
}

/// The knob is real: an owner-set value overrides the default in both
/// directions.
#[test]
fn owner_set_cap_overrides_the_default() {
    let r = room(6, Some(7));
    let d = dms_holding_under(&r.state, &r.params, spread(&r, 0..60, 3));
    assert_eq!(held(&d), (53..60).collect::<Vec<u64>>());
}

/// Both caps hold simultaneously, and the global trim can never resurrect a DM
/// the per-pair trim discarded. Pins the ORDER of the two trims in
/// `enforce_caps_and_sort`.
#[test]
fn per_pair_cap_still_holds_under_a_generous_global_cap() {
    // One pair, more than the per-pair cap, with a global cap far above it.
    let r = room(4, Some(10_000));
    let msgs: Vec<AuthorizedDirectMessage> = (0..MAX_DM_MESSAGES_PER_PAIR as u64 + 30)
        .map(|o| dm(&r, 0, 1, o))
        .collect();
    let d = dms_holding_under(&r.state, &r.params, msgs);

    assert_eq!(
        d.messages.len(),
        MAX_DM_MESSAGES_PER_PAIR,
        "the per-pair cap must still bind when the global cap is slack"
    );
    // And the state must be verifiable — `verify` rejects an over-cap pair.
    let state = ChatRoomStateV1 {
        direct_messages: d,
        ..r.state.clone()
    };
    assert!(
        state.direct_messages.verify(&state, &r.params).is_ok(),
        "a state left over the PER-PAIR cap would fail verify"
    );
}

/// Pins the ORDER of the two trims in `enforce_caps_and_sort`.
///
/// Either order leaves every pair legal — whichever runs last only shrinks the
/// set — so legality cannot detect a swap. What a swap loses is RETENTION: the
/// global trim would spend the budget on DMs the per-pair trim then discards,
/// leaving the peer below its own cap while DMs that would have fitted were
/// dropped.
///
/// One busy pair holds the room's newest 150 DMs (over the per-pair cap of
/// 100); three quiet pairs hold 100 older ones; the global cap is 200.
/// Pair-first retains the full 200. Global-first would keep the newest 200
/// (all 150 busy + 50 quiet), then cut the busy pair to 100 — only 150.
#[test]
fn pair_trim_runs_before_the_global_trim_so_the_budget_is_not_wasted() {
    let r = room(8, Some(200));

    let mut msgs: Vec<AuthorizedDirectMessage> = spread(&r, 0..100, 3);
    msgs.extend((100..250).map(|o| dm(&r, 6, 7, o)));

    let d = dms_holding_under(&r.state, &r.params, msgs);

    assert_eq!(
        d.messages.len(),
        200,
        "the per-pair trim must run FIRST: trimming globally first spends the \
         200-DM budget on messages the per-pair cap then discards, leaving only \
         150 retained"
    );
    let busy = d
        .messages
        .iter()
        .filter(|m| m.message.sender == r.id(6))
        .count();
    assert_eq!(
        busy, MAX_DM_MESSAGES_PER_PAIR,
        "busy pair capped at its own cap"
    );
    assert_eq!(
        d.messages.len() - busy,
        100,
        "quiet pairs keep all their DMs"
    );
}

// ---------------------------------------------------------------------------
// Determinism / idempotence
// ---------------------------------------------------------------------------

/// Two peers that receive the same DMs in different orders — and in different
/// chunkings, so the cap bites at different intermediate points — must end up
/// byte-identical. This is what makes the trim a valid CRDT join rather than an
/// arrival-order lottery.
#[test]
fn global_trim_is_deterministic_across_arrival_orders() {
    let r = room(6, Some(25));
    let universe = spread(&r, 0..90, 3);

    let chunks: Vec<Vec<AuthorizedDirectMessage>> =
        universe.chunks(9).map(|c| c.to_vec()).collect();

    let mut forward = DirectMessagesV1::default();
    for c in &chunks {
        apply(&mut forward, &r.state, &r.params, c.clone());
    }

    let mut reverse = DirectMessagesV1::default();
    for c in chunks.iter().rev() {
        apply(&mut reverse, &r.state, &r.params, c.clone());
    }

    // Interleaved: odd chunks first, then even.
    let mut interleaved = DirectMessagesV1::default();
    for c in chunks.iter().skip(1).step_by(2) {
        apply(&mut interleaved, &r.state, &r.params, c.clone());
    }
    for c in chunks.iter().step_by(2) {
        apply(&mut interleaved, &r.state, &r.params, c.clone());
    }

    assert_eq!(
        forward.messages.len(),
        25,
        "test premise: the cap must bite"
    );
    assert_eq!(
        forward, reverse,
        "peers that received the same DMs in a different order diverged"
    );
    assert_eq!(
        forward, interleaved,
        "peers that received the same DMs in a different chunking diverged"
    );
}

/// Re-applying the same delta must not change the state, and neither must
/// applying an EMPTY delta — the latter runs the caps unconditionally so a
/// state that arrived over-cap by a path that skips `apply_delta` converges
/// down instead of sitting over-cap forever.
#[test]
fn apply_delta_is_idempotent_under_the_global_cap() {
    let r = room(6, Some(20));
    let msgs = spread(&r, 0..60, 3);

    let mut d = dms_holding_under(&r.state, &r.params, msgs.clone());
    let once = d.clone();

    apply(&mut d, &r.state, &r.params, msgs);
    assert_eq!(d, once, "re-applying the same DMs changed the state");

    d.apply_delta(&r.state, &r.params, &None)
        .expect("empty delta must apply");
    assert_eq!(d, once, "an empty delta changed the state");
}

/// An over-cap state that arrived by a path bypassing the trim (a full-state
/// PUT, or the #292 migration PUT carrying a legacy set larger than the room's
/// current cap) converges down on the next `apply_delta`, including an empty
/// one.
#[test]
fn an_over_cap_state_converges_down_on_the_next_apply() {
    let r = room(6, Some(10));
    // Build the over-cap set with a slack cap, then hand it to a peer whose
    // configuration caps at 10 — exactly the migration-PUT shape.
    let slack = r.state_with_cap(10_000);
    let mut d = dms_holding_under(&slack, &r.params, spread(&r, 0..50, 3));
    assert_eq!(d.messages.len(), 50, "test premise: state starts over-cap");

    d.apply_delta(&r.state, &r.params, &None)
        .expect("empty delta must apply");
    assert_eq!(
        held(&d),
        (40..50).collect::<Vec<u64>>(),
        "an over-cap state must converge down to the newest `cap`"
    );
}

/// The horizon must not depend on where in the `messages` vector the oldest DM
/// happens to sit. `sort_state` orders by `(sender, recipient, timestamp,
/// signature)`, so the room-wide oldest DM is NOT at index 0 — computing the
/// horizon by index would read a different key out of a differently-ordered
/// state and diverge the summary bytes between peers.
#[test]
fn global_horizon_is_independent_of_the_state_vec_order() {
    let r = room(6, Some(30));
    let mut d = dms_holding_under(&r.state, &r.params, spread(&r, 0..30, 3));

    let forward = d.global_retention_horizon(30);
    d.messages.reverse();
    let reversed = d.global_retention_horizon(30);

    assert!(
        matches!(forward, DmRetentionHorizon::OldestRetained(_)),
        "test premise: at capacity the horizon must be a real key, not Open"
    );
    assert_eq!(
        forward, reversed,
        "the global horizon changed with the vector order"
    );
}

// ---------------------------------------------------------------------------
// The resend loop
// ---------------------------------------------------------------------------

/// THE regression test for the loop this cap would otherwise open.
///
/// A is at the global cap holding the newest window. B holds an older,
/// overlapping window. Everything B has that A lacks is older than anything A
/// retains, so A would discard all of it — B must offer nothing.
///
/// Every pair here stays far below `MAX_DM_MESSAGES_PER_PAIR`, so the per-pair
/// horizon is empty and only the global horizon can suppress the offer.
#[test]
fn sender_does_not_offer_dms_the_receiver_would_globally_prune() {
    let r = room(6, Some(40));

    let a = dms_holding_under(&r.state, &r.params, spread(&r, 110..150, 3));
    let b = dms_holding_under(&r.state, &r.params, spread(&r, 100..140, 3));

    let a_summary = a.summarize(&r.state, &r.params);

    assert!(
        a_summary.pair_horizons.is_empty(),
        "test premise: no pair may be at its own cap, or the per-pair horizon \
         would be doing this test's work"
    );
    assert!(
        matches!(
            a_summary.global_horizon,
            DmRetentionHorizon::OldestRetained(_)
        ),
        "test premise: A is at the global cap so it must publish a real horizon"
    );

    let a_held = held(&a);
    let unseen: Vec<u64> = held(&b)
        .into_iter()
        .filter(|o| !a_held.contains(o))
        .collect();
    assert_eq!(
        unseen,
        (100..110).collect::<Vec<u64>>(),
        "test premise: B must hold DMs A lacks, all older than A's oldest"
    );

    assert_eq!(
        b.delta(&r.state, &r.params, &a_summary),
        None,
        "B offered DMs that A would discard on arrival — this is the resend loop"
    );
}

/// The loop detector, in the asymmetric shape that actually persists in
/// production: B lags and never receives anything newer, so its old window
/// never ages out. It pushes to A on every fan-out; if A discards the payload
/// its summary is unchanged, so B recomputes the identical payload forever.
#[test]
fn dm_gossip_from_a_lagging_peer_terminates_under_the_global_cap() {
    let r = room(6, Some(30));
    let mut a = dms_holding_under(&r.state, &r.params, spread(&r, 60..90, 3));
    let b = dms_holding_under(&r.state, &r.params, spread(&r, 30..60, 3));

    for round in 0..12 {
        let Some(offered) = b.delta(&r.state, &r.params, &a.summarize(&r.state, &r.params)) else {
            return; // terminated
        };
        let before = a.clone();
        apply(&mut a, &r.state, &r.params, offered.new_messages.clone());
        assert_ne!(
            a,
            before,
            "round {round}: A applied an offer of {} DMs and its state did not \
             change — B cannot see the rejection, so it will re-offer the same \
             payload forever. This is the resend loop.",
            offered.new_messages.len()
        );
    }
    panic!("gossip did not terminate within 12 rounds");
}

/// Two peers exchanging in both directions converge on the newest window, and
/// the exchange goes quiet.
#[test]
fn offset_windows_converge_and_go_quiet() {
    let r = room(6, Some(30));
    let mut a = dms_holding_under(&r.state, &r.params, spread(&r, 60..90, 3));
    let mut b = dms_holding_under(&r.state, &r.params, spread(&r, 45..75, 3));

    for _ in 0..12 {
        let to_a = b.delta(&r.state, &r.params, &a.summarize(&r.state, &r.params));
        if let Some(d) = &to_a {
            apply(&mut a, &r.state, &r.params, d.new_messages.clone());
        }
        let to_b = a.delta(&r.state, &r.params, &b.summarize(&r.state, &r.params));
        if let Some(d) = &to_b {
            apply(&mut b, &r.state, &r.params, d.new_messages.clone());
        }
        if to_a.is_none() && to_b.is_none() {
            assert_eq!(held(&a), (60..90).collect::<Vec<u64>>());
            assert_eq!(a, b, "peers went quiet while still holding different sets");
            return;
        }
    }
    panic!("the exchange never went quiet");
}

/// INTERLEAVED windows — the case that pins the horizon's DIRECTION.
///
/// `offset_windows_converge_and_go_quiet` above uses contiguous overlapping
/// windows, which converge even if `global_retention_horizon` published the
/// NEWEST held key instead of the oldest: each peer simply offers less and the
/// pair still ends up agreeing. That makes it blind to the one error mode the
/// design docs call the dangerous one — an OVER-stated horizon silently
/// withholding DMs the peer would have kept.
///
/// Interleaved windows expose it. A holds the even offsets, B the odd ones, so
/// every DM either peer lacks is interior to the other's range. With the
/// correct `min`, both converge on the newest 30 of the union. With a
/// max-published horizon, A is offered only the single key above its own
/// maximum, both sides then have nothing to offer, and the exchange goes quiet
/// with the two peers holding DIFFERENT sets — silent divergence, which is
/// worse than a loop because nothing surfaces it.
#[test]
fn interleaved_windows_converge_so_the_horizon_cannot_be_over_stated() {
    let r = room(6, Some(30));

    let evens: Vec<u64> = (60..120).step_by(2).collect();
    let odds: Vec<u64> = (61..120).step_by(2).collect();
    let mut a = dms_holding_under(&r.state, &r.params, spread(&r, evens, 3));
    let mut b = dms_holding_under(&r.state, &r.params, spread(&r, odds, 3));

    assert_eq!(a.messages.len(), 30, "test premise: A is at the global cap");
    assert_eq!(b.messages.len(), 30, "test premise: B is at the global cap");

    for _ in 0..12 {
        let to_a = b.delta(&r.state, &r.params, &a.summarize(&r.state, &r.params));
        if let Some(d) = &to_a {
            apply(&mut a, &r.state, &r.params, d.new_messages.clone());
        }
        let to_b = a.delta(&r.state, &r.params, &b.summarize(&r.state, &r.params));
        if let Some(d) = &to_b {
            apply(&mut b, &r.state, &r.params, d.new_messages.clone());
        }
        if to_a.is_none() && to_b.is_none() {
            assert_eq!(
                held(&a),
                (90..120).collect::<Vec<u64>>(),
                "the pair must converge on the newest 30 of the union; a horizon \
                 published from the NEWEST held key instead of the oldest leaves A \
                 holding its own window plus one"
            );
            assert_eq!(
                a, b,
                "the peers went quiet holding DIFFERENT sets — the horizon was \
                 over-stated, so each silently withheld DMs the other would have kept"
            );
            return;
        }
    }
    panic!("the interleaved exchange never went quiet");
}

/// A peer BELOW the global cap publishes `Open` and must still be offered older
/// DMs — the horizon must not suppress legitimate back-fill.
#[test]
fn a_peer_below_the_global_cap_still_receives_older_dms() {
    let r = room(6, Some(40));
    let a = dms_holding_under(&r.state, &r.params, spread(&r, 30..35, 3));
    let b = dms_holding_under(&r.state, &r.params, spread(&r, 0..35, 3));

    let a_summary = a.summarize(&r.state, &r.params);
    assert_eq!(
        a_summary.global_horizon,
        DmRetentionHorizon::Open,
        "test premise: A holds fewer than the cap so it must publish Open"
    );

    let delta = b
        .delta(&r.state, &r.params, &a_summary)
        .expect("B must back-fill the older DMs A is missing");
    assert_eq!(delta.new_messages.len(), 30);
}

/// A zero cap must suppress the delta outright rather than loop against a peer
/// that keeps nothing. `AuthorizedConfigurationV1::apply_delta` rejects a zero,
/// but `verify` does not, so an owner-signed zero can still arrive by the
/// full-state path.
#[test]
fn zero_global_cap_suppresses_the_delta_instead_of_looping() {
    let r = room(6, Some(40));
    let zero = r.state_with_cap(0);

    let receiver = DirectMessagesV1::default();
    let sender = dms_holding_under(&r.state, &r.params, spread(&r, 0..20, 3));

    let summary = receiver.summarize(&zero, &r.params);
    assert_eq!(summary.global_horizon, DmRetentionHorizon::Closed);
    assert_eq!(
        sender.delta(&zero, &r.params, &summary),
        None,
        "a sender must offer nothing to a peer that retains nothing"
    );

    // And a zero-cap peer that somehow holds DMs drops them on the next apply.
    let mut holding = dms_holding_under(&r.state, &r.params, spread(&r, 0..20, 3));
    holding
        .apply_delta(&zero, &r.params, &None)
        .expect("empty delta must apply");
    assert!(holding.messages.is_empty());
}

/// Peers of one room can legitimately run different caps while a configuration
/// update propagates. Neither direction may loop.
#[test]
fn asymmetric_caps_terminate_in_both_directions() {
    let r = room(6, Some(40));
    let tight = r.state_with_cap(10);
    let loose = r.state_with_cap(60);

    let mut small = dms_holding_under(&tight, &r.params, spread(&r, 0..40, 3));
    let mut big = dms_holding_under(&loose, &r.params, spread(&r, 20..70, 3));

    for _ in 0..15 {
        let to_small = big.delta(&loose, &r.params, &small.summarize(&tight, &r.params));
        if let Some(d) = &to_small {
            apply(&mut small, &tight, &r.params, d.new_messages.clone());
        }
        let to_big = small.delta(&tight, &r.params, &big.summarize(&loose, &r.params));
        if let Some(d) = &to_big {
            apply(&mut big, &loose, &r.params, d.new_messages.clone());
        }
        if to_small.is_none() && to_big.is_none() {
            assert_eq!(small.messages.len(), 10, "the tight peer must stay capped");
            return;
        }
    }
    panic!("asymmetric-cap exchange never went quiet");
}

/// The summary crosses the wire as CBOR, so the horizon must survive the exact
/// round trip `summarize_state` / `get_state_delta` perform. A horizon that
/// decoded back to the `#[serde(default)]` `Open` would silently re-open the
/// loop in production while every in-process test still passed.
#[test]
fn global_horizon_survives_the_cbor_round_trip_the_contract_does() {
    let r = room(6, Some(30));
    let a = dms_holding_under(&r.state, &r.params, spread(&r, 0..30, 3));
    let b = dms_holding_under(&r.state, &r.params, spread(&r, 100..130, 3));

    let summary = a.summarize(&r.state, &r.params);
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&summary, &mut bytes).expect("serialize summary");
    let decoded: <DirectMessagesV1 as ComposableState>::Summary =
        ciborium::de::from_reader(&bytes[..]).expect("deserialize summary");

    assert_eq!(
        decoded.global_horizon, summary.global_horizon,
        "the global horizon did not survive the CBOR round trip"
    );
    // And it still suppresses: everything B holds is newer, so B offers all 30 —
    // the point is that the decoded horizon is a real key, not a defaulted Open.
    assert!(matches!(
        decoded.global_horizon,
        DmRetentionHorizon::OldestRetained(_)
    ));
    assert_eq!(
        b.delta(&r.state, &r.params, &decoded)
            .expect("newer DMs must still be offered")
            .new_messages
            .len(),
        30
    );
}

// ---------------------------------------------------------------------------
// The #519 symptom: member pinning
// ---------------------------------------------------------------------------

/// The reason this cap exists. Members with no messages, kept alive solely by
/// being a DM participant, must now be bounded by the cap.
#[test]
fn global_cap_bounds_the_dm_pinned_member_set() {
    const CAP: usize = 5;
    let r = room(21, Some(CAP));

    // Every member 1..21 sends exactly one DM to member 0, at increasing
    // timestamps. Nobody authors a room message, so DMs are the ONLY thing
    // keeping any of them in the member list.
    let msgs: Vec<AuthorizedDirectMessage> = (1..21).map(|i| dm(&r, i, 0, i as u64)).collect();

    let mut state = r.state.clone();
    state.direct_messages = dms_holding_under(&r.state, &r.params, msgs);
    assert_eq!(
        state.direct_messages.messages.len(),
        CAP,
        "test premise: the global cap must have trimmed the DM set"
    );

    state
        .post_apply_cleanup(&r.params)
        .expect("cleanup must succeed");

    // The 5 surviving DMs are from members 16..21 to member 0: 6 members.
    // Without the global cap all 20 senders plus member 0 — 21 members — would
    // be pinned, which is the freenet/river#519 symptom.
    assert_eq!(
        state.members.members.len(),
        6,
        "the member set pinned by DM MESSAGES must be bounded by the DM cap"
    );
    // Scoped to this fixture on purpose. `2 * CAP` bounds the members pinned by
    // retained MESSAGES, and this fixture has no purge envelopes — but
    // `active_participants` ALSO pins every `purges[].recipient_id`, and nothing
    // bounds the NUMBER of purge envelopes (`MAX_PURGED_TOMBSTONES_PER_RECIPIENT`
    // caps entries WITHIN one envelope, not the envelope count). So a member who
    // purges their last DM still pins themselves indefinitely. That half of the
    // pinning problem is NOT closed by this change; asserting an unconditional
    // `<= 2 * CAP` here would claim a bound the code does not provide.
    assert!(
        state.direct_messages.purges.is_empty(),
        "fixture premise: no purge envelopes, so the members pinned here are \
         exactly those pinned by retained messages"
    );
    assert!(
        state.members.members.len() <= 2 * CAP,
        "at most two members can be pinned per retained DM"
    );
}

/// `post_apply_cleanup` must stay idempotent — Freenet runs it a variable
/// number of times across peers, so `cleanup(S) != cleanup(cleanup(S))` would
/// diverge the member set permanently.
#[test]
fn post_apply_cleanup_stays_idempotent_under_the_global_cap() {
    let r = room(21, Some(5));
    let msgs: Vec<AuthorizedDirectMessage> = (1..21).map(|i| dm(&r, i, 0, i as u64)).collect();

    let mut once = r.state.clone();
    once.direct_messages = dms_holding_under(&r.state, &r.params, msgs);
    once.post_apply_cleanup(&r.params).expect("cleanup 1");

    let mut twice = once.clone();
    twice.post_apply_cleanup(&r.params).expect("cleanup 2");

    assert_eq!(
        once.members.members, twice.members.members,
        "cleanup(S) != cleanup(cleanup(S)) for members"
    );
    assert_eq!(
        once.direct_messages, twice.direct_messages,
        "cleanup(S) != cleanup(cleanup(S)) for direct messages"
    );
}

/// The client-side merge, unrolled exactly as `room_synchronizer`'s
/// `merge_incoming_state` does it: the receiver summarises against its OWN
/// state (so the horizon reads the room's real cap), while `apply_delta` keeps
/// the cheap sentinel parent the `#[composable]` macro ignores anyway.
fn merge_incoming_state(
    local: &mut ChatRoomStateV1,
    params: &ChatRoomParametersV1,
    incoming: &ChatRoomStateV1,
) -> Result<(), String> {
    let summary = local.summarize(local, params);
    let delta = incoming.delta(incoming, params, &summary);
    local.apply_delta(&ChatRoomStateV1::default(), params, &delta)
}

/// WHOLE-STATE gossip through the real client merge path, asserting `verify()`
/// after every round.
///
/// The field-level tests above drive `DirectMessagesV1` against a FIXED parent
/// whose member list never changes. That misses the feedback loop this change
/// creates: trimming DMs makes `post_apply_cleanup` prune members (step 3 is an
/// unconditional `retain`, NOT gated on `max_members`), and the pruned member
/// set feeds straight back into `apply_delta`'s silent `resolve_member_vk` drop
/// and `sweep_after_membership_change`. Before this change a DM participant
/// could never be pruned — their DM was held forever, which is bug #519 — so
/// this path is newly live and has to be exercised end to end.
///
/// A state that fails `verify` here would not be a test failure but a broken
/// room: freenet-core rejects the update outright.
///
/// Scope note: this asserts quiescence of the DIRECT-MESSAGE channel, not of
/// the whole delta. The MEMBERS channel does not go quiet, for reasons that
/// predate this PR entirely — see
/// `member_prune_reoffer_loop_is_pre_existing_and_not_caused_by_the_dm_cap`.
#[test]
fn whole_state_gossip_under_the_cap_converges_and_stays_verifiable() {
    const CAP: usize = 8;
    let r = room(12, Some(CAP));

    // Per-pair CONTIGUOUS blocks, not the round-robin `spread`: pair k owns
    // offsets [100 + 20k, 120 + 20k). That concentrates the newest DMs in the
    // last pair or two, so the trim leaves most members unpinned and the prune
    // actually fires. Round-robin offsets would spread the surviving `CAP` DMs
    // across every pair and pin all 12 members, making this test vacuous.
    let blocks = |offsets: std::ops::Range<u64>| -> Vec<AuthorizedDirectMessage> {
        offsets
            .map(|o| {
                let k = ((o - 100) / 20) as usize;
                dm(&r, 2 * k, 2 * k + 1, o)
            })
            .collect()
    };

    // Nobody authors a room message, so DMs are the ONLY thing keeping any
    // member in the list — which is what makes the trim prune members.
    let mut a = r.state.clone();
    a.direct_messages = dms_holding_under(&r.state, &r.params, blocks(100..190));
    let mut b = r.state.clone();
    b.direct_messages = dms_holding_under(&r.state, &r.params, blocks(130..220));

    let members_before = r.state.members.members.len();
    let dm_offer = |from: &ChatRoomStateV1, to: &ChatRoomStateV1| -> usize {
        from.delta(from, &r.params, &to.summarize(to, &r.params))
            .and_then(|d| d.direct_messages.map(|dm| dm.new_messages.len()))
            .unwrap_or(0)
    };

    for round in 0..12 {
        merge_incoming_state(&mut a, &r.params, &b)
            .unwrap_or_else(|e| panic!("round {round}: merge into A failed: {e}"));
        assert!(
            a.verify(&a, &r.params).is_ok(),
            "round {round}: A failed verify after a merge — the cap-driven trim plus \
             member prune left state freenet-core would reject: {:?}",
            a.verify(&a, &r.params)
        );

        merge_incoming_state(&mut b, &r.params, &a)
            .unwrap_or_else(|e| panic!("round {round}: merge into B failed: {e}"));
        assert!(
            b.verify(&b, &r.params).is_ok(),
            "round {round}: B failed verify after a merge: {:?}",
            b.verify(&b, &r.params)
        );

        if dm_offer(&b, &a) == 0 && dm_offer(&a, &b) == 0 {
            assert!(
                a.direct_messages.messages.len() <= CAP,
                "the global cap must hold on the whole-state path"
            );
            assert_eq!(
                a.direct_messages, b.direct_messages,
                "the DM channel went quiet with the peers holding different sets — \
                 silent divergence"
            );
            assert!(
                a.members.members.len() < members_before,
                "test premise: the trim must actually have pruned members, or this \
                 test never exercises the feedback loop it exists for"
            );
            return;
        }
    }
    panic!("the DM channel never went quiet within 12 rounds");
}

/// Characterises a PRE-EXISTING loop that this PR does not cause and does not
/// fix, so the next reader does not mistake it for a regression of the DM cap.
///
/// `post_apply_cleanup` prunes members that nothing pins, but `MembersV1`'s
/// summary/delta has no retention horizon — so a peer that has pruned re-offers
/// nothing while a peer that has NOT pruned re-offers the same member records
/// forever, and the pruning peer discards them on every round. This is the
/// `MembersV1` non-monotonicity that `outbound_summary` in the UI already names
/// as deferred, alongside `BansV1`.
///
/// It is reproducible on `origin/main` with **zero DMs involved** — which is
/// what this test pins, deliberately using no DMs at all. The DM cap therefore
/// cannot be its cause. What the DM cap does change is EXPOSURE: DM
/// participants used to be exempt from the prune (bug #519), and now they are
/// not, so more members fall into the pruned-and-re-offered population. It
/// self-heals in any room with traffic, because the lagging peer runs
/// `post_apply_cleanup` as soon as it applies any delta.
///
/// Inverted assertion, in the same style as
/// `retention_loop_test.rs::ban_sweep_reopening_the_horizon_still_loops_known_gap`:
/// the day `MembersV1` gets a horizon, this test fails and forces whoever fixed
/// it to flip the polarity.
#[test]
fn member_prune_reoffer_loop_is_pre_existing_and_not_caused_by_the_dm_cap() {
    let r = room(10, Some(500));

    // Members pinned by NOTHING: no DMs, no messages, no secrets, no bans.
    assert!(
        r.state.direct_messages.messages.is_empty(),
        "test premise: no DMs, so the DM cap cannot be implicated"
    );

    let mut pruned = r.state.clone();
    pruned
        .post_apply_cleanup(&r.params)
        .expect("cleanup must succeed");
    assert!(
        pruned.members.members.is_empty(),
        "test premise: unpinned members must be pruned by cleanup"
    );
    let lagging = r.state.clone();

    for round in 0..5 {
        let offered = lagging
            .delta(&lagging, &r.params, &pruned.summarize(&pruned, &r.params))
            .unwrap_or_else(|| {
                panic!(
                    "round {round}: the lagging peer had nothing to offer. The \
                     MembersV1 non-monotonicity this test characterises has been \
                     CLOSED — that is good news. Rename this test, restore a \
                     terminating assertion, and update the note in \
                     `whole_state_gossip_under_the_cap_converges_and_stays_verifiable`."
                )
            });
        assert_eq!(
            offered.members.as_ref().map(|m| m.added().len()),
            Some(10),
            "round {round}: the offer must be the 10 member records the prune removed"
        );
        merge_incoming_state(&mut pruned, &r.params, &lagging)
            .unwrap_or_else(|e| panic!("round {round}: merge failed: {e}"));
        assert!(
            pruned.members.members.is_empty(),
            "round {round}: cleanup must re-prune them, which is exactly why the \
             lagging peer never learns and re-offers forever"
        );
    }
}

/// Duplicate entries must not defeat the cap.
///
/// `apply_delta` dedupes on insert, but `verify` has no distinct-signature
/// check, so a hand-built or hostile full-state PUT can carry duplicates. Both
/// trims key off `DmOrderKey`, and duplicates share one — so without
/// `dedup_by_signature` the global trim's cutoff can land on a repeated key and
/// `retain(>= cutoff)` drops NOTHING, leaving the peer permanently over the cap
/// with the member set still pinned.
#[test]
fn duplicate_entries_do_not_defeat_the_global_cap() {
    const CAP: usize = 20;
    let r = room(6, Some(CAP));

    // 30 copies of the single oldest DM (well under the per-pair cap, so the
    // pair trim does not clear them) plus 25 distinct newer ones. The cutoff
    // rank lands inside the duplicate run, which is the degenerate case.
    let oldest = dm(&r, 0, 1, 0);
    let mut messages: Vec<AuthorizedDirectMessage> = vec![oldest.clone(); 30];
    messages.extend(spread(&r, 100..125, 3));

    let mut d = DirectMessagesV1::default();
    d.messages = messages;
    d.apply_delta(&r.state, &r.params, &None)
        .expect("empty delta must apply");

    assert_eq!(
        d.messages.len(),
        CAP,
        "duplicates made the global trim a no-op, leaving the set over cap"
    );
    assert!(
        !d.messages
            .iter()
            .any(|m| m.sender_signature == oldest.sender_signature),
        "the duplicated oldest DM should have been trimmed away entirely"
    );
}

// ---------------------------------------------------------------------------
// Migration safety
// ---------------------------------------------------------------------------

/// The load-bearing migration pin.
///
/// `AuthorizedConfigurationV1::verify_signature` re-serializes `Configuration`
/// with ciborium and checks the owner's signature over those bytes. Adding
/// `max_direct_messages` as a plain `#[serde(default)] usize` would make the
/// re-serialized bytes differ from what every existing owner signed, so
/// `verify` — which also gates the #292 migration PUT — would fail for every
/// room in existence. `Option` + `skip_serializing_if` keeps the addition
/// byte-neutral.
///
/// The fixture is the REAL CBOR encoding of `Configuration::default()` captured
/// from `origin/main` before this change, not a re-derivation from the current
/// struct — otherwise the test would move in lockstep with the bug it guards.
#[test]
fn legacy_configuration_bytes_still_verify_after_adding_the_field() {
    const LEGACY_DEFAULT_CONFIG_CBOR: &str = "ab6f6f776e65725f6d656d6265725f69640075636f6e6669677572\
6174696f6e5f76657273696f6e016c707269766163795f6d6f6465665075626c696367646973706c6179a2646e616d65a166\
5075626c6963a16576616c75659118441865186618611875186c187418201852186f186f186d1820184e1861186d18656b64\
6573637269707469\
6f6ef6736d61785f726563656e745f6d6573736167657318646d6d61785f757365725f62616e730a706d61785f6d65737361\
67655f73697a651903e8716d61785f6e69636b6e616d655f73697a6518326b6d61785f6d656d6265727318c86d6d61785f72\
6f6f6d5f6e616d651864746d61785f726f6f6d5f6465736372697074696f6e1901f4";

    let legacy_bytes: Vec<u8> = (0..LEGACY_DEFAULT_CONFIG_CBOR.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&LEGACY_DEFAULT_CONFIG_CBOR[i..i + 2], 16).expect("hex"))
        .collect();

    // 1. Old bytes still decode, and the absent field reads as `None`.
    let decoded: Configuration =
        ciborium::de::from_reader(&legacy_bytes[..]).expect("legacy configuration must decode");
    assert_eq!(
        decoded.max_direct_messages, None,
        "a configuration signed before the field existed must decode to None"
    );

    // 2. Re-encoding is byte-identical, which is what keeps the owner's
    //    signature valid. This is the assertion that fails if the field is ever
    //    changed to a plain `#[serde(default)] usize`.
    let mut re_encoded = Vec::new();
    ciborium::ser::into_writer(&decoded, &mut re_encoded).expect("re-encode");
    assert_eq!(
        re_encoded, legacy_bytes,
        "re-serializing a legacy configuration changed its bytes — every \
         existing room's owner signature would fail `verify`, which also gates \
         the #292 migration PUT. Any new `Configuration` field MUST be \
         `Option` + `skip_serializing_if`."
    );

    // 3. End-to-end: a signature made over the legacy bytes still verifies
    //    through the real API the contract calls.
    let owner_sk = SigningKey::from_bytes(&[3u8; 32]);
    let signature: Signature = {
        use ed25519_dalek::Signer;
        owner_sk.sign(&legacy_bytes)
    };
    let authorized = AuthorizedConfigurationV1::with_signature(decoded, signature);
    assert!(
        authorized
            .verify_signature(&owner_sk.verifying_key())
            .is_ok(),
        "an owner signature over pre-#519 configuration bytes must still verify"
    );

    // 4. And it must still be bounded despite carrying no explicit value.
    assert_eq!(
        authorized.configuration.effective_max_direct_messages(),
        DEFAULT_MAX_DIRECT_MESSAGES
    );
}

/// `verify` must NOT enforce the global cap.
///
/// The #292 migration PUT is gated on `validate_state` == `verify`. A legacy
/// room whose stored DM set is larger than the new cap — or any room whose
/// owner lowers the cap below the stored count — would be REJECTED outright if
/// `verify` enforced the bound, stranding the room on an unreachable contract
/// key with no recovery path. Retention is an `apply_delta` concern; `verify`
/// stays stable across configuration changes. Mirrors `MessagesV1`, which does
/// not enforce `max_recent_messages` in `verify` either.
#[test]
fn verify_accepts_state_over_the_global_cap_so_the_migration_put_survives() {
    let r = room(6, Some(5));

    // Build a large set under a slack cap, then present it to the room whose
    // configuration caps at 5 — exactly what a migration PUT of legacy state
    // looks like.
    let slack = r.state_with_cap(10_000);
    let big = dms_holding_under(&slack, &r.params, spread(&r, 0..60, 3));
    assert_eq!(big.messages.len(), 60, "test premise: state is over-cap");

    let state = ChatRoomStateV1 {
        direct_messages: big,
        ..r.state.clone()
    };
    assert!(
        state.direct_messages.verify(&state, &r.params).is_ok(),
        "verify rejected an over-cap DM set — this would reject the #292 \
         migration PUT and strand the room"
    );
    assert!(
        state.verify(&state, &r.params).is_ok(),
        "whole-state verify rejected an over-cap DM set"
    );
}

/// A zero cap is rejected at configuration-update time, alongside the other
/// zero-valued limits.
#[test]
fn zero_max_direct_messages_is_rejected_by_configuration_apply_delta() {
    let r = room(4, Some(40));
    let mut cfg = r.state.configuration.clone();

    let mut bad = cfg.configuration.clone();
    bad.configuration_version += 1;
    bad.max_direct_messages = Some(0);
    let bad = AuthorizedConfigurationV1::new(bad, &r.owner_sk);

    let err = cfg
        .apply_delta(&r.state, &r.params, &Some(bad))
        .expect_err("a zero DM cap must be rejected");
    assert_eq!(err, "Invalid configuration values");
}
