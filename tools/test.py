"""Boot Inti in QEMU and put it through every test, over the serial port.

    python tools/test.py                 the full run (about five minutes)
    python tools/test.py --fuzz 20000    a short run while developing

It writes

    evidence/test_serial.log        everything the serial port produced (carriage returns removed)
    evidence/test_nonx_serial.log   the same for a second, short boot on a processor model
                                    without the no-execute bit
    evidence/test.json              every check with its result, and every measured number
    docs/img/qemu-*.png             the emulated screen at four moments

and exits with 0 only if every check passed. Nothing here is timed with QEMU tracing on.
"""
from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import platform
import re
import statistics
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))
from vm import VM, VMError  # noqa: E402  (vm.py also puts the project root on sys.path)
import build  # noqa: E402

EVIDENCE = ROOT / "evidence"
IMAGES = ROOT / "docs" / "img"
SERVICES = ("console", "fs", "disk", "display")
# The supervisor counts a server that ends less than 2 s after it started towards its
# restart back-off. Faults that are timed are therefore injected into settled servers.
SETTLE_S = 2.6
BANNER = r"Inti (\S+) shell ready at t=(\d+) us"
# QEMU's plain 32-bit processor model has no no-execute bit. One short boot uses it, to
# record what the system says and does where W^X cannot be enforced.
CPU_WITHOUT_NX = "qemu32"
# The other runs use the VM's default of 64 MiB; the kernel manages at most 128 MiB.
SMALL_MEMORY_MB = 64
FULL_MEMORY_MB = 256
SWEEP = r"took (\d+) page frames in batches of (\d+); (\d+) of (\d+) were free"
# What the serial log must show, in this order, when fs crashes while the supervisor is
# waiting for a hung console to answer a ping.
DOUBLE_DONE = r"DOUBLE FAILURE: fs answering again after (\d+) us, console after (\d+) us"
DOUBLE_ORDER = (
    r"init: fs crashed: page fault",
    r"init: noticed while waiting for console to answer a ping",
    r"init: fs restarted after \d+ us",
    r"init: console did not answer for \d+ ms; ending it",
    r"init: console ended: killed",
    r"init: console restarted after \d+ us",
    DOUBLE_DONE,
)
# What the guided tour must print, in this order (the shell's own lines only).
TOUR_ORDER = (
    r"^TOUR 1/6: ",
    r"ROGUE: (\d+)/\1 attempts denied, 1 skipped",
    r"^TOUR 2/6: ",
    r"RECOVERY: console crash -> answering again after \d+ us",
    r"^TOUR 3/6: ",
    r"RECOVERY: display crash -> answering again after \d+ us",
    r"^TOUR 4/6: ",
    r"RECOVERY: fs hang -> answering again after \d+ us",
    r"^TOUR 5/6: ",
    DOUBLE_DONE,
    r"^TOUR 6/6: ",
    r"\b0 kernel anomalies",
    r"kernel invariants: all hold",
    r"^TOUR: done",
)
ATTEMPT = r"^\s*(\d+)\. (.+?)\s+(denied: .+|NOT DENIED.*|skipped)\s*$"


class Run:
    """The machine under test plus the checks and numbers collected so far."""

    def __init__(self, vm: VM):
        self.vm = vm
        self.checks: list[dict] = []
        self.measured: dict = {}

    def check(self, name: str, ok: bool, detail: str = "") -> bool:
        self.checks.append({"name": name, "ok": bool(ok), "detail": detail})
        mark = "ok  " if ok else "FAIL"
        print(f"test: {mark} {name}" + (f"  [{detail}]" if detail else ""), flush=True)
        return bool(ok)

    def log_since(self, mark: int) -> str:
        return bytes(self.vm.raw[mark:]).decode("utf-8", "replace").replace("\r", "")


def spread(values: list[float]) -> dict:
    return {
        "min": min(values),
        "median": round(statistics.median(values)),
        "max": max(values),
        "runs": len(values),
        "samples": values,
    }


def boot(run: Run) -> None:
    text = run.vm.wait_for_prompt(timeout=20)
    host_ms = round((time.monotonic() - run.vm.started_at) * 1000)
    found = re.search(BANNER, text)
    run.check("boots from its own boot sector to the shell prompt within 20 s", found is not None,
              f"{host_ms} ms after QEMU started")
    run.measured["boot"] = {
        "host_ms": host_ms,
        "guest_us": int(found[2]) if found else None,
        "version": found[1] if found else None,
    }
    run.check("the kernel enforces W^X: its boot log says the no-execute bit is in use",
              "W^X enforced" in run.vm.log(), f"QEMU -cpu {build.QEMU_CPU}")


