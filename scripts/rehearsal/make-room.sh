# Build a populated room with an OLD riverctl (so on that riverctl's room-contract
# generation). Sourced by run.sh after start_node. Usage:
#   make_room <riverctl> <work-dir> <label>
# Leaves <work-dir>/{owner,m1..m4} riverctl config dirs and <work-dir>/room.txt.
# Shape: owner invites m1, m2, m4; m1 invites m3. 11 messages. owner deputizes
# m1. Bans: owner bans m4, m1 bans m3. DMs: owner<->m1, m2->m1.
make_room() {
  local rc=$1 w=$2 label=$3
  mkdir -p "$w"
  rcx() { local who=$1; shift; timeout 180 "$rc" --node-url "$URL" --config-dir "$w/$who" -f json "$@" 2>>"$w/riverctl.log"; }
  local room
  room=$(rcx owner room create --name "Rehearsal $label" --nickname owner | jq -r .owner_key)
  [ -n "$room" ] && [ "$room" != null ] || { echo "room create failed (see $w/riverctl.log)"; return 1; }
  echo "$room" > "$w/room.txt"
  join() { # inviter joiner
    local code
    code=$(rcx "$1" invite create "$room" | jq -r .invitation_code)
    rcx "$2" invite accept "$code" --nickname "$2" > /dev/null || { echo "$2 could not join"; return 1; }
  }
  join owner m1; join owner m2; join owner m4; join m1 m3
  local who i
  for who in owner m1 m2 m3 m4; do
    for i in 1 2; do rcx "$who" message send "$room" "$label: $who says $i" > /dev/null || echo "send $who $i failed"; done
  done
  rcx owner message send "$room" "$label: owner wraps up" > /dev/null
  id_of() { rcx owner member list "$room" | jq -r --arg n "$1" '.[] | select(.nickname==$n) | .member_id'; }
  local m1 m2 m3 m4
  m1=$(id_of m1); m2=$(id_of m2); m3=$(id_of m3); m4=$(id_of m4)
  rcx owner member deputize "$room" "$m1" > /dev/null || echo "deputize failed"
  rcx owner dm send "$room" "$m1" "$label: owner to m1" > /dev/null || echo "dm owner->m1 failed"
  rcx m1 dm send "$room" "$(id_of owner)" "$label: m1 to owner" > /dev/null || echo "dm m1->owner failed"
  rcx m2 dm send "$room" "$m1" "$label: m2 to m1" > /dev/null || echo "dm m2->m1 failed"
  rcx owner member ban "$room" "$m4" > /dev/null || echo "ban m4 failed"
  rcx m1 member ban "$room" "$m3" > /dev/null || echo "ban m3 failed"
  echo "   room $room built by $("$rc" --version) (m1=$m1 m2=$m2 m3=$m3 m4=$m4)"
}

# Mutual ban between two moderators, built by an OLD riverctl:
#   owner invites d1, d2, m; deputizes d1 and d2; everyone posts; d1<->m DMs;
#   then d1 bans d2 and d2 bans d1 AT THE SAME TIME (two processes), so
#   neither ban is issued after seeing the other.
make_mutual_room() {
  local rc=$1 w=$2 label=$3 nobans=${4:-}
  mkdir -p "$w"
  rcx() { local who=$1; shift; timeout 180 "$rc" --node-url "$URL" --config-dir "$w/$who" -f json "$@" 2>>"$w/riverctl.log"; }
  local room
  room=$(rcx owner room create --name "Mutual $label" --nickname owner | jq -r .owner_key)
  [ -n "$room" ] && [ "$room" != null ] || { echo "room create failed"; return 1; }
  echo "$room" > "$w/room.txt"
  local who code
  for who in d1 d2 m; do
    code=$(rcx owner invite create "$room" | jq -r .invitation_code)
    rcx "$who" invite accept "$code" --nickname "$who" > /dev/null || echo "$who could not join"
  done
  for who in owner d1 d2 m; do rcx "$who" message send "$room" "$label: $who here" > /dev/null || echo "send $who failed"; done
  id_of() { rcx owner member list "$room" | jq -r --arg n "$1" '.[] | select(.nickname==$n) | .member_id'; }
  local d1 d2 m; d1=$(id_of d1); d2=$(id_of d2); m=$(id_of m)
  echo "$d1 $d2 $m" > "$w/ids.txt"
  rcx owner member deputize "$room" "$d1" > /dev/null || echo "deputize d1 failed"
  rcx owner member deputize "$room" "$d2" > /dev/null || echo "deputize d2 failed"
  rcx d1 dm send "$room" "$m" "$label: d1 to m" > /dev/null || echo "dm d1->m failed"
  rcx m dm send "$room" "$d1" "$label: m to d1" > /dev/null || echo "dm m->d1 failed"
  if [ -n "$nobans" ]; then echo "   mutual room $room by $("$rc" --version) without bans (d1=$d1 d2=$d2 m=$m)"; return 0; fi
  # Both deputies refresh, then ban each other concurrently.
  rcx d1 member list "$room" > /dev/null; rcx d2 member list "$room" > /dev/null
  ( rcx d1 member ban "$room" "$d2" > "$w/ban-d1.out"; echo "exit $?" >> "$w/ban-d1.out" ) & local p1=$!
  ( rcx d2 member ban "$room" "$d1" > "$w/ban-d2.out"; echo "exit $?" >> "$w/ban-d2.out" ) & local p2=$!
  wait "$p1" "$p2"   # never a bare wait: the node is also a background job
  echo "   mutual room $room by $("$rc" --version) (d1=$d1 d2=$d2 m=$m); ban d1->d2: $(tr '\n' ' ' < "$w/ban-d1.out" | cut -c1-120); ban d2->d1: $(tr '\n' ' ' < "$w/ban-d2.out" | cut -c1-120)"
}
