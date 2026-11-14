#!/usr/bin/env python3
"""Draws Satchel's icon: a pale satchel glyph (a bag seen from the front: a body, a flap over its
top with a clasp, and a handle) on a green rounded square. Pure Python (no PIL): every shape is a
signed distance, sampled once per pixel with analytic anti-aliasing, at each size it's written
at, so small sizes stay crisp.

    python3 scripts/icon.py                 # assets/icon/: satchel.svg, png/satchel-N.png, satchel.ico
    python3 scripts/icon.py --mac DIR       # DIR/icon_*.png for iconutil (scripts/package.sh)

The geometry is in a 1024-unit square. `full` fills the square (Windows, Linux); `mac` sits in
macOS's icon grid (an 824-unit tile with a 100-unit margin), as Finder and the Dock expect.
"""
import math
import os
import struct
import sys
import zlib

# Colours (sRGB): a deep green, lighter at the top, and a warm off-white glyph.
TOP = (0x3A, 0x9D, 0x6A)
BOTTOM = (0x1B, 0x5E, 0x3F)
GLYPH = (0xF6, 0xF2, 0xE6)

# The glyph, in the 1024 square of the full variant. Boxes are (x0, y0, x1, y1, top corner
# radius, bottom corner radius). The flap is a little wider than the body, and a groove of the
# tile's green runs along its lower edge across the body; the clasp is a green ring over the
# flap's edge, its middle the glyph's colour.
HANDLE = (332, 196, 692, 430, 104, 104)  # drawn as an outline HANDLE_W wide
HANDLE_W = 72
BODY = (222, 380, 802, 836, 80, 92)
FLAP = (192, 318, 832, 574, 92, 140)
GROOVE_W = 44
CLASP = (436, 518, 588, 644, 30, 30)  # drawn as a ring CLASP_W wide
CLASP_W = 40


def tile(variant):
    """The rounded square's (inset, corner radius) in the 1024 square."""
    return (100, 185) if variant == "mac" else (40, 210)


def glyph_scale(variant):
    """The glyph's scale about the centre: inside the mac tile it's smaller."""
    return 824 / 944 if variant == "mac" else 1.0


def sd_round_rect(x, y, inset, r):
    # Distance to a rounded square from inset to 1024 - inset.
    return sd_box(x, y, (inset, inset, 1024 - inset, 1024 - inset, r, r))


def sd_box(px, py, box):
    """Distance to a box (x0, y0, x1, y1, top radius, bottom radius); negative inside."""
    x0, y0, x1, y1, rt, rb = box
    r = rt if py < (y0 + y1) / 2 else rb
    qx = abs(px - (x0 + x1) / 2) - ((x1 - x0) / 2 - r)
    qy = abs(py - (y0 + y1) / 2) - ((y1 - y0) / 2 - r)
    return math.hypot(max(qx, 0.0), max(qy, 0.0)) + min(max(qx, qy), 0.0) - r


def shapes(variant, boost, solid_clasp=False):
    """The glyph's boxes and line widths in 1024 units, scaled for the variant; `boost` thickens
    the handle, the groove and the clasp at small sizes so they don't vanish, and `solid_clasp`
    fills the clasp's ring in (at 16 px its middle would be a blur)."""
    k = glyph_scale(variant)

    def box(b):
        x0, y0, x1, y1, rt, rb = b
        return (512 + (x0 - 512) * k, 512 + (y0 - 512) * k, 512 + (x1 - 512) * k, 512 + (y1 - 512) * k, rt * k, rb * k)

    return {
        "handle": box(HANDLE),
        "body": box(BODY),
        "flap": box(FLAP),
        "clasp": box(CLASP),
        "handle_w": HANDLE_W * k * boost,
        "groove_w": GROOVE_W * k * boost,
        "clasp_w": CLASP_W * k * boost,
        "solid_clasp": solid_clasp,
    }


def sd_glyph(x, y, g):
    """Distance to the whole glyph: handle, flap and body together, less the groove below the
    flap (on the body only, and not under the clasp) and the clasp's ring."""
    flap = sd_box(x, y, g["flap"])
    body = sd_box(x, y, g["body"])
    clasp = sd_box(x, y, g["clasp"])
    handle = abs(sd_box(x, y, g["handle"])) - g["handle_w"] / 2
    d = min(handle, flap, body)
    groove = max(-flap, flap - g["groove_w"], body, -(clasp - g["clasp_w"] / 2))
    ring = (clasp if g["solid_clasp"] else abs(clasp)) - g["clasp_w"] / 2
    return max(d, -groove, -ring)


