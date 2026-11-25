#!/bin/bash
# Test scripts/ci-published-base.sh together with the two WASM migration gates
# (check-migration.sh, check-room-contract-migration.sh) on throwaway repos.
#
# For each gate it runs eight cases, once with the OLD base (the PR's base
# commit, which for a stacked PR is an unpublished chain tip) and once with the
# NEW base (scripts/ci-published-base.sh), and asserts the NEW verdict. The
# table is printed so a reviewer can read old against new. Cases where the two
# differ are deliberate and marked: a stacked PR is not asked to register
# generations that were never published, and is asked for the published one.
set -uo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
command -v b3sum >/dev/null || { echo "b3sum not found"; exit 2; }
FAILS=0

run_gate() { # script wasm toml
  local script="$1" wasm="$2" toml="$3"
  local T; T="$(mktemp -d)"; trap 'rm -rf "$T"' RETURN
  cd "$T" || exit 3; git init -q -b main .; git config user.email t@t; git config user.name t
  git remote add origin .
  mkdir -p scripts "$(dirname "$wasm")" "$(dirname "$toml")"
  cp "$HERE/$script" "$HERE/ci-published-base.sh" scripts/
  printf 'gen0' > "$wasm"; printf '# registry\n' > "$toml"
  git add -A; git commit -qm main0
  local M; M=$(git rev-parse HEAD)
  hashof() { git show "$1:$wasm" | b3sum --no-names; }
  entry() { printf '\n[[entry]]\ncode_hash = "%s"\n' "$(hashof "$1")" >> "$toml"; }
  commit() { git add -A; git commit -qm "$1"; git rev-parse HEAD; }
  old() { scripts/$script --ci "$1" "$2" >/dev/null 2>&1 && echo PASS || echo FAIL; }
  new() { # base_ref base_sha head
    local b; b=$(scripts/ci-published-base.sh "$1" "$2" "$3" 2>/dev/null) || { echo ERR; return; }
    old "$b" "$3"; }
  check() { # name expected_new old_verdict new_verdict
    local mark=""; [ "$3" != "$4" ] && mark="  (differs, intended)"
    local ok=ok; [ "$4" != "$2" ] && { ok="WRONG (expected $2)"; FAILS=$((FAILS+1)); }
    printf '%-32s %-36s old=%-4s new=%-4s %s%s\n' "$script" "$1" "$3" "$4" "$ok" "$mark"; }

  git checkout -q -B c1 "$M"; printf gen1 > "$wasm"; local H; H=$(commit c1)
  check "1 main-based, unregistered" FAIL "$(old $M $H)" "$(new main $M $H)"

  git checkout -q -B c2 "$M"; entry "$M"; printf gen1 > "$wasm"; H=$(commit c2)
  check "2 main-based, registered" PASS "$(old $M $H)" "$(new main $M $H)"

  git checkout -q -B s1 "$M"; entry "$M"; printf gen1 > "$wasm"; local S1; S1=$(commit s1)
  git checkout -q -B s2 "$S1"; entry "$S1"; printf gen2 > "$wasm"; H=$(commit s2)
  check "3 stack, all registered" PASS "$(old $S1 $H)" "$(new s1 $S1 $H)"

  git checkout -q -B d1 "$M"; printf gen1 > "$wasm"; local D1; D1=$(commit d1)
  git checkout -q -B d2 "$D1"; entry "$D1"; printf gen2 > "$wasm"; H=$(commit d2)
  check "4 stack, published never reg." FAIL "$(old $D1 $H)" "$(new d1 $D1 $H)"

  git checkout -q -B e1 "$M"; entry "$M"; printf gen1 > "$wasm"; local E1; E1=$(commit e1)
  git checkout -q -B e2 "$E1"; printf gen2 > "$wasm"; H=$(commit e2)
  check "5 stack, unpublished mid unreg." PASS "$(old $E1 $H)" "$(new e1 $E1 $H)"

  git checkout -q -B f1 "$M"; entry "$M"; printf gen1 > "$wasm"; local F1; F1=$(commit f1)
  git checkout -q -B f2 "$F1"; printf '# registry\n' > "$toml"; printf gen2 > "$wasm"; H=$(commit f2)
  check "6 registry entries removed" FAIL "$(old $F1 $H)" "$(new main $F1 $H)"

  # 7: main published gen1 (registering gen0); a PR puts gen0 back without registering gen1.
  git checkout -q -B g1 "$M"; entry "$M"; printf gen1 > "$wasm"; local G1; G1=$(commit g1)
  git checkout -q -B g2 "$G1"; printf gen0 > "$wasm"; H=$(commit g2)
  check "7 revert to older, main gen unreg" FAIL "$(old $G1 $H)" "$(new main $G1 $H)"

  # 8: main moves on after the stack branched; the stack is judged against its merge-base.
  git checkout -q -B h1 "$M"; entry "$M"; printf genH > "$wasm"; local H1; H1=$(commit h1)
  git checkout -q -B h2 "$H1"; entry "$H1"; printf genH2 > "$wasm"; H=$(commit h2)
  git checkout -q -B main "$M"; printf genM2 > "$wasm"; commit m2 >/dev/null
  check "8 stale merge-base (known gap)" PASS "$(old $H1 $H)" "$(new h1 $H1 $H)"
  cd "$HERE"
}

run_gate check-migration.sh ui/public/contracts/chat_delegate.wasm legacy_delegates.toml
run_gate check-room-contract-migration.sh ui/public/contracts/room_contract.wasm common/legacy_room_contracts.toml
[ "$FAILS" -eq 0 ] && echo "ci-published-base-test: all cases as expected" || { echo "ci-published-base-test: $FAILS case(s) WRONG"; exit 1; }
