# shellcheck shell=bash
# Shared reader for pointer-records.toml. Source this; do not execute it.
#
# ## Why a hand-rolled reader rather than a TOML library
#
# The freshness gate is a CI job whose only purpose is to check. Giving it a
# Python or Rust TOML dependency makes the gate itself something that can fail
# to install, and a gate that cannot run is a gate that reports nothing. The
# schema is ours and is a handful of scalar keys per `[[record]]` block.
#
# ## Why it is SHARED rather than copied
#
# There were two near-identical copies of these functions, in the gate and in
# the signer. A parsing fix applied to one would not reach the other, and the
# two disagreeing about what a record says is the worst failure available here:
# the signer would write something the gate reads differently, or vice versa.
# One copy, sourced by both.
#
# ## What the parsing handles, and what it deliberately does not
#
# Handled: whitespace around `=`, quoted and bare values, and a trailing
# `# comment` after either. A `#` INSIDE a quoted value survives correctly,
# because a quoted value is taken up to its closing quote.
#
# NOT handled: multi-line values, arrays, inline tables, a DUPLICATE key within
# one block (the first occurrence wins and no error is raised, where real TOML
# would reject the file — the signer writes this file so a duplicate means a bad
# merge or a hand-edit), and — deliberately — a key INDENTED from the line
# start. Every pattern anchors `[[record]]` and
# the key at column zero, which is what stops a mention of either inside a
# comment or a prose block from contributing a phantom record or field. The
# cost is that TOML's legal leading indentation is not read. That cost is paid
# knowingly: an indented key fails CLOSED, with `record N is missing <field>`,
# rather than being silently misread. Verified, not assumed — indenting
# `app_id` produces exactly that error.
#
# If indentation is ever wanted, widen the anchor and re-check that a
# `[[record]]` inside a comment still cannot create a phantom record. The
# header of pointer-records.toml contains exactly such a mention.

# Prints the value of KEY inside the Nth [[record]] block of FILE.
#   pointer_field <file> <n> <key>
pointer_field() {
    awk -v want="$2" -v key="$3" '
        function unquote(v) {
            sub("^" key "[ \t]*=[ \t]*", "", v)
            if (substr(v, 1, 1) == "\"") {
                # Quoted: take up to the closing quote, so a trailing comment
                # (or a "#" inside the value) cannot leak in.
                v = substr(v, 2)
                idx = index(v, "\"")
                if (idx > 0) v = substr(v, 1, idx - 1)
            } else {
                sub(/[ \t]*#.*$/, "", v)   # bare value: drop a trailing comment
                gsub(/^[ \t]+|[ \t]+$/, "", v)
            }
            return v
        }
        /^\[\[record\]\]/ { i++; next }
        i == want && $0 ~ "^" key "[ \t]*=" { print unquote($0); exit }
    ' "$1"
}

# Prints the value of a top-level KEY in FILE (anything before the first record).
#   pointer_top_field <file> <key>
pointer_top_field() {
    awk -v key="$2" '
        function unquote(v) {
            sub("^" key "[ \t]*=[ \t]*", "", v)
            if (substr(v, 1, 1) == "\"") {
                v = substr(v, 2)
                idx = index(v, "\"")
                if (idx > 0) v = substr(v, 1, idx - 1)
            } else {
                sub(/[ \t]*#.*$/, "", v)
                gsub(/^[ \t]+|[ \t]+$/, "", v)
            }
            return v
        }
        /^\[\[record\]\]/ { exit }
        $0 ~ "^" key "[ \t]*=" { print unquote($0); exit }
    ' "$1"
}

# Number of [[record]] blocks in FILE. Refuses to return zero.
#
# `grep -c` exits 1 when it matches nothing, so under `set -e` an empty registry
# killed the caller with a bare exit code and NO message — swallowing the
# caller's own "no [[record]] blocks" error in exactly the case that message
# exists for. This says the same thing out loud and still fails.
#
# The `|| true` below suppresses grep's exit code ONLY so this function can
# report the reason itself; it must never be relaxed into simply returning 0.
# Every caller multiplies this number into a `seq 1 $N` loop, so a quiet 0 turns
# each of them into a loop that runs zero times and then reports success over
# nothing — a publish script printing "All 0 record(s) published AND verified
# from the network" having published nothing at all. That is a fail-CLOSED bug
# traded for a fail-OPEN one. Failing stays the point; the only thing fixed here
# is that it now says why. Callers guard the result as well, so an empty
# registry is still refused where `set -e` is off.
pointer_record_count() {
    local n
    n="$(grep -c '^\[\[record\]\]' "$1" || true)"
    if [ "${n:-0}" -eq 0 ]; then
        echo "ERROR: no [[record]] blocks in $1 — the registry is empty, so there is" >&2
        echo "       nothing to check, sign or publish. This is a hard failure: an empty" >&2
        echo "       registry is not 'zero work to do', it is a registry that lost its" >&2
        echo "       records." >&2
        return 1
    fi
    printf '%s\n' "$n"
}

# Every app_id in FILE, one per line.
pointer_app_ids() {
    awk '
        /^app_id[ \t]*=/ {
            v = $0
            sub("^app_id[ \t]*=[ \t]*", "", v)
            if (substr(v, 1, 1) == "\"") {
                v = substr(v, 2); idx = index(v, "\""); if (idx > 0) v = substr(v, 1, idx - 1)
            } else {
                sub(/[ \t]*#.*$/, "", v); gsub(/^[ \t]+|[ \t]+$/, "", v)
            }
            print v
        }
    ' "$1"
}

# The 1-based index of the record carrying APP_ID in FILE, or empty.
#
# Records are looked up BY app_id and never by position. Position-matching
# means reordering two blocks makes each index compare two DIFFERENT apps, so
# a version-regression check keyed on it skips every record at once — in a diff
# where nothing signals "skip me".
pointer_index_of_app() {
    awk -v want="$2" '
        /^\[\[record\]\]/ { i++; next }
        /^app_id[ \t]*=/ {
            v = $0
            sub("^app_id[ \t]*=[ \t]*", "", v)
            if (substr(v, 1, 1) == "\"") {
                v = substr(v, 2); idx = index(v, "\""); if (idx > 0) v = substr(v, 1, idx - 1)
            } else {
                sub(/[ \t]*#.*$/, "", v); gsub(/^[ \t]+|[ \t]+$/, "", v)
            }
            if (v == want) { print i; exit }
        }
    ' "$1"
}
