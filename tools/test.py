"""Boot Inti in QEMU and put it through every test, over the serial port.

    python tools/test.py                 the full run (about five minutes)
    python tools/test.py --fuzz 20000    a short run while developing

It writes

    evidence/test_serial.log        everything the serial port produced (carriage returns removed)
    evidence/test_nonx_serial.log   the same for a second, short boot on a processor model
                                    without the no-execute bit
    evidence/test.json              every check with its result, and every measured number
    docs/img/qemu-*.png             the emulated screen at three moments

and exits with 0 only if every check passed. Nothing here is timed with QEMU tracing on.
"""
from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import platform
import re
import statistics
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))
from vm import VM, VMError  # noqa: E402  (vm.py also puts the project root on sys.path)
import build  # noqa: E402

EVIDENCE = ROOT / "evidence"
IMAGES = ROOT / "docs" / "img"
SERVICES = ("console", "fs")
# The supervisor counts a server that ends less than 2 s after it started towards its
# restart back-off. Faults that are timed are therefore injected into settled servers.
SETTLE_S = 2.6
BANNER = r"Inti (\S+) shell ready at t=(\d+) us"
# QEMU's plain 32-bit processor model has no no-execute bit. One short boot uses it, to
# record what the system says and does where W^X cannot be enforced.
CPU_WITHOUT_NX = "qemu32"
# What the serial log must show, in this order, when fs crashes while the supervisor is
# waiting for a hung console to answer a ping.
DOUBLE_DONE = r"DOUBLE FAILURE: fs answering again after (\d+) us, console after (\d+) us"
DOUBLE_ORDER = (
    r"init: fs crashed: page fault",
    r"init: noticed while waiting for console to answer a ping",
    r"init: fs restarted after \d+ us",
    r"init: console did not answer for \d+ ms; ending it",
    r"init: console ended: killed",
    r"init: console restarted after \d+ us",
    DOUBLE_DONE,
)
# What the guided tour must print, in this order (the shell's own lines only).
TOUR_ORDER = (
    r"^TOUR 1/5: ",
    r"ROGUE: (\d+)/\1 attempts denied, 1 skipped",
    r"^TOUR 2/5: ",
    r"RECOVERY: console crash -> answering again after \d+ us",
    r"^TOUR 3/5: ",
    r"RECOVERY: fs hang -> answering again after \d+ us",
    r"^TOUR 4/5: ",
    DOUBLE_DONE,
    r"^TOUR 5/5: ",
    r"\b0 kernel anomalies",
    r"kernel invariants: all hold",
    r"^TOUR: done",
)
ATTEMPT = r"^\s*(\d+)\. (.+?)\s+(denied: .+|NOT DENIED.*|skipped)\s*$"


class Run:
    """The machine under test plus the checks and numbers collected so far."""

    def __init__(self, vm: VM):
        self.vm = vm
        self.checks: list[dict] = []
        self.measured: dict = {}

    def check(self, name: str, ok: bool, detail: str = "") -> bool:
        self.checks.append({"name": name, "ok": bool(ok), "detail": detail})
        mark = "ok  " if ok else "FAIL"
        print(f"test: {mark} {name}" + (f"  [{detail}]" if detail else ""), flush=True)
        return bool(ok)

    def log_since(self, mark: int) -> str:
        return bytes(self.vm.raw[mark:]).decode("utf-8", "replace").replace("\r", "")


def spread(values: list[float]) -> dict:
    return {
        "min": min(values),
        "median": round(statistics.median(values)),
        "max": max(values),
        "runs": len(values),
        "samples": values,
    }


def boot(run: Run) -> None:
    text = run.vm.wait_for_prompt(timeout=20)
    host_ms = round((time.monotonic() - run.vm.started_at) * 1000)
    found = re.search(BANNER, text)
    run.check("boots from its own boot sector to the shell prompt within 20 s", found is not None,
              f"{host_ms} ms after QEMU started")
    run.measured["boot"] = {
        "host_ms": host_ms,
        "guest_us": int(found[2]) if found else None,
        "version": found[1] if found else None,
    }
    run.check("the kernel enforces W^X: its boot log says the no-execute bit is in use",
              "W^X enforced" in run.vm.log(), f"QEMU -cpu {build.QEMU_CPU}")


def selftest(run: Run, label: str) -> None:
    text = run.vm.command("selftest", timeout=90)
    found = re.search(r"SELFTEST: PASS (\d+)/(\d+)", text)
    failures = re.findall(r"^\s*FAIL (.+)$", text, re.M)
    skipped = re.findall(r"^\s*skip (.+)$", text, re.M)
    passed = (found is not None and found[1] == found[2] and int(found[2]) >= 30
              and not failures and not skipped)
    run.check(f"selftest {label}", passed, found[0] if found else text[-300:])
    if found:
        key = label.replace(" ", "_")
        run.measured.setdefault("selftest", {})[key] = int(found[2])
        run.measured["selftest"][f"{key}_skipped"] = len(skipped)
    if label == "after boot":
        written = "written through the Log capability" in run.vm.log()
        run.check("the selftest's line written through the Log capability is in the kernel log", written)


def rogue(run: Run) -> None:
    text = run.vm.command("rogue all", timeout=90)
    found = re.search(r"ROGUE: (\d+)/(\d+) attempts denied\n", text)
    attempts = re.findall(ATTEMPT, text, re.M)
    denied = [attempt for attempt in attempts if attempt[2].startswith("denied")]
    complete = found is not None and found[1] == found[2] and len(denied) == len(attempts) == int(found[2])
    run.check("rogue all: every attempt of the hostile program is denied", complete,
              found[0].strip() if found else text[-300:])
    run.check("rogue all: the kernel invariants hold afterwards", "kernel invariants: all hold" in text)
    run.vm.screenshot(IMAGES / "qemu-rogue.png")
    total = int(found[2]) if found else 0

    text = run.vm.command("rogue", timeout=90)
    found = re.search(r"ROGUE: (\d+)/(\d+) attempts denied, (\d+) skipped", text)
    browser_safe = found is not None and found[1] == found[2] and int(found[2]) + int(found[3]) == total
    run.check("rogue: the default run denies every attempt it makes and reports the one it skips",
              browser_safe and int(found[3]) == 1, found[0] if found else text[-300:])
    run.measured["rogue"] = {
        "all_denied": len(denied),
        "all_total": total,
        "default_denied": int(found[1]) if found else None,
        "default_skipped": int(found[3]) if found else None,
        "attempts": [f"{number}. {what}: {result}" for number, what, result in attempts],
    }
    still_up = "readme.txt" in run.vm.command("ls")
    run.check("the system is still up after the hostile program (ls answers)", still_up)


def inject(run: Run, verb: str, target: str) -> dict:
    """Break one server on purpose; time how long the shell waits until it answers again."""
    vm = run.vm
    mark = len(vm.raw)
    started = time.monotonic()
    vm.send(f"{verb} {target}\r")
    text = vm.read_until(rf"RECOVERY: {target} {verb} -> answering again after \d+ us", timeout=20)
    host_ms = round((time.monotonic() - started) * 1000)
    vm.wait_for_prompt()
    listing = vm.command("ls")
    log = run.log_since(mark)
    respawn = re.search(rf"init: {target} restarted after (\d+) us", log)
    if verb == "crash":
        noticed = re.search(rf"init: {target} crashed: page fault", log)
    else:
        noticed = re.search(rf"init: {target} did not answer for \d+ ms; ending it", log)
    return {
        "guest_us": int(re.findall(r"answering again after (\d+) us", text)[-1]),
        "host_ms": host_ms,
        "respawn_us": int(respawn[1]) if respawn else None,
        "answers": "readme.txt" in listing,
        "noticed": noticed is not None,
        "slowed": "times in a row" in log,
    }


def summarise(run: Run, name: str, what: str, samples: list[dict]) -> None:
    good = all(
        sample["answers"] and sample["noticed"] and sample["respawn_us"] is not None and not sample["slowed"]
        for sample in samples
    )
    guest = spread([sample["guest_us"] for sample in samples])
    run.check(f"{what}: restarted by the supervisor, shell and file listing answer, in {len(samples)} of "
              f"{len(samples)} runs", good, f"median {guest['median']} us, worst {guest['max']} us")
    run.measured.setdefault("recovery", {})[name] = {
        "guest_us": guest,
        "host_ms": spread([sample["host_ms"] for sample in samples]),
        "respawn_us": spread([sample["respawn_us"] or 0 for sample in samples]),
    }


def recovery(run: Run, runs: int) -> None:
    for verb in ("crash", "hang"):
        for target in SERVICES:
            samples = []
            for index in range(runs):
                time.sleep(SETTLE_S)
                samples.append(inject(run, verb, target))
                if verb == "crash" and target == "console" and index == 0:
                    run.vm.command("svc")
                    run.vm.screenshot(IMAGES / "qemu-recovery.png")
            summarise(run, f"{target}_{verb}", f"{target} {verb}", samples)
    # The slowest case: a server that hangs right after it was restarted, before the
    # supervisor has started pinging it.
    samples = []
    for _ in range(min(runs, 3)):
        time.sleep(SETTLE_S)
        inject(run, "crash", "fs")
        samples.append(inject(run, "hang", "fs"))
    summarise(run, "fs_hang_just_restarted", "fs hang right after a restart", samples)


def in_order(patterns: tuple[str, ...], text: str) -> str | None:
    """The first pattern that is not found after the one before it, or None if all are."""
    position = 0
    for pattern in patterns:
        found = re.compile(pattern, re.M).search(text, position)
        if found is None:
            return pattern
        position = found.end()
    return None


def double_failure(run: Run, runs: int) -> None:
    """Two servers fail at once: fs crashes while the supervisor waits for a hung console."""
    vm = run.vm
    samples = []
    for _ in range(runs):
        time.sleep(SETTLE_S)
        mark = len(vm.raw)
        vm.send("double\r")
        text = vm.read_until(DOUBLE_DONE, timeout=20)
        vm.wait_for_prompt()
        listing = vm.command("ls")
        log = run.log_since(mark)
        times = re.findall(DOUBLE_DONE, text)[-1]
        samples.append({
            "fs_us": int(times[0]),
            "console_us": int(times[1]),
            "missing": in_order(DOUBLE_ORDER, log),
            "answers": "readme.txt" in listing,
            "slowed": "times in a row" in log,
        })
    good = all(sample["missing"] is None and sample["answers"] and not sample["slowed"] for sample in samples)
    fs_us = spread([sample["fs_us"] for sample in samples])
    console_us = spread([sample["console_us"] for sample in samples])
    detail = f"median fs {fs_us['median']} us, console {console_us['median']} us"
    missing = sorted({sample["missing"] for sample in samples if sample["missing"]})
    if missing:
        detail += "; not found in order: " + "; ".join(missing)
    run.check("double failure: fs crashes while the supervisor waits for a hung console; the supervisor "
              "reports the crash during that wait, restarts both, and the shell and file listing answer, "
              f"in {len(samples)} of {len(samples)} runs", good, detail)
    run.measured["double"] = {"fs_us": fs_us, "console_us": console_us}


def tour(run: Run) -> None:
    """The guided tour: five announced steps, ending in the shell that started it."""
    vm = run.vm
    time.sleep(SETTLE_S)
    mark = len(vm.raw)
    started = time.monotonic()
    vm.send("tour\r")
    text = vm.read_until(r"TOUR: done[^\n]*\n", timeout=90)
    host_ms = round((time.monotonic() - started) * 1000)
    vm.wait_for_prompt()
    listing = vm.command("ls")
    missing = in_order(TOUR_ORDER, text)
    steps = len(re.findall(r"^TOUR \d+/\d+: ", text, re.M))
    good = missing is None and "times in a row" not in run.log_since(mark) and "readme.txt" in listing
    run.check("tour: the guided tour announces and runs its steps in order (hostile program, crashed "
              "driver, hung driver, two failures at once, counters) and ends in the shell that started it",
              good, f"{steps} steps in {host_ms} ms" + (f"; not found in order: {missing}" if missing else ""))
    run.measured["tour"] = {"steps": steps, "host_ms": host_ms}


def backoff(run: Run) -> None:
    """A server that keeps dying is restarted more slowly, and the system stays usable."""
    vm = run.vm
    time.sleep(SETTLE_S)
    mark = len(vm.raw)
    for _ in range(5):
        vm.command("crash fs", timeout=20)
    log = run.log_since(mark)
    slowed = re.findall(r"init: fs ended (\d+) times in a row; next restart in (\d+) ms", log)
    listing = vm.command("ls")
    services = vm.command("svc")
    run.check("a server that dies again and again is restarted with a growing delay and still comes back",
              len(slowed) >= 2 and "readme.txt" in listing,
              "; ".join(f"ended {count} times in a row -> {delay} ms" for count, delay in slowed))
    run.measured["backoff"] = {
        "delays_ms": [int(delay) for _, delay in slowed],
        "svc": services.strip().splitlines(),
    }
    time.sleep(SETTLE_S)


def supervisor_crash(run: Run) -> None:
    vm = run.vm
    mark = len(vm.raw)
    started = time.monotonic()
    vm.send("crash init\r")
    text = vm.read_until(BANNER, timeout=20)
    host_ms = round((time.monotonic() - started) * 1000)
    vm.wait_for_prompt()
    restarted = "[user space restarted]" in run.log_since(mark)
    listing = vm.command("ls")
    run.check("crash init: the kernel starts user space again and the new shell answers",
              restarted and "readme.txt" in listing, f"new prompt {host_ms} ms later (host time)")
    run.measured["init_crash"] = {"host_ms": host_ms, "banner": re.search(BANNER, text)[0]}
    time.sleep(SETTLE_S)


def fuzz(run: Run, count: int) -> None:
    vm = run.vm
    mark = len(vm.raw)
    started = time.monotonic()
    text = vm.command(f"fuzz {count}", timeout=max(120.0, count / 2000))
    host_s = time.monotonic() - started
    banner = re.search(r"Fuzzing the kernel with (\d+) random system calls, seed (\d+)\.", run.log_since(mark))
    calls = re.search(r"FUZZ: (\d+) random system calls from (\d+) tasks in (\d+) ms", text)
    health = re.search(r"FUZZ: (\d+) invariant checks, (\d+) violations, (\d+) kernel anomalies, (\d+) leaks", text)
    ended = re.search(r"FUZZ: tasks ended: (\d+) exited, (\d+) were stopped, (\d+) hit a CPU exception", text)
    parsed = banner is not None and calls is not None and health is not None and ended is not None
    run.check("fuzz: the report is complete", parsed, text[-300:] if not parsed else "")
    if not parsed:
        return
    clean = all(int(health[index]) == 0 for index in (2, 3, 4)) and "FUZZ: PASS" in text
    run.check(f"fuzz: at least {count} random system calls, 0 invariant violations, 0 kernel anomalies, 0 leaks",
              clean and int(calls[1]) >= count, f"{calls[0]}; {health[0]}")
    table = text[text.find("Calls the tasks reported"):text.find("FUZZ: PASS")].strip().splitlines()
    run.measured["fuzz"] = {
        "requested": count,
        "seed": int(banner[2]),
        "calls": int(calls[1]),
        "tasks": int(calls[2]),
        "guest_ms": int(calls[3]),
        "host_ms": round(host_s * 1000),
        "calls_per_second": round(int(calls[1]) * 1000 / max(1, int(calls[3]))),
        "checks": int(health[1]),
        "violations": int(health[2]),
        "anomalies": int(health[3]),
        "leaks": int(health[4]),
        "exited": int(ended[1]),
        "stopped": int(ended[2]),
        "exceptions": int(ended[3]),
        "per_call": table,
    }
    vm.screenshot(IMAGES / "qemu-fuzz.png")


