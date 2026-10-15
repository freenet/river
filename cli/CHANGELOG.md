# Changelog

All notable changes to riverctl will be documented in this file.

## [0.2.20] - 2026-10-15

### Fixed
- Migrating a room to a new contract no longer mistakes another room's
  subscription notification for the answer to its own migration PUT. The node
  multiplexes every subscription onto one connection, so a notification for an
  unrelated room arriving during the wait was counted as success: the wrong
  contract key was returned for that run, and the old state's merge was not
  retried. The migration now waits only for the PUT's own answer, always records
  the new contract's key, and refuses to start if the bundled contract does not
  match it. (freenet/river#689)

## [0.2.18] - 2026-09-12

### Fixed
- `message stream` no longer goes silently deaf when River re-keys the room
  contract while it is running. It used to look up the room's address once at
  startup and never again, so after a re-key it kept listening to a contract
  nobody writes to: no error, no output, just a room that appeared to go quiet
  until the bot was restarted. It now re-checks every few minutes and, when
  River's pointer names a different generation, **exits with status 75** so a
  supervisor restarts it. A restarted riverctl uses the current generation.
  (freenet/river#694)

  **If you run a bot, run it under something that restarts it on any failure,
  with a short delay** (systemd `Restart=on-failure` plus `RestartSec=`), and
  note that a restart with `--initial-messages N` re-emits the last N messages.
  See "Running `message stream` as a long-lived bot" in the README.
- `message stream` without `--subscribe` no longer re-emits the room's recent
  history when it starts. It recorded only the messages it displayed, so with the
  default `--initial-messages 0` the first poll reported every recent message as
  new, and with `-i N` it reported all but N of them. It now records everything
  already in the room and shows only the last N, as `--subscribe` always has.
  This matters more now that streams restart on every re-key.
- A polling `message stream` whose `--poll-interval` is longer than the re-check
  interval (a few minutes) now also polls at each re-check, so a long interval
  cannot delay noticing a re-key.

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