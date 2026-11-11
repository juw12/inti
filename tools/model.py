"""Run the kernel model and keep what it printed.

    python tools/model.py <model program> [<target triple>]

`python build.py model` compiles the program (the kernel's own source on a fake machine, see
model/src) and calls this script. It runs `model all`: the directed tests, then the bounded
exploration of every scenario. Everything the program prints goes to evidence/model.log; the
numbers in it go to evidence/model.json, where tools/claims.py finds them.

The exit status is 0 only if the program's own was 0 (every test passed, no explored step
broke a rule) and every line this script expects was there to read.
"""
from __future__ import annotations

import datetime
import json
import platform
import re
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import claims  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
EVIDENCE = ROOT / "evidence"
LOG = EVIDENCE / "model.log"
RESULT = EVIDENCE / "model.json"

SCENARIO = re.compile(r"^MODEL scenario ([\w-]+): (.+) \(depth \d+\)$")
EXPLORE = re.compile(
    r"^MODEL explore ([\w-]+): depth (\d+), (\d+) states, (\d+) transitions, (\d+) violations, complete (yes|no), "
    r"(\d+) deliveries checked, (\d+) replies checked, (\d+) restarts, (\d+) beyond budget, "
)
RESULTS = re.compile(r"^MODEL results ([\w-]+): (.+)$")
PAIRS = re.compile(r"^MODEL pairs ([\w-]+): (\d+) of (\d+) ordered pairs of failure kinds, (\d+) failure steps")
TOTAL = re.compile(
    r"^MODEL explore total: (\d+) scenarios, (\d+) states, (\d+) transitions, (\d+) violations, "
    r"(\d+) of (\d+) ordered pairs of failure kinds"
)
NOT_MET = re.compile(r"^MODEL pairs not met: (.+)$")
TABLES = re.compile(r"^MODEL tables: (PASS|FAIL) (\d+)/(\d+)")
SEEDS = re.compile(r"^MODEL seeds: (\d+) of (\d+) ")
BLIND = re.compile(r"^MODEL blind: (\d+) of (\d+) ")


def parse(lines: list[str]) -> dict:
    """The numbers in the model's output. Raises KeyError naming the first line that is missing."""
    scenarios: dict[str, dict] = {}
    found: dict[str, object] = {}
    for line in lines:
        if match := SCENARIO.match(line):
            scenarios[match.group(1)] = {"name": match.group(1), "about": match.group(2)}
        elif match := EXPLORE.match(line):
            depth, states, transitions, violations = (int(match.group(index)) for index in (2, 3, 4, 5))
            deliveries, replies, restarts, beyond = (int(match.group(index)) for index in (7, 8, 9, 10))
            scenarios[match.group(1)].update(
                depth=depth, states=states, transitions=transitions, violations=violations,
                complete=match.group(6) == "yes", deliveries=deliveries, replies=replies,
                restarts=restarts, beyond_budget=beyond,
            )
        elif match := RESULTS.match(line):
            counts = dict(part.rsplit(" ", 1) for part in match.group(2).split(", "))
            scenarios[match.group(1)]["results"] = {name: int(count) for name, count in counts.items()}
        elif match := PAIRS.match(line):
            scenarios[match.group(1)].update(pairs=int(match.group(2)), failure_steps=int(match.group(4)))
        elif match := TOTAL.match(line):
            names = ("scenarios", "states", "transitions", "violations", "pairs", "pairs_possible")
            found["total"] = {name: int(match.group(index + 1)) for index, name in enumerate(names)}
        elif match := NOT_MET.match(line):
            found["pairs_not_met"] = match.group(1).split("; ")
        elif match := TABLES.match(line):
            found["tables"] = {"passed": int(match.group(2)), "total": int(match.group(3))}
        elif match := SEEDS.match(line):
            found["seeds"] = {"found": int(match.group(1)), "planted": int(match.group(2))}
        elif match := BLIND.match(line):
            found["blind"] = {"unseen": int(match.group(1)), "forged": int(match.group(2))}
    for key in ("tables", "seeds", "blind", "total"):
        if key not in found:
            raise KeyError(f"no line for {key}")
    for scenario in scenarios.values():
        for key in ("states", "results", "pairs"):
            if key not in scenario:
                raise KeyError(f"scenario {scenario['name']}: no line for {key}")
    if len(scenarios) != found["total"]["scenarios"]:
        raise KeyError("the total counts a different number of scenarios than were printed")
    found.setdefault("pairs_not_met", [])
    found["scenarios"] = list(scenarios.values())
    found["verdict"] = lines[-1] if lines else ""
    return found


def main() -> int:
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    program = Path(sys.argv[1])
    started = time.monotonic()
    done = subprocess.run([str(program), "all"], cwd=ROOT, capture_output=True, text=True)
    seconds = round(time.monotonic() - started)
    output = done.stdout + done.stderr
    EVIDENCE.mkdir(exist_ok=True)
    LOG.write_text(output, encoding="utf-8", newline="\n")
    lines = output.splitlines()
    print("\n".join(line for line in lines if line.startswith("MODEL")), flush=True)
    try:
        result = parse(lines)
    except KeyError as missing:
        print(f"model: {missing.args[0]}; see {LOG.relative_to(ROOT).as_posix()}", flush=True)
        return 1
    passed = done.returncode == 0 and result["verdict"] == "MODEL: PASS"
    result = {
        "date": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
        "source_sha256": claims.model_fingerprint(),
        "command": "model all",
        "target": sys.argv[2] if len(sys.argv) > 2 else "not given",
        "host": platform.platform(),
        "duration_s": seconds,
        "passed": passed,
        **result,
    }
    RESULT.write_text(json.dumps(result, indent=1) + "\n", encoding="utf-8", newline="\n")
    print(f"model: {'PASS' if passed else 'FAIL'}, {seconds} s -> evidence/model.log, evidence/model.json", flush=True)
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
