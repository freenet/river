//! Regression tests for the capped-collection prune/resend loop.
//!
//! # The bug
//!
//! `ComposableState` convergence assumes `merge` is a join: state grows
//! monotonically toward a least upper bound, so `delta` eventually returns
//! `None` and the fan-out stops. Two collections in `ChatRoomStateV1` break
//! that assumption by REMOVING entries during `apply_delta`:
//!
//! * `MessagesV1` drops the oldest messages over `max_recent_messages`.
//! * `DirectMessagesV1` caps each ordered `(sender, recipient)` pair at
//!   `MAX_DM_MESSAGES_PER_PAIR`.
//!
//! Before the fix, `delta` was a pure "everything you don't have"
//! set-difference, so a peer whose retained window differed from its
//! neighbour's re-offered exactly the entries the neighbour discards, forever.
//! `delta` never returned `None`, the "empty delta -> skip" path in
//! freenet-core's broadcast never fired, and the pair burned bandwidth in a
//! fixed loop. On 2026-07-25 this made the Freenet Official room 63.7% of all
//! byte-weighted broadcast work network-wide (1.2% the day before), with peers
//! reporting tens of GB per hour.
//!
//! # The property these tests pin
//!
//! After one merge, the sender's next `delta` for the same receiver must be
//! `None` — never the same payload twice against an unchanged receiver.

use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use rand::rngs::OsRng;
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::direct_messages::{
    sign_direct_message, AuthorizedDirectMessage, DirectMessagesDelta, DirectMessagesV1,
    MAX_DM_MESSAGES_PER_PAIR,
};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersV1};
use river_core::room_state::message::{
    AuthorizedMessageV1, MessageV1, MessagesV1, RetentionHorizon, RoomMessageBody,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1};
use std::time::{Duration, SystemTime};

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// Epoch offset for message timestamps, so the small integers the tests pass
/// around read as "message 10", "message 11" rather than raw clock values.
const BASE_SECS: u64 = 1_700_000_000;

struct Room {
    params: ChatRoomParametersV1,
    state: ChatRoomStateV1,
    author_sk: SigningKey,
    author_id: MemberId,
    owner_id: MemberId,
    peer_id: MemberId,
}

/// A room with an owner, one message author and one other member, configured
/// with the given `max_recent_messages`.
fn room(max_recent_messages: usize) -> Room {
    let owner_sk = SigningKey::generate(&mut OsRng);
    let owner_vk = owner_sk.verifying_key();
    let owner_id = MemberId::from(&owner_vk);

    let author_sk = SigningKey::generate(&mut OsRng);
    let author_vk = author_sk.verifying_key();
    let author_id = MemberId::from(&author_vk);

    let peer_sk = SigningKey::generate(&mut OsRng);
    let peer_vk = peer_sk.verifying_key();
    let peer_id = MemberId::from(&peer_vk);

    let members = vec![
        AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: author_vk,
            },
            &owner_sk,
        ),
        AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: peer_vk,
            },
            &owner_sk,
        ),
    ];

    let config = Configuration {
        max_recent_messages,
        max_message_size: 1000,
        max_members: 10,
        ..Default::default()
    };

    Room {
        params: ChatRoomParametersV1 { owner: owner_vk },
        state: ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(config, &owner_sk),
            members: MembersV1 { members },
            ..Default::default()
        },
        author_sk,
        author_id,
        owner_id,
        peer_id,
    }
}

/// A signed message at `BASE_SECS + secs`, with `secs` embedded in the body so
/// every message is distinct.
fn msg_at(r: &Room, secs: u64) -> AuthorizedMessageV1 {
    AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: r.owner_id,
            author: r.author_id,
            time: SystemTime::UNIX_EPOCH + Duration::from_secs(BASE_SECS + secs),
            content: RoomMessageBody::public(format!("message {secs}")),
        },
        &r.author_sk,
    )
}

