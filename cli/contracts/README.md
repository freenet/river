# riverctl bundled contract

`room_contract.wasm` is a byte-for-byte copy of `ui/public/contracts/room_contract.wasm`.
riverctl derives every room's contract key from it, and crates.io packages need it committed.
`cli/build.rs` embeds the `ui/public/contracts/` copy in a workspace build and this one in a crates.io build.

**Never copy or rebuild it by hand.** A changed WASM changes the contract key.
Run `cargo make sync-wasm` and follow `.claude/rules/delegate-migration.md`.
CI's `check-cli-wasm` fails when the two copies differ.
