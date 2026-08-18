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

# Number of [[record]] blocks in FILE.
pointer_record_count() { grep -c '^\[\[record\]\]' "$1"; }

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