/// `MessagesV1` holding exactly the messages at the given offsets, normalised
/// through `apply_delta` (sorted and pruned) the way a real peer's stored state
/// always is.
fn messages_holding(r: &Room, offsets: impl IntoIterator<Item = u64>) -> MessagesV1 {
    let mut m = MessagesV1::default();
    let delta: Vec<AuthorizedMessageV1> = offsets.into_iter().map(|s| msg_at(r, s)).collect();
    m.apply_delta(&r.state, &r.params, &Some(delta))
        .expect("fixture messages must apply");
    m
}

/// The offsets a `MessagesV1` currently retains, ascending.
fn held(m: &MessagesV1) -> Vec<u64> {
    let mut v: Vec<u64> = m
        .messages
        .iter()
        .map(|m| {
            m.message
                .time
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("message time is after the epoch")
                .as_secs()
                - BASE_SECS
        })
        .collect();
    v.sort_unstable();
    v
}

// ---------------------------------------------------------------------------
// MessagesV1
// ---------------------------------------------------------------------------

/// THE regression test for the incident.
///
/// Peer A holds the newest window and is at capacity. Peer B holds an
/// overlapping-but-older window. Everything B has that A lacks is older than
/// anything A retains, so A would discard all of it — B must offer nothing.
///
/// Before the fix this produced a 5-message delta that A applied and
/// immediately pruned back to its starting state, so the same 5 were re-sent on
/// every subsequent fan-out, forever.
#[test]
fn sender_does_not_offer_messages_the_receiver_would_prune() {
    let r = room(10);

    // A: 10..19 (at the cap of 10).  B: 5..14 (older window, same cap).
    let a = messages_holding(&r, 10..20);
    let b = messages_holding(&r, 5..15);

    let a_summary = a.summarize(&r.state, &r.params);
    assert_eq!(
        a_summary.horizon,
        RetentionHorizon::OldestRetained(a.messages[0].order_key()),
        "test premise: A is at capacity so it must publish a real horizon"
    );

    // Premise check: B really does hold messages A lacks, all of them older
    // than A's oldest. Without this the assertion below would pass vacuously.
    let a_held = held(&a);
    let unseen: Vec<u64> = held(&b)
        .into_iter()
        .filter(|s| !a_held.contains(s))
        .collect();
    assert_eq!(
        unseen,
        vec![5, 6, 7, 8, 9],
        "test premise: B must hold messages A lacks"
    );

    assert_eq!(
        b.delta(&r.state, &r.params, &a_summary),
        None,
        "B offered messages that A would discard on arrival — this is the resend loop"
    );
}

/// The loop detector.
///
/// Models the shape that actually persisted in production, which is
/// ASYMMETRIC: peer B lags, and in a quiet room it never receives anything
/// newer, so its old window never ages out. It pushes to A on every fan-out; A
/// discards the payload and its summary is unchanged, so B — which cannot see
/// the rejection — computes the identical payload again next round, forever.
///
/// A bidirectional exchange would heal this pair on its own (B learns A's newer
/// messages and prunes its old window away), which is why
/// `offset_windows_converge_to_the_newest_messages` below is a CONVERGENCE test
/// and not a loop detector. This one drives only the direction that loops.
#[test]
fn gossip_from_a_lagging_peer_terminates() {
    let r = room(10);
    let mut a = messages_holding(&r, 10..20);
    let b = messages_holding(&r, 5..15);

    for round in 0..10 {
        let offered = b.delta(&r.state, &r.params, &a.summarize(&r.state, &r.params));
        let Some(offered) = offered else {
            return;
        };
        a.apply_delta(&r.state, &r.params, &Some(offered))
            .unwrap_or_else(|e| panic!("round {round} apply failed: {e}"));
    }
    panic!(
        "B was still offering payloads to A after 10 rounds — B never learns A \
         discards them, so this is the unbounded resend loop"
    );
}

/// Convergence, as distinct from termination: a full bidirectional exchange
/// must leave both peers holding the newest window.
///
/// NOT a loop detector — it passes with or without the retention horizon,
/// because the second direction teaches the lagging peer the newer messages and
/// its stale window prunes itself away. Kept because "the windows agree
/// afterwards" is a real property, and stated explicitly so nobody mistakes it
/// for the regression pin.
#[test]
fn offset_windows_converge_to_the_newest_messages() {
    let r = room(10);
    let mut a = messages_holding(&r, 10..20);
    let mut b = messages_holding(&r, 5..15);

    a.merge(&r.state, &r.params, &b).expect("a.merge(b)");
    b.merge(&r.state, &r.params, &a).expect("b.merge(a)");

    assert_eq!(
        held(&a),
        held(&b),
        "both peers must converge on the same window"
    );
    assert_eq!(
        held(&a),
        (10..20).collect::<Vec<_>>(),
        "the converged window is the newest `max_recent_messages`"
    );
}

