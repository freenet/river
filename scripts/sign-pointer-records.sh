#!/bin/bash
# Re-sign every pointer record against the WASM currently in the working tree.
#
# Run this whenever a pointed-at WASM changes, BEFORE committing, then commit
# pointer-records.toml alongside the new WASM. CI (check-pointer-freshness)
# fails the PR if you forget.
#
# Signing is offline: it needs the author key but not the network. Publishing
# the signed records is a separate step (scripts/publish-pointer-records.sh),
# run from main after merge.
#
# Usage:
#   scripts/sign-pointer-records.sh            # bump version, re-sign changed records
#   scripts/sign-pointer-records.sh --force    # re-sign even records that did not change
#
# The key is read from ~/.config/river/web-container-keys.toml — the SAME key
# that signs the web container (Ian's decision, 2026-08-18). It is piped
# straight into the signer and never lands in a variable, a temp file, or argv.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TOML_PATH="pointer-records.toml"
KEY_FILE="${RIVER_POINTER_KEY_FILE:-$HOME/.config/river/web-container-keys.toml}"

cd "$REPO_ROOT"

die() { echo "ERROR: $*" >&2; exit 1; }

FORCE=0
for arg in "$@"; do
    case "$arg" in
        --force) FORCE=1 ;;
        # Strict: a typo'd flag that fell through would produce records the
        # caller did not ask for, signed with a production key.
        *) die "unknown argument '$arg' (expected nothing, or --force)" ;;
    esac
done

command -v b3sum >/dev/null 2>&1 || die "b3sum not found. Install with: cargo install b3sum"
command -v pointer-record >/dev/null 2>&1 || die "pointer-record not found — see scripts/check-pointer-freshness.sh for the install line"
[ -f "$KEY_FILE" ] || die "key file not found: $KEY_FILE"
[ -f "$TOML_PATH" ] || die "$TOML_PATH not found"

# Refuse to sign against a key file anyone else can read. The author key is the
# whole trust anchor for every integrator resolving a River pointer.
PERMS="$(stat -c '%a' "$KEY_FILE")"
case "$PERMS" in
    600|400) ;;
    *) die "$KEY_FILE has mode $PERMS; expected 600. Fix with: chmod 600 '$KEY_FILE'" ;;
esac

field_of_record() {
    awk -v want="$1" -v key="$2" '
        /^\[\[record\]\]/ { i++; next }
        i == want && $0 ~ "^"key"[ \t]*=" {
            sub("^"key"[ \t]*=[ \t]*", ""); gsub(/^"|"$/, ""); print; exit
        }
    ' "$TOML_PATH"
}
top_level_field() {
    awk -v key="$1" '
        /^\[\[record\]\]/ { exit }
        $0 ~ "^"key"[ \t]*=" {
            sub("^"key"[ \t]*=[ \t]*", ""); gsub(/^"|"$/, ""); print; exit
        }
    ' "$TOML_PATH"
}

AUTHOR_VK="$(top_level_field author_verifying_key)"
[ -n "$AUTHOR_VK" ] || die "no author_verifying_key in $TOML_PATH"

N="$(grep -c '^\[\[record\]\]' "$TOML_PATH")"
CHANGED=0

for i in $(seq 1 "$N"); do
    APP_ID="$(field_of_record "$i" app_id)"
    WASM_PATH="$(field_of_record "$i" wasm_path)"
    OLD_HASH="$(field_of_record "$i" code_hash)"
    OLD_VERSION="$(field_of_record "$i" version)"

    [ -f "$WASM_PATH" ] || die "$WASM_PATH not found (record $i, $APP_ID)"
    NEW_HASH="$(b3sum "$WASM_PATH" | cut -d' ' -f1)"

    if [ "$NEW_HASH" = "$OLD_HASH" ] && [ "$FORCE" -eq 0 ]; then
        echo "$APP_ID: unchanged at $NEW_HASH (version $OLD_VERSION)"
        continue
    fi

    # The version is a monotonic counter gating the network update. A republish
    # at an already-used version is a silent NO-OP: the contract deliberately
    # does not error on a stale update, so the network would never tell us the
    # release was ignored.
    NEW_VERSION=$((OLD_VERSION + 1))
    echo "$APP_ID: $OLD_HASH -> $NEW_HASH  (version $OLD_VERSION -> $NEW_VERSION)"

    # The key is piped straight in; --author-vk makes the signer REFUSE if the
    # key file has drifted from the identity published in FREENET.md.
    OUT="$(pointer-record sign \
        --author-vk "$AUTHOR_VK" \
        --app-id "$APP_ID" \
        --version "$NEW_VERSION" \
        --code-hash "$NEW_HASH" < "$KEY_FILE")" || die "signing $APP_ID failed"

    NEW_STATE="$(printf '%s\n' "$OUT" | sed -n 's/^state=//p')"
    NEW_KEY="$(printf '%s\n' "$OUT" | sed -n 's/^key=//p')"
    [ -n "$NEW_STATE" ] && [ -n "$NEW_KEY" ] || die "signer produced no state/key for $APP_ID"

    # Rewrite only this record's block, matched by app_id rather than by line
    # number: a line-addressed edit silently rewrites the wrong record the first
    # time somebody reorders the file.
    python3 - "$TOML_PATH" "$APP_ID" "$NEW_VERSION" "$NEW_HASH" "$NEW_STATE" "$NEW_KEY" <<'PY'
import re, sys
path, app_id, version, code_hash, state, key = sys.argv[1:7]
src = open(path, encoding='utf-8').read()
blocks = src.split('[[record]]')
out = [blocks[0]]
hits = 0
for b in blocks[1:]:
    if re.search(r'^app_id\s*=\s*"%s"\s*$' % re.escape(app_id), b, re.M):
        hits += 1
        b = re.sub(r'^version\s*=.*$',    'version = %s' % version,     b, count=1, flags=re.M)
        b = re.sub(r'^code_hash\s*=.*$',  'code_hash = "%s"' % code_hash, b, count=1, flags=re.M)
        b = re.sub(r'^state\s*=.*$',      'state = "%s"' % state,       b, count=1, flags=re.M)
        b = re.sub(r'^pointer_key\s*=.*$','pointer_key = "%s"' % key,   b, count=1, flags=re.M)
    out.append(b)
if hits != 1:
    sys.exit("expected exactly one [[record]] with app_id=%s, found %d" % (app_id, hits))
open(path, 'w', encoding='utf-8').write('[[record]]'.join(out))
PY
    CHANGED=1
done

if [ "$CHANGED" -eq 0 ]; then
    echo ""
    echo "Nothing to do — every record already names the WASM in the working tree."
    exit 0
fi

echo ""
echo "Re-verifying through the gate CI will run..."
./scripts/check-pointer-freshness.sh

cat <<'EOF'

Next:
  1. git add pointer-records.toml   (and the WASM change that caused this)
  2. Open a PR. check-pointer-freshness will re-verify.
  3. AFTER merge, from main: cargo make publish-pointer-records

Signing does not publish. Until step 3 runs, integrators still resolve the
PREVIOUS record — which is correct, because the new WASM is not on the network
until the UI is republished either.
EOF