def selftest(run: Run, label: str) -> None:
    text = run.vm.command("selftest", timeout=90)
    found = re.search(r"SELFTEST: PASS (\d+)/(\d+)", text)
    failures = re.findall(r"^\s*FAIL (.+)$", text, re.M)
    skipped = re.findall(r"^\s*skip (.+)$", text, re.M)
    passed = (found is not None and found[1] == found[2] and int(found[2]) >= 30
              and not failures and not skipped)
    run.check(f"selftest {label}", passed, found[0] if found else text[-300:])
    if found:
        key = label.replace(" ", "_")
        run.measured.setdefault("selftest", {})[key] = int(found[2])
        run.measured["selftest"][f"{key}_skipped"] = len(skipped)
    if label == "after boot":
        written = "written through the Log capability" in run.vm.log()
        run.check("the selftest's line written through the Log capability is in the kernel log", written)


def rogue(run: Run) -> None:
    text = run.vm.command("rogue all", timeout=90)
    found = re.search(r"ROGUE: (\d+)/(\d+) attempts denied\n", text)
    attempts = re.findall(ATTEMPT, text, re.M)
    denied = [attempt for attempt in attempts if attempt[2].startswith("denied")]
    complete = found is not None and found[1] == found[2] and len(denied) == len(attempts) == int(found[2])
    run.check("rogue all: every attempt of the hostile program is denied", complete,
              found[0].strip() if found else text[-300:])
    run.check("rogue all: the kernel invariants hold afterwards", "kernel invariants: all hold" in text)
    run.vm.screenshot(IMAGES / "qemu-rogue.png")
    total = int(found[2]) if found else 0

    text = run.vm.command("rogue", timeout=90)
    found = re.search(r"ROGUE: (\d+)/(\d+) attempts denied, (\d+) skipped", text)
    browser_safe = found is not None and found[1] == found[2] and int(found[2]) + int(found[3]) == total
    run.check("rogue: the default run denies every attempt it makes and reports the one it skips",
              browser_safe and int(found[3]) == 1, found[0] if found else text[-300:])
    run.measured["rogue"] = {
        "all_denied": len(denied),
        "all_total": total,
        "default_denied": int(found[1]) if found else None,
        "default_skipped": int(found[3]) if found else None,
        "attempts": [f"{number}. {what}: {result}" for number, what, result in attempts],
    }
    still_up = "readme.txt" in run.vm.command("ls")
    run.check("the system is still up after the hostile program (ls answers)", still_up)


def inject(run: Run, verb: str, target: str) -> dict:
    """Break one server on purpose; time how long the shell waits until it answers again."""
    vm = run.vm
    mark = len(vm.raw)
    started = time.monotonic()
    vm.send(f"{verb} {target}\r")
    text = vm.read_until(rf"RECOVERY: {target} {verb} -> answering again after \d+ us", timeout=20)
    host_ms = round((time.monotonic() - started) * 1000)
    vm.wait_for_prompt()
    listing = vm.command("ls")
    log = run.log_since(mark)
    respawn = re.search(rf"init: {target} restarted after (\d+) us", log)
    if verb == "crash":
        noticed = re.search(rf"init: {target} crashed: page fault", log)
    else:
        noticed = re.search(rf"init: {target} did not answer for \d+ ms; ending it", log)
    return {
        "guest_us": int(re.findall(r"answering again after (\d+) us", text)[-1]),
        "host_ms": host_ms,
        "respawn_us": int(respawn[1]) if respawn else None,
        "answers": "readme.txt" in listing,
        "noticed": noticed is not None,
        "slowed": "times in a row" in log,
    }


def summarise(run: Run, name: str, what: str, samples: list[dict]) -> None:
    good = all(
        sample["answers"] and sample["noticed"] and sample["respawn_us"] is not None and not sample["slowed"]
        for sample in samples
    )
    guest = spread([sample["guest_us"] for sample in samples])
    run.check(f"{what}: restarted by the supervisor, shell and file listing answer, in {len(samples)} of "
              f"{len(samples)} runs", good, f"median {guest['median']} us, worst {guest['max']} us")
    run.measured.setdefault("recovery", {})[name] = {
        "guest_us": guest,
        "host_ms": spread([sample["host_ms"] for sample in samples]),
        "respawn_us": spread([sample["respawn_us"] or 0 for sample in samples]),
    }


def recovery(run: Run, runs: int) -> None:
    for verb in ("crash", "hang"):
        for target in SERVICES:
            samples = []
            for index in range(runs):
                time.sleep(SETTLE_S)
                samples.append(inject(run, verb, target))
                if verb == "crash" and target == "console" and index == 0:
                    run.vm.command("svc")
                    run.vm.screenshot(IMAGES / "qemu-recovery.png")
            summarise(run, f"{target}_{verb}", f"{target} {verb}", samples)
    # The slowest case: a server that hangs right after it was restarted, before the
    # supervisor has started pinging it.
    samples = []
    for _ in range(min(runs, 3)):
        time.sleep(SETTLE_S)
        inject(run, "crash", "fs")
        samples.append(inject(run, "hang", "fs"))
    summarise(run, "fs_hang_just_restarted", "fs hang right after a restart", samples)


