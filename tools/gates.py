"""Check the nine hard gates of rubric.md.

    python build.py gates                  clean build, full QEMU test, browser check, claims check
    python build.py gates --quick          static checks against the evidence already on disk
    python build.py gates --quick --save   the same, and write the result to evidence/gates.json

Only the full run can pass: a quick run does not rebuild, so gate G1 stays unchecked.
The QEMU test and the browser check get up to three tries each, because their timing
checks can fail on a busy host. Every try and its outcome is recorded.
"""
from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import platform
import re
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
import build  # noqa: E402

EVIDENCE = ROOT / "evidence"
IMAGE = build.BUILD / "inti.img"
RUBRIC = "inti-os-v1"
GATE_TRIES = 3
KERNEL_LINE_LIMIT = 4000
MIN_SELFTESTS = 30
MIN_FUZZ_CALLS = 1_000_000
BOOT_LIMIT_MS = 20_000
SAFETY_REACH = 8  # a SAFETY comment counts if it is on the line or at most this many lines above
KERNEL_SOURCES = ("kernel/src", "abi/src")
ALL_SOURCES = ("kernel/src", "abi/src", "boot/src", "user/src")
SUPERVISED_FAULTS = ("console_crash", "fs_crash", "console_hang", "fs_hang")
MULTIBOOT_MAGICS = (bytes.fromhex("02b0ad1b"), bytes.fromhex("d65052e8"))

TITLES = {
    "G1": "One command builds the disk image from a clean tree",
    "G2": "Boots from its own boot sector to a shell prompt in QEMU within 20 s",
    "G3": "The self-test inside the system passes with at least 30 checks",
    "G4": "Each supervised service is crashed and hung, restarted automatically, and timed",
    "G5": "Every attempt of the hostile program without capabilities is denied",
    "G6": "At least 1,000,000 random system calls: no kernel panic, no invariant violated",
    "G7": "No heap in the kernel, every unsafe justified, at most 4,000 lines of kernel code",
    "G8": "The same disk image boots to the shell prompt in a web browser",
    "G9": "Every number in the documents comes from a script or carries a source",
}

# Gate G5 names seven kinds of attempt. Each must appear among the hostile program's attempts.
ROGUE_KINDS = {
    "port I/O": ("port",),
    "interrupts": ("interrupt",),
    "physical memory": ("map physical memory",),
    "kernel memory": ("kernel memory",),
    "other tasks": ("kill another task", "other programs' console requests"),
    "forged capabilities": ("capability number", "receive right", "re-badge", "capability it does not hold"),
    "forged tokens": ("forged tokens",),
}

Result = tuple[bool, str]


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def load(name: str) -> dict:
    path = EVIDENCE / name
    return json.loads(path.read_text(encoding="utf-8")) if path.exists() else {}


def run(script: str, *arguments: str) -> tuple[int, float]:
    """Run one of the project's scripts with its output passed through; return exit code and seconds."""
    began = time.monotonic()
    done = subprocess.run([sys.executable, str(ROOT / script), *arguments], cwd=ROOT, check=False)
    return done.returncode, time.monotonic() - began


def attempt(script: str) -> list[int]:
    """Run a test script until it passes, at most GATE_TRIES times; return every exit code."""
    codes = []
    while len(codes) < GATE_TRIES:
        code, _ = run(script)
        codes.append(code)
        if code == 0:
            break
    return codes


# ---- reading the source ---------------------------------------------------------------------

def rust_files(folders: tuple[str, ...]) -> list[Path]:
    return [path for folder in folders for path in sorted((ROOT / folder).rglob("*.rs"))]


def code_only(line: str) -> str:
    """The line with string literals emptied and its line comment removed."""
    return re.sub(r'"(?:[^"\\]|\\.)*"', '""', line).split("//")[0]


def count_code_lines(path: Path) -> int:
    """Lines that are neither blank nor only a comment."""
    count = 0
    in_block_comment = False
    for line in path.read_text(encoding="utf-8").splitlines():
        text = line.strip()
        if in_block_comment or text.startswith("/*"):
            in_block_comment = "*/" not in text
        elif text and not text.startswith("//"):
            count += 1
    return count


