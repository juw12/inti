"""Plant defects in a copy of the kernel and count how many the model catches.

    python tools/mutants.py [--sample N] [--jobs N] [--held-out | --fresh | --unseen | --fifth] [--only N,N,...]

Why. The model (model/src, `python build.py model`) reports no violation in the kernel. That
says something only if it would have reported one, so this script breaks the kernel on
purpose, one small change at a time, and runs the same checks against every broken kernel.

Which changes. Every place in the kernel files the model compiles where one of the OPERATORS
below applies is a site. The sites are put in a fixed order (file, line, column, operator),
the list is cut into SAMPLE equal stretches and the middle site of each stretch is taken:
nobody picks them by hand, and the same sources give the same sample.

The held-out sample. Once tests have been written against the mutants that got away, the
figure for that sample says how well those tests were aimed, and no longer what the checks
would catch elsewhere. --held-out takes the first site of each stretch instead of the
middle one: a second sample that shares no site with the first and that no test was
written against. Its result goes to files of its own and is reported as it comes out.

The fresh sample. Once tests have been aimed at the held-out sample too, it is held out
no longer. --fresh takes the site three quarters of the way through each stretch: a third
sample that shares no site with the other two. Whether a test was written against it is
not something the script can know; the documents say when it was drawn.

The unseen and the fifth sample. --unseen takes the site a quarter of the way through each
stretch. Once tests had been aimed at what it let through, --fifth was added: the site an
eighth of the way through each stretch, which shares no site with the other four.

What counts. A changed kernel (a "mutant") is caught when a directed test or a scenario of
the exploration fails, when the program stops with a panic, or when a check runs past its
time limit. The checks are the ones `python build.py model` runs, with the same depths,
stopped at the first that objects. A mutant that does not compile is set aside. A mutant
that is not caught is either a hole in the checks or a change that makes no difference;
the script cannot tell which, and evidence/mutants.json lists every one of them.

Which sources. The result records a fingerprint of every Rust file of the kernel, the
interface crate and the model, taken before the first mutant and compared after the last.
If a file changed in between, nothing is written; and tools/check_claims.py refuses a
result whose fingerprint is not that of the sources as they are.

Nothing in the repository is changed: the work happens in copies under build/mutants/.
The result goes to evidence/mutants.json and evidence/mutants.log, or with --held-out to
evidence/mutants-held-out.json and evidence/mutants-held-out.log, or with --fresh to
evidence/mutants-fresh.json and evidence/mutants-fresh.log, or with --unseen to
evidence/mutants-unseen.json and evidence/mutants-unseen.log, or with --fifth to
evidence/mutants-fifth.json and evidence/mutants-fifth.log. --only writes nothing.
"""
from __future__ import annotations

import argparse
import datetime
import json
import queue
import re
import shutil
import subprocess
import sys
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(ROOT / "tools"))
import build  # noqa: E402
import claims  # noqa: E402

EVIDENCE = ROOT / "evidence"
WORK = ROOT / "build" / "mutants"
# The kernel files that model/src/main.rs compiles; arch.rs and main.rs are replaced there.
FILES = ("check.rs", "elf.rs", "frame.rs", "ipc.rs", "mem.rs", "sys.rs", "task.rs")
COPIED = ("abi", "boot", "kernel", "user", "model", ".cargo", "Cargo.toml", "Cargo.lock")
SAMPLE = 60
JOBS = 2
BUILD_LIMIT = 900  # seconds for one compilation
LIMIT_FLOOR = 60  # seconds: the least time any check gets
LIMIT_FACTOR = 6  # a check gets this many times what it took on the unchanged kernel

