#!/usr/bin/env bash
#
# Apply River's Android overlay to the dx-generated module.
#
# dx (the Dioxus CLI) regenerates the Android module — manifest,
# MainActivity stub, etc. — every build. The Dioxus-managed way to add
# our own Kotlin code and tweak the manifest is to (a) keep the canonical
# files in `ui/android/`, and (b) re-apply them on top of the generated
# tree right before gradle assembles the APK. This script does (b).
#
# What it does (idempotent — safe to re-run on every build):
#   1. Copies every file under `ui/android/kotlin/` into the dx-generated
#      `src/main/kotlin/` directory, overwriting the stubs dx wrote there.
#   2. Patches `AndroidManifest.xml` to:
#      - Add `FOREGROUND_SERVICE` permission (INTERNET is already added
#        by dx).
#      - Add `FOREGROUND_SERVICE_DATA_SYNC` permission (required on
#        API 34+ for the `dataSync` foregroundServiceType; ignored on
#        older OS versions).
#      - Declare `<service android:name="dev.dioxus.main.RiverNodeService"
#        android:foregroundServiceType="dataSync" />` inside the
#        `<application>` element. `dataSync` is the closest stock
#        foreground-service type for a P2P node's background sync work
#        and is the only one compatible with the dx-generated
#        `compileSdk = 33`. If the project ever bumps compileSdk to 34+
#        we can switch to `specialUse` with a `freenet_p2p_node` subtype.
#
# The script writes only to `target/dx/`. It never touches `ui/android/`
# (the source-of-truth overlay) or anything outside the dx-generated
# Android module.

set -euo pipefail

# Where dx writes the Android module. Mirrors the path in
# `Makefile.toml`'s build-android task.
ANDROID_MODULE_DIR=${ANDROID_MODULE_DIR:-target/dx/river-ui/debug/android/app}
OVERLAY_DIR=${OVERLAY_DIR:-ui/android}

if [[ ! -d "$ANDROID_MODULE_DIR" ]]; then
    echo "ERROR: dx-generated Android module not found at $ANDROID_MODULE_DIR" >&2
    echo "       Run 'cargo make build-android' once to generate it, then re-run this script." >&2
    exit 1
fi

if [[ ! -d "$OVERLAY_DIR" ]]; then
    echo "ERROR: overlay source dir not found at $OVERLAY_DIR" >&2
    exit 1
fi

MANIFEST="$ANDROID_MODULE_DIR/app/src/main/AndroidManifest.xml"
if [[ ! -f "$MANIFEST" ]]; then
    echo "ERROR: AndroidManifest.xml not found at $MANIFEST" >&2
    exit 1
fi

echo "Applying River Android overlay…"
echo "  Overlay source:  $OVERLAY_DIR"
echo "  Module target:   $ANDROID_MODULE_DIR"

# 1. Copy overlay Kotlin / resources into the generated module.
#    `cp -R` preserves the directory layout (so dev/dioxus/main/*.kt
#    lands at src/main/kotlin/dev/dioxus/main/*.kt).
if [[ -d "$OVERLAY_DIR/kotlin" ]]; then
    cp -R "$OVERLAY_DIR/kotlin/." "$ANDROID_MODULE_DIR/app/src/main/kotlin/"
    echo "  ✓ Copied Kotlin overlay"
fi
if [[ -d "$OVERLAY_DIR/res" && -n "$(ls -A "$OVERLAY_DIR/res" 2>/dev/null)" ]]; then
    # For every file in our overlay tree, remove any same-basename file
    # in the target that has a DIFFERENT extension. Android's resource
    # merger treats `ic_launcher.png` and `ic_launcher.webp` in the same
    # density bucket as duplicates of the `ic_launcher` resource and
    # fails the build. The dx-generated tree ships defaults as .webp,
    # but a prior overlay run that placed .png files will leave them in
    # the target on subsequent builds (dx doesn't clean). This sweep
    # makes the overlay self-healing — pruning stale variants before
    # copying our canonical extension.
    while IFS= read -r -d '' overlay_file; do
        rel="${overlay_file#$OVERLAY_DIR/res/}"
        rel_dir=$(dirname "$rel")
        full_base=$(basename "$rel")
        base="${full_base%.*}"
        ext="${full_base##*.}"
        target_dir="$ANDROID_MODULE_DIR/app/src/main/res/$rel_dir"
        if [[ -d "$target_dir" ]]; then
            for stale in "$target_dir/$base".*; do
                if [[ -f "$stale" && "${stale##*.}" != "$ext" ]]; then
                    rm "$stale"
                fi
            done
        fi
    done < <(find "$OVERLAY_DIR/res" -type f -print0)
    cp -R "$OVERLAY_DIR/res/." "$ANDROID_MODULE_DIR/app/src/main/res/"
    echo "  ✓ Copied res overlay (pruned stale duplicate-extension variants)"
fi

# 2. Patch the manifest. Each insertion is idempotent — we grep for
#    the entry first and only add if missing. This means re-running the
#    script after dx has regenerated a fresh manifest produces the same
#    result as running it the first time.
patch_manifest() {
    local pattern=$1
    local before=$2
    local insertion=$3

    if grep -q -F "$pattern" "$MANIFEST"; then
        return 0
    fi

    # Insert `$insertion` on the line BEFORE `$before`. macOS sed needs
    # the -i '' form; we use a portable Perl invocation instead so the
    # script runs the same on macOS and Linux CI.
    perl -i -pe "s|($before)|$insertion\n\$1|" "$MANIFEST"
}

