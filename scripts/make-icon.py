#!/usr/bin/env python3
"""Draws Waggle's placeholder icon: a honeycomb cell with the figure-eight of
the waggle dance. Writes crates/waggle/ui/waggle.png (256 px) and
assets/waggle.ico (16-256 px) and crates/waggle/ui/tray-32.rgba (raw
32x32 RGBA for the tray). Requires Pillow."""
import math
from pathlib import Path
from PIL import Image, ImageDraw

root = Path(__file__).resolve().parent.parent
S = 1024  # draw large, then downsample for smooth edges
img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
d = ImageDraw.Draw(img)
cx = cy = S / 2
r = S * 0.47
hexagon = [(cx + r * math.cos(math.radians(60 * i - 90)), cy + r * math.sin(math.radians(60 * i - 90))) for i in range(6)]
d.polygon(hexagon, fill=(224, 168, 58, 255))
# figure eight (lemniscate) in the background colour
pts = []
for i in range(721):
    t = math.radians(i / 2)
    s = 1 + math.sin(t) ** 2
    x = cx + S * 0.30 * math.cos(t) / s
    y = cy + S * 0.30 * math.sin(t) * math.cos(t) / s
    pts.append((x, y))
ink = (15, 17, 20, 255)
w = S * 0.035  # stroke radius; stamped discs avoid PIL's jagged thick-line joins


def stamp(points):
    for x, y in points:
        d.ellipse((x - w, y - w, x + w, y + w), fill=ink)


stamp(pts)
# the straight run through the middle, which is the part that points the way
a, b = (cx - S * 0.13, cy + S * 0.13), (cx + S * 0.13, cy - S * 0.13)
stamp([(a[0] + (b[0] - a[0]) * k / 200, a[1] + (b[1] - a[1]) * k / 200) for k in range(201)])
png = img.resize((256, 256), Image.LANCZOS)
png.save(root / "crates/waggle/ui/waggle.png")
img.resize((256, 256), Image.LANCZOS).save(root / "assets/waggle.ico", sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
# raw 32x32 RGBA for the tray, embedded as-is so no PNG decoder is needed
(root / "crates/waggle/ui/tray-32.rgba").write_bytes(img.resize((32, 32), Image.LANCZOS).tobytes())
print("wrote icon")