# name, what to find, what to put there
TOKEN_OPERATORS = (
    ("== becomes !=", " == ", " != "),
    ("!= becomes ==", " != ", " == "),
    ("< becomes <=", " < ", " <= "),
    ("<= becomes <", " <= ", " < "),
    ("> becomes >=", " > ", " >= "),
    (">= becomes >", " >= ", " > "),
    ("&& becomes ||", " && ", " || "),
    ("|| becomes &&", " || ", " && "),
    ("+ becomes -", " + ", " - "),
    ("- becomes +", " - ", " + "),
)
CALL = re.compile(
    r"^\s+(?!return\b|let\b|break\b|continue\b|assert|debug_assert|if\b|while\b|for\b|match\b|unsafe\b)"
    r"[A-Za-z_][\w:.\[\]]*\(.*\);$"
)
ASSIGNMENT = re.compile(r"^\s+(?!let\b|const\b|static\b|return\b|pub\b|type\b|use\b)[^=\s][^=]*? (=|\+=|-=|\|=|&=) [^=].*;$")
CONDITION = re.compile(r"^(\s*(?:\} else )?if )(?!let\b)(.+)( \{)$")
LINE_OPERATORS = ("call removed", "assignment removed", "condition inverted")
OPERATORS = [name for name, _, _ in TOKEN_OPERATORS] + list(LINE_OPERATORS)


def sites() -> list[dict]:
    """Every place an operator applies, in a fixed order."""
    found = []
    for name in FILES:
        path = f"kernel/src/{name}"
        for number, line in enumerate((ROOT / path).read_text(encoding="utf-8").splitlines(), start=1):
            code = line.split("//", 1)[0].rstrip()
            if not code.strip() or '"' in code:
                continue  # comments, blank lines, and lines with text in them
            comment = line[len(code):]
            for operator, old, new in TOKEN_OPERATORS:
                for match in re.finditer(re.escape(old), code):
                    after = code[:match.start()] + new + code[match.end():] + comment
                    found.append({"file": path, "line": number, "column": match.start(), "operator": operator,
                                  "before": line, "after": after})
            if CALL.match(code):
                found.append({"file": path, "line": number, "column": 0, "operator": "call removed",
                              "before": line, "after": ""})
            if ASSIGNMENT.match(code):
                found.append({"file": path, "line": number, "column": 0, "operator": "assignment removed",
                              "before": line, "after": ""})
            if match := CONDITION.match(code):
                after = f"{match.group(1)}!({match.group(2)}){match.group(3)}" + comment
                found.append({"file": path, "line": number, "column": 0, "operator": "condition inverted",
                              "before": line, "after": after})
    found.sort(key=lambda site: (site["file"], site["line"], site["column"], OPERATORS.index(site["operator"])))
    return found


# Where in its stretch each sample takes its site, in eighths of the stretch.
EIGHTH = {"first": 4, "held-out": 0, "fresh": 6, "unseen": 2, "fifth": 1}


