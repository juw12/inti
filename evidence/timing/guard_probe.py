"""Run the self-test with the steadiness guard many times on a busy host.

    guard_probe.py <root> <boots> <out.jsonl>

Each boot runs the self-test twice. Every run is recorded with the five stretches of the
measurement that was used, the stretches of every measurement that was set aside, and
host wall-clock times.
"""
import json
import re
import sys
import time
from pathlib import Path

root = Path(sys.argv[1])
boots = int(sys.argv[2])
out = Path(sys.argv[3])
sys.path.insert(0, str(root / "tools"))
from vm import VM  # noqa: E402

FIGURES = r": (\d+) rounds, (\d+) ticks of which (\d+) found this program running, (\d+) task switches"
USED = re.compile(r"^\s+(alone, half a second|beside the group[^:]*)" + FIGURES, re.M)
ASIDE = re.compile(r"set aside: (by itself[^:]*|next to the group[^:]*)" + FIGURES)
TRIES = re.compile(r"try (\d+) of (\d+) set aside: the machine changed speed during it, from (\d+) to (\d+) rounds")


def numbers(found):
    return [[name, int(a), int(b), int(c), int(d)] for name, a, b, c, d in found]


with open(out, "a", encoding="utf-8") as log:
    for boot in range(boots):
        record = {"boot": boot, "started": round(time.time(), 3), "runs": []}
        try:
            with VM() as vm:
                vm.wait_for_prompt(timeout=120)
                for _ in range(2):
                    began = time.time()
                    text = vm.command("selftest", timeout=400)
                    record["runs"].append({
                        "started": round(began, 3),
                        "ended": round(time.time(), 3),
                        "stretches": numbers(USED.findall(text)),
                        "set_aside": [[int(value) for value in found] for found in TRIES.findall(text)],
                        "set_aside_stretches": numbers(ASIDE.findall(text)),
                        "result": re.findall(r"SELFTEST: [^\r\n]*", text)[-1:],
                        "failed": re.findall(r"^\s*FAIL [^\r\n]*", text, re.M),
                    })
        except Exception as problem:  # a boot that never reaches the prompt is a result too
            record["error"] = repr(problem)[:300]
        record["ended"] = round(time.time(), 3)
        log.write(json.dumps(record) + "\n")
        log.flush()
print("done")
