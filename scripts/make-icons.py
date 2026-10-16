#!/usr/bin/env python3
"""Generate Sonder's app icons (PWA + Tauri) from one procedural design.

    python3 scripts/make-icons.py

Writes
  crates/sonder-web/static/icons/   icon.svg, icon-192.png, icon-512.png, maskable-512.png,
                                    apple-touch-icon.png (180)
  crates/sonder-desktop/icons/      32x32.png, 128x128.png, 128x128@2x.png, icon.png (512),
                                    icon.ico, icon.icns   (the set `tauri.conf.json` lists)

The design: the founding image — a bright headlight on a dark overpass, sound arcs
radiating from it. Drawn twice: once as SVG (hand-written below) and once with Pillow at
4x and downsampled, so the PNGs need no SVG rasterizer. Only Pillow is required.
"""
from __future__ import annotations

import math
import os
import sys

try:
    from PIL import Image, ImageDraw
except ImportError:  # pragma: no cover
    sys.exit("Pillow is required: pip install pillow")

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
WEB_ICONS = os.path.join(ROOT, "crates", "sonder-web", "static", "icons")
DESKTOP_ICONS = os.path.join(ROOT, "crates", "sonder-desktop", "icons")

BG = (13, 15, 18)          # --bg-1
BG_DEEP = (7, 8, 10)       # --bg
ACCENT = (232, 200, 138)   # --accent
LINE = (61, 67, 76)        # --fg-faint

# Geometry in a 0..1 unit square; the headlight sits low-left, arcs open up-right.
LIGHT = (0.36, 0.64)
ARCS = (0.20, 0.34, 0.48)
HORIZON = 0.70

SVG = f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512">
  <defs>
    <radialGradient id="glow" cx="{LIGHT[0]}" cy="{LIGHT[1]}" r="0.22">
      <stop offset="0" stop-color="rgb{ACCENT}" stop-opacity="0.9"/>
      <stop offset="1" stop-color="rgb{ACCENT}" stop-opacity="0"/>
    </radialGradient>
  </defs>
  <rect width="512" height="512" rx="112" fill="rgb{BG}"/>
  <rect y="{HORIZON * 512:.0f}" width="512" height="{512 - HORIZON * 512:.0f}" fill="rgb{BG_DEEP}"/>
  <line x1="0" y1="{HORIZON * 512:.0f}" x2="512" y2="{HORIZON * 512:.0f}" stroke="rgb{LINE}" stroke-width="3"/>
  <rect width="512" height="512" fill="url(#glow)"/>
  <g fill="none" stroke="rgb{ACCENT}" stroke-linecap="round" stroke-width="14">
    <path d="M {LIGHT[0]*512 + ARCS[0]*512*math.cos(math.radians(-100)):.1f} {LIGHT[1]*512 + ARCS[0]*512*math.sin(math.radians(-100)):.1f} A {ARCS[0]*512:.1f} {ARCS[0]*512:.1f} 0 0 1 {LIGHT[0]*512 + ARCS[0]*512*math.cos(math.radians(10)):.1f} {LIGHT[1]*512 + ARCS[0]*512*math.sin(math.radians(10)):.1f}" opacity="0.95"/>
    <path d="M {LIGHT[0]*512 + ARCS[1]*512*math.cos(math.radians(-100)):.1f} {LIGHT[1]*512 + ARCS[1]*512*math.sin(math.radians(-100)):.1f} A {ARCS[1]*512:.1f} {ARCS[1]*512:.1f} 0 0 1 {LIGHT[0]*512 + ARCS[1]*512*math.cos(math.radians(10)):.1f} {LIGHT[1]*512 + ARCS[1]*512*math.sin(math.radians(10)):.1f}" opacity="0.6"/>
    <path d="M {LIGHT[0]*512 + ARCS[2]*512*math.cos(math.radians(-100)):.1f} {LIGHT[1]*512 + ARCS[2]*512*math.sin(math.radians(-100)):.1f} A {ARCS[2]*512:.1f} {ARCS[2]*512:.1f} 0 0 1 {LIGHT[0]*512 + ARCS[2]*512*math.cos(math.radians(10)):.1f} {LIGHT[1]*512 + ARCS[2]*512*math.sin(math.radians(10)):.1f}" opacity="0.3"/>
  </g>
  <circle cx="{LIGHT[0]*512:.0f}" cy="{LIGHT[1]*512:.0f}" r="22" fill="rgb{ACCENT}"/>
  <circle cx="{LIGHT[0]*512:.0f}" cy="{LIGHT[1]*512:.0f}" r="10" fill="#fff6e2"/>
