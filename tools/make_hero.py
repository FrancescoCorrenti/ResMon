"""Compose docs/hero.png: a desktop mockup with the docked tab, the hover panel and the full view.

Built from the raw self-test screenshots (not committed: they show the author's own processes).
Usage: python tools/make_hero.py <dir with tab.png panel.png full.png> [out=docs/hero.png]
"""
import sys
from pathlib import Path
from PIL import Image, ImageDraw, ImageFilter

src = Path(sys.argv[1])
out = Path(sys.argv[2]) if len(sys.argv) > 2 else Path(__file__).resolve().parents[1] / "docs" / "hero.png"
W, H = 1600, 900


def rounded(im, r):
    mask = Image.new("L", im.size, 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, im.width - 1, im.height - 1], r, fill=255)
    im = im.convert("RGBA")
    im.putalpha(mask)
    return im


def paste_with_shadow(canvas, im, xy, blur=28, offset=(0, 14), alpha=150):
    sh = Image.new("RGBA", (im.width + blur * 4, im.height + blur * 4), (0, 0, 0, 0))
    a = im.split()[-1].point(lambda v: alpha if v else 0)
    sh.paste((0, 0, 0, 255), (blur * 2, blur * 2), a)
    sh = sh.filter(ImageFilter.GaussianBlur(blur))
    canvas.alpha_composite(sh, (xy[0] - blur * 2 + offset[0], xy[1] - blur * 2 + offset[1]))
    canvas.alpha_composite(im, xy)


# wallpaper: diagonal gradient with a soft glow
bg = Image.new("RGBA", (W, H))
px = bg.load()
for y in range(H):
    for x in range(W):
        t = (x / W) * 0.6 + (y / H) * 0.4
        px[x, y] = (int(18 + 30 * t), int(24 + 22 * t), int(48 + 60 * t), 255)
glow = Image.new("RGBA", (W, H), (0, 0, 0, 0))
ImageDraw.Draw(glow).ellipse([250, 120, 1150, 820], fill=(90, 120, 255, 70))
bg.alpha_composite(glow.filter(ImageFilter.GaussianBlur(160)))

# full view: gauges + treemap only (the process table holds command lines)
full = Image.open(src / "full.png").convert("RGB").crop((0, 0, 1440, 585))
full = full.resize((1080, int(585 * 1080 / 1440)), Image.LANCZOS)
paste_with_shadow(bg, rounded(full, 14), (70, 160))

# hover panel docked on the right edge
panel = Image.open(src / "panel.png").convert("RGB").crop((92, 64, 411, 886))
panel = panel.resize((int(panel.width * 0.86), int(panel.height * 0.86)), Image.LANCZOS)
paste_with_shadow(bg, rounded(panel, 14), (W - panel.width - 6, (H - panel.height) // 2))

# docked tab, drawn a second time lower on the edge as an inset callout
tab = Image.open(src / "tab.png").convert("RGB").crop((220, 150, 247, 354))
tab = rounded(tab, 8)
label_x = W - panel.width - 60
paste_with_shadow(bg, tab, (label_x, 690), blur=10, offset=(0, 4))

d = ImageDraw.Draw(bg)
try:
    from PIL import ImageFont
    f1 = ImageFont.truetype("segoeuib.ttf", 34)
    f2 = ImageFont.truetype("segoeui.ttf", 19)
except OSError:
    f1 = f2 = None
d.text((70, 64), "ResMon", fill=(235, 238, 255), font=f1)
d.text((70, 110), "Resource widget for Windows: docked tab, hover panel, full process treemap",
       fill=(170, 180, 215), font=f2)
d.text((label_x - 128, 778), "docked tab", fill=(170, 180, 215), font=f2)

out.parent.mkdir(parents=True, exist_ok=True)
bg.convert("RGB").save(out, optimize=True)
print(out, bg.size)
