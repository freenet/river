#!/usr/bin/env bash
# A coarse pixel tripwire for the native app (it catches whole-screen shifts and fill changes, not
# single-pixel ones): 16x16 grayscale fingerprints of five scenes, so a
# Rune bump (or any change to how the app draws) can be compared with something before anyone
# looks at screenshots. NOT run by CI: the fingerprints come from the GPU path the machine has,
# and a different Vulkan driver (CI's software one) can differ by more than the tolerance.
# Run it on the machine that made the fingerprints, before and after a bump.
#
#   scene-fingerprints.sh check  [river binary]   compare the scenes with scene-fingerprints.json
#   scene-fingerprints.sh update [river binary]   rewrite scene-fingerprints.json from the binary
#
# The binary defaults to apps/target/debug/river. Needs python3 with Pillow. A scene passes when
# the mean difference of its 256 cells is at most 0.25 and no cell differs by more than 3 (of 255); the bump to Rune eff95e80 moved these scenes by a mean of 0.5 to 1.3 and up to 9, so it fails them.
# Update the file in the same PR as a bump that moves the pixels, and say so in the PR.
set -euo pipefail

MODE="${1:?usage: scene-fingerprints.sh check|update [river binary]}"
APPS="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${2:-$APPS/target/debug/river}"
FILE="$APPS/scripts/scene-fingerprints.json"
# composer typing, a dialog with fields, a direct-message thread, the narrow layout, the focus ring
SCENES="chat-composing chat-room-details-editing chat-dm-thread chat-narrow chat-search-empty"
OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT

[[ -x "$BIN" ]] || { echo "scene-fingerprints.sh: $BIN is not an executable" >&2; exit 1; }
# Each scene is rendered twice into separate directories: the check below first requires the two
# renders to match, so a run on a machine or driver that is not deterministic says so instead of
# reporting a false failure. A crash prints what the binary said.
for pass in a b; do
    mkdir -p "$OUT/$pass"
    for scene in $SCENES; do
        size=()
        case "$scene" in *narrow*) size=(--size 820x720) ;; esac
        if ! "$BIN" --snap "$OUT/$pass/$scene.png" --scene "$scene" --scale 1 "${size[@]}" \
            >"$OUT/log" 2>&1; then
            echo "scene-fingerprints.sh: $BIN failed on scene $scene:" >&2
            cat "$OUT/log" >&2
            exit 1
        fi
    done
done

python3 -I - "$MODE" "$FILE" "$OUT" $SCENES <<'PY'
import json, sys
from PIL import Image

mode, file, out, *scenes = sys.argv[1:]

def fingerprint(path):
    im = Image.open(path).convert("L").resize((16, 16), Image.BOX)
    return list(im.getdata())

now = {s: fingerprint(f"{out}/a/{s}.png") for s in scenes}
again = {s: fingerprint(f"{out}/b/{s}.png") for s in scenes}
if now != again:
    print("the same binary drew a scene differently twice: rendering here is not deterministic,")
    print("so a fingerprint check would give false failures; not comparing")
    sys.exit(2)
if mode == "update":
    with open(file, "w") as f:
        f.write("{\n" + ",\n".join(f'"{s}": {json.dumps(v)}' for s, v in now.items()) + "\n}\n")
    print(f"wrote {file}")
    sys.exit(0)
want = json.load(open(file))
bad = 0
for s in scenes:
    if s not in want:
        print(f"FAIL {s}: not in {file}; run 'scene-fingerprints.sh update'")
        bad += 1
        continue
    d = [abs(a - b) for a, b in zip(now[s], want[s])]
    mean, worst = sum(d) / len(d), max(d)
    ok = mean <= 0.25 and worst <= 3
    bad += not ok
    print(f"{'ok  ' if ok else 'FAIL'} {s}: mean {mean:.2f}, worst cell {worst}")
sys.exit(1 if bad else 0)
PY
