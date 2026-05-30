---
description: When working on private rooms — secret rotation, encrypted_secrets distribution, member_info coupling, invitation-carried secrets, or CLI parity for private-room paths
globs:
  - common/src/room_state/privacy.rs
  - common/src/room_state/secret.rs
  - common/src/room_state/configuration.rs
  - common/src/key_derivation.rs
  - common/src/ecies.rs
  - ui/src/util/ecies.rs
  - ui/src/room_data.rs
  - cli/src/private_room.rs
  - cli/src/storage.rs
  - delegates/chat-delegate/src/subscription.rs
  - common/tests/private_room_test.rs
---

# Private Room Support

- Messages, metadata, and member nicknames are encrypted with AES-256-GCM.
- Room secrets distributed two ways: (a) owner-signed `encrypted_secrets`
  blobs in the room contract, ECIES-wrapped per member (X25519 + AES-256-GCM);
  (b) for a new invitee, the secrets are also embedded in the `Invitation`
  artifact so they can read the room immediately on join without waiting for
  the owner's delegate to back-fill an `encrypted_secrets` blob. The contract
  blob is authoritative and supersedes the invitation-carried copy.

## Secret rotation

Two converging paths:

- **UI synchronous fast-path** (`RoomData::rotate_secret`): runs while the
  owner is actively driving a state change — banning a member, clicking the
  manual "Rotate" button. Synchronous because we need the next owner-sent
  message to use the new key before the just-banned member can decrypt it.
- **Delegate asynchronous catch-up**
  (`chat-delegate::handle_contract_notification`): runs when the UI isn't
  actively driving — auto-prune from message lifecycle, peer state updates
  received in the background. Triggered by `ContractNotification` from the
  runtime when a subscribed contract's state changes. Owner does NOT need
  the UI open.
- Both paths derive the new secret deterministically via
  `river_core::key_derivation::derive_room_secret(seed, owner_vk, new_version)`,
  so they produce **byte-identical** secrets for the same target version.
  Concurrent rotation by both paths therefore converges via the contract's
  duplicate-version dedup in `RoomSecretsV1::apply_delta`
  (`secret.rs:140-145`).

## Shared rotation back-fill helper (issue #110)

