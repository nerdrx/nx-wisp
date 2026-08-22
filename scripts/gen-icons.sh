#!/usr/bin/env bash
# Rasterise the NX Wisp mark from its SVG masters.
#
#   scripts/gen-icons.sh
#
# Writes packaging/icons/<size>x<size>.png for every size a .desktop entry and
# NX Hub's launcher tiles want. The PNGs are COMMITTED — build-appimage.sh only
# ever copies them, so cutting a release never needs a rasteriser installed.
# Run this by hand after editing a master, and commit the result.
#
# DESIGN.md §8: three variants exist and one file cannot span 16 -> 512px.
#   icon.svg        master, full bevel/facets/lit edge      48px and up
#   icon-small.svg  wider crystal, flat fills, no edge      16, 24, 32
#   tray.svg        flat violet, knocked-out holes          tray only
# Never scale the master below 48px.
set -euo pipefail
cd "$(dirname "$0")/.."

ICONS=packaging/icons
SMALL_SIZES=(16 24 32)
MASTER_SIZES=(48 64 128 256 512)

command -v rsvg-convert >/dev/null || {
    echo "rsvg-convert not found (Arch: pacman -S librsvg). The committed PNGs"
    echo "in $ICONS are still fine — you only need this to regenerate them."
    exit 1
}

render() { # <svg> <size> <out>
    rsvg-convert --width="$2" --height="$2" --background-color=none "$1" -o "$3"
    printf '    %-14s %s\n' "$(basename "$3")" "$(basename "$1")"
}

echo "==> small variant (icon-small.svg)"
for s in "${SMALL_SIZES[@]}"; do
    render "$ICONS/icon-small.svg" "$s" "$ICONS/${s}x${s}.png"
done

echo "==> master (icon.svg)"
for s in "${MASTER_SIZES[@]}"; do
    render "$ICONS/icon.svg" "$s" "$ICONS/${s}x${s}.png"
done

echo "==> tray (tray.svg)"
render "$ICONS/tray.svg" 64 "$ICONS/tray-64.png"

echo "==> done. Commit packaging/icons/*.png alongside the master you changed."
