#!/usr/bin/env python3
"""Boot the built image on several emulated machines and record what works on each.

    python tools/machines.py            every machine in the list; writes evidence/machines.json
    python tools/machines.py --only q35 one machine; writes nothing

Inti has never been started on a physical computer. This is the nearest thing that can be
measured here: the same image on QEMU machines that differ in the ways real PCs differ, in
processor features, in memory, and in the controller the disk hangs on. For every machine it
records whether the shell's prompt appeared, what `selftest` reported (and, for every check
that failed, its name and the lines it printed), what the disk server said about the disk, and whether a file written by the shell could be read back.

A machine in the list that does not start is a result and not an error: the table in
docs/hardware.md shows it as it was. The exit status is 0 if the machine the tests use
(the first one) reached the prompt, passed `selftest` and kept a file.
"""
from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import platform
import re
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))
from vm import VM, VMError  # noqa: E402  (vm.py also puts the project root on sys.path)
import build  # noqa: E402
from test import qemu_version  # noqa: E402

EVIDENCE = ROOT / "evidence"
IMAGE = build.BUILD / "inti.img"
PROMPT_WAIT_S = 40
SELFTEST_WAIT_S = 180
SELFTEST = re.compile(r"SELFTEST: (PASS|FAIL) (\d+)/(\d+)")
VERDICT_LINE = re.compile(r"^  (ok  |FAIL|skip) (.+)$")  # one line of `selftest` for one check
DETAIL_LINES = 60  # the most lines kept of what a failed check printed before its verdict
DISK = re.compile(r"^(?:disk|df): (.+)$", re.M)
STICK = f"if=none,id=stick,format=raw,file={IMAGE}"

# name, what differs from the first machine, extra QEMU arguments, memory in MiB, "-drive" value.
MACHINES = (
    ("tested", "the machine every test uses: i440FX board, 32-bit processor with the no-execute bit, IDE disk",
     [], 64, None),
    ("no-nx", "a processor without the no-execute bit (Pentium III)", ["-cpu", "pentium3"], 64, None),
    ("newest-cpu", "every processor feature QEMU can emulate, 64-bit ones included", ["-cpu", "max"], 64, None),
    ("no-pae", "a processor without PAE paging (486)", ["-cpu", "486"], 64, None),
    ("small-memory", "the least memory tried", [], 16, None),
    ("large-memory", "the most memory tried", [], 2048, None),
    ("two-processors", "two processors", ["-smp", "2"], 64, None),
    ("q35", "Q35 board: the disk hangs on an AHCI (SATA) controller", ["-machine", "q35"], 64, None),
    ("usb-stick", "the image on a USB stick, no other disk",
     ["-usb", "-device", "usb-storage,drive=stick,bootindex=0"], 64, STICK),
    ("whpx", "hardware virtualisation (Windows Hypervisor Platform) instead of emulation",
     ["-accel", "whpx"], 64, None),
)


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def tidy(text: str) -> str:
    """One line of an error, without the paths of this machine.

    A kernel panic is the line worth keeping; otherwise the last line is.
    """
    emulator = Path(build.find_qemu())
    for path, short in ((str(emulator), emulator.stem), (str(emulator.parent), "..."), (str(IMAGE), "..."),
                        (str(ROOT), "..."), (str(ROOT.parent), "..."), (str(Path.home()), "...")):
        text = text.replace(path, short).replace(path.replace("\\", "/"), short)
    lines = [line.strip() for line in text.splitlines() if line.strip()]
    for index, line in enumerate(lines):
        if "KERNEL PANIC" in line:  # the message is on the line after "panicked at FILE:LINE:"
            return " ".join(lines[index:index + 2]).replace("\\", "/")[:200]
    return (lines[-1] if lines else "")[:200]


def failed_checks(report: str) -> tuple[list[str], dict[str, list[str]]]:
    """The names of the checks that failed, and for each one what it printed before its verdict.

    `selftest` prints every expectation that did not hold, and the measurement it was about,
    above the line `  FAIL <name>`. The name alone does not say which expectation it was.
    """
    names: list[str] = []
    detail: dict[str, list[str]] = {}
    since_verdict: list[str] = []
    for line in report.splitlines():
        verdict = VERDICT_LINE.match(line)
        if not verdict:
            if line.strip():
                since_verdict.append(line.strip())
            continue
        if verdict.group(1) == "FAIL":
            name = verdict.group(2).strip()
            names.append(name)
            detail[name] = since_verdict[-DETAIL_LINES:]
        since_verdict = []
    return names, detail


