#!/usr/bin/env python3
"""Draws the installer's Welcome/Finish sidebar (164x314) and page header
(150x57) as BMPs in assets/, from the app icon. Requires Pillow and a bold
sans font (Liberation Sans Bold, or pass a path as the first argument)."""
import sys
from pathlib import Path
from PIL import Image, ImageDraw, ImageFont

root = Path(__file__).resolve().parent.parent
font_path = sys.argv[1] if len(sys.argv) > 1 else "/usr/share/fonts/liberation/LiberationSans-Bold.ttf"
icon = Image.open(root / "crates/waggle/ui/waggle.png").convert("RGBA")
dark, gold, dim = (15, 17, 20), (224, 168, 58), (138, 146, 156)

sidebar = Image.new("RGB", (164, 314), dark)
mark = icon.resize((88, 88), Image.LANCZOS)
sidebar.paste(mark, ((164 - 88) // 2, 64), mark)
draw = ImageDraw.Draw(sidebar)
title = ImageFont.truetype(font_path, 20)
small = ImageFont.truetype(font_path, 10)
for text, font, colour, y in [("WAGGLE", title, gold, 172), ("intel · pings · PAP · mining", small, dim, 202)]:
    width = draw.textlength(text, font=font)
    draw.text(((164 - width) / 2, y), text, font=font, fill=colour)
sidebar.save(root / "assets/installer-sidebar.bmp")

header = Image.new("RGB", (150, 57), (255, 255, 255))
mark = icon.resize((40, 40), Image.LANCZOS)
header.paste(mark, (150 - 40 - 8, (57 - 40) // 2), mark)
header.save(root / "assets/installer-header.bmp")
print("wrote installer art")
