---
description: Merge-law rules for the room contract — required reading for any change under common/src/room_state/ or contracts/room-contract/
globs:
  - common/src/room_state/**/*.rs
  - common/src/room_state.rs
  - contracts/room-contract/src/**/*.rs
---

# Room-contract merge laws (freenet/river#703)

freenet-core checks contracts against merge laws (state commutativity,
associativity, idempotence, delta idempotence). A room merge that breaks one
forks rooms between peers. These rules hold the room contract to them.

## A validly signed record never fails a merge

`apply_delta` returns `Err` only for a forgery (a record whose signature no
key it could be checked against verifies) or a work bound exceeded. A record
that is validly signed but that the room does not admit (over a cap, at an
unknown secret version, a public message in a private room, a second record
for a member id this peer already holds) is SKIPPED on arrival and dropped
where held, by the same rule in both places. So is a ban whose known
issuer's key does not verify it: another peer may not know that issuer. An
`Err` for such a record fails every merge with a peer holding it, in one
direction only, and nothing but its author can repair it.

A configuration that fails the value checks is the one record that is never
dropped: an owner-signed one ranks below every valid configuration, so a
held one is replaced by any valid configuration, or by an invalid one of
higher rank, and an arriving one that does not outrank the held one is
skipped. A configuration the owner did not sign is never offered.

## Use the room's own merge

`ChatRoomStateV1::merge` (and `merge_from`) is an inherent method that
SHADOWS `ComposableState::merge`. Never call `ComposableState::merge` on a
room: the trait default reads retention horizons against the receiver's
pre-merge configuration, does not trim to the work bounds, and is not
commutative for rooms. The contract's `update_state` is pinned to
`merge_from` by a native test in `contracts/room-contract`. A receiver that
merges through its own summary (the UI) uses
`delta_from_merged_summary`, which trims the same way.

## Summaries carry a rank, with a digest tie-break

A field that resolves two records by version carries the version AND a
signature or record digest in its summary (`(version, SigDigest)` and
similar), so two DIFFERENT records at one version are told apart and both
peers keep the higher one. A summary that carries only the version leaves two
peers holding different records unreconciled for good. See
`contract-summary-determinism.md` for the encoding rules.

## Held state is normalized on every apply

`verify` accepts states no apply produces (a full-state PUT runs only
`verify`): duplicate records, records over a cap, blobs for members who left.
Every apply (an empty delta included) normalizes what is held by the same
rules arrivals meet, and `post_apply_cleanup` leaves nothing a later apply
would drop, so every state an apply or merge produces is a fixpoint of
`merge(S, S)`. `verify` itself stays as it is: tightening it would make the
migration PUT reject states rooms hold today.

## The real check is FORK_SEEDS=1000

`common/tests/fork_merge_closure_test.rs` grows pairs of forks (honest,
adversarial, extended ban shapes, colliding configurations, and the rich mode:
secrets, private rooms, DMs, purges, pointers) and requires convergence, a
commutative first exchange, and cleanup and `merge(S, S)` fixpoints after
every apply and merge. The default seed count is a smoke test. Run
`FORK_SEEDS=1000` (with `CARGO_PROFILE_TEST_OPT_LEVEL=3`) before merging any
change to `common/src/room_state/`: one fix in #703 was found only at that
count.
