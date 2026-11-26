#!/usr/bin/env python3
"""Résumer les preuves déjà présentes dans un dossier d’essai de Victory."""

import argparse
from datetime import datetime, timedelta
import json
from pathlib import Path
import re
import subprocess


EVENTS = {
    "XUserAddAsync: the offline local user": "Demande d’utilisateur",
    'diagnostic controller input key="Enter"': "Entrée injectée",
    "argument clamp:": "Premier message du bornage indirect",
    "the indirect argument clamp uses precompiled DXIL": "Bornage DXIL",
    "card compare uses precompiled DXIL": "Comparateur GPU DXIL",
    "the first frame of 500 draws or more": "Seuil de 500 appels de dessin",
}
SHADOW = re.compile(r"allocation counters .*\bshadow_mib=(\d+)")
SHADOW_ORIGINS = re.compile(r"\bshadow_buffer_mib=(\d+) shadow_window_mib=(\d+) shadow_view_mib=(\d+)")
SHADOW_FRAME = re.compile(r"host copies of title memory: \d+ alive ([\d.]+) MB")
STORAGE_HELD = re.compile(r"dstorage: a queue half full submitted itself; .*\bheld=(\d+)")
CAPTURE = re.compile(r"frame-(\d+)\.png")
RUN_ALIAS = re.compile(r"^Z:/private/tmp/victory-mac-[^/]+/")
OPTION_NAMES = ("--max-wired-mib", "--poll-interval", "--bottle", "--dll")


def timestamp(value):
    if isinstance(value, str):
        try:
            when = datetime.fromisoformat(value)
            if when.tzinfo is not None:
                return when
        except ValueError:
            pass
    return None


def argument(command, option):
    if option in command:
        index = command.index(option) + 1
        if index < len(command):
            return command[index]
    return None


def state(run, meta):
    if meta.get("dry_run"):
        return "simulation"
    if "exit_code" in meta:
        return f"terminé, code {meta['exit_code']}"

    pid = meta.get("monitor_pid")
    telemetry = str(run / "telemetry.jsonl")
    command = meta.get("command") or []
    if isinstance(pid, int) and pid > 0 and argument(command, "--telemetry-path") == telemetry:
        try:
            result = subprocess.run(
                ["/bin/ps", "-ww", "-p", str(pid), "-o", "command="],
                capture_output=True, text=True, timeout=2, check=False,
            )
        except (OSError, subprocess.TimeoutExpired):
            return "fin non enregistrée, moniteur non vérifié"
        if (result.returncode == 0 and "monitor_xcl_run.py " in result.stdout
                and f"--telemetry-path {telemetry} " in result.stdout):
            return f"actif, moniteur {pid} vérifié"
    return "fin non enregistrée, moniteur non vérifié"


def lines(path):
    try:
        with path.open(encoding="utf-8", errors="replace") as stream:
            yield from stream
    except FileNotFoundError:
        return


def read_console(path):
    events = {}
    shadow, shadow_frames = [], []
    peak_shadow, origins_at_peak = -1, None
    held_peak = None
    stop = None
    for line in lines(path):
        if line.startswith("XCL stopped:"):
            stop = line.removeprefix("XCL stopped:").strip()
        if match := SHADOW.search(line):
            if when := timestamp(line.split()[0]):
                current = int(match[1])
                shadow.append((when, current))
                if current > peak_shadow:
                    peak_shadow = current
                    origins = SHADOW_ORIGINS.search(line)
                    origins_at_peak = tuple(map(int, origins.groups())) if origins else None
        elif match := SHADOW_FRAME.search(line):
            if when := timestamp(line.split()[0]):
                shadow_frames.append((when, float(match[1])))
        if match := STORAGE_HELD.search(line):
            held = int(match[1])
            if held > 0 and (held_peak is None or held > held_peak[0]):
                if when := timestamp(line.split()[0]):
                    held_peak = (held, when)
        for marker, name in EVENTS.items():
            if name not in events and marker in line:
                if when := timestamp(line.split()[0]):
                    events[name] = when
    return events, shadow or shadow_frames, bool(shadow), origins_at_peak, held_peak, stop


def wired_samples(path):
    for line in lines(path):
        try:
            row = json.loads(line)
        except ValueError:  # The last line can still be in flight in a live run.
            continue
        if isinstance(row, dict) and (when := timestamp(row.get("time_utc"))):
            value = row.get("wired_mib")
            if isinstance(value, (int, float)) and 0 <= value < float("inf"):
                yield when, value