def in_order(patterns: tuple[str, ...], text: str) -> str | None:
    """The first pattern that is not found after the one before it, or None if all are."""
    position = 0
    for pattern in patterns:
        found = re.compile(pattern, re.M).search(text, position)
        if found is None:
            return pattern
        position = found.end()
    return None


def double_failure(run: Run, runs: int) -> None:
    """Two servers fail at once: fs crashes while the supervisor waits for a hung console."""
    vm = run.vm
    samples = []
    for _ in range(runs):
        time.sleep(SETTLE_S)
        mark = len(vm.raw)
        vm.send("double\r")
        text = vm.read_until(DOUBLE_DONE, timeout=20)
        vm.wait_for_prompt()
        listing = vm.command("ls")
        log = run.log_since(mark)
        times = re.findall(DOUBLE_DONE, text)[-1]
        samples.append({
            "fs_us": int(times[0]),
            "console_us": int(times[1]),
            "missing": in_order(DOUBLE_ORDER, log),
            "answers": "readme.txt" in listing,
            "slowed": "times in a row" in log,
        })
    good = all(sample["missing"] is None and sample["answers"] and not sample["slowed"] for sample in samples)
    fs_us = spread([sample["fs_us"] for sample in samples])
    console_us = spread([sample["console_us"] for sample in samples])
    detail = f"median fs {fs_us['median']} us, console {console_us['median']} us"
    missing = sorted({sample["missing"] for sample in samples if sample["missing"]})
    if missing:
        detail += "; not found in order: " + "; ".join(missing)
    run.check("double failure: fs crashes while the supervisor waits for a hung console; the supervisor "
              "reports the crash during that wait, restarts both, and the shell and file listing answer, "
              f"in {len(samples)} of {len(samples)} runs", good, detail)
    run.measured["double"] = {"fs_us": fs_us, "console_us": console_us}


def tour(run: Run) -> None:
    """The guided tour: six announced steps, ending in the shell that started it."""
    vm = run.vm
    time.sleep(SETTLE_S)
    mark = len(vm.raw)
    started = time.monotonic()
    vm.send("tour\r")
    text = vm.read_until(r"TOUR: done[^\n]*\n", timeout=90)
    host_ms = round((time.monotonic() - started) * 1000)
    vm.wait_for_prompt()
    listing = vm.command("ls")
    missing = in_order(TOUR_ORDER, text)
    steps = len(re.findall(r"^TOUR \d+/\d+: ", text, re.M))
    good = missing is None and "times in a row" not in run.log_since(mark) and "readme.txt" in listing
    run.check("tour: the guided tour announces and runs its steps in order (hostile program, crashed "
              "driver, crashed display, hung server, two failures at once, counters) and ends in the "
              "shell that started it",
              good, f"{steps} steps in {host_ms} ms" + (f"; not found in order: {missing}" if missing else ""))
    run.measured["tour"] = {"steps": steps, "host_ms": host_ms}

# ---------------------------------------------------------------------------- the screen

DISPLAY_SOURCE = ROOT / "user" / "src" / "bin" / "display.rs"
PROTO_SOURCE = ROOT / "user" / "src" / "proto.rs"
WINDOW_LINE = r"^\s*(\d+)\s+(.+?)\s+(-?\d+)\s+(-?\d+)\s+(\d+)\s+(\d+)\s*$"
WINDOWS_DONE = r"WINDOWS: (\d+) of programs, (\d+) frames drawn, display restarted (\d+) times, pointer at (-?\d+),(-?\d+)"


def screen_constants() -> dict:
    """The palette, the colour names and the frame sizes, read from the sources."""
    display = DISPLAY_SOURCE.read_text(encoding="utf-8")
    proto = PROTO_SOURCE.read_text(encoding="utf-8")
    table = display[display.index("const PALETTE"):]
    table = table[:table.index("\n];")]
    palette = [tuple(int(part, 16) for part in entry)
               for entry in re.findall(r"\[0x(\w\w), 0x(\w\w), 0x(\w\w)\]", table)]
    module = proto[proto.index("pub mod colour"):]
    module = module[:module.index("\n}")]
    names = {name: int(value) for name, value in re.findall(r"pub const (\w+): u8 = (\d+);", module)}
    sizes = {name: int(value) for name, value in re.findall(r"const (BORDER|TOP|COLUMNS|LINES): usize = (\d+);", display)}
    return {"colour": {name: palette[index] for name, index in names.items()}, **sizes}


def is_colour(pixel: tuple, colour: tuple) -> bool:
    """The card keeps six bits of each colour, so a pixel may be a little off."""
    return all(abs(got - wanted) <= 4 for got, wanted in zip(pixel[:3], colour))


