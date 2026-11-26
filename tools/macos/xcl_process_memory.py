"""Optional Darwin telemetry for the XCL host that owns a launch's private log."""
from concurrent.futures import ThreadPoolExecutor
import ctypes
from datetime import datetime, timezone
import json
import os
import re
from pathlib import Path
import subprocess
import sys
import threading
import time

def game_footprint(pid: int) -> dict[str, object]:
    """Get the game's physical footprint and graphics categories from macOS."""
    result = subprocess.run(["footprint", "-p", str(pid), "--format", "bytes",
                             "--wired", "--swapped"], capture_output=True,
                            text=True, timeout=8)
    if result.returncode != 0:
        return {"pid": pid, "error_exit_code": result.returncode}
    values: dict[str, object] = {"pid": pid}
    for key in ("phys_footprint", "phys_footprint_peak"):
        match = re.search(rf"^\s*{key}:\s*(\d+) B$", result.stdout, re.MULTILINE)
        if match:
            values[f"{key}_mib"] = round(int(match.group(1)) / (1024 * 1024))
    values["graphics_category_lines"] = [line.strip() for line in result.stdout.splitlines()
                                         if "(graphics)" in line or "IOSurface" in line or "IOAccelerator" in line]
    return values


class Rusage(ctypes.Structure):
    # Darwin sys/resource.h, RUSAGE_INFO_V0. Sizes are bytes, start is mach absolute time.
    _fields_ = [("uuid", ctypes.c_ubyte * 16)] + [
        (name, ctypes.c_uint64) for name in (
            "user", "system", "idle", "wakes", "pageins", "wired", "resident",
            "footprint", "start", "exit",
        )
    ]


def native_reader():
    lib = ctypes.CDLL("/usr/lib/libproc.dylib")
    lib.proc_pid_rusage.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_void_p]
    lib.proc_pid_rusage.restype = ctypes.c_int

    def read(pid):
        info = Rusage()
        if lib.proc_pid_rusage(pid, 0, ctypes.byref(info)) or not info.start or info.exit:
            return None
        return {"pid": pid, "native_start_ticks": info.start, "footprint_bytes": info.footprint,
                "rss_bytes": info.resident, "wired_bytes": info.wired}
    return read


def writers(pgid, log):
    # This host opens xcl.log at host_main entry. A launcher, reader, or process
    # working on another run cannot qualify on the file name alone.
    result = subprocess.run(
        ["/usr/sbin/lsof", "-nP", "-a", "-g", str(pgid), "-Fpfa", str(log)],
        capture_output=True, text=True, timeout=2,
    )
    if result.returncode == 1 and not result.stderr and not result.stdout:
        return set()
    result.check_returncode()
    found = set()
    pid = None
    for field in result.stdout.splitlines():
        if field.startswith("p"):
            pid = int(field[1:])
        elif pid is not None and field in ("aw", "au"):
            found.add(pid)
    return found


class ProcessSampler(threading.Thread):
    def __init__(self, pgid: int, log: Path, output: Path, trace_footprint=False):
        super().__init__(name="xcl-process-memory", daemon=True)
        self.pgid = pgid
        self.log = log
        self.output = output
        self.trace_footprint = trace_footprint
        self.footprint_bytes = None
        self.done = threading.Event()

    def run(self):
        try:
            self.output.parent.mkdir(parents=True, exist_ok=True)
            with self.output.open("x", encoding="utf-8") as stream:
                def record(event, **fields):
                    if event == "sample":
                        self.footprint_bytes = fields.get("footprint_bytes")
                    now = datetime.now(timezone.utc).isoformat(timespec="milliseconds")
                    stream.write(json.dumps({"time_utc": now, "event": event, **fields}) + "\n")
                    stream.flush()
                record("start", pgid=self.pgid, log=str(self.log))
                try:
                    self.sample(record)
                except (OSError, ValueError, subprocess.SubprocessError) as error:
                    record("unavailable", reason=str(error), footprint_bytes=None)
        except OSError as error:
            print(f"Cannot save optional process telemetry: {error}", file=sys.stderr)
        finally:
            self.footprint_bytes = None

    def sample(self, record):
        read = native_reader()
        # Pin the native identity across two independent observations of the writer.
        while not self.done.is_set():
            matches = writers(self.pgid, self.log) if self.log.is_file() else set()
            if len(matches) > 1:
                record("ambiguous_writers", pids=sorted(matches), footprint_bytes=None)
                return
            if matches:
                pid = matches.pop()
                first = read(pid)
                if first and writers(self.pgid, self.log) == {pid}:
                    second = read(pid)
                    if second and second["native_start_ticks"] == first["native_start_ticks"]:
                        break
            self.done.wait(.05)
        else:
            record("no_identified_writer", footprint_bytes=None)
            return

        start = second["native_start_ticks"]
        record("sample", **second)

        def current():
            data = read(pid)
            try:
                if data and data["native_start_ticks"] == start and os.getpgid(pid) == self.pgid:
                    return data
            except ProcessLookupError:
                pass
            return None

        def profile():
            # The category scan may take seconds. Verify its identity again after
            # it returns and let the fast counter and the guard continue meanwhile.
            at = datetime.now(timezone.utc).isoformat(timespec="milliseconds")
            lines = None
            try:
                if current() is not None:
                    values = game_footprint(pid)
                    if current() is not None:
                        lines = values.get("graphics_category_lines")
            except (OSError, subprocess.SubprocessError):
                pass
            return {"started_utc": at, "category_lines": lines}

        next_snapshot = 0
        snapshot = None
        with ThreadPoolExecutor(max_workers=1, thread_name_prefix="xcl-footprint") as pool:
            while not self.done.is_set():
                if snapshot is not None and snapshot.done():
                    record("graphics", pid=pid, native_start_ticks=start, **snapshot.result())
                    snapshot = None
                if self.trace_footprint and snapshot is None and time.monotonic() >= next_snapshot:
                    snapshot = pool.submit(profile)
                    next_snapshot = time.monotonic() + 4
                if self.done.wait(.1):
                    break
                data = current()
                if data is None:
                    break
                record("sample", **data)
            if snapshot is not None:
                record("graphics", pid=pid, native_start_ticks=start, **snapshot.result())
        record("end", pid=pid, native_start_ticks=start)
