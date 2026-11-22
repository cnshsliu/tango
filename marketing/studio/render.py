"""Render a tmux `capture-pane -e` dump (ANSI SGR text) into a PNG in a macOS-style window.

    from render import Renderer
    r = Renderer(cols, rows); img = r.render(ansi_text, cursor=(x, y), title="tango")
"""
import re
import unicodedata
from PIL import Image, ImageDraw, ImageFont

FONT_DIR = "/Users/lucas/Library/Fonts"
FONT = f"{FONT_DIR}/MapleMono-NF-CN-Regular.ttf"
FONT_BOLD = f"{FONT_DIR}/MapleMono-NF-CN-Bold.ttf"
EMOJI = "/System/Library/Fonts/Apple Color Emoji.ttc"

BG = (22, 24, 33)
FG = (205, 211, 222)
# xterm-ish palette tuned to a dark theme
BASE16 = [
    (30, 32, 44), (237, 102, 117), (152, 205, 125), (231, 195, 115),
    (110, 160, 240), (196, 140, 232), (102, 204, 214), (196, 201, 212),
    (92, 98, 120), (255, 128, 140), (176, 226, 150), (246, 214, 140),
    (140, 184, 255), (216, 166, 250), (130, 226, 236), (236, 240, 246),
]


def xterm256(n):
    if n < 16:
        return BASE16[n]
    if n < 232:
        n -= 16
        steps = [0, 95, 135, 175, 215, 255]
        return (steps[n // 36], steps[(n // 6) % 6], steps[n % 6])
    v = 8 + (n - 232) * 10
    return (v, v, v)


def width(ch):
    if unicodedata.east_asian_width(ch) in ("W", "F"):
        return 2
    o = ord(ch)
    return 2 if 0x1F300 <= o <= 0x1FAFF else 1


SGR = re.compile(r"\x1b\[([0-9;:]*)m")


class Style:
    __slots__ = ("fg", "bg", "bold", "dim", "italic", "under", "rev")

    def __init__(self):
        self.reset()

    def reset(self):
        self.fg = self.bg = None
        self.bold = self.dim = self.italic = self.under = self.rev = False

    def copy(self):
        s = Style()
        for k in self.__slots__:
            setattr(s, k, getattr(self, k))
        return s

    def apply(self, params):
        ps = [int(p) if p.isdigit() else 0 for p in re.split("[;:]", params)] if params else [0]
        i = 0
        while i < len(ps):
            p = ps[i]
            if p == 0:
                self.reset()
            elif p == 1:
                self.bold = True
            elif p == 2:
                self.dim = True
            elif p == 3:
                self.italic = True
            elif p == 4:
                self.under = True
            elif p == 7:
                self.rev = True
            elif p == 22:
                self.bold = self.dim = False
            elif p == 23:
                self.italic = False
            elif p == 24:
                self.under = False
            elif p == 27:
                self.rev = False
            elif 30 <= p <= 37:
                self.fg = BASE16[p - 30]
            elif 90 <= p <= 97:
                self.fg = BASE16[p - 90 + 8]
            elif 40 <= p <= 47:
                self.bg = BASE16[p - 40]
            elif 100 <= p <= 107:
                self.bg = BASE16[p - 100 + 8]
            elif p == 39:
                self.fg = None
            elif p == 49:
                self.bg = None
            elif p in (38, 48) and i + 1 < len(ps):
                if ps[i + 1] == 5 and i + 2 < len(ps):
                    c = xterm256(ps[i + 2])
                    i += 2
                elif ps[i + 1] == 2 and i + 4 < len(ps):
                    c = tuple(ps[i + 2:i + 5])
                    i += 4
                else:
                    c = None
                if p == 38:
                    self.fg = c
                else:
                    self.bg = c
            i += 1


def parse(text, cols, rows):
    """→ grid[row] = list of (char, Style, cellwidth) placed at their starting column."""
    grid = []
    st = Style()
    for line in text.split("\n")[:rows]:
        cells, col, pos = [], 0, 0
        for m in list(SGR.finditer(line)) + [None]:
            end = m.start() if m else len(line)
            for ch in line[pos:end]:
                if ch < " ":
                    continue
                w = width(ch)
                if col + w > cols:
                    break
                cells.append((col, ch, st.copy(), w))
                col += w
            if m:
                st.apply(m.group(1))
                pos = m.end()
        grid.append(cells)
    while len(grid) < rows:
        grid.append([])
    return grid


def mix(a, b, t):
    return tuple(int(a[i] * (1 - t) + b[i] * t) for i in range(3))


class Renderer:
    """Window chrome + terminal grid. scale=2 gives crisp images for X (retina-like)."""

    def __init__(self, cols, rows, scale=2, size=15):
        self.cols, self.rows, self.s = cols, rows, scale
        self.font = ImageFont.truetype(FONT, size * scale)
        self.bold = ImageFont.truetype(FONT_BOLD, size * scale)
        try:
            self.emoji = ImageFont.truetype(EMOJI, 40)  # bitmap font: only fixed sizes work
        except OSError:
            self.emoji = None
        self.cw = round(self.font.getlength("M"))
        self.ch = round(size * scale * 1.42)
        self.pad = 18 * scale
        self.bar = 34 * scale
        self.margin = 36 * scale
        self.ascent = self.font.getmetrics()[0]

    def size(self):
        w = self.cols * self.cw + 2 * self.pad + 2 * self.margin
        h = self.rows * self.ch + 2 * self.pad + self.bar + 2 * self.margin
        return w, h

    def render(self, text, cursor=None, title="tango"):
        s = self.s
        W, H = self.size()
        img = Image.new("RGB", (W, H), (0, 0, 0))
        # backdrop gradient
        top, bot = (38, 52, 92), (14, 16, 28)
        bd = ImageDraw.Draw(img)
        for y in range(H):
            bd.line([(0, y), (W, y)], fill=mix(top, bot, y / H))
        m = self.margin
        win = (m, m, W - m, H - m)
        shadow = Image.new("RGBA", (W, H), (0, 0, 0, 0))
        sd = ImageDraw.Draw(shadow)
        sd.rounded_rectangle((m + 4 * s, m + 10 * s, W - m + 4 * s, H - m + 10 * s), 14 * s, fill=(0, 0, 0, 110))
        img.paste(Image.new("RGB", (W, H), (0, 0, 0)), (0, 0), shadow)
        d = ImageDraw.Draw(img)
        d.rounded_rectangle(win, 12 * s, fill=BG, outline=(60, 64, 82), width=s)
        d.rectangle((m + s, m + self.bar - 1, W - m - s, m + self.bar), fill=(44, 47, 62))
        for i, c in enumerate([(255, 95, 87), (254, 188, 46), (40, 200, 64)]):
            cx, cy = m + 22 * s + i * 20 * s, m + self.bar // 2
            d.ellipse((cx - 6 * s, cy - 6 * s, cx + 6 * s, cy + 6 * s), fill=c)
        tw = self.font.getlength(title)
        d.text(((W - tw) / 2, m + (self.bar - self.ch) / 2 + 3 * s), title, font=self.font, fill=(150, 156, 175))
        ox, oy = m + self.pad, m + self.bar + self.pad
        for r, cells in enumerate(parse(text, self.cols, self.rows)):
            y = oy + r * self.ch
            for col, chx, st, w in cells:
                fg, bg = st.fg or FG, st.bg
                if st.rev:
                    fg, bg = bg or BG, fg
                if st.dim:
                    fg = mix(fg, BG, 0.45)
                x = ox + col * self.cw
                if bg:
                    d.rectangle((x, y, x + w * self.cw, y + self.ch), fill=bg)
                if chx == " ":
                    continue
                self._glyph(img, d, x, y, chx, fg, st, w)
                if st.under:
                    d.line((x, y + self.ch - 3 * s, x + w * self.cw, y + self.ch - 3 * s), fill=fg, width=s)
        if cursor:
            cx, cy = cursor
            x, y = ox + cx * self.cw, oy + cy * self.ch
            d.rectangle((x, y + 2 * s, x + 2 * s, y + self.ch - 2 * s), fill=(230, 230, 240))
        return img

    def _glyph(self, img, d, x, y, chx, fg, st, w):
        o = ord(chx)
        if self.emoji and self._missing(chx):
            g = Image.new("RGBA", (64, 64), (0, 0, 0, 0))
            ImageDraw.Draw(g).text((0, 0), chx, font=self.emoji, embedded_color=True)
            g = g.crop(g.getbbox() or (0, 0, 1, 1))
            side = int(self.ch * 0.78)
            g = g.resize((side, side), Image.LANCZOS)
            img.paste(g, (int(x + (w * self.cw - side) / 2), int(y + (self.ch - side) / 2)), g)
            return
        if self._box(d, x, y, chx, fg):
            return
        font = self.bold if st.bold else self.font
        d.text((x, y + (self.ch - self.ascent * 1.18) / 2), chx, font=font, fill=fg)

    def _missing(self, chx):
        """True when the text font would draw its tofu box for chx."""
        if not hasattr(self, "_tofu"):
            self._tofu = bytes(self.font.getmask("\U0010fffd"))
            self._miss = {}
        if chx not in self._miss:
            self._miss[chx] = ord(chx) > 0x2000 and bytes(self.font.getmask(chx)) == self._tofu
        return self._miss[chx]

    def _box(self, d, x, y, chx, fg):
        """Draw box-drawing lines geometrically so borders join seamlessly across rows."""
        o = ord(chx)
        if not (0x2500 <= o <= 0x257F or 0x2580 <= o <= 0x259F):
            return False
        cw, ch, s = self.cw, self.ch, self.s
        mx, my = x + cw // 2, y + ch // 2
        heavy = chx in "━┃┏┓┗┛┣┫┳┻╋"
        lw = 2 * s if heavy else s
        arms = {
            "─": "lr", "━": "lr", "│": "ud", "┃": "ud", "┌": "rd", "┏": "rd", "┐": "ld", "┓": "ld",
            "└": "ru", "┗": "ru", "┘": "lu", "┛": "lu", "├": "udr", "┣": "udr", "┤": "udl", "┫": "udl",
            "┬": "lrd", "┳": "lrd", "┴": "lru", "┻": "lru", "┼": "lrud", "╋": "lrud",
            "╴": "l", "╶": "r", "╵": "u", "╷": "d",
        }
        if chx in "╭╮╰╯":
            r = min(cw, ch) // 2
            box = {"╭": (mx, my, mx + 2 * r, my + 2 * r, 180, 270), "╮": (mx - 2 * r, my, mx, my + 2 * r, 270, 360),
                   "╰": (mx, my - 2 * r, mx + 2 * r, my, 90, 180), "╯": (mx - 2 * r, my - 2 * r, mx, my, 0, 90)}[chx]
            d.arc(box[:4], box[4], box[5], fill=fg, width=lw)
            hor = (mx + r, x + cw) if chx in "╭╰" else (x, mx - r)
            ver = (my + r, y + ch) if chx in "╭╮" else (y, my - r)
            d.line((hor[0], my, hor[1], my), fill=fg, width=lw)
            d.line((mx, ver[0], mx, ver[1]), fill=fg, width=lw)
            return True
        if chx in arms:
            for a in arms[chx]:
                if a == "l":
                    d.line((x, my, mx, my), fill=fg, width=lw)
                elif a == "r":
                    d.line((mx, my, x + cw, my), fill=fg, width=lw)
                elif a == "u":
                    d.line((mx, y, mx, my), fill=fg, width=lw)
                else:
                    d.line((mx, my, mx, y + ch), fill=fg, width=lw)
            return True
        blocks = {"█": (0, 1), "▀": (0, .5), "▄": (.5, 1), "▌": None, "▐": None}
        if chx in blocks and blocks[chx]:
            a, b = blocks[chx]
            d.rectangle((x, y + ch * a, x + cw, y + ch * b), fill=fg)
            return True
        return False
