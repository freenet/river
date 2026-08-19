#!/bin/bash
# Fail the build if a committed pointer record has gone stale.
#
# River publishes pointer records (see pointer-records.toml) so third parties
# can resolve our artifacts' CURRENT keys instead of pinning one that re-keys
# under them. Once a pointer exists, a STALE pointer is worse than none: before
# it existed an integrator pinning our key knew they were pinning it; after, they
# resolve, get a confident answer, and derive a dead key.
#
# So the rule this enforces is: whenever a pointed-at WASM changes, a new record
# must be signed in the same PR.
#
# ## Why this is not a "does an entry exist" check
#
# check-migration.sh asks whether the OLD hash was recorded — a question that is
# only meaningful when the WASM changed. This asks whether the record names the
# CURRENT bytes, which is meaningful on every commit. That difference matters:
# a presence check passes forever after the first record is committed, and would
# sail through exactly the failure this script exists to catch.
#
# Three things are checked per record, and each can fail on its own:
#
#   1. code_hash == BLAKE3 of the committed WASM at wasm_path.  <- the real gate
#   2. The 100-byte `state` VERIFIES under the author key in FREENET.md, and
#      carries that same code_hash and version. Catches a hand-edited hash that
#      left its signature behind, which check 1 alone would accept.
#   3. version strictly increased if the record changed since the base commit.
#      A repeat version is a silent no-op on the network: the contract treats a
#      stale update as success on purpose, so nothing downstream would tell us.
#
# Usage:
#   scripts/check-pointer-freshness.sh                         # HEAD vs working tree
#   scripts/check-pointer-freshness.sh --ci BASE_SHA HEAD_SHA  # CI mode
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TOML_PATH="pointer-records.toml"
FREENET_MD="FREENET.md"

# The signing/verifying tool, pinned by revision. Pinned rather than floating for
# the same reason integrators pin it: this tool decides what a record's bytes
# ARE, and a gate whose oracle can change under it is not a gate.
POINTER_TOOL_REPO="https://github.com/freenet/freenet-migrate"
POINTER_TOOL_REV="${POINTER_TOOL_REV:-5e1759c39f98ec54f51c84d632e28fc33578b48d}"

cd "$REPO_ROOT"

die() { echo "ERROR: $*" >&2; exit 1; }

command -v b3sum >/dev/null 2>&1 || die "b3sum not found. Install with: cargo install b3sum"
command -v pointer-record >/dev/null 2>&1 || die "pointer-record not found. Install with:
  cargo install --git $POINTER_TOOL_REPO --rev $POINTER_TOOL_REV --features publish --locked freenet-pointer-contract"

# --------------------------------------------------------------------- parsing
#
# One shared reader, in scripts/pointer-toml-lib.sh, used by this gate AND by
# sign-pointer-records.sh. There used to be two near-identical copies; the two
# disagreeing about what a record says is the worst failure available here,
# because the signer would write something the gate reads differently.
. "$(dirname "$0")/pointer-toml-lib.sh"

field_of_record() { pointer_field "${3:-$TOML_PATH}" "$1" "$2"; }
top_level_field() { pointer_top_field "${2:-$TOML_PATH}" "$1"; }
record_count()    { pointer_record_count "$TOML_PATH"; }
app_ids_in()      { pointer_app_ids "$1"; }
index_of_app()    { pointer_index_of_app "$1" "$2"; }

wasm_hash_at_ref() {
    local ref="$1" path="$2"
    if git cat-file -e "$ref:$path" 2>/dev/null; then
        git show "$ref:$path" | b3sum | cut -d' ' -f1
    else
        echo ""
    fi
}

# ------------------------------------------------------------------- arguments

MODE="local"
BASE_SHA=""
HEAD_SHA=""
if [ "${1:-}" = "--ci" ]; then
    MODE="ci"
    BASE_SHA="${2:?Usage: check-pointer-freshness.sh --ci BASE_SHA HEAD_SHA}"
    HEAD_SHA="${3:?Usage: check-pointer-freshness.sh --ci BASE_SHA HEAD_SHA}"
elif [ -n "${1:-}" ]; then
    # Strict on purpose. A typo'd flag that silently fell through to local mode
    # would be a gate reporting success having compared the wrong things.
    die "unknown argument '$1' (expected nothing, or --ci BASE_SHA HEAD_SHA)"
fi

[ -f "$TOML_PATH" ] || die "$TOML_PATH not found"

