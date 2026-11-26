"""Compose the README images from ResMon's own frame dumps.

Shots come from a demo run (RESMON_DEMO=1: real measurements, process names replaced by common apps),
taken with RESMON_OPEN / RESMON_SCRIPT and converted from %TEMP%/resmon_*.bgra to RGBA PNG.

    python tools/compose_hero.py <shots dir> <out dir>

expects in <shots dir>: full.png (1800x1125, memory map), cores.png (per-core view), menu.png (map with the
right-click menu open), panel.png (330x988) and tab.png (54x223). Writes hero.png, cores.png, menu.png, panel.png.
"""
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter, ImageFont

src, out = Path(sys.argv[1]), Path(sys.argv[2])
out.mkdir(parents=True, exist_ok=True)
FONTS = Path("C:/Windows/Fonts")
bold = lambda s: ImageFont.truetype(str(FONTS / "segoeuib.ttf"), s)
semi = lambda s: ImageFont.truetype(str(FONTS / "seguisb.ttf"), s)
light = lambda s: ImageFont.truetype(str(FONTS / "segoeuisl.ttf"), s)


def backdrop(w, h):
    """Deep blue-violet gradient with two soft glows, like a desktop wallpaper behind the widget."""
    import numpy as np

    y, x = np.mgrid[0:h, 0:w]
    t = ((x / w) * 0.6 + (y / h) * 0.4)[..., None]  # diagonal, top-left dark to bottom-right violet
    rgb = np.array([11, 14, 28]) * (1 - t) + np.array([38, 26, 72]) * t
    img = Image.fromarray(rgb.astype(np.uint8))
    glow = Image.new("RGB", (w, h), 0)
    d = ImageDraw.Draw(glow)
    d.ellipse((int(w * 0.55), -int(h * 0.3), int(w * 1.25), int(h * 0.6)), fill=(40, 90, 160))
    d.ellipse((-int(w * 0.2), int(h * 0.55), int(w * 0.35), int(h * 1.3)), fill=(90, 40, 120))
    glow = glow.filter(ImageFilter.GaussianBlur(w // 8))
    return Image.blend(img, Image.composite(glow, img, glow.convert("L")), 0.55).convert("RGBA")


def shadowed(canvas, im, xy, blur=40, alpha=170, dy=24):
    """Paste an RGBA window with a soft drop shadow taken from its own alpha."""
    pad = blur * 3
    sh = Image.new("RGBA", (im.width + 2 * pad, im.height + 2 * pad), (0, 0, 0, 0))
    mask = im.getchannel("A").point(lambda v: alpha if v > 0 else 0)
    sh.paste((0, 0, 0, 255), (pad, pad), mask)
    sh = sh.filter(ImageFilter.GaussianBlur(blur))
    canvas.alpha_composite(sh, (xy[0] - pad, xy[1] - pad + dy))
    canvas.alpha_composite(im, xy)


def scaled(im, f):
    return im.resize((round(im.width * f), round(im.height * f)), Image.LANCZOS)


full = Image.open(src / "full.png").convert("RGBA")
panel = Image.open(src / "panel.png").convert("RGBA")
tab = Image.open(src / "tab.png").convert("RGBA")

# ---------------------------------------------------------------- hero: full view, the panel beside it, the tab on the edge
W, H = 2400, 1360
hero = backdrop(W, H)
d = ImageDraw.Draw(hero)
d.text((120, 96), "ResMon", font=bold(92), fill=(236, 240, 250))
d.text((124, 214), "See who is eating your machine. A resource monitor that lives on the edge of the screen.",
       font=light(38), fill=(178, 188, 214))
fv = scaled(full, 0.9)
shadowed(hero, fv, (120, 310))
pv = scaled(panel, 1.0)
px = W - pv.width - 150
shadowed(hero, pv, (px, 300))
# the docked tab sits on the right edge, the panel slides out of it
tb = scaled(tab, 1.25)
shadowed(hero, tb, (W - tb.width, 300 + pv.height // 2 - tb.height // 2), blur=20, alpha=140, dy=8)
hero.convert("RGB").save(out / "hero.png", optimize=True)


# ---------------------------------------------------------------- gallery: each view on the same backdrop, with a caption
def card(im, caption, size, scale):
    c = backdrop(*size)
    v = scaled(im, scale)
    x, y = (size[0] - v.width) // 2, 110
    ImageDraw.Draw(c).text((x, 34), caption, font=semi(40), fill=(226, 232, 246))
    shadowed(c, v, (x, y), blur=30, dy=16)
    return c.convert("RGB")


cores = Image.open(src / "cores.png").convert("RGBA")
card(cores, "Per core: every core, its two threads, and what ran there", (1800, 1000), 0.82).save(out / "cores.png", optimize=True)
menu = Image.open(src / "menu.png").convert("RGBA").crop((880, 250, 1800, 1060))
card(menu, "Right-click anything", (1100, 960), 1.0).save(out / "menu.png", optimize=True)
card(panel, "The hover panel", (520, 1180), 1.0).save(out / "panel.png", optimize=True)
print("ok")