def settle(machine: VM, seconds: float = 1.5) -> None:
    """Read what the machine still prints, so that a panic message is whole."""
    until = time.monotonic() + seconds
    while time.monotonic() < until:
        machine.pump(0.1)


def try_machine(name: str, what: str, extra: list[str], memory: int, drive: str | None) -> dict:
    result = {
        "name": name,
        "what": what,
        "arguments": " ".join(extra) or "-",
        "memory_mib": memory,
        "started": False,
        "prompt": False,
        "prompt_s": None,
        "selftest": None,
        "failed": [],
        "failed_detail": {},
        "disk": None,
        "kept": False,
        "note": "",
    }
    machine = VM(memory_mb=memory, extra=extra, drive=drive)
    try:
        machine.start()
    except VMError as error:
        # The harness's message holds times that differ from run to run; what QEMU wrote says why.
        written = str(error).rpartition("QEMU wrote: ")[2]
        result["note"] = "QEMU did not start; it wrote: " + (tidy(written) or "nothing")
        return result
    result["started"] = True
    try:
        began = time.monotonic()
        try:
            machine.wait_for_prompt(PROMPT_WAIT_S)
        except VMError:
            settle(machine)
            result["note"] = "no prompt; the serial port says: " + (tidy(machine.log()) or "(nothing)")
            return result
        result["prompt"] = True
        result["prompt_s"] = round(time.monotonic() - began, 1)
        try:
            space = machine.command("df", timeout=30)
            found = DISK.search(space)
            result["disk"] = found.group(1).strip() if found else tidy(space)
            machine.command("write machine.txt kept", timeout=30)
            result["kept"] = "\nkept" in machine.command("cat machine.txt", timeout=30).replace("\r", "")
            report = machine.command("selftest", timeout=SELFTEST_WAIT_S).replace("\r", "")
            verdict = SELFTEST.search(report)
            if verdict:
                result["selftest"] = f"{verdict.group(1)} {verdict.group(2)}/{verdict.group(3)}"
            # A count alone does not say what went wrong, and neither does a name: keep the
            # names of the failed checks and what each one printed.
            result["failed"], result["failed_detail"] = failed_checks(report)
            if result["failed"]:
                result["note"] = "selftest failed: " + "; ".join(result["failed"])
        except VMError as error:
            result["note"] = tidy(str(error))
    finally:
        machine.close()
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--only", choices=[machine[0] for machine in MACHINES], action="append",
                        help="try only this machine")
    options = parser.parse_args()

    began = time.monotonic()
    image_sha256 = sha256(IMAGE)
    results = []
    for name, what, extra, memory, drive in MACHINES:
        if options.only and name not in options.only:
            continue
        result = try_machine(name, what, extra, memory, drive)
        results.append(result)
        print(f"machines: {name:15} prompt={result['prompt']} selftest={result['selftest']} "
              f"disk={result['disk']!r} kept={result['kept']} {result['note']}", flush=True)
    unchanged = sha256(IMAGE) == image_sha256
    first = results[0]
    passed = unchanged and (bool(options.only) or (
        first["prompt"] and (first["selftest"] or "").startswith("PASS") and first["kept"]
    ))
    print(f"machines: {sum(result['prompt'] for result in results)} of {len(results)} reached the prompt, "
          f"{round(time.monotonic() - began)} s", flush=True)
    if options.only:
        print("machines: a partial run; evidence/machines.json is left as it was", flush=True)
        return 0 if passed else 1
    report = {
        "passed": passed,
        "date": datetime.datetime.now().astimezone().isoformat(timespec="seconds"),
        "duration_s": round(time.monotonic() - began),
        "emulator": f"QEMU {qemu_version()}",
        "host": f"{platform.system()} {platform.release()}",
        "image_sha256": image_sha256,
        "image_unchanged": unchanged,
        "machines": results,
    }
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    (EVIDENCE / "machines.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8", newline="\n")
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