# ------------------------------------------------- read ONE tree, not two
#
# On `pull_request`, actions/checkout gives the synthetic MERGE commit, so the
# working tree is base+head merged. The WASM is hashed from
# `github.event.pull_request.head.sha`. Reading the registry from the working
# tree while hashing the WASM from head.sha compares two different trees: if
# the base branch lands a new WASM and its matching record after this branch
# forked, the merged registry (correctly carrying base's new record) is checked
# against head's OLD WASM and fails as stale, on a PR that is actually fine.
#
# So in CI mode every head-side file is materialized from $HEAD_SHA. Base-side
# reads already used `git show`. This mirrors check-migration.sh, which reads
# both sides through git and never touches the working tree.
if [ "$MODE" = "ci" ]; then
    HEAD_TREE="$(mktemp -d)"
    trap 'rm -rf "$HEAD_TREE"' EXIT
    for f in "$TOML_PATH" "$FREENET_MD"; do
        git show "$HEAD_SHA:$f" > "$HEAD_TREE/$(basename "$f")" 2>/dev/null \
            || die "$f does not exist at head ($HEAD_SHA)"
    done
    TOML_PATH="$HEAD_TREE/$(basename "$TOML_PATH")"
    FREENET_MD="$HEAD_TREE/$(basename "$FREENET_MD")"
    echo "== reading the registry and FREENET.md from head ($HEAD_SHA) =="
    echo ""
fi

AUTHOR_VK="$(top_level_field author_verifying_key)"
[ -n "$AUTHOR_VK" ] || die "no author_verifying_key in $TOML_PATH"

# ----------------------------------------------------- 0. the published anchor
#
# The author key is the ENTIRE trust anchor: an integrator takes it from
# FREENET.md and verifies every record against it. If this file and that file
# disagree, we sign records nobody can verify — and it fails silently, because
# a record signed by the wrong key is perfectly well-formed.
#
# Matched as a WHOLE LINE (`-qx`), never as a substring. The key is published in
# FREENET.md alone on its own line, and a substring match accepts any value that
# is merely a PREFIX or SUFFIX of it — a truncated key, or one that lost its
# `river:v1:vk:` prefix — which is precisely the disagreement this check exists
# to catch. It failed OPEN on exactly its own subject matter: with the prefix
# stripped from this file only, the whole gate reported success while the two
# files published textually different anchors.
echo "== author key =="
if ! grep -qxF "$AUTHOR_VK" "$FREENET_MD"; then
    echo "FAILED: $TOML_PATH publishes author_verifying_key"
    echo "  $AUTHOR_VK"
    echo "but $FREENET_MD does not carry that value on a line of its own."
    echo ""
    echo "A near-miss counts as a mismatch: a truncated key, or one missing its"
    echo "\`river:v1:vk:\` prefix, is a DIFFERENT published anchor even though it"
    echo "reads almost the same."
    echo ""
    echo "Integrators take the author key from $FREENET_MD. If the two disagree,"
    echo "every record we sign is one they will reject — and the record will look"
    echo "perfectly valid from our side."
    exit 1
fi
echo "  $AUTHOR_VK (present in $FREENET_MD)"
echo ""

N="$(record_count)"
[ "$N" -gt 0 ] || die "no [[record]] blocks in $TOML_PATH"

FAILED=0

# ------------------------------------------------- no record may VANISH
#
# Every check below is per-record, so they all pass happily on a file that has
# had a record DELETED from it — the gate dutifully verifies what remains and
# reports success. That is the one way a pointer can go stale with CI green:
# the record stops being maintained here while the address it published stays
# live on the network forever, still answering with the last hash it was given.
#
# Adding a record is fine. Removing one is a deliberate act with consequences
# for anyone already resolving it, so it must be visible in review rather than
# silent.
if [ "$MODE" = "ci" ]; then
    BASE_TOML_ALL="$(mktemp)"
    if git show "$BASE_SHA:$TOML_PATH" > "$BASE_TOML_ALL" 2>/dev/null; then
        # Compared as exact STRINGS via `grep -qxF` against the head file's
        # extracted app_id list — never by interpolating the app_id into a
        # regex. Both real app_ids contain a `.`, which is a regex wildcard, so
        # an `-E` match would have accepted `riverXroom-contract` as proof that
        # `river.room-contract` is still present. A rename is exactly the
        # deletion this check exists to catch, so that is the one input that
        # must not slip through.
        HEAD_APPS="$(mktemp)"
        app_ids_in "$TOML_PATH" > "$HEAD_APPS"
        MISSING=""
        while IFS= read -r base_app; do
            [ -z "$base_app" ] && continue
            grep -qxF "$base_app" "$HEAD_APPS" || MISSING="$MISSING $base_app"
        done < <(app_ids_in "$BASE_TOML_ALL")
        rm -f "$HEAD_APPS"
        if [ -n "$MISSING" ]; then
            echo "FAILED: a pointer record present at the base commit is GONE at head:"
            for m in $MISSING; do echo "    $m"; done
            echo ""
            echo "The address that record published stays live on the network and keeps"
            echo "answering with the last code hash it was given. Deleting the record here"
            echo "only stops us maintaining it — it does not retract it."
            echo ""
            echo "If retiring a pointer is genuinely intended, that is a withdrawal and"
            echo "needs doing on the network, not by deleting a line."
            exit 1
        fi
    fi
    rm -f "$BASE_TOML_ALL"
