//! Property-based tests for the retention monoid laws on the two capped
//! collections in `ChatRoomStateV1`.
//!
//! # Why these exist
//!
//! `MessagesV1` and `DirectMessagesV1` REMOVE entries during `apply_delta`
//! (newest-`max_recent_messages`, newest-`MAX_DM_MESSAGES_PER_PAIR`-per-pair).
//! Pruning is correct and stays: "newest N by `(time, id)`" IS a valid join,
//! because everything it drops is older than everything it keeps and can never
//! re-enter. The state converges fine.
//!
//! What broke was the GOSSIP. `Summary` carried no prune horizon, so `delta`
//! was a pure "everything you don't have" set-difference: a peer offered
//! exactly the entries its neighbour discards on arrival, the neighbour's
//! summary was therefore unchanged, and the identical non-empty payload was
//! recomputed on every fan-out. `delta` never returned `None`, so
//! freenet-core's "empty delta -> skip" path never fired. On 2026-07-25 that
//! drove the room contract to 63.7% of all byte-weighted broadcast work
//! network-wide.
//!
//! So the load-bearing property here is **delta termination**, not state
//! convergence — and the retention invariant is deliberately phrased as an
//! EXACT equality against "the newest `min(|union|, cap)`", so a "fix" that
//! raises or removes the cap fails it rather than passing vacuously.
//!
//! `common/tests/retention_loop_test.rs` holds the hand-written regression
//! tests for the specific shapes seen in production; this file generalises
//! them over randomly-generated divergent windows.
//!
//! # Determinism
//!
//! Every property runs on a fixed-seed `TestRunner`, so the case sequence is
//! identical on every machine and a failure reproduces by re-running the test.
//! `failure_persistence` is therefore off — a `proptest-regressions/` file
//! would add an untracked build artifact for no extra reproducibility.

use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use proptest::prelude::*;
use proptest::test_runner::{RngAlgorithm, TestRng, TestRunner};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::direct_messages::{
    sign_direct_message, AuthorizedDirectMessage, DirectMessagesDelta, DirectMessagesV1,
    DmOrderKey, DmRetentionHorizon, MAX_DM_MESSAGES_PER_PAIR,
};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersV1};
use river_core::room_state::message::{
    AuthorizedMessageV1, MessageOrderKey, MessageV1, MessagesV1, RoomMessageBody,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Summary};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

// ---------------------------------------------------------------------------
// Shared fixture
// ---------------------------------------------------------------------------

/// Epoch offset so generated timestamps read as small integers in failure
/// output rather than raw clock values.
const BASE_SECS: u64 = 1_700_000_000;

/// Room-message pool size. Comfortably larger than the largest cap the
/// properties use, so "at capacity with divergent windows" — the shape that
/// looped — is reachable, not merely possible in principle.
const MSG_POOL: usize = 24;

/// Distinct timestamps in the message pool. Two messages share each timestamp,
/// so `MessageOrderKey`'s `id` tiebreak is exercised at the horizon rather than
/// only in the easy strictly-increasing case.
const MSG_PER_TIMESTAMP: usize = 2;

/// DM pool size per ordered pair. Must exceed `MAX_DM_MESSAGES_PER_PAIR` or a
/// pair can never reach the cap and every DM property passes vacuously.
const DM_POOL: usize = MAX_DM_MESSAGES_PER_PAIR + 20;

/// The largest `max_recent_messages` the properties generate. Kept below
/// `MSG_POOL` so most cases put at least one peer AT capacity.
const MAX_CAP: usize = 10;

struct Fixture {
    params: ChatRoomParametersV1,
    owner_sk: SigningKey,
    members: MembersV1,
    /// Signed room messages, ascending by `(time, id)` is NOT guaranteed —
    /// index order follows timestamp order, but ties within a timestamp are
    /// ordered by id, which is a hash. Callers must sort by `order_key()`.
    messages: Vec<AuthorizedMessageV1>,
    /// DMs for the ordered pair `(author -> peer)`, ascending by timestamp.
    dms_forward: Vec<AuthorizedDirectMessage>,
    /// DMs for the ordered pair `(peer -> author)`, ascending by timestamp.
    dms_reverse: Vec<AuthorizedDirectMessage>,
}

/// Signing is the expensive part of every case, so the pools are built exactly
/// once and the strategies select index subsets out of them.
fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(build_fixture)
}

fn build_fixture() -> Fixture {
    // FIXED seeds, not `SigningKey::generate(&mut OsRng)`.
    //
    // `MessageId` is a hash of the signature, so random keys make every
    // `MessageId` — and therefore the `(time, id)` tiebreak that
    // `MSG_PER_TIMESTAMP = 2` exists to exercise — different on every run. The
    // same goes for `MemberId`, which orders `DmPairHorizon`. With random keys
    // the module doc's "a failure reproduces by re-running" would be false: the
    // deterministic RNG would replay the same INDEX choices against different
    // underlying ids, and `failure_persistence: None` saves nothing to fall
    // back on. Matches `summary_determinism_test.rs`, which seeds the same way.
    let owner_sk = SigningKey::from_bytes(&[7u8; 32]);
    let owner_vk = owner_sk.verifying_key();
    let owner_id = MemberId::from(&owner_vk);

    let author_sk = SigningKey::from_bytes(&[11u8; 32]);
    let author_vk = author_sk.verifying_key();
    let author_id = MemberId::from(&author_vk);

    let peer_sk = SigningKey::from_bytes(&[23u8; 32]);
    let peer_vk = peer_sk.verifying_key();
    let peer_id = MemberId::from(&peer_vk);

    let members = MembersV1 {
        members: vec![
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
        ],
    };

    let messages = (0..MSG_POOL)
        .map(|i| {
            AuthorizedMessageV1::new(
                MessageV1 {
                    room_owner: owner_id,
                    author: author_id,
                    time: SystemTime::UNIX_EPOCH
                        + Duration::from_secs(BASE_SECS + (i / MSG_PER_TIMESTAMP) as u64),
                    content: RoomMessageBody::public(format!("message {i}")),
                },
                &author_sk,
            )
        })
        .collect();

    let dms_forward = (0..DM_POOL)
        .map(|i| {
            sign_direct_message(
                &author_sk,
                author_id,
                peer_id,
                &owner_vk,
                BASE_SECS + i as u64,
                vec![(i % 251) as u8; 8],
            )
            .expect("sign forward dm")
        })
        .collect();

    let dms_reverse = (0..DM_POOL)
        .map(|i| {
            sign_direct_message(
                &peer_sk,
                peer_id,
                author_id,
                &owner_vk,
                BASE_SECS + i as u64,
                vec![(i % 251) as u8; 8],
            )
            .expect("sign reverse dm")
        })
        .collect();

    Fixture {
        params: ChatRoomParametersV1 { owner: owner_vk },
        owner_sk,
        members,
        messages,
        dms_forward,
        dms_reverse,
    }
}

