# Agent guide for River

River is a decentralized group chat on Freenet. Rooms are contracts on the network; there is no server.

- `common/` (`river-core`) is compiled into the room contract and chat delegate WASM, except modules behind client-only features (`invitation`, `stored-rooms`, `ecies-client`, ...).
- `apps/` is **a separate Cargo workspace** with its own lockfile. `cargo make` does not build it.

## Commands

```bash
cargo make dev-example               # dx serve with example data (Freenet sync stays on)
cargo make build-ui-example-no-sync  # UI with example data, no Freenet connection
cargo make build                     # UI + web container; embeds the committed contract/delegate WASM
cargo make sync-wasm                 # the only task that rebuilds the committed WASM (re-keys: see below)
cargo make test                      # contracts, common, delegate; run before every PR
cargo make test-ui                   # river-ui native unit tests (CI-gated)
cd common && cargo test private_room # after touching encryption or secret distribution
cargo make clippy && cargo fmt
```

`cargo test -p river-ui --bins` alone fails on a clean tree with "Asset at /assets/styles.css doesn't exist": `ui/assets/styles.css` is a gitignored build product. `cargo make test-ui` runs `cargo make build-tailwind` first.

Use `freenet local` for routine UI work and manual testing. Use `freenet network` only to validate peer-to-peer sync or multi-node behaviour, and say in the PR that you did.

## Read before you touch

Rule files in `.claude/rules/` load automatically only when you open a matching file. A command-driven task (publishing, releasing, a dependency bump) may open none, and other tools load none, so read the rule for your task first.

| If you are about to... | Read |
|---|---|
| Commit anything | `wasm-safety.md` |
| Edit `ui/src/` | `dioxus-signal-safety.md` |
| Run, write or debug UI tests | `ui-testing.md`, then the `local-dev` skill |
| Change room history or DM thread scrolling | `history-scrolling.md` |
| Edit `common/src/room_state/` or `contracts/room-contract/` | `merge-laws.md`, `contract-summary-determinism.md` |
| Touch direct messages | `direct-messages.md` |
| Touch private rooms, secrets or `member_info` | `private-rooms.md` |
| Change anything under `common/`, `contracts/`, `delegates/`, or root `Cargo.toml`/`Cargo.lock` | `delegate-migration.md` |
| Change how clients follow a room to a new contract key | `contract-upgrade.md` |
| Publish the UI or release riverctl | `river-publish.md` |
| Edit `apps/` | `native-apps.md` |
| Release the desktop app | `desktop-release.md` |

## Hard rules

### WASM changes re-key

A change under `delegates/`, `contracts/` or `common/`, and most changes to the root `Cargo.toml`/`Cargo.lock`, change the delegate or contract WASM and so its key. **Without a migration entry, users lose all room data.** A comment-only edit can re-key the room contract.

Run `cargo make add-migration` before the WASM changes, and `cargo make sign-pointer-records` with it. CI's `check-pointer-freshness` fails a PR that changes a pointed-at WASM without a new record. A migration entry carries our users' data forward; a pointer record says where the artifact moved (addressing only). Do not blur the two. Full workflow: `delegate-migration.md`.

`apps/Cargo.toml` and `apps/Cargo.lock` cannot change a key: the WASM builds from the root workspace and lockfile.

Never `git add -A`, `.` or `-u`: it picks up rebuilt, non-reproducible WASM. Add files by name (`wasm-safety.md`).

### State authorization rule

**Every piece of data in contract state must be cryptographically authorized. Never accept unauthorized data, even as a "temporary" or "lenient" measure.**

| Record | Must have |
|---|---|
| Message | A valid signature from a verified member at the time it is added |
| Member | A valid invitation chain back to the room owner |
| Ban | Authorization from the room owner's tree |

Verify at insertion time, never "when the data arrives later". If a delta would add data that cannot be verified (a message whose author is not in the member list), include the authorization data in the delta (the member entry beside the message); do not relax verification.

Fully authorized state is what makes contract migration permissionless: anyone can move a room's state to a new contract key, and the new contract checks every signature (`contract-upgrade.md`).