def code_lines(folders: tuple[str, ...]) -> int:
    return sum(count_code_lines(path) for path in rust_files(folders))


def unsafe_report(folders: tuple[str, ...]) -> tuple[int, list[str]]:
    """Count the uses of `unsafe` in code; list those with no SAFETY comment on or just above them."""
    uses = 0
    unexplained = []
    for path in rust_files(folders):
        lines = path.read_text(encoding="utf-8").splitlines()
        for index, line in enumerate(lines):
            if not re.search(r"\bunsafe\b", code_only(line)):
                continue
            uses += 1
            nearby = lines[max(0, index - SAFETY_REACH):index + 1]
            if not any("SAFETY:" in text or "# Safety" in text for text in nearby):
                unexplained.append(f"{path.relative_to(ROOT).as_posix()}:{index + 1}")
    return uses, unexplained


def uses_heap(folders: tuple[str, ...]) -> list[str]:
    """Kernel files that name the alloc crate or a global allocator."""
    found = []
    for path in rust_files(folders):
        code = "\n".join(code_only(line) for line in path.read_text(encoding="utf-8").splitlines())
        if re.search(r"\balloc::|extern\s+crate\s+alloc\b|global_allocator", code):
            found.append(path.relative_to(ROOT).as_posix())
    return found


# ---- the gates ------------------------------------------------------------------------------

def passed(test: dict, start: str) -> bool:
    """True if the test run has a check whose name starts with `start`, and that check passed."""
    return any(check["name"].startswith(start) and check["ok"] for check in test.get("checks", []))


def gate_build() -> tuple[Result, int]:
    run("build.py", "clean")
    code, seconds = run("build.py")
    if code != 0 or not IMAGE.exists():
        return (False, f"python build.py exited with code {code}"), round(seconds)
    detail = f"clean build in {seconds:.0f} s wrote build/inti.img ({IMAGE.stat().st_size:,} bytes)"
    return (True, detail), round(seconds)


def gate_boot(test: dict, sizes: dict) -> Result:
    boot = test["measured"].get("boot", {})
    loader = IMAGE.read_bytes()[:512 + sizes.get("kernel_bytes", 0)]
    problems = []
    if not passed(test, "boots from its own boot sector"):
        problems.append("the boot check failed")
    if boot.get("host_ms", BOOT_LIMIT_MS) >= BOOT_LIMIT_MS:
        problems.append("no shell prompt within 20 s")
    if loader[510:512] != b"\x55\xaa":
        problems.append("the first sector has no boot signature")
    if b"grub" in loader.lower() or any(magic in loader for magic in MULTIBOOT_MAGICS):
        problems.append("the boot sector or kernel mentions GRUB or carries a multiboot header")
    if problems:
        return False, "; ".join(problems)
    return True, (f"shell prompt {boot['host_ms']} ms after QEMU started; the image begins with Inti's own "
                  "boot sector (signature 55 AA); no multiboot header and no GRUB in boot sector or kernel")


def gate_selftest(test: dict) -> Result:
    selftest = test["measured"].get("selftest", {})
    after_boot = selftest.get("after_boot", 0)
    after_faults = selftest.get("after_all_faults", 0)
    none_skipped = selftest.get("after_boot_skipped") == 0 and selftest.get("after_all_faults_skipped") == 0
    ok = (after_boot >= MIN_SELFTESTS and after_faults == after_boot and none_skipped
          and passed(test, "selftest after boot") and passed(test, "selftest after all faults")
          and passed(test, "the kernel enforces W^X"))
    return ok, (f"SELFTEST: PASS {after_boot}/{after_boot} after boot and {after_faults}/{after_faults} "
                "again after every injected fault and the fuzz run, none skipped, with W^X enforced "
                f"(QEMU -cpu {test.get('cpu')})")


