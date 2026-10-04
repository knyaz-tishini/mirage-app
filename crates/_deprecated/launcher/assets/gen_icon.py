"""
Generate the 'cube · 3d wireframe' mirage logo as PNGs and a Windows .ico.

Reference SVG (from Mirage Design System logos.html, card 21):
    viewBox = "0 0 40 40"
    outer  : M20 4 L34 12 V28 L20 36 L6 28 V12 Z   stroke=#FBB26A 1.5
    inner  : M6 12 L20 20 L34 12 M20 20 V36         stroke=#FBB26A 1.5 opacity 0.7

Output:
  icon-16/32/48/64/128/256.png  +  icon.ico
"""

from PIL import Image, ImageDraw
import os

ACCENT          = (251, 178, 106, 255)
ACCENT_70       = (251, 178, 106, 178)
SIZES           = [16, 32, 48, 64, 128, 256]
SVG_VIEWBOX     = 40.0  # the reference uses 0..40 in both axes

# 6 hexagon outline points (the cube silhouette).
HEX_PTS = [
    (20, 4), (34, 12), (34, 28), (20, 36), (6, 28), (6, 12),
]
# Inner Y / converging lines:
#   from (6,12) → (20,20) → (34,12)   ← top of the back face
#   from (20,20) → (20,36)            ← vertical down the centre
INNER_LINES = [
    [(6, 12), (20, 20), (34, 12)],
    [(20, 20), (20, 36)],
]


def _render_at(big: int, stroke_w: float) -> Image.Image:
    """Direct rasterisation at exactly `big` px with given line width."""
    img = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img, "RGBA")
    scale = big / SVG_VIEWBOX

    def s(p):
        return (p[0] * scale, p[1] * scale)

    sw_outer = max(1, int(round(stroke_w * scale)))
    sw_inner = max(1, int(round(stroke_w * scale)))

    pts = [s(p) for p in HEX_PTS]
    draw.line(pts + [pts[0]], fill=ACCENT, width=sw_outer, joint="curve")
    for poly in INNER_LINES:
        draw.line([s(p) for p in poly], fill=ACCENT_70, width=sw_inner, joint="curve")
    return img


def render(size: int) -> Image.Image:
    """Antialiased render — draw at 8× then downsample with LANCZOS.

    Stroke width is scaled to icon size: at 16-32 px we need a much thicker
    relative line (≈2.5 SVG units) so the cube reads at all; at 256 px the
    original 1.5 unit stroke is correct and looks clean.
    """
    # Pick stroke width per target — small icons need fatter lines to read.
    # Bumped 1.5× over the previous tuning so the cube reads cleanly in
    # taskbar / Alt-Tab / systray at 16-32 px.
    if size <= 24:
        stroke_w = 4.2
    elif size <= 48:
        stroke_w = 3.3
    elif size <= 96:
        stroke_w = 2.7
    else:
        stroke_w = 2.25

    OVER = 8
    big = _render_at(size * OVER, stroke_w)
    return big.resize((size, size), Image.LANCZOS)


def main():
    out_dir = os.path.dirname(os.path.abspath(__file__))
    images = []
    for size in SIZES:
        img = render(size)
        path = os.path.join(out_dir, f"icon-{size}.png")
        img.save(path, format="PNG")
        print(f"wrote {path}")
        images.append(img)

    # Build ICO bundling ALL sizes — Pillow's `sizes` param only resamples
    # from the base image which loses fidelity at large sizes.  Writing each
    # size manually preserves the per-size rendering.
    ico_path = os.path.join(out_dir, "icon.ico")
    biggest = images[-1]
    biggest.save(
        ico_path,
        format="ICO",
        sizes=[(s, s) for s in SIZES],
        append_images=images[:-1],
    )
    print(f"wrote {ico_path}")


if __name__ == "__main__":
    main()