/// A parent state carrying `cap` as `max_recent_messages`. `MessagesV1` reads
/// its retention horizon out of this, and it is per-room config, so two peers
/// can legitimately hold different values.
fn parent_with_cap(cap: usize) -> ChatRoomStateV1 {
    let f = fixture();
    ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                max_recent_messages: cap,
                max_message_size: 1_000,
                max_members: 16,
                ..Default::default()
            },
            &f.owner_sk,
        ),
        members: f.members.clone(),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Test-runner harness
// ---------------------------------------------------------------------------

/// Shrinking budget, in wall-clock milliseconds.
///
/// A DM case merges states holding a few hundred signed messages, so a single
/// shrink step is not cheap. Left unbounded, a failing DM property shrinks for
/// well over ten minutes — measured, while mutation-testing this file — which
/// on CI reads as a hung job rather than a failed test. 30s is ample to shrink
/// these index-set generators to a readable counterexample; the cap only ever
/// costs a slightly larger one.
const MAX_SHRINK_MS: u32 = 30_000;

fn check<S>(cases: u32, strategy: S, test: impl Fn(S::Value) -> Result<(), TestCaseError>)
where
    S: Strategy,
    S::Value: std::fmt::Debug,
{
    let config = ProptestConfig {
        cases,
        max_shrink_time: MAX_SHRINK_MS,
        failure_persistence: None,
        ..ProptestConfig::default()
    };
    let mut runner =
        TestRunner::new_with_rng(config, TestRng::deterministic_rng(RngAlgorithm::ChaCha));
    if let Err(err) = runner.run(&strategy, test) {
        panic!("{err}");
    }
}

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

/// Index sets modelling how peers actually diverge.
///
/// A plain uniform subset is not enough: the production shape is two peers
/// whose retained windows OVERLAP but differ, one of them at capacity. So the
/// generator mixes contiguous stretches (a peer online for a while), stretches
/// with holes (windows that overlap but disagree), and unconstrained subsets
/// (adversarial). Empty sets are reachable in all three.
fn held_indices(pool: usize) -> BoxedStrategy<Vec<usize>> {
    prop_oneof![
        3 => (0..pool, 0..=pool).prop_map(move |(start, len)| {
            (start..(start + len).min(pool)).collect::<Vec<usize>>()
        }),
        3 => (0..pool, 0..=pool, prop::collection::vec(any::<bool>(), pool)).prop_map(
            move |(start, len, keep)| (start..(start + len).min(pool))
                .filter(|i| keep[*i])
                .collect::<Vec<usize>>()
        ),
        2 => prop::collection::btree_set(0..pool, 0..=pool)
            .prop_map(|s| s.into_iter().collect::<Vec<usize>>()),
    ]
    .boxed()
}

/// Index sets for a DM pair, biased so most cases put the pair AT
/// `MAX_DM_MESSAGES_PER_PAIR`.
///
/// The generic `held_indices` is wrong here: against a 120-entry pool it
/// reaches the 100-message cap in about 1% of cases (measured — that is what
/// `the_generator_reaches_at_capacity_peers_with_divergent_windows` is for), so
/// every DM property would pass without ever exercising the trim, the pair
/// horizon, or the loop they exist to catch.
fn dm_held_indices() -> BoxedStrategy<Vec<usize>> {
    let full = DM_POOL - MAX_DM_MESSAGES_PER_PAIR;
    prop_oneof![
        // A window that always reaches the cap: the pair the loop needs.
        6 => (0..=full, MAX_DM_MESSAGES_PER_PAIR..=DM_POOL).prop_map(move |(start, len)| {
            (start..(start + len).min(DM_POOL)).collect::<Vec<usize>>()
        }),
        // The same window with holes, so peers straddle the cap boundary in
        // both directions and their retained sets genuinely disagree.
        3 => (
            0..=full,
            MAX_DM_MESSAGES_PER_PAIR..=DM_POOL,
            prop::collection::vec(0..10u8, DM_POOL),
        )
            .prop_map(move |(start, len, roll)| (start..(start + len).min(DM_POOL))
                .filter(|i| roll[*i] > 0)
                .collect::<Vec<usize>>()),
        // Well below the cap: back-fill, and the empty state.
        2 => prop::collection::btree_set(0..DM_POOL, 0..=30)
            .prop_map(|s| s.into_iter().collect::<Vec<usize>>()),
    ]
    .boxed()
}

/// The same index set presented in two independently-shuffled orders. Used to
/// prove a summary's bytes do not depend on the order the state Vec happens to
/// be in — the `HashMap`/`HashSet`-in-a-summary regression that caused ~20M
/// spurious anti-entropy heals in freenet-core (#4857).
fn two_orders(indices: BoxedStrategy<Vec<usize>>) -> BoxedStrategy<(Vec<usize>, Vec<usize>)> {
    indices
        .prop_flat_map(|idx| (Just(idx.clone()).prop_shuffle(), Just(idx).prop_shuffle()))
        .boxed()
}

// ---------------------------------------------------------------------------
// Builders and oracles
// ---------------------------------------------------------------------------

/// `MessagesV1` holding the pool entries at `idx`, normalised through
/// `apply_delta` (sorted and pruned) exactly as a real peer's stored state is.
fn messages_from(parent: &ChatRoomStateV1, idx: &[usize]) -> MessagesV1 {
    let f = fixture();
    let mut m = MessagesV1::default();
    let delta: Vec<AuthorizedMessageV1> = idx.iter().map(|i| f.messages[*i].clone()).collect();
    m.apply_delta(parent, &f.params, &Some(delta))
        .expect("fixture messages must apply");
    m
}

fn message_keys(m: &MessagesV1) -> Vec<MessageOrderKey> {
    let mut keys: Vec<MessageOrderKey> = m.messages.iter().map(|m| m.order_key()).collect();
    keys.sort();
    keys
}

/// The oracle: the newest `min(|union|, cap)` entries by `(time, id)`.
///
/// Stated as an exact set, so it fails BOTH ways — a merge that keeps too
/// little (a horizon that over-states and withholds entries the receiver would
/// have kept) and a merge that keeps too much (a "fix" that raises or removes
/// the cap).
fn newest_message_keys(mut keys: Vec<MessageOrderKey>, cap: usize) -> Vec<MessageOrderKey> {
    keys.sort();
    keys.dedup();
    let cut = keys.len().saturating_sub(cap);
    keys.split_off(cut)
}

