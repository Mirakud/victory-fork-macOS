"""Create a PNG display copy with a small mirakud signature. Requires Pillow 10.1+."""
import argparse
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageFont


def watermark(source, output):
    if source.resolve() == output.resolve():
        raise ValueError("Choose a separate output file to preserve the source.")
    with Image.open(source) as image:
        baseline = image.convert("RGB")
    # No resize or JPEG recompression. Only the glyphs and their outline change pixels.
    display = baseline.copy()
    display.info.clear()
    width, height = display.size
    size = max(12, min(21, height // 34))
    margin = max(5, height // 80)
    font = ImageFont.load_default(size=size)
    draw = ImageDraw.Draw(display)
    text = "mirakud"
    position = (width - margin, height - margin)
    options = dict(font=font, anchor="rb", stroke_width=1)
    bounds = draw.textbbox(position, text, **options)
    draw.text(position, text, fill=(244, 244, 244), stroke_fill=(32, 32, 32), **options)
    display.save(output, format="PNG", compress_level=7)
    with Image.open(output) as saved:
        changed = ImageChops.difference(baseline, saved.convert("RGB")).getbbox()
    if changed is None or any((changed[0] < bounds[0], changed[1] < bounds[1],
                               changed[2] > bounds[2], changed[3] > bounds[3])):
        raise ValueError("The output does not preserve pixels outside the signature.")
    return changed


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    print(watermark(args.source, args.output))
