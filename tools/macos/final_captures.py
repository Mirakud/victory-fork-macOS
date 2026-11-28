"""Export the completed game plane independently of the renderer and its HUD."""
from bisect import bisect_left
import json
from pathlib import Path
import re
import sys
from threading import Event, Thread
import time

FRAME = re.compile(r"frame-(\d+)\.png")
HISTORY = "hud-mesures.jsonl"


class FinalCaptures:
    def __init__(self, run):
        self.run = Path(run)
        self.stop = Event()
        self.worker = Thread(target=self.collect, name="victory-final-captures", daemon=True)
        self.worker.start()

    def close(self):
        self.stop.set()
        self.worker.join()

    def collect(self):
        try:
            from watermark import export_final
        except ImportError as error:
            print(f"Export des captures finales indisponible ({error}). Installer Pillow avec le Python du lanceur.", file=sys.stderr)
            return
        raw = self.run / "captures"
        output = raw / "finales"
        samples, timestamps = [], []
        history_pos = 0
        errors = set()
        while True:
            finished = self.stop.is_set()
            try:
                with (self.run / HISTORY).open("rb") as history:
                    history.seek(history_pos)
                    for row in history:
                        if not row.endswith(b"\n"):
                            break
                        history_pos += len(row)
                        try:
                            sample = json.loads(row)
                        except (UnicodeError, ValueError):
                            continue
                        if not isinstance(sample, dict) or not sample.get("game_pid"):
                            continue
                        stamp = sample.get("updated")
                        if type(stamp) not in (int, float) or not 0 <= stamp < 253402300799:
                            continue
                        if not timestamps or stamp >= timestamps[-1]:
                            timestamps.append(stamp)
                            samples.append(sample)
            except OSError:
                pass
            if raw.is_dir():
                output.mkdir(exist_ok=True)
                for src in raw.iterdir():
                    match = FRAME.fullmatch(src.name)
                    if not match:
                        continue
                    try:
                        stamp = src.stat().st_mtime
                        dst = output / src.name
                        if dst.is_file() and dst.stat().st_mtime >= stamp:
                            continue
                        # The HUD records once per second. Include the sample
                        # on the other side of the PNG timestamp if available.
                        if not finished and time.time() < stamp + 1.1:
                            continue
                        i = bisect_left(timestamps, stamp)
                        nearby = range(max(0, i - 1), min(len(timestamps), i + 1))
                        best = min(nearby, key=lambda j: abs(timestamps[j] - stamp), default=None)
                        sample = samples[best] if best is not None and abs(timestamps[best] - stamp) <= 3 else None
                        export_final(src, dst, sample, int(match[1]))
                    except (OSError, ValueError) as error:
                        # A PNG can be visible before the final bytes land.
                        # Retry on the next pass and report a persistent error.
                        if finished or src in errors:
                            print(f"Export de {src.name} en attente ({error})", file=sys.stderr)
                        errors.add(src)
            if finished:
                return
            self.stop.wait(1)