### Backwards compatibility rule

`ChatRoomStateV1` and all its sub-types must stay backwards-compatible.

- New fields use `#[serde(default)]`.
- **On an individually signed sub-struct** (`MemberInfo`, `Member`, `UserBan`, `MessageV1`, `Configuration`: anything in an `Authorized…` wrapper whose signature covers its ciborium bytes), a new field needs **both** `#[serde(default)]` **and** `#[serde(skip_serializing_if = …)]`, so the empty value serializes byte-identically to the old record. Plain `#[serde(default)]` re-serializes a wider struct, every existing signature fails, and every room migrates to empty; it is safe only on unsigned containers. Put the field last, never reorder, and add a byte-identity test (see `member_info.rs::empty_deputies_serializes_identically_to_legacy_member_info`, #410).
- Never remove or rename a field, or change how an existing field serializes.
- A truly breaking change needs a V2 type with an explicit migration (a separate project).

This keeps any old state, normalized by `ChatRoomStateV1::normalized`, acceptable to a new WASM's `validate_state()`.

### Dioxus WASM signal safety rules

The UI is single-threaded WASM, so a re-entrant `RefCell` borrow is a panic. Full rules with examples: `dioxus-signal-safety.md`.

- Read signals with `try_read()`, and guard memos and effects with `signal_guard::anchor()` and `schedule_nudge()`.
- Never call `spawn_local` inside a polled future; use `crate::util::safe_spawn_local`.
- Wrap signal mutations from `spawn_local` tasks and event handlers in `crate::util::defer()`.
- Never use raw `setTimeout` for signal mutations.
- Never defer signal clears in `use_effect`.
- Never `use_memo` against non-signal values in an always-mounted component.

### Summaries are deterministic

Every field of a contract `Summary` serializes deterministically: `BTreeMap`/`BTreeSet`/sorted `Vec`, never `HashMap`/`HashSet`. freenet-core byte-compares summaries to decide staleness (`contract-summary-determinism.md`).

## UI conventions

Automation targets `data-testid`, never `data-dioxus-id`, which is a render-order index (freenet/river#25). Give every new interactive surface one; it is additive markup only and never changes rendering or logic. Use kebab-case `<surface>-<role>` (`send-message-button`), a plain id for list containers (`room-list`), and `<entity>-item-{id}` for list items (`room-item-{base58(owner_vk)}`, matched by `[data-testid^="room-item-"]`).

`@source "../src/**/*.rs"` in `ui/assets/tailwind.css` is required, or Tailwind v4 drops responsive utilities like `md:flex`.

Freenet share links in message text render as links to the reader's own node. `ui/src/util/share_link.rs` must accept exactly what freenet.org/open and freenet-core's `freenet:` handler accept; all three test against one vector file, copied at `ui/src/util/share-link-vectors.json`, and the `share-link-vectors.yml` workflow fails when it drifts. Change a rule only together with that file and the other two sides.

## Releasing

| Surface | Goes to | How |
|---|---|---|
| River UI | Freenet, contract `raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv` | Run the Playwright suite, then `cargo make publish-river` (`river-publish.md`) |
| riverctl | crates.io and a GitHub release | Bump `cli/Cargo.toml`, merge, push a `riverctl-v<version>` tag; `release-riverctl.yml` publishes. Never wire UI publishing into that workflow |
| Desktop app | GitHub release (draft) | Push a `river-desktop-v<version>` tag (`desktop-release.md`) |

**The publish counter `published-contract/contract-version.txt` only moves forward.** `cargo make sign-webapp` increments it. Commit the bump even when the publish reports a failure: re-using a version forked the site on 2026-08-04.

Check what is live:

```bash
curl -s http://127.0.0.1:7509/v1/contract/web/raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv/ | grep -o 'Built: [^<]*' | head -1
```

## PRs

Conventional Commit titles (`fix(ui): ...`). The body says why, what tests cover the change, and, when touching contracts, any redeploy steps.
