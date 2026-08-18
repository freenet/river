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
- Every contract/delegate here can re-key on any release (see the Migration notes above) — **a build-time-constant reference to a key will silently go stale.** Resolve a pointer instead; see below.

## Stable identity: resolve a pointer, do not pin a key

River publishes **pointer records** for the two artifacts that re-key. A pointer record is a contract at a **fixed address** whose state names the artifact's *current* code hash, signed by River's author key. You GET the pointer, read the code hash, and derive the key you actually wanted from that hash **plus your own params**. The address never changes, so your build-time constant never goes stale.

This implements the convention in [freenet-core#5194](https://github.com/freenet/freenet-core/issues/5194).

### The author verifying key — your trust anchor

```
river:v1:vk:9Ebskq4y7NvJpTQTrF1FAxU8g6bR4Rhe4TRikXba55EJ
```

Pin **this 32-byte value** as a constant in your build. It is the entire trust anchor: take it from anywhere else and you may resolve a validly-signed pointer belonging to somebody else. It is the same key that signs River's web container, which is why `raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv` derives from it.

Two things we would rather you learned here than discovered later:

- **River does not keep this key offline**, contrary to the pointer contract's own recommendation, because the same key signs every UI publish. We are telling you what we actually do rather than what the docs advise.
- **Rotating it would move everything at once** — the web container's address and both pointer addresses. That would strand anyone who baked in the author vk, so it is a coordinated flag day, not routine key hygiene.

### The pointers

| `app_id` | Points at | Pointer key (fixed, GET this) |
|---|---|---|
| `river.room-contract` | [`contracts/room-contract/`](contracts/room-contract/) | `Ai4VLoC2jGdhpcB2UU8VPo3efUoxjm1Ju9VKXqRC63Az` |
| `river.chat-delegate` | [`delegates/chat-delegate/`](delegates/chat-delegate/) | `6qF2H5JRPBxbKC45UtPnzdDzyfsejYFW1UwDLGDU66mu` |

Both addresses are derivable offline from the pointer contract's frozen code hash `8wnAPaSRY1oYZCz723fdwK6BgzL6q8ozP3buVovXnt6v` and `(author_vk ‖ app_id)` — you do not have to trust the table.

Current records are in [`pointer-records.toml`](pointer-records.toml), which CI checks on every PR (`scripts/check-pointer-freshness.sh`): if a pointed-at WASM changes and no new record is signed, the build fails. That gate is the reason resolving is safer than pinning.

### How to resolve

Rust integrators should use the resolver rather than hand-rolling it — it carries the anti-rollback floor and the absence-vs-unreachability distinction, neither of which you get from decoding the record yourself:

```rust
use freenet_migrate::pointer::{resolve_app_pointer, PointerFloor, PointerOutcome};

let outcome = resolve_app_pointer(&mut io, &RIVER_AUTHOR_VK, b"river.chat-delegate", floor).await?;
```

Handle **every** arm. A bare `if let Some(r) = outcome.resolved()` silently does nothing on the outcomes that carry no record, which is how a withdrawal, a rollback attempt and a plain timeout all become "no output". Only `NeverPublished` permits falling back to a baked-in key. Persist `outcome.next_floor()`, keyed by `(author_vk, app_id)`.

Non-Rust implementers: the wire format, the four resolution steps and hex test vectors are in the [pointer contract's README](https://github.com/freenet/freenet-migrate/tree/main/contracts/pointer-contract) and `TEST-VECTORS.md`.

### What a pointer does NOT tell you

**It solves addressing only.** It tells you which code hash is current. It says nothing about whether any state or any secret held under the previous key survived the re-key.

This matters most for `river.chat-delegate`. Delegate secrets move only when River's own UI has run on that user's node, so you can resolve the pointer perfectly, derive the right key, and still find an **empty namespace** — which looks like "this user has no data" rather than like an error. That is the specific confusion the pointer exists to remove, so please do not let it back in one level up: treat data survival as a separate question, verify it per artifact, and assume it is unsolved until you have.