def gate_recovery(test: dict) -> Result:
    recovery = test["measured"].get("recovery", {})
    runs = test.get("options", {}).get("runs", 0)
    ok = runs > 0
    parts = []
    for fault in SUPERVISED_FAULTS:
        label = fault.replace("_", " ")
        times = recovery.get(fault, {}).get("guest_us")
        if not times or times["runs"] != runs or not passed(test, f"{label}: restarted by the supervisor"):
            ok = False
            parts.append(f"{label} FAILED")
        else:
            parts.append(f"{label} {times['median'] / 1000:.1f} ms")
    double = test["measured"].get("double")
    if not double or double["fs_us"]["runs"] != runs or not passed(test, "double failure:"):
        ok = False
        parts.append("double failure FAILED")
    else:
        parts.append(f"fs crash during a console hang: fs {double['fs_us']['median'] / 1000:.1f} ms and "
                     f"console {double['console_us']['median'] / 1000:.1f} ms")
    return ok, f"{', '.join(parts)} (median of {runs} runs each, guest clock); the shell answered after every run"


def gate_rogue(test: dict) -> Result:
    rogue = test["measured"].get("rogue", {})
    attempts = rogue.get("attempts", [])
    denied = [line for line in attempts if "denied: " in line]
    problems = []
    if not attempts or len(denied) != len(attempts) or rogue.get("all_denied") != rogue.get("all_total"):
        problems.append("not every attempt was denied")
    kinds = []
    for kind, words in ROGUE_KINDS.items():
        found = [line for line in attempts if any(word in line for word in words)]
        if not found or any(line not in denied for line in found):
            problems.append(f"no denied attempt of kind {kind}")
        kinds.append(f"{kind} {len(found)}")
    for check in ("rogue all: every attempt", "rogue all: the kernel invariants hold",
                  "the system is still up after the hostile program"):
        if not passed(test, check):
            problems.append(f"check failed: {check}")
    if problems:
        return False, "; ".join(problems)
    return True, (f"{len(denied)}/{len(attempts)} attempts denied (including {', '.join(kinds)}); "
                  "the kernel invariants hold and the shell still answers")


def gate_fuzz(test: dict) -> Result:
    fuzz = test["measured"].get("fuzz", {})
    log_path = EVIDENCE / "test_serial.log"
    log = log_path.read_text(encoding="utf-8") if log_path.exists() else "KERNEL PANIC: no serial log"
    ok = (fuzz.get("calls", 0) >= MIN_FUZZ_CALLS and fuzz.get("violations") == 0 and fuzz.get("anomalies") == 0
          and "KERNEL PANIC" not in log and passed(test, "no kernel panic"))
    return ok, (f"{fuzz.get('calls', 0):,} random system calls from {fuzz.get('tasks', 0):,} tasks, "
                f"{fuzz.get('checks', 0):,} invariant checks, {fuzz.get('violations')} violations, "
                f"{fuzz.get('anomalies')} kernel anomalies, no kernel panic in the serial log")


def gate_discipline() -> Result:
    heap = uses_heap(KERNEL_SOURCES)
    uses, unexplained = unsafe_report(ALL_SOURCES)
    lines = code_lines(KERNEL_SOURCES)
    problems = []
    if heap:
        problems.append(f"the alloc crate is used in {', '.join(heap)}")
    if unexplained:
        problems.append(f"unsafe without a SAFETY comment at {', '.join(unexplained[:6])}")
    if lines > KERNEL_LINE_LIMIT:
        problems.append(f"{lines:,} lines of kernel code, over the limit of {KERNEL_LINE_LIMIT:,}")
    if problems:
        return False, "; ".join(problems)
    return True, (f"no alloc crate in the kernel; {uses} uses of unsafe in kernel, boot sector and programs, "
                  f"each with a SAFETY comment; {lines:,} lines of kernel code (limit {KERNEL_LINE_LIMIT:,})")


