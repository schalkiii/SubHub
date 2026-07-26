"""
Generate the SubHub app icon (a hexagon "hub" mark representing many
nodes aggregated into one). Output: app/icons/icon.ico (multi-size, with
alpha) + app/icons/icon.png (512 reference).

Run:  python docs/make_icon.py
"""
import math
import os
from PIL import Image, ImageDraw

HERE = os.path.dirname(os.path.abspath(__file__))
ICON_DIR = os.path.join(HERE, "..", "app", "icons")
S = 2048  # master render resolution (supersampled, then downscaled)
CX = CY = S // 2


def vgrad(top, bottom, w, h):
    img = Image.new("RGB", (w, h))
    d = ImageDraw.Draw(img)
    r1, g1, b1 = top
    r2, g2, b2 = bottom
    for y in range(h):
        t = y / (h - 1)
        r = int(r1 + (r2 - r1) * t)
        g = int(g1 + (g2 - g1) * t)
        b = int(b1 + (b2 - b1) * t)
        d.line([(0, y), (w, y)], fill=(r, g, b))
    return img


def rounded_mask(size, radius, pad):
    m = Image.new("L", (size, size), 0)
    d = ImageDraw.Draw(m)
    d.rounded_rectangle(
        [pad, pad, size - pad - 1, size - pad - 1], radius=radius, fill=255
    )
    return m


def draw_master():
    base = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    grad = vgrad((43, 107, 255), (11, 47, 122), S, S).convert("RGBA")
    mask = rounded_mask(S, int(S * 0.20), int(S * 0.02))
    base.paste(grad, (0, 0), mask)

    d = ImageDraw.Draw(base)

    # pointy-top hexagon outline
    R = int(S * 0.36)
    pts = []
    for i in range(6):
        ang = math.radians(-90 + 60 * i)
        pts.append((CX + R * math.cos(ang), CY + R * math.sin(ang)))
    d.line(pts + [pts[0]], fill=(255, 255, 255, 235),
           width=int(S * 0.012), joint="curve")

    # hub + 3 satellite nodes (aggregation: many -> one)
    hub_r = int(S * 0.052)
    sat_r = int(S * 0.033)
    dist = int(S * 0.205)
    sats = [-150, -30, 90]
    for a in sats:
        ang = math.radians(a)
        sx = CX + dist * math.cos(ang)
        sy = CY + dist * math.sin(ang)
        d.line([(CX, CY), (sx, sy)], fill=(255, 255, 255, 205),
               width=int(S * 0.0085))
    for a in sats:
        ang = math.radians(a)
        sx = CX + dist * math.cos(ang)
        sy = CY + dist * math.sin(ang)
        d.ellipse([sx - sat_r, sy - sat_r, sx + sat_r, sy + sat_r],
                  fill=(255, 255, 255, 255))
    d.ellipse([CX - hub_r, CY - hub_r, CX + hub_r, CY + hub_r],
              fill=(255, 255, 255, 255))
    return base


def main():
    os.makedirs(ICON_DIR, exist_ok=True)
    master = draw_master()
    ico_sizes = [16, 24, 32, 48, 64, 128, 256, 512]
    master.save(
        os.path.join(ICON_DIR, "icon.ico"),
        format="ICO",
        sizes=[(s, s) for s in ico_sizes],
    )
    master.resize((512, 512), Image.LANCZOS).save(
        os.path.join(ICON_DIR, "icon.png")
    )
    # smallest preview for quick visual check
    master.resize((256, 256), Image.LANCZOS).save(
        os.path.join(ICON_DIR, "icon_preview.png")
    )
    print("icon written to", os.path.abspath(ICON_DIR))


if __name__ == "__main__":
    main()