def windows(vm: VM) -> dict:
    """What the `windows` command says: every window by title, and the pointer."""
    text = vm.command("windows")
    found = {}
    for number, title, x, y, width, height in re.findall(WINDOW_LINE, text, re.M):
        found[title] = {"number": int(number), "x": int(x), "y": int(y), "width": int(width), "height": int(height)}
    done = re.search(WINDOWS_DONE, text)
    return {
        "windows": found,
        "programs": int(done[1]) if done else None,
        "restarts": int(done[3]) if done else None,
        "pointer": (int(done[4]), int(done[5])) if done else None,
    }


def picture(vm: VM, name: str):
    from PIL import Image
    path = vm.screenshot(EVIDENCE / name)
    with Image.open(path) as image:
        return image.convert("RGB")


def text_matches(image, x: int, y: int, text: str, ink: tuple) -> bool:
    """Is `text` drawn at x, y in the system font: every pixel of every letter?"""
    import font
    glyphs = font.glyphs()
    for index, letter in enumerate(text):
        for row, bits in enumerate(glyphs[ord(letter)]):
            for column in range(font.WIDTH):
                lit = bits & (0x80 >> column) != 0
                if is_colour(image.getpixel((x + index * font.WIDTH + column, y + row)), ink) != lit:
                    return False
    return True


def terminal_lines(image, terminal: dict, sizes: dict) -> list[bytes]:
    """The picture of each text line of the terminal window."""
    import font
    left, top = terminal["x"] + sizes["BORDER"], terminal["y"] + sizes["TOP"]
    width = sizes["COLUMNS"] * font.WIDTH
    return [image.crop((left, top + line * font.HEIGHT, left + width, top + (line + 1) * font.HEIGHT)).tobytes()
            for line in range(sizes["LINES"])]


def lines_kept(before: list[bytes], after: list[bytes]) -> int:
    """How many lines with text stand in `after` exactly as in `before`, moved up or not."""
    blank = max(set(before), key=before.count)
    best = 0
    for moved in range(len(before)):
        kept = 0
        for line in range(len(before) - moved):
            if after[line] != before[line + moved]:
                break
            kept += before[line + moved] != blank
        best = max(best, kept)
    return best


def count_colour(image, colour: tuple, box: tuple | None = None) -> int:
    region = image.crop(box) if box else image
    return sum(1 for pixel in region.getdata() if is_colour(pixel, colour))


def monitor(vm: VM, line: str) -> None:
    vm.qmp("human-monitor-command", **{"command-line": line})


