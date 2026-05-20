#!/usr/bin/env bash
# make-mac-icon.sh — build a macOS .icns (+ 1024 PNG) from a source image.
#
# Renders the source to a 1024² PNG, rasterises it into a complete
# `.iconset` with every retina pair Apple expects, then packs it with
# `iconutil`. This is Apple's first-party packer; the resulting .icns has
# the layout/ordering macOS's icon services are best-tested with.
#
# Usage:
#   script/make-mac-icon.sh <input.svg|input.png> [--out-dir DIR] [--name NAME]
#
# Defaults:
#   out-dir  same directory as <input>
#   name     basename of <input> without extension
#
# Produces NAME.icns and NAME.png in out-dir. The intermediate
# NAME.iconset directory is removed on success.
#
# Requirements:
#   - ImageMagick `convert`  (for SVG → PNG)
#   - macOS built-ins `sips` and `iconutil`

set -euo pipefail

usage() {
  echo "usage: $(basename "$0") <input.svg|input.png> [--out-dir DIR] [--name NAME]" >&2
  exit 2
}

[[ $# -ge 1 ]] || usage

input="$1"; shift
out_dir=""
name=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --out-dir) out_dir="$2"; shift 2 ;;
    --name)    name="$2";    shift 2 ;;
    -h|--help) usage ;;
    *) echo "unknown argument: $1" >&2; usage ;;
  esac
done

[[ -f "$input" ]] || { echo "input not found: $input" >&2; exit 1; }
out_dir="${out_dir:-$(dirname "$input")}"
name="${name:-$(basename "$input" | sed 's/\.[^.]*$//')}"

for cmd in convert sips iconutil; do
  command -v "$cmd" >/dev/null || { echo "missing required tool: $cmd" >&2; exit 1; }
done

mkdir -p "$out_dir"
master="$out_dir/$name.png"
iconset="$out_dir/$name.iconset"

# 1. Master 1024² PNG. `-density 1024` makes ImageMagick rasterise SVGs
#    at the target resolution rather than its 72 DPI default; for a PNG
#    input this is a no-op resize. Render to a temp file first so we
#    don't clobber the source when it's already a PNG at this path.
#
#    `PNG32:` forces 8-bit RGBA (PNG color type 6) regardless of how few
#    colors the source uses. macOS's icon services apply a different
#    visual treatment to grayscale+alpha icons (color type 4) — they
#    pick up a subtle shiny highlight band that RGBA icons don't get —
#    so we always go through the RGBA path.
master_tmp="$(mktemp -t "$name.XXXXXX").png"
trap 'rm -f "$master_tmp"' EXIT
convert -background none -density 1024 "$input" -resize 1024x1024 PNG32:"$master_tmp"
cp "$master_tmp" "$master"

# 2. Build the .iconset directory with every retina pair iconutil wants.
rm -rf "$iconset"
mkdir "$iconset"
pairs=(
  "16   icon_16x16.png"
  "32   icon_16x16@2x.png"
  "32   icon_32x32.png"
  "64   icon_32x32@2x.png"
  "128  icon_128x128.png"
  "256  icon_128x128@2x.png"
  "256  icon_256x256.png"
  "512  icon_256x256@2x.png"
  "512  icon_512x512.png"
  "1024 icon_512x512@2x.png"
)
for entry in "${pairs[@]}"; do
  size="${entry%% *}"
  fname="${entry##* }"
  sips -z "$size" "$size" "$master" --out "$iconset/$fname" >/dev/null
done

# 3. Pack and clean up.
iconutil -c icns "$iconset" -o "$out_dir/$name.icns"
rm -rf "$iconset"

echo "wrote $out_dir/$name.icns"
echo "wrote $out_dir/$name.png"
