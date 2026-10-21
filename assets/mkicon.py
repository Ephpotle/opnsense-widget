"""Draw the widget's app icon: accent-blue rounded tile, white router glyph (body, LEDs, antennas).
Writes icon.png (256 px) and icon_64.rgba (raw RGBA the app embeds for its window icons)
next to this script. Run: python widget/assets/mkicon.py (needs Pillow)."""
import os
from PIL import Image, ImageDraw

out = os.path.dirname(os.path.abspath(__file__))
S = 1024  # draw large, downsample for smooth edges

img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
d = ImageDraw.Draw(img)
# Tile: Windows default accent family, a subtle vertical gradient.
top, bot = (0x1a, 0x7f, 0xd9), (0x00, 0x5a, 0xa8)
tile = Image.new("RGBA", (S, S))
td = ImageDraw.Draw(tile)
for y in range(S):
    t = y / (S - 1)
    td.line([(0, y), (S, y)], fill=tuple(round(a + (b - a) * t) for a, b in zip(top, bot)) + (255,))
mask = Image.new("L", (S, S), 0)
ImageDraw.Draw(mask).rounded_rectangle([40, 40, S - 40, S - 40], radius=210, fill=255)
img.paste(tile, (0, 0), mask)

white = (255, 255, 255, 255)
led = (0x10, 0x6c, 0xc4, 255)  # tile colour, so the LEDs read as cut-outs
# Router body with status LEDs.
d.rounded_rectangle([190, 560, S - 190, 800], radius=70, fill=white)
for x in (330, 450, 570):
    d.ellipse([x - 34, 680 - 34, x + 34, 680 + 34], fill=led)
d.rounded_rectangle([680, 650, 840, 710], radius=30, fill=led)
# Antennas.
for x in (330, S - 330):
    d.rounded_rectangle([x - 30, 230, x + 30, 580], radius=30, fill=white)
    d.ellipse([x - 52, 200, x + 52, 304], fill=white)

img.resize((256, 256), Image.LANCZOS).save(os.path.join(out, "icon.png"))
open(os.path.join(out, "icon_64.rgba"), "wb").write(img.resize((64, 64), Image.LANCZOS).tobytes())
print("wrote icon.png, icon_64.rgba")
