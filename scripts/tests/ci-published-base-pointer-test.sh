#!/bin/bash
# The pointer-freshness gate under the same eight stacked-PR cases as
# ci-published-base-test.sh, on throwaway repos with a throwaway author key.
#
# Needs `pointer-record` on PATH and a throwaway signing key file in
# $POINTER_TEST_KEY (same format as web-container-keys.toml: signing_key and
# verifying_key lines). Without them it exits 0 and says SKIPPED on stderr:
# nothing in CI can mint a key, so this runs by hand when the gate changes.
# Never point it at the real author key.
set -uo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"; ROOT="$(cd "$HERE/.." && pwd)"
KEY="${POINTER_TEST_KEY:-}"
if [ -z "$KEY" ] || [ ! -f "$KEY" ] || ! command -v pointer-record >/dev/null; then
  echo "SKIPPED: set POINTER_TEST_KEY to a THROWAWAY key file and install pointer-record" >&2; exit 0; fi
VK="$(sed -n 's/^verifying_key *= *"\(.*\)"/\1/p' "$KEY" | head -1)"
FAILS=0
T="$(mktemp -d)" || exit 3; trap 'rm -rf "$T"' EXIT; cd "$T" || exit 3
git init -q -b main .; git config user.email t@t; git config user.name t; git remote add origin .
mkdir -p scripts ui/public/contracts
cp "$HERE/check-pointer-freshness.sh" "$HERE/pointer-toml-lib.sh" "$HERE/ci-published-base.sh" scripts/
git -C "$ROOT" show HEAD:pointer-records.toml | sed "s|^author_verifying_key *=.*|author_verifying_key = \"$VK\"|" > pointer-records.toml
printf '%s\n' "$VK" > FREENET.md
printf rc0 > ui/public/contracts/room_contract.wasm; printf dl0 > ui/public/contracts/chat_delegate.wasm
. scripts/pointer-toml-lib.sh
RC=river.room-contract; CD=river.chat-delegate
# setrec APP VERSION: sign APP at VERSION for the WASM currently on disk and write it into the toml.
setrec() {
  local app="$1" ver="$2" i wasm h out st k
  for i in $(seq 1 "$(pointer_record_count pointer-records.toml)"); do
    [ "$(pointer_field pointer-records.toml "$i" app_id)" = "$app" ] && wasm="$(pointer_field pointer-records.toml "$i" wasm_path)"; done
  h="$(b3sum "$wasm" | cut -d' ' -f1)"
  out="$(pointer-record sign --author-vk "$VK" --app-id "$app" --version "$ver" --code-hash "$h" < "$KEY")" || exit 3
  st="$(printf '%s\n' "$out" | sed -n 's/^state=//p')"; k="$(printf '%s\n' "$out" | sed -n 's/^key=//p')"
  python3 - pointer-records.toml "$app" "$ver" "$h" "$st" "$k" <<'PY'
import re,sys
p,app,ver,h,st,k=sys.argv[1:7]; bl=open(p).read().split('[[record]]'); o=[bl[0]]
for b in bl[1:]:
    if re.search(r'^app_id\s*=\s*"%s"\s*$'%re.escape(app),b,re.M):
        b=re.sub(r'^version\s*=.*$','version = %s'%ver,b,count=1,flags=re.M)
        b=re.sub(r'^code_hash\s*=.*$','code_hash = "%s"'%h,b,count=1,flags=re.M)
        b=re.sub(r'^state\s*=.*$','state = "%s"'%st,b,count=1,flags=re.M)
        b=re.sub(r'^pointer_key\s*=.*$','pointer_key = "%s"'%k,b,count=1,flags=re.M)
    o.append(b)
open(p,'w').write('[[record]]'.join(o))
PY
}
both() { setrec $RC "$1"; setrec $CD "$1"; }
commit() { git add -A; git commit -qm "$1"; git rev-parse HEAD; }
old() { scripts/check-pointer-freshness.sh --ci "$1" "$2" >/dev/null 2>&1 && echo PASS || echo FAIL; }
new() { local b; b=$(scripts/ci-published-base.sh "$1" "$2" "$3" 2>/dev/null) || { echo ERR; return; }; old "$b" "$3"; }
check() { local mark=""; [ "$3" != "$4" ] && mark="  (differs, intended)"; local ok=ok
  [ "$4" != "$2" ] && { ok="WRONG (expected $2)"; FAILS=$((FAILS+1)); }
  printf '%-34s old=%-4s new=%-4s %s%s\n' "$1" "$3" "$4" "$ok" "$mark"; }
chg() { printf '%s' "$1" > ui/public/contracts/room_contract.wasm; printf '%s' "$2" > ui/public/contracts/chat_delegate.wasm; }

both 10; M=$(commit main0)
git checkout -q -B c1 $M; chg rc1 dl1; H=$(commit c1)
check "pointer 1 main-based, no re-sign" FAIL "$(old $M $H)" "$(new main $M $H)"
git checkout -q -B c2 $M; chg rc1 dl1; both 11; H=$(commit c2)
check "pointer 2 main-based, re-signed" PASS "$(old $M $H)" "$(new main $M $H)"
git checkout -q -B s1 $M; chg rc1 dl1; both 11; S1=$(commit s1)
git checkout -q -B s2 $S1; chg rc2 dl2; both 12; H=$(commit s2)
check "pointer 3 stack, all re-signed" PASS "$(old $S1 $H)" "$(new s1 $S1 $H)"
git checkout -q -B t1 $M; chg rc1 dl1; both 15; T1=$(commit t1)
git checkout -q -B t2 $T1; chg rc2 dl2; both 12; H=$(commit t2)
check "pointer 4 stack, v above main<tip" PASS "$(old $T1 $H)" "$(new t1 $T1 $H)"
git checkout -q -B u1 $M; chg rc1 dl1; both 11; U1=$(commit u1)
git checkout -q -B u2 $U1; chg rc2 dl2; both 10; H=$(commit u2)
check "pointer 5 stack, v not above main" FAIL "$(old $U1 $H)" "$(new u1 $U1 $H)"
git checkout -q -B v1 $M; chg rc1 dl1; both 11; python3 - <<'PY'
p='pointer-records.toml'; s=open(p).read().split('[[record]]'); open(p,'w').write('[[record]]'.join(s[:2]))
PY
H=$(commit v1)
check "pointer 6 record vanished" FAIL "$(old $M $H)" "$(new main $M $H)"
git checkout -q -B w1 $M; chg rc1 dl1; both 10; H=$(commit w1)
check "pointer 7 main-based, same version" FAIL "$(old $M $H)" "$(new main $M $H)"
git checkout -q -B x1 $M; chg rc1 dl1; both 11; X1=$(commit x1)
git checkout -q -B x2 $X1; chg rc2 dl2; both 12; H=$(commit x2)
git checkout -q -B main $M; chg rcM dlM; both 11; commit main1 >/dev/null
check "pointer 8 stale merge-base (gap)" PASS "$(old $X1 $H)" "$(new x1 $X1 $H)"
[ "$FAILS" -eq 0 ] && echo "pointer matrix: all cases as expected" || { echo "pointer matrix: $FAILS WRONG"; exit 1; }
