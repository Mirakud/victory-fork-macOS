"""Export a final PNG with the Victory HUD above the game and the mirakud signature (Pillow 10+)."""
import argparse
from datetime import datetime, timezone
import json
import math
from pathlib import Path
import re
from tempfile import TemporaryDirectory

from PIL import Image, ImageChops, ImageDraw, ImageFont


METRICS = (
    ("FPS", "fps"), ("CPU Victory", "cpu_percent"), ("RAM Victory", "ram_mib"),
    ("Temps image", "frame_ms"), ("Attente GPU", "gpu_ms"), ("Copies / image", "copy_ms"),
)


def metric_text(key, value):
    """Use the same French units and precision as Readings in victory_perf_hud.swift."""
    if type(value) not in (int, float) or not math.isfinite(value) or value < 0:
        return "—"
    if key == "fps":
        if value == 0:
            return "0"
        if value < 0.001:
            return "< 0,001"
        decimals, unit = (3 if value < 0.1 else 2 if value < 1 else 1), ""
    elif key == "cpu_percent":
        decimals, unit = int(value < 100), " %"
    elif key == "ram_mib":
        value, decimals, unit = value / 1024, 2, " Gio"
    else:
        decimals, unit = int(value < 100), " ms"
    return f"{value:.{decimals}f}{unit}".replace(".", ",")


def _font(size):
    try:
        return ImageFont.load_default(size=size)
    except TypeError:
        # Pillow 10.0 bundled a bitmap font with no size argument or em dash.
        for face in ("Helvetica.ttc", "DejaVuSans.ttf", "LiberationSans-Regular.ttf", "arial.ttf"):
            try:
                return ImageFont.truetype(face, size)
            except OSError:
                continue
        raise OSError("A scalable sans-serif font or Pillow 10.1+ is needed for the capture HUD.")


def _header(width, sample, frame):
    # Match the charcoal, white and mint of the live AppKit overlay. Use the
    # font bundled with Pillow so the same HUD can be rendered off the Mac.
    scale = min(3, max(0.85, width / 960))
    def px(n):
        return round(n * scale)

    cols = max(1, min(len(METRICS), width // px(135)))
    rows = math.ceil(len(METRICS) / cols)
    band = Image.new("RGB", (width, px(34 + 49 * rows)), (14, 17, 20))
    draw = ImageDraw.Draw(band)
    title = _font(px(17))
    label = _font(px(13))
    reading = _font(px(22))
    mint, muted, white = (176, 219, 201), (177, 185, 191), (249, 250, 251)
    draw.text((px(14), px(6)), "mirakud", font=title, fill=mint, stroke_width=1, stroke_fill=mint)
    stamp = ""
    if frame is not None:
        stamp = f"Image {frame}"
    elif type(updated := sample.get("updated")) in (int, float) and 0 <= updated < 253402300799:
        stamp = datetime.fromtimestamp(updated, timezone.utc).strftime("%H:%M:%S UTC")
    if stamp:
        draw.text((width - px(14), px(7)), stamp, anchor="ra", font=label, fill=muted)
    draw.line((px(14), px(31), width - px(14), px(31)), fill=(66, 73, 77))
    for i, (caption, key) in enumerate(METRICS):
        row, col = divmod(i, cols)
        x, y = width * col // cols + px(14), px(36 + 49 * row)
        draw.text((x, y), caption, font=label, fill=muted)
        draw.text((x, y + px(16)), metric_text(key, sample.get(key)), font=reading, fill=white)
    return band


def _export(source, output, final, sample=None, frame=None):
    source, output = Path(source), Path(output)
    if source.resolve() == output.resolve() or output.exists() and source.samefile(output):
        raise ValueError("Choose a separate output file to preserve the source.")
    if sample is not None and not isinstance(sample, dict):
        raise ValueError("The HUD sample must be a JSON object.")
    if frame is None and (match := re.fullmatch(r"frame-(\d+)", source.stem)):
        frame = int(match[1])
    with Image.open(source) as image:
        color_profile = image.info.get("icc_profile")
        baseline = image.convert("RGBA" if "A" in image.getbands() else "RGB")

    scene = baseline.copy()
    width, height = scene.size
    size = max(12, min(21, height // 34))
    margin = max(5, height // 80)
    draw = ImageDraw.Draw(scene)
    position = (width - margin, height - margin)
    options = dict(font=_font(size), anchor="rb", stroke_width=1)
    draw.text(position, "mirakud", fill=(244, 244, 244), stroke_fill=(32, 32, 32), **options)
    if final:
        band = _header(max(144, width), sample or {}, frame)
        display = Image.new(scene.mode, (band.width, band.height + height), (14, 17, 20))
        display.paste(band, (0, 0))
        display.paste(scene, (0, band.height))
    else:
        display = scene

    display.info.clear()
    with TemporaryDirectory(dir=output.parent) as tmp:
        staged = Path(tmp) / "image.png"
        display.save(staged, format="PNG", compress_level=7, icc_profile=color_profile)
        staged.replace(output)


def export_final(source, output, sample=None, frame=None):
    """Write the display PNG. Missing measurements are shown as dashes."""
    _export(source, output, True, sample, frame)


def watermark(source, output):
    """Keep the signature-only API for existing callers and return its changed bounds."""
    _export(source, output, False)
    with Image.open(source) as original, Image.open(output) as signed:
        return ImageChops.difference(original.convert("RGB"), signed.convert("RGB")).getbbox()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--hud", type=Path, help="HUD sample in the format of hud.json")
    parser.add_argument("--frame", type=int, help="presentation number, inferred from frame-N.png when possible")
    parser.add_argument("--signature-only", action="store_true", help="export the historical signature-only layout")
    args = parser.parse_args()
    if args.signature_only:
        watermark(args.source, args.output)
    else:
        sample = json.loads(args.hud.read_text()) if args.hud else None
        export_final(args.source, args.output, sample, args.frame)