/// The displacement case, which the horizon has to get right: B offers a
/// message NEWER than anything A holds, so A accepts it and drops its own
/// oldest. A's horizon must move up with it, or B immediately re-offers the
/// message A just dropped and the loop restarts one message further along.
#[test]
fn horizon_advances_when_an_accepted_message_displaces_the_oldest() {
    let r = room(5);
    let a = messages_holding(&r, [10, 11, 12, 13, 14]);
    // B overlaps A but also holds one strictly older message (8) and, below,
    // gains one strictly newer (20).
    let b = messages_holding(&r, [8, 10, 11, 12, 13]);

    let mut a_after = a.clone();
    a_after.merge(&r.state, &r.params, &b).expect("a.merge(b)");
    assert_eq!(
        held(&a_after),
        held(&a),
        "B's only unseen message (8) is below A's horizon, so nothing changes"
    );

    let mut b2 = b.clone();
    b2.apply_delta(&r.state, &r.params, &Some(vec![msg_at(&r, 20)]))
        .expect("b2 gains a newer message");

    let mut a2 = a.clone();
    a2.merge(&r.state, &r.params, &b2).expect("a.merge(b2)");
    assert_eq!(
        held(&a2),
        vec![11, 12, 13, 14, 20],
        "A accepts the newer message and drops its own oldest"
    );

    assert_eq!(
        b2.delta(&r.state, &r.params, &a2.summarize(&r.state, &r.params)),
        None,
        "after displacing A's oldest, B must not re-offer the message A just dropped"
    );
}

/// A peer BELOW its cap must still be offered older messages. The horizon must
/// not become a blanket "never send anything old" rule, or a peer that joined
/// late would never back-fill.
#[test]
fn a_peer_below_capacity_still_receives_older_messages() {
    let r = room(10);
    let mut late_joiner = messages_holding(&r, [18, 19]);
    let established = messages_holding(&r, 10..20);

    assert_eq!(
        late_joiner.summarize(&r.state, &r.params).horizon,
        RetentionHorizon::Open,
        "test premise: a peer holding 2 of 10 must advertise an open horizon"
    );

    late_joiner
        .merge(&r.state, &r.params, &established)
        .expect("merge");

    assert_eq!(
        held(&late_joiner),
        (10..20).collect::<Vec<_>>(),
        "a peer with spare capacity must back-fill the full window"
    );
}

/// Applying the same delta twice must leave the state untouched the second
/// time. This holds structurally (the prune keeps the newest N of a superset)
/// but it is the property the whole design rests on, so it is pinned directly.
#[test]
fn apply_delta_is_idempotent() {
    let r = room(5);
    let mut m = messages_holding(&r, [10, 11, 12]);
    let delta: Vec<AuthorizedMessageV1> =
        [8, 9, 13, 14, 15].iter().map(|s| msg_at(&r, *s)).collect();

    m.apply_delta(&r.state, &r.params, &Some(delta.clone()))
        .expect("first apply");
    let once = m.clone();
    m.apply_delta(&r.state, &r.params, &Some(delta))
        .expect("second apply");

    assert_eq!(
        held(&m),
        held(&once),
        "re-applying an identical delta must not change the retained window"
    );
}

