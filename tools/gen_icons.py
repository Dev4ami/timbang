#!/usr/bin/env python
"""Generate Timbang's PWA raster icons from the same geometry as web/favicon.svg.

The logo is an app tile split diagonally: Pro (biru) upper-left, Kontra (merah)
lower-right, a white seam between (§1: two sides, no winner). SVG is the source of
truth for the browser tab; these PNGs exist only because iOS/Android need raster.

Rendered at 4x then downscaled so the diagonal seam and rounded corners come out
antialiased. Re-run after changing colours or shape:  python tools/gen_icons.py
"""
from PIL import Image, ImageDraw

PRO = (29, 78, 111)     # --pro  #1d4e6f
KONTRA = (122, 46, 46)  # --kontra #7a2e2e
SEAM = (255, 255, 255)

SS = 4  # supersample factor


def tile(size, *, rounded=True, bleed=0.0):
    """One icon. `bleed` (0..~0.12) pushes art past the edge for maskable safe zone."""
    S = size * SS
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)

    ext = int(S * bleed)
    lo, hi = -ext, S + ext  # art bounds, optionally past the edges

    # Pro fills the whole square; Kontra overpaints the lower-right triangle.
    d.rectangle([lo, lo, hi, hi], fill=PRO)
    d.polygon([(hi, lo), (hi, hi), (lo, hi)], fill=KONTRA)
    # White seam corner-to-corner (bottom-left -> top-right).
    seam_w = max(2, int(S * 0.07))
    d.line([(lo, hi), (hi, lo)], fill=SEAM, width=seam_w)

    if rounded:
        radius = int(S * 0.24)
        mask = Image.new("L", (S, S), 0)
        ImageDraw.Draw(mask).rounded_rectangle([0, 0, S - 1, S - 1], radius, fill=255)
        img.putalpha(mask)

    return img.resize((size, size), Image.LANCZOS)


OUT = "web"
JOBS = [
    ("icon-180.png", 180, dict(rounded=True)),   # apple-touch (iOS)
    ("icon-192.png", 192, dict(rounded=True)),   # PWA any
    ("icon-512.png", 512, dict(rounded=True)),   # PWA any (install/splash)
    ("icon-maskable-512.png", 512, dict(rounded=False, bleed=0.10)),  # Android
    ("favicon.png", 48, dict(rounded=True)),     # legacy .ico fallback
]

if __name__ == "__main__":
    for name, size, kw in JOBS:
        tile(size, **kw).save(f"{OUT}/{name}")
        print(f"wrote {OUT}/{name} ({size}px)")