def positions(total: int, count: int, which: str) -> list[int]:
    """Where a sample lies in the ordered list: one site from each of `count` equal stretches.

    The first sample takes the middle site of each stretch, the held-out sample the first one,
    the fresh sample the one three quarters of the way through, the unseen sample the one a
    quarter of the way through, the fifth sample the one an eighth of the way through.
    """
    return [(8 * index + EIGHTH[which]) * total // (8 * count) for index in range(count)]


def sample(found: list[dict], count: int, which: str = "first") -> list[dict]:
    """`count` sites at even spacing, numbered from one."""
    count = min(count, len(found))
    chosen = [dict(found[position]) for position in positions(len(found), count, which)]
    for number, site in enumerate(chosen, start=1):
        site["number"] = number
    return chosen


class Job:
    """One copy of the sources, with its own compiled model."""

    def __init__(self, number: int):
        self.root = WORK / f"job{number}"
        self.command, self.env, self.program, _ = build.model_command(self.root)
        self.checks: list[list[str]] = []
        self.limits: dict[str, float] = {}

    def prepare(self) -> str:
        """Copy the sources, compile them unchanged and time every check. Returns "" or what went wrong.

        The copies get the time of the copying, not the time of the files they are made from.
        cargo decides by those times whether to compile, and with the old times it would keep
        the program it built from the last mutant of an earlier run and call it the unchanged
        kernel.
        """
        for name in COPIED:
            source, target = ROOT / name, self.root / name
            if source.is_dir():
                shutil.rmtree(target, ignore_errors=True)
                shutil.copytree(source, target, ignore=shutil.ignore_patterns("target"),
                                copy_function=shutil.copyfile)
            else:
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(source, target)
        if not self.compile(self.root / "kernel" / "src" / FILES[0]):
            return "the unchanged kernel does not compile"
        listed = subprocess.run([str(self.program), "list"], cwd=self.root, capture_output=True, text=True)
        self.checks = [["tables"]] + [["explore", line.split()[0]] for line in listed.stdout.splitlines() if line.strip()]
        for check in self.checks:
            started = time.monotonic()
            done = subprocess.run([str(self.program), *check], cwd=self.root, capture_output=True, text=True)
            if done.returncode != 0:
                return f"the unchanged kernel fails `model {' '.join(check)}`"
            self.limits[" ".join(check)] = max(LIMIT_FLOOR, LIMIT_FACTOR * (time.monotonic() - started))
        return ""

    def compile(self, changed: Path) -> bool:
        """Compile the copy; False if it does not compile. `changed` is a file written since the last time.

        If the program is not newer than that file afterwards, cargo did not compile at all, and
        what would be judged is the program of an earlier mutant. That is an error of this script
        and not a verdict, so it stops the run.
        """
        try:
            done = subprocess.run(self.command, cwd=self.root, env=self.env, capture_output=True, text=True,
                                  timeout=BUILD_LIMIT)
        except subprocess.TimeoutExpired:
            return False
        if done.returncode != 0:
            return False
        if self.program.stat().st_mtime_ns <= changed.stat().st_mtime_ns:
            raise RuntimeError(f"{self.program} is not newer than {changed}: cargo did not compile the change")
        return True

    def judge(self, site: dict) -> dict:
        """Plant one change, compile, run the checks, put the file back."""
        path = self.root / site["file"]
        original = path.read_bytes()
        lines = original.decode("utf-8").split("\n")
        ending = "\r" if lines[site["line"] - 1].endswith("\r") else ""
        if lines[site["line"] - 1].rstrip("\r") != site["before"]:
            raise RuntimeError(f"{site['file']}:{site['line']} is not the line the site was found on")
        lines[site["line"] - 1] = site["after"] + ending
        result = {key: site[key] for key in ("number", "file", "line", "operator")}
        result["before"] = site["before"].strip()
        result["after"] = site["after"].strip() or "(line removed)"
        try:
            path.write_bytes("\n".join(lines).encode("utf-8"))
            if not self.compile(path):
                return {**result, "verdict": "does not compile", "how": "", "by": ""}
            for check in self.checks:
                name = " ".join(check)
                try:
                    done = subprocess.run([str(self.program), *check], cwd=self.root, capture_output=True, text=True,
                                          timeout=self.limits[name])
                except subprocess.TimeoutExpired:
                    return {**result, "verdict": "caught", "how": "time limit", "by": name}
                if done.returncode != 0:
                    how = "panic" if "MODEL PANIC" in done.stdout + done.stderr else "failed check"
                    return {**result, "verdict": "caught", "how": how, "by": name}
            return {**result, "verdict": "not caught", "how": "", "by": ""}
        finally:
            path.write_bytes(original)


def summary(results: list[dict], site_count: int) -> dict:
    compiled = [result for result in results if result["verdict"] != "does not compile"]
    caught = [result for result in compiled if result["verdict"] == "caught"]
    return {
        "sites": site_count,
        "sample": len(results),
        "not_compiled": len(results) - len(compiled),
        "compiled": len(compiled),
        "caught": len(caught),
        "not_caught": len(compiled) - len(caught),
        "caught_by_failed_check": sum(1 for result in caught if result["how"] == "failed check"),
        "caught_by_panic": sum(1 for result in caught if result["how"] == "panic"),
        "caught_by_time_limit": sum(1 for result in caught if result["how"] == "time limit"),
        "caught_by_tables": sum(1 for result in caught if result["by"] == "tables"),
        "caught_by_exploration": sum(1 for result in caught if result["by"].startswith("explore")),
        "caught_percent": round(100 * len(caught) / len(compiled)) if compiled else 0,
    }


def line_of(result: dict) -> str:
    outcome = result["verdict"]
    if result["verdict"] == "caught":
        outcome = f"caught by `model {result['by']}` ({result['how']})"
    return f"mutant {result['number']:3d}  {result['file']}:{result['line']}  {result['operator']}: {outcome}"


def main() -> int:
    parser = argparse.ArgumentParser(description="Plant defects in a copy of the kernel; count how many the model catches.")
    parser.add_argument("--sample", type=int, default=SAMPLE, help=f"how many sites to try (default {SAMPLE})")
    parser.add_argument("--jobs", type=int, default=JOBS, help=f"how many copies to work in at once (default {JOBS})")
    parser.add_argument("--held-out", action="store_true",
                        help="take the second sample: the first site of each stretch, none shared with the first")
    parser.add_argument("--fresh", action="store_true",
                        help="take the third sample: three quarters of the way through each stretch")
    parser.add_argument("--unseen", action="store_true",
                        help="take the fourth sample: a quarter of the way through each stretch")
    parser.add_argument("--fifth", action="store_true",
                        help="take the fifth sample: an eighth of the way through each stretch")
    parser.add_argument("--only", default="", help="numbers of the sampled mutants to run; nothing is written")
    options = parser.parse_args()
    named = [name for name in ("held-out", "fresh", "unseen", "fifth") if getattr(options, name.replace("-", "_"))]
    if len(named) > 1:
        print(f"mutants: stopped: --{named[0]} and --{named[1]} name two different samples; give one", flush=True)
        return 1
    which = named[0] if named else "first"

    fingerprint = claims.model_fingerprint()
    found = sites()
    count = min(options.sample, len(found))
    drawn = [position for name in EIGHTH for position in positions(len(found), count, name)]
    if len(set(drawn)) != len(drawn):
        print("mutants: stopped: two of the samples would share a site; take a smaller --sample", flush=True)
        return 1
    chosen = sample(found, options.sample, which)
    if options.only:
        wanted = {int(number) for number in options.only.split(",")}
        chosen = [site for site in chosen if site["number"] in wanted]
    print(f"mutants: {len(found)} sites in {len(FILES)} kernel files, {len(chosen)} to try ({which} sample), "
          f"{options.jobs} at a time", flush=True)

    waiting: queue.Queue = queue.Queue()
    for site in chosen:
        waiting.put(site)
    results: list[dict] = []
    problems: list[str] = []
    lock = threading.Lock()

    def work(number: int) -> None:
        job = Job(number)
        problem = job.prepare()
        if problem:
            with lock:
                problems.append(problem)
            return
        while not problems:
            try:
                site = waiting.get_nowait()
            except queue.Empty:
                return
            result = job.judge(site)
            with lock:
                results.append(result)
                print(line_of(result), flush=True)

    threads = [threading.Thread(target=work, args=(number,)) for number in range(1, options.jobs + 1)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    if problems or len(results) != len(chosen):
        print(f"mutants: stopped: {problems[0] if problems else 'not every mutant was tried'}", flush=True)
        return 1
    if claims.model_fingerprint() != fingerprint:
        print("mutants: stopped: the sources changed while the mutants ran; nothing is written", flush=True)
        return 1

    results.sort(key=lambda result: result["number"])
    counts = summary(results, len(found))
    print(
        f"MUTANTS: {counts['caught']} of {counts['compiled']} caught ({counts['caught_percent']}%), "
        f"{counts['not_caught']} not caught, {counts['not_compiled']} of {counts['sample']} did not compile",
        flush=True,
    )
    if options.only:
        return 0
    report = {
        "date": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
        "command": "python build.py mutants" + ("" if which == "first" else f" --{which}")
                   + f" --sample {options.sample}",
        "held_out": options.held_out,
        "fresh": options.fresh,
        "unseen": options.unseen,
        "fifth": options.fifth,
        "source_sha256": fingerprint,
        "files": [f"kernel/src/{name}" for name in FILES],
        "operators": OPERATORS,
        "summary": counts,
        "mutants": results,
    }
    name = "mutants" if which == "first" else f"mutants-{which}"
    EVIDENCE.mkdir(exist_ok=True)
    (EVIDENCE / f"{name}.json").write_text(json.dumps(report, indent=1) + "\n", encoding="utf-8", newline="\n")
    log = [line_of(result) for result in results]
    log.append(f"MUTANTS: {counts['caught']} of {counts['compiled']} caught, {counts['not_caught']} not caught, "
               f"{counts['not_compiled']} of {counts['sample']} did not compile")
    (EVIDENCE / f"{name}.log").write_text("\n".join(log) + "\n", encoding="utf-8", newline="\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
