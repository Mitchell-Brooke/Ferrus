#!/bin/bash
# Build a Debian .deb for Ferrus without dpkg-deb (works on any distro with
# ar + tar). Run from the repo root:  pack/build-deb.sh [version]
set -euo pipefail
cd "$(dirname "$0")/.."
VERSION="${1:-0.1.0}"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target}"
GUI="$CARGO_TARGET_DIR/release/ferrus"
HELPER="$CARGO_TARGET_DIR/release/ferrus-helper"

if [ ! -x "$GUI" ] || [ ! -x "$HELPER" ]; then
  echo "release binaries missing; run:" >&2
  echo "  CARGO_TARGET_DIR=$CARGO_TARGET_DIR cargo build --workspace --release" >&2
  exit 1
fi

# ---- payload -------------------------------------------------------------
install -Dm755 "$GUI"    "$STAGE/usr/bin/ferrus"
install -Dm755 "$HELPER" "$STAGE/usr/libexec/ferrus/ferrus-helper"
install -Dm644 res/uefi-ntfs.img      "$STAGE/usr/share/ferrus/uefi-ntfs.img"
install -Dm644 pack/ferrus.desktop    "$STAGE/usr/share/applications/ferrus.desktop"
install -Dm644 pack/com.ferrus.ferrus.policy \
  "$STAGE/usr/share/polkit-1/actions/com.ferrus.ferrus.policy"
install -Dm644 LICENSE "$STAGE/usr/share/doc/ferrus/copyright"
[ -f README.md ] && install -Dm644 README.md "$STAGE/usr/share/doc/ferrus/README"

# ---- control -------------------------------------------------------------
mkdir -p "$STAGE/DEBIAN"
KIB=$(du -sk "$STAGE/usr" | cut -f1)
sed "s/@INSTALLED_SIZE@/$((KIB + 16))/" pack/control > "$STAGE/DEBIAN/control"

# ---- archive (dpkg-deb-free) ---------------------------------------------
printf '2.0\n' > "$STAGE/debian-binary"
WORK=/tmp/.ferrus-deb
rm -rf "$WORK"; mkdir -p "$WORK"
(
  cd "$STAGE"
  tar --sort=name --owner=root:0 --group=root:0 -czf "$WORK/control.tar.gz" DEBIAN
  tar --sort=name --owner=root:0 --group=root:0 -czf "$WORK/data.tar.gz" usr
)
DEB="ferrus_${VERSION}_amd64.deb"
rm -f "$DEB"
ar -r -U "$DEB" \
  "$STAGE/debian-binary" "$WORK/control.tar.gz" "$WORK/data.tar.gz" 2>/dev/null ||
  ar rc "$DEB" \
    "$STAGE/debian-binary" "$WORK/control.tar.gz" "$WORK/data.tar.gz"
rm -rf "$WORK"

echo "built $DEB"
