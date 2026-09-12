# Changelog

All notable changes to riverctl will be documented in this file.

## [Unreleased]

### Fixed
- `message stream` (both `--subscribe` and polling modes) now re-checks River's
  room-contract pointer every five minutes instead of once at startup, and
  follows a re-key while running. Previously a long-running bot resolved the
  pointer once, stayed bound to the retired contract for the life of the
  process, and went silently deaf: the node has nothing left to send for a
  generation nobody writes to, and a poll against the retired contract still
  succeeds because that contract still exists. Neither mode surfaced any error,
  so the only symptom was a busy room appearing to go quiet, and the only fix
  was restarting the bot. On a re-key the subscription now re-subscribes to the
  new generation, prints a stderr notice naming both generations, and catches up
  on anything written during the gap. Reads follow the move even when the new
  generation is one this riverctl is too old to write to. (freenet/river#694)

  Details worth knowing if you run a bot:
  - The catch-up fetch runs after **every** refresh, not only after a re-key,
    because the pointer GET shares the node connection with the subscription and
    steps over (discarding) frames while it waits for its own answer. One of
    those can be an update notification for the room being streamed.
  - A re-SUBSCRIBE the node will not yet accept is **not** fatal. The node most
    likely to refuse is one that does not hold the newly-published generation
    yet, so riverctl retries every 30s and keeps polling meanwhile rather than
    exiting.
  - A stream moves only when the move is both **evidenced and actionable**. A
    refresh that merely timed out can name a different hash, and acting on that
    would announce a re-key that never happened and re-subscribe to a retired
    generation. And a *verified* re-key to a generation this riverctl does not
    know is reported but **not** followed: the new key holds nothing this binary
    can reach (the backward probe will not search from a generation it does not
    know, and writing is refused), so following it would trade a subscription
    that is still delivering for a key it cannot act on — leaving the stream
    silent on both. In that case riverctl stays where the data is and tells you
    to run `cargo install riverctl --force`.
  - A signed **withdrawal** of the pointer record ends the stream, rather than
    being swallowed as a transient failure like every other resolution error.
  - The interval carries ±20% jitter, so a fleet of bots started together does
    not hit the network in one synchronised burst after a re-key.
  - The stream's **periodic** fetch no longer writes: it does not migrate the
    room, does not self-heal `member_info`, and does not republish state it
    recovered from an older generation. All three still happen on the stream's
    first fetch and in every one-shot command. Issuing them on a timer as a side
    effect of reading was surprising, and — because of a known hazard when a
    migration's GET races a live notification — unsafe. The periodic fetch still
    READS across older generations, which is what makes a catch-up straight
    after a re-key return anything at all.
  - `message stream` without `--subscribe` now does one full fetch at startup
    regardless of `--initial-messages`, so migration and the `member_info`
    self-heal still run in the default polling mode.

## [0.2.15] - 2026-09-06

### Fixed
- `message stream --format json` now includes `author_verifying_key` on every
  event (`message`, `edit`, `delete`, `reaction`), matching `message list`.
  Previously only the `message list` backfill carried the author's full key, so a
  bot driven off the live stream had to fall back to a `message list` call per
  event to recover it, or trust the 8-character `author` short id (a 40-bit
  truncation of a 64-bit non-cryptographic hash, which two members can share) or
  the member-controlled `nickname`. Both stream modes are covered (polling and
  `--subscribe`). Resolution and base58 encoding now come from a single shared
  helper, so the backfill and the live feed cannot drift. The value is `null`
  when the author is not in the room state riverctl holds — departed, pruned, or
  a members delta not yet merged — and never a wrong key. On a `reaction` event
  it names the author of the message reacted to, not the reactor; `reactors`
  remains short ids only. (freenet/river#679)

## [0.2.14] - 2026-08-31

### Added
- Member-targeting commands (`member ban`, `member deputize`,
  `member revoke-deputy`, `member deputies`, `member deputized-by`) now accept a
  member's full base58 verifying key in place of the 8-character short ID. A
  full key names exactly one member (the short ID is a 40-bit truncation that
  two members can share), so it resolves unambiguously and satisfies
  `--require-exact-member-id` by construction.
- `message list --format json` now includes an `author_verifying_key` field
  (base58, or `null` when the author is no longer in room state), so a bot can
  identify a message's author by their collision-proof key.

## [0.2.13] - 2026-08-31

### Added
- `member list` now shows each member's full ed25519 verifying key (base58) —
  in human output as an indented `key:` line, and in `--format json` as a
  `verifying_key` field. The 8-character short id is only a 40-bit truncation
  and is cheap to collide on a targeted basis, so use the full verifying key
  (which matches `identity whoami`'s `verifying_key`) as the collision-proof
  identity when trusting a member, e.g. a bot allow-list.

## [0.1.8] - 2025-08-09

### Fixed
- Publish membership delta on invite accept so other members see invitee
- Add INFO logs for GET/SUBSCRIBE/UPDATE during accept
- Reduce GET/SUBSCRIBE timeouts (2s/1s) to fail fast

## [0.1.7] - 2025-08-01

### Fixed
- Fixed architectural issue with GET operations using `subscribe: true`
  - GET operations now use `subscribe: false` followed by separate SUBSCRIBE operations
  - This fixes compatibility with Freenet's current architecture
  - Both `get_room()` and `accept_invitation()` methods updated
- This fix enables multi-user messaging to work properly

### Technical Details
- GET with subscribe:true requires performing sub-operations from within the main operation and waiting for them to complete, which was never implemented in Freenet
- The fix separates GET and SUBSCRIBE into distinct operations, matching how the River web UI already works

## [0.1.6] - 2025-08-01

### Fixed
- Fixed critical bug where invited users could not send messages after accepting invitations (#28)
  - Room state is now properly initialized when accepting invitations
  - Invited users are correctly added to the members list
  - Member info with nickname is properly created
- Added validation to ensure room state initialization is correct

### Added
- Comprehensive unit tests for invitation flow
- Integration test script for multi-user scenarios

## [0.1.5] - Previous releases...