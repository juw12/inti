"""Compare QEMU priority classes on a busy host.

    priority_probe.py <root> <cycles> <out.jsonl>

Each cycle boots one machine per priority class, in turn, and runs the self-test twice.
Every self-test is recorded with host wall-clock times, so it can be laid beside the
host monitor's samples.
"""
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

root = Path(sys.argv[1])
cycles = int(sys.argv[2])
out = Path(sys.argv[3])
subprocess.run([sys.executable, "build.py", "build"], cwd=root, check=True, stdout=subprocess.DEVNULL)
sys.path.insert(0, str(root / "tools"))
from vm import VM  # noqa: E402

STRETCH = re.compile(r"(alone, half a second|beside the group[^:]*): (\d+) rounds, (\d+) ticks of which (\d+) "
                     r"found this program running, (\d+) task switches")
CLASSES = ["above", "high"]

with open(out, "a", encoding="utf-8") as log:
    for cycle in range(cycles):
        order = CLASSES if cycle % 2 == 0 else CLASSES[::-1]
        for priority in order:
            os.environ["INTI_QEMU_PRIORITY"] = priority
            record = {"cycle": cycle, "priority": priority, "started": round(time.time(), 3), "runs": []}
            try:
                with VM() as vm:
                    vm.wait_for_prompt(timeout=120)
                    for _ in range(2):
                        began = time.time()
                        text = vm.command("selftest", timeout=400)
                        record["runs"].append({
                            "started": round(began, 3),
                            "ended": round(time.time(), 3),
                            "stretches": [[name, int(a), int(b), int(c), int(d)]
                                          for name, a, b, c, d in STRETCH.findall(text)],
                            "result": re.findall(r"SELFTEST: [^\r\n]*", text)[-1:],
                            "failed": re.findall(r"^\s*FAIL [^\r\n]*", text, re.M),
                        })
            except Exception as problem:  # a boot that never reaches the prompt is a result too
                record["error"] = repr(problem)[:300]
            record["ended"] = round(time.time(), 3)
            log.write(json.dumps(record) + "\n")
            log.flush()
print("done")
