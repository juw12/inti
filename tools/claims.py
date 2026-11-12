"""Collect every number the documents show, and render the documents from their templates.

    python tools/claims.py

Every claim is computed from files that are in the repository: the test evidence
(evidence/test.json, evidence/test_serial.log, evidence/web.json, evidence/gates.json) and
the source code. The claims go to evidence/claims.json with the file each one came from.

The documents are templates in docs/src/. A placeholder {{key}} is replaced by the value of
the claim `key` (whole numbers get thousands separators), {{key:raw}} by the value as it is.
A placeholder without a claim is an error. tools/check_claims.py repeats all of this and
compares the result with what is on disk.
"""
from __future__ import annotations

import hashlib
import json
import re
import sys
from pathlib import Path
from typing import Callable

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))
import gates  # noqa: E402

TEMPLATES = {
    "docs/src/README.md": "README.md",
    "docs/src/design.md": "docs/design.md",
    "docs/src/weaknesses.md": "docs/weaknesses.md",
    "docs/src/scorecard.md": "docs/scorecard.md",
}
CLAIMS = gates.EVIDENCE / "claims.json"
SERIAL_LOG = gates.EVIDENCE / "test_serial.log"
NONX_SERIAL_LOG = gates.EVIDENCE / "test_nonx_serial.log"  # the same image on a processor without NX
PLACEHOLDER = re.compile(r"\{\{([a-z0-9_.]+)(:raw)?\}\}")
ELLIPSIS = "..."  # stands for lines left out of a log excerpt

TEST = "evidence/test.json"
WEB = "evidence/web.json"
GATES = "evidence/gates.json"
LOG = "evidence/test_serial.log"
NONX_LOG = "evidence/test_nonx_serial.log"
SOURCES = "evidence/sources.json"
MODEL = "evidence/model.json"
MODEL_LOG = "evidence/model.log"
MODEL_BEFORE = "evidence/model-before-fixes.log"
MODEL_MAIN = "model/src/main.rs"
MUTANTS = "evidence/mutants.json"
MUTANTS_BEFORE = "evidence/mutants-before-tests.log"
MUTANTS_HELD_OUT = "evidence/mutants-held-out.json"

FAULTS = {
    "console_crash": "console driver crashes (page fault)",
    "fs_crash": "file server crashes (page fault)",
    "console_hang": "console driver hangs (stops answering)",
    "fs_hang": "file server hangs (stops answering)",
    "fs_hang_just_restarted": "file server hangs right after a restart",
}
BENCH = {
    "system call entry and return": "syscall",
    "read the clock": "clock",
    "IPC call and reply": "ipc",
    "start, run and reap a task": "spawn",
    "map and unmap one page": "map",
    "check every kernel invariant": "check",
}
# Attempts shown in the short excerpt.
ROGUE_SAMPLE = (1, 2, 3, 4, 5, 9, 11, 15, 20, 24, 25, 26, 27, 28, 31, 35, 36, 40, 46, 49, 50, 51)

Put = Callable[[str, object, str], None]


# ---- small helpers --------------------------------------------------------------------------