fi
for i in $(seq 1 "$N"); do
    APP_ID="$(field_of_record "$i" app_id)"
    WASM_PATH="$(field_of_record "$i" wasm_path)"
    VERSION="$(field_of_record "$i" version)"
    CODE_HASH="$(field_of_record "$i" code_hash)"
    STATE="$(field_of_record "$i" state)"
    POINTER_KEY="$(field_of_record "$i" pointer_key)"

    echo "== $APP_ID =="
    for v in APP_ID WASM_PATH VERSION CODE_HASH STATE POINTER_KEY; do
        [ -n "${!v}" ] || die "record $i is missing $(echo "$v" | tr 'A-Z_' 'a-z-')"
    done

    # --- 1. does the record name the bytes that are actually committed? -------
    if [ "$MODE" = "ci" ]; then
        ACTUAL="$(wasm_hash_at_ref "$HEAD_SHA" "$WASM_PATH")"
        WHERE="committed at $HEAD_SHA"
    else
        [ -f "$WASM_PATH" ] || die "$WASM_PATH not found"
        ACTUAL="$(b3sum "$WASM_PATH" | cut -d' ' -f1)"
        WHERE="in the working tree"
    fi
    [ -n "$ACTUAL" ] || die "$WASM_PATH does not exist $WHERE"

    if [ "$CODE_HASH" != "$ACTUAL" ]; then
        echo "  FAILED: the record is STALE."
        echo "    record names : $CODE_HASH"
        echo "    $WASM_PATH ($WHERE): $ACTUAL"
        echo ""
        echo "  The WASM this pointer names has changed and no new record was signed."
        echo "  Any integrator resolving $APP_ID would derive a DEAD key."
        echo ""
        echo "  To fix:  cargo make sign-pointer-records   (then commit $TOML_PATH)"
        echo "  Or revert the WASM change."
        FAILED=1
        echo ""
        continue
    fi
    echo "  code_hash matches $WASM_PATH ($WHERE): $ACTUAL"

    # --- 2. is the record actually SIGNED for what it claims? ----------------
    # Check 1 compares two strings in files we control, so on its own it would
    # accept a hand-edited hash whose signature was left behind. This is the
    # check that makes the file's contents mean something.
    if ! pointer-record verify \
            --author-vk "$AUTHOR_VK" \
            --app-id "$APP_ID" \
            --state "$STATE" \
            --expect-version "$VERSION" \
            --expect-code-hash "$CODE_HASH" \
            --expect-key "$POINTER_KEY" >/dev/null; then
        echo "  FAILED: the record's bytes do not verify (see the error above)."
        echo ""
        echo "  Do not hand-edit code_hash, version, state or pointer_key."
        echo "  Run:  cargo make sign-pointer-records"
        FAILED=1
        echo ""
        continue
    fi
    echo "  signature verifies under the author key, at version $VERSION"

    # --- 3. did the version move when the record did? ------------------------
    # A republish at an already-used version is a no-op SUCCESS on the network —
    # the contract refuses to error on a stale update by design, so nothing
    # downstream would ever tell us the release was ignored.
    if [ "$MODE" = "ci" ]; then
        BASE_TOML="$(mktemp)"
        if git show "$BASE_SHA:$TOML_PATH" > "$BASE_TOML" 2>/dev/null; then
            # Looked up BY app_id. An index-matched lookup skips this check
            # whenever the two blocks are reordered — for each index the base
            # and head apps differ, so both records dodge the version gate at
            # once, in a diff where nothing signals "skip me".
            BASE_I="$(index_of_app "$BASE_TOML" "$APP_ID")"
            BASE_STATE=""
            BASE_VERSION=""
            if [ -n "$BASE_I" ]; then
                BASE_STATE="$(field_of_record "$BASE_I" state "$BASE_TOML")"
                BASE_VERSION="$(field_of_record "$BASE_I" version "$BASE_TOML")"
            fi
            if [ -n "$BASE_STATE" ]; then
                if [ "$BASE_STATE" != "$STATE" ] && [ "$VERSION" -le "$BASE_VERSION" ]; then
                    echo "  FAILED: the record changed but version did not increase"
                    echo "    base ($BASE_SHA): version $BASE_VERSION"
                    echo "    head ($HEAD_SHA): version $VERSION"
                    echo ""
                    echo "  Publishing at an already-used version is a silent no-op:"
                    echo "  the network accepts it and keeps the OLD record."
                    FAILED=1
                elif [ "$BASE_STATE" != "$STATE" ]; then
                    echo "  version advanced $BASE_VERSION -> $VERSION"
                fi
            fi
        fi
        rm -f "$BASE_TOML"
    fi
    echo ""
done

if [ "$FAILED" -ne 0 ]; then
    echo "One or more pointer records are stale or unverifiable. See above."
    exit 1
fi

echo "All $N pointer record(s) are fresh, signed, and name the committed WASM."