def gate_browser(web: dict, image_hash: str) -> Result:
    ok = bool(web.get("ok")) and bool(web.get("same_image_as_build")) and web.get("image_sha256") == image_hash
    if not ok:
        return False, "evidence/web.json does not show the current image booting: " + "; ".join(
            str(failure) for failure in web.get("failures", [])[:3])
    commands = web.get("commands", {})
    return True, (f"shell prompt after {web['boot_seconds']} s in {web['emulator']} under {web['browser']}; "
                  f"{len(commands)} commands answered; byte-for-byte the image of the QEMU test")


def evaluate(first: Result) -> dict[str, Result]:
    """Judge G1 to G8 from the build on disk and the evidence files."""
    results = {"G1": first}
    image_hash = sha256(IMAGE) if IMAGE.exists() else ""
    sizes_path = build.BUILD / "sizes.json"
    sizes = json.loads(sizes_path.read_text(encoding="utf-8")) if sizes_path.exists() else {}
    test = load("test.json")
    if not image_hash or test.get("image_sha256") != image_hash:
        stale = (False, "evidence/test.json was not measured on the current build/inti.img")
        results.update({gate: stale for gate in ("G2", "G3", "G4", "G5", "G6")})
    else:
        results["G2"] = gate_boot(test, sizes)
        results["G3"] = gate_selftest(test)
        results["G4"] = gate_recovery(test)
        results["G5"] = gate_rogue(test)
        results["G6"] = gate_fuzz(test)
    results["G7"] = gate_discipline()
    results["G8"] = gate_browser(load("web.json"), image_hash)
    return results


def report(results: dict[str, Result], mode: str, extra: dict) -> dict:
    gates = [{"id": gate, "title": TITLES[gate], "ok": ok, "detail": detail} for gate, (ok, detail) in results.items()]
    sizes_path = build.BUILD / "sizes.json"
    rustc = subprocess.run(["rustc", "--version"], capture_output=True, text=True, check=False).stdout.strip()
    return {
        "rubric": RUBRIC,
        "mode": mode,
        "date": datetime.datetime.now().astimezone().isoformat(timespec="seconds"),
        "image_sha256": sha256(IMAGE) if IMAGE.exists() else "",
        "sizes": json.loads(sizes_path.read_text(encoding="utf-8")) if sizes_path.exists() else {},
        "toolchain": {"rustc": rustc, "python": platform.python_version()},
        **extra,
        "gates": gates,
        "passed": sum(1 for gate in gates if gate["ok"]),
        "total": len(gates),
    }


def save(document: dict) -> None:
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    (EVIDENCE / "gates.json").write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8", newline="\n")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--quick", action="store_true", help="no rebuild and no new test run")
    parser.add_argument("--save", action="store_true", help="with --quick: still write evidence/gates.json")
    options = parser.parse_args()

    extra: dict = {}
    if options.quick:
        first = (False, "not checked: a quick run does not rebuild from a clean tree")
    else:
        first, extra["build_seconds"] = gate_build()
        if first[0]:
            extra["tries"] = {"qemu_test": attempt("tools/test.py"), "browser_check": attempt("tools/web_check.py")}
    results = evaluate(first)
    mode = "quick" if options.quick else "full"

    # G9 checks the documents, and the documents show the gates. So: assume G9 passes, render,
    # check; if the check fails, record that and render again so the documents say so.
    results["G9"] = (True, "tools/check_claims.py passes: claims recomputed, documents re-rendered and compared")
    if options.save or not options.quick:
        save(report(results, mode, extra))
        run("tools/claims.py")
    if run("tools/check_claims.py")[0] != 0:
        results["G9"] = (False, "tools/check_claims.py reports problems")
        if options.save or not options.quick:
            save(report(results, mode, extra))
            run("tools/claims.py")

    for gate, (ok, detail) in results.items():
        print(f"{gate} {'PASS' if ok else 'FAIL'}  {detail}", flush=True)
    count = sum(1 for ok, _ in results.values() if ok)
    print(f"gates: {count}/{len(results)} passed ({mode} run)", flush=True)
    return 0 if count == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