def render(size, variant):
    """RGBA rows (bytes) of the icon at `size` pixels."""
    inset, radius = tile(variant)
    unit = 1024 / size  # 1024 units per image, so a pixel is this many units
    boost = 1.0 if size >= 64 else 1.0 + (64 - size) / 64 * 0.45
    g = shapes(variant, boost, solid_clasp=size < 24)
    rows = []
    for j in range(size):
        y = (j + 0.5) * unit
        shade = (y - inset) / (1024 - 2 * inset)
        shade = min(max(shade, 0.0), 1.0)
        bg = tuple(TOP[i] + (BOTTOM[i] - TOP[i]) * shade for i in range(3))
        row = bytearray()
        for i in range(size):
            x = (i + 0.5) * unit
            d_tile = sd_round_rect(x, y, inset, radius)
            a_tile = min(max(0.5 - d_tile / unit, 0.0), 1.0)
            if a_tile == 0.0:
                row += b"\0\0\0\0"
                continue
            a = min(max(0.5 - sd_glyph(x, y, g) / unit, 0.0), 1.0)
            rgb = [round(bg[c] + (GLYPH[c] - bg[c]) * a) for c in range(3)]
            row += bytes(rgb + [round(255 * a_tile)])
        rows.append(bytes(row))
    return rows


def png(size, rows):
    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    raw = b"".join(b"\0" + r for r in rows)
    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b"")


def ico(images):
    """An .ico of PNG images (Windows Vista and later read PNG at every size)."""
    out = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    entries, data = b"", b""
    for size, blob in images:
        dim = 0 if size >= 256 else size
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(blob), offset + len(data))
        data += blob
    return out + entries + data


def svg_box(b, **attrs):
    """A box as an SVG path: a rounded rectangle with its own top and bottom corner radii."""
    x0, y0, x1, y1, rt, rb = b
    d = (
        f"M{x0 + rt:g},{y0:g} H{x1 - rt:g} A{rt:g},{rt:g} 0 0 1 {x1:g},{y0 + rt:g} "
        f"V{y1 - rb:g} A{rb:g},{rb:g} 0 0 1 {x1 - rb:g},{y1:g} H{x0 + rb:g} "
        f"A{rb:g},{rb:g} 0 0 1 {x0:g},{y1 - rb:g} V{y0 + rt:g} A{rt:g},{rt:g} 0 0 1 {x0 + rt:g},{y0:g} Z"
    )
    extra = "".join(f' {k.replace("_", "-")}="{v}"' for k, v in attrs.items())
    return f'<path d="{d}"{extra}/>'


def svg(variant="full"):
    inset, radius = tile(variant)
    g = shapes(variant, 1.0)
    side = 1024 - 2 * inset
    glyph = f"#{GLYPH[0]:02x}{GLYPH[1]:02x}{GLYPH[2]:02x}"
    # The groove and the clasp's ring are painted in the tile's gradient (in user space, so it
    # lines up with the tile's): the flap's outline, twice the groove wide and clipped to the
    # body, is covered on its inner half by the flap itself.
    lines = [
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024" width="1024" height="1024">',
        "  <!-- Generated by scripts/icon.py; edit the geometry there. -->",
        f'  <defs><linearGradient id="bg" gradientUnits="userSpaceOnUse" x1="0" y1="{inset}" x2="0" y2="{1024 - inset}">'
        f'<stop offset="0" stop-color="#{TOP[0]:02x}{TOP[1]:02x}{TOP[2]:02x}"/>'
        f'<stop offset="1" stop-color="#{BOTTOM[0]:02x}{BOTTOM[1]:02x}{BOTTOM[2]:02x}"/></linearGradient>',
        f'  <clipPath id="body">{svg_box(g["body"])}</clipPath></defs>',
        f'  <rect x="{inset}" y="{inset}" width="{side}" height="{side}" rx="{radius}" fill="url(#bg)"/>',
        "  " + svg_box(g["handle"], fill="none", stroke=glyph, stroke_width=f'{g["handle_w"]:g}'),
        "  " + svg_box(g["body"], fill=glyph),
        "  " + svg_box(g["flap"], fill="none", stroke="url(#bg)", stroke_width=f'{2 * g["groove_w"]:g}', clip_path="url(#body)"),
        "  " + svg_box(g["flap"], fill=glyph),
        "  " + svg_box(g["clasp"], fill=glyph, stroke="url(#bg)", stroke_width=f'{g["clasp_w"]:g}'),
        "</svg>",
        "",
    ]
    return "\n".join(lines)


def write(path, data):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)


def main():
    root = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "assets", "icon")
    if len(sys.argv) == 3 and sys.argv[1] == "--mac":
        # iconutil's names: icon_NxN.png and icon_NxN@2x.png.
        out = sys.argv[2]
        for base in (16, 32, 128, 256, 512):
            for scale, suffix in ((1, ""), (2, "@2x")):
                n = base * scale
                write(os.path.join(out, f"icon_{base}x{base}{suffix}.png"), png(n, render(n, "mac")))
        return
    if len(sys.argv) != 1:
        sys.exit(__doc__)
    write(os.path.join(root, "satchel.svg"), svg().encode())
    pngs = {}
    for n in (16, 24, 32, 48, 64, 128, 256, 512):
        pngs[n] = png(n, render(n, "full"))
        write(os.path.join(root, "png", f"satchel-{n}.png"), pngs[n])
    write(os.path.join(root, "satchel.ico"), ico([(n, pngs[n]) for n in (16, 24, 32, 48, 64, 128, 256)]))
    write(os.path.join(root, "satchel-mac-1024.png"), png(1024, render(1024, "mac")))


if __name__ == "__main__":
    main()