def source_text(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def constant(path: str, name: str) -> int:
    """The value of `const NAME: type = <integer literal>;` in a Rust file."""
    found = re.search(rf"\bconst {name}: \w+ = (0x[0-9A-Fa-f_]+|[0-9_]+);", source_text(path))
    if not found:
        raise KeyError(f"{path}: no integer constant named {name}")
    return int(found.group(1).replace("_", ""), 0)


def pattern_numbers(path: str, pattern: str) -> tuple[int, ...]:
    """The integers captured by `pattern` at its one match in a source file."""
    found = re.search(pattern, source_text(path))
    if not found:
        raise KeyError(f"{path}: nothing matches {pattern}")
    return tuple(int(group.replace("_", ""), 0) for group in found.groups())


def milliseconds(microseconds: float) -> float | int:
    """Microseconds as milliseconds: one decimal below 100 ms, whole milliseconds above."""
    value = microseconds / 1000
    return round(value) if value >= 100 else round(value, 1)


def show(value: object, raw: bool = False) -> str:
    if isinstance(value, bool):
        return "yes" if value else "no"
    if isinstance(value, int) and not raw:
        return f"{value:,}"
    return str(value)


def table(header: list[str], rows: list[list[object]]) -> str:
    lines = ["| " + " | ".join(header) + " |", "|" + "---|" * len(header)]
    lines += ["| " + " | ".join(show(cell) for cell in row) + " |" for row in rows]
    return "\n".join(lines)


def block(lines: list[str], start: str, end: str, occurrence: int = 0) -> list[str]:
    """The lines from a line matching `start` to the next line matching `end`, both included.

    `occurrence` picks which match of `start` to use (0 is the first, -1 the last).
    """
    starts = [index for index, line in enumerate(lines) if re.search(start, line)]
    if not starts:
        raise KeyError(f"no log line matches {start}")
    first = starts[occurrence]
    for last in range(first + 1, len(lines)):
        if re.search(end, lines[last]):
            return lines[first:last + 1]
    raise KeyError(f"no log line matches {end} after {start}")


def shorten(lines: list[str], keep: Callable[[str], bool]) -> list[str]:
    """The lines that `keep` accepts; each run of dropped lines becomes one `...` line."""
    result: list[str] = []
    for line in lines:
        if keep(line):
            result.append(line)
        elif not result or result[-1] != ELLIPSIS:
            result.append(ELLIPSIS)
    return result


def python_lines() -> int:
    """Lines of the build and test scripts that are neither blank nor only a comment."""
    paths = [ROOT / "build.py", *sorted((ROOT / "tools").glob("*.py"))]
    return sum(
        1
        for path in paths
        for line in path.read_text(encoding="utf-8").splitlines()
        if line.strip() and not line.strip().startswith("#")
    )


def system_calls() -> list[tuple[int, str, str]]:
    """(number, name, what it does) for every system call, from the `nr` module of the ABI."""
    module = re.search(r"pub mod nr \{(.*?)\n\}", source_text("abi/src/lib.rs"), re.DOTALL)
    if not module:
        raise KeyError("abi/src/lib.rs: no module nr")
    calls = re.findall(r"pub const (\w+): u32 = (\d+); // (.+)", module.group(1))
    return [(int(number), name.lower(), text.strip()) for name, number, text in calls]


def error_codes() -> list[tuple[int, str, str]]:
    codes = re.findall(r"pub const (E_\w+): i32 = (-\d+); // (.+)", source_text("abi/src/lib.rs"))
    return [(int(number), name, text.strip()) for name, number, text in codes]


# ---- the claims -----------------------------------------------------------------------------

def add_test(put: Put, test: dict) -> None:
    measured = test["measured"]
    put("test.date", test["date"][:10], TEST)
    put("test.emulator", test["emulator"], TEST)
    put("test.host", test["host"], TEST)
    put("test.duration_s", test["duration_s"], TEST)
    put("test.checks_total", len(test["checks"]), TEST)
    put("test.checks_passed", sum(1 for check in test["checks"] if check["ok"]), TEST)
    put("test.fault_runs", test["options"]["runs"], TEST)
    put("version", measured["boot"]["version"], TEST)
    put("boot.host_ms", measured["boot"]["host_ms"], TEST)
    put("boot.guest_ms", milliseconds(measured["boot"]["guest_us"]), TEST)
    put("selftest.checks", measured["selftest"]["after_boot"], TEST)
    put("selftest.after_faults", measured["selftest"]["after_all_faults"], TEST)

    rogue = measured["rogue"]
    reasons = [line.split("denied: ", 1)[1] for line in rogue["attempts"] if "denied: " in line]
    for key in ("all_denied", "all_total", "default_denied", "default_skipped"):
        put(f"rogue.{key}", rogue[key], TEST)
    put("rogue.by_kernel", sum(1 for reason in reasons if reason.startswith("E_")), TEST)
    put("rogue.by_cpu", sum(1 for reason in reasons if "fault" in reason), TEST)
    put("rogue.by_driver", sum(1 for reason in reasons if "driver" in reason), TEST)
    put("rogue.ignored", sum(1 for reason in reasons if "ignored" in reason), TEST)
    put("rogue.software", sum(1 for reason in reasons if "fault" not in reason), TEST)

    for case, times in measured["recovery"].items():
        guest, host, respawn = times["guest_us"], times["host_ms"], times["respawn_us"]
        put(f"recovery.{case}.median_ms", milliseconds(guest["median"]), TEST)
        put(f"recovery.{case}.min_ms", milliseconds(guest["min"]), TEST)
        put(f"recovery.{case}.max_ms", milliseconds(guest["max"]), TEST)
        put(f"recovery.{case}.runs", guest["runs"], TEST)
        put(f"recovery.{case}.host_median_ms", round(host["median"]), TEST)
        put(f"recovery.{case}.respawn_median_ms", milliseconds(respawn["median"]), TEST)
    delays = measured["backoff"]["delays_ms"]
    put("backoff.third_ms", delays[0], TEST)
    put("backoff.fourth_ms", delays[1], TEST)
    put("init_crash.host_ms", measured["init_crash"]["host_ms"], TEST)

    double = measured["double"]
    for server in ("fs", "console"):
        times = double[f"{server}_us"]
        put(f"double.{server}_ms", milliseconds(times["median"]), TEST)
        put(f"double.{server}_min_ms", milliseconds(times["min"]), TEST)
        put(f"double.{server}_max_ms", milliseconds(times["max"]), TEST)
    put("double.runs", double["fs_us"]["runs"], TEST)
    put("tour.steps", measured["tour"]["steps"], TEST)
    put("tour.seconds", round(measured["tour"]["host_ms"] / 1000, 1), TEST)
    for key in ("selftest_passed", "selftest_skipped", "rogue_denied", "rogue_total", "rogue_not_denied"):
        put(f"nonx.{key}", measured["no_nx"][key], TEST)
    put("test.cpu", test["cpu"], TEST)
    put("test.cpu_without_nx", test["cpu_without_nx"], TEST)

    fuzz = measured["fuzz"]
    for key in ("calls", "requested", "tasks", "calls_per_second", "checks", "violations", "anomalies",
                "leaks", "exited", "stopped", "exceptions", "seed"):
        put(f"fuzz.{key}", fuzz[key], TEST)
    put("fuzz.seconds", round(fuzz["guest_ms"] / 1000, 1), TEST)

    for name, short in BENCH.items():
        nanoseconds = measured["bench_ns"][name]
        put(f"bench.{short}_ns", nanoseconds, TEST)
        put(f"bench.{short}_us", milliseconds(nanoseconds), TEST)
    put("clock.idle_ratio", measured["clock"]["idle_ratio"], TEST)
    put("clock.fuzz_ratio", measured["clock"]["fuzz_guest_over_host"], TEST)
    for key, value in measured["counters"].items():
        put(f"counters.{key}", value, TEST)


def add_web(put: Put, web: dict) -> None:
    commands = web["commands"]
    put("web.boot_seconds", web["boot_seconds"], WEB)
    put("web.emulator", web["emulator"], WEB)
    put("web.browser", web["browser"], WEB)
    put("web.commands", len(commands), WEB)
    put("web.commands_ok", sum(1 for entry in commands.values() if entry["ok"]), WEB)
    put("web.command_list", ", ".join(f"`{name}`" for name in commands), WEB)
    put("web.tour_seconds", commands["tour"]["seconds"], WEB)


def add_gates(put: Put, result: dict) -> None:
    tries = result.get("tries", {})
    put("gates.passed", result["passed"], GATES)
    put("gates.total", result["total"], GATES)
    put("gates.date", result["date"][:10], GATES)
    put("gates.mode", result["mode"], GATES)
    put("gates.build_seconds", result.get("build_seconds", "not measured (quick run)"), GATES)
    put("gates.rustc", result["toolchain"]["rustc"], GATES)
    put("gates.python", result["toolchain"]["python"], GATES)
    put("gates.qemu_tries", len(tries["qemu_test"]) if tries else "not run", GATES)
    put("gates.browser_tries", len(tries["browser_check"]) if tries else "not run", GATES)

    sizes = result["sizes"]
    files = sizes["files"]
    put("size.boot_sector_bytes", sizes["boot_sector_bytes"], GATES)
    put("size.kernel_bytes", sizes["kernel_bytes"], GATES)
    put("size.kernel_kib", round(sizes["kernel_bytes"] / 1024, 1), GATES)
    put("size.archive_kib", round(sizes["archive_bytes"] / 1024, 1), GATES)
    put("size.payload_kib", round(sizes["payload_bytes"] / 1024, 1), GATES)
    put("size.image_mib", round(sizes["image_bytes"] / 2**20, 2), GATES)
    put("size.programs", sum(1 for name in files if "." not in name), GATES)
    put("size.files", len(files), GATES)


def add_source(put: Put) -> None:
    abi, init, fuzz = "abi/src/lib.rs", "user/src/bin/init.rs", "user/src/bin/fuzz.rs"
    put("limits.tasks", constant(abi, "MAX_TASKS"), abi)
    put("limits.endpoints", constant(abi, "MAX_ENDPOINTS"), abi)
    put("limits.cap_slots", constant(abi, "CAP_SLOTS"), abi)
    put("limits.spawn_caps", constant(abi, "SPAWN_CAPS"), abi)
    put("limits.msg_data", constant(abi, "MSG_DATA"), abi)
    put("limits.page_bytes", constant(abi, "PAGE"), abi)
    put("limits.irq_lines", constant("kernel/src/task.rs", "IRQ_LINES"), "kernel/src/task.rs")
    (max_ram_mib,) = pattern_numbers("kernel/src/mem.rs", r"const MAX_RAM: u32 = (\d+) << 20;")
    put("limits.max_ram_mib", max_ram_mib, "kernel/src/mem.rs")
    put("count.cap_types", len(re.findall(r"(?m)^pub const CAP_(?!NULL)\w+: u32 = \d+;", source_text(abi))), abi)
    put("build.python_min", re.search(r"Python (\d+\.\d+) or newer", source_text("build.py")).group(1), "build.py")

    put("supervisor.poll_ms", constant(init, "POLL_MS"), init)
    put("supervisor.ping_every_ms", constant(init, "PING_EVERY_US") // 1000, init)
    put("supervisor.ping_timeout_ms", constant(init, "PING_TIMEOUT_MS"), init)
    put("supervisor.ping_slice_ms", constant(init, "PING_SLICE_MS"), init)
    put("supervisor.grace_ms", constant(init, "GRACE_US") // 1000, init)
    put("supervisor.quick_end_s", constant(init, "QUICK_END_US") // 1_000_000, init)
    put("supervisor.retry_ms", constant(init, "RETRY_US") // 1000, init)
    step, below, cap = pattern_numbers(init, r"\((\d+) \* \(service\.quick_ends - (\d+)\)\)\.min\((\d+)\)")
    put("supervisor.backoff_step_ms", step, init)
    put("supervisor.backoff_from", below + 1, init)
    put("supervisor.backoff_cap_ms", cap, init)
    put("supervisor.hang_worst_ms", constant(init, "PING_EVERY_US") // 1000 + constant(init, "PING_TIMEOUT_MS")
        + constant(init, "POLL_MS"), init)
    put("kernel.root_restart_gap_s", constant("kernel/src/task.rs", "ROOT_MIN_US") // 1_000_000, "kernel/src/task.rs")
    put("shares.window_ticks", constant("kernel/src/task.rs", "CPU_WINDOW_TICKS"), "kernel/src/task.rs")
    put("shares.shell", constant(init, "SHELL_SHARE"), init)
    (full,) = pattern_numbers("kernel/src/task.rs", r"cpu_limit: (\d+),")
    put("shares.full", full, "kernel/src/task.rs")
    (spawn_word,) = pattern_numbers("kernel/src/sys.rs", r"let cpu_limit = match word\((\d+)\)")
    put("shares.spawn_word", spawn_word, "kernel/src/sys.rs")
    put("console.report_ms", constant("user/src/client.rs", "REPORT_WAIT_MS"), "user/src/client.rs")
    put("fuzz.kid_life_ms", constant(fuzz, "KID_LIFE_US") // 1000, fuzz)
    put("fuzz.check_every_ms", constant(fuzz, "CHECK_EVERY_US") // 1000, fuzz)
    put("fuzz.default_calls", constant(fuzz, "DEFAULT_CALLS"), fuzz)
    put("fuzz.kids", constant(fuzz, "PLACES"), fuzz)
    put("supervisor.services", constant(init, "SERVICES"), init)
    put("supervisor.server_pages", constant(init, "SERVER_PAGES"), init)

    arch, frame = "kernel/src/arch.rs", "kernel/src/frame.rs"
    clock_hz = constant(frame, "CLOCK_HZ")
    (timer_hz,) = pattern_numbers(arch, r"const PIT_RELOAD: u32 = \(CLOCK_HZ / (\d+)\)")
    put("clock.hz", clock_hz, frame)
    put("clock.resolution_ns", round(1e9 / clock_hz), frame)
    put("clock.wrap_ms", round(65536 / clock_hz * 1000, 1), frame)
    put("timer.hz", timer_hz, arch)
    put("timer.period_ms", 1000 // timer_hz, arch)

    check = source_text("kernel/src/check.rs")
    rules = {int(number) for number in re.findall(r"(?:Err\(|ok_or\()(\d+)", check)}
    put("count.invariant_rules", len(rules), "kernel/src/check.rs")
    put("count.syscalls", len(system_calls()), abi)
    put("count.errors", len(error_codes()), abi)
    put("count.kernel_files", len(gates.rust_files(("kernel/src",))), "kernel/src")
    put("count.unsafe_total", gates.unsafe_report(gates.ALL_SOURCES)[0], "kernel/src, abi/src, boot/src, user/src")
    put("count.unsafe_kernel", gates.unsafe_report(gates.KERNEL_SOURCES)[0], "kernel/src, abi/src")

    put("loc.kernel", gates.code_lines(("kernel/src",)), "kernel/src")
    put("loc.abi", gates.code_lines(("abi/src",)), "abi/src")
    put("loc.kernel_total", gates.code_lines(gates.KERNEL_SOURCES), "kernel/src, abi/src")
    put("loc.limit", gates.KERNEL_LINE_LIMIT, "tools/gates.py")
    put("loc.boot", gates.code_lines(("boot/src",)), "boot/src")
    put("loc.user", gates.code_lines(("user/src",)), "user/src")
    put("loc.tools", python_lines(), "build.py, tools/*.py")


def add_tables(put: Put, test: dict, result: dict) -> None:
    measured = test["measured"]
    put("table.gates", table(
        ["Gate", "Requirement", "Result", "Measured"],
        [[gate["id"], gate["title"], "pass" if gate["ok"] else "FAIL", gate["detail"]] for gate in result["gates"]],
    ), GATES)
    put("table.checks", table(
        ["Check", "Result", "Measured"],
        [[check["name"], "pass" if check["ok"] else "FAIL", check.get("detail", "")] for check in test["checks"]],
    ), TEST)

    rows = []
    for case, label in FAULTS.items():
        times = measured["recovery"][case]
        guest = times["guest_us"]
        rows.append([
            label,
            guest["runs"],
            f"{milliseconds(guest['median'])} ms",
            f"{milliseconds(guest['min'])} to {milliseconds(guest['max'])} ms",
            f"{round(times['host_ms']['median'])} ms",
            f"{milliseconds(times['respawn_us']['median'])} ms",
        ])
    put("table.recovery", table(
        ["Injected fault", "Runs", "Median until the service answers again", "Range",
         "Host stopwatch, median", "Supervisor restart, median"],
        rows,
    ), TEST)
    put("table.bench", table(
        ["Operation", "Time"],
        [[name, f"{measured['bench_ns'][name]:,} ns"] for name in BENCH],
    ), TEST)

    sizes = result["sizes"]
    rows = [["boot sector", sizes["boot_sector_bytes"]], ["kernel", sizes["kernel_bytes"]]]
    rows += [[f"`{name}`", size] for name, size in sizes["files"].items()]
    rows += [["disk image, padded", sizes["image_bytes"]]]
    put("table.sizes", table(["Part of the disk image", "Bytes"], rows), GATES)

    rows = []
    for path in gates.rust_files(gates.KERNEL_SOURCES):
        name = path.relative_to(ROOT).as_posix()
        rows.append([f"`{name}`", gates.count_code_lines(path)])
    rows.append(["total", gates.code_lines(gates.KERNEL_SOURCES)])
    put("table.loc", table(["Kernel source file", "Lines of code"], rows), "kernel/src, abi/src")

    put("table.syscalls", table(
        ["Number", "Call", "What it does"],
        [[number, f"`{name}`", text] for number, name, text in system_calls()],
    ), "abi/src/lib.rs")
    put("table.errors", table(
        ["Code", "Name", "Meaning"],
        [[number, f"`{name}`", text] for number, name, text in error_codes()],
    ), "abi/src/lib.rs")


def add_logs(put: Put, log: list[str], web: dict, nonx: list[str]) -> None:
    def put_log(key: str, lines: list[str], source: str = LOG) -> None:
        put(key, "\n".join(lines), source)

    put_log("log.boot", block(log, r"^Inti \S+ kernel$", r"^Type help"))
    selftest = block(log, r"^inti> selftest$", r"^SELFTEST:")
    put_log("log.selftest", selftest)
    put_log("log.selftest_short", selftest[:6] + [ELLIPSIS] + selftest[-3:])

    # The share measurement is printed by every self-test run: the first is after boot, the
    # last after all the injected faults. The figures are whatever the log says.
    measured = re.findall(
        r"kept (\d+)% of its speed when they shared (\d+)% of the processor, and (\d+)% when they had "
        r"no share of their own; the kernel charged the group (\d+)% and (\d+)% of a second",
        " ".join(" ".join(log).split()),
    )
    if len(measured) < 2:
        raise KeyError(f"{LOG}: the share measurement appears {len(measured)} times, expected at least 2")
    for run, figures in (("boot", measured[0]), ("faults", measured[-1])):
        kept_held, limit, kept_free, charged_held, charged_free = (int(figure) for figure in figures)
        put(f"shares.{run}.limit", limit, LOG)
        put(f"shares.{run}.kept_held", kept_held, LOG)
        put(f"shares.{run}.kept_free", kept_free, LOG)
        put(f"shares.{run}.charged_held", charged_held, LOG)
        put(f"shares.{run}.charged_free", charged_free, LOG)
    put_log("log.shares", block(log, r"^  ok   a child gets its parent's share", r"^  ok   three spinning tasks"))

    def sampled(line: str) -> bool:
        attempt = re.match(r"\s*(\d+)\. ", line)
        return attempt is None or int(attempt.group(1)) in ROGUE_SAMPLE

    rogue = block(log, r"^inti> rogue all$", r"^kernel invariants:")
    put_log("log.rogue", rogue)
    put_log("log.rogue_short", shorten(rogue, sampled))
    default = block(log, r"^inti> rogue$", r"^kernel invariants:")
    put_log("log.rogue_default", shorten(default, lambda line: re.match(r"\s*\d+\. ", line) is None or "skipped" in line))

    put_log("log.crash_console", block(log, r"^inti> crash console$", r"^RECOVERY: console crash"))
    put_log("log.crash_fs", block(log, r"^inti> crash fs$", r"^RECOVERY: fs crash"))
    put_log("log.hang_console", block(log, r"^inti> hang console$", r"^RECOVERY: console hang"))
    put_log("log.hang_fs", block(log, r"^inti> hang fs$", r"^RECOVERY: fs hang"))
    put_log("log.double", block(log, r"^inti> double$", r"^DOUBLE FAILURE:"))
    tour = block(log, r"^inti> tour$", r"^TOUR: done")
    put_log("log.tour", shorten(tour, lambda line: re.match(r"\s*\d+\. ", line) is None))

    slowed = next(index for index, line in enumerate(log) if "times in a row" in line)
    first = max(index for index in range(slowed) if log[index].startswith("inti> crash fs"))
    ends = [index for index in range(slowed, len(log)) if log[index].startswith("RECOVERY:")]
    put_log("log.backoff", log[first:ends[1] + 1])
    put_log("log.svc", block(log, r"^inti> svc$", r"^shell ", occurrence=-1))
    put_log("log.crash_init", block(log, r"^inti> crash init$", r"^Type help"))

    fuzz = block(log, r"^inti> fuzz ", r"^FUZZ: PASS")
    progress = [line for line in fuzz if re.match(r"\s+\d+%", line)]
    put_log("log.fuzz", shorten(fuzz, lambda line: line not in progress[1:-1]))
    put_log("log.bench", block(log, r"^inti> bench$", r"^BENCH: done"))
    put_log("log.uptime", block(log, r"^inti> uptime$", r"^kernel invariants:", occurrence=-1))

    browser = web["serial"].splitlines()
    rogue = block(browser, r"^inti> rogue$", r"^kernel invariants:")
    put_log("weblog.rogue", shorten(rogue, lambda line: re.match(r"\s*\d+\. ", line) is None or "skipped" in line), WEB)
    put_log("weblog.faults", block(browser, r"^inti> crash console$", r"^RECOVERY: fs hang"), WEB)
    # What the browser run shows about W^X: the kernel's boot line, then the execute checks of
    # the self-test and the execute attempts of the hostile program, before the tour repeats them.
    tour_at = next((index for index, line in enumerate(browser) if line.startswith("inti> tour")), len(browser))
    executing = re.compile(r"W\^X|run code from|^inti> (selftest|rogue)$|^SELFTEST:|^ROGUE:")
    put_log("weblog.wx", shorten(browser[:tour_at], lambda line: executing.search(line) is not None), WEB)

    telling = re.compile(
        r"W\^X|^\s*skip |^SELFTEST:|NOT DENIED|^ROGUE:|no-execute bit|^inti> (selftest|rogue all)$|^kernel invariants:"
    )
    put_log("nonxlog.summary", shorten(nonx, lambda line: telling.search(line) is not None), NONX_LOG)


def add_sources(put: Put, result: dict) -> None:
    """The cited sources and what tools/check_sources.py found when it fetched them."""
    entries = result["sources"]
    cited = sum(1 for entry in entries if entry["status"] == "confirmed")
    copies = sum(1 for entry in entries if entry["status"] == "copy")
    put("sources.date", result["date"][:10], SOURCES)
    put("sources.total", len(entries), SOURCES)
    put("sources.confirmed", cited + copies, SOURCES)
    put("sources.confirmed_cited", cited, SOURCES)
    put("sources.confirmed_copy", copies, SOURCES)
    put("table.sources", table(
        ["Source", "What is taken from it", "Words the script looked for", "What the script found"],
        [
            [
                f"[{entry['title']}]({entry['url']})",
                entry["cited"],
                "; ".join(f'"{phrase}"' for phrase in entry["phrases"]),
                entry["result"],
            ]
            for entry in entries
        ],
    ), SOURCES)


def model_fingerprint() -> str:
    """One hash over every source file the model is compiled from: the kernel, the interface and the model."""
    digest = hashlib.sha256()
    for path in sorted(gates.rust_files(("kernel/src", "abi/src", "model/src"))):
        digest.update(path.relative_to(ROOT).as_posix().encode("utf-8") + b"\0")
        digest.update(path.read_bytes().replace(b"\r\n", b"\n") + b"\0")
    return digest.hexdigest()


def evidence_lines(name: str) -> list[str]:
    return (gates.EVIDENCE / name).read_text(encoding="utf-8").splitlines()


def add_model(put: Put, model: dict) -> None:
    """The kernel's source run on the build machine: python build.py model."""
    if not model:
        raise KeyError("evidence/model.json is missing; run python build.py model")
    for key in ("date", "host", "target"):
        put(f"model.{key}", model[key], MODEL)
    put("model.duration_s", round(model["duration_s"]), MODEL)
    put("model.tests", model["tables"]["total"], MODEL)
    put("model.tests_passed", model["tables"]["passed"], MODEL)
    put("model.seeds", model["seeds"]["planted"], MODEL)
    put("model.seeds_found", model["seeds"]["found"], MODEL)
    put("model.blind", model["blind"]["forged"], MODEL)
    put("model.blind_unseen", model["blind"]["unseen"], MODEL)
    for key in ("scenarios", "states", "transitions", "violations", "pairs", "pairs_possible"):
        put(f"model.{key}", model["total"][key], MODEL)
    put("model.pairs_not_met", len(model["pairs_not_met"]), MODEL)
    put("model.pairs_not_met_list", "; ".join(model["pairs_not_met"]), MODEL)

    scenarios = model["scenarios"]
    complete = [scenario["name"] for scenario in scenarios if scenario["complete"] in (True, "yes")]
    put("model.complete", len(complete), MODEL)
    put("model.complete_list", ", ".join(f"`{name}`" for name in complete), MODEL)
    for key in ("deliveries", "replies", "restarts"):
        put(f"model.{key}", sum(scenario[key] for scenario in scenarios), MODEL)

    def ended_with(wanted: Callable[[str], bool]) -> int:
        return sum(count for scenario in scenarios for code, count in scenario["results"].items() if wanted(code))

    put("model.refused", ended_with(lambda code: code.startswith("E_")), MODEL)
    put("model.no_memory", ended_with(lambda code: code == "E_NOMEM"), MODEL)
    put("model.no_slot", ended_with(lambda code: code == "E_NOSLOT"), MODEL)
    put("table.model", table(
        ["Scenario", "What it holds", "Depth", "States", "Steps", "Explored to the end", "Violations"],
        [
            [
                f"`{scenario['name']}`", scenario["about"], scenario["depth"], scenario["states"],
                scenario["transitions"], "yes" if scenario["name"] in complete else "no", scenario["violations"],
            ]
            for scenario in scenarios
        ],
    ), MODEL)

    before = evidence_lines("model-before-fixes.log")
    passed, total = (int(number) for number in re.search(r"MODEL tables: FAIL (\d+)/(\d+)", "\n".join(before)).groups())
    found, planted = (int(number) for number in re.search(r"MODEL seeds: (\d+) of (\d+)", "\n".join(before)).groups())
    put("model.before.tests_passed", passed, MODEL_BEFORE)
    put("model.before.tests", total, MODEL_BEFORE)
    put("model.before.seeds_found", found, MODEL_BEFORE)
    put("model.before.seeds", planted, MODEL_BEFORE)
    put("modellog.before", "\n".join(
        line for line in before if re.match(r"\s*FAIL  |MODEL (tables|seeds):", line)
    ), MODEL_BEFORE)
    put("modellog.result", "\n".join(
        line for line in evidence_lines("model.log")
        if re.match(r"MODEL (tables|seeds|blind|explore total):|MODEL: ", line)
    ), MODEL_LOG)

    included = re.findall(r'#\[path = "\.\./\.\./(kernel/src/\w+\.rs)"\]', source_text(MODEL_MAIN))
    left_out = [
        path for path in gates.rust_files(("kernel/src",))
        if path.relative_to(ROOT).as_posix() not in included
    ]
    put("model.kernel_files", len(included), MODEL_MAIN)
    put("model.kernel_file_list", ", ".join(f"`{name.rsplit('/', 1)[1]}`" for name in included), MODEL_MAIN)
    put("model.kernel_lines", sum(gates.count_code_lines(ROOT / name) for name in included), MODEL_MAIN)
    put("model.left_out_files", len(left_out), MODEL_MAIN)
    put("model.left_out_list", " and ".join(f"`{path.name}`" for path in left_out), MODEL_MAIN)
    put("model.left_out_lines", sum(gates.count_code_lines(path) for path in left_out), MODEL_MAIN)
    put("loc.model", gates.code_lines(("model/src",)), "model/src")


# Why each mutant that no check caught is still alive, judged by reading the code. Keyed by file, line and
# operator. Every note begins with one of the verdicts below, which is how the documents count them. A survivor
# without a note stops the documents from being built, so that none is published unread.
JUDGMENTS = {
    "Makes no difference": "judged_no_difference",
    "Outside what the model can see": "judged_outside_model",
    "A hole in the checks": "judged_hole",
}

SURVIVORS: dict[str, str] = {
    "kernel/src/mem.rs:49:&& becomes ||": (
        "Makes no difference. The line is an assertion that the compiler evaluates while it builds the kernel, "
        "and both halves hold for the constants as they are, so the changed kernel is the same program."
    ),
    "kernel/src/task.rs:545:!= becomes ==": (
        "Makes no difference in any state the kernel can reach. An unbound line names no endpoint, so the changed "
        "loop never runs, and what it then leaves out is done by every caller: a dying task's lines are unbound "
        "before its capabilities are dropped, and after a drop or a revocation `release` unbinds what the task "
        "can no longer receive on. The loop is a second line of defence, kept on purpose."
    ),
    # The held-out sample. No test was written against any of these.
    "kernel/src/ipc.rs:75:assignment removed": (
        "A hole in the checks. With the line gone, whether a receiver may be handed a capability is left over from "
        "its own last call instead of coming from the capability it receives through. Every call and reply in the "
        "directed tests sends no capability, and the exploration takes a refused transfer for a call that failed "
        "and changed nothing. The self-test in QEMU does pass capabilities in messages; whether it catches this "
        "mutant was not tried."
    ),
    "kernel/src/ipc.rs:154:assignment removed": (
        "A hole in the checks. An interrupt that was reported then stays pending, so every later receive on that "
        "endpoint reports it again. No check asks that an interrupt is reported only once: the directed tests "
        "raise none, and the exploration, which does, has no rule about how often one arrives."
    ),
    "kernel/src/mem.rs:60:- becomes +": (
        "A hole in the checks, at one boundary. Rounding up to whole pages then goes wrong only for a size that "
        "is a whole number of pages or one byte short of it. The function is used only for the size of the boot "
        "archive, and the archive the model boots from is neither."
    ),
    "kernel/src/mem.rs:154:assignment removed": (
        "A hole in the checks, in a reported figure only. The number of pages the kernel itself occupies is then "
        "never set. It appears in the statistics call and in the boot log; nothing decides by it, and no check "
        "compares it."
    ),
    "kernel/src/mem.rs:196:== becomes !=": (
        "Outside what the model can see. The kernel's own half of every address space would then map page zero "
        "and nothing else. The model calls the kernel's functions directly and never runs the kernel through its "
        "page tables, and no check reads that half. In QEMU the kernel could not go on once paging is switched "
        "on; that is a judgment, since the mutants were not run there."
    ),
    "kernel/src/mem.rs:296:&& becomes ||": (
        "Makes no difference while the kernel is right. The line is an assertion about arguments that the kernel "
        "itself supplies. It holds in both versions, and no check hands it a wrong frame to see that it stops "
        "the kernel."
    ),
    "kernel/src/mem.rs:514:condition inverted": (
        "Outside what the model can see. After an unmapping the kernel reloads the address space so that the "
        "processor forgets the old translations. The function is only ever called for the calling task, whose "
        "address space is the loaded one, so the changed kernel simply never reloads, and the simulated machine "
        "keeps no translations that could go stale. On a real processor a task could go on using a page it has "
        "given back. Whether the tests in QEMU notice was not tried."
    ),
    "kernel/src/sys.rs:86:assignment removed": (
        "A hole in the checks. A read from an I/O port then leaves the caller's register as it was instead of "
        "returning the value read. No check looks at that value: the simulated port always reads as zero, and "
        "the helper the tests use returns only the status of the call."
    ),
    "kernel/src/sys.rs:154:< becomes <=": (
        "A hole in the checks. A port capability that begins at the first port of its source can then no longer "
        "be derived. The directed tests derive one port capability, out of the middle of the capability for "
        "every port, and none that begins where its source begins. The changed kernel refuses more, not less."
    ),
    "kernel/src/sys.rs:324:> becomes >=": (
        "A hole in the checks. A spawn that asks for exactly the memory quota its caller has left is then "
        "refused. No test asks for exactly that much. The changed kernel refuses more, not less."
    ),
    "kernel/src/task.rs:29:&& becomes ||": (
        "Makes no difference. The line is an assertion that the compiler evaluates while it builds the kernel, "
        "and all three of its parts hold for the constants as they are, so the changed kernel is the same program."
    ),
    "kernel/src/task.rs:201:< becomes <=": (
        "A hole in the checks. The frame just past the range of a memory capability then counts as covered by "
        "it. The function decides which pages of device memory are unmapped after a capability is dropped or "
        "revoked, and the checker asks it the same question, so the checker is weakened in the same way. It "
        "would show only when a task loses a capability for device memory while it still holds one for the "
        "range just below, and no test does that."
    ),
    "kernel/src/task.rs:356:!= becomes ==": (
        "A hole in the checks, in a counter only. The number of task switches is then counted wrongly. It is "
        "reported by the statistics call; nothing decides by it, the summary of a state leaves counters out, and "
        "no check reads it."
    ),
    "kernel/src/task.rs:423:assignment removed": (
        "Makes no difference in any state the kernel can reach, as far as reading shows. The line resets the "
        "deadline of a task that is not waiting. Every path that ends a wait already sets the deadline to never "
        "and every path that begins one writes a new deadline, so the branch guards against a state that does "
        "not arise."
    ),
    "kernel/src/task.rs:647:assignment removed": (
        "Makes no difference that a check could see, as far as reading shows; this is the least certain of these "
        "judgments. The function writes the task's state again before it returns. In between, the only code that "
        "reads states is the waking of tasks queued on an endpoint whose last receiver is gone, and the most it "
        "could do is wake the dying task itself, whose state is then overwritten."
    ),
}

MUTANT_KEYS = (
    "sites", "sample", "compiled", "not_compiled", "caught", "not_caught", "caught_by_failed_check",
    "caught_by_panic", "caught_by_time_limit", "caught_by_tables", "caught_by_exploration", "caught_percent",
)


def survivor_key(mutant: dict) -> str:
    return f"{mutant['file']}:{mutant['line']}:{mutant['operator']}"


def add_mutant_run(put: Put, prefix: str, table_name: str, mutants: dict, source: str, command: str) -> None:
    """The figures of one sample of mutants, and the table of those that no check caught."""
    if not mutants:
        raise KeyError(f"{source} is missing; run {command}")
    summary = mutants["summary"]
    for key in MUTANT_KEYS:
        put(f"{prefix}.{key}", summary[key], source)

    def code(text: str) -> str:
        return "`" + text.strip().replace("|", "\\|") + "`"

    survivors = [mutant for mutant in mutants["mutants"] if mutant["verdict"] == "not caught"]
    if len(survivors) != summary["not_caught"]:
        raise ValueError(f"{source}: the list of mutants does not match its summary")
    judged = dict.fromkeys(JUDGMENTS.values(), 0)
    rows = []
    for mutant in survivors:
        key = survivor_key(mutant)
        if key not in SURVIVORS:
            raise KeyError(f"{source}: no check caught {key} and SURVIVORS has no note on it; read the code, add one")
        verdict = next((name for name in JUDGMENTS if SURVIVORS[key].startswith(name)), None)
        if verdict is None:
            raise ValueError(f"the note on {key} must begin with one of: {'; '.join(JUDGMENTS)}")
        judged[JUDGMENTS[verdict]] += 1
        rows.append([
            f"`{mutant['file']}`, line {mutant['line']}",
            mutant["operator"].replace("|", "\\|"),
            code(mutant["before"]),
            code(mutant["after"]),
            SURVIVORS[key],
        ])
    put(table_name, table(["Where", "Change", "Before", "After", "Why no check caught it"], rows), source)
    for name, count in judged.items():
        put(f"{prefix}.{name}", count, f"{source}, as judged in tools/claims.py")


def add_mutants(put: Put, mutants: dict, held_out: dict) -> None:
    """The kernel broken on purpose, one place at a time: python build.py mutants, with and without --held-out."""
    add_mutant_run(put, "mutants", "table.mutants_not_caught", mutants, MUTANTS, "python build.py mutants")
    add_mutant_run(
        put, "mutants.held_out", "table.mutants_held_out_not_caught", held_out, MUTANTS_HELD_OUT,
        "python build.py mutants --held-out",
    )

    def identity(mutant: dict) -> tuple:
        return mutant["file"], mutant["line"], mutant["operator"], mutant["after"]

    if {identity(mutant) for mutant in mutants["mutants"]} & {identity(mutant) for mutant in held_out["mutants"]}:
        raise ValueError(f"{MUTANTS_HELD_OUT} shares a mutant with {MUTANTS}; it is not held out")

    # A note that belongs to no surviving mutant has outlived what it describes.
    alive = {
        survivor_key(mutant)
        for run in (mutants, held_out)
        for mutant in run["mutants"]
        if mutant["verdict"] == "not caught"
    }
    stale = sorted(set(SURVIVORS) - alive)
    if stale:
        raise ValueError(f"SURVIVORS has notes on mutants that did not survive: {', '.join(stale)}")

    # The first run, before any test was written against a mutant that got away.
    before = re.search(
        r"MUTANTS: (\d+) of (\d+) caught(?: \(\d+%\))?, (\d+) not caught",
        "\n".join(evidence_lines("mutants-before-tests.log")),
    )
    for key, value in zip(("caught", "compiled", "not_caught"), before.groups()):
        put(f"mutants.before.{key}", int(value), MUTANTS_BEFORE)


def collect() -> dict[str, dict]:
    """Every claim, as key -> {"value": ..., "source": the file it was computed from}."""
    claims: dict[str, dict] = {}

    def put(key: str, value: object, source: str) -> None:
        if key in claims:
            raise KeyError(f"claim {key} is defined twice")
        claims[key] = {"value": value, "source": source}

    test = gates.load("test.json")
    web = gates.load("web.json")
    result = gates.load("gates.json")
    add_test(put, test)
    add_web(put, web)
    add_gates(put, result)
    add_source(put)
    add_tables(put, test, result)
    add_sources(put, gates.load("sources.json"))
    add_logs(
        put,
        SERIAL_LOG.read_text(encoding="utf-8").splitlines(),
        web,
        NONX_SERIAL_LOG.read_text(encoding="utf-8").splitlines(),
    )
    add_model(put, gates.load("model.json"))
    add_mutants(put, gates.load("mutants.json"), gates.load("mutants-held-out.json"))
    return claims


# ---- rendering ------------------------------------------------------------------------------

def render(template: str, claims: dict[str, dict]) -> str:
    def value(match: re.Match) -> str:
        key = match.group(1)
        if key not in claims:
            raise KeyError(f"no claim named {key}")
        return show(claims[key]["value"], raw=bool(match.group(2)))

    return PLACEHOLDER.sub(value, template)


def render_all(claims: dict[str, dict]) -> dict[str, str]:
    """Output path -> text of every document."""
    documents = {}
    for template, output in TEMPLATES.items():
        note = f"<!-- Generated by tools/claims.py from {template}. Edit the template, not this file. -->\n"
        documents[output] = note + render(source_text(template), claims)
    return documents


def main() -> int:
    claims = collect()
    CLAIMS.write_text(json.dumps(claims, indent=1, ensure_ascii=False) + "\n", encoding="utf-8", newline="\n")
    documents = render_all(claims)
    for output, text in documents.items():
        (ROOT / output).write_text(text, encoding="utf-8", newline="\n")
    print(f"claims: {len(claims)} claims -> evidence/claims.json; {len(documents)} documents rendered", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
