"""Drive Inti inside QEMU from Python: serial console, QMP monitor, screenshots.

    from vm import VM
    with VM() as vm:
        vm.wait_for_prompt()
        print(vm.command("ps"))

The serial port and the monitor are TCP connections that QEMU makes to sockets this side
already listens on. The machine is started stopped ("-S") and let go only when both are
open, so the first byte of the boot log is never lost.
"""
from __future__ import annotations

import atexit
import json
import re
import socket
import os
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
import build  # noqa: E402

PROMPT = "inti> "
# Kernel and supervisor log lines are written to the serial port in one piece,
# but they can land in the middle of a line that the console driver is writing.
LOG_LINE = re.compile(rb"(?:init|kernel): [^\r\n]*\r\n")


# Windows priority classes for the QEMU process; elsewhere the flag is 0 and does nothing.
PRIORITY_CLASSES = {
    "normal": 0,
    "above": getattr(subprocess, "ABOVE_NORMAL_PRIORITY_CLASS", 0),
    "high": getattr(subprocess, "HIGH_PRIORITY_CLASS", 0),
}
DEFAULT_PRIORITY = "above"


# Every QEMU this process has started. A test that fails between `start` and `close`
# (a machine that never reaches its prompt, for example) would otherwise leave its QEMU
# running: on Windows a child outlives its parent.
_started: list[subprocess.Popen] = []
_job = None  # Windows only: a job object whose processes end when this process ends


def _kill_leftovers() -> None:
    for process in _started:
        if process.poll() is None:
            process.kill()


atexit.register(_kill_leftovers)


def _tie_to_this_process(process: subprocess.Popen) -> None:
    """Make sure `process` does not outlive this one.

    `_kill_leftovers` covers an ordinary exit. It does not run when this process is
    killed from outside (a test that ran out of time), so on Windows the child is also
    put into a job object that ends its processes when its last handle is closed, and
    the only handle is ours. If the job cannot be made, the at-exit cleanup still holds.
    """
    global _job
    _started.append(process)
    if os.name != "nt":
        return
    try:
        import ctypes
        from ctypes import wintypes

        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel32.CreateJobObjectW.restype = wintypes.HANDLE
        kernel32.CreateJobObjectW.argtypes = [ctypes.c_void_p, wintypes.LPCWSTR]
        kernel32.SetInformationJobObject.argtypes = [wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD]
        kernel32.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
        if _job is None:
            class BasicLimits(ctypes.Structure):
                _fields_ = [
                    ("PerProcessUserTimeLimit", ctypes.c_int64),
                    ("PerJobUserTimeLimit", ctypes.c_int64),
                    ("LimitFlags", wintypes.DWORD),
                    ("MinimumWorkingSetSize", ctypes.c_size_t),
                    ("MaximumWorkingSetSize", ctypes.c_size_t),
                    ("ActiveProcessLimit", wintypes.DWORD),
                    ("Affinity", ctypes.c_size_t),
                    ("PriorityClass", wintypes.DWORD),
                    ("SchedulingClass", wintypes.DWORD),
                ]

            class IoCounters(ctypes.Structure):
                _fields_ = [(name, ctypes.c_uint64) for name in (
                    "ReadOperationCount", "WriteOperationCount", "OtherOperationCount",
                    "ReadTransferCount", "WriteTransferCount", "OtherTransferCount")]

            class ExtendedLimits(ctypes.Structure):
                _fields_ = [
                    ("BasicLimitInformation", BasicLimits),
                    ("IoInfo", IoCounters),
                    ("ProcessMemoryLimit", ctypes.c_size_t),
                    ("JobMemoryLimit", ctypes.c_size_t),
                    ("PeakProcessMemoryUsed", ctypes.c_size_t),
                    ("PeakJobMemoryUsed", ctypes.c_size_t),
                ]

            kill_on_job_close = 0x2000
            extended_limit_information = 9
            job = kernel32.CreateJobObjectW(None, None)
            limits = ExtendedLimits()
            limits.BasicLimitInformation.LimitFlags = kill_on_job_close
            if not job or not kernel32.SetInformationJobObject(
                    job, extended_limit_information, ctypes.byref(limits), ctypes.sizeof(limits)):
                return
            _job = job
        kernel32.AssignProcessToJobObject(_job, int(process._handle))
    except (OSError, AttributeError, ImportError):
        pass


