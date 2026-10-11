"""Keep README PNGs under the size budget. Files already small enough stay
lossless; bigger ones get a dithered 255-color palette plus one fully
transparent color (the frames have hard, bezel-colored edges, so a binary
alpha mask is enough)."""
import os
import sys

from PIL import Image

BUDGET = 380 * 1024
BEZEL = (29, 26, 34)

for path in sys.argv[1:]:
    before = os.path.getsize(path)
    if before <= BUDGET:
        print(f"{os.path.basename(path)}: {before // 1024} KB (kept lossless)")
        continue
    im = Image.open(path).convert("RGBA")
    alpha = im.getchannel("A")
    rgb = Image.new("RGB", im.size, BEZEL)
    rgb.paste(im, mask=alpha)
    palette = rgb.quantize(colors=255, method=Image.Quantize.MEDIANCUT)
    q = rgb.quantize(palette=palette, dither=Image.Dither.FLOYDSTEINBERG)
    clear = alpha.point(lambda a: 255 if a < 8 else 0)
    q.paste(255, mask=clear)
    q.save(path, optimize=True, transparency=255)
    print(f"{os.path.basename(path)}: {before // 1024} KB -> {os.path.getsize(path) // 1024} KB")
