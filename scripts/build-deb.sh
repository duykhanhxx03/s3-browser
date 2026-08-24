#!/usr/bin/env bash
# Builds the Debian package from an already-built Linux release binary.
#
# Usage: scripts/build-deb.sh <version> <binary-path> <out-dir>
#
# The icon has to land in every size hicolor's index.theme actually
# declares (16 through 512) plus /usr/share/pixmaps as a theme-independent
# fallback. A single 1024x1024/apps/ directory looks reasonable but is
# silently invisible: GNOME's default hicolor index.theme does not list a
# 1024 bucket, so gtk_icon_theme_lookup_icon() never finds it and the app
# has no icon anywhere — not the launcher, not the dock, not alt-tab. That
# shipped in 1.0.3's first .deb and was only caught by hand.
#
# postinst/postrm run gtk-update-icon-cache and update-desktop-database so
# the icon and menu entry show up without the user having to log out.
set -euo pipefail

VERSION="${1:?usage: build-deb.sh <version> <linux-binary> <out-dir>}"
BINARY="${2:?usage: build-deb.sh <version> <linux-binary> <out-dir>}"
OUT_DIR="${3:?usage: build-deb.sh <version> <linux-binary> <out-dir>}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

DEB="$WORK/s3browser_${VERSION}_amd64"
mkdir -p "$DEB/DEBIAN" "$DEB/usr/bin" "$DEB/usr/share/applications" \
  "$DEB/usr/share/pixmaps" "$DEB/usr/share/doc/s3browser"
for size in 16 22 24 32 48 64 128 256 512; do
  mkdir -p "$DEB/usr/share/icons/hicolor/${size}x${size}/apps"
done

python3 - "$ROOT" "$DEB" <<'PY'
import sys
from PIL import Image
root, deb = sys.argv[1], sys.argv[2]
src = Image.open(f"{root}/assets/brand/s3-browser-icon.png").convert("RGBA")
for size in (16, 22, 24, 32, 48, 64, 128, 256, 512):
    src.resize((size, size), Image.LANCZOS).save(
        f"{deb}/usr/share/icons/hicolor/{size}x{size}/apps/s3browser.png"
    )
src.save(f"{deb}/usr/share/pixmaps/s3browser.png")
PY

cat > "$DEB/usr/share/applications/s3browser.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=S3 Browser
Comment=Desktop S3 client for AWS S3 and S3-compatible stores
Comment[vi]=Trình duyệt S3 cho AWS S3 và các dịch vụ tương thích S3
Exec=s3browser
Icon=s3browser
Terminal=false
Categories=Utility;Network;
Keywords=S3;AWS;MinIO;R2;bucket;storage;
EOF

cat > "$DEB/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = "configure" ]; then
    command -v gtk-update-icon-cache >/dev/null 2>&1 &&
        gtk-update-icon-cache -f -t /usr/share/icons/hicolor >/dev/null 2>&1 || true
    command -v update-desktop-database >/dev/null 2>&1 &&
        update-desktop-database -q /usr/share/applications >/dev/null 2>&1 || true
fi
exit 0
EOF

cat > "$DEB/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = "remove" ] || [ "$1" = "purge" ]; then
    command -v gtk-update-icon-cache >/dev/null 2>&1 &&
        gtk-update-icon-cache -f -t /usr/share/icons/hicolor >/dev/null 2>&1 || true
    command -v update-desktop-database >/dev/null 2>&1 &&
        update-desktop-database -q /usr/share/applications >/dev/null 2>&1 || true
fi
exit 0
EOF

chmod 755 "$DEB/DEBIAN/postinst" "$DEB/DEBIAN/postrm"
cp "$BINARY" "$DEB/usr/bin/s3browser"
chmod 755 "$DEB/usr/bin/s3browser"
cp "$ROOT/LICENSE" "$DEB/usr/share/doc/s3browser/copyright"

SIZE=$(du -sk "$DEB" --exclude=DEBIAN | cut -f1)
cat > "$DEB/DEBIAN/control" <<EOF
Package: s3browser
Version: ${VERSION}
Section: utils
Priority: optional
Architecture: amd64
Depends: libc6 (>= 2.35), libxcb1, libxkbcommon0, libxkbcommon-x11-0, libvulkan1
Recommends: gnome-keyring | kwalletmanager
Installed-Size: ${SIZE}
Maintainer: duykhanhxx03 <khanhtd@falcongames.com>
Homepage: https://github.com/duykhanhxx03/s3-browser
Description: Desktop S3 client for AWS S3 and S3-compatible stores
 A desktop S3 browser built on GPUI. Talks to Amazon S3 and to
 S3-compatible stores - Cloudflare R2, Backblaze B2, Wasabi,
 DigitalOcean Spaces, MinIO. Secret keys are stored in the system
 credential store (Secret Service), never in config files.
EOF

mkdir -p "$OUT_DIR"
dpkg-deb --build --root-owner-group "$DEB" "$OUT_DIR/s3browser_${VERSION}_amd64.deb"
echo "built: $OUT_DIR/s3browser_${VERSION}_amd64.deb"
