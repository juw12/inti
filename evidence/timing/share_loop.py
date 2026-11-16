"""Run the self-test many times on a machine with two processors and on one with one, in turn.

    share_loop.py <root> <rounds> <runs> <out.jsonl>

A round is one boot with `-smp 2` and one boot without, so that both kinds of machine see
the same minutes of the host. Each boot runs the self-test <runs> times. One line is
written per boot. Every run is recorded with the five stretches of the measurement that
was used, every measurement that was set aside with the reason the self-test gave, the
last line of the self-test, and the lines of the checks that failed.
"""
import json
import re
import sys
import time
from pathlib import Path

root = Path(sys.argv[1])
rounds = int(sys.argv[2])
runs = int(sys.argv[3])
out = Path(sys.argv[4])
sys.path.insert(0, str(root / "tools"))
from vm import VM  # noqa: E402

MACHINES = (("two processors", ["-smp", "2"]), ("one processor", []))
FIGURES = r": (\d+) rounds, (\d+) ticks of which (\d+) found this program running, (\d+) task switches"
USED = re.compile(r"^\s+(alone, half a second|beside the group[^:]*)" + FIGURES, re.M)
ASIDE = re.compile(r"set aside: (by itself[^:]*|next to the group[^:]*)" + FIGURES)
SPEED = re.compile(r"try (\d+) of (\d+) set aside: the machine changed speed during it, from (\d+) to (\d+) rounds")
TICKS = re.compile(r"try (\d+) of (\d+) set aside: the machine lost timer ticks during it, one stretch\s+"
                   r"got (\d+) of the (\d+) that were due")


def numbers(found):
    return [[name, int(a), int(b), int(c), int(d)] for name, a, b, c, d in found]


def whole_numbers(found):
    return [[int(value) for value in match] for match in found]


with open(out, "a", encoding="utf-8", newline="\n") as log:
    for index in range(rounds):
        for machine, extra in MACHINES:
            record = {"round": index, "machine": machine, "started": round(time.time(), 3), "runs": []}
            try:
                with VM(extra=extra) as vm:
                    vm.wait_for_prompt(timeout=120)
                    for _ in range(runs):
                        began = time.time()
                        text = vm.command("selftest", timeout=400)
                        record["runs"].append({
                            "started": round(began, 3),
                            "ended": round(time.time(), 3),
                            "stretches": numbers(USED.findall(text)),
                            "set_aside_for_speed": whole_numbers(SPEED.findall(text)),
                            "set_aside_for_ticks": whole_numbers(TICKS.findall(text)),
                            "set_aside_stretches": numbers(ASIDE.findall(text)),
                            "result": re.findall(r"SELFTEST: [^\r\n]*", text)[-1:],
                            "failed": [line.strip() for line in re.findall(r"^\s*FAIL [^\r\n]*", text, re.M)],
                        })
            except Exception as problem:  # a boot that never reaches the prompt is a result too
                record["error"] = repr(problem)[:300]
            record["ended"] = round(time.time(), 3)
            log.write(json.dumps(record) + "\n")
            log.flush()
            print(index, machine, len(record["runs"]), "runs", record.get("error", ""), flush=True)
print("done")
