"""Feed Victory's macOS overlay from the guarded run's own process and logs."""
import ctypes
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import threading
import time

from xcl_process_memory import native_reader


FRAME = re.compile(rb'victory_gpu::host: [\d.]+ fps, ([\d.]+) ms/frame')
BREAKDOWN = re.compile(rb'preparing: copies ([\d.]+) ms [^;]+; ([\d.]+) on the GPU;')


def build_hud(source):
    key = hashlib.sha256(source.read_bytes()).hexdigest()[:20]
    folder = Path.home() / "Library/Caches/victory-macos/hud" / key
    folder.mkdir(parents=True, exist_ok=True)
    binary = folder / "victory-perf-hud"
    if not binary.is_file():
        with tempfile.TemporaryDirectory(dir=folder) as tmp:
            output = Path(tmp) / binary.name
            subprocess.run(["/usr/bin/xcrun", "swiftc", "-O", str(source), "-o", str(output)], check=True)
            output.replace(binary)
    return binary


def write_state(path, data):
    tmp = path.with_suffix(".partial")
    tmp.write_text(json.dumps(data, allow_nan=False) + "\n")
    tmp.replace(path)


def wired_memory(path):
    if not path.is_file():
        return None
    with path.open("rb") as stream:
        stream.seek(max(0, stream.seek(0, os.SEEK_END) - 4096))
        for line in reversed(stream.read().splitlines()):
            try:
                return json.loads(line)["wired_mib"]
            except (ValueError, KeyError):
                continue
    return None


def find_game(candidate, monitor_pid):
    output = subprocess.check_output(["/bin/ps", "-axo", "pid=,ppid=,pgid=,comm="], text=True, timeout=2)
    rows = [row.split(None, 3) for row in output.splitlines()]
    parents = {int(pid): int(parent) for pid, parent, _, _ in rows}
    groups = {pid for pid, parent in parents.items() if parent == monitor_pid}
    wanted = str(candidate).casefold()
    for pid, parent, group, name in rows:
        native = name.replace("\\", "/").removeprefix("Z:").removeprefix("z:").casefold()
        if native == wanted and (int(group) in groups or int(parent) == monitor_pid):
            return int(pid)
    return None


class PerformanceHUD:
    def __init__(self, binary, run, candidate, monitor_pid):
        self.path = run / "hud.json"
        self.run = run
        self.candidate = candidate
        self.monitor_pid = monitor_pid
        self.stop = threading.Event()
        self.read_memory = native_reader(include_cpu=True)
        self.ticks = ctypes.CDLL(None).mach_absolute_time
        self.ticks.argtypes = []
        self.ticks.restype = ctypes.c_uint64
        write_state(self.path, {"updated": time.time()})
        self.log = (run / "hud.log").open("wb")
        try:
            self.panel = subprocess.Popen([str(binary), str(self.path)], stdin=subprocess.DEVNULL,
                                          stdout=self.log, stderr=subprocess.STDOUT)
        except OSError:
            self.log.close()
            raise
        self.worker = threading.Thread(target=self.collect, name="victory-hud-feed", daemon=True)
        self.worker.start()

    def collect(self):
        try:
            self.sample()
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            self.log.write(f"HUD telemetry: {error}\n".encode())
            self.log.flush()

    def sample(self):
        pid = start = frame_ms = measured = None
        gpu_ms = copy_ms = previous_cpu = previous_wall = None
        pending = b""
        with (self.run / "console.txt").open("rb") as console:
            while not self.stop.is_set() and self.panel.poll() is None:
                if pid is None:
                    pid = find_game(self.candidate, self.monitor_pid)
                mem = self.read_memory(pid) if pid is not None else None
                cpu_percent = None
                if mem:
                    if start is not None and mem["native_start_ticks"] != start:
                        break
                    start = mem["native_start_ticks"]
                    wall = self.ticks()
                    cpu = mem["cpu_ticks"]
                    if previous_wall is not None and wall > previous_wall and cpu >= previous_cpu:
                        cpu_percent = 100 * (cpu - previous_cpu) / (wall - previous_wall)
                    previous_wall, previous_cpu = wall, cpu
                elif start is not None:
                    break
                data = pending + console.read(1 << 20)
                lines = data.split(b"\n")
                pending = lines.pop()
                for line in reversed(lines):
                    match = FRAME.search(line)
                    if match and float(match[1]) > 0:
                        frame_ms = float(match[1])
                        breakdown = BREAKDOWN.search(line, match.end())
                        copy_ms, gpu_ms = map(float, breakdown.groups()) if breakdown else (None, None)
                        measured = time.monotonic()
                        break
                recent = measured is not None and time.monotonic() - measured <= max(20, 3 * frame_ms / 1000)
                write_state(self.path, {
                    "game_pid": pid, "updated": time.time(),
                    "fps": 1000 / frame_ms if recent else None, "frame_ms": frame_ms if recent else None,
                    "gpu_ms": gpu_ms if recent else None, "copy_ms": copy_ms if recent else None,
                    "cpu_percent": cpu_percent,
                    "ram_mib": mem["footprint_bytes"] / (1 << 20) if mem else None,
                    "wired_mib": wired_memory(self.run / "telemetry.jsonl"),
                })
                self.stop.wait(1)

    def close(self):
        self.stop.set()
        self.worker.join()
        write_state(self.path, {"ended": True, "updated": time.time()})
        try:
            self.panel.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.panel.terminate()
            self.panel.wait()
        self.log.close()
