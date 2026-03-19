# WASM File Safety — CRITICAL

## Never use `git add -A`, `git add .`, or `git add -u` in this repo

These commands will pick up rebuilt WASM binaries from `ui/public/contracts/` and
`cli/contracts/`. WASM builds are **non-reproducible** — rebuilding produces different
binaries with different hashes, which changes delegate/contract keys and breaks room
data access.

**Always add files by name:**
```bash
# RIGHT
git add ui/src/components/conversation.rs ui/src/util.rs

# WRONG — will pick up any rebuilt WASMs in the working tree
git add -A
git add .
git add -u
```

## Never commit WASM files without migration

If `git status` shows modified `.wasm` files in `ui/public/contracts/` or `cli/contracts/`,
**do not commit them** unless you have:
1. Run `cargo make add-migration` to record the old delegate key
2. Run `cargo make sync-wasm` to intentionally rebuild WASMs
3. Run `cargo test -p river-core --test migration_test` to validate

If you see modified WASMs and did NOT intentionally change them (e.g., `cargo make build`
rebuilt them as a side effect), restore them:
```bash
git checkout HEAD -- ui/public/contracts/ cli/contracts/
```

## Use `cargo make build-ui` instead of `cargo make build` when only UI changed

`cargo make build` rebuilds delegate and contract WASMs. `cargo make build-ui` only
rebuilds the UI WASM, preserving the committed delegate/contract WASMs.

**Use `cargo make build`** only when you intentionally changed delegate, contract, or
common code and need new WASMs.

## March 2026 incident

`git add -A` during a commit amend picked up WASMs that `cargo make build` had rebuilt.
The new WASMs had different delegate keys. Users lost access to room data stored under
the old delegate key. Required emergency migration entry and 3 republishes to fix.