def graphics(run: Run) -> None:
    """The display server: what is on the screen, whose windows they are, and its restart."""
    vm = run.vm
    sizes = screen_constants()
    colour = sizes["colour"]
    border, top = sizes["BORDER"], sizes["TOP"]
    time.sleep(SETTLE_S)

    state = windows(vm)
    terminal = state["windows"].get("Terminal")
    time.sleep(0.5)
    image = picture(vm, "screen_desktop.png")
    good = (image.size == (640, 480) and terminal is not None
            and is_colour(image.getpixel((636, 476)), colour["NAVY"])
            and is_colour(image.getpixel((terminal["x"] + terminal["width"] - 8, terminal["y"] + 8)), colour["BLUE"])
            and text_matches(image, terminal["x"] + border + 4, terminal["y"] + 3, "Terminal", colour["WHITE"]))
    run.check("screen: 640 by 480 pixels in graphics mode, with the desktop and the terminal window whose "
              "title is drawn in the system font, pixel for pixel", good, f"image {image.size}")
    if terminal is None:
        return

    # A program with a window, and what it may not do.
    vm.send("wintest +window &\r")
    text = vm.read_until(r"WINTEST: (PASS|FAIL) \d+/\d+\s*\n", timeout=30)
    found = re.search(r"WINTEST: PASS (\d+)/(\d+)", text)
    failures = re.findall(r"^\s*FAIL (.+)$", text, re.M)
    run.check("windows: a program can draw in its own window and nowhere else (the wintest program's checks)",
              found is not None and found[1] == found[2] and not failures, found[0] if found else text[-300:])
    run.measured["wintest"] = {"passed": int(found[1]), "total": int(found[2])} if found else None
    vm.command("")
    time.sleep(0.5)
    state = windows(vm)
    window = state["windows"].get("Window test")
    time.sleep(0.5)
    image = picture(vm, "screen_window.png")
    if window:
        inside = (window["x"] + border, window["y"] + top,
                  window["x"] + window["width"] - border, window["y"] + window["height"] - border)
        area = (inside[2] - inside[0]) * (inside[3] - inside[1])
        within, anywhere = count_colour(image, colour["CORAL"], inside), count_colour(image, colour["CORAL"])
    else:
        area, within, anywhere = 0, -1, -1
    run.check("windows: a fill of 65535 by 65535 pixels coloured the inside of the program's own window "
              "and not one pixel outside it", window is not None and within == area == anywhere,
              f"{within} of {area} pixels inside, {anywhere} on the whole screen")

    # The mouse.
    before = state["pointer"]
    monitor(vm, "mouse_move 37 -23")
    time.sleep(0.5)
    after = windows(vm)["pointer"]
    time.sleep(0.5)
    image = picture(vm, "screen_pointer.png")
    drawn = (after is not None and is_colour(image.getpixel(after), colour["BLACK"])
             and is_colour(image.getpixel((after[0] + 1, after[1] + 3)), colour["WHITE"]))
    run.check("mouse: moving the emulated PS/2 mouse by 37, -23 moves the pointer on the screen by as much",
              before is not None and after == (before[0] + 37, before[1] - 23) and drawn, f"{before} -> {after}")
    if window and after:
        # Take the window by its title bar and carry it.
        monitor(vm, f"mouse_move {window['x'] + 60 - after[0]} {window['y'] + 8 - after[1]}")
        time.sleep(0.3)
        monitor(vm, "mouse_button 1")
        time.sleep(0.3)
        # A window stands on a multiple of eight pixels from the left edge.
        monitor(vm, "mouse_move -16 180")
        time.sleep(0.3)
        monitor(vm, "mouse_button 0")
        time.sleep(0.5)
        moved = windows(vm)["windows"].get("Window test")
        run.check("mouse: a window dragged by its title bar moves with the pointer",
                  moved is not None and (moved["x"], moved["y"]) == (window["x"] - 16, window["y"] + 180),
                  f"{(window['x'], window['y'])} -> {(moved['x'], moved['y']) if moved else None}")

    # Programs in the background, each with a window, while the shell answers.
    vm.command("desktop")
    time.sleep(1.5)
    state = windows(vm)
    clock_window = state["windows"].get("Clock")
    jobs = vm.command("jobs")
    changed = False
    if clock_window:
        box = (clock_window["x"], clock_window["y"], clock_window["x"] + clock_window["width"],
               clock_window["y"] + clock_window["height"])
        first = picture(vm, "screen_jobs.png").crop(box).tobytes()
        time.sleep(2.2)
        changed = picture(vm, "screen_jobs.png").crop(box).tobytes() != first
    run.check("background: clock and life run with a window each while the shell answers, and the clock's "
              "window changes within 2.2 s", state["programs"] == 3 and "Life" in state["windows"]
              and all(name in jobs for name in ("clock", "life", "wintest")) and changed,
              f"{state['programs']} windows of programs")
    vm.command("ls")
    time.sleep(0.5)
    vm.screenshot(IMAGES / "qemu-desktop.png")

    # The display server crashes: the text comes back, the programs open their windows again.
    time.sleep(0.5)
    lines_before = terminal_lines(picture(vm, "screen_before_crash.png"), terminal, sizes)
    mark = len(vm.raw)
    vm.send("crash display\r")
    vm.read_until(r"RECOVERY: display crash -> answering again after \d+ us", timeout=20)
    vm.wait_for_prompt()
    refused = True
    try:
        if "the old window was refused" not in run.log_since(mark):
            vm.read_until(r"WINTEST: the old window was refused after a display restart", timeout=10)
    except VMError:
        refused = False
    time.sleep(2.0)
    image = picture(vm, "screen_after_crash.png")
    state = windows(vm)
    kept = lines_kept(lines_before, terminal_lines(image, terminal, sizes))
    with_text = sum(1 for line in lines_before if line != max(set(lines_before), key=lines_before.count))
    run.check("display crash: the lines that stood in the terminal window before are on the screen again, "
              "pixel for pixel", kept >= max(with_text - 6, 5), f"{kept} of {with_text} lines with text")
    run.check("display crash: a window capability from before the restart is refused, and every program "
              "opens a new window", refused and state["programs"] == 3 and state["restarts"] is not None
              and state["restarts"] >= 1 and {"Clock", "Life", "Window test"} <= set(state["windows"]),
              f"{state['programs']} windows of programs, display restarted {state['restarts']} times")

    # A program that ends takes its windows with it.
    for name in ("wintest", "life", "clock"):
        vm.command(f"stop {name}")
    gone_after = None
    started = time.monotonic()
    while time.monotonic() - started < 8:
        time.sleep(0.5)
        if windows(vm)["programs"] == 0:
            gone_after = round(time.monotonic() - started, 1)
            break
    time.sleep(0.5)
    image = picture(vm, "screen_closed.png")
    coral = count_colour(image, colour["CORAL"])
    run.check("windows: when a program is stopped its windows are closed, with nothing of them left on "
              "the screen", gone_after is not None and coral == 0 and "kernel invariants: all hold" in vm.command("uptime"),
              f"gone after {gone_after} s; {coral} pixels left")
    run.measured["windows_closed_after_s"] = gone_after
    for name in ("desktop", "window", "pointer", "jobs", "before_crash", "after_crash", "closed"):
        (EVIDENCE / f"screen_{name}.png").unlink(missing_ok=True)


