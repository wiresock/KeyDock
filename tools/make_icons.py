#!/usr/bin/env python3
"""Generates the KeyDock icons (assets/*.ico) with Pillow.

    python3 tools/make_icons.py

Each icon is drawn separately at every size (4x supersampled) so that the
small tray sizes stay crisp.
"""
from pathlib import Path
from PIL import Image, ImageDraw

SIZES = [16, 20, 24, 32, 40, 48, 64, 256]
SS = 4
OUT = Path(__file__).resolve().parent.parent / "assets"

SLATE = (43, 58, 85, 255)
KEY = (236, 241, 248, 255)
BLUE = (0, 120, 212, 255)
GREY = (128, 136, 148, 255)
ORANGE = (240, 140, 0, 255)
WHITE = (255, 255, 255, 255)


def keyboard(d, x0, y0, x1, y1, body, s):
    """Rounded keyboard body with a key grid; detail adapts to the size."""
    r = (y1 - y0) * 0.18
    d.rounded_rectangle([x0, y0, x1, y1], radius=r, fill=body)
    w, h = x1 - x0, y1 - y0
    rows = 2 if s < 24 else 3
    cols = 5 if s < 24 else 8
    pad = w * 0.08
    gap = max(w * 0.025, SS * 0.75)
    kw = (w - 2 * pad - (cols - 1) * gap) / cols
    kh = (h * 0.62 - (rows - 1) * gap) / rows
    top = y0 + h * 0.12
    for r_ in range(rows):
        for c in range(cols):
            kx = x0 + pad + c * (kw + gap)
            ky = top + r_ * (kh + gap)
            d.rectangle([kx, ky, kx + kw, ky + kh], fill=KEY)
    # space bar
    sy = top + rows * (kh + gap)
    sy1 = min(sy + kh, y1 - h * 0.08)
    if sy1 > sy:
        d.rectangle([x0 + w * 0.28, sy, x1 - w * 0.28, sy1], fill=KEY)


def draw(kind, s):
    S = s * SS
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    if kind in ("app", "docked", "paused"):
        body = GREY if kind == "paused" else SLATE
        accent = GREY if kind == "paused" else BLUE
        keyboard(d, S * 0.04, S * 0.16, S * 0.96, S * 0.62, body, s)
        # cable from keyboard to dock
        d.rectangle([S * 0.46, S * 0.62, S * 0.54, S * 0.74], fill=accent)
        # dock base
        d.rounded_rectangle([S * 0.14, S * 0.74, S * 0.86, S * 0.92], radius=S * 0.06, fill=accent)
        d.rectangle([S * 0.30, S * 0.80, S * 0.70, S * 0.85], fill=WHITE if s >= 24 else accent)
        if kind == "paused":
            cx, cy, rr = S * 0.74, S * 0.70, S * 0.27
            d.ellipse([cx - rr, cy - rr, cx + rr, cy + rr], fill=ORANGE)
            bw, bh = rr * 0.28, rr * 1.0
            d.rectangle([cx - bw * 1.6, cy - bh / 2, cx - bw * 0.6, cy + bh / 2], fill=WHITE)
            d.rectangle([cx + bw * 0.6, cy - bh / 2, cx + bw * 1.6, cy + bh / 2], fill=WHITE)
    elif kind == "laptop":
        # screen
        lw = max(S * 0.07, SS * 1.5)
        d.rounded_rectangle([S * 0.14, S * 0.10, S * 0.86, S * 0.62], radius=S * 0.05, outline=BLUE, width=int(lw))
        d.rectangle([S * 0.14 + lw, S * 0.10 + lw, S * 0.86 - lw, S * 0.62 - lw], fill=(0, 120, 212, 60))
        # base with keys
        d.rounded_rectangle([S * 0.02, S * 0.66, S * 0.98, S * 0.90], radius=S * 0.05, fill=BLUE)
        d.rectangle([S * 0.36, S * 0.74, S * 0.64, S * 0.82], fill=KEY)
    return img.resize((s, s), Image.LANCZOS)


def main():
    OUT.mkdir(exist_ok=True)
    for kind in ("app", "docked", "laptop", "paused"):
        imgs = [draw(kind, s) for s in SIZES]
        name = "keydock.ico" if kind == "app" else f"{kind}.ico"
        big = imgs[-1]
        big.save(OUT / name, format="ICO", sizes=[(s, s) for s in SIZES], append_images=imgs[:-1])
    print("wrote", OUT)


if __name__ == "__main__":
    main()