fn message_key_union(a: &MessagesV1, b: &MessagesV1) -> Vec<MessageOrderKey> {
    let mut keys = message_keys(a);
    keys.extend(message_keys(b));
    keys
}

/// `DirectMessagesV1` holding the pool entries at `fwd` (author -> peer) and
/// `rev` (peer -> author), normalised (trimmed to the per-pair cap and sorted)
/// exactly as a real peer's stored state is.
///
/// The messages are assembled directly and normalised with an EMPTY delta
/// rather than fed in as `new_messages`. `apply_delta`'s trim and sort still
/// run — those are what "normalised" means — but its per-message ed25519
/// verification is skipped for pool entries already known to be valid. That
/// verification dominates the runtime of an unoptimised test build and buys
/// nothing here: the merge UNDER TEST still drives the full
/// verify-and-accept path for everything the sender offers.
fn dms_from(parent: &ChatRoomStateV1, fwd: &[usize], rev: &[usize]) -> DirectMessagesV1 {
    let f = fixture();
    let mut messages: Vec<AuthorizedDirectMessage> =
        fwd.iter().map(|i| f.dms_forward[*i].clone()).collect();
    messages.extend(rev.iter().map(|i| f.dms_reverse[*i].clone()));

    let mut d = DirectMessagesV1 {
        messages,
        ..Default::default()
    };
    d.apply_delta(
        parent,
        &f.params,
        &Some(DirectMessagesDelta {
            new_messages: vec![],
            advanced_purges: vec![],
        }),
    )
    .expect("fixture dms must normalise");
    d
}

type PairKeys = BTreeMap<(MemberId, MemberId), Vec<DmOrderKey>>;

fn dm_keys(d: &DirectMessagesV1) -> PairKeys {
    let mut by_pair: PairKeys = BTreeMap::new();
    for m in &d.messages {
        by_pair
            .entry((m.message.sender, m.message.recipient))
            .or_default()
            .push(m.order_key());
    }
    for keys in by_pair.values_mut() {
        keys.sort();
    }
    by_pair
}

/// The DM oracle: per ordered pair, the newest
/// `min(|pair union|, MAX_DM_MESSAGES_PER_PAIR)` entries by
/// `(timestamp, signature)`.
fn newest_dm_keys(mut union: PairKeys) -> PairKeys {
    for keys in union.values_mut() {
        keys.sort();
        keys.dedup();
        let cut = keys.len().saturating_sub(MAX_DM_MESSAGES_PER_PAIR);
        *keys = keys.split_off(cut);
    }
    union.retain(|_, keys| !keys.is_empty());
    union
}

fn dm_key_union(a: &DirectMessagesV1, b: &DirectMessagesV1) -> PairKeys {
    let mut union = dm_keys(a);
    for (pair, keys) in dm_keys(b) {
        union.entry(pair).or_default().extend(keys);
    }
    union
}

// ---------------------------------------------------------------------------
// MessagesV1 — delta termination (the property the bug violated)
// ---------------------------------------------------------------------------

/// **THE** property. After a peer merges what a neighbour offered, the
/// neighbour must have nothing left to offer.
///
/// Before the fix this failed for every pair whose retained windows diverged
/// while the receiver sat at capacity: the receiver applied the payload, pruned
/// it straight back, and its summary was unchanged — so the sender, which
/// cannot see the rejection, recomputed the identical payload forever.
#[test]
fn merging_a_peer_leaves_it_with_nothing_further_to_offer() {
    let f = fixture();
    check(
        512,
        (
            held_indices(MSG_POOL),
            held_indices(MSG_POOL),
            0..=MAX_CAP,
            0..=MAX_CAP,
        ),
        |(idx_a, idx_b, cap_a, cap_b)| {
            let parent_a = parent_with_cap(cap_a);
            let parent_b = parent_with_cap(cap_b);

            let mut a = messages_from(&parent_a, &idx_a);
            let b = messages_from(&parent_b, &idx_b);

            // The receiver always merges against its OWN parent state — that is
            // what the contract and `merge_incoming_state` do, and it is why
            // the caps may differ.
            a.merge(&parent_a, &f.params, &b).expect("a.merge(b)");

            prop_assert_eq!(
                b.delta(&parent_a, &f.params, &a.summarize(&parent_a, &f.params)),
                None,
                "B still had a payload for A after A merged it — this is the resend loop"
            );
            Ok(())
        },
    );
}

/// Gossip idempotence: re-merging the SAME neighbour must be a no-op in both
/// state and traffic.
///
/// The state half held even with the bug (the prune is absorbing), so the
/// traffic half is the live pin — it is the exact assertion that failed.
#[test]
fn re_merging_the_same_peer_changes_neither_state_nor_traffic() {
    let f = fixture();
    check(
        256,
        (
            held_indices(MSG_POOL),
            held_indices(MSG_POOL),
            0..=MAX_CAP,
            0..=MAX_CAP,
        ),
        |(idx_a, idx_b, cap_a, cap_b)| {
            let parent_a = parent_with_cap(cap_a);
            let parent_b = parent_with_cap(cap_b);

            let mut once = messages_from(&parent_a, &idx_a);
            let b = messages_from(&parent_b, &idx_b);
            once.merge(&parent_a, &f.params, &b).expect("first merge");

            let mut twice = once.clone();
            twice.merge(&parent_a, &f.params, &b).expect("second merge");

            prop_assert_eq!(
                message_keys(&twice),
                message_keys(&once),
                "a second merge of the same peer changed the retained window"
            );
            prop_assert_eq!(
                b.delta(&parent_a, &f.params, &twice.summarize(&parent_a, &f.params)),
                None,
                "a second merge of the same peer still produced traffic"
            );
            Ok(())
        },
    );
}

/// Self-merge is the identity. Structural, and it passes against the
/// pre-fix code (a peer's summary always covers its own state, so the
/// set-difference is empty either way) — kept because it is the base case of
/// the monoid and would catch a `delta` that offered entries the receiver
/// already holds.
#[test]
fn merging_a_peer_with_itself_is_the_identity() {
    let f = fixture();
    check(256, (held_indices(MSG_POOL), 0..=MAX_CAP), |(idx, cap)| {
        let parent = parent_with_cap(cap);
        let original = messages_from(&parent, &idx);

        let mut merged = original.clone();
        let other = original.clone();
        merged
            .merge(&parent, &f.params, &other)
            .expect("self merge");

        prop_assert_eq!(
            message_keys(&merged),
            message_keys(&original),
            "merging a state with itself changed it"
        );
        Ok(())
    });
}