def backoff(run: Run) -> None:
    """A server that keeps dying is restarted more slowly, and the system stays usable."""
    vm = run.vm
    time.sleep(SETTLE_S)
    mark = len(vm.raw)
    for _ in range(5):
        vm.command("crash fs", timeout=20)
    log = run.log_since(mark)
    slowed = re.findall(r"init: fs ended (\d+) times in a row; next restart in (\d+) ms", log)
    listing = vm.command("ls")
    services = vm.command("svc")
    run.check("a server that dies again and again is restarted with a growing delay and still comes back",
              len(slowed) >= 2 and "readme.txt" in listing,
              "; ".join(f"ended {count} times in a row -> {delay} ms" for count, delay in slowed))
    run.measured["backoff"] = {
        "delays_ms": [int(delay) for _, delay in slowed],
        "svc": services.strip().splitlines(),
    }
    time.sleep(SETTLE_S)


def supervisor_crash(run: Run) -> None:
    vm = run.vm
    mark = len(vm.raw)
    started = time.monotonic()
    vm.send("crash init\r")
    text = vm.read_until(BANNER, timeout=20)
    host_ms = round((time.monotonic() - started) * 1000)
    vm.wait_for_prompt()
    restarted = "[user space restarted]" in run.log_since(mark)
    listing = vm.command("ls")
    run.check("crash init: the kernel starts user space again and the new shell answers",
              restarted and "readme.txt" in listing, f"new prompt {host_ms} ms later (host time)")
    run.measured["init_crash"] = {"host_ms": host_ms, "banner": re.search(BANNER, text)[0]}
    time.sleep(SETTLE_S)


def fuzz(run: Run, count: int) -> None:
    vm = run.vm
    mark = len(vm.raw)
    started = time.monotonic()
    text = vm.command(f"fuzz {count}", timeout=max(120.0, count / 2000))
    host_s = time.monotonic() - started
    banner = re.search(r"Fuzzing the kernel with (\d+) random system calls, seed (\d+)\.", run.log_since(mark))
    calls = re.search(r"FUZZ: (\d+) random system calls from (\d+) tasks in (\d+) ms", text)
    health = re.search(r"FUZZ: (\d+) invariant checks, (\d+) violations, (\d+) kernel anomalies, (\d+) leaks", text)
    ended = re.search(r"FUZZ: tasks ended: (\d+) exited, (\d+) were stopped, (\d+) hit a CPU exception", text)
    parsed = banner is not None and calls is not None and health is not None and ended is not None
    run.check("fuzz: the report is complete", parsed, text[-300:] if not parsed else "")
    if not parsed:
        return
    clean = all(int(health[index]) == 0 for index in (2, 3, 4)) and "FUZZ: PASS" in text
    run.check(f"fuzz: at least {count} random system calls, 0 invariant violations, 0 kernel anomalies, 0 leaks",
              clean and int(calls[1]) >= count, f"{calls[0]}; {health[0]}")
    table = text[text.find("Calls the tasks reported"):text.find("FUZZ: PASS")].strip().splitlines()
    run.measured["fuzz"] = {
        "requested": count,
        "seed": int(banner[2]),
        "calls": int(calls[1]),
        "tasks": int(calls[2]),
        "guest_ms": int(calls[3]),
        "host_ms": round(host_s * 1000),
        "calls_per_second": round(int(calls[1]) * 1000 / max(1, int(calls[3]))),
        "checks": int(health[1]),
        "violations": int(health[2]),
        "anomalies": int(health[3]),
        "leaks": int(health[4]),
        "exited": int(ended[1]),
        "stopped": int(ended[2]),
        "exceptions": int(ended[3]),
        "per_call": table,
    }
    vm.screenshot(IMAGES / "qemu-fuzz.png")


def bench(run: Run) -> None:
    text = run.vm.command("bench", timeout=180)
    results = {name: int(value) for name, value in re.findall(r"BENCH: (.+?): (\d+) ns", text)}
    run.check("bench runs to the end", "BENCH: done" in text and len(results) > 0, f"{len(results)} results")
    run.measured["bench_ns"] = results


def programs_from_disk(run: Run) -> None:
    """A program file copied to the disk runs by its name, and with nothing but an ordinary program's share."""
    run.vm.command("cp hello hi", timeout=30)
    text = run.vm.command("hi", timeout=30)
    run.check("a program copied to the disk runs from there", "Hello from user space." in text, text[-200:])
    run.vm.command("cp selftest st2", timeout=30)
    text = run.vm.command("st2", timeout=60)
    run.check("a copy of the self-test on the disk gets none of its extra capabilities",
              "SELFTEST: FAIL" in text and "SELFTEST: PASS" not in text, text[-200:])
    run.vm.command("rm hi", timeout=30)
    run.vm.command("rm st2", timeout=30)


def keyboard(run: Run) -> None:
    run.vm.press("u", "p", "t", "i", "m", "e", "ret")
    text = run.vm.wait_for_prompt(timeout=15)
    run.check("keys pressed on the emulated PS/2 keyboard reach the shell", "kernel invariants: all hold" in text)


