#!/usr/bin/env python3
"""Cover cards for the domestic platforms: big title on the dark backdrop + one screenshot.

    .venv/bin/python cover.py <shot.png> <out.png> <W>x<H> "<title line 1>|<line 2>" ["<subtitle>"]

Sizes used: 小红书 1242x1656 (3:4), 公众号 900x383 (2.35:1) and 500x500 (1:1 share card).
"""
import sys
from PIL import Image, ImageDraw, ImageFont

BOLD = "/Users/lucas/Library/Fonts/MapleMono-NF-CN-Bold.ttf"
REG = "/Users/lucas/Library/Fonts/MapleMono-NF-CN-Regular.ttf"
AMBER, WHITE, DIM = (245, 166, 35), (240, 242, 248), (150, 156, 178)


def main(shot, out, size, title, sub=""):
    W, H = map(int, size.split("x"))
    img = Image.new("RGB", (W, H))
    d = ImageDraw.Draw(img)
    for y in range(H):  # same navy gradient as the screenshot backdrop
        t = y / H
        d.line([(0, y), (W, y)], fill=tuple(int(a * (1 - t) + b * t) for a, b in zip((38, 52, 92), (14, 16, 28))))
    wide = W / H > 1.6
    pad = int(min(W, H) * 0.07)
    lines = title.split("|")
    tsize = int((H * 0.16 if wide else W * 0.105) / max(1, len(lines) - 0.6))
    tf, sf, mf = ImageFont.truetype(BOLD, tsize), ImageFont.truetype(REG, int(tsize * 0.42)), ImageFont.truetype(BOLD, int(tsize * 0.38))
    d.text((pad, pad), "Tango", font=mf, fill=AMBER)
    y = pad + int(tsize * 0.75)
    for ln in lines:
        d.text((pad, y), ln, font=tf, fill=WHITE)
        y += int(tsize * 1.2)
    if sub:
        d.text((pad, y + int(tsize * 0.1)), sub, font=sf, fill=DIM)
        y += int(tsize * 0.8)
    s = Image.open(shot).convert("RGB")
    if wide:  # screenshot on the right half, title on the left
        box = (int(W * 0.52), pad // 2, W - pad // 2, H - pad // 2)
    else:  # screenshot fills the lower part
        box = (pad // 2, y + pad // 2, W - pad // 2, H - pad // 2)
    bw, bh = box[2] - box[0], box[3] - box[1]
    k = min(bw / s.width, bh / s.height)
    s = s.resize((int(s.width * k), int(s.height * k)), Image.LANCZOS)
    img.paste(s, (box[0] + (bw - s.width) // 2, box[1] + (bh - s.height) // 2))
    img.save(out, quality=92)
    print(out, img.size)


if __name__ == "__main__":
    main(*sys.argv[1:])
