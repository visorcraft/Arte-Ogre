#!/usr/bin/env bash
# Build the Arte Ogre AppImage — the release deliverable (PROD), not a dev build.
# Produces a single-file AppImage in target/appimage/. Mirrors README -> Install.
# Requires linuxdeploy + appimagetool on PATH and `magick` (ImageMagick) for the icon.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

for tool in linuxdeploy appimagetool magick; do
    command -v "$tool" >/dev/null 2>&1 || { cat >&2 <<EOF
ERROR: required tool '$tool' not found on PATH.

One-time setup (linuxdeploy + appimagetool are continuous static builds):
  curl -L https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage -o ~/.local/bin/linuxdeploy
  curl -L https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage -o ~/.local/bin/appimagetool
  chmod +x ~/.local/bin/linuxdeploy ~/.local/bin/appimagetool
'magick' is provided by ImageMagick.
EOF
        exit 1; }
done

echo "==> cargo build --release -p ogre"
cargo build --release -p ogre

outdir="target/appimage"
appdir="$outdir/AppDir"
rm -rf "$appdir"
mkdir -p "$appdir/usr/bin" \
         "$appdir/usr/share/applications" \
         "$appdir/usr/share/icons/hicolor/256x256/apps"

cp target/release/ogre "$appdir/usr/bin/arte-ogre"
magick assets/ArteOgre.png -resize 256x256 \
    "$appdir/usr/share/icons/hicolor/256x256/apps/arte-ogre.png"

cat > "$appdir/usr/share/applications/arte-ogre.desktop" <<'EOF'
[Desktop Entry]
Name=Arte Ogre
Exec=arte-ogre %F
Icon=arte-ogre
Type=Application
Categories=Graphics;2DGraphics;RasterGraphics;
Comment=GPU-native image editor
Terminal=false
StartupWMClass=arte-ogre
MimeType=image/png;image/jpeg;image/webp;image/avif;image/tiff;image/x-ora;
EOF

echo "==> linuxdeploy -> AppImage"
# NO_STRIP=1 is required where libraries use DT_RELR relocations.
( cd "$outdir" && NO_STRIP=1 linuxdeploy \
    --appdir AppDir \
    --desktop-file AppDir/usr/share/applications/arte-ogre.desktop \
    --icon-file AppDir/usr/share/icons/hicolor/256x256/apps/arte-ogre.png \
    --output appimage )

echo "AppImage built: $(ls -1 "$outdir"/*.AppImage)"