def uptime(vm: VM) -> tuple[float, float, str]:
    text = vm.command("uptime")
    return float(re.search(r"up (\d+\.\d+) s", text)[1]), time.monotonic(), text


def clock(run: Run) -> None:
    guest_before, host_before, _ = uptime(run.vm)
    time.sleep(5)
    guest_after, host_after, _ = uptime(run.vm)
    ratio = (guest_after - guest_before) / (host_after - host_before)
    run.check("the guest clock keeps time against the host clock over 5 idle seconds", 0.95 <= ratio <= 1.02,
              f"guest/host = {ratio:.4f}")
    run.measured["clock"] = {"idle_ratio": round(ratio, 4)}
    fuzzed = run.measured.get("fuzz")
    if fuzzed:
        run.measured["clock"]["fuzz_guest_over_host"] = round(fuzzed["guest_ms"] / fuzzed["host_ms"], 4)


def counters(run: Run) -> None:
    _, _, text = uptime(run.vm)
    numbers = {
        key: int(value)
        for value, key in re.findall(
            r"(\d+) (timer interrupts|tasks started|tasks|endpoints|system calls|task switches|"
            r"device interrupts|faults contained|kernel anomalies)", text)
    }
    healthy = numbers.get("kernel anomalies") == 0 and "kernel invariants: all hold" in text
    run.check("at the end: 0 kernel anomalies and every kernel invariant holds", healthy,
              f"{numbers.get('system calls')} system calls, {numbers.get('faults contained')} faults contained, "
              f"{numbers.get('tasks started')} tasks started")
    run.measured["counters"] = {key.replace(" ", "_"): value for key, value in numbers.items()}


def shutdown(run: Run) -> None:
    vm = run.vm
    vm.send("shutdown\r")
    deadline = time.monotonic() + 15
    while vm.process.poll() is None and time.monotonic() < deadline:
        vm.pump()
    vm.pump(0.2)
    code = vm.process.poll()
    run.check("shutdown switches the machine off (QEMU exits with code 0)", code == 0, f"exit code {code}")


def without_nx(run: Run) -> str:
    """Boot the same image on a processor model without the no-execute bit; return its serial log.

    W^X cannot be enforced there. The kernel has to say so, the selftest has to report its
    execute tests as skipped and not as passed, and the hostile program has to report its
    execute attempts as not denied.
    """
    where = "without the no-execute bit"
    vm = VM(extra=["-cpu", CPU_WITHOUT_NX])
    vm.start()
    try:
        vm.wait_for_prompt(timeout=20)
        run.check(f"{where}: the kernel boots and says that W^X is not enforced",
                  "W^X NOT enforced" in vm.log(), f"QEMU -cpu {CPU_WITHOUT_NX}")

        text = vm.command("selftest", timeout=90)
        found = re.search(r"SELFTEST: PASS (\d+)/(\d+), (\d+) skipped", text)
        skipped = re.findall(r"^\s*skip (.+)$", text, re.M)
        failures = re.findall(r"^\s*FAIL (.+)$", text, re.M)
        honest = (found is not None and found[1] == found[2] and int(found[3]) == len(skipped) > 0
                  and all("cannot run code from" in name for name in skipped) and not failures)
        run.check(f"{where}: the selftest passes and reports each execute test as skipped, not as passed",
                  honest, found[0] if found else text[-300:])

        text = vm.command("rogue all", timeout=90)
        total = re.search(r"ROGUE: (\d+)/(\d+) attempts denied\n", text)
        attempts = re.findall(ATTEMPT, text, re.M)
        denied = [attempt for attempt in attempts if attempt[2].startswith("denied")]
        not_denied = [attempt for attempt in attempts if not attempt[2].startswith("denied")]
        plain = (total is not None and len(attempts) == int(total[2]) and len(denied) == int(total[1])
                 and len(not_denied) > 0
                 and all(what.startswith("run code from") and result == "NOT DENIED: no no-execute bit"
                         for _, what, result in not_denied))
        run.check(f"{where}: the hostile program reports each execute attempt as not denied and every "
                  "other attempt as denied", plain, total[0].strip() if total else text[-300:])
        run.check(f"{where}: the kernel invariants hold after the hostile program",
                  "kernel invariants: all hold" in text)
        run.check(f"{where}: the system is still up (ls answers)", "readme.txt" in vm.command("ls"))
        run.measured["no_nx"] = {
            "selftest_passed": int(found[2]) if found else None,
            "selftest_skipped": len(skipped),
            "rogue_denied": len(denied),
            "rogue_total": len(attempts),
            "rogue_not_denied": len(not_denied),
            "not_denied": [f"{number}. {what}: {result}" for number, what, result in not_denied],
        }
    except VMError as error:
        run.check(f"{where}: the run reached its end", False, str(error)[-600:])
    finally:
        vm.close()
    log = vm.log().replace("\r", "")
    run.check(f"{where}: no kernel panic anywhere in the serial log", "KERNEL PANIC" not in log)
    return log