// ---------------------------------------------------------------------------
// MessagesV1 — the retention invariant
// ---------------------------------------------------------------------------

/// The merged result is EXACTLY the newest `min(|union|, max_recent_messages)`
/// entries by `(time, id)`.
///
/// Deliberately an exact equality in both directions:
///
/// * A "fix" that stops pruning, or raises the cap, keeps more than the newest
///   `cap` and FAILS. Pruning is correct and must stay.
/// * A horizon that OVER-states (withholding entries the receiver would have
///   kept — e.g. one published while below capacity) keeps less than the newest
///   `cap` and also FAILS.
#[test]
fn a_merge_retains_exactly_the_newest_cap_entries_of_the_union() {
    let f = fixture();
    check(
        512,
        (
            held_indices(MSG_POOL),
            held_indices(MSG_POOL),
            0..=MAX_CAP,
            0..=MAX_CAP,
        ),
        |(idx_a, idx_b, cap_a, cap_b)| {
            let parent_a = parent_with_cap(cap_a);
            let parent_b = parent_with_cap(cap_b);

            let mut a = messages_from(&parent_a, &idx_a);
            let b = messages_from(&parent_b, &idx_b);
            let expected = newest_message_keys(message_key_union(&a, &b), cap_a);

            a.merge(&parent_a, &f.params, &b).expect("a.merge(b)");

            prop_assert_eq!(
                message_keys(&a),
                expected,
                "the merge did not retain exactly the newest {} of the union",
                cap_a
            );
            Ok(())
        },
    );
}

/// Commutativity: absorbing two neighbours' states in either order lands on the
/// same window.
#[test]
fn absorbing_two_peers_is_commutative() {
    let f = fixture();
    check(
        256,
        (
            held_indices(MSG_POOL),
            held_indices(MSG_POOL),
            held_indices(MSG_POOL),
            1..=MAX_CAP,
        ),
        |(idx_base, idx_b, idx_c, cap)| {
            let parent = parent_with_cap(cap);
            let base = messages_from(&parent, &idx_base);
            let b = messages_from(&parent, &idx_b);
            let c = messages_from(&parent, &idx_c);

            let mut bc = base.clone();
            bc.merge(&parent, &f.params, &b).expect("merge b");
            bc.merge(&parent, &f.params, &c).expect("merge c");

            let mut cb = base.clone();
            cb.merge(&parent, &f.params, &c).expect("merge c");
            cb.merge(&parent, &f.params, &b).expect("merge b");

            prop_assert_eq!(
                message_keys(&bc),
                message_keys(&cb),
                "the retained window depended on which neighbour was merged first"
            );
            Ok(())
        },
    );
}

/// Associativity: pre-combining two neighbours before absorbing them is the
/// same as absorbing them one at a time. Together with commutativity and
/// idempotence this is the join-semilattice law set, restricted to a single
/// shared cap (with differing caps the results are each the newest-N of the
/// same union for their own N, which is the retention invariant above).
#[test]
fn absorbing_two_peers_is_associative() {
    let f = fixture();
    check(
        256,
        (
            held_indices(MSG_POOL),
            held_indices(MSG_POOL),
            held_indices(MSG_POOL),
            1..=MAX_CAP,
        ),
        |(idx_a, idx_b, idx_c, cap)| {
            let parent = parent_with_cap(cap);
            let a = messages_from(&parent, &idx_a);
            let b = messages_from(&parent, &idx_b);
            let c = messages_from(&parent, &idx_c);

            // (A . B) . C
            let mut left = a.clone();
            left.merge(&parent, &f.params, &b).expect("a.merge(b)");
            left.merge(&parent, &f.params, &c).expect("(ab).merge(c)");

            // A . (B . C)
            let mut bc = b.clone();
            bc.merge(&parent, &f.params, &c).expect("b.merge(c)");
            let mut right = a.clone();
            right.merge(&parent, &f.params, &bc).expect("a.merge(bc)");

            prop_assert_eq!(
                message_keys(&left),
                message_keys(&right),
                "grouping changed the retained window"
            );
            Ok(())
        },
    );
}

/// A zero cap is reachable: `AuthorizedConfigurationV1::apply_delta` rejects
/// it, but `verify` does not, so an owner-signed zero still arrives on the
/// full-state path. The peer must retain nothing AND be offered nothing —
/// suppressing the delta rather than looping against a peer that keeps nothing.
#[test]
fn a_zero_cap_peer_retains_nothing_and_is_offered_nothing() {
    let f = fixture();
    check(
        128,
        (held_indices(MSG_POOL), held_indices(MSG_POOL), 1..=MAX_CAP),
        |(idx_a, idx_b, cap_b)| {
            let parent_zero = parent_with_cap(0);
            let parent_b = parent_with_cap(cap_b);

            let mut a = messages_from(&parent_zero, &idx_a);
            let b = messages_from(&parent_b, &idx_b);

            prop_assert!(
                a.messages.is_empty(),
                "a peer with max_recent_messages = 0 must retain nothing"
            );
            prop_assert_eq!(
                b.delta(
                    &parent_zero,
                    &f.params,
                    &a.summarize(&parent_zero, &f.params)
                ),
                None,
                "a peer that retains nothing was still offered messages"
            );

            a.merge(&parent_zero, &f.params, &b).expect("merge");
            prop_assert!(
                a.messages.is_empty(),
                "a zero-cap peer retained messages after a merge"
            );
            Ok(())
        },
    );
}

// ---------------------------------------------------------------------------
// DirectMessagesV1
// ---------------------------------------------------------------------------

/// The DM analogue of the message loop. The per-pair cap used to be
/// first-come-wins, so two peers could sit at the cap holding different sets,
/// each re-offering what the other discards. At up to 32 KiB of ciphertext per
/// DM that is a far heavier loop per round than the message one.
#[test]
fn merging_a_dm_peer_leaves_it_with_nothing_further_to_offer() {
    let f = fixture();
    let parent = parent_with_cap(MAX_CAP);
    check(
        64,
        (
            dm_held_indices(),
            dm_held_indices(),
            dm_held_indices(),
            dm_held_indices(),
        ),
        |(fwd_a, rev_a, fwd_b, rev_b)| {
            let mut a = dms_from(&parent, &fwd_a, &rev_a);
            let b = dms_from(&parent, &fwd_b, &rev_b);

            a.merge(&parent, &f.params, &b).expect("a.merge(b)");

            prop_assert_eq!(
                b.delta(&parent, &f.params, &a.summarize(&parent, &f.params)),
                None,
                "B still had DMs for A after A merged them — the DM resend loop"
            );
            Ok(())
        },
    );
}

