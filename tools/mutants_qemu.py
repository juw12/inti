#!/usr/bin/env python3
"""Mutants in QEMU: the changed kernels the host model let through, booted and tested.

    python build.py mutants-qemu                 every mutant the four samples did not catch
    python build.py mutants-qemu --only kernel/src/mem.rs:165   chosen places; nothing is written

`python build.py mutants` runs the kernel's source on the build machine, where the paging
code, the trap frames and the processor are replaced. A changed kernel that those checks let
through may still be noticed by the system itself. This script takes every mutant marked
"not caught" in evidence/mutants.json, mutants-held-out.json, mutants-fresh.json and
mutants-unseen.json, and for each one:

1. plants the same one-line change in a copy of the sources under build/mutants/qemu,
2. builds the disk image there (`python build.py build`),
3. boots it in QEMU and runs the test suite (`python tools/test.py --fuzz N --runs N`).

The verdicts:

    caught             the suite failed: a check failed, or the run passed its time limit
    same image         the disk image is byte for byte the unchanged one, so the change cannot
                       be observed by any test (for example a compile-time assertion)
    not caught         a different kernel, and every check passed: a hole, or a change that
                       makes no difference at run time; the script cannot tell which
    does not build     the image could not be built

The unchanged copy is tested before the first mutant and again after the last. Both runs
must pass, or nothing is written: a suite that fails on the unchanged kernel proves nothing
about a changed one.

The suite here is shorter than `python build.py test` (fewer random system calls, fewer
repeats of each injected fault); the numbers used are recorded in the result. Nothing in
the repository is changed. The result goes to evidence/mutants-qemu.json and
evidence/mutants-qemu.log, with the fingerprint of the sources, which
tools/check_claims.py compares with the sources as they are.
"""
from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(ROOT / "tools"))
import build  # noqa: E402
import claims  # noqa: E402
import mutants  # noqa: E402

EVIDENCE = ROOT / "evidence"
COPY = ROOT / "build" / "mutants" / "qemu"
COPIED = (*mutants.COPIED, "build.py", "tools", "files")
SAMPLES = (("first", "mutants.json"), ("held-out", "mutants-held-out.json"),
           ("fresh", "mutants-fresh.json"), ("unseen", "mutants-unseen.json"))
FUZZ = 200_000
RUNS = 2
BUILD_LIMIT = 1800  # seconds for building one image
LIMIT_FLOOR = 600  # seconds: the least time one run of the suite gets
LIMIT_FACTOR = 3  # a run gets this many times what the unchanged kernel took


def survivors(fingerprint: str) -> tuple[list[dict], str]:
    """Every mutant the host model did not catch, with its whole source line. Returns (list, problem)."""
    places = {(site["file"], site["line"], site["operator"]): site for site in mutants.sites()}
    found = []
    for sample, name in SAMPLES:
        path = EVIDENCE / name
        if not path.exists():
            return [], f"evidence/{name} is missing; run python build.py mutants for that sample"
        report = json.loads(path.read_text(encoding="utf-8"))
        if report["source_sha256"] != fingerprint:
            return [], f"evidence/{name} was made from other sources; run that sample again"
        for mutant in report["mutants"]:
            if mutant["verdict"] != "not caught":
                continue
            site = places.get((mutant["file"], mutant["line"], mutant["operator"]))
            if site is None:
                return [], f"{mutant['file']}:{mutant['line']} of evidence/{name} is not a site any more"
            found.append({**site, "sample": sample, "number": mutant["number"]})
    return found, ""


def copy_sources() -> None:
    """A copy of everything `python build.py test` needs. The copies get the time of the copying."""
    for name in COPIED:
        source, target = ROOT / name, COPY / name
        if source.is_dir():
            shutil.rmtree(target, ignore_errors=True)
            shutil.copytree(source, target, ignore=shutil.ignore_patterns("target", "__pycache__"),
                            copy_function=shutil.copyfile)
        else:
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, target)
    shutil.rmtree(COPY / "evidence", ignore_errors=True)


