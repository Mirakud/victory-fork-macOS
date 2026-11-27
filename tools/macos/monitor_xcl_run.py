#!/usr/bin/env python3
"""Run CrossOver/XCL while guarding this 16 GiB Mac's free memory."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time
from xcl_guarded_command import GuardedCommand


@dataclass
class PeakUsage:
    threshold: int
    peak: int = 0

    def record(self, free: int) -> dict[str, int]:
        used = 100 - free
        self.peak = max(self.peak, used)
        return {
            "used_percent": used,
            "peak_used_percent": self.peak,
            "report_threshold_percent": self.threshold,
            "peak_overshoot_points": max(0, self.peak - self.threshold),
        }


class MemoryThrottle:
    def __init__(self, wired_limit_mib, process_budget_mib):
        self.limit = wired_limit_mib
        self.resume_at = wired_limit_mib - min(256, wired_limit_mib / 8)
        self.process_budget = process_budget_mib
        self.mode = "running"
        self.next_switch = 0.0

    def update(self, guard, wired_mib, footprint_bytes):
        now = time.monotonic()
        mode = "running"
        if wired_mib >= self.limit or (self.mode == "paused" and wired_mib > self.resume_at):
            mode = "paused"
        elif wired_mib > self.resume_at or (
            footprint_bytes is not None and footprint_bytes >= self.process_budget * (1 << 20)
        ):
            mode = "slowed"
        if mode != self.mode:
            print(f"XCL memory throttle: {mode} (system wired {wired_mib:.1f} MiB)", file=sys.stderr, flush=True)
            self.mode = mode
            self.next_switch = 0.0
        if mode != "slowed":
            guard.set_paused(mode == "paused")
        elif now >= self.next_switch:
            guard.set_paused(not guard.paused)
            self.next_switch = now + (0.30 if guard.paused else 0.10)


def free_percent() -> int:
    result = subprocess.run(["memory_pressure", "-Q"], capture_output=True, text=True,
                            timeout=5)
    if result.returncode != 0:
        raise RuntimeError(f"memory_pressure exited with code {result.returncode}")
    match = re.search(r"free percentage: (\d+)%", result.stdout)
    if match is None:
        raise RuntimeError("memory_pressure did not report a free-memory percentage")
    return int(match.group(1))


def parse_vm_stat(output: str) -> tuple[int, int, int]:
    size = re.search(r"page size of (\d+) bytes", output)
    def pages(name: str) -> int:
        match = re.search(rf"^{re.escape(name)}:\s*([\d,]+)\.?\s*$", output, re.MULTILINE)
        if match is None:
            raise RuntimeError(f"vm_stat did not report {name}")
        return int(match.group(1).replace(",", ""))
    if size is None:
        raise RuntimeError("vm_stat did not report a page size")
    return int(size.group(1)), pages("Pages free"), pages("Pages wired down")


def vm_stat() -> tuple[int, int, int]:
    result = subprocess.run(["vm_stat"], capture_output=True, text=True, timeout=5)
    if result.returncode != 0:
        raise RuntimeError(f"vm_stat exited with code {result.returncode}")
    return parse_vm_stat(result.stdout)


def sample_memory(with_vm_stat: bool, telemetry_path: Path | None,
                  usage: PeakUsage | None = None) -> tuple[int, int | None, float | None]:
    free = free_percent()
    page_size, raw_free, wired_pages = vm_stat() if with_vm_stat else (0, None, None)
    wired_mib = wired_pages * page_size / (1 << 20) if wired_pages is not None else None
    if telemetry_path is not None:
        telemetry_path.parent.mkdir(parents=True, exist_ok=True)
        with telemetry_path.open("a", encoding="utf-8") as stream:
            stream.write(json.dumps({
                "time_utc": datetime.now(timezone.utc).isoformat(timespec="milliseconds"),
                "free_percent": free,
                "raw_free_pages": raw_free,
                "wired_mib": wired_mib,
                "page_size_bytes": page_size,
                **(usage.record(free) if usage is not None else {}),
            }, separators=(",", ":")) + "\n")
            stream.flush()
            os.fsync(stream.fileno())
    return free, raw_free, wired_mib


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    percent_mode = parser.add_mutually_exclusive_group()
    percent_mode.add_argument("--min-free-percent", type=int,
                              help="Stop XCL if system free memory falls below this percentage (default: 10)")
    percent_mode.add_argument("--report-peak-used-percent", type=int, metavar="PERCENT",
                              help="Log peak used percentage and overshoot without stopping on percentage. "
                                   "Requires an absolute telemetry path and either a wired cap or --no-memory-limit.")
    parser.add_argument("--no-memory-limit", action="store_true",
                        help="Record memory usage while letting macOS manage all available RAM.")
    parser.add_argument("--max-wired-mib", type=int,
                        help="Wired memory threshold in MiB (stops XCL unless throttling is selected)")
    parser.add_argument("--throttle-process-mib", type=int, metavar="MIB",
                        help="Slow the title at this process footprint and suspend it at the wired threshold")
    parser.add_argument("--telemetry-path", type=Path,
                        help="Append and sync one memory sample per poll to this persistent absolute path")
    parser.add_argument("--process-log", type=Path, metavar="ABS_PATH",
                        help="Identify the XCL host by the writable xcl.log of this launch's private profile")
    parser.add_argument("--process-telemetry-path", type=Path, metavar="NEW_JSONL",
                        help="Save macOS host-process memory counters to this separate new file")
    parser.add_argument("--trace-footprint", action="store_true",
                        help="Also collect graphics category lines from footprint every four seconds")
    parser.add_argument("--poll-interval", type=float, default=1.0,
                        help="Seconds to wait between memory samples (default: 1)")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.no_memory_limit and any(value is not None for value in
                                    (args.max_wired_mib, args.min_free_percent, args.throttle_process_mib)):
        parser.error("--no-memory-limit cannot be combined with a memory stop or throttle")
    usage = None
    if args.report_peak_used_percent is not None:
        if not 1 <= args.report_peak_used_percent <= 100:
            parser.error("--report-peak-used-percent must be from 1 to 100")
        if not args.no_memory_limit and (args.max_wired_mib is None or args.max_wired_mib > 10240):
            parser.error("--report-peak-used-percent requires --max-wired-mib of at most 10240")
        if args.telemetry_path is None:
            parser.error("--report-peak-used-percent requires --telemetry-path")
        usage = PeakUsage(args.report_peak_used_percent)
    elif args.min_free_percent is None and not args.no_memory_limit:
        args.min_free_percent = 10
    if args.min_free_percent is not None and not 1 <= args.min_free_percent <= 100:
        parser.error("--min-free-percent must be from 1 to 100")
    if args.max_wired_mib is not None and args.max_wired_mib <= 0:
        parser.error("--max-wired-mib must be positive")
    throttle = None
    if args.throttle_process_mib is not None:
        if args.throttle_process_mib < 1 or args.max_wired_mib is None or args.process_log is None or usage is None:
            parser.error("throttling requires a positive process budget, a wired threshold, process telemetry and a reported peak percentage")
        throttle = MemoryThrottle(args.max_wired_mib, args.throttle_process_mib)
    if args.telemetry_path is not None and not args.telemetry_path.is_absolute():
        parser.error("--telemetry-path must be absolute")
    if (args.process_log is None) != (args.process_telemetry_path is None):
        parser.error("--process-log and --process-telemetry-path must be used together")
    if args.process_telemetry_path is not None:
        if not (args.process_log.is_absolute() and args.process_telemetry_path.is_absolute()):
            parser.error("process telemetry requires absolute paths")
        output = args.process_telemetry_path.resolve()
        inputs = [args.process_log] + ([args.telemetry_path] if args.telemetry_path else [])
        if output.exists() or any(output == path.resolve() for path in inputs):
            parser.error("--process-telemetry-path requires a new separate file")
    if args.trace_footprint and args.process_log is None:
        parser.error("--trace-footprint requires --process-log")
    if not 0 < args.poll_interval < float("inf"):
        parser.error("--poll-interval must be a finite positive number")
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("a CrossOver command is required after --")

    with_vm_stat = args.max_wired_mib is not None or args.telemetry_path is not None
    try:
        current_free, _, wired_mib = sample_memory(with_vm_stat, args.telemetry_path, usage)
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
        print(f"Cannot monitor memory: {error}", file=sys.stderr)
        return 2
    if args.min_free_percent is not None and current_free < args.min_free_percent:
        print(f"Only {current_free}% of system memory is free; XCL was not started.",
              file=sys.stderr)
        return 2
    if args.max_wired_mib is not None and wired_mib >= args.max_wired_mib:
        if throttle is None:
            print(f"Wired memory is {wired_mib:.1f} MiB (guard: {args.max_wired_mib} MiB); XCL was not started.",
                  file=sys.stderr)
            return 2
        print("Waiting for wired memory to fall before starting XCL.", file=sys.stderr, flush=True)
        try:
            while wired_mib > throttle.resume_at:
                time.sleep(args.poll_interval)
                _, _, wired_mib = sample_memory(True, args.telemetry_path, usage)
        except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
            print(f"Cannot monitor memory: {error}", file=sys.stderr)
            return 2
        except KeyboardInterrupt:
            return 130

    guard = GuardedCommand(command, sys.stdout.buffer, sys.stderr.buffer)
    process = guard.process
    stop_reason = None
    sampler = None
    interrupted = False

    def request_stop(signum, frame):
        nonlocal interrupted
        interrupted = True

    previous_signals = {}
    try:
        if throttle is not None:
            for sig in (signal.SIGHUP, signal.SIGINT, signal.SIGTERM):
                previous_signals[sig] = signal.signal(sig, request_stop)
        if args.process_log is not None:
            from xcl_process_memory import ProcessSampler
            sampler = ProcessSampler(process.pid, args.process_log, args.process_telemetry_path, args.trace_footprint)
            sampler.start()
        while not guard.stopped.is_set() and guard.running():
            if interrupted:
                stop_reason = "interrupted by user"
                break
            try:
                current_free, _, wired_mib = sample_memory(with_vm_stat, args.telemetry_path, usage)
            except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
                guard.abort(f"memory monitor failed: {error}")
                break
            if args.min_free_percent is not None and current_free < args.min_free_percent:
                stop_reason = (f"system free memory fell to {current_free}% "
                               f"(guard: {args.min_free_percent}%)")
                break
            if throttle is not None:
                throttle.update(guard, wired_mib, sampler.footprint_bytes)
            elif args.max_wired_mib is not None and wired_mib >= args.max_wired_mib:
                guard.abort(f"wired memory reached {wired_mib:.1f} MiB "
                            f"(guard: {args.max_wired_mib} MiB)")
                break
            guard.stopped.wait(args.poll_interval)
    except KeyboardInterrupt:
        stop_reason = "interrupted by user"
    except (OSError, subprocess.SubprocessError) as error:
        stop_reason = f"XCL monitor failed: {error}"
    finally:
        if stop_reason is not None:
            guard.stop()
        guard.finish()
        if sampler is not None:
            sampler.done.set()
            sampler.join()
        for sig, handler in previous_signals.items():
            signal.signal(sig, handler)
        stop_reason = guard.reason or stop_reason

    if usage is not None:
        print(f"Peak sampled memory usage: {usage.peak}% "
              f"({max(0, usage.peak - usage.threshold)} percentage points above {usage.threshold}%)",
              file=sys.stderr)
    if stop_reason is not None:
        print(f"XCL stopped: {stop_reason}", file=sys.stderr)
        return 1
    return process.returncode or 0


if __name__ == "__main__":
    raise SystemExit(main())
