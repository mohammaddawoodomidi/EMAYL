#!/usr/bin/env bash
# Rebuilds every icon file from emayl-source.png (the glossy blue envelope).
# Needs: python3 + Pillow, ImageMagick `convert`. Run from this folder:  ./make-icons.sh
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
mkdir -p png
python3 - <<'PY'
from PIL import Image
src = Image.open("emayl-source.png").convert("RGBA")
w, h = src.size
# Trim the empty dark margin a little so the envelope reads better when small.
m = round(w * 0.095)
art = src.crop((m, m, w - m, h - m))
for n in (16, 24, 32, 48, 64, 128, 256, 512):
    art.resize((n, n), Image.LANCZOS).save(f"png/emayl-{n}.png", optimize=True)
open("window-64.rgba", "wb").write(Image.open("png/emayl-64.png").convert("RGBA").tobytes())
PY
convert png/emayl-16.png png/emayl-24.png png/emayl-32.png png/emayl-48.png \
        png/emayl-64.png png/emayl-128.png png/emayl-256.png emayl.ico
# Pictures for the Windows installer pages.
w=../../packaging/windows
convert -size 164x314 gradient:'#0A3BD6'-'#041A66' \( png/emayl-128.png -resize 112x112 \) \
        -gravity north -geometry +0+44 -composite -define bmp:format=bmp3 BMP3:$w/sidebar.bmp
convert -size 150x57 xc:white \( png/emayl-64.png -resize 44x44 \) \
        -gravity east -geometry +10+0 -composite -define bmp:format=bmp3 BMP3:$w/header.bmp
echo "icon files rebuilt"
