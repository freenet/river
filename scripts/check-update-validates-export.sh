#!/bin/bash
# Check that a built room contract WASM opts in to `update_validates`: it must
# export `__frnt__update_validates` with signature () -> i32, returning 1.
# freenet-core reads this export to skip its own validate_state of update
# results. Usage: scripts/check-update-validates-export.sh <room_contract.wasm>
set -euo pipefail
wasm="${1:?usage: $0 <room_contract.wasm>}"
fail() { echo "ERROR: $wasm: $1" >&2; exit 1; }

idx=$(wasm-objdump -x -j Export "$wasm" |
    sed -n 's/^ - func\[\([0-9]*\)\] .*-> "__frnt__update_validates"$/\1/p')
[ -n "$idx" ] || fail "no __frnt__update_validates export (is #[contract(update_validates)] present?)"

sig=$(wasm-objdump -x -j Function "$wasm" | sed -n "s/^ - func\[$idx\] sig=\([0-9]*\).*/\1/p")
wasm-objdump -x -j Type "$wasm" | grep -Fq " - type[$sig] () -> i32" ||
    fail "__frnt__update_validates is not () -> i32"

body=$(wasm-objdump -d "$wasm" | sed -n "/^[0-9a-f]* func\[$idx\] /,/^[0-9a-f]* func\[$((idx + 1))\] /p" |
    sed -n 's/^ [0-9a-f]*: [0-9a-f ]*| //p' | head -n 2)
[ "$body" = $'i32.const 1\nend' ] || fail "__frnt__update_validates does not return 1: $body"
echo "OK: $wasm exports __frnt__update_validates () -> i32 returning 1"