class VMError(Exception):
    pass


# How long one step of a start may take: QEMU making one of its connections, or answering
# on the monitor. An ordinary start takes well under a second.
START_STEP_S = 30
# A start in which QEMU stays alive but does not get through a step in that time is tried
# again from the beginning, up to this many starts in all. The machine is still stopped
# then and has not run one instruction, so a second try cannot hide anything the system
# under test did. Every second try is counted in `start_retries` and reported on stderr.
# A QEMU that exits by itself (a wrong argument, a missing image) is not tried again.
START_ATTEMPTS = 3
start_retries = 0


def _listener() -> socket.socket:
    """A listening socket on a port the system chooses, for QEMU to connect to.

    The harness once chose a free port, closed it and told QEMU to listen there. Another
    process could take the port in between, and a connection attempt given up on this
    side could still be accepted by QEMU. Both are impossible when this side listens.
    """
    channel = socket.socket()
    channel.bind(("127.0.0.1", 0))
    channel.listen(1)
    return channel


class VM:
    def __init__(self, image: Path | None = None, memory_mb: int = 64, extra: list[str] | None = None,
                 debug_log: Path | None = None, snapshot: bool = True, drive: str | None = None):
        self.image = Path(image) if image else build.BUILD / "inti.img"
        # With `snapshot`, what the machine writes to its disk is thrown away when QEMU
        # exits and the image file stays as it is. Tests of what survives a restart pass
        # False and a copy of the image.
        self.snapshot = snapshot
        # `drive` replaces the whole "-drive" value, for a test that puts a block filter
        # (a log of every write, for example) between the machine and the image file.
        self.drive = drive
        self.memory_mb = memory_mb
        self.extra = extra or []
        self.debug_log = debug_log
        self.raw = bytearray()  # every byte the serial port has produced
        self.cursor = 0  # how far `read_until` has consumed `raw`
        self.serial_closed = False
        self.process = None
        self.serial = None
        self.monitor = None
        self.monitor_buffer = b""
        self.started_at = 0.0

    def __enter__(self) -> "VM":
        self.start()
        return self

    def __exit__(self, *_) -> None:
        self.close()

    def start(self) -> None:
        global start_retries
        failures = []
        while True:
            try:
                self._start_stopped()
                break
            except VMError as error:
                exited_by_itself, written = self._abandon()
                failures.append(f"{error}; QEMU wrote: {written or 'nothing'}")
                if exited_by_itself or len(failures) == START_ATTEMPTS:
                    raise VMError("QEMU did not start: " + " | then: ".join(failures)) from error
                start_retries += 1
                print(f"vm.py: a start failed and is tried again (number {start_retries} in this process): "
                      f"{failures[-1]}", file=sys.stderr, flush=True)
        self.qmp("cont")  # both channels are open: let the machine run
        self.started_at = time.monotonic()

    def _start_stopped(self) -> None:
        """Start QEMU with the machine stopped and both channels open, or raise VMError."""
        listeners = {"serial": _listener(), "monitor": _listener()}
        ports = {name: listening.getsockname()[1] for name, listening in listeners.items()}
        command = [
            build.find_qemu(),
            "-cpu", build.QEMU_CPU,  # a later "-cpu" in `extra` replaces this one
            "-drive", self.drive or f"format=raw,file={self.image},if=ide",
            "-m", str(self.memory_mb),
            "-display", "none",
            "-no-reboot",
            "-S",  # stopped until `cont` in `start`
            "-serial", f"tcp:127.0.0.1:{ports['serial']},server=off",
            "-qmp", f"tcp:127.0.0.1:{ports['monitor']},server=off",
            *self.extra,
        ]
        if self.snapshot:
            command.append("-snapshot")
        if self.debug_log:
            command += ["-d", "int,cpu_reset", "-D", str(self.debug_log)]
        # On a busy host the emulated CPU can be left waiting for longer than the guest's
        # 54.9 ms clock counter takes to wrap, and the guest clock then falls behind.
        # Running QEMU above normal priority keeps that from happening. The environment
        # variable INTI_QEMU_PRIORITY chooses the class: "normal", "above" or "high".
        priority = PRIORITY_CLASSES[os.environ.get("INTI_QEMU_PRIORITY", DEFAULT_PRIORITY)]
        done = []  # the steps that worked, with the time each took
        step = "starting the QEMU process"
        began = time.monotonic()

        def next_step(name: str) -> None:
            nonlocal step, began
            done.append(f"{step} {time.monotonic() - began:.1f} s")
            step = name
            began = time.monotonic()

        self.process = None
        try:
            self.process = subprocess.Popen(
                command,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                creationflags=priority,
            )
            _tie_to_this_process(self.process)
            next_step("waiting for QEMU to connect its serial port")
            self.serial = self._accept(listeners["serial"])
            self.serial.settimeout(0.05)
            next_step("waiting for QEMU to connect its monitor")
            self.monitor = self._accept(listeners["monitor"])
            self.monitor.settimeout(START_STEP_S)
            next_step("waiting for the monitor's greeting")
            self._monitor_read()
            next_step("waiting for the monitor's answer to the first command")
            self.qmp("qmp_capabilities")
            self.monitor.settimeout(10)
        except (OSError, ValueError, VMError) as error:
            if self.process is None:
                process_state = "the QEMU process was not created"
            elif self.process.poll() is None:
                process_state = "QEMU was still running"
            else:
                process_state = f"QEMU had exited with code {self.process.poll()}"
            raise VMError(f"{step} failed after {time.monotonic() - began:.1f} s ({type(error).__name__}: {error}); "
                          f"before that: {', '.join(done) or 'nothing'}; {process_state}") from error
        finally:
            for listening in listeners.values():
                listening.close()

    def _accept(self, listening: socket.socket) -> socket.socket:
        deadline = time.monotonic() + START_STEP_S
        listening.settimeout(0.2)
        while True:
            try:
                return listening.accept()[0]
            except (socket.timeout, TimeoutError):
                if self.process.poll() is not None:
                    raise VMError("QEMU exited before it connected") from None
                if time.monotonic() > deadline:
                    raise VMError(f"no connection in {START_STEP_S} s") from None

    def _abandon(self) -> tuple[bool, str]:
        """Clear away a start that failed; return whether QEMU had exited by itself, and what it wrote."""
        exited_by_itself = self.process is None  # it could not even be created: the same holds
        written = ""
        if self.process:
            exited_by_itself = self.process.poll() is not None
            if not exited_by_itself:
                self.process.kill()
            self.process.wait()
            written = self.process.stderr.read().decode("utf-8", "replace").strip()
            self.process.stderr.close()
        for name in ("serial", "monitor"):
            channel = getattr(self, name)
            if channel:
                channel.close()
            setattr(self, name, None)
        self.monitor_buffer = b""
        return exited_by_itself, written

    # --- serial port -------------------------------------------------------

    def pump(self, wait: float = 0.05) -> None:
        """Move whatever the serial port has produced into `raw`."""
        if self.serial_closed:
            time.sleep(wait)
            return
        self.serial.settimeout(wait)
        try:
            data = self.serial.recv(65536)
        except (socket.timeout, TimeoutError):
            return
        except (ConnectionResetError, ConnectionAbortedError):
            data = b""
        if data:
            self.raw += data
        else:
            self.serial_closed = True  # QEMU closed the port: the machine is off

    def send(self, text: str) -> None:
        self.serial.sendall(text.encode("ascii"))

    def read_until(self, pattern: str, timeout: float = 20) -> str:
        """Wait until `pattern` (a regular expression) appears in new output; return that output."""
        matcher = re.compile(pattern.encode("ascii"))
        deadline = time.monotonic() + timeout
        while True:
            found = matcher.search(self.raw, self.cursor)
            if found:
                text = bytes(self.raw[self.cursor:found.end()])
                self.cursor = found.end()
                return clean(text)
            if b"KERNEL PANIC" in self.raw[self.cursor:]:
                self.pump(0.3)
                raise VMError("kernel panic:\n" + clean(bytes(self.raw[self.cursor:])))
            if time.monotonic() > deadline:
                tail = clean(bytes(self.raw[self.cursor:]))[-1500:]
                raise VMError(f"timed out after {timeout} s waiting for {pattern!r}; "
                              f"machine: {self.state()}; output so far:\n{tail}")
            if self.process.poll() is not None:
                self.pump(0.2)
                if not matcher.search(self.raw, self.cursor):
                    raise VMError(f"QEMU exited while waiting for {pattern!r}")
            self.pump()

    def wait_for_prompt(self, timeout: float = 20) -> str:
        return self.read_until(re.escape(PROMPT), timeout)

    def command(self, line: str, timeout: float = 30, expect: str | None = None) -> str:
        """Type one shell command and return what it printed, up to the next prompt."""
        self.send(line + "\r")
        return self.read_until(expect or re.escape(PROMPT), timeout)

    def log(self) -> str:
        return bytes(self.raw).decode("utf-8", "replace")

    # --- monitor -----------------------------------------------------------

    def _monitor_read(self) -> dict:
        while b"\n" not in self.monitor_buffer:
            data = self.monitor.recv(65536)
            if not data:
                raise VMError("QEMU closed the monitor connection")
            self.monitor_buffer += data
        line, _, self.monitor_buffer = self.monitor_buffer.partition(b"\n")
        return json.loads(line)

    def qmp(self, execute: str, **arguments) -> dict:
        request = {"execute": execute}
        if arguments:
            request["arguments"] = arguments
        try:
            self.monitor.sendall(json.dumps(request).encode("ascii") + b"\n")
            while True:
                answer = self._monitor_read()
                if "return" in answer:
                    return answer["return"]
                if "error" in answer:
                    raise VMError(f"QMP {execute}: {answer['error']}")
        except OSError as error:
            raise VMError(f"QMP {execute}: no answer ({type(error).__name__}: {error})") from error

    def state(self) -> str:
        """One line on what the machine is doing now, for the message of a wait that ran out."""
        try:
            status = self.qmp("query-status")["status"]
            registers = self.qmp("human-monitor-command", **{"command-line": "info registers"})
            found = re.search(r"EIP=([0-9a-f]+).*?HLT=(\d)", registers, re.S)
            where = f"EIP={found[1]} HLT={found[2]}" if found else "registers not read"
            return f"{status}, {where}, {len(self.raw)} serial bytes in all"
        except (OSError, VMError, ValueError, KeyError, TypeError) as error:
            return f"state not read ({type(error).__name__})"

    def screenshot(self, path: Path) -> Path:
        """Save what the emulated VGA screen shows as a PNG."""
        from PIL import Image

        path = Path(path)
        path.parent.mkdir(parents=True, exist_ok=True)
        raw_path = path.with_suffix(".ppm")
        self.qmp("screendump", filename=str(raw_path))
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            try:
                with Image.open(raw_path) as picture:
                    picture.save(path)
                break
            except (OSError, SyntaxError):
                time.sleep(0.1)
        raw_path.unlink(missing_ok=True)
        return path

    def press(self, *keys: str) -> None:
        """Press keys on the emulated PS/2 keyboard, one after another (QEMU key names)."""
        for key in keys:
            names = key.split("-")
            self.qmp("send-key", keys=[{"type": "qcode", "data": name} for name in names], **{"hold-time": 40})
            time.sleep(0.09)

    def close(self) -> None:
        if self.process and self.process.poll() is None:
            try:
                self.qmp("quit")
            except (OSError, VMError, ValueError):
                pass
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
        for channel in (self.serial, self.monitor):
            if channel:
                channel.close()


def clean(raw: bytes) -> str:
    """Serial output as text, without kernel and supervisor log lines and without carriage returns."""
    return LOG_LINE.sub(b"", raw).decode("utf-8", "replace").replace("\r", "")


def main() -> int:
    """Boot, run the commands given on the command line, print everything.

    `--trace` as the first argument also writes QEMU's interrupt trace to
    build/qemu_debug.log. Tracing slows every trap down, so never time anything with it.
    """
    lines = sys.argv[1:]
    trace = bool(lines) and lines[0] == "--trace"
    if trace:
        lines = lines[1:]
    with VM(debug_log=build.BUILD / "qemu_debug.log" if trace else None) as vm:
        try:
            vm.wait_for_prompt()
            for line in lines:
                vm.command(line, timeout=120)
        except VMError as error:
            print(f"vm.py: {error}")
        print(vm.log())
    return 0


if __name__ == "__main__":
    sys.exit(main())