/// Per ordered `(sender, recipient)` pair, the merged result is EXACTLY the
/// newest `min(|pair union|, MAX_DM_MESSAGES_PER_PAIR)` by
/// `(timestamp, signature)`.
///
/// This is what makes the pair converge: newest-N is a pure function of the
/// union, whereas the old first-come-wins depended on arrival order. It also
/// pins the user-visible half of that bug — once a pair filled up, every later
/// DM from that sender used to be dropped forever.
#[test]
fn a_dm_merge_retains_exactly_the_newest_per_pair_cap() {
    let f = fixture();
    let parent = parent_with_cap(MAX_CAP);
    check(
        64,
        (
            dm_held_indices(),
            dm_held_indices(),
            dm_held_indices(),
            dm_held_indices(),
        ),
        |(fwd_a, rev_a, fwd_b, rev_b)| {
            let mut a = dms_from(&parent, &fwd_a, &rev_a);
            let b = dms_from(&parent, &fwd_b, &rev_b);
            let expected = newest_dm_keys(dm_key_union(&a, &b));

            a.merge(&parent, &f.params, &b).expect("a.merge(b)");

            prop_assert_eq!(
                dm_keys(&a),
                expected,
                "the DM merge did not retain exactly the newest {} per pair",
                MAX_DM_MESSAGES_PER_PAIR
            );
            Ok(())
        },
    );
}

/// Commutativity for DMs. This is the law first-come-wins broke outright: which
/// DMs a peer ended up holding depended on the order neighbours were merged in.
#[test]
fn absorbing_two_dm_peers_is_commutative() {
    let f = fixture();
    let parent = parent_with_cap(MAX_CAP);
    check(
        64,
        (
            dm_held_indices(),
            dm_held_indices(),
            dm_held_indices(),
            dm_held_indices(),
            dm_held_indices(),
            dm_held_indices(),
        ),
        |(fwd_base, rev_base, fwd_b, rev_b, fwd_c, rev_c)| {
            let base = dms_from(&parent, &fwd_base, &rev_base);
            let b = dms_from(&parent, &fwd_b, &rev_b);
            let c = dms_from(&parent, &fwd_c, &rev_c);

            let mut bc = base.clone();
            bc.merge(&parent, &f.params, &b).expect("merge b");
            bc.merge(&parent, &f.params, &c).expect("merge c");

            let mut cb = base.clone();
            cb.merge(&parent, &f.params, &c).expect("merge c");
            cb.merge(&parent, &f.params, &b).expect("merge b");

            prop_assert_eq!(
                dm_keys(&bc),
                dm_keys(&cb),
                "the retained DM set depended on which neighbour was merged first"
            );
            Ok(())
        },
    );
}

/// Associativity for DMs.
#[test]
fn absorbing_two_dm_peers_is_associative() {
    let f = fixture();
    let parent = parent_with_cap(MAX_CAP);
    check(
        64,
        (
            dm_held_indices(),
            dm_held_indices(),
            dm_held_indices(),
            dm_held_indices(),
            dm_held_indices(),
            dm_held_indices(),
        ),
        |(fwd_a, rev_a, fwd_b, rev_b, fwd_c, rev_c)| {
            let a = dms_from(&parent, &fwd_a, &rev_a);
            let b = dms_from(&parent, &fwd_b, &rev_b);
            let c = dms_from(&parent, &fwd_c, &rev_c);

            let mut left = a.clone();
            left.merge(&parent, &f.params, &b).expect("a.merge(b)");
            left.merge(&parent, &f.params, &c).expect("(ab).merge(c)");

            let mut bc = b.clone();
            bc.merge(&parent, &f.params, &c).expect("b.merge(c)");
            let mut right = a.clone();
            right.merge(&parent, &f.params, &bc).expect("a.merge(bc)");

            prop_assert_eq!(
                dm_keys(&left),
                dm_keys(&right),
                "grouping changed the retained DM set"
            );
            Ok(())
        },
    );
}

/// Self-merge is the identity for DMs too.
///
/// **STRUCTURALLY WEAK — it cannot fail for any mutation of the DM horizon, and
/// must not be counted as covering it.** A self-merge produces `delta == None`,
/// so the horizon FILTER in `delta` is not on this path at all.
///
/// (Since freenet/river#519 the `None` arm of `DirectMessagesV1::apply_delta`
/// does run the trims, via `enforce_caps_and_sort`, so they are technically on
/// this path — but only as a no-op, because the state was already normalised
/// when it was built. The property remains weak.)
///
/// Kept because it is the base case of the monoid and would catch a `delta`
/// that offered entries the receiver already holds. Labelled explicitly so it
/// is not mistaken for a retention pin; the load-bearing DM idempotence
/// property is `re_merging_the_same_dm_peer_changes_neither_state_nor_traffic`
/// below, which drives a REAL non-empty delta.
///
/// (Its messages-side twin, `merging_a_peer_with_itself_is_the_identity`, is
/// weak for the same reason and is labelled the same way.)
#[test]
fn merging_a_dm_peer_with_itself_is_the_identity() {
    let f = fixture();
    let parent = parent_with_cap(MAX_CAP);
    check(64, (dm_held_indices(), dm_held_indices()), |(fwd, rev)| {
        let original = dms_from(&parent, &fwd, &rev);
        let mut merged = original.clone();
        let other = original.clone();
        merged
            .merge(&parent, &f.params, &other)
            .expect("self merge");

        prop_assert_eq!(
            dm_keys(&merged),
            dm_keys(&original),
            "merging a DM state with itself changed it"
        );
        Ok(())
    });
}

/// Gossip idempotence for DMs: re-merging the SAME neighbour must change
/// neither state nor traffic.
///
/// This is the DM counterpart of
/// `re_merging_the_same_peer_changes_neither_state_nor_traffic`, and it exists
/// because the self-merge property above is structurally unable to reach the
/// trim or the pair horizon. Here the FIRST merge carries a real non-empty
/// delta, so `apply_delta` runs all the way through `trim_pairs_to_cap`, and
/// the second merge is the actual fixpoint assertion.
#[test]
fn re_merging_the_same_dm_peer_changes_neither_state_nor_traffic() {
    let f = fixture();
    let parent = parent_with_cap(MAX_CAP);
    check(
        64,
        (
            dm_held_indices(),
            dm_held_indices(),
            dm_held_indices(),
            dm_held_indices(),
        ),
        |(fwd_a, rev_a, fwd_b, rev_b)| {
            let mut once = dms_from(&parent, &fwd_a, &rev_a);
            let b = dms_from(&parent, &fwd_b, &rev_b);
            once.merge(&parent, &f.params, &b).expect("first merge");

            let mut twice = once.clone();
            twice.merge(&parent, &f.params, &b).expect("second merge");

            prop_assert_eq!(
                dm_keys(&twice),
                dm_keys(&once),
                "a second merge of the same DM peer changed the retained set"
            );
            prop_assert_eq!(
                b.delta(&parent, &f.params, &twice.summarize(&parent, &f.params)),
                None,
                "a second merge of the same DM peer still produced traffic"
            );
            Ok(())
        },
    );
}

