#!/usr/bin/env python3
"""The kernel's source under Miri: the model's directed checks, run by an interpreter that watches for
undefined behaviour.

    python build.py miri                  the directed checks (`model tables`) under Miri
    python build.py miri --limit 7200     with another time limit, in seconds

The model (see model/src/main.rs) compiles the kernel's own source files into an ordinary program. Miri
runs that program step by step and stops at the first read of uninitialised memory, access outside an
allocation, misaligned access, data race, or use of a pointer that the aliasing rules (Stacked Borrows)
forbid. So this is a check of the `unsafe` blocks that the model reaches, and of nothing else: the paging
code, the trap frames, the port access and the context switch only exist in the image that runs in QEMU,
and Miri never sees them.

Miri is slow: it is hundreds of times slower than the compiled program. The run therefore has a time
limit, and the result says how far it got: how many directed checks had passed when it ended, and
whether it ended by itself. A run that was cut short is recorded as cut short, not as a pass.

It needs a nightly Rust toolchain with the `miri` component, which nothing else in this repository
needs. Name the toolchain with INTI_MIRI_TOOLCHAIN (default: nightly), and the target with
INTI_MIRI_TARGET if it is not the toolchain's own.

The result goes to evidence/miri.json and evidence/miri.log, with the fingerprint of the sources, which
tools/check_claims.py compares with the sources as they are.
"""
from __future__ import annotations

import argparse
import datetime
import json
import os
import re
import subprocess
import sys
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(ROOT / "tools"))
import claims  # noqa: E402

EVIDENCE = ROOT / "evidence"
LIMIT = 3 * 3600  # seconds
UNDEFINED = re.compile(r"^error: Undefined Behavior", re.MULTILINE)
PATHS = re.compile(r"[A-Za-z]:\\[^\s`'\"]*|/(?:home|Users)/[^\s`'\"]*")


def version(toolchain: str, *arguments: str) -> str:
    done = subprocess.run(["cargo", f"+{toolchain}", *arguments], capture_output=True, text=True, cwd=ROOT)
    return done.stdout.strip().splitlines()[0] if done.returncode == 0 and done.stdout.strip() else ""


def end_tree(process: subprocess.Popen) -> None:
    if os.name == "nt":  # the interpreter is a grandchild of cargo
        subprocess.run(["taskkill", "/F", "/T", "/PID", str(process.pid)], capture_output=True)
    else:
        process.kill()


def main() -> int:
    parser = argparse.ArgumentParser(description="Run the model's directed checks under Miri.")
    parser.add_argument("--limit", type=int, default=LIMIT, help=f"seconds before the run is cut short (default {LIMIT})")
    options = parser.parse_args()

    toolchain = os.environ.get("INTI_MIRI_TOOLCHAIN", "nightly")
    target = os.environ.get("INTI_MIRI_TARGET", "")
    miri = version(toolchain, "miri", "--version")
    if not miri:
        print(f"miri: stopped: `cargo +{toolchain} miri --version` does not work; install a nightly toolchain "
              "with the miri component and name it in INTI_MIRI_TOOLCHAIN", flush=True)
        return 1
    fingerprint = claims.model_fingerprint()
    shown = ["cargo", f"+{toolchain}", "miri", "run", "-p", "model"]
    if target:
        shown += ["--target", target]
    command = list(shown)
    if os.environ.get("INTI_MIRI_TARGET_DIR"):  # where the build goes is not part of what was run
        command += ["--target-dir", os.environ["INTI_MIRI_TARGET_DIR"]]
    shown += ["--", "tables"]
    command += ["--", "tables"]
    print(f"miri: {miri}", flush=True)
    print(f"miri: {' '.join(shown)} (time limit {options.limit} s)", flush=True)

    env = dict(os.environ)
    env.pop("RUSTFLAGS", None)
    env.pop("CARGO_ENCODED_RUSTFLAGS", None)
    started = time.monotonic()
    process = subprocess.Popen(command, cwd=ROOT, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                               text=True, encoding="utf-8", errors="replace")
    lines: list[str] = []

    def read() -> None:
        for line in process.stdout:
            line = line.rstrip("\n")
            lines.append(line)
            if not line.startswith("  ok"):
                print(line, flush=True)

    reader = threading.Thread(target=read, daemon=True)
    reader.start()
    timed_out = False
    try:
        code = process.wait(timeout=options.limit)
    except subprocess.TimeoutExpired:
        timed_out = True
        end_tree(process)
        code = process.wait()
    reader.join(timeout=30)
    seconds = round(time.monotonic() - started)
    if claims.model_fingerprint() != fingerprint:
        print("miri: stopped: the sources changed while Miri ran; nothing is written", flush=True)
        return 1

    output = "\n".join(lines)
    passed = sum(line.startswith("  ok") for line in lines)
    failed = sum(line.startswith("  FAIL") for line in lines)
    sections = [line for line in lines if line and not line.startswith((" ", "MODEL", "error", "warning", "note"))
                and not re.match(r"\s*(Compiling|Finished|Running|Preparing|Downloading|Blocking)", line)]
    total = re.search(r"MODEL tables: (PASS|FAIL) (\d+)/(\d+)", output)
    undefined = len(UNDEFINED.findall(output))
    finished = not timed_out and total is not None
    report = {
        "date": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
        "command": "python build.py miri",
        "ran": " ".join(shown),
        "miri": miri,
        "rustc": subprocess.run(
            ["rustc", f"+{toolchain}", "--version"], capture_output=True, text=True).stdout.strip(),
        "source_sha256": fingerprint,
        "limit_s": options.limit,
        "seconds": seconds,
        "timed_out": timed_out,
        "finished": finished,
        "exit_code": None if timed_out else code,
        "checks_passed": passed,
        "checks_failed": failed,
        "checks_total": int(total.group(3)) if total else None,
        "sections_started": len(sections),
        "last_section": sections[-1] if sections else "",
        "undefined_behaviour": undefined,
        "passed": finished and code == 0 and undefined == 0 and failed == 0,
    }
    if timed_out:
        final = (f"MIRI: cut short after {seconds} s: {passed} directed checks had passed, {failed} had failed, "
                 f"{undefined} reports of undefined behaviour")
    else:
        final = (f"MIRI: {'PASS' if report['passed'] else 'FAIL'}: exit code {code} after {seconds} s, {passed} directed "
                 f"checks passed, {failed} failed, {undefined} reports of undefined behaviour")
    print(final, flush=True)
    EVIDENCE.mkdir(exist_ok=True)
    (EVIDENCE / "miri.json").write_text(json.dumps(report, indent=1) + "\n", encoding="utf-8", newline="\n")
    kept = [PATHS.sub("<path>", line) for line in lines] + [final]
    (EVIDENCE / "miri.log").write_text("\n".join(kept) + "\n", encoding="utf-8", newline="\n")
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
