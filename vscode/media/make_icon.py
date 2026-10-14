"""Draw Norn's icon: three threads drawn into one.

    python vscode/media/make_icon.py

Writes icon.png (256 px, the extension's icon), icon.svg (the same,
as a drawing) and norn.svg (the mark alone in the current colour, for
the activity bar) beside this file. The mark is on a 32-unit grid: three
knots on the left, their threads meeting at one point, and one thread
going on from there to a knot of another colour.
"""
from pathlib import Path

from PIL import Image, ImageDraw

HERE = Path(__file__).resolve().parent

# The mark, on its 32-unit grid.
KNOTS = [(7.0, 7.5), (7.0, 16.0), (7.0, 24.5)]
MEET = (19.5, 16.0)
END = (25.5, 16.0)
THREADS = [
    # Cubic curves: start, two controls, end.
    (KNOTS[0], (14.5, 7.5), (13.0, 16.0), MEET),
    (KNOTS[1], (11.0, 16.0), (15.0, 16.0), MEET),
    (KNOTS[2], (14.5, 24.5), (13.0, 16.0), MEET),
]
WIDTH, KNOT, END_KNOT = 2.3, 2.3, 3.1

NIGHT_TOP, NIGHT_BOTTOM = (34, 41, 84), (13, 17, 38)
THREAD = (233, 237, 250)
GOLD = (246, 190, 82)


def bezier(p0, p1, p2, p3, n=400):
    for i in range(n + 1):
        t = i / n
        u = 1 - t
        yield tuple(u**3 * a + 3 * u * u * t * b + 3 * u * t * t * c + t**3 * d for a, b, c, d in zip(p0, p1, p2, p3))


def png(size=256, scale=4):
    s = size * scale
    k = s / 32
    # Ground: night, lighter at the top, with rounded corners.
    ground = Image.new("RGB", (1, s))
    for y in range(s):
        t = y / (s - 1)
        ground.putpixel((0, y), tuple(round(a + (b - a) * t) for a, b in zip(NIGHT_TOP, NIGHT_BOTTOM)))
    ground = ground.resize((s, s))
    mask = Image.new("L", (s, s), 0)
    ImageDraw.Draw(mask).rounded_rectangle((0, 0, s - 1, s - 1), radius=round(s * 0.21), fill=255)
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    img.paste(ground, (0, 0), mask)
    d = ImageDraw.Draw(img)

    def dot(p, r, colour):
        d.ellipse(((p[0] - r) * k, (p[1] - r) * k, (p[0] + r) * k, (p[1] + r) * k), fill=colour)

    for thread in THREADS:
        for p in bezier(*thread):
            dot(p, WIDTH / 2, THREAD)
    for i in range(401):
        t = i / 400
        dot((MEET[0] + (END[0] - MEET[0]) * t, MEET[1]), WIDTH / 2, GOLD)
    for knot in KNOTS:
        dot(knot, KNOT, THREAD)
    dot(END, END_KNOT, GOLD)
    return img.resize((size, size), Image.LANCZOS)


def path(thread):
    (a, b, c, e) = thread
    return f"M{a[0]} {a[1]}C{b[0]} {b[1]} {c[0]} {c[1]} {e[0]} {e[1]}"


def svg(icon):
    hexa = lambda c: "#%02x%02x%02x" % c
    thread, gold = (hexa(THREAD), hexa(GOLD)) if icon else ("currentColor", "currentColor")
    threads = "".join(f'<path d="{path(t)}"/>' for t in THREADS)
    knots = "".join(f'<circle cx="{x}" cy="{y}" r="{KNOT}"/>' for x, y in KNOTS)
    ground = (
        f'<defs><linearGradient id="night" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="{hexa(NIGHT_TOP)}"/>'
        f'<stop offset="1" stop-color="{hexa(NIGHT_BOTTOM)}"/></linearGradient></defs><rect width="32" height="32" rx="6.7" fill="url(#night)"/>'
        if icon
        else ""
    )
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{256 if icon else 24}" height="{256 if icon else 24}" viewBox="0 0 32 32">{ground}'
        f'<g fill="none" stroke="{thread}" stroke-width="{WIDTH}" stroke-linecap="round">{threads}</g>'
        f'<path d="M{MEET[0]} {MEET[1]}H{END[0]}" fill="none" stroke="{gold}" stroke-width="{WIDTH}" stroke-linecap="round"/>'
        f'<g fill="{thread}">{knots}</g><circle cx="{END[0]}" cy="{END[1]}" r="{END_KNOT}" fill="{gold}"/></svg>\n'
    )


if __name__ == "__main__":
    png().save(HERE / "icon.png")
    (HERE / "icon.svg").write_text(svg(True), encoding="utf-8")
    (HERE / "norn.svg").write_text(svg(False), encoding="utf-8")
    print("icon.png, icon.svg, norn.svg")
