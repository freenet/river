#!/usr/bin/env bash
# Migration rehearsal for a River commit that re-keys the chat delegate, the
# room contract, or both. Everything runs on an ISOLATED network-mode node (no
# peers, telemetry off) and under a throwaway web-container key, so nothing
# reaches the real network or the production UI.
#
#   run.sh <commit> [rooms|corpus|ui|mutual|all]      (default: all)
#
#   rooms : rooms built by the OLD riverctl on (a) the live room contract and
#           (b) the one before it (a skipped release), then migrated by
#           <commit>'s riverctl. Judged as element SETS. Skipped when <commit>
#           does not change the room contract.
#   corpus: real room states (REHEARSAL_CORPUS) PUT into <commit>'s room
#           contract. Skipped when REHEARSAL_CORPUS is unset.
#   ui    : the live UI (and the one before it) seeds rooms into its chat
#           delegate; <commit>'s UI is published over it and its migration walk
#           must carry every secret to the new delegate key and every room to
#           the new room contract.
#   mutual: observational, not part of "all": two deputies ban each other on
#           the live room contract, then <commit>'s riverctl migrates the room.
#
# Parameters, prerequisites and what PASS means: README.md beside this file.
# Logs: $REHEARSAL_WORK/logs/<sha8>/ . Summary: SUMMARY-<scenario>.txt there.
set -uo pipefail
SHA=${1:?usage: run.sh <commit> [rooms|corpus|ui|mutual|all]}
WHAT=${2:-all}
case $WHAT in rooms|corpus|ui|mutual|all) ;; *) echo "unknown scenario '$WHAT'" >&2; exit 2 ;; esac
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(git -C "$HERE" rev-parse --show-toplevel)
WORK=$(readlink -f -m "${REHEARSAL_WORK:-${XDG_CACHE_HOME:-$HOME/.cache}/river-rehearsal}")
mkdir -p "$WORK"
for t in git cargo b3sum jq xxd fdev python3 curl ss node npm; do
  command -v "$t" > /dev/null || { echo "missing prerequisite: $t" >&2; exit 1; }
done
source "$HERE/lib.sh"
source "$HERE/make-room.sh"
# Prefix for every compile step (cargo, npm ci, dx build), for example
# "flock /path/build.lock" so builds on a shared machine take turns. Node,
# riverctl and browser phases never run under it.
BUILD=${REHEARSAL_BUILD_WRAPPER:-}
build() { $BUILD "$@"; }   # BUILD is word-split on purpose
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4}
trap stop_node EXIT

# ---------------------------------------------------------------- commits
git -C "$REPO" fetch -q origin || echo "warning: git fetch failed; using local refs" >&2
resolve() { git -C "$REPO" rev-parse --verify -q "$1^{commit}" || { echo "unknown commit '$1'" >&2; return 1; }; }
# The generation before live is the parent of the last commit (up to live) that
# changed the WASM: users who skipped the live release are still on it.
prev_of() {
  local c; c=$(git -C "$REPO" log -1 --format=%H "$LIVE_C" -- "$1")
  [ -n "$c" ] || { echo "no history for $1 before $LIVE_C" >&2; return 1; }
  resolve "$c^"
}
FULL=$(resolve "$SHA") || exit 1
S8=${FULL:0:8}
LIVE_C=$(resolve "${REHEARSAL_LIVE:-origin/main}") || exit 1
PREV_C=$(resolve "${REHEARSAL_PREV_ROOM:-$(prev_of ui/public/contracts/room_contract.wasm)}") || exit 1
PREVD_C=$(resolve "${REHEARSAL_PREV_DELEGATE:-$(prev_of ui/public/contracts/chat_delegate.wasm)}") || exit 1

# Each commit gets the harness's own detached worktree under WORK, never a
# checkout someone else is working in.
wt() { # name commit -> path
  local p=$WORK/wt/$1
  if [ ! -e "$p/.git" ]; then
    git -C "$REPO" worktree add -q --detach "$p" "$2" >&2 || return 1
  elif [ "$(git -C "$p" rev-parse HEAD)" != "$2" ]; then
    [ -z "$(git -C "$p" status --porcelain)" ] || { echo "$p is dirty; refusing to switch" >&2; return 1; }
    git -C "$p" checkout -q --detach "$2" >&2 || return 1
  fi
  echo "$p"
}
NEW=$(wt new "$FULL") || exit 1
LIVE=$(wt live "$LIVE_C") || exit 1
PREV=$(wt prev-room "$PREV_C") || exit 1
if [ "$PREVD_C" = "$PREV_C" ]; then PREVD=$PREV; else PREVD=$(wt prev-delegate "$PREVD_C") || exit 1; fi

L=$WORK/logs/$S8${NODE_TAG:+-$NODE_TAG}; mkdir -p "$L"   # NODE_TAG: set when comparing node binaries
SUM=$L/SUMMARY-$WHAT.txt; : > "$SUM"
say() { echo "$*" | tee -a "$SUM"; }
FAILS=0
fail() { say "   FAIL: $*"; FAILS=$((FAILS+1)); }
progress "run.sh $S8 ($WHAT) started"

# ---------------------------------------------------------------- build
HOST=$(rustc -vV | sed -n 's/^host: //p')
for w in $(printf '%s\n' "$NEW" "$LIVE" "$PREV" "$PREVD" | sort -u); do
  (cd "$w" && build cargo build -q --release --locked -p riverctl) >> "$L/build-riverctl.log" 2>&1 \
    || { echo "riverctl build failed in $w (see $L/build-riverctl.log)"; exit 1; }
done
(cd "$NEW" && build cargo build -q --release --locked -p web-container-tool --target "$HOST" --target-dir target/native) \
  > "$L/build-wct.log" 2>&1 || { echo "web-container-tool build failed (see $L/build-wct.log)"; exit 1; }