/// `AuthorizedConfigurationV1::apply_delta` rejects a zero cap, but `verify`
/// does not, so a zero can still reach a peer on the full-state path. It must
/// suppress the delta rather than panic or loop.
#[test]
fn zero_cap_suppresses_the_delta_instead_of_looping() {
    let r = room(0);

    // The sender's own messages are built under a normal cap; only the
    // RECEIVER's configuration is zero here.
    let normal = room(10);
    let sender = messages_holding(&normal, 10..15);

    let empty = MessagesV1::default();
    let summary = empty.summarize(&r.state, &r.params);
    assert_eq!(summary.horizon, RetentionHorizon::Closed);
    assert_eq!(
        sender.delta(&r.state, &r.params, &summary),
        None,
        "a peer that retains nothing must not be offered anything"
    );

    // Summarizing a non-empty state under a zero cap must not panic.
    let nonempty = MessagesV1 {
        messages: (10..15).map(|s| msg_at(&r, s)).collect(),
        ..Default::default()
    };
    assert_eq!(
        nonempty.summarize(&r.state, &r.params).horizon,
        RetentionHorizon::Closed
    );
}

/// The horizon has to survive the wire, not just an in-process call.
///
/// On the network the summary makes a round trip through ciborium — the room
/// contract's `summarize_state` encodes `ChatRoomStateV1Summary`, and
/// `get_state_delta` decodes the peer's copy before calling `delta`. A serde
/// mistake in [`RetentionHorizon`] or `MessageOrderKey` (`SystemTime` is the
/// obvious candidate) would leave every in-memory test passing while the
/// deployed contract kept looping.
#[test]
fn horizon_survives_the_cbor_round_trip_the_contract_does() {
    let r = room(10);

    let mut holder = r.state.clone();
    holder.recent_messages = messages_holding(&r, 10..20);
    let mut lagging = r.state.clone();
    lagging.recent_messages = messages_holding(&r, 5..15);

    // Exactly what `summarize_state` emits and `get_state_delta` consumes.
    let summary = holder.summarize(&holder, &r.params);
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&summary, &mut bytes).expect("encode summary");
    let decoded: river_core::room_state::ChatRoomStateV1Summary =
        ciborium::de::from_reader(bytes.as_slice()).expect("decode summary");

    assert_eq!(
        decoded.recent_messages, summary.recent_messages,
        "the messages summary must survive the round trip unchanged"
    );
    assert!(
        lagging
            .delta(&lagging, &r.params, &decoded)
            .and_then(|d| d.recent_messages)
            .is_none(),
        "after a CBOR round trip the horizon must still suppress the messages delta"
    );

    // Byte-stability matters too: freenet-core compares these bytes to decide
    // staleness, so re-encoding the same state must reproduce them exactly.
    let mut again = Vec::new();
    ciborium::ser::into_writer(&holder.summarize(&holder, &r.params), &mut again)
        .expect("re-encode summary");
    assert_eq!(bytes, again, "summary bytes must be stable across calls");
}

// ---------------------------------------------------------------------------
// DirectMessagesV1
// ---------------------------------------------------------------------------

fn dm_at(r: &Room, timestamp: u64, tag: u8) -> AuthorizedDirectMessage {
    sign_direct_message(
        &r.author_sk,
        r.author_id,
        r.peer_id,
        &r.params.owner,
        timestamp,
        vec![tag; 8],
    )
    .expect("sign dm")
}

/// `DirectMessagesV1` holding the given DMs, normalised through `apply_delta`.
fn dms_holding(r: &Room, msgs: Vec<AuthorizedDirectMessage>) -> DirectMessagesV1 {
    let mut d = DirectMessagesV1::default();
    d.apply_delta(
        &r.state,
        &r.params,
        &Some(DirectMessagesDelta {
            new_messages: msgs,
            advanced_purges: vec![],
        }),
    )
    .expect("fixture dms must apply");
    d
}

