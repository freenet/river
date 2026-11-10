#!/bin/bash
# Package a built Linux `river` binary as River-<version>-linux-<arch>.tar.gz.
#
#   package-linux.sh <river binary> <version> <output dir>
#
# The tarball holds one directory with the binary, river.desktop, a 512 px icon, the
# licence and a README that says where to copy them. It does not install anything.
# Run by .github/workflows/release-desktop.yml; also fine to run locally.

set -euo pipefail

BIN="${1:?usage: package-linux.sh <river binary> <version> <output dir>}"
VERSION="${2:?version required}"
OUT_DIR="${3:?output dir required}"

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../.." && pwd)"

[[ -x "$BIN" ]] || { echo "package-linux.sh: $BIN is not an executable" >&2; exit 1; }

# The binary must be the version being released. --version needs no display or GPU.
reported="$("$BIN" --version)"
if [[ "$reported" != "river $VERSION" ]]; then
    echo "package-linux.sh: $BIN reports '$reported', expected 'river $VERSION'" >&2
    exit 1
fi

case "$(uname -m)" in
    x86_64) ARCH=x86_64 ;;
    aarch64) ARCH=aarch64 ;;
    *) echo "package-linux.sh: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

NAME="River-$VERSION-linux-$ARCH"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
DIR="$STAGE/$NAME"
mkdir -p "$DIR"

install -m 0755 "$BIN" "$DIR/river"
install -m 0644 "$HERE/linux/river.desktop" "$DIR/river.desktop"
install -m 0644 "$HERE/icons/river.png" "$DIR/river.png"
install -m 0644 "$REPO_ROOT/LICENSE" "$DIR/LICENSE"
cat > "$DIR/README.txt" <<EOF
River $VERSION for Linux ($ARCH)

Run ./river to start. River finds the Freenet node on this computer, or offers to
install it.

Before first use: this preview cannot set up River's storage on your Freenet node
by itself. If you have never used River on this computer, open River in your web
browser through Freenet once first, then start this app.

To add River to your applications menu:

  mkdir -p ~/.local/bin ~/.local/share/applications ~/.local/share/icons/hicolor/512x512/apps
  cp river ~/.local/bin/
  cp river.desktop ~/.local/share/applications/
  cp river.png ~/.local/share/icons/hicolor/512x512/apps/

~/.local/bin must be on your PATH for the menu entry to start it.

River needs libxkbcommon and libwayland-client (installed on most desktops) and a
GPU driver with Vulkan support.

Source: https://github.com/freenet/river
EOF

# Fixed owner, order and timestamps, so the archive depends only on its contents.
mtime="${SOURCE_DATE_EPOCH:-$(git -C "$REPO_ROOT" log -1 --format=%ct)}"
mkdir -p "$OUT_DIR"
ARCHIVE="$(cd "$OUT_DIR" && pwd)/$NAME.tar.gz"
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$mtime" --mode=u+rwX,go+rX,go-w \
    -C "$STAGE" -cf - "$NAME" | gzip -n -9 > "$ARCHIVE"

# The newest glibc symbol version the binary needs is the oldest glibc it runs on.
glibc="$(objdump -T "$BIN" | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -n1 || true)"
echo "Requires at least: ${glibc:-unknown glibc}"
echo "$ARCHIVE"
tar -tvzf "$ARCHIVE"
