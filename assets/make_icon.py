"""Draws the app icon: a blue-violet tile holding three translucent "fences".

Run: python -I assets/make_icon.py   (writes assets/app.ico and assets/icon-preview.png)
"""
from pathlib import Path
from PIL import Image, ImageDraw

OUT = Path(__file__).resolve().parent
S = 1024  # master size; every icon size is downscaled from it


def lerp(a, b, t):
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


def master() -> Image.Image:
    # Diagonal gradient, top-left blue to bottom-right violet.
    top, bottom = (59, 130, 246), (139, 92, 246)
    grad = Image.new("RGBA", (S, S))
    px = grad.load()
    for y in range(S):
        for x in range(S):
            px[x, y] = lerp(top, bottom, (x + y) / (2 * S - 2)) + (255,)

    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).rounded_rectangle((32, 32, S - 32, S - 32), radius=220, fill=255)
    tile = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    tile.paste(grad, (0, 0), mask)

    # Soft highlight on the upper half for depth.
    gloss = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    ImageDraw.Draw(gloss).rounded_rectangle((32, 32, S - 32, S // 2), radius=220, fill=(255, 255, 255, 28))
    tile = Image.alpha_composite(tile, Image.composite(gloss, Image.new("RGBA", (S, S)), mask))

    # Three fences: a tall one on the left, two stacked on the right.
    panels = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(panels)
    m, gap, r = 200, 56, 64
    left = (m, m, S // 2 - gap // 2, S - m)
    right_top = (S // 2 + gap // 2, m, S - m, S // 2 - gap // 2)
    right_bottom = (S // 2 + gap // 2, S // 2 + gap // 2, S - m, S - m)
    for box in (left, right_top, right_bottom):
        d.rounded_rectangle(box, radius=r, fill=(255, 255, 255, 150))
        x0, y0, x1, _ = box
        # Brighter title bar strip.
        d.rounded_rectangle((x0, y0, x1, y0 + 92), radius=r, fill=(255, 255, 255, 235))
        d.rectangle((x0, y0 + 60, x1, y0 + 92), fill=(255, 255, 255, 235))
    return Image.alpha_composite(tile, panels)


def main():
    img = master()
    sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
    frames = [img.resize((n, n), Image.LANCZOS) for n in sizes]
    frames[-1].save(OUT / "app.ico", sizes=[(n, n) for n in sizes], append_images=frames[:-1])
    # Preview strip of a few sizes on a dark and a light background.
    strip = Image.new("RGBA", (16 + 32 + 48 + 64 + 128 + 6 * 16, 2 * 160), (32, 32, 36, 255))
    ImageDraw.Draw(strip).rectangle((0, 160, strip.width, 320), fill=(235, 235, 240, 255))
    x = 16
    for n in (16, 32, 48, 64, 128):
        f = img.resize((n, n), Image.LANCZOS)
        for y0 in (0, 160):
            strip.alpha_composite(f, (x, y0 + (160 - n) // 2))
        x += n + 16
    strip.save(OUT / "icon-preview.png")
    print("wrote", OUT / "app.ico")


if __name__ == "__main__":
    main()