/// The DM analogue of the message loop. The per-pair cap used to be
/// first-come-wins: a peer already holding `MAX_DM_MESSAGES_PER_PAIR` messages
/// for a pair silently dropped every incoming one, while the sender's `delta`
/// (a plain signature-set difference) re-offered them on every fan-out. With DM
/// ciphertexts capped at 32 KiB that is a far heavier loop per round than the
/// message one.
#[test]
fn sender_does_not_offer_dms_the_receiver_would_drop() {
    let r = room(100);

    // A holds the newest cap-full window; B holds an older, overlapping one.
    let a = dms_holding(
        &r,
        (0..MAX_DM_MESSAGES_PER_PAIR)
            .map(|i| dm_at(&r, 1_000 + i as u64, i as u8))
            .collect(),
    );
    let b = dms_holding(
        &r,
        (0..MAX_DM_MESSAGES_PER_PAIR)
            .map(|i| dm_at(&r, 900 + i as u64, i as u8))
            .collect(),
    );

    assert_eq!(
        a.messages.len(),
        MAX_DM_MESSAGES_PER_PAIR,
        "test premise: A must be exactly at the per-pair cap"
    );
    let a_sigs: Vec<_> = a.messages.iter().map(|m| m.sender_signature).collect();
    assert!(
        b.messages
            .iter()
            .any(|m| !a_sigs.contains(&m.sender_signature)),
        "test premise: B must hold DMs A lacks"
    );

    assert_eq!(
        b.delta(&r.state, &r.params, &a.summarize(&r.state, &r.params)),
        None,
        "B offered DMs that A's per-pair cap would discard — the DM resend loop"
    );
}

/// The per-pair cap must retain the NEWEST messages, not the first ones seen.
/// First-come-wins was both the loop's cause and a user-visible bug on its own:
/// once a pair filled up, every later DM from that sender was silently dropped.
#[test]
fn dm_pair_cap_keeps_the_newest_messages() {
    let r = room(100);
    let mut d = dms_holding(
        &r,
        (0..MAX_DM_MESSAGES_PER_PAIR)
            .map(|i| dm_at(&r, 1_000 + i as u64, 1))
            .collect(),
    );

    let newcomer = dm_at(&r, 9_999, 2);
    d.apply_delta(
        &r.state,
        &r.params,
        &Some(DirectMessagesDelta {
            new_messages: vec![newcomer.clone()],
            advanced_purges: vec![],
        }),
    )
    .expect("apply");

    assert_eq!(d.messages.len(), MAX_DM_MESSAGES_PER_PAIR);
    assert!(
        d.messages.contains(&newcomer),
        "a newer DM must displace the oldest, not be dropped on arrival"
    );
    assert!(
        !d.messages.iter().any(|m| m.message.timestamp == 1_000),
        "the oldest DM in the pair is the one that goes"
    );
}

/// The DM loop detector, matching `gossip_from_a_lagging_peer_terminates`:
/// only the lagging peer pushes, because that is the direction that used to
/// repeat forever. (A bidirectional exchange heals the pair on its own, so it
/// would pass with or without the pair horizon.)
#[test]
fn dm_gossip_from_a_lagging_peer_terminates() {
    let r = room(100);
    let mut a = dms_holding(
        &r,
        (0..MAX_DM_MESSAGES_PER_PAIR)
            .map(|i| dm_at(&r, 1_000 + i as u64, i as u8))
            .collect(),
    );
    let b = dms_holding(
        &r,
        (0..MAX_DM_MESSAGES_PER_PAIR)
            .map(|i| dm_at(&r, 950 + i as u64, i as u8))
            .collect(),
    );

    for round in 0..10 {
        let offered = b.delta(&r.state, &r.params, &a.summarize(&r.state, &r.params));
        let Some(offered) = offered else {
            return;
        };
        a.apply_delta(
            &r.state,
            &r.params,
            &Some(DirectMessagesDelta {
                new_messages: offered.new_messages,
                advanced_purges: offered.advanced_purges,
            }),
        )
        .unwrap_or_else(|e| panic!("round {round} apply failed: {e}"));
    }
    panic!(
        "B was still offering DMs to A after 10 rounds — B never learns A's \
         per-pair cap discards them, so this is the unbounded resend loop"
    );
}

/// A pair BELOW the cap must still receive older DMs.
#[test]
fn dm_pair_below_cap_still_receives_older_messages() {
    let r = room(100);
    let mut receiver = dms_holding(&r, vec![dm_at(&r, 5_000, 1)]);
    let sender = dms_holding(
        &r,
        (0..5)
            .map(|i| dm_at(&r, 1_000 + i as u64, i as u8))
            .collect(),
    );

    receiver.merge(&r.state, &r.params, &sender).expect("merge");
    assert_eq!(
        receiver.messages.len(),
        6,
        "a pair with spare capacity must back-fill older DMs"
    );
}