</svg>
"""


def render(size: int, pad: float = 0.0, rounded: bool = True) -> Image.Image:
    """Draw the icon at `size` px. `pad` is the fraction of the canvas left as margin on
    each side (maskable icons keep the art inside the 80% safe zone)."""
    s = size * 4  # supersample
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    d = ImageDraw.Draw(img, "RGBA")
    m = int(s * pad)
    box = (m, m, s - m - 1, s - m - 1)
    art = s - 2 * m
    r = int(art * 112 / 512) if rounded else 0
    d.rounded_rectangle(box, radius=r, fill=BG + (255,))

    # Ground below the horizon, clipped to the rounded square via a mask.
    ground = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    gd = ImageDraw.Draw(ground)
    gd.rectangle((0, m + int(art * HORIZON), s, s), fill=BG_DEEP + (255,))
    mask = Image.new("L", (s, s), 0)
    ImageDraw.Draw(mask).rounded_rectangle(box, radius=r, fill=255)
    img.paste(ground, (0, 0), Image.composite(mask, Image.new("L", (s, s), 0), ground.split()[3]))
    y = m + int(art * HORIZON)
    d.line((m, y, s - m, y), fill=LINE + (255,), width=max(1, int(art * 3 / 512)))

    cx, cy = m + LIGHT[0] * art, m + LIGHT[1] * art
    # Soft glow: concentric translucent discs.
    glow = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    gd = ImageDraw.Draw(glow)
    gr = art * 0.22
    for i in range(24, 0, -1):
        rr = gr * i / 24
        a = int(110 * (1 - i / 24) ** 1.6)
        gd.ellipse((cx - rr, cy - rr, cx + rr, cy + rr), fill=ACCENT + (a,))
    img.alpha_composite(Image.composite(glow, Image.new("RGBA", (s, s), (0, 0, 0, 0)), mask))

    w = max(2, int(art * 14 / 512))
    for radius, alpha in zip(ARCS, (242, 153, 77)):
        rr = radius * art
        d.arc((cx - rr, cy - rr, cx + rr, cy + rr), start=-100, end=10, fill=ACCENT + (alpha,), width=w)
    rr = art * 22 / 512
    d.ellipse((cx - rr, cy - rr, cx + rr, cy + rr), fill=ACCENT + (255,))
    rr = art * 10 / 512
    d.ellipse((cx - rr, cy - rr, cx + rr, cy + rr), fill=(255, 246, 226, 255))
    return img.resize((size, size), Image.LANCZOS)


def main() -> None:
    os.makedirs(WEB_ICONS, exist_ok=True)
    os.makedirs(DESKTOP_ICONS, exist_ok=True)
    with open(os.path.join(WEB_ICONS, "icon.svg"), "w") as f:
        f.write(SVG)
    render(192).save(os.path.join(WEB_ICONS, "icon-192.png"))
    render(512).save(os.path.join(WEB_ICONS, "icon-512.png"))
    render(180).save(os.path.join(WEB_ICONS, "apple-touch-icon.png"))
    # Maskable: the OS crops to a circle/squircle, so draw a full-bleed background and keep
    # the art inside the safe zone.
    mk = Image.new("RGBA", (512, 512), BG + (255,))
    mk.alpha_composite(render(512, pad=0.1, rounded=False))
    mk.save(os.path.join(WEB_ICONS, "maskable-512.png"))

    for size, name in ((32, "32x32.png"), (128, "128x128.png"), (256, "128x128@2x.png"), (512, "icon.png")):
        render(size).save(os.path.join(DESKTOP_ICONS, name))
    base = render(256)
    base.save(os.path.join(DESKTOP_ICONS, "icon.ico"), sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
    render(512).save(os.path.join(DESKTOP_ICONS, "icon.icns"))
    print(f"icons written to {WEB_ICONS} and {DESKTOP_ICONS}")


if __name__ == "__main__":
    main()