def bench(run: Run) -> None:
    text = run.vm.command("bench", timeout=180)
    results = {name: int(value) for name, value in re.findall(r"BENCH: (.+?): (\d+) ns", text)}
    run.check("bench runs to the end", "BENCH: done" in text and len(results) > 0, f"{len(results)} results")
    run.measured["bench_ns"] = results


def keyboard(run: Run) -> None:
    run.vm.press("u", "p", "t", "i", "m", "e", "ret")
    text = run.vm.wait_for_prompt(timeout=15)
    run.check("keys pressed on the emulated PS/2 keyboard reach the shell", "kernel invariants: all hold" in text)


def uptime(vm: VM) -> tuple[float, float, str]:
    text = vm.command("uptime")
    return float(re.search(r"up (\d+\.\d+) s", text)[1]), time.monotonic(), text


def clock(run: Run) -> None:
    guest_before, host_before, _ = uptime(run.vm)
    time.sleep(5)
    guest_after, host_after, _ = uptime(run.vm)
    ratio = (guest_after - guest_before) / (host_after - host_before)
    run.check("the guest clock keeps time against the host clock over 5 idle seconds", 0.95 <= ratio <= 1.02,
              f"guest/host = {ratio:.4f}")
    run.measured["clock"] = {"idle_ratio": round(ratio, 4)}
    fuzzed = run.measured.get("fuzz")
    if fuzzed:
        run.measured["clock"]["fuzz_guest_over_host"] = round(fuzzed["guest_ms"] / fuzzed["host_ms"], 4)


def counters(run: Run) -> None:
    _, _, text = uptime(run.vm)
    numbers = {
        key: int(value)
        for value, key in re.findall(
            r"(\d+) (timer interrupts|tasks started|tasks|endpoints|system calls|task switches|"
            r"device interrupts|faults contained|kernel anomalies)", text)
    }
    healthy = numbers.get("kernel anomalies") == 0 and "kernel invariants: all hold" in text
    run.check("at the end: 0 kernel anomalies and every kernel invariant holds", healthy,
              f"{numbers.get('system calls')} system calls, {numbers.get('faults contained')} faults contained, "
              f"{numbers.get('tasks started')} tasks started")
    run.measured["counters"] = {key.replace(" ", "_"): value for key, value in numbers.items()}


def shutdown(run: Run) -> None:
    vm = run.vm
    vm.send("shutdown\r")
    deadline = time.monotonic() + 15
    while vm.process.poll() is None and time.monotonic() < deadline:
        vm.pump()
    vm.pump(0.2)
    code = vm.process.poll()
    run.check("shutdown switches the machine off (QEMU exits with code 0)", code == 0, f"exit code {code}")


def without_nx(run: Run) -> str:
    """Boot the same image on a processor model without the no-execute bit; return its serial log.

    W^X cannot be enforced there. The kernel has to say so, the selftest has to report its
    execute tests as skipped and not as passed, and the hostile program has to report its
    execute attempts as not denied.
    """
    where = "without the no-execute bit"
    vm = VM(extra=["-cpu", CPU_WITHOUT_NX])
    vm.start()
    try:
        vm.wait_for_prompt(timeout=20)
        run.check(f"{where}: the kernel boots and says that W^X is not enforced",
                  "W^X NOT enforced" in vm.log(), f"QEMU -cpu {CPU_WITHOUT_NX}")

        text = vm.command("selftest", timeout=90)
        found = re.search(r"SELFTEST: PASS (\d+)/(\d+), (\d+) skipped", text)
        skipped = re.findall(r"^\s*skip (.+)$", text, re.M)
        failures = re.findall(r"^\s*FAIL (.+)$", text, re.M)
        honest = (found is not None and found[1] == found[2] and int(found[3]) == len(skipped) > 0
                  and all("cannot run code from" in name for name in skipped) and not failures)
        run.check(f"{where}: the selftest passes and reports each execute test as skipped, not as passed",
                  honest, found[0] if found else text[-300:])

        text = vm.command("rogue all", timeout=90)
        total = re.search(r"ROGUE: (\d+)/(\d+) attempts denied\n", text)
        attempts = re.findall(ATTEMPT, text, re.M)
        denied = [attempt for attempt in attempts if attempt[2].startswith("denied")]
        not_denied = [attempt for attempt in attempts if not attempt[2].startswith("denied")]
        plain = (total is not None and len(attempts) == int(total[2]) and len(denied) == int(total[1])
                 and len(not_denied) > 0
                 and all(what.startswith("run code from") and result == "NOT DENIED: no no-execute bit"
                         for _, what, result in not_denied))
        run.check(f"{where}: the hostile program reports each execute attempt as not denied and every "
                  "other attempt as denied", plain, total[0].strip() if total else text[-300:])
        run.check(f"{where}: the kernel invariants hold after the hostile program",
                  "kernel invariants: all hold" in text)
        run.check(f"{where}: the system is still up (ls answers)", "readme.txt" in vm.command("ls"))
        run.measured["no_nx"] = {
            "selftest_passed": int(found[2]) if found else None,
            "selftest_skipped": len(skipped),
            "rogue_denied": len(denied),
            "rogue_total": len(attempts),
            "rogue_not_denied": len(not_denied),
            "not_denied": [f"{number}. {what}: {result}" for number, what, result in not_denied],
        }
    except VMError as error:
        run.check(f"{where}: the run reached its end", False, str(error)[-600:])
    finally:
        vm.close()
    log = vm.log().replace("\r", "")
    run.check(f"{where}: no kernel panic anywhere in the serial log", "KERNEL PANIC" not in log)
    return log