// ---------------------------------------------------------------------------
// ChatRoomStateV1 — end to end, through the real client merge path
// ---------------------------------------------------------------------------

/// Build a full room state. `fwd` always includes the NEWEST forward DM so both
/// members stay DM participants and are therefore exempt from
/// `post_apply_cleanup`'s inactivity prune on BOTH peers — otherwise a peer
/// that happened to retain no messages would prune a member the other peer
/// keeps, and the resulting membership delta would mask the property under
/// test. The newest index is used because the oldest is what the per-pair cap
/// trims first.
fn room_state(cap: usize, msgs: &[usize], fwd: &[usize], rev: &[usize]) -> ChatRoomStateV1 {
    let mut anchored: Vec<usize> = fwd.to_vec();
    if !anchored.contains(&(DM_POOL - 1)) {
        anchored.push(DM_POOL - 1);
    }
    let parent = parent_with_cap(cap);
    ChatRoomStateV1 {
        recent_messages: messages_from(&parent, msgs),
        direct_messages: dms_from(&parent, &anchored, rev),
        ..parent
    }
}

/// The client-side merge, unrolled exactly as `room_synchronizer`'s
/// `merge_incoming_state` does it: the receiver summarises against its OWN
/// state (so `MessagesV1::summarize` reads the room's real cap), while
/// `apply_delta` keeps the cheap sentinel parent the `#[composable]` macro
/// ignores anyway.
fn merge_incoming_state(
    local: &mut ChatRoomStateV1,
    params: &ChatRoomParametersV1,
    incoming: &ChatRoomStateV1,
) -> Result<(), String> {
    let summary = local.summarize(local, params);
    let delta = incoming.delta(incoming, params, &summary);
    local.apply_delta(&ChatRoomStateV1::default(), params, &delta)
}

/// End-to-end delta termination on the whole room state, through the real
/// client merge path and across the CBOR round trip the contract performs.
///
/// The field-level properties above call `summarize`/`delta` in-process. The
/// deployed contract instead encodes `ChatRoomStateV1Summary` in
/// `summarize_state` and decodes it in `get_state_delta`, so a serde mistake in
/// `RetentionHorizon`, `MessageOrderKey` or `DmPairHorizon` (`SystemTime` being
/// the obvious candidate) would leave every in-process property green while the
/// deployed contract kept looping.
#[test]
fn whole_room_state_gossip_terminates_across_the_cbor_round_trip() {
    let f = fixture();
    check(
        64,
        (
            held_indices(MSG_POOL),
            held_indices(MSG_POOL),
            dm_held_indices(),
            dm_held_indices(),
            1..=MAX_CAP,
        ),
        |(msgs_a, msgs_b, dms_a, dms_b, cap)| {
            // One shared cap: `max_recent_messages` is per-room configuration,
            // so two peers of the SAME room normally agree on it, and a
            // differing one would emit a configuration delta that has nothing
            // to do with retention. Cross-cap behaviour is covered field-level
            // by `a_merge_retains_exactly_the_newest_cap_entries_of_the_union`.
            let mut a = room_state(cap, &msgs_a, &dms_a, &[]);
            let b = room_state(cap, &msgs_b, &dms_b, &[]);

            merge_incoming_state(&mut a, &f.params, &b).expect("merge incoming state");

            // Round-trip A's summary exactly as the contract does.
            let summary = a.summarize(&a, &f.params);
            let mut bytes = Vec::new();
            ciborium::ser::into_writer(&summary, &mut bytes).expect("encode summary");
            let decoded: ChatRoomStateV1Summary =
                ciborium::de::from_reader(bytes.as_slice()).expect("decode summary");
            prop_assert_eq!(
                &decoded,
                &summary,
                "the summary did not survive the CBOR round trip"
            );

            prop_assert_eq!(
                b.delta(&b, &f.params, &decoded),
                None,
                "B still had a payload for A after A merged it — the whole-state resend loop"
            );

            // freenet-core byte-compares these bytes to decide staleness, so
            // re-encoding the same state must reproduce them exactly.
            let mut again = Vec::new();
            ciborium::ser::into_writer(&a.summarize(&a, &f.params), &mut again)
                .expect("re-encode summary");
            prop_assert_eq!(bytes, again, "summary bytes were not stable across calls");

            // The exemption the fixture relies on must actually hold, or the
            // assertions above could pass because membership churn happened to
            // cancel out rather than because retention terminated.
            prop_assert_eq!(
                a.members.members.len(),
                f.members.members.len(),
                "post_apply_cleanup pruned a member — the fixture's DM anchor failed"
            );
            Ok(())
        },
    );
}

// ---------------------------------------------------------------------------
// DirectMessagesV1 — the GLOBAL cap (freenet/river#519)
// ---------------------------------------------------------------------------
//
// Every DM property above leaves `max_direct_messages` unset, so it runs at the
// DEFAULT (300) while `DM_POOL` allows at most 200 retained DMs across the two
// ordered pairs — the global trim can never fire there and the global horizon
// is permanently `Open`. These properties supply a cap that BITES, so the third
// retention rule gets the same generalised treatment as the other two.
//
// That headroom is now only 100 (200 held vs a 300 default), where it used to
// be 300. If `DEFAULT_MAX_DIRECT_MESSAGES` ever drops to 200 or below, the
// properties above silently START exercising the global trim against an oracle
// (`newest_dm_keys`) that models only the per-pair rule, and they will fail.
// That is the correct failure — fix them by threading the cap through, not by
// raising the pool.

/// The largest global DM cap these properties generate. Kept well below the
/// ~200 a peer can hold after the per-pair trim, so the global cap genuinely
/// binds rather than being slack.
const MAX_DM_CAP: usize = 60;

