---
description: The native apps workspace under apps/ (river-app and the desktop shell), what it reads and writes in the chat delegate, and how to build and render it
globs:
  - apps/**
paths:
  - "apps/**"
---

# Native apps (`apps/`)

`apps/` is a **separate Cargo workspace** with its own `Cargo.lock` and `rust-toolchain.toml`. It is separate so its UI dependencies can never change the root lockfile, which fixes the contract and delegate WASM bytes and so their keys. `cargo make` tasks and `build.yml` do not build it; `.github/workflows/apps.yml` does.

| Crate | Role |
|---|---|
| `apps/river-app` | UI and setup state machine, shared by every platform |
| `apps/desktop` | Desktop shell (binary `river`), including the Freenet installer, which verifies the release's signed checksums |

```bash
cd apps
cargo test --locked                  # unit tests
cargo run -p river-desktop           # the app
cargo run -p river-desktop -- --list-scenes
cargo run -p river-desktop -- --snap out.png --scene not-installed   # headless render
cargo run -p river-desktop -- --snap out.png --scene chat-narrow --size 820x720
```

The window opens at 1280x800 and shrinks to 640x480. Every screen, setup included, uses one dark palette (`apps/river-app/src/chat/style.rs`); there is no light theme yet. A scene cannot set the window size, so the `*narrow*` scenes need `--size 820x720` to render as CI renders them (`scene_size` in `apps/river-app/src/lib.rs`, checked against `apps.yml` by a test).

Linux builds need `libxkbcommon-dev` and `libwayland-dev`. Headless renders without a GPU need a software Vulkan driver: install `mesa-vulkan-drivers` and point `VK_ICD_FILENAMES` at its `lvp_icd*.json`.

## How river-app reads rooms

Once setup connects, `src/rooms/` reads the user's rooms from the local node:

1. It asks the node for the River web app's identity through a user-approved app grant (freenet-core v0.2.156+).
2. It reads the rooms stored in the chat delegate and subscribes to each room contract.
3. When the node has no chat delegate, it registers the one it carries, as the web UI does since #757 (list first, register on a missing delegate). It does this only on the grant connection: the node records, for good, the identity a delegate was first registered under.

A room the app stores does not hide older rooms from the web UI. The web UI's journaled walk (#763) carries them forward whatever the current delegate holds (`src/rooms/work.rs` has the details). So rooms from an earlier River in the browser show up in the app after River has been opened in the browser once.

The chat screen (`src/chat/`) shows the rooms. Snapshots and tests use built-in example data (`chat::Source::Example`). Colours and sizes follow the web UI in `ui/`.

## What river-app writes

| Write | Where | Guard |
|---|---|---|
| Messages, replies, edits and deletions of own messages, reactions | Room contract UPDATE, built and signed as the web UI does | `src/rooms/outbox.rs` |
| A room it creates | Seed PUT under that room's own contract (its only PUT) | `src/rooms/create.rs` |
| Joining from an invitation | UPDATE (the web UI PUTs instead) | `src/rooms/work.rs` |
| New or joined room | Chat delegate, by the web UI's generation compare-and-swap (`GetVersionedRequest`, then `CasStoreRequest`, re-read on conflict). Only ADDS a room where the key holds nothing, or a tombstone when joining | `river_core::stored_rooms` |
| `rooms_meta` | Chat delegate, changing only the open room | `river_core::stored_rooms` |
| Each room's signing key (`self_sk`) | Delegate `signing_key:` namespace, only when the delegate holds none. Never replaces a key. Lowest priority, one request at a time, backed off on failure | `src/rooms/keys.rs` |

Those are its only writes to the chat delegate, which the web UI writes at the same time. The `stored_rooms_drift` tests in `ui/src/room_data.rs` pin the bytes it stores to the web UI's types.

Anything the app stores in the chat delegate must stay decodable by the web UI builds in use, or their migration walk stops on every load.

When a pointer record names a generation newer than the WASM the app carries, the app stops writing and asks the user to update (`apps/river-app/src/rooms/generation.rs`).

## What it shares with the root workspace

It depends on `river-core` (`common/`) by path, with the client-only features `ecies-randomized`, `ecies-client`, `mentions`, `stored-rooms` and `invitation`. A change under `common/`, or to the root `Cargo.toml` (river-core inherits versions from it), runs `apps.yml`.

It compiles in three `ui/` files with `include_bytes!`. An edit to any of them runs `apps.yml`; nothing else under `ui/` is read by `apps/`.

| File | Why |
|---|---|
| `ui/assets/river_logo.svg` | The logo |
| `ui/public/contracts/chat_delegate.wasm` | Its hash is the delegate key the app reads rooms from |
| `ui/public/contracts/room_contract.wasm` | Its hash is part of every room's contract key |

A test checks that its `freenet-stdlib` (the client API's wire format) is the version river-core resolves to in the root `Cargo.lock`.

`apps/Cargo.toml` and `apps/Cargo.lock` cannot change a contract or delegate key. The WASM is built from the root workspace with the root `Cargo.lock`, and the apps' lockfile is never consulted there, even though `apps/river-app` depends on `river-core` by path (see the note in `apps/Cargo.toml`). Every client-only feature it enables must stay off for the room-contract and chat-delegate builds.

Releasing: `.claude/rules/desktop-release.md`.
