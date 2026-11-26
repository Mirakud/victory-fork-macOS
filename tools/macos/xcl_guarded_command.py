"""Relay XCL output and stop its process group on a Metal command-buffer memory failure."""

import os
import re
import selectors
import signal
import subprocess
import threading
import time


METAL_OOM = "Metal command buffer failed: out of memory"
_OOM = re.compile(
    rb"MTLCommandBufferErrorDomain Code=8\b[^\r\n]{0,96}Insufficient Memory"
    rb"|command buffer completion error:[^\r\n]{0,192}kIOGPUCommandBufferCallbackErrorOutOfMemory",
    re.IGNORECASE,
)


class GuardedCommand:
    def __init__(self, command, stdout, stderr, stdin=None):
        self.reason = None
        self.stopped = threading.Event()
        self.finished = threading.Event()
        self._graceful_stop = threading.Event()
        self._abort_lock = threading.Lock()
        self.paused = False
        merged = os.path.samestat(os.fstat(stdout.fileno()), os.fstat(stderr.fileno()))
        self.process = subprocess.Popen(command, stdin=stdin, stdout=subprocess.PIPE,
                                       stderr=subprocess.STDOUT if merged else subprocess.PIPE,
                                       start_new_session=True)
        self.thread = threading.Thread(target=self._relay, args=(stdout, stderr), name="xcl-output")
        self.thread.start()

    def _signal_group(self, sig):
        # cxstart --wait and its Wine child share this launch's PGID even if launchd
        # reparents the child. The group can still exist after the launcher exits.
        try:
            os.killpg(self.process.pid, sig)
            return True
        except ProcessLookupError:
            return False
        except PermissionError as denied:
            # Darwin can report EPERM as the group disappears, even for SIGKILL.
            # Inspect the entire PGID, since Wine children can outlive the launcher.
            try:
                live = self._has_live_group_member()
            except (OSError, subprocess.SubprocessError, ValueError) as error:
                raise denied from error
            if sig and live:
                raise
            return live

    def _has_live_group_member(self):
        rows = subprocess.check_output(["/bin/ps", "-axo", "pgid=,stat="], text=True, timeout=1)
        if not rows.strip():
            raise ValueError("cannot verify XCL process group: ps returned no processes")
        for row in rows.splitlines():
            group, state = row.split()
            if int(group) == self.process.pid and not state.startswith("Z"):
                return True
        return False

    def running(self):
        if self.process.poll() is None:
            return True
        return self._signal_group(0)

    def set_paused(self, pause):
        with self._abort_lock:
            if self.stopped.is_set() or self.paused == pause:
                return
            if self._signal_group(signal.SIGSTOP if pause else signal.SIGCONT):
                self.paused = pause

    def abort(self, reason):
        with self._abort_lock:
            if not self.stopped.is_set():
                self.reason = reason
                self._signal_group(signal.SIGKILL)
                self.stopped.set()

    def stop(self):
        self._graceful_stop.set()
        if self.stopped.is_set():
            return
        self._signal_group(signal.SIGTERM)
        # Deliver the pending termination signal even if the guard suspended the group.
        self.set_paused(False)
        deadline = time.monotonic() + 5
        while self.running():
            if time.monotonic() >= deadline:
                self._signal_group(signal.SIGKILL)
                break
            self.stopped.wait(0.05)
            if self.stopped.is_set():
                break
        self.process.wait(timeout=5)

    def _relay(self, stdout, stderr):
        tails = {}
        try:
            with selectors.DefaultSelector() as streams:
                streams.register(self.process.stdout, selectors.EVENT_READ, stdout)
                if self.process.stderr is not None:
                    streams.register(self.process.stderr, selectors.EVENT_READ, stderr)
                while streams.get_map():
                    ready = streams.select(0 if self.finished.is_set() else 0.05)
                    if not ready and self.finished.is_set():
                        break
                    for key, _ in ready:
                        chunk = os.read(key.fd, 65536)
                        if not chunk:
                            streams.unregister(key.fileobj)
                            continue
                        text = tails.get(key.fd, b"") + chunk
                        if self.reason is None and _OOM.search(text):
                            self.abort(METAL_OOM)
                        tails[key.fd] = text[-320:]
                        key.data.write(chunk)
                        key.data.flush()
                # A normal exit may close stdio just before the process disappears.
                if not streams.get_map() and self.reason is None and self.running():
                    if not self.finished.wait(0.25) and not self._graceful_stop.is_set() and self.running():
                        self.abort("XCL console streams closed while the launch was still running")
        except (OSError, ValueError) as error:
            self.abort(f"XCL output monitor failed: {error}")
        finally:
            self.process.stdout.close()
            if self.process.stderr is not None:
                self.process.stderr.close()

    def finish(self):
        # Drain output already written before an otherwise normal exit, including an OOM
        # in the last write. A service outside this launch may retain an idle write end.
        self.finished.set()
        self.thread.join()
        self.process.wait(timeout=5)