WCT_BIN=$NEW/target/native/$HOST/release/web-container-tool
# rinspect decodes states with the river-core of the commit under rehearsal.
RI=$WORK/rinspect
mkdir -p "$RI"; rm -rf "$RI/src"; cp -r "$HERE/rinspect/src" "$RI/"
[ -f "$RI/Cargo.lock" ] || cp "$HERE/rinspect/Cargo.lock" "$RI/"
sed "s|path = \"../../../common\"|path = \"$NEW/common\"|" "$HERE/rinspect/Cargo.toml" > "$RI/Cargo.toml"
grep -qF "path = \"$NEW/common\"" "$RI/Cargo.toml" || { echo "could not point rinspect at $NEW/common"; exit 1; }
(cd "$RI" && build cargo build -q --release) > "$L/build-rinspect.log" 2>&1 || { echo "rinspect build failed (see $L/build-rinspect.log)"; exit 1; }
I=$RI/target/release/rinspect
# A throwaway web-container key, generated once per work dir. Never a real key.
KEYS=$WORK/staging-keys.toml
PARAMS=$WORK/webapp.parameters
[ -f "$KEYS" ] || "$WCT_BIN" generate --output "$KEYS" > /dev/null || { echo "key generation failed"; exit 1; }
[ -f "$PARAMS" ] || "$WCT_BIN" export-parameters --parameters "$PARAMS" --key-file "$KEYS" > /dev/null || { echo "export-parameters failed"; exit 1; }
export WCT=$WCT_BIN KEYS PARAMS
if [ "$WHAT" = ui ] || [ "$WHAT" = all ]; then
  command -v dx > /dev/null || [ -x "$HOME/.cargo/bin/dx" ] || { echo "missing prerequisite: dx (dioxus-cli)"; exit 1; }
  python3 -c 'import cbor2' 2> /dev/null || { echo "missing prerequisite: python3 cbor2 module"; exit 1; }
  PW=$WORK/pw; mkdir -p "$PW"; cp "$HERE/pw/package.json" "$HERE/pw/package-lock.json" "$PW/"
  { [ -d "$PW/node_modules/playwright" ] || (cd "$PW" && build npm ci --silent); } > "$L/playwright.log" 2>&1 \
    && (cd "$PW" && npx playwright install chromium) >> "$L/playwright.log" 2>&1 \
    || { echo "playwright setup failed (see $L/playwright.log)"; exit 1; }
  export PLAYWRIGHT_MODULE=$PW/node_modules/playwright
fi
RC_NEW=$NEW/target/release/riverctl
RC_LIVE=$LIVE/target/release/riverctl
RC_PREV=$PREV/target/release/riverctl
RC_PREVD=$PREVD/target/release/riverctl
ROOM_WASM=$NEW/ui/public/contracts/room_contract.wasm
DEL_WASM=$NEW/ui/public/contracts/chat_delegate.wasm
h8() { b3sum --no-names "$1" | cut -c1-8; }
NEW_ROOM8=$(h8 "$ROOM_WASM")
LIVE_ROOM8=$(h8 "$LIVE/ui/public/contracts/room_contract.wasm")
PREV_ROOM8=$(h8 "$PREV/ui/public/contracts/room_contract.wasm")