def qemu_version() -> str:
    answer = subprocess.run([build.find_qemu(), "--version"], capture_output=True, text=True, check=False)
    found = re.search(r"version (\S+)", answer.stdout)
    return found[1] if found else "unknown"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--fuzz", type=int, default=1_000_000, help="random system calls to ask for")
    parser.add_argument("--runs", type=int, default=5, help="times each fault is injected")
    options = parser.parse_args()

    began = time.monotonic()
    IMAGES.mkdir(parents=True, exist_ok=True)
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    image_sha256 = hashlib.sha256((build.BUILD / "inti.img").read_bytes()).hexdigest()
    vm = VM()
    run = Run(vm)
    vm.start()
    try:
        boot(run)
        selftest(run, "after boot")
        rogue(run)
        recovery(run, options.runs)
        double_failure(run, options.runs)
        tour(run)
        backoff(run)
        supervisor_crash(run)
        fuzz(run, options.fuzz)
        bench(run)
        selftest(run, "after all faults")
        keyboard(run)
        clock(run)
        counters(run)
        shutdown(run)
    except VMError as error:
        run.check("the run reached its end", False, str(error)[-600:])
    finally:
        vm.close()
    log = vm.log().replace("\r", "")
    run.check("no kernel panic anywhere in the serial log", "KERNEL PANIC" not in log)
    log_without_nx = without_nx(run)

    passed = all(check["ok"] for check in run.checks)
    (EVIDENCE / "test_serial.log").write_text(log, encoding="utf-8", newline="\n")
    (EVIDENCE / "test_nonx_serial.log").write_text(log_without_nx, encoding="utf-8", newline="\n")
    report = {
        "passed": passed,
        "date": datetime.datetime.now().astimezone().isoformat(timespec="seconds"),
        "duration_s": round(time.monotonic() - began),
        "emulator": f"QEMU {qemu_version()} (TCG, no hardware virtualisation)",
        "qemu_version": qemu_version(),
        "cpu": build.QEMU_CPU,
        "cpu_without_nx": CPU_WITHOUT_NX,
        "host": f"{platform.system()} {platform.release()}",
        "image_sha256": image_sha256,
        "options": {"fuzz": options.fuzz, "runs": options.runs},
        "checks": run.checks,
        "measured": run.measured,
    }
    (EVIDENCE / "test.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8", newline="\n")
    failed = [check["name"] for check in run.checks if not check["ok"]]
    print(f"test: {'PASS' if passed else 'FAIL'} {len(run.checks) - len(failed)}/{len(run.checks)} checks, "
          f"{report['duration_s']} s", flush=True)
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