/// `parent_with_cap`, plus an explicit global DM cap.
fn parent_with_dm_cap(dm_cap: usize) -> ChatRoomStateV1 {
    let f = fixture();
    ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                max_recent_messages: MAX_CAP,
                max_message_size: 1_000,
                max_members: 16,
                max_direct_messages: Some(dm_cap),
                ..Default::default()
            },
            &f.owner_sk,
        ),
        members: f.members.clone(),
        ..Default::default()
    }
}

/// The globally-capped DM oracle: the per-pair oracle FIRST, then the newest
/// `dm_cap` of what survives — matching `enforce_caps_and_sort`'s pinned order.
/// Applying the global cut first would model a different (and worse) function;
/// `dm_global_cap_test::pair_trim_runs_before_the_global_trim_…` pins the order
/// itself.
fn newest_dm_keys_globally_capped(union: PairKeys, dm_cap: usize) -> PairKeys {
    let per_pair = newest_dm_keys(union);

    let mut all: Vec<(DmOrderKey, (MemberId, MemberId))> = per_pair
        .iter()
        .flat_map(|(pair, keys)| keys.iter().cloned().map(move |k| (k, *pair)))
        .collect();
    // Signatures are unique per DM, so ordering by key alone is already a
    // strict total order; the pair rides along only to rebuild the map.
    all.sort();
    let cut = all.len().saturating_sub(dm_cap);
    let kept = all.split_off(cut);

    let mut out: PairKeys = BTreeMap::new();
    for (key, pair) in kept {
        out.entry(pair).or_default().push(key);
    }
    for keys in out.values_mut() {
        keys.sort();
    }
    out
}

/// Retention: a merge keeps EXACTLY the newest `dm_cap` of the union, after the
/// per-pair trim. Exact equality, so a change that raises or drops the global
/// cap fails rather than passing vacuously.
#[test]
fn a_dm_merge_retains_exactly_the_newest_global_cap_of_the_union() {
    let f = fixture();
    check(
        64,
        (dm_held_indices(), dm_held_indices(), 1..=MAX_DM_CAP),
        |(fwd, rev, dm_cap)| {
            let parent = parent_with_dm_cap(dm_cap);
            let mut a = dms_from(&parent, &fwd, &[]);
            let b = dms_from(&parent, &[], &rev);

            let expected = newest_dm_keys_globally_capped(dm_key_union(&a, &b), dm_cap);

            let delta = b.delta(&parent, &f.params, &a.summarize(&parent, &f.params));
            a.apply_delta(&parent, &f.params, &delta)
                .expect("apply dm delta");

            prop_assert!(
                a.messages.len() <= dm_cap,
                "the merged state exceeds the global cap: {} > {}",
                a.messages.len(),
                dm_cap
            );
            prop_assert_eq!(
                dm_keys(&a),
                expected,
                "the merge did not retain exactly the newest {} of the union",
                dm_cap
            );
            Ok(())
        },
    );
}

/// Termination on the global axis: after one merge the sender has nothing
/// further to offer. This is the property the resend loop violated.
#[test]
fn merging_a_dm_peer_under_a_global_cap_leaves_it_with_nothing_further_to_offer() {
    let f = fixture();
    check(
        64,
        (dm_held_indices(), dm_held_indices(), 1..=MAX_DM_CAP),
        |(fwd, rev, dm_cap)| {
            let parent = parent_with_dm_cap(dm_cap);
            let mut a = dms_from(&parent, &fwd, &rev);
            let b = dms_from(&parent, &rev, &fwd);

            let delta = b.delta(&parent, &f.params, &a.summarize(&parent, &f.params));
            a.apply_delta(&parent, &f.params, &delta)
                .expect("apply dm delta");

            prop_assert_eq!(
                b.delta(&parent, &f.params, &a.summarize(&parent, &f.params)),
                None,
                "B still had a DM payload for A after A merged it — the resend loop, \
                 now on the global axis"
            );
            Ok(())
        },
    );
}

/// Absorbing two peers is commutative under the global cap — the strongest
/// available statement that the trim is a pure function of the union rather
/// than of arrival order.
#[test]
fn absorbing_two_dm_peers_under_a_global_cap_is_commutative() {
    let f = fixture();
    check(
        48,
        (
            dm_held_indices(),
            dm_held_indices(),
            dm_held_indices(),
            1..=MAX_DM_CAP,
        ),
        |(base, x, y, dm_cap)| {
            let parent = parent_with_dm_cap(dm_cap);
            let peer_x = dms_from(&parent, &x, &[]);
            let peer_y = dms_from(&parent, &[], &y);

            let absorb = |first: &DirectMessagesV1, second: &DirectMessagesV1| {
                let mut s = dms_from(&parent, &base, &[]);
                for peer in [first, second] {
                    let delta = peer.delta(&parent, &f.params, &s.summarize(&parent, &f.params));
                    s.apply_delta(&parent, &f.params, &delta)
                        .expect("apply dm delta");
                }
                s
            };

            prop_assert_eq!(
                absorb(&peer_x, &peer_y),
                absorb(&peer_y, &peer_x),
                "absorbing two peers in the opposite order produced a different state"
            );
            Ok(())
        },
    );
}

/// Anti-vacuity self-check, mirroring the one the other axes have: the
/// generators must actually REACH the global cap, or every property above is
/// decorative. Without this, raising `MAX_DM_CAP` past what the pools can
/// produce would silently turn the whole block into a no-op.
#[test]
fn the_generator_reaches_peers_at_the_global_dm_cap() {
    let at_cap = Cell::new(0usize);
    let horizon_published = Cell::new(0usize);
    let f = fixture();

    check(
        64,
        (dm_held_indices(), dm_held_indices(), 1..=MAX_DM_CAP),
        |(fwd, rev, dm_cap)| {
            let parent = parent_with_dm_cap(dm_cap);
            let d = dms_from(&parent, &fwd, &rev);
            if d.messages.len() >= dm_cap {
                at_cap.set(at_cap.get() + 1);
            }
            if matches!(
                d.summarize(&parent, &f.params).global_horizon,
                DmRetentionHorizon::OldestRetained(_)
            ) {
                horizon_published.set(horizon_published.get() + 1);
            }
            Ok(())
        },
    );

    assert!(
        at_cap.get() > 32,
        "only {} of 64 generated peers reached the global cap — the properties \
         above are largely vacuous",
        at_cap.get()
    );
    assert!(
        horizon_published.get() > 32,
        "only {} of 64 generated peers published a real global horizon — the \
         termination property above is largely vacuous",
        horizon_published.get()
    );
}

// ---------------------------------------------------------------------------
// Summary determinism
// ---------------------------------------------------------------------------