def run_limited(command: list[str], env: dict[str, str], limit: float) -> tuple[int | None, str]:
    """Run in the copy. Returns (exit code or None when the time ran out, output)."""
    process = subprocess.Popen(command, cwd=COPY, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                               text=True, encoding="utf-8", errors="replace")
    try:
        output, _ = process.communicate(timeout=limit)
        return process.returncode, output
    except subprocess.TimeoutExpired:
        if os.name == "nt":  # the emulator is a grandchild; end the whole tree
            subprocess.run(["taskkill", "/F", "/T", "/PID", str(process.pid)], capture_output=True)
        else:
            process.kill()
        output, _ = process.communicate()
        return None, output


def build_image(env: dict[str, str]) -> str:
    """Build the image in the copy. Returns its SHA-256, or "" if it could not be built."""
    image = COPY / "build" / "inti.img"
    image.unlink(missing_ok=True)
    code, _ = run_limited([sys.executable, "build.py", "build"], env, BUILD_LIMIT)
    if code != 0 or not image.exists():
        return ""
    return hashlib.sha256(image.read_bytes()).hexdigest()


def run_suite(env: dict[str, str], fuzz: int, runs: int, limit: float) -> dict:
    """Boot the image in the copy and test it. Returns passed, failed check names, seconds, timed_out."""
    report_path = COPY / "evidence" / "test.json"
    report_path.unlink(missing_ok=True)
    started = time.monotonic()
    code, output = run_limited(
        [sys.executable, str(Path("tools") / "test.py"), "--fuzz", str(fuzz), "--runs", str(runs)], env, limit)
    seconds = round(time.monotonic() - started)
    if code is None:
        return {"passed": False, "failed": [], "checks": 0, "seconds": seconds, "timed_out": True}
    if not report_path.exists():
        return {"passed": False, "failed": ["the suite ended without a report: " + output[-300:].strip()],
                "checks": 0, "seconds": seconds, "timed_out": False}
    report = json.loads(report_path.read_text(encoding="utf-8"))
    failed = [check["name"] for check in report["checks"] if not check["ok"]]
    return {"passed": code == 0 and report["passed"], "failed": failed, "checks": len(report["checks"]),
            "seconds": seconds, "timed_out": False}


def judge(site: dict, env: dict[str, str], unchanged_image: str, fuzz: int, runs: int, limit: float) -> dict:
    """Plant one change, build, test, put the file back."""
    path = COPY / site["file"]
    original = path.read_bytes()
    lines = original.decode("utf-8").split("\n")
    ending = "\r" if lines[site["line"] - 1].endswith("\r") else ""
    if lines[site["line"] - 1].rstrip("\r") != site["before"]:
        raise RuntimeError(f"{site['file']}:{site['line']} is not the line the site was found on")
    lines[site["line"] - 1] = site["after"] + ending
    result = {key: site[key] for key in ("sample", "number", "file", "line", "operator")}
    result["before"] = site["before"].strip()
    result["after"] = site["after"].strip() or "(line removed)"
    try:
        path.write_bytes("\n".join(lines).encode("utf-8"))
        image = build_image(env)
        if not image:
            return {**result, "verdict": "does not build", "how": "", "by": [], "seconds": 0}
        if image == unchanged_image:
            return {**result, "verdict": "same image", "how": "", "by": [], "seconds": 0}
        outcome = run_suite(env, fuzz, runs, limit)
        if outcome["timed_out"]:
            return {**result, "verdict": "caught", "how": "time limit", "by": [], "seconds": outcome["seconds"]}
        if not outcome["passed"]:
            return {**result, "verdict": "caught", "how": "failed check", "by": outcome["failed"],
                    "seconds": outcome["seconds"]}
        return {**result, "verdict": "not caught", "how": "", "by": [], "seconds": outcome["seconds"]}
    finally:
        path.write_bytes(original)


def line_of(result: dict) -> str:
    verdict = result["verdict"]
    if verdict == "caught":
        first = f": {result['by'][0]}" if result["by"] else ""
        more = f" (+{len(result['by']) - 1} more)" if len(result["by"]) > 1 else ""
        verdict = f"caught by {result['how']}{first}{more}"
    return (f"QEMU MUTANT {result['sample']} #{result['number']} {result['file']}:{result['line']} "
            f"[{result['operator']}] {verdict}")