# Add FOREGROUND_SERVICE + FOREGROUND_SERVICE_DATA_SYNC permissions.
# We grep-test for each by full string so re-runs are no-ops.
patch_manifest \
    'android.permission.FOREGROUND_SERVICE"' \
    '<application ' \
    '    <uses-permission android:name="android.permission.FOREGROUND_SERVICE" />'
patch_manifest \
    'android.permission.FOREGROUND_SERVICE_DATA_SYNC"' \
    '<application ' \
    '    <uses-permission android:name="android.permission.FOREGROUND_SERVICE_DATA_SYNC" />'

# POST_NOTIFICATIONS is required on API 33+ for the foreground-service
# notification to be visible in the status bar. Without it the FGS still
# runs (Android allows the process to keep executing) but the user sees
# nothing — defeating the "ongoing presence" UX of the service.
# This is a runtime permission too: the user must grant it on first
# launch. The MainActivity overlay requests it via PermissionHelper.
patch_manifest \
    'android.permission.POST_NOTIFICATIONS"' \
    '<application ' \
    '    <uses-permission android:name="android.permission.POST_NOTIFICATIONS" />'

# Declare the service inside <application>. Inserted on the line BEFORE
# `</application>` so the closing tag remains intact.
patch_manifest \
    '"dev.dioxus.main.RiverNodeService"' \
    '</application>' \
    '        <service android:name="dev.dioxus.main.RiverNodeService" android:exported="false" android:foregroundServiceType="dataSync" />'

echo "  ✓ Patched AndroidManifest.xml"

# 3. Variant transforms (River = release / production; River Debug = debug).
#    Controlled by RIVER_VARIANT env var (default: release). The debug
#    variant ships side-by-side with release via a `.debug` applicationId
#    suffix, a distinct launcher label ("River Debug"), and an orange
#    adaptive-icon background so the two installs are visually
#    distinguishable on the home screen.
#
#    All edits write OVER the release-default files copied above, so the
#    release path is left untouched and any future overlay file change
#    lands first; the debug overrides come on top.
RIVER_VARIANT=${RIVER_VARIANT:-release}
echo "  Variant:         $RIVER_VARIANT"

case "$RIVER_VARIANT" in
    release)
        : # nothing to do — overlay defaults already are release
        ;;
    debug)
        # Launcher / activity label.
        cat > "$ANDROID_MODULE_DIR/app/src/main/res/values/strings.xml" <<'STRINGS_EOF'
<?xml version="1.0" encoding="utf-8"?>
<!--
  AUTO-GENERATED by scripts/apply-android-overlay.sh for RIVER_VARIANT=debug.
  Do not edit by hand — edit the heredoc in that script instead.
-->
<resources>
    <string name="app_name">River Debug</string>
</resources>
STRINGS_EOF

        # Adaptive-icon background color (Android 8+). For release we
        # keep the user-supplied solid-white WebP. For debug we swap the
        # adaptive-icon XML to point at a color resource and define that
        # color via colors.xml — same dolphin foreground, distinct
        # orange backdrop. Pre-API-26 fallback (the legacy
        # `mipmap-*/ic_launcher.webp`) is left as-is; the homescreen
        # launcher will use the round / squircle adaptive icon on every
        # device this side of 2018.
        cat > "$ANDROID_MODULE_DIR/app/src/main/res/values/colors.xml" <<'COLORS_EOF'
<?xml version="1.0" encoding="utf-8"?>
<resources>
    <color name="ic_launcher_background">#D97706</color>
</resources>
COLORS_EOF
        cat > "$ANDROID_MODULE_DIR/app/src/main/res/mipmap-anydpi-v26/ic_launcher.xml" <<'ICON_EOF'
<?xml version="1.0" encoding="utf-8"?>
<adaptive-icon xmlns:android="http://schemas.android.com/apk/res/android">
  <background android:drawable="@color/ic_launcher_background"/>
  <foreground android:drawable="@mipmap/ic_launcher_adaptive_fore"/>
</adaptive-icon>
ICON_EOF

        # applicationId suffix. dx writes `applicationId = "org.freenet.river"`
        # into build.gradle.kts on every regen. We rewrite to add a
        # `.debug` suffix so the debug APK installs side-by-side with
        # the release. The Kotlin namespace stays
        # `org.freenet.river` — that's the BuildConfig package, not the
        # install id. Idempotent: if the file already has `.debug`, the
        # regex doesn't match and the file is unchanged.
        perl -i -pe 's/applicationId\s*=\s*"org\.freenet\.river"$/applicationId = "org.freenet.river.debug"/' \
            "$ANDROID_MODULE_DIR/app/build.gradle.kts"

        echo "  ✓ Applied debug variant transforms (label, icon color, applicationId .debug)"
        ;;
    *)
        echo "ERROR: unknown RIVER_VARIANT=$RIVER_VARIANT (expected 'release' or 'debug')" >&2
        exit 1
        ;;
esac

echo "Overlay applied successfully."