/// A summary's bytes must depend only on the logical state, never on the order
/// the state Vec happens to be in.
///
/// freenet-core byte-compares `summarize_state` output to decide staleness, so
/// a `HashMap`/`HashSet` (or an index-derived rather than order-derived
/// horizon) makes two identical peers look perpetually out of sync. That
/// produced ~20M spurious anti-entropy heals in freenet-core#4857.
///
/// `verify` does not require `messages` to be sorted, so an unsorted state is
/// reachable via a hand-built or hostile full-state PUT — this builds the state
/// directly rather than through `apply_delta` for exactly that reason.
#[test]
fn message_summary_bytes_are_independent_of_state_vec_order() {
    let f = fixture();
    let parent = parent_with_cap(MAX_CAP);
    check(
        128,
        two_orders(held_indices(MSG_POOL)),
        |(order_one, order_two)| {
            let build = |idx: &[usize]| MessagesV1 {
                messages: idx.iter().map(|i| f.messages[*i].clone()).collect(),
                ..Default::default()
            };

            let one = build(&order_one).summarize(&parent, &f.params);
            let two = build(&order_two).summarize(&parent, &f.params);
            prop_assert_eq!(&one, &two, "the same logical state summarised differently");

            let mut bytes_one = Vec::new();
            ciborium::ser::into_writer(&one, &mut bytes_one).expect("encode");
            let mut bytes_two = Vec::new();
            ciborium::ser::into_writer(&two, &mut bytes_two).expect("encode");
            prop_assert_eq!(
                bytes_one,
                bytes_two,
                "the same logical state produced different summary BYTES"
            );
            Ok(())
        },
    );
}

/// The DM counterpart. `pair_horizons` is built through a `HashMap` internally,
/// so it must be sorted before it leaves `summarize` — this is what catches a
/// regression that drops that sort.
///
/// BOTH ordered pairs are loaded, and both reach the cap, or the property is
/// vacuous: `pair_horizons` only emits an entry for an at-capacity pair, and a
/// one-entry `Vec` cannot expose a map-iteration-order bug.
#[test]
fn dm_summary_bytes_are_independent_of_state_vec_order() {
    let f = fixture();
    let parent = parent_with_cap(MAX_CAP);
    check(
        64,
        two_orders(dm_held_indices()),
        |(order_one, order_two)| {
            let build = |idx: &[usize]| {
                let mut messages: Vec<AuthorizedDirectMessage> =
                    idx.iter().map(|i| f.dms_forward[*i].clone()).collect();
                messages.extend(idx.iter().map(|i| f.dms_reverse[*i].clone()));
                DirectMessagesV1 {
                    messages,
                    ..Default::default()
                }
            };

            let one = build(&order_one).summarize(&parent, &f.params);
            let two = build(&order_two).summarize(&parent, &f.params);
            prop_assert_eq!(
                &one,
                &two,
                "the same logical DM state summarised differently"
            );

            let mut bytes_one = Vec::new();
            ciborium::ser::into_writer(&one, &mut bytes_one).expect("encode");
            let mut bytes_two = Vec::new();
            ciborium::ser::into_writer(&two, &mut bytes_two).expect("encode");
            prop_assert_eq!(
                bytes_one,
                bytes_two,
                "the same logical DM state produced different summary BYTES"
            );
            Ok(())
        },
    );
}

// ---------------------------------------------------------------------------
// Generator self-checks
// ---------------------------------------------------------------------------

/// A property that never reaches the failing shape is decorative. The bug needs
/// a receiver AT capacity whose retained window DIVERGES from the sender's, so
/// this asserts the generator actually produces that — often enough to matter,
/// not merely in principle.
#[test]
fn the_generator_reaches_at_capacity_peers_with_divergent_windows() {
    let mut runner = TestRunner::new_with_rng(
        ProptestConfig {
            cases: 512,
            failure_persistence: None,
            ..ProptestConfig::default()
        },
        TestRng::deterministic_rng(RngAlgorithm::ChaCha),
    );

    // `TestRunner::run` takes an `Fn`, so the tallies live behind `Cell`.
    let divergent_at_capacity = Cell::new(0usize);
    let dm_pair_at_capacity = Cell::new(0usize);
    let both_pairs_publish_a_horizon = Cell::new(0usize);
    let strategy = (
        held_indices(MSG_POOL),
        held_indices(MSG_POOL),
        1..=MAX_CAP,
        dm_held_indices(),
    );

    runner
        .run(&strategy, |(idx_a, idx_b, cap, dm_idx)| {
            let parent = parent_with_cap(cap);
            let a = messages_from(&parent, &idx_a);
            let b = messages_from(&parent, &idx_b);

            let a_keys = message_keys(&a);
            let b_keys = message_keys(&b);
            // "Divergent" in the sense that matters: B holds something A lacks
            // AND A is full, so A must prune to take anything on.
            if a_keys.len() == cap && b_keys.iter().any(|k| !a_keys.contains(k)) {
                divergent_at_capacity.set(divergent_at_capacity.get() + 1);
            }

            let dms = dms_from(&parent, &dm_idx, &[]);
            if dms.messages.len() >= MAX_DM_MESSAGES_PER_PAIR {
                dm_pair_at_capacity.set(dm_pair_at_capacity.get() + 1);
            }

            // `dm_summary_bytes_are_independent_of_state_vec_order` can only
            // expose a map-iteration-order bug when BOTH ordered pairs publish
            // a horizon — a one-entry `Vec` has no order to get wrong.
            let both_pairs = dms_from(&parent, &dm_idx, &dm_idx);
            if both_pairs.pair_horizons().len() == 2 {
                both_pairs_publish_a_horizon.set(both_pairs_publish_a_horizon.get() + 1);
            }
            Ok(())
        })
        .expect("self-check strategy must not fail");

    let divergent_at_capacity = divergent_at_capacity.get();
    let dm_pair_at_capacity = dm_pair_at_capacity.get();
    let both_pairs_publish_a_horizon = both_pairs_publish_a_horizon.get();
    assert!(
        divergent_at_capacity >= 50,
        "only {divergent_at_capacity}/512 cases put an at-capacity peer against a \
         divergent window — the message properties would be mostly decorative"
    );
    assert!(
        dm_pair_at_capacity >= 50,
        "only {dm_pair_at_capacity}/512 cases filled a DM pair to \
         MAX_DM_MESSAGES_PER_PAIR — the DM properties would be mostly decorative"
    );
    assert!(
        both_pairs_publish_a_horizon >= 50,
        "only {both_pairs_publish_a_horizon}/512 cases had BOTH ordered pairs publish a \
         horizon — dm_summary_bytes_are_independent_of_state_vec_order could not \
         expose a map-iteration-order bug"
    );
}
