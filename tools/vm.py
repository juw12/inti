"""Drive Inti inside QEMU from Python: serial console, QMP monitor, screenshots.

    from vm import VM
    with VM() as vm:
        vm.wait_for_prompt()
        print(vm.command("ps"))

The serial port is a TCP socket that QEMU opens before the machine starts, so
the first byte of the boot log is never lost.
"""
from __future__ import annotations

import json
import re
import socket
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


def free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def connect(port: int, deadline: float) -> socket.socket:
    while True:
        try:
            return socket.create_connection(("127.0.0.1", port), timeout=1)
        except OSError:
            if time.monotonic() > deadline:
                raise
            time.sleep(0.05)


class VMError(Exception):
    pass


class VM:
    def __init__(self, image: Path | None = None, memory_mb: int = 64, extra: list[str] | None = None,
                 debug_log: Path | None = None):
        self.image = Path(image) if image else build.BUILD / "inti.img"
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
        serial_port = free_port()
        monitor_port = free_port()
        command = [
            build.find_qemu(),
            "-cpu", build.QEMU_CPU,  # a later "-cpu" in `extra` replaces this one
            "-drive", f"format=raw,file={self.image},if=ide",
            "-m", str(self.memory_mb),
            "-display", "none",
            "-no-reboot",
            "-serial", f"tcp:127.0.0.1:{serial_port},server=on,wait=on",
            "-qmp", f"tcp:127.0.0.1:{monitor_port},server=on,wait=off",
            *self.extra,
        ]
        if self.debug_log:
            command += ["-d", "int,cpu_reset", "-D", str(self.debug_log)]
        # On a busy host the emulated CPU can be left waiting for longer than the guest's
        # 54.9 ms clock counter takes to wrap, and the guest clock then falls behind.
        # Running QEMU just above normal priority keeps that from happening.
        priority = getattr(subprocess, "ABOVE_NORMAL_PRIORITY_CLASS", 0)
        self.process = subprocess.Popen(
            command,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            creationflags=priority,
        )
        deadline = time.monotonic() + 30
        try:
            # QEMU waits for the serial connection before it opens the monitor.
            self.serial = connect(serial_port, deadline)
            self.monitor = connect(monitor_port, deadline)
        except OSError as error:
            self.process.kill()
            details = self.process.stderr.read().decode("utf-8", "replace")
            raise VMError(f"QEMU did not start: {error}: {details}") from error
        self.started_at = time.monotonic()
        self.serial.settimeout(0.05)
        self.monitor.settimeout(10)
        self._monitor_read()  # the greeting
        self.qmp("qmp_capabilities")

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
                raise VMError(f"timed out after {timeout} s waiting for {pattern!r}; output so far:\n{tail}")
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
            self.monitor_buffer += self.monitor.recv(65536)
        line, _, self.monitor_buffer = self.monitor_buffer.partition(b"\n")
        return json.loads(line)

    def qmp(self, execute: str, **arguments) -> dict:
        request = {"execute": execute}
        if arguments:
            request["arguments"] = arguments
        self.monitor.sendall(json.dumps(request).encode("ascii") + b"\n")
        while True:
            answer = self._monitor_read()
            if "return" in answer:
                return answer["return"]
            if "error" in answer:
                raise VMError(f"QMP {execute}: {answer['error']}")

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
