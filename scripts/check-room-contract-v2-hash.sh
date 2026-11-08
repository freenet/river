#!/bin/bash
# Pin the room-state v2 contract's WASM hash.
#
# The v2 contract's key is BLAKE3(BLAKE3(wasm) || params), so any change to a
# byte of room_contract_v2.wasm moves the key of every converted room. Nothing
# else records that, so this script builds the contract and compares its BLAKE3
# hash with contracts/room-contract-v2/wasm.blake3. Changing the pin is a
# deliberate, reviewed diff.
#
# What reaches the WASM: everything in common/src that the contract uses (all
# of room_state_v2 except convert, bench and official_shaped, which are behind
# the `room-state-v2-client` feature, plus encoding and the v1 room_state types
# legacy.rs uses), the contract crate, Cargo.lock and the pinned toolchain.
# Client and tool code belongs in those gated modules, in ui/ or in cli/.
#
# Reproducibility: the build uses the rustc that rust-toolchain.toml pins
# (rustup selects it from the repo directory) and `--locked`. Two more things
# put the machine into the bytes, and both are handled here. Panic messages
# embed absolute source paths, so the build passes --remap-path-prefix for the
# tree, CARGO_HOME and the toolchain. And cargo folds a path dependency's
# absolute path into its symbol hashes, which changes the bytes too, so the
# build runs from a copy of the tree at a fixed path (BUILD_ROOT below), the
# same on every machine. Publishing the v2 contract must use this same recipe.
#
# Usage:
#   scripts/check-room-contract-v2-hash.sh            # build and compare
#   scripts/check-room-contract-v2-hash.sh --update   # rewrite the pin
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PIN="$REPO_ROOT/contracts/room-contract-v2/wasm.blake3"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target-room-contract-v2}"
# Part of the bytes: do not change it without re-pinning.
BUILD_ROOT=/tmp/river-room-contract-v2-pin

die() { echo "ERROR: $*" >&2; exit 1; }
command -v b3sum >/dev/null 2>&1 || die "b3sum not found. Install with: cargo install b3sum --locked"

PINNED=$(sed -n 's/^channel *= *"\(.*\)".*/\1/p' "$REPO_ROOT/rust-toolchain.toml")
[ -n "$PINNED" ] || die "no channel in rust-toolchain.toml"
cd "$REPO_ROOT"
USING=$(rustc --version | awk '{print $2}')
[ "$USING" = "$PINNED" ] || die "rustc $USING would build the WASM, but rust-toolchain.toml pins $PINNED. Run from inside the repo, and unset RUSTUP_TOOLCHAIN."

rm -rf "$BUILD_ROOT"
mkdir -p "$BUILD_ROOT"
tar --exclude=./.git --exclude='./target*' --exclude=node_modules -cf - . | tar -xf - -C "$BUILD_ROOT"
MANIFEST="$BUILD_ROOT/contracts/room-contract-v2/Cargo.toml"

# The client feature must not reach the contract's own (non-dev) build.
if cargo tree --manifest-path "$MANIFEST" --locked --target wasm32-unknown-unknown \
        -e normal,build -f '{p} {f}' | grep -q 'room-state-v2-client'; then
    die "room-state-v2-client is enabled in the contract's build; it would compile convert, bench and official_shaped into the WASM."
fi

SYSROOT="$(rustc --print sysroot)"
CARGO_HOME_DIR="${CARGO_HOME:-$HOME/.cargo}"
export RUSTFLAGS="--remap-path-prefix=$BUILD_ROOT=/river --remap-path-prefix=$CARGO_HOME_DIR=/cargo --remap-path-prefix=$SYSROOT=/rustup"
cargo build --manifest-path "$MANIFEST" --locked --release --target wasm32-unknown-unknown \
    --target-dir "$TARGET_DIR"

WASM="$TARGET_DIR/wasm32-unknown-unknown/release/room_contract_v2.wasm"
[ -f "$WASM" ] || die "build output not found: $WASM"
GOT=$(b3sum "$WASM" | cut -d' ' -f1)

if [ "${1:-}" = "--update" ]; then
    echo "$GOT" > "$PIN"
    echo "Pinned room_contract_v2.wasm hash: $GOT"
    exit 0
fi

WANT=$(tr -d '[:space:]' < "$PIN")
if [ "$GOT" != "$WANT" ]; then
    echo "ERROR: room_contract_v2.wasm hash changed." >&2
    echo "  pinned: $WANT" >&2
    echo "  built:  $GOT" >&2
    echo "A change to the hash moves the key of every converted room. If it is" >&2
    echo "intended, run scripts/check-room-contract-v2-hash.sh --update and commit" >&2
    echo "the new wasm.blake3 in a reviewed diff. If not, a change reached code the" >&2
    echo "contract uses (common/src outside the client-gated modules)." >&2
    exit 1
fi
echo "room_contract_v2.wasm hash matches the pin: $GOT"