def full_memory(run: Run) -> str:
    """Boot the same image with more memory than the kernel manages; return its serial log.

    The other runs give the machine 64 MiB, half of what the kernel can manage, so they never
    touch its highest page frames or the page tables that map them. Here the selftest's sweep
    takes every free frame up to the last one.
    """
    where = f"with {FULL_MEMORY_MB} MiB"
    vm = VM(memory_mb=FULL_MEMORY_MB)
    vm.start()
    try:
        vm.wait_for_prompt(timeout=20)
        text = vm.command("selftest", timeout=180)
        found = re.search(r"SELFTEST: PASS (\d+)/(\d+)", text)
        failures = re.findall(r"^\s*FAIL (.+)$", text, re.M)
        skipped = re.findall(r"^\s*skip (.+)$", text, re.M)
        passed = found is not None and found[1] == found[2] and not failures and not skipped
        run.check(f"{where}: the kernel boots and the selftest passes", passed,
                  found[0] if found else text[-300:])
        sweep = re.search(SWEEP, text)
        taken, batch, free, total = (int(number) for number in sweep.groups()) if sweep else (0, 0, 0, 0)
        run.check(f"{where}: more page frames are free than a {SMALL_MEMORY_MB} MiB machine has, and the "
                  "selftest took every one of them",
                  sweep is not None and free > SMALL_MEMORY_MB * 256 and taken > free,
                  sweep[0].strip() if sweep else text[-300:])
        run.check(f"{where}: the kernel invariants hold afterwards",
                  "kernel invariants: all hold" in vm.command("uptime"))
        run.measured["full_memory"] = {
            "memory_mb": FULL_MEMORY_MB,
            "selftest_passed": int(found[2]) if found else None,
            "frames_taken": taken,
            "batch": batch,
            "free_frames": free,
            "total_frames": total,
        }
    except VMError as error:
        run.check(f"{where}: the run reached its end", False, str(error)[-600:])
    finally:
        vm.close()
    log = vm.log().replace("\r", "")
    run.check(f"{where}: no kernel panic anywhere in the serial log", "KERNEL PANIC" not in log)
    return log


def qemu_version() -> str:
    answer = subprocess.run([build.find_qemu(), "--version"], capture_output=True, text=True, check=False)
    found = re.search(r"version (\S+)", answer.stdout)
    return found[1] if found else "unknown"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--fuzz", type=int, default=1_000_000, help="random system calls to ask for")
    parser.add_argument("--runs", type=int, default=5, help="times each fault is injected")
    options = parser.parse_args()

    began = time.monotonic()
    IMAGES.mkdir(parents=True, exist_ok=True)
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    image_sha256 = hashlib.sha256((build.BUILD / "inti.img").read_bytes()).hexdigest()
    vm = VM()
    run = Run(vm)
    vm.start()
    try:
        boot(run)
        selftest(run, "after boot")
        rogue(run)
        recovery(run, options.runs)
        double_failure(run, options.runs)
        tour(run)
        graphics(run)
        backoff(run)
        supervisor_crash(run)
        fuzz(run, options.fuzz)
        bench(run)
        programs_from_disk(run)
        selftest(run, "after all faults")
        keyboard(run)
        clock(run)
        counters(run)
        shutdown(run)
    except VMError as error:
        run.check("the run reached its end", False, str(error)[-600:])
    finally:
        vm.close()
    log = vm.log().replace("\r", "")
    run.check("no kernel panic anywhere in the serial log", "KERNEL PANIC" not in log)
    log_without_nx = without_nx(run)
    log_full_memory = full_memory(run)

    passed = all(check["ok"] for check in run.checks)
    (EVIDENCE / "test_serial.log").write_text(log, encoding="utf-8", newline="\n")
    (EVIDENCE / "test_nonx_serial.log").write_text(log_without_nx, encoding="utf-8", newline="\n")
    (EVIDENCE / "test_fullmem_serial.log").write_text(log_full_memory, encoding="utf-8", newline="\n")
    report = {
        "passed": passed,
        "date": datetime.datetime.now().astimezone().isoformat(timespec="seconds"),
        "duration_s": round(time.monotonic() - began),
        "emulator": f"QEMU {qemu_version()} (TCG, no hardware virtualisation)",
        "qemu_version": qemu_version(),
        "cpu": build.QEMU_CPU,
        "cpu_without_nx": CPU_WITHOUT_NX,
        "host": f"{platform.system()} {platform.release()}",
        "image_sha256": image_sha256,
        "options": {"fuzz": options.fuzz, "runs": options.runs},
        "checks": run.checks,
        "measured": run.measured,
    }
    (EVIDENCE / "test.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8", newline="\n")
    failed = [check["name"] for check in run.checks if not check["ok"]]
    print(f"test: {'PASS' if passed else 'FAIL'} {len(run.checks) - len(failed)}/{len(run.checks)} checks, "
          f"{report['duration_s']} s", flush=True)
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