def show_memory(label, samples, start):
    samples = sorted(samples, key=lambda sample: sample[0])
    if not samples:
        print(f"{label} — aucun relevé")
        return
    peak_time, peak = max(samples, key=lambda sample: sample[1])
    lag = f" à +{(peak_time - start).total_seconds():.1f} s" if start else ""
    delta = f", {peak - samples[0][1]:+.1f} Mio depuis le premier relevé" if len(samples) > 1 else ""
    print(f"{label} — pic échantillonné {peak:.1f} Mio{lag}{delta} ({len(samples)} relevés)")
    recent = next(sample for sample in samples if sample[0] >= peak_time - timedelta(seconds=5))
    elapsed = (peak_time - recent[0]).total_seconds()
    if elapsed > 0:
        print(f"  Approche du pic — {recent[1]:.1f} → {peak:.1f} Mio en {elapsed:.2f} s")


def summarize(run, meta):
    start = timestamp(meta.get("start_local"))
    end = timestamp(meta.get("end_local"))
    duration = f" en {(end - start).total_seconds():.0f} s" if start and end else ""
    print(f"{run} — {state(run, meta)}{duration}")
    print(f"Console {meta.get('console', 'non renseignée')} · SHA-256 {meta.get('sha256', 'non renseigné')}")
    events, shadow, between_frames, origins, held_peak, stop = read_console(run / "console.txt")
    for label, when in sorted(events.items(), key=lambda item: item[1]):
        at = f"+{(when - start).total_seconds():.1f} s" if start else when.isoformat()
        print(f"{label} — {at}")
    if not events:
        print("Aucun jalon horodaté reconnu dans le journal")
    if held_peak:
        held, when = held_peak
        print(f"DirectStorage — maximum journalisé de {held} commandes retenues à {when.isoformat(timespec='seconds')}")
    if stop:
        print(f"Arrêt signalé — {stop}")

    show_memory("Mémoire câblée macOS", wired_samples(run / "telemetry.jsonl"), start)
    if shadow:
        source = "sonde entre trames" if between_frames else "journal par trame"
        show_memory(f"Copies hôte Victory (taille logique, {source})", shadow, start)
        if origins:
            buffers, windows, views = origins
            print(f"  Au pic — {buffers} Mio de tampons, {windows} Mio de fenêtres, {views} Mio de vues")
    else:
        print("Copies hôte Victory — aucun relevé")

    captures = ((int(match[1]), file.name) for file in (run / "captures").glob("frame-*.png")
                if (match := CAPTURE.fullmatch(file.name)) and file.is_file())
    latest = max(captures, default=None)
    if latest:
        print(f"Dernière capture — {run / 'captures' / latest[1]}")
    else:
        print("Aucune capture PNG numérotée")


def recorded_options(meta):
    command = meta.get("command")
    if not command:
        return None
    options = {key: argument(command, key) for key in OPTION_NAMES}
    for entry in (argument(command, "--env") or "").split():
        key, sep, value = entry.partition("=")
        if sep and key.startswith("VICTORY_"):
            options[key] = RUN_ALIAS.sub("Z:/<essai>/", value)
    return options


def compare(reference_run, reference, current):
    print(f"\nRéférence — {reference_run}")
    for key, label in (("sha256", "Binaire"), ("console", "Console"), ("stage", "Paquet"),
                       ("profile_from", "Source déclarée du profil"),
                       ("ui_resolution", "Résolution du profil"), ("pipeline_seed_from", "Source des pipelines")):
        before, after = reference.get(key), current.get(key)
        if before != after:
            print(f"{label} — {before or 'non renseigné'} → {after or 'non renseigné'}")
        elif key in ("sha256", "console", "profile_from"):
            value = "identique" if before is not None else "non renseigné des deux côtés"
            print(f"{label} — {value}")
    print("Contenu initial des sauvegardes — non vérifié")

    before, after = recorded_options(reference), recorded_options(current)
    if before is None or after is None:
        print("Comparaison des options indisponible")
        return
    differences = sorted(key for key in before.keys() | after.keys() if before.get(key) != after.get(key))
    for key in differences:
        print(f"{key} — {before.get(key) or 'absent'} → {after.get(key) or 'absent'}")
    if not differences:
        print("Options enregistrées identiques")


def metadata(run):
    with (run / "launch.json").open(encoding="utf-8") as stream:
        record = json.load(stream)
    if not isinstance(record, dict):
        raise ValueError(f"{run / 'launch.json'} ne contient pas un enregistrement")
    return record


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("essai", type=Path)
    parser.add_argument("--reference", type=Path, help="Comparer les options enregistrées à cet essai")
    args = parser.parse_args()
    try:
        run = args.essai.resolve()
        current = metadata(run)
        reference_run = args.reference.resolve() if args.reference else None
        reference = metadata(reference_run) if reference_run else None
        summarize(run, current)
        if reference is not None:
            compare(reference_run, reference, current)
    except (OSError, ValueError) as error:
        parser.exit(1, f"{error}\n")


if __name__ == "__main__":
    main()
