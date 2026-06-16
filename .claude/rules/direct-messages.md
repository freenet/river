---
description: When working on in-room direct messages — UI/CLI surface, ECIES envelope, outbound plaintext cache, archive (hide) state, or anything touching DirectMessagesV1
globs:
  - common/src/room_state/direct_messages.rs
  - common/src/ecies.rs
  - common/src/chat_delegate.rs
  - ui/src/components/direct_messages/**
  - cli/src/commands/dm.rs
  - cli/src/private_room.rs
---

# In-Room Direct Messages

End-to-end-encrypted DMs between two members of the same room, carried
inside `ChatRoomStateV1` (NOT a separate contract). Types and validation
live in `common/src/room_state/direct_messages.rs`.

## Wire-format invariants

- Each DM is sender-signed over canonical bytes prefixed by domain tag
  `b'M'`; recipient purge envelopes use `b'P'`. Per-recipient purge
  envelopes are monotonically versioned (Configuration pattern) and the
  tombstone set is BLAKE3-derived `PurgeToken`s, which prevents
  signature-grinding attacks against tombstones.
- Bans are NOT enforced in `DirectMessagesV1::verify` — instead,
  `ChatRoomStateV1::post_apply_cleanup` sweeps DMs whose sender or
  recipient is now banned or no longer a member. Mirrors `MessagesV1`'s
  precedent and keeps `verify` stable across ban-state changes.
- `seal_dm_for_recipient` / `unseal_dm_from_sender` in
  `common/src/ecies.rs` carry the per-message ECIES envelope. Distinct
  from the deterministic `encrypt_secret_for_member` because DM
  plaintext is attacker-controlled (random ephemeral + random nonce per
  call). `open_direct_message` is feature-gated on `ecies` so the
  room-contract WASM (which never decrypts) still builds.
- UI and CLI emit byte-identical wire bytes via shared `river-core`
  helpers: `compose_direct_message`, `open_direct_message`,
  `advance_recipient_purges`.

## UI surface

`ui/src/components/direct_messages/` — thread modal opened from
member-info modal, inbox modal with unread badges, in-memory
`DM_LAST_SEEN` per `(room, peer)`. `seed_dm_last_seen_if_needed`
(called from `App()` via a `use_effect` that subscribes to `ROOMS`)
seeds `DM_LAST_SEEN` from the max inbound DM timestamp per
`(room, peer)` exactly once on first hydration; a one-shot
`DM_LAST_SEEN_SEEDED` flag prevents re-seeding — otherwise every
arriving inbound DM would be instantly marked seen and never surface as
unread.

## Share-invite-via-DM picker

`INVITE_VIA_DM_PICKER` global signal opens
`invite_via_dm_picker_modal.rs`, which lists every other room the
local user is in and signs an invitation against the CANDIDATE room's
key (not the current room). `DM_DRAFT` carries the pre-composed body to
`DmThreadModalBody`, which drains it on mount, APPENDING to any text
the user has already typed (never overwriting).
`invite_member_modal::get_invitation_base_url()` is `pub(crate)` so the
picker produces byte-identical invitation URLs — any change to the URL
format must touch one place. The Invite-Member-modal "Send to a
co-member" entry and the cross-room "is target already a member" filter
are deferred (per-room identities make the filter structurally
infeasible without a global-identity layer).

**Accepting an invite DM.** The UI's invitation card Accept button decodes
the embedded `Invitation` and routes through `present_invitation`. The CLI
counterpart is `riverctl dm accept <carrier_room_id> [--from <sender>]
[--room <target>]` (`cli/src/commands/dm.rs::execute_accept`): it decrypts
the carrier room's inbound DMs, keeps the `Invite` bodies, validates each
embedded `Invitation`'s target against its advertised `room_owner_vk`
(`decode_invitation_from_payload`), selects a single valid invitation
(`select_invite_to_accept` — malformed candidates are skipped, not fatal),
and joins via the shared `ApiClient::accept_invitation_struct` — the same
core the base58 `invite accept` path uses, so the #308 re-accept guard and
`room_secrets` persistence are identical across both entry points.

The UI has its OWN, separate re-accept guard
(`receive_invitation_modal::accept_invitation`, freenet/river#365) that is
NOT the same as the CLI's #308 guard — do not assume they are
interchangeable. The CLI's #308 guard refuses re-accept whenever it has any
stored room credentials; the UI guard is identity-aware, refusing only when
the per-room key the user already holds (`RoomData.self_sk`) is itself a
member, which deliberately preserves the UI's restore-access branch. The UI
guard is best-effort (it falls through on an unreadable `ROOMS`); making the
invitation-accept GET handler structurally re-accept-proof is tracked as
freenet/river#367.

## Placeholder render

`BodyKind::{Plaintext, Placeholder}` in `dm_thread_modal.rs` routes
placeholder strings (`"sent — ciphertext only"`,
`"unable to decrypt: …"`) through a plain muted text node, skipping
markdown — the markdown crate's autolinker otherwise mangled the
`<scheme:...>` prefix into a broken anchor.

## Outbound-DM plaintext cache (issue #256)

The room contract carries DM bodies as ECIES ciphertext only the
recipient can decrypt, so the sender's UI / `riverctl dm list` cache
plaintext in the chat delegate.

- **Wire format** in `common/src/chat_delegate.rs`:
  `OUTBOUND_DMS_STORAGE_KEY = b"outbound_dms"`,
  `OutboundDmStore { entries: Vec<OutboundDmEntry> }` — `Vec` not
  `HashMap` so JSON serialisation works (per the "non-string map keys
  in JSON-serialized API types" bug-prevention pattern); JSON and CBOR
  round-trip tests pin both shapes.
- **UI in-memory cache**: `OUTBOUND_DMS: GlobalSignal<OutboundDmsCache>`
  keyed by `(VerifyingKey, MemberId, PurgeToken)`. Hydrated by
  `fire_load_outbound_dms_request`.
- **Render path**: both `DmThreadModalBody` and `riverctl dm list` go
  through the shared pure helper
  `lookup_outbound_plaintext(cache, room, recipient, token)`. Cache
  hit → plaintext; miss → legacy placeholder. Pinned by
  `dm_outbound_lookup_returns_plaintext_on_hit` /
  `…_returns_err_on_miss`.
- **Save path**: `save_outbound_dm()` defers the cache insert, enforces
  `MAX_DM_MESSAGES_PER_PAIR` eviction, and queues a coalesced save via
  `save_outbound_dms_to_delegate`. The coalesce primitive is the shared
  `coalesce_save` helper in `chat_delegate.rs`, driven by a
  `CoalesceState` (futures-Mutex + AtomicBool DIRTY +
  `Mutex<Result<(), String>>` last-result store). The last-result store
  propagates failures to queued callers whose own loop runs zero
  iterations, so e.g. `mark_legacy_migration_done()` in
  `response_handler.rs` can't see a false-`Ok` from a catch-up save
  that actually failed. A chain of N rapid mutations produces at most 2
  delegate writes. `save_outbound_dms_to_delegate` and
  `save_rooms_to_delegate` both share the helper — grep `CoalesceState`
  to find every caller.
- **Prune path**: `prune_outbound_dms_for_purges` (UI) and
  `prune_outbound_cache_for_room` (CLI) act ONLY on entries whose
  `(room, recipient, token)` appears in some recipient's
  `AuthorizedRecipientPurges` envelope — NEVER on the negative "no
  longer in `direct_messages.messages`" signal. The negative signal
  destroys the cache on cold-start when `outbound_dms` hydrates before
  `direct_messages` state has caught up.
- **Legacy migration**: see `.claude/rules/river-publish.md` "How
  Delegate Migration Works". `outbound_dms` is a FIXED single key, so it
  must stay in the `storage_keys` probe array in
  `fire_legacy_migration_request` AND keep its routing in
  `response_handler.rs`. (Dynamic key families like `room:<vk>` are
  instead discovered via the legacy `ListRequest` path — see river-publish.md
  for that carve-out. `outbound_dms` is NOT dynamic, so the fixed probe
  is the right mechanism for it.)
- **CLI side**: persists the same `OutboundDmStore` shape into
  `outbound_dms.json` in the riverctl data dir (consistent with
  `rooms.json`'s plaintext-on-disk threat model).

## Archive-stale-DM-threads (issue #261)

Local-only view filter that takes a DM thread off the left rail. UX
label is "Archive" everywhere; the data shape and Rust APIs keep the
original "hide" / `hidden_threads` / `HIDDEN_DM_THREADS` names because
renaming would force a delegate migration for zero functional benefit.
Treat "Archive" as the UX label and "hide" as the implementation noun —
do NOT rename or the on-wire blob and the visible UI drift.

Storage piggybacks **the same** `OUTBOUND_DMS_STORAGE_KEY` blob —
`OutboundDmStore` has a `hidden_threads: Vec<HiddenDmThreadEntry>`
field with `#[serde(default)]` so old bytes still decode. **Do not add
a second top-level delegate storage key for hide state**: a new key
needs its own probe in `fire_legacy_migration_request` and routing in
`response_handler.rs`, AND splits the multi-device save path into two
writes that can race. Filter helper `chat_delegate::is_thread_hidden`
uses strict `<=`; rail-side pure helper
`dm_rail_section::filter_rail_entries` is pinned by
`filter_rail_entries_*` tests; the "click Archive again after revival
must re-hide" branch is pinned by `hide_unhide_rehide_round_trip`.

The per-row rollover ✕ in `DmRailSection` is the archive control (the
old modal-header "Hide" button next to close ✕ was repeatedly
mis-clicked). "Delete their messages" goes through a Cancel/Delete
confirmation modal, not `purge_thread` directly. The "Archived (N)"
viewer at the bottom of `DmRailSection` surfaces every archived
`(room, peer)` with an Un-archive control; sorting/projection go
through `build_archived_rows`, pinned by
`build_archived_rows_projects_and_sorts` and
`build_archived_rows_falls_back_when_room_missing`.
