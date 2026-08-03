#!/usr/bin/env bash
# Regenerate every icon from the SVGs in assets/.
#
# `tauri icon` downsamples a single master into every size, which loses the
# small ones — so afterwards we rebuild icon.icns from an iconset that
# substitutes the separately-drawn small artwork for the 16pt and 32pt entries.
set -euo pipefail

cd "$(dirname "$0")/.."

command -v rsvg-convert >/dev/null || {
  echo "rsvg-convert not found — brew install librsvg" >&2
  exit 1
}

icons=src-tauri/icons
iconset=$(mktemp -d)/gru.iconset
mkdir -p "$iconset"

render() { rsvg-convert -w "$2" -h "$2" "$1" -o "$3"; }

master=$(mktemp -d)/master.png
render assets/icon.svg 1024 "$master"
# Quiet unless it fails — it lists 60-odd Windows/iOS/Android files we don't ship.
if ! log=$(npx tauri icon "$master" -o "$icons" 2>&1); then
  echo "$log" >&2
  exit 1
fi
echo "generated the full icon set from assets/icon.svg"

# 16pt and 32pt, at both scales, use the simplified drawing.
render assets/icon-small.svg 16   "$iconset/icon_16x16.png"
render assets/icon-small.svg 32   "$iconset/icon_16x16@2x.png"
render assets/icon-small.svg 32   "$iconset/icon_32x32.png"
render assets/icon-small.svg 64   "$iconset/icon_32x32@2x.png"
for pair in "128 icon_128x128" "256 icon_128x128@2x" "256 icon_256x256" \
            "512 icon_256x256@2x" "512 icon_512x512" "1024 icon_512x512@2x"; do
  render assets/icon.svg "${pair% *}" "$iconset/${pair#* }.png"
done
iconutil -c icns "$iconset" -o "$icons/icon.icns"
render assets/icon-small.svg 32 "$icons/32x32.png"
echo "rebuilt icon.icns with size-specific artwork"

# tray-icon renders this at 18pt, so 36px is exactly 2x on Retina.
render assets/tray.svg 36 "$icons/tray.png"
echo "rendered the menu bar template glyph"