say "== rehearsal of $FULL ($(date '+%F %H:%M %Z'))"
say "   live $LIVE_C, previous room generation $PREV_C, previous delegate generation $PREVD_C"
say "   room contract $(b3sum --no-names "$ROOM_WASM") (cli copy $(h8 "$NEW/cli/contracts/room_contract.wasm")); live $LIVE_ROOM8, previous $PREV_ROOM8"
say "   chat delegate $(b3sum --no-names "$DEL_WASM")"
say "   riverctl $("$RC_NEW" --version), node $("$FREENET_BIN" --version | awk 'NR==1') ($FREENET_BIN, b3 $(h8 "$FREENET_BIN"))"
# Delegate keys three ways: stdlib, raw b3sum of the code hash, registry row.
for pair in "new:$DEL_WASM" "live:$LIVE/ui/public/contracts/chat_delegate.wasm" "prev:$PREVD/ui/public/contracts/chat_delegate.wasm"; do
  n=${pair%%:*}; f=${pair#*:}
  std=$("$I" delegate-key "$f" | awk '{print $2}')
  raw=$(b3sum --no-names "$f" | xxd -r -p | b3sum --no-names)
  reg=$(grep -B0 "delegate_key = \"$raw\"" "$NEW/legacy_delegates.toml" > /dev/null && echo in-registry || echo not-in-registry)
  say "   delegate[$n] code $(b3sum --no-names "$f" | cut -c1-8) key $raw $([ "$std" = "$raw" ] && echo stdlib-agrees || echo STDLIB-DISAGREES) $reg"
  [ "$std" = "$raw" ] || fail "delegate key derivation disagrees for $n"
done
# Likewise the live delegate must be registered only when this commit re-keys it.
DELEGATE_CHANGED=
if [ "$(b3sum --no-names "$LIVE/ui/public/contracts/chat_delegate.wasm")" != "$(b3sum --no-names "$DEL_WASM")" ]; then
  DELEGATE_CHANGED=1
  grep -q "$(b3sum --no-names "$LIVE/ui/public/contracts/chat_delegate.wasm" | xxd -r -p | b3sum --no-names)" "$NEW/legacy_delegates.toml" || fail "live delegate is not in the registry"
else
  say "   chat delegate unchanged from live: no delegate registry entry expected"
fi
# The live room contract must be registered as legacy only when this commit
# re-keys it; a delegate-only change leaves it byte-identical and current.
if [ "$(b3sum --no-names "$LIVE/ui/public/contracts/room_contract.wasm")" != "$(b3sum --no-names "$ROOM_WASM")" ]; then
  grep -q "$(b3sum --no-names "$LIVE/ui/public/contracts/room_contract.wasm")" "$NEW/common/legacy_room_contracts.toml" || fail "live room contract is not in the room registry"
else
  say "   room contract unchanged from live: no room registry entry expected"
fi
grep -q "$(b3sum --no-names "$ROOM_WASM")" "$NEW/common/legacy_room_contracts.toml" && fail "the NEW room contract is already listed as legacy"

gen_id() { # owner-b58 code-hash8 -> contract id under that generation
  "$I" keys "$1" "$ROOM_WASM" | awk -v c="$2" '$2==c {print $3}'
}

# ---------------------------------------------------------------- rooms
compare_states() { # old.json new.json label -> 0 if every old element is in new
  python3 - "$1" "$2" "$3" <<'PY'
import json, sys
o = json.load(open(sys.argv[1])); n = json.load(open(sys.argv[2])); label = sys.argv[3]
bad = 0
for k in sorted(set(o["sets"]) | set(n["sets"])):
    a = set(o["sets"].get(k, [])); b = set(n["sets"].get(k, []))
    lost = a - b; gained = b - a
    if a or b:
        print(f"      {k:45s} old {len(a):3d} new {len(b):3d} lost {len(lost)} gained {len(gained)}")
    if lost:
        bad += 1
print(f"      bytes old {o['bytes']} new {n['bytes']}; config v{o['configuration_version']} -> v{n['configuration_version']}; identical={o['blake3']==n['blake3']}")
sys.exit(1 if bad else 0)
PY
}

room_scenario() { # name old-riverctl old-gen-hash8 [skipped-gen-hash8]
  local name=$1 rc_old=$2 gen=$3 skipped=${4:-} d=$L/rooms-$1
  say "== rooms/$name: room built by $("$rc_old" --version) on $gen${skipped:+, $skipped never written (skipped release)}"
  start_node "$d/node" >> "$SUM"; tail -n 1 "$SUM"
  make_room "$rc_old" "$d/cfg" "$name" | tee -a "$SUM"
  local room; room=$(cat "$d/cfg/room.txt" 2>/dev/null)
  [ -n "$room" ] || { fail "room fixture not built"; stop_node; return; }
  local old_id new_id; old_id=$(gen_id "$room" "$gen"); new_id=$(gen_id "$room" "$(b3sum --no-names "$ROOM_WASM" | cut -c1-8)")
  local stored; stored=$(jq -r --arg r "$room" '.rooms[$r].contract_key' "$d/cfg/owner/rooms.json")
  [ "$stored" = "$old_id" ] && say "   fixture is genuine: owner's rooms.json key $stored = derived $gen key" || fail "owner rooms.json key $stored != derived $gen key $old_id"
  "$I" get "$URL" "$old_id" "$d/old.cbor" > "$d/get-old.txt"; say "   GET old $gen: $(cat "$d/get-old.txt")"
  "$I" state "$d/old.cbor" > "$d/old.json" || { fail "old state undecodable"; stop_node; return; }
  say "   old state counts: $(jq -c .counts "$d/old.json")"
  "$I" get "$URL" "$new_id" "$d/new-before.cbor" > "$d/get-new-before.txt"
  say "   GET new key BEFORE migration: $(cat "$d/get-new-before.txt")"
  grep -q '^STATE' "$d/get-new-before.txt" && fail "new key already held state before migration (fixture cannot show a migration)"
  if [ -n "$skipped" ]; then
    local sk_id; sk_id=$(gen_id "$room" "$skipped")
    "$I" get "$URL" "$sk_id" "$d/skipped.cbor" > "$d/get-skipped.txt"; say "   GET skipped gen $skipped: $(cat "$d/get-skipped.txt")"
    grep -q '^STATE' "$d/get-skipped.txt" && fail "skipped generation holds state; not a skipped release"
  fi
  # Item 3: riverctl from <sha> migrates each config dir. A plain member first
  # (any member may migrate), then the owner, the deputy, and a banned member.
  local who rc
  for who in m2 owner m1 m4; do
    local t0=$SECONDS
    timeout 300 "$RC_NEW" --node-url "$URL" --config-dir "$d/cfg/$who" -d -f json message list "$room" > "$d/new-$who.out" 2> "$d/new-$who.log"; rc=$?
    local k; k=$(jq -r --arg r "$room" '.rooms[$r].contract_key' "$d/cfg/$who/rooms.json" 2>/dev/null)
    local nmsg; nmsg=$(jq 'if type=="array" then length elif .messages then (.messages|length) else "?" end' "$d/new-$who.out" 2>/dev/null)
    say "   riverctl-new message list as $who: exit $rc in $((SECONDS-t0))s, $nmsg messages, rooms.json key now $([ "$k" = "$new_id" ] && echo NEW || echo "$k")"
    grep -hE "missing contract|not found|Room not found|Pointer GET|probing|Recovered room|migrat" "$d/new-$who.log" | sed -E 's/^.*(INFO|WARN|ERROR|DEBUG) //' | cut -c1-200 | sort | uniq -c | sort -rn | head -8 | sed 's/^/      /' >> "$SUM"
    if [ "$who" != m4 ]; then
      [ "$rc" = 0 ] || fail "riverctl-new exited $rc as $who"
      [ "$k" = "$new_id" ] || fail "$who's rooms.json not moved to the new key"
    fi
    if [ "$who" = m2 ]; then
      # POPULATED SUCCESSOR: the new key now holds state. m2 writes there with
      # riverctl-new; the owner, still on the old riverctl, writes LATE to the
      # old key. The later migrations must merge into the populated new key
      # and lose neither write.
      timeout 120 "$RC_NEW" --node-url "$URL" --config-dir "$d/cfg/m2" -f json message send "$room" "$name: m2 on the NEW key" > "$d/m2-new-post.out" 2>> "$d/m2-new-post.log"
      say "   populated successor: m2 posted on the new key with riverctl-new (exit $?)"
      timeout 120 "$rc_old" --node-url "$URL" --config-dir "$d/cfg/owner" -f json message send "$room" "$name: owner LATE on the old key" > "$d/owner-late.out" 2>> "$d/owner-late.log"
      say "   late write: owner posted on the OLD key with $("$rc_old" --version) (exit $?)"
      "$I" get "$URL" "$old_id" "$d/old.cbor" > /dev/null; "$I" state "$d/old.cbor" > "$d/old.json"
      "$I" get "$URL" "$new_id" "$d/new-populated.cbor" > /dev/null; "$I" state "$d/new-populated.cbor" > "$d/new-populated.json"
      say "   old state now $(jq -c .counts "$d/old.json")"
      say "   new key before the remaining migrations $(jq -c .counts "$d/new-populated.json")"
    fi
  done
  "$I" get "$URL" "$new_id" "$d/new.cbor" > "$d/get-new.txt"; say "   GET new key AFTER: $(cat "$d/get-new.txt")"
  if "$I" state "$d/new.cbor" > "$d/new.json" 2>/dev/null; then
    if compare_states "$d/old.json" "$d/new.json" "$name" | tee -a "$SUM"; then say "   PASS: every old element is in the migrated state"; else fail "elements lost in migration"; fi
    say "   successor-before vs successor-after (the populated successor must keep its own writes):"
    if compare_states "$d/new-populated.json" "$d/new.json" "$name" | tee -a "$SUM"; then say "   PASS: nothing the populated successor held was lost"; else fail "the populated successor lost elements"; fi
  else
    fail "no migrated state at the new key"
  fi
  say "   peer connections: $(peer_connections)"
  [ "$(peer_connections)" = 0 ] || fail "node was not isolated"
  stop_node
}

# ---------------------------------------------------------------- corpus
corpus_scenario() {
  if [ -z "${REHEARSAL_CORPUS:-}" ]; then say "== corpus: skipped (REHEARSAL_CORPUS is not set)"; return; fi
  local corpus d=$L/corpus; corpus=$(readlink -f "$REHEARSAL_CORPUS"); mkdir -p "$d"
  [ -f "$corpus/params.cbor" ] || { fail "corpus has no params.cbor ($corpus)"; return; }
  say "== corpus: room states from $corpus PUT into $NEW_ROOM8"
  start_node "$d/node" >> "$SUM"; tail -n 1 "$SUM"
  local owner; owner=$(python3 -c "
import sys
b=open(sys.argv[1],'rb').read()
alpha='123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz'
vk=b[-32:]; n=int.from_bytes(vk,'big'); s=''
while n: n,r=divmod(n,58); s=alpha[r]+s
print('1'*(len(vk)-len(vk.lstrip(b'\0')))+s)" "$corpus/params.cbor")
  local new_id; new_id=$(gen_id "$owner" "$NEW_ROOM8")
  say "   corpus room owner $owner -> new id $new_id"
  local f n=0
  for f in $(cd "$corpus" && ls ./*.cbor | sed 's|^\./||; s|\.cbor$||' | grep -vx params); do
    n=$((n+1))
    # Fresh node per state so each PUT is an initial PUT, not a merge.
    [ $n -gt 1 ] && { stop_node; start_node "$d/node-$f" >> "$SUM"; }
    "$I" state "$corpus/$f.cbor" > "$d/$f.in.json"
    fdev -p "$WS_PORT" publish --code "$ROOM_WASM" --parameters "$corpus/params.cbor" contract --state "$corpus/$f.cbor" > "$d/$f.put.log" 2>&1
    local ok; ok=$(grep -cE "published successfully" "$d/$f.put.log")
    "$I" get "$URL" "$new_id" "$d/$f.out.cbor" > "$d/$f.get.txt"
    say "   $f ($(stat -c %s "$corpus/$f.cbor") bytes): PUT $([ "$ok" -ge 1 ] && echo accepted || echo REJECTED), GET back: $(cat "$d/$f.get.txt")"
    if grep -q '^STATE' "$d/$f.get.txt"; then
      "$I" state "$d/$f.out.cbor" > "$d/$f.out.json"
      compare_states "$d/$f.in.json" "$d/$f.out.json" "$f" | tee -a "$SUM" || fail "$f lost elements on PUT"
    else
      fail "$f did not validate in the new contract"; grep -iE "error|invalid" "$d/$f.put.log" | head -3 | sed 's/^/      /' >> "$SUM"
    fi
  done
  [ "$n" -gt 0 ] || fail "corpus $corpus holds no states besides params.cbor"
  stop_node
  # Optional: a state the commit's room contract must REFUSE as an initial PUT
  # (for a change that tightens validation). REHEARSAL_REJECT is a directory
  # holding params.cbor and state.cbor.
  if [ -n "${REHEARSAL_REJECT:-}" ]; then
    start_node "$d/node-reject" >> "$SUM"
    fdev -p "$WS_PORT" publish --code "$ROOM_WASM" --parameters "$REHEARSAL_REJECT/params.cbor" contract --state "$REHEARSAL_REJECT/state.cbor" > "$d/reject.put.log" 2>&1
    if grep -qE "published successfully" "$d/reject.put.log"; then fail "the REHEARSAL_REJECT state was accepted as an initial PUT"
    else say "   REHEARSAL_REJECT state, initial PUT: REFUSED as expected ($(grep -oE 'rror.{0,120}' "$d/reject.put.log" | head -1))"; fi
    stop_node
  fi
}

# ---------------------------------------------------------------- ui
ui_scenario() { # name old-ui-dir old-riverctl old-delegate-wasm [skipped-delegate-wasm]
  local name=$1 olddir=$2 rc_old=$3 oldwasm=$4 skippedwasm=${5:-} d=$L/ui-$1
  local newdir=$WORK/ui-new-$S8
  mkdir -p "$d"
  local cid; cid=$(fdev get-contract-id --code "$NEW/published-contract/web_container_contract.wasm" --parameters "$PARAMS")
  local page="http://127.0.0.1:$WS_PORT/v1/contract/web/$cid/"
  local oldkey newkey; oldkey=$(b3sum --no-names "$oldwasm" | xxd -r -p | b3sum --no-names); newkey=$(b3sum --no-names "$DEL_WASM" | xxd -r -p | b3sum --no-names)
  say "== ui/$name: $(cat "$olddir/commit.txt" | cut -c1-8) UI seeds delegate ${oldkey:0:8}; $S8 UI must walk it to ${newkey:0:8}"
  start_node "$d/node" >> "$SUM"; tail -n 1 "$SUM"
  publish_ui() { fdev -p "$WS_PORT" publish --code "$NEW/published-contract/web_container_contract.wasm" --parameters "$PARAMS" contract --webapp-archive "$1/webapp.tar.xz" --webapp-metadata "$1/webapp.metadata" > "$2" 2>&1; grep -qE "(published|updated) successfully" "$2"; }
  token() { curl -s "$page" | grep -oE 'freenetBridge\("[1-9A-HJ-NP-Za-km-z]+"' | head -1 | cut -d'"' -f2; }
  # A room with other members, built by the old riverctl, for the UI to join.
  make_room "$rc_old" "$d/cfg" "ui-$name" | tee -a "$SUM"
  local room code; room=$(cat "$d/cfg/room.txt")
  code=$(timeout 120 "$rc_old" --node-url "$URL" --config-dir "$d/cfg/owner" -f json invite create "$room" 2>/dev/null | jq -r .invitation_code)
  publish_ui "$olddir" "$d/publish-old.log" || { fail "old UI publish failed"; stop_node; return; }
  node "$HERE/ui.js" seed "$page" "$d/seed" "UI room $name" "$code"; local src=$?
  say "   seed: exit $src, rooms shown $(jq -c '[.rooms[]?.text]' "$d/seed.json")"
  [ $src = 0 ] || fail "seeding through the old UI failed (see $d/seed.log)"
  "$I" dump "$URL&authToken=$(token)" "$oldwasm" "$d/dump-old.json" > "$d/dump-old.txt" 2>&1
  say "   old delegate ${oldkey:0:8}: $(head -1 "$d/dump-old.txt")"; sed -n '2,$p' "$d/dump-old.txt" | sed 's/^/   /' >> "$SUM"
  secret_listing "$d/secrets-before.txt"
  say "   secret dirs before: $(tr '\n' ' ' < "$d/secrets-before.txt")"
  grep -q "$(key_dirname "$oldkey")" "$d/secrets-before.txt" || fail "old delegate has no secret dir before migration"
  grep -q "$(key_dirname "$newkey")" "$d/secrets-before.txt" && fail "NEW delegate already had a secret dir before migration"
  if [ -n "$skippedwasm" ]; then
    local sk; sk=$(b3sum --no-names "$skippedwasm" | xxd -r -p | b3sum --no-names)
    grep -q "$(key_dirname "$sk")" "$d/secrets-before.txt" && fail "skipped delegate ${sk:0:8} is present; not a skipped release"
    say "   skipped delegate ${sk:0:8}: $(grep -q "$(key_dirname "$sk")" "$d/secrets-before.txt" && echo PRESENT || echo absent, as intended)"
  fi
  publish_ui "$newdir" "$d/publish-new.log" || { fail "new UI publish failed"; stop_node; return; }
  node "$HERE/ui.js" load "$page" "$d/load1" 240; local lrc=$?
  say "   new UI load 1: exit $lrc, walk reported $(jq .walk_reported "$d/load1.json"), rooms shown $(jq -c '[.rooms[]?.text]' "$d/load1.json")"
  jq -r '.walk_lines[]' "$d/load1.json" | sed -E 's/^\[[0-9.]+s\] \[[a-z]+\] //' | cut -c1-260 | sed 's/^/      /' >> "$SUM"
  # The walk must reach a verdict on EVERY registered generation (a walk that
  # stops is a failure) and must import the generation that holds the data.
  local ngen verdicts seeded_gen
  ngen=$(grep -c '^\[\[entry\]\]' "$NEW/legacy_delegates.toml")
  verdicts=$(grep -oE "crate-migration predecessor gen [0-9]+" "$d/load1.log" | sort -u | wc -l)
  seeded_gen=$(awk -v k="$oldkey" '/^\[\[entry\]\]/{n++} index($0,k){print n-1}' "$NEW/legacy_delegates.toml")
  say "   walk: verdicts on $verdicts of $ngen registered generations; seeded generation is gen $seeded_gen"
  [ "$verdicts" = "$ngen" ] || fail "the walk stopped: verdicts on $verdicts of $ngen generations"
  grep -qE "crate-migration predecessor gen $seeded_gen: Imported" "$d/load1.log" || fail "gen $seeded_gen (the one holding the data) was not Imported"
  local unresp; unresp=$(grep -cE "crate-migration predecessor gen [0-9]+: Unresponsive" "$d/load1.log")
  say "   walk classified $unresp generation(s) Unresponsive; final line: $(grep -oE 'crate-migration: walk [a-z ]+' "$d/load1.log" | tail -1)"
  # The UI must also carry each room's CONTRACT state to the new room contract.
  local rk oldgen8 new8; oldgen8=$(b3sum --no-names "$olddir/room_contract.wasm" | cut -c1-8); new8=$(b3sum --no-names "$ROOM_WASM" | cut -c1-8)
  for rk in $(jq -r '.rooms[].key' "$d/seed.json"); do
    "$I" get "$URL" "$(gen_id "$rk" "$oldgen8")" "$d/room-$rk-old.cbor" > /dev/null
    "$I" get "$URL" "$(gen_id "$rk" "$new8")" "$d/room-$rk-new.cbor" > "$d/room-$rk-new.txt"
    say "   room ${rk:0:8}: $oldgen8 -> $new8 after UI load: $(cat "$d/room-$rk-new.txt")"
    if grep -q '^STATE' "$d/room-$rk-new.txt"; then
      "$I" state "$d/room-$rk-old.cbor" > "$d/room-$rk-old.json"; "$I" state "$d/room-$rk-new.cbor" > "$d/room-$rk-new.json"
      compare_states "$d/room-$rk-old.json" "$d/room-$rk-new.json" x | tee -a "$SUM" > /dev/null || fail "room ${rk:0:8} lost elements in the UI's forward migration"
    else
      fail "UI did not migrate room ${rk:0:8} to the new room contract"
    fi
  done
  "$I" dump "$URL&authToken=$(token)" "$DEL_WASM" "$d/dump-new.json" > "$d/dump-new.txt" 2>&1
  say "   new delegate ${newkey:0:8}: $(head -1 "$d/dump-new.txt")"; sed -n '2,$p' "$d/dump-new.txt" | sed 's/^/   /' >> "$SUM"
  secret_listing "$d/secrets-after.txt"
  say "   secret dirs after: $(tr '\n' ' ' < "$d/secrets-after.txt")"
  python3 "$HERE/compare_delegates.py" "$d/dump-old.json" "$d/dump-new.json" | tee -a "$SUM" || fail "delegate data did not all arrive"
  local nold nnew; nold=$(jq '.rooms|length' "$d/seed.json"); nnew=$(jq '.rooms|length' "$d/load1.json")
  [ "$nnew" -ge "$nold" ] || fail "UI shows $nnew rooms after migration, $nold before"
  # A second load: markers must short-circuit, rooms must still be there.
  node "$HERE/ui.js" load "$page" "$d/load2" 150
  say "   new UI load 2: rooms shown $(jq -c '[.rooms[]?.text]' "$d/load2.json")"
  jq -r '.walk_lines[]' "$d/load2.json" | sed -E 's/^\[[0-9.]+s\] \[[a-z]+\] //' | cut -c1-200 | sed 's/^/      /' >> "$SUM"
  [ "$name" = newest ] && ui_populated_and_emptied "$d" "$olddir" "$newdir" "$oldwasm" "$page" "$oldkey" "$newkey"
  say "   peer connections: $(peer_connections)"
  [ "$(peer_connections)" = 0 ] || fail "node was not isolated"
  stop_node
}


# Re-sign an already-built bundle at another version (the node only accepts a
# higher version), so the same UI can be published again over the other one.
resign() { # src-dir version out-dir
  mkdir -p "$3"; cp "$1/webapp.tar.xz" "$3/"
  "$WCT_BIN" sign --input "$3/webapp.tar.xz" --output "$3/webapp.metadata" --parameters "$PARAMS" \
    --version "$2" --key-file "$KEYS" > "$3/sign.log" 2>&1
}

# (a) POPULATED SUCCESSOR and (b) EMPTIED SUCCESSOR, after the newest-release
# migration has run on this node. Judged by arithmetic on delegate dumps.
ui_populated_and_emptied() { # d olddir newdir oldwasm page oldkey newkey
  local d=$1 olddir=$2 newdir=$3 oldwasm=$4 page=$5 oldkey=$6 newkey=$7 i
  say "== ui/populated: the successor already holds the migrated rooms; it gets a room of its own, then an old tab writes late"
  node "$HERE/ui.js" seed "$page" "$d/succ" "Successor-only room"; say "   new UI created a successor-only room: exit $?, rooms shown $(jq -c '[.rooms[]?.text]' "$d/succ.json")"
  "$I" dump "$URL&authToken=$(token)" "$DEL_WASM" "$d/dump-succ.json" > "$d/dump-succ.txt" 2>&1; say "   successor before the late write: $(head -1 "$d/dump-succ.txt")"
  resign "$olddir" 10 "$d/old-v10"; publish_ui "$d/old-v10" "$d/publish-old-v10.log" || fail "re-publishing the old UI failed"
  node "$HERE/ui.js" seed "$page" "$d/late" "Late old-tab room"; say "   old UI (v10) created a LATE room in the predecessor: exit $?, rooms shown $(jq -c '[.rooms[]?.text]' "$d/late.json")"
  "$I" dump "$URL&authToken=$(token)" "$oldwasm" "$d/dump-old-late.json" > "$d/dump-old-late.txt" 2>&1; say "   predecessor after the late write: $(head -1 "$d/dump-old-late.txt")"
  resign "$newdir" 11 "$d/new-v11"; publish_ui "$d/new-v11" "$d/publish-new-v11.log" || fail "re-publishing the new UI failed"
  local topped=0
  for i in 1 2 3 4 5 6; do
    node "$HERE/ui.js" load "$page" "$d/topup$i" 200
    "$I" dump "$URL&authToken=$(token)" "$DEL_WASM" "$d/dump-topup$i.json" > "$d/dump-topup$i.txt" 2>&1
    say "   top-up load $i: rooms shown $(jq -c '[.rooms[]?.text]' "$d/topup$i.json"); successor $(head -1 "$d/dump-topup$i.txt")"
    if python3 "$HERE/compare_delegates.py" "$d/dump-old-late.json" "$d/dump-topup$i.json" > "$d/cmp-topup$i.txt" 2>&1; then topped=$i; break; fi
  done
  if [ "$topped" -gt 0 ]; then
    say "   PASS: every predecessor value (including the late room) reached the populated successor on load $topped:"; sed 's/^/   /' "$d/cmp-topup$topped.txt" >> "$SUM"
  else
    fail "the late predecessor write never reached the populated successor in 6 loads"; sed 's/^/   /' "$d/cmp-topup6.txt" >> "$SUM"; topped=6
  fi
  say "   successor-only data kept (successor before the late write vs after the top-up):"
  python3 "$HERE/compare_delegates.py" "$d/dump-succ.json" "$d/dump-topup$topped.json" | tee -a "$SUM" || fail "the populated successor lost its own data in the top-up"

  say "== ui/emptied: stop the node, move the successor delegate's secrets aside, restart, load the new UI"
  local sdir; sdir=$(find "$NODE_DIR/data/secrets" -mindepth 1 -maxdepth 2 -type d -name "$(key_dirname "$newkey")" | head -1)
  [ -n "$sdir" ] || { fail "successor secret dir not found"; return; }
  local nd=$NODE_DIR; stop_node
  mv "$sdir" "$d/successor-secrets-moved-aside"
  KEEP_DATA=1 start_node "$nd" >> "$SUM"; tail -n 1 "$SUM"
  secret_listing "$d/secrets-emptied.txt"; say "   secret dirs after emptying: $(tr '\n' ' ' < "$d/secrets-emptied.txt")"
  grep -q "$(key_dirname "$newkey")" "$d/secrets-emptied.txt" && fail "successor secrets still present after emptying"
  local ok=0
  for i in 1 2 3 4; do
    node "$HERE/ui.js" load "$page" "$d/emptied$i" 240
    "$I" dump "$URL&authToken=$(token)" "$DEL_WASM" "$d/dump-emptied$i.json" > "$d/dump-emptied$i.txt" 2>&1
    say "   emptied load $i: rooms shown $(jq -c '[.rooms[]?.text]' "$d/emptied$i.json"); successor $(head -1 "$d/dump-emptied$i.txt")"
    if python3 "$HERE/compare_delegates.py" "$d/dump-old-late.json" "$d/dump-emptied$i.json" > "$d/cmp-emptied$i.txt" 2>&1; then ok=$i; break; fi
  done
  if [ "$ok" -gt 0 ]; then
    say "   PASS: the emptied successor recovered every predecessor value on load $ok:"; sed 's/^/   /' "$d/cmp-emptied$ok.txt" >> "$SUM"
  else
    fail "the emptied successor did not recover the predecessor's values in 4 loads"; sed 's/^/   /' "$d/cmp-emptied4.txt" >> "$SUM"
  fi
  say "   (the successor-only room lived only in the moved-aside secrets; its absence after emptying is expected, not a loss)"
}

# Observational (reports, does not assert): a mutual ban
# between two deputies on the OLD contract, then migrated by riverctl-new.
mutual_scenario() {
  local d=$L/rooms-mutual
  say "== rooms/mutual: two deputies ban each other on $LIVE_ROOM8 ($("$RC_LIVE" --version)), then riverctl-new migrates"
  start_node "$d/node" >> "$SUM"; tail -n 1 "$SUM"
  make_mutual_room "$RC_LIVE" "$d/cfg" mutual | tee -a "$SUM"
  local room; room=$(cat "$d/cfg/room.txt" 2>/dev/null); [ -n "$room" ] || { fail "mutual fixture not built"; stop_node; return; }
  read -r d1 d2 m < "$d/cfg/ids.txt"
  local old_id new_id; old_id=$(gen_id "$room" "$LIVE_ROOM8"); new_id=$(gen_id "$room" "$(b3sum --no-names "$ROOM_WASM" | cut -c1-8)")
  "$I" get "$URL" "$old_id" "$d/old.cbor" > /dev/null; "$I" state "$d/old.cbor" > "$d/old.json"
  say "   OLD $LIVE_ROOM8 state: $(jq -c .counts "$d/old.json")"
  "$I" members "$d/old.cbor" "$room" > "$d/old-members.txt" 2>&1; sed 's/^/      old: /' "$d/old-members.txt" >> "$SUM"
  local who rc
  for who in m owner d1 d2; do
    timeout 300 "$RC_NEW" --node-url "$URL" --config-dir "$d/cfg/$who" -f json member list "$room" > "$d/new-list-$who.out" 2> "$d/new-list-$who.log"; rc=$?
    say "   riverctl-new member list as $who: exit $rc, shows $(jq -c '[.[]?.nickname]' "$d/new-list-$who.out" 2>/dev/null)"
  done
  "$I" get "$URL" "$new_id" "$d/new.cbor" > "$d/get-new.txt"; say "   GET new key: $(cat "$d/get-new.txt")"
  if "$I" state "$d/new.cbor" > "$d/new.json" 2>/dev/null; then
    compare_states "$d/old.json" "$d/new.json" mutual | tee -a "$SUM" || fail "mutual room lost elements in migration"
    "$I" members "$d/new.cbor" "$room" > "$d/new-members.txt" 2>&1; sed 's/^/      new: /' "$d/new-members.txt" >> "$SUM"
  else fail "mutual room not migrated"; fi
  for who in d1 d2; do
    timeout 120 "$RC_NEW" --node-url "$URL" --config-dir "$d/cfg/$who" -f json message send "$room" "after migration: $who speaks" > "$d/send-$who.out" 2> "$d/send-$who.log"; rc=$?
    say "   $who message send after migration: exit $rc $(tail -c 200 "$d/send-$who.out" "$d/send-$who.log" 2>/dev/null | tr '\n' ' ' | grep -oE '(error|Error|banned|not a member)[^\"]{0,100}' | head -1)"
  done
  timeout 120 "$RC_NEW" --node-url "$URL" --config-dir "$d/cfg/d1" -f json dm send "$room" "$m" "after migration: d1 to m" > "$d/dm-d1.out" 2> "$d/dm-d1.log"; rc=$?
  say "   d1 dm send to m after migration: exit $rc $(tail -c 300 "$d/dm-d1.log" | tr '\n' ' ' | grep -oE '(error|Error)[^\"]{0,120}' | head -1)"
  timeout 120 "$RC_NEW" --node-url "$URL" --config-dir "$d/cfg/m" -f json dm send "$room" "$d1" "after migration: m to d1" > "$d/dm-m.out" 2> "$d/dm-m.log"; rc=$?
  say "   m dm send to d1 after migration: exit $rc $(tail -c 300 "$d/dm-m.log" | tr '\n' ' ' | grep -oE '(error|Error)[^\"]{0,120}' | head -1)"
  "$I" get "$URL" "$new_id" "$d/new-after.cbor" > /dev/null; "$I" state "$d/new-after.cbor" > "$d/new-after.json"
  say "   state after those attempts: $(jq -c .counts "$d/new-after.json")"
  "$I" members "$d/new-after.cbor" "$room" > "$d/after-members.txt" 2>&1; sed 's/^/      after: /' "$d/after-members.txt" >> "$SUM"
  stop_node
}

# Part B: a TRUE mutual ban. A single node serialises the two CLI bans (the
# second is rejected), so the state two diverged peers would converge on is
# built directly: both bans, signed by the two deputies' own keys, added to
# the ban-free state, then PUT into the OLD contract on a fresh node.
mutual_crafted_scenario() {
  local d=$L/rooms-mutual-crafted
  say "== rooms/mutual-crafted: both deputies' bans signed offline into one $LIVE_ROOM8 state"
  start_node "$d/node" >> "$SUM"; tail -n 1 "$SUM"
  make_mutual_room "$RC_LIVE" "$d/cfg" crafted nobans | tee -a "$SUM"
  local room; room=$(cat "$d/cfg/room.txt" 2>/dev/null); [ -n "$room" ] || { fail "crafted fixture not built"; stop_node; return; }
  read -r d1 d2 m < "$d/cfg/ids.txt"
  local old_id new_id; old_id=$(gen_id "$room" "$LIVE_ROOM8"); new_id=$(gen_id "$room" "$(b3sum --no-names "$ROOM_WASM" | cut -c1-8)")
  "$I" get "$URL" "$old_id" "$d/s0.cbor" > /dev/null
  "$I" mutual-ban "$d/s0.cbor" "$room" "$d/cfg/d1/rooms.json" "$d/cfg/d2/rooms.json" "$d/s1.cbor" | sed 's/^/   /' | tee -a "$SUM"
  "$I" members "$d/s1.cbor" "$room" | sed 's/^/      crafted (rules of the rehearsed commit): /' >> "$SUM"
  "$I" params "$room" > "$d/params.cbor"
  # (i) deliver it to the running old contract, which already holds the ban-free state (a merge)
  fdev -p "$WS_PORT" publish --code "$LIVE/ui/public/contracts/room_contract.wasm" --parameters "$d/params.cbor" contract --state "$d/s1.cbor" > "$d/merge-old.log" 2>&1
  "$I" get "$URL" "$old_id" "$d/merged-old.cbor" > /dev/null
  say "   (i) PUT onto the live $LIVE_ROOM8 copy: $(grep -oE 'published successfully|rror.{0,160}' "$d/merge-old.log" | head -1); bans now $("$I" state "$d/merged-old.cbor" | jq '.counts["state.bans"]')"
  "$I" members "$d/merged-old.cbor" "$room" | sed 's/^/      merged-old: /' >> "$SUM"
  stop_node
  # (ii) fresh node: initial PUT of the mutual-ban state into the OLD contract
  start_node "$d/node2" >> "$SUM"; tail -n 1 "$SUM"
  fdev -p "$WS_PORT" publish --code "$LIVE/ui/public/contracts/room_contract.wasm" --parameters "$d/params.cbor" contract --state "$d/s1.cbor" > "$d/put-old.log" 2>&1
  say "   initial PUT into $LIVE_ROOM8: $(grep -oE 'published successfully|rror.{0,160}' "$d/put-old.log" | head -1)"
  "$I" get "$URL" "$old_id" "$d/old.cbor" > "$d/get-old.txt"; say "   GET $LIVE_ROOM8: $(cat "$d/get-old.txt")"
  grep -q '^STATE' "$d/get-old.txt" || { say "   old contract did not accept the mutual-ban state; nothing to migrate"; stop_node; return; }
  "$I" state "$d/old.cbor" > "$d/old.json"; say "   OLD state: $(jq -c .counts "$d/old.json")"
  "$I" members "$d/old.cbor" "$room" | sed 's/^/      old: /' >> "$SUM"
  local who rc
  for who in m owner d1 d2; do
    timeout 300 "$RC_NEW" --node-url "$URL" --config-dir "$d/cfg/$who" -f json member list "$room" > "$d/new-list-$who.out" 2> "$d/new-list-$who.log"; rc=$?
    say "   riverctl-new member list as $who: exit $rc, shows $(jq -c '[.[]?.nickname]' "$d/new-list-$who.out" 2>/dev/null)"
  done
  "$I" get "$URL" "$new_id" "$d/new.cbor" > "$d/get-new.txt"; say "   GET new-gen key: $(cat "$d/get-new.txt")"
  if "$I" state "$d/new.cbor" > "$d/new.json" 2>/dev/null; then
    compare_states "$d/old.json" "$d/new.json" crafted | tee -a "$SUM" || fail "crafted mutual room lost elements in migration"
    "$I" members "$d/new.cbor" "$room" | sed 's/^/      new: /' >> "$SUM"
  else fail "crafted mutual room not migrated"; stop_node; return; fi
  for who in d1 d2 m; do
    local before; before=$(jq '.counts["state.recent_messages.messages"]' "$d/new.json")
    timeout 120 "$RC_NEW" --node-url "$URL" --config-dir "$d/cfg/$who" -f json message send "$room" "after migration: $who speaks" > "$d/send-$who.out" 2> "$d/send-$who.log"; rc=$?
    "$I" get "$URL" "$new_id" "$d/after-$who.cbor" > /dev/null; "$I" state "$d/after-$who.cbor" > "$d/after-$who.json"
    say "   $who message send: exit $rc, cli says $(head -c 120 "$d/send-$who.out" | tr '\n' ' ')$(grep -oE 'Error: .{0,120}' "$d/send-$who.log" | head -1); messages in state $before -> $(jq '.counts["state.recent_messages.messages"]' "$d/after-$who.json")"
    cp "$d/after-$who.json" "$d/new.json"
  done
  for pair in "d1 $m" "m $d1" "m $d2"; do
    set -- $pair
    timeout 120 "$RC_NEW" --node-url "$URL" --config-dir "$d/cfg/$1" -f json dm send "$room" "$2" "after migration: dm" > "$d/dm-$1-$2.out" 2> "$d/dm-$1-$2.log"; rc=$?
    say "   dm $1 -> $2: exit $rc $(grep -oE 'Error: .{0,140}' "$d/dm-$1-$2.log" | head -1)"
  done
  "$I" get "$URL" "$new_id" "$d/final.cbor" > /dev/null; "$I" state "$d/final.cbor" > "$d/final.json"
  say "   final new-gen state: $(jq -c .counts "$d/final.json")"
  "$I" members "$d/final.cbor" "$room" | sed 's/^/      final: /' >> "$SUM"
  stop_node
}

case $WHAT in
  rooms|all)
    if [ "$NEW_ROOM8" = "$LIVE_ROOM8" ]; then
      say "== rooms: skipped, the room contract is unchanged from live ($NEW_ROOM8)"
    else
      room_scenario live "$RC_LIVE" "$LIVE_ROOM8"
      room_scenario prev "$RC_PREV" "$PREV_ROOM8" "$LIVE_ROOM8"
    fi ;;
esac
case $WHAT in
  mutual) mutual_scenario; mutual_crafted_scenario ;;
esac
case $WHAT in
  corpus|all) corpus_scenario ;;
esac
case $WHAT in
  ui|all)
    if [ -z "$DELEGATE_CHANGED" ]; then
      say "== ui: skipped, the chat delegate is unchanged from live (the scenario judges a delegate walk)"
    else
      build "$HERE/build-ui.sh" "$LIVE" "$WORK/ui-live" 2 > "$L/build-ui-live.log" 2>&1 || fail "live UI build"
      build "$HERE/build-ui.sh" "$PREVD" "$WORK/ui-prev" 1 > "$L/build-ui-prev.log" 2>&1 || fail "prev UI build"
      build "$HERE/build-ui.sh" "$NEW" "$WORK/ui-new-$S8" 3 > "$L/build-ui-new.log" 2>&1 || fail "new UI build"
      ui_scenario newest "$WORK/ui-live" "$RC_LIVE" "$LIVE/ui/public/contracts/chat_delegate.wasm"
      ui_scenario skipped "$WORK/ui-prev" "$RC_PREVD" "$PREVD/ui/public/contracts/chat_delegate.wasm" "$LIVE/ui/public/contracts/chat_delegate.wasm"
    fi ;;
esac
[ -s "$WORK/TELEMETRY_LEAK" ] && fail "telemetry leak recorded (see $WORK/TELEMETRY_LEAK; delete it once understood)"
if [ "$FAILS" = 0 ]; then say "REHEARSAL PASSED ($S8, $WHAT)"; else say "REHEARSAL FAILED: $FAILS failure(s) ($S8, $WHAT)"; fi
progress "run.sh $S8 ($WHAT) finished: $([ "$FAILS" = 0 ] && echo PASSED || echo "FAILED ($FAILS)"); summary $SUM"
[ "$FAILS" = 0 ]