def main() -> int:
    parser = argparse.ArgumentParser(description="Boot the mutants the host model did not catch; count how many the tests catch.")
    parser.add_argument("--fuzz", type=int, default=FUZZ, help=f"random system calls per run (default {FUZZ})")
    parser.add_argument("--runs", type=int, default=RUNS, help=f"times each fault is injected per run (default {RUNS})")
    parser.add_argument("--only", default="", help="places to run, as file:line,file:line; nothing is written")
    options = parser.parse_args()

    fingerprint = claims.model_fingerprint()
    chosen, problem = survivors(fingerprint)
    if problem:
        print(f"mutants-qemu: stopped: {problem}", flush=True)
        return 1
    if options.only:
        wanted = set(options.only.split(","))
        chosen = [site for site in chosen if f"{site['file']}:{site['line']}" in wanted]
    env = dict(os.environ)
    env["INTI_QEMU"] = build.find_qemu()
    env["PYTHONIOENCODING"] = "utf-8"
    print(f"mutants-qemu: {len(chosen)} mutants the host model did not catch, "
          f"{options.fuzz} random system calls and {options.runs} repeats per run", flush=True)

    copy_sources()
    unchanged_image = build_image(env)
    if not unchanged_image:
        print("mutants-qemu: stopped: the unchanged copy does not build", flush=True)
        return 1
    before = run_suite(env, options.fuzz, options.runs, 4 * LIMIT_FLOOR)
    print(f"mutants-qemu: unchanged kernel before: {'PASS' if before['passed'] else 'FAIL'} "
          f"{before['checks'] - len(before['failed'])}/{before['checks']} checks, {before['seconds']} s", flush=True)
    if not before["passed"]:
        print(f"mutants-qemu: stopped: the unchanged kernel fails: {before['failed'][:3]}", flush=True)
        return 1
    limit = max(LIMIT_FLOOR, LIMIT_FACTOR * before["seconds"])

    results = []
    for site in chosen:
        result = judge(site, env, unchanged_image, options.fuzz, options.runs, limit)
        results.append(result)
        print(line_of(result), flush=True)

    if build_image(env) != unchanged_image:
        print("mutants-qemu: stopped: the copy does not build back to the unchanged image", flush=True)
        return 1
    after = run_suite(env, options.fuzz, options.runs, limit)
    print(f"mutants-qemu: unchanged kernel after: {'PASS' if after['passed'] else 'FAIL'} "
          f"{after['checks'] - len(after['failed'])}/{after['checks']} checks, {after['seconds']} s", flush=True)
    if not after["passed"]:
        print(f"mutants-qemu: stopped: the unchanged kernel fails afterwards: {after['failed'][:3]}", flush=True)
        return 1
    if claims.model_fingerprint() != fingerprint:
        print("mutants-qemu: stopped: the sources changed while the mutants ran; nothing is written", flush=True)
        return 1

    counts = {
        "tried": len(results),
        "caught": sum(result["verdict"] == "caught" for result in results),
        "same_image": sum(result["verdict"] == "same image" for result in results),
        "not_caught": sum(result["verdict"] == "not caught" for result in results),
        "does_not_build": sum(result["verdict"] == "does not build" for result in results),
    }
    final = (f"MUTANTS IN QEMU: {counts['caught']} of {counts['tried']} caught, {counts['same_image']} build the "
             f"same image, {counts['not_caught']} not caught, {counts['does_not_build']} do not build")
    print(final, flush=True)
    if options.only:
        return 0
    report = {
        "date": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
        "command": "python build.py mutants-qemu",
        "source_sha256": fingerprint,
        "options": {"fuzz": options.fuzz, "runs": options.runs},
        "unchanged": {"image_sha256": unchanged_image, "before": before, "after": after},
        "time_limit_s": limit,
        "summary": counts,
        "mutants": results,
    }
    EVIDENCE.mkdir(exist_ok=True)
    (EVIDENCE / "mutants-qemu.json").write_text(json.dumps(report, indent=1) + "\n", encoding="utf-8", newline="\n")
    log = [line_of(result) for result in results] + [final]
    (EVIDENCE / "mutants-qemu.log").write_text("\n".join(log) + "\n", encoding="utf-8", newline="\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
