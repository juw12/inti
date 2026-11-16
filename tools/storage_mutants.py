#!/usr/bin/env python3
"""Test the storage test: break the disk server on purpose and see whether the test notices.

    python build.py storage-mutants                     every planted fault
    python build.py storage-mutants --only older-slot   chosen faults; nothing is written
    python build.py storage-mutants --redo older-slot   chosen faults again, into the saved result

`python build.py storage` passing says little unless the same test fails on a file system
that is wrong. This script plants one fault at a time in a copy of the sources under
build/mutants/storage, builds the disk image there, and runs tools/storage_test.py on it
(with fewer power cuts than the full run; the numbers are recorded). Each fault is one the
design of the disk server exists to prevent: writing over the meta block in use, leaving out
a flush, reusing a block the state on the disk still needs, trusting a meta block that was
not checked, staying silent towards the supervisor while the drive is slow, going on without
a reset after the drive failed. The list is written by hand, in MUTANTS below, with the
reason for each.

The verdicts:

    caught             the test failed on the changed disk server twice, and both runs ended by
                       themselves with a report of the checks that failed
    timed out          a run was ended at the time limit: no verdict, counted neither as caught
                       nor as not caught. A test that is slow because the computer is busy looks
                       the same as one that hangs, so the limit says nothing about the fault
    not repeated       the test failed once and passed when it was run again; not counted
    not caught         a different image, and every check passed: a hole in the test, or a
                       change that makes no difference; the note of the mutant says which
                       was expected
    same image         the image is byte for byte the unchanged one
    does not build     the image could not be built

The unchanged copy is tested before the first fault and after the last, and both runs must
pass, or nothing is written. Nothing in the repository is changed. The result goes to
evidence/storage_mutants.json with the SHA-256 of user/src/bin/disk.rs, tools/intifs.py and
tools/storage_test.py, so that a result made from other sources can be told apart.

--redo plants the named faults again and puts their verdicts into the saved result. It is for a
fault whose text in MUTANTS had to be corrected (one that did not compile, say), so that the
other faults need not be run again. It refuses a saved result from other sources, another
image or other options, tests the unchanged copy before and after like a full run, and lists
what it replaced under "redone" in the result.
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
EVIDENCE = ROOT / "evidence"
COPY = ROOT / "build" / "mutants" / "storage"
COPIED = ("abi", "boot", "kernel", "user", "model", ".cargo", "Cargo.toml", "Cargo.lock",
          "build.py", "tools", "files", "disk")
SOURCES = ("user/src/bin/disk.rs", "tools/intifs.py", "tools/storage_test.py")
TARGET = "user/src/bin/disk.rs"
CUTS = 6
FUZZ = 40
BUILD_LIMIT = 1800  # seconds for building one image
LIMIT_FLOOR = 1800  # seconds: the least time one run of the test gets
LIMIT_FACTOR = 6  # a run gets this many times what the unchanged disk server took

# (name, what is planted, the text taken out, the text put in, what is expected and why)
MUTANTS = (
    ("slot-in-use",
     "publish writes the new meta block over the one in use instead of the other slot",
     "        let slot = 1 - self.active;\n",
     "        let slot = self.active;\n",
     "caught: a cut inside those 8 sectors leaves no sealed meta block of the old state"),
    ("no-data-flush",
     "publish does not flush the data blocks before it writes the meta block",
     "        if !self.ata.flush() {\n            return Err(D_IO);\n        }\n        let slot = 1 - self.active;\n",
     "        let slot = 1 - self.active;\n",
     "caught: the meta block can reach the disk before the data it points to"),
    ("no-meta-flush",
     "publish does not flush after the meta block",
     "        if !self.put(slot, Source::Work) || !self.ata.flush() {\n",
     "        if !self.put(slot, Source::Work) {\n",
     "caught: a change the shell was told is done can still be lost"),
    ("reuse-live-block",
     "a block counts as free when only the state being prepared does not use it",
     "next(self.meta, block) == FREE && next(self.work, block) == FREE",
     "next(self.work, block) == FREE",
     "caught: data is written over blocks the state on the disk still points to"),
    ("mount-unchecked",
     "mount accepts a sealed meta block without checking that it describes a sound volume",
     "        if !consistent(self.meta) {\n            return D_DAMAGED;\n        }\n"
     "        self.work.copy_from_slice(&self.meta[..]);\n",
     "        self.work.copy_from_slice(&self.meta[..]);\n",
     "caught: a sealed but unsound meta block must be refused"),
    ("older-slot",
     "mount takes the older of two sealed meta blocks",
     "        if second && (!first || newer) {\n",
     "        if second && (!first || !newer) {\n",
     "caught: the last change is gone after a restart"),
    ("no-checksum",
     "a meta block counts as sealed without its checksum being compared",
     "        && checksum(&meta[..CHECKSUM_AT]) == get32(meta, CHECKSUM_AT)\n",
     "",
     "caught: a torn meta block is taken for a whole one"),
    ("publish-unchecked",
     "publish writes the meta block without checking it first",
     "        if !consistent(self.work) {\n            return Err(D_DAMAGED);\n        }\n        if !self.ata.flush() {\n",
     "        if !self.ata.flush() {\n",
     "not caught: the guard is a second line of defence; no known request makes the server "
     "prepare an unsound meta block, so removing it changes nothing a test can see"),
    ("no-rollback",
     "a refused change leaves its half-made chains in the state being prepared",
     "        self.draft = None;\n        self.work.copy_from_slice(&self.meta[..]);\n    }\n",
     "        self.draft = None;\n    }\n",
     "caught: the next change publishes blocks of the refused one"),
    ("shared-block",
     "the soundness check lets two files share a block",
     "            if used[byte] & bit != 0 {\n                return false;\n            }\n",
     "",
     "caught: the disk server and intifs.py disagree on such a meta block"),
    ("partition-past-end",
     "mount accepts a partition that reaches past the end of the drive",
     "sectors < 3 * SECTORS_PER_BLOCK || !fits {",
     "sectors < 3 * SECTORS_PER_BLOCK {",
     "caught: such a disk must be refused, not written to"),
    ("no-ping-while-waiting",
     "the disk server does not look at its requests while it waits for the drive",
     "        if self.count == HELD {\n            if wait_ms > 0 {\n",
     "        if self.count <= HELD {\n            if wait_ms > 0 {\n",
     "caught: the supervisor takes a disk server that waits for a slow drive for a hung one and ends it"),
    ("no-reset-after-failure",
     "the next command is sent without a reset after a step of the drive failed",
     "        if self.unsure && !self.reset() {\n            return false;\n        }\n"
     "        if !self.wait(Until::Idle) {\n            return false;\n        }\n        out(DEVICE,",
     "        if !self.wait(Until::Idle) {\n            return false;\n        }\n        out(DEVICE,",
     "caught: the drive takes the next command for part of the write it was left in"),
)


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def fingerprint() -> dict[str, str]:
    """The SHA-256 of each source the result depends on, with line endings as in the repository."""
    return {name: hashlib.sha256((ROOT / name).read_bytes().replace(b"\r\n", b"\n")).hexdigest() for name in SOURCES}


def copy_sources() -> None:
    """A copy of everything the build and the storage test need."""
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
    return sha256(image)


def run_test(env: dict[str, str], cuts: int, fuzz: int, limit: float) -> dict:
    """Run the storage test in the copy. Returns passed, failed check names, checks, seconds, timed_out."""
    report_path = COPY / "evidence" / "storage.json"
    report_path.unlink(missing_ok=True)
    started = time.monotonic()
    code, output = run_limited(
        [sys.executable, str(Path("tools") / "storage_test.py"), "--cuts", str(cuts), "--fuzz", str(fuzz)], env, limit)
    seconds = round(time.monotonic() - started)
    if code is None:
        return {"passed": False, "failed": [], "checks": 0, "seconds": seconds, "timed_out": True}
    if not report_path.exists():
        return {"passed": False, "failed": ["the test ended without a report: " + output[-300:].strip()],
                "checks": 0, "seconds": seconds, "timed_out": False}
    report = json.loads(report_path.read_text(encoding="utf-8"))
    failed = [check["name"] for check in report["checks"] if not check["ok"]]
    return {"passed": code == 0 and report["passed"], "failed": failed, "checks": len(report["checks"]),
            "seconds": seconds, "timed_out": False}


def judge(mutant: tuple[str, str, str, str, str], env: dict[str, str], unchanged_image: str, cuts: int, fuzz: int,
          limit: float) -> dict:
    """Plant one fault, build, test, test again if it failed; put the file back."""
    name, what, before, after, expected = mutant
    path = COPY / TARGET
    original = path.read_bytes()
    text = original.decode("utf-8").replace("\r\n", "\n")
    result = {"name": name, "what": what, "expected": expected, "file": TARGET}
    if text.count(before) != 1:
        return {**result, "verdict": "not planted", "by": [], "runs": [],
                "note": f"the text to replace is there {text.count(before)} times, not once"}
    result["line"] = text[:text.index(before)].count("\n") + 1
    try:
        path.write_bytes(text.replace(before, after).encode("utf-8"))
        image = build_image(env)
        if not image:
            return {**result, "verdict": "does not build", "by": [], "runs": []}
        if image == unchanged_image:
            return {**result, "verdict": "same image", "by": [], "runs": []}
        first = run_test(env, cuts, fuzz, limit)
        if first["timed_out"]:  # slow or hanging, the limit cannot tell which: no verdict
            return {**result, "verdict": "timed out", "by": [], "runs": [first]}
        if first["passed"]:
            return {**result, "verdict": "not caught", "by": [], "runs": [first]}
        second = run_test(env, cuts, fuzz, limit)
        if second["timed_out"]:
            return {**result, "verdict": "timed out", "by": [], "runs": [first, second]}
        if second["passed"]:
            return {**result, "verdict": "not repeated", "by": first["failed"], "runs": [first, second]}
        both = [check for check in first["failed"] if check in second["failed"]]
        return {**result, "verdict": "caught", "by": both or first["failed"], "runs": [first, second]}
    finally:
        path.write_bytes(original)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--only", action="append", choices=[mutant[0] for mutant in MUTANTS],
                        help="plant only this fault; nothing is written to evidence/")
    parser.add_argument("--redo", action="append", choices=[mutant[0] for mutant in MUTANTS],
                        help="plant this fault again and put its verdict into the saved result")
    parser.add_argument("--cuts", type=int, default=CUTS, help="power cuts in a row in each run")
    parser.add_argument("--fuzz", type=int, default=FUZZ, help="random changes to a meta block in each run")
    options = parser.parse_args()

    began = time.monotonic()
    sources = fingerprint()
    env = dict(os.environ)
    saved_path = EVIDENCE / "storage_mutants.json"
    saved = None
    if options.redo:
        if options.only:
            parser.error("--redo and --only do not go together")
        if not saved_path.exists():
            print("storage-mutants: --redo needs a saved result of a full run", flush=True)
            return 1
        saved = json.loads(saved_path.read_text(encoding="utf-8"))
        if saved["sources_sha256"] != sources or saved["options"] != {"cuts": options.cuts, "fuzz": options.fuzz}:
            print("storage-mutants: the saved result is from other sources or other options; run all faults",
                  flush=True)
            return 1
    COPY.mkdir(parents=True, exist_ok=True)
    copy_sources()
    unchanged_image = build_image(env)
    if not unchanged_image:
        print("storage-mutants: the unchanged copy does not build", flush=True)
        return 1
    if saved and saved["image_sha256"] != unchanged_image:
        print("storage-mutants: the saved result is from another image; run all faults", flush=True)
        return 1
    before = run_test(env, options.cuts, options.fuzz, BUILD_LIMIT)
    print(f"storage-mutants: unchanged, before: {'pass' if before['passed'] else 'FAIL'} "
          f"{before['checks']} checks, {before['seconds']} s {before['failed']}", flush=True)
    if not before["passed"]:
        return 1
    limit = max(LIMIT_FLOOR, LIMIT_FACTOR * before["seconds"])

    results = []
    for mutant in MUTANTS:
        if (options.only or options.redo) and mutant[0] not in (options.only or options.redo):
            continue
        result = judge(mutant, env, unchanged_image, options.cuts, options.fuzz, limit)
        results.append(result)
        print(f"storage-mutants: {result['name']:<20} {result['verdict']:<14} "
              f"{'; '.join(result['by'])[:300]}", flush=True)

    rebuilt = build_image(env)
    after = run_test(env, options.cuts, options.fuzz, limit)
    print(f"storage-mutants: unchanged, after: {'pass' if after['passed'] else 'FAIL'} "
          f"{after['checks']} checks, {after['seconds']} s {after['failed']}", flush=True)
    sound = after["passed"] and rebuilt == unchanged_image and fingerprint() == sources
    caught = sum(result["verdict"] == "caught" for result in results)
    timed_out = sum(result["verdict"] == "timed out" for result in results)
    print(f"storage-mutants: {caught} of {len(results)} caught, {timed_out} without a verdict (time limit "
          f"{round(limit)} s), {round(time.monotonic() - began)} s", flush=True)
    if not sound:
        print("storage-mutants: the unchanged copy did not pass or did not build to the same image afterwards, "
              "or the sources changed during the run; nothing is written", flush=True)
        return 1
    if options.only:
        print("storage-mutants: a partial run; evidence/storage_mutants.json is left as it was", flush=True)
        return 0
    now = datetime.datetime.now().astimezone().isoformat(timespec="seconds")
    if saved:
        by_name = {result["name"]: result for result in results}
        saved.setdefault("redone", []).extend(
            {"name": mutant["name"], "date": now, "verdict_before": mutant["verdict"],
             "unchanged": {"before": before, "after": after}}
            for mutant in saved["mutants"] if mutant["name"] in by_name
        )
        saved["mutants"] = [by_name.get(mutant["name"], mutant) for mutant in saved["mutants"]]
        saved["caught"] = sum(mutant["verdict"] == "caught" for mutant in saved["mutants"])
        saved["duration_s"] += round(time.monotonic() - began)
        saved_path.write_text(json.dumps(saved, indent=2) + "\n", encoding="utf-8", newline="\n")
        print(f"storage-mutants: {len(by_name)} verdicts replaced; {saved['caught']} of {saved['planted']} caught",
              flush=True)
        return 0
    report = {
        "date": now,
        "duration_s": round(time.monotonic() - began),
        "sources_sha256": sources,
        "image_sha256": unchanged_image,
        "options": {"cuts": options.cuts, "fuzz": options.fuzz},
        "limit_s": round(limit),
        "unchanged": {"before": before, "after": after},
        "planted": len(results),
        "caught": caught,
        "mutants": results,
    }
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    saved_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8", newline="\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