`river_core::room_state::secret::build_rotation_encrypted_secrets` is the
single source of truth for the set of `AuthorizedEncryptedSecretForMember`
blobs a rotation emits. Both `RoomData::rotate_secret` (UI fast-path) and
`chat-delegate::subscription::run_rotation` (delegate catch-up) call into
it with the same `(signing_key, owner_vk, owner_id, new_version, new_secret,
current_members_with_vks, existing_encrypted_secrets)` inputs and therefore
emit **byte-identical** blob sets. Convergence depends on this — if the two
paths drift, concurrent rotation would produce different `(member, version)`
tuples and the contract's dedup couldn't reconcile them. The helper iterates
the versions actually present in `existing_encrypted_secrets` (plus the
caller's `new_version`), NOT the numeric range `0..=new_version`, so a sparse
state with a high `current_version` doesn't loop a billion times per member.

## `post_apply_cleanup` encrypted_secrets exemption (issue #110)

A member for whom the owner has issued an
`AuthorizedEncryptedSecretForMember` blob **at the current secret version**
is exempt from inactivity-prune. The owner-issued blob is treated as proof
of membership-intent that pre-dates any authored join_event — without this,
an invitee's first state ingestion would prune them before they've authored
anything, surfacing as "newly-invited member silently dropped".

The exemption is SCOPED to `current_version` so cleanup still prunes members
whose blobs only exist at older versions (a member who never got re-issued
at the latest rotation is "stale" by the same definition as one who never
authored). Banned members are NOT exempted even if they hold a blob — the
`members_by_id.contains_key(recipient_id)` guard at the cleanup site
short-circuits before the exemption can fire (the ban delta runs through
the member-prune path first).

Pinned by `test_member_with_encrypted_secret_survives_cleanup`,
`test_banned_member_with_encrypted_secret_is_still_pruned`,
`test_stale_secret_recipient_is_pruned_after_rotation`, and
`test_ban_race_with_encrypted_secret_converges_to_pruned` in
`common/src/room_state.rs`.

## `member_info` must accompany every membership change

Whenever a member is added to `room_state.members` on a path that goes to
the network, their `AuthorizedMemberInfo` MUST be written to
`room_state.member_info` in the same wire payload. A member present in
`members` but absent from `member_info` is valid contract state
(member_info entries are optional per `MemberInfoV1::verify`) but renders
as **"Unknown"** to every other peer.

`build_state_for_put` (invitation-accept PUT) is the canonical example: it
must inject the invitee's `member_info` byte-identically to the deferred
local-state copy — the same build-once-reuse discipline the synthesised
join_event follows.

The remediation for already-stranded members is
`RoomData::build_member_info_heal`: on every GET of an existing room it
detects "self in `members`, absent from `member_info`" and re-publishes a
self-signed `member_info` (folded into the PUT for imported rooms, sent as
a standalone UPDATE for already-subscribed rooms). A non-owner's
`member_info` is only valid when self-signed by that member's own key, so
this heal can ONLY run client-side, by the affected member — never
owner-side. For a private room the heal defers (publishes nothing) until
the room secret is available, so it never leaks a plaintext nickname.

## In-memory secret repopulation (issue #251)

`room_data.secrets: HashMap<u32, [u8; 32]>` is `#[serde(skip)]` and must
be rebuilt from `room_state.secrets.encrypted_secrets` after EVERY network
state ingestion — initial GET, refresh/suspension GET, delegate-load merge,
`apply_delta`, and full-state `update_room_state`. The helper
`RoomData::repopulate_secrets_from_state` is the single source of truth;
any new ingestion path MUST call it (the
`repopulate_secrets_call_sites_pinned` test pins the existing call sites
by source-grep so dropping one fails CI). Skipping the helper causes the
bug from #251: newly-joined private-room members render
`[Encrypted message - secret vN not available]` until they hard-refresh,
because the back-filled blob arrives in a *subsequent* state update that
the in-memory map never sees.

`repopulate_secrets_from_state` also folds in `room_data.invitation_secrets`
(secrets carried in the `Invitation` artifact) for versions the contract
has not yet provided an owner-signed blob for; the owner-signed blob is
authoritative and overwrites an invitation-carried value at the same
version (and prunes it from `invitation_secrets`).

## CLI (riverctl) parity surface (issue #302)

riverctl carries the same `Invitation::room_secrets` wire shape as the UI,
with `cli::private_room::{collect_secrets_for_room, collect_invitation_secrets,
seal_invitee_nickname, current_secret_from_state}` as the byte-identical
CLI counterparts. `StoredRoomInfo::invitation_secrets` (a
`HashMap<u32, [u8; 32]>` in `rooms.json`) persists the invitation-carried
secrets across CLI invocations.

**Critical**: `collect_secrets_for_room` does NOT derive owner secrets via
`derive_room_secret` — the initial random v0 from `generate_room_secret()`
cannot be re-derived, so the owner-as-inviter path always decrypts the
owner-addressed contract blob from `state.secrets.encrypted_secrets` like
any other member.

The CLI does NOT currently have a `build_member_info_heal` counterpart: a
private-room invitee whose invitation lacks `current_version`'s secret
will defer `member_info` and surface as **"Unknown"** to other peers
indefinitely (filed as freenet/river#304). The CLI also does NOT prune
superseded `invitation_secrets` entries the way the UI does — storage
waste only; the heal path is the natural place to hook the prune.

### Rejoin nickname restoration (`StoredRoomInfo.self_nickname`)

The CLI persists the member's own nickname in
`StoredRoomInfo.self_nickname` (set on `accept_invitation`,
`set_nickname`, and public-room `import_identity`). When a member is
pruned for inactivity and later reposts, `ApiClient::build_rejoin_delta`
re-adds them AND restores this nickname via the free helper
`rejoin_preferred_nickname` instead of the generic `"Member"`
placeholder. The helper routes the nickname through
`seal_invitee_nickname` (public bytes for a public room, sealed for a
private room) and falls back to public `"Member"` when no nickname is
stored, a private room has no secret (so the plaintext is never leaked),
OR the stored nickname exceeds the room's current `max_nickname_size`
(which would otherwise make the contract reject the whole rejoin delta).

This is a PARTIAL analog of the UI's `build_member_info_heal`, NOT the
full #304 heal: it only restores the nickname on the rejoin/send path,
and it diverges from the UI in the private-room-no-secret case — the UI
*defers* `member_info` (member shows "Unknown"), whereas the CLI
publishes a public `"Member"` placeholder (pre-existing CLI behavior;
safe — no plaintext leak). The call site is pinned by
`rejoin_nickname_wiring_pinned` in `cli/src/storage.rs` and the
selection matrix by `rejoin_nickname_tests` in `cli/src/api.rs`.

## Key files

- `common/src/room_state/privacy.rs`, `secret.rs`, `configuration.rs`
- `common/src/key_derivation.rs`
- `ui/src/util/ecies.rs`, `ui/src/room_data.rs`
- `cli/src/private_room.rs`, `cli/src/storage.rs::StoredRoomInfo`
- `delegates/chat-delegate/src/subscription.rs`
- `common/tests/private_room_test.rs`
