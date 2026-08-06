# FREENET.md

This file enumerates the Freenet contracts and delegates published from this repository — what each one is for, where its source lives, and how to depend on it — for anyone integrating with River rather than building it. It's a convention (see [freenet-core#5194](https://github.com/freenet/freenet-core/issues/5194)), not a protocol requirement: a fixed, predictable place to look before reading source.

## Contracts

### room-contract
- **Purpose:** Manages a single chat room's membership, permissions, message history, bans, and (for private rooms) encrypted secret distribution.
- **Source:** [`contracts/room-contract/`](contracts/room-contract/)
- **Shared types crate:** [`river-core`](https://crates.io/crates/river-core) (published) — `ChatRoomStateV1` and everything under it live here, independent of the WASM target.
- **Deployed key:** none fixed — every room is its own instance. A room's contract key is derived from the room owner's verifying key (see `river-core`'s key derivation helpers), so there are as many keys as there are rooms.
- **Migration:** re-keys on any WASM change; `common/legacy_room_contracts.toml` records every prior generation so a client can recover a room dormant across an upgrade (see [`.claude/rules/delegate-migration.md`](.claude/rules/delegate-migration.md)).

### web-container-contract
- **Purpose:** Serves the compiled River UI (the Dioxus web app) as a Freenet contract asset — this is what a browser loads when it opens River.
- **Source:** [`contracts/web-container-contract/`](contracts/web-container-contract/)
- **Deployed key:** `raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv` (fixed — one instance, republished in place on every UI release via a monotonically-versioned author-signed update, never re-keyed for a UI-only change).
- Not vendored elsewhere as source — other Freenet apps that need a web-container contract reuse the compiled `web_container_contract.wasm` artifact directly rather than depending on this crate (see ghostkeys and Atlas, which both do this).

## Delegates

### chat-delegate
- **Purpose:** Executes chat-specific background logic on the user's own node — room list/metadata management, per-room secret storage, outbound-DM plaintext caching, legacy-migration probing.
- **Source:** [`delegates/chat-delegate/`](delegates/chat-delegate/)
- **Shared types crate:** [`river-core`](https://crates.io/crates/river-core) (published) — the delegate message types (`InboundAppMessage`/`OutboundAppMessage` equivalents) live alongside the contract types.
- **Migration:** re-keys on any WASM change (code, dependency bump, even a version-string bump); `legacy_delegates.toml` records every prior generation, and the delegate itself sweeps that registry on startup to carry a user's existing rooms/secrets forward (see [`.claude/rules/delegate-migration.md`](.claude/rules/delegate-migration.md)).

## CLI

### riverctl
- **Purpose:** Command-line client for River — read/send messages, manage rooms and invitations, scriptable access without the UI.
- **Source:** [`cli/`](cli/)
- **Crate:** [`riverctl`](https://crates.io/crates/riverctl) (published, `cargo install riverctl`).

## Notes for integrators

- Depend on `river-core` for the wire types; you almost never need to compile or execute the contract/delegate WASM yourself to read or construct River-compatible data.
- Every contract/delegate here can re-key on any release (see the Migration notes above) — a build-time-constant reference to a key will silently go stale. There is no stable-identity pointer published yet; track [freenet-core#5194](https://github.com/freenet/freenet-core/issues/5194) for when one lands.
