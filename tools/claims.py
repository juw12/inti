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
MUTANTS_HELD_OUT_BEFORE = "evidence/mutants-held-out-before-tests.log"
MUTANTS_FRESH = "evidence/mutants-fresh.json"
MUTANTS_FRESH_BEFORE = "evidence/mutants-fresh-before-tests.log"
MUTANTS_UNSEEN = "evidence/mutants-unseen.json"
MUTANTS_UNSEEN_BEFORE = "evidence/mutants-unseen-before-tests.log"
MUTANTS_FIFTH = "evidence/mutants-fifth.json"
MUTANTS_QEMU = "evidence/mutants-qemu.json"
MIRI = "evidence/miri.json"
MUTANTS_PREVIOUS = "evidence/mutants-previous.json"

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
    # The boot with more memory than the kernel manages, where the selftest's sweep reaches the top frames.
    full = measured["full_memory"]
    for key in ("memory_mb", "selftest_passed", "frames_taken", "batch", "free_frames", "total_frames"):
        put(f"fullmem.{key}", full[key], TEST)
    put("fullmem.checks", sum(1 for check in test["checks"] if check["name"].startswith("with ")), TEST)

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
    add_unsafe(put)

    put("loc.kernel", gates.code_lines(("kernel/src",)), "kernel/src")
    put("loc.abi", gates.code_lines(("abi/src",)), "abi/src")
    put("loc.kernel_total", gates.code_lines(gates.KERNEL_SOURCES), "kernel/src, abi/src")
    put("loc.limit", gates.KERNEL_LINE_LIMIT, "tools/gates.py")
    put("loc.boot", gates.code_lines(("boot/src",)), "boot/src")
    put("loc.user", gates.code_lines(("user/src",)), "user/src")
    put("loc.tools", python_lines(), "build.py, tools/*.py")


def add_unsafe(put: Put) -> None:
    """Where the kernel's uses of `unsafe` are, and which of them the host model (and so Miri) runs."""
    in_model = {
        "kernel/src/" + name
        for name in re.findall(r'#\[path = "\.\./\.\./kernel/src/(\w+\.rs)"\]', source_text(MODEL_MAIN))
    }
    # The model links the abi crate, but not the modules that crate only builds for the 32-bit target.
    only_x86 = {
        "abi/src/" + name + ".rs"
        for name in re.findall(r'#\[cfg\(target_arch = "x86"\)\]\s+pub mod (\w+);', source_text("abi/src/lib.rs"))
    }
    rows = []
    seen = 0
    unseen = 0
    for path in gates.rust_files(gates.KERNEL_SOURCES):
        name = path.relative_to(ROOT).as_posix()
        uses = sum(
            1 for line in path.read_text(encoding="utf-8").splitlines()
            if re.search(r"\bunsafe\b", gates.code_only(line))
        )
        if not uses:
            continue
        reached = name in in_model or (name.startswith("abi/src/") and name not in only_x86)
        seen += uses if reached else 0
        unseen += 0 if reached else uses
        rows.append([
            f"`{name}`",
            uses,
            "compiled into the host model, so the model's checks and Miri run it" if reached
            else "only in the image that boots in QEMU",
        ])
    if seen + unseen != gates.unsafe_report(gates.KERNEL_SOURCES)[0]:
        raise ValueError("the uses of unsafe per file do not add up to the count the gate reports")
    sources = "kernel/src, abi/src, model/src/main.rs"
    put("count.unsafe_in_model", seen, sources)
    put("count.unsafe_outside_model", unseen, sources)
    put("table.unsafe", table(["File", "Uses of `unsafe`", "Where it runs"], rows), sources)


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
    "kernel/src/mem.rs:57:+ becomes -": (
        "Outside what the model can see. The root of the kernel's own map would then lie on the last of the "
        "tables that map memory, not after the directory. The checker compares that map with the function that "
        "filled it, so the two agree with each other, and the model calls the kernel's functions directly and "
        "never runs the kernel through its page tables. The first reading, that in QEMU the kernel could not go "
        "on once paging is switched on, was wrong, and the runs in QEMU refuted it. Only the last table is "
        "spoilt, the one that maps the top two mebibytes of all the memory the kernel can manage, and the tests "
        "give the emulated machine half of that, so the spoilt part of the map covers memory that is not there. "
        "Frames are handed out from the bottom, so even on a machine with all the memory only a test that "
        "reaches the top of it would notice. Until round seven there was no such test. Now the tests start the "
        "machine once more with more memory than the kernel can manage and take every free frame; the table of "
        "the runs in QEMU says what happened there."
    ),
    "kernel/src/mem.rs:196:condition inverted": (
        "Outside what the model can see. The kernel's own half of every address space would then map page zero "
        "and nothing else. The model calls the kernel's functions directly and never runs the kernel through its "
        "page tables, and no check reads that half. The reading was that in QEMU the kernel could not go on once "
        "paging is switched on; the table of the runs in QEMU says what happened there."
    ),
    "kernel/src/mem.rs:253:assignment removed": (
        "Makes no difference to anything that is promised. The line remembers where the last free frame was "
        "found, so that the next search starts there. Without it every search starts from the beginning and "
        "finds a free frame all the same; which free frame a task is handed is not promised, and no check may "
        "depend on it. The table of the runs in QEMU says what happened there."
    ),
    "kernel/src/mem.rs:296:&& becomes ||": (
        "Makes no difference while the kernel is right. The line is an assertion about arguments that the kernel "
        "itself supplies. It holds in both versions, and no check hands it a wrong frame to see that it stops "
        "the kernel."
    ),
    "kernel/src/mem.rs:515:call removed": (
        "Outside what the model can see. After an unmapping the kernel reloads the address space so that the "
        "processor forgets the old translations. The changed kernel never reloads, and the simulated machine "
        "keeps no translations that could go stale. On a real processor a task could go on using a page it has "
        "given back, until another address space is loaded. QEMU keeps translations as a processor does. Until "
        "round seven no test there touched a page after unmapping it; now the self-test has a child do so and "
        "expects the fault. The table of the runs in QEMU says what happened there."
    ),
    "kernel/src/task.rs:372:assignment removed": (
        "A hole in the checks, read through and left open. The flag that asks for a new choice of task is then "
        "never lowered, so the kernel chooses again at every entry and not only after an event that calls for it. "
        "Every change of rank or of readiness raises the flag anyway, and nothing else decides the choice except "
        "whether a task is over its share. So the one difference found is that a task which goes over its share "
        "is switched away at its next system call and not at the next timer tick, at most one tick sooner. The "
        "harness of the model marks the running task ready before every step, which raises the same flag, and "
        "the flag is private, so no test in the model can tell the two kernels apart; the flag was not made "
        "public for the sake of a test. This is a reading and not a proof."
    ),
    "kernel/src/task.rs:708:assignment removed": (
        "Makes no difference that reading could find; it is a reading and not a proof. The line asks for a new "
        "choice of task after a kill. A running task that has just died is no longer ready, so the scheduler "
        "chooses again without being asked, and a task that the death wakes raises the flag itself when it is "
        "woken."
    ),
    # The second sample. Tests were aimed at six of its places in round 5, and a test written in round 6 for
    # the fresh sample caught a seventh as a side effect; these two are what is left.
    "kernel/src/mem.rs:76:< becomes <=": (
        "Makes no difference while the kernel is right. The line is an assertion about an index that the kernel "
        "itself supplies. It holds in both versions, and no check hands it an index one past the end to see that "
        "it stops the kernel."
    ),
    "kernel/src/task.rs:31:< becomes <=": (
        "Makes no difference. The line is an assertion that the compiler evaluates while it builds the kernel, "
        "and it holds for the constants as they are, so the changed kernel is the same program."
    ),
    # The fresh sample, drawn after those tests. No test was written against any of these.
    "kernel/src/mem.rs:70:< becomes <=": (
        "Makes no difference while the kernel is right. The line is an assertion about an index that the kernel "
        "itself supplies. It holds in both versions, and no check hands it an index one past the end to see that "
        "it stops the kernel."
    ),
    "kernel/src/mem.rs:165:assignment removed": (
        "Outside what the model can see. The kernel would then forget where the root of its own map is and "
        "switch paging on with an address of zero. The model keeps the same forgotten value on both sides of its "
        "comparison and never runs the kernel through its page tables. The reading was that in QEMU the kernel "
        "could not go on once paging is switched on; the table of the runs in QEMU says what happened there."
    ),
    "kernel/src/mem.rs:308:&& becomes ||": (
        "Makes no difference while the kernel is right. The line is an assertion about arguments that the kernel "
        "itself supplies. It holds in both versions, and no check hands it a wrong frame to see that it stops "
        "the kernel."
    ),
    "kernel/src/sys.rs:374:assignment removed": (
        "Outside what the model can see. A new task would then have no starting registers: no entry point, no "
        "stack, no arguments. The model drives every task by making its system calls for it and never runs a "
        "task's code, so it never reads them. The reading was that in QEMU no task could start; the table of the "
        "runs in QEMU says what happened there."
    ),
    "kernel/src/task.rs:345:condition inverted": (
        "Outside what the model can see. The kernel would then print the wrong one of two sentences about the "
        "no-execute bit at boot and enforce exactly what it did before. The simulated serial port only counts "
        "the bytes it is given. The tests in QEMU read that sentence, with the bit and without it."
    ),
    # The fourth sample. It was the unseen one in round 6, when seven of its mutants survived. Round 7 aimed
    # six checks of the model at the six that were holes; this one is what is left.
    "kernel/src/task.rs:33:&& becomes ||": (
        "Makes no difference. The line is an assertion that the compiler evaluates while it builds the kernel, "
        "and it holds for the constants as they are, so the changed kernel is the same program."
    ),
    # The fifth sample, drawn in round 7 after those checks. No test was written against any of these, and
    # none will be before a new unseen sample is drawn.
    "kernel/src/ipc.rs:83:assignment removed": (
        "A hole in the checks. A task that waits for a message with a time limit would then wait with no limit: "
        "the line that sets its deadline is gone, and the deadline of a task that runs is always never. The "
        "exploration of the model expires a wait by setting the deadline itself, so it cannot see whether the "
        "kernel had set one, and every directed test of a receive either polls or is answered. The self-test in "
        "QEMU has a receive that must run out; the table of the runs in QEMU says what happened there."
    ),
    "kernel/src/mem.rs:49:&& becomes ||": (
        "Makes no difference. The line is an assertion that the compiler evaluates while it builds the kernel, "
        "and it holds for the constants as they are, so the changed kernel is the same program."
    ),
    "kernel/src/mem.rs:282:< becomes <=": (
        "Makes no difference while the kernel is right. The line is an assertion about an index that the kernel "
        "itself supplies. It holds in both versions, and no check hands it an index one past the end to see that "
        "it stops the kernel."
    ),
    "kernel/src/mem.rs:322:>= becomes >": (
        "Makes no difference while the kernel is right. The line refuses the number of a page table that lies "
        "past the user range. Every caller takes the number from a user address it has checked or from a loop "
        "that stops below the bound, so the one value that the two versions treat differently never arrives, "
        "and no check hands it in to see that it is refused. This is a reading and not a proof."
    ),
    "kernel/src/task.rs:728:assignment removed": (
        "Makes no difference that reading could find; it is a reading and not a proof. The line clears the "
        "deadline of a task that is being destroyed. The timer wakes nobody that has ended or whose slot is "
        "free, and clears such a deadline itself when it comes due; a slot that is used again gets a new "
        "deadline when its task is made ready, and every way of waiting sets its own."
    ),
    "kernel/src/task.rs:771:assignment removed": (
        "Makes no difference that reading could find; it is a reading and not a proof. The line forgets the "
        "fault endpoint of a slot that has just become free. Starting a task writes that field anew, the kernel "
        "reads it only for a task that is being destroyed or reaped, and the checker skips free slots when it "
        "counts the references to an endpoint."
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


def add_mutants(put: Put, mutants: dict, held_out: dict, fresh: dict, unseen: dict, fifth: dict) -> None:
    """The kernel broken on purpose, one place at a time: python build.py mutants, plain, --held-out, --fresh,
    --unseen and --fifth."""
    add_mutant_run(put, "mutants", "table.mutants_not_caught", mutants, MUTANTS, "python build.py mutants")
    add_mutant_run(
        put, "mutants.held_out", "table.mutants_held_out_not_caught", held_out, MUTANTS_HELD_OUT,
        "python build.py mutants --held-out",
    )
    add_mutant_run(
        put, "mutants.fresh", "table.mutants_fresh_not_caught", fresh, MUTANTS_FRESH,
        "python build.py mutants --fresh",
    )
    add_mutant_run(
        put, "mutants.unseen", "table.mutants_unseen_not_caught", unseen, MUTANTS_UNSEEN,
        "python build.py mutants --unseen",
    )
    add_mutant_run(
        put, "mutants.fifth", "table.mutants_fifth_not_caught", fifth, MUTANTS_FIFTH,
        "python build.py mutants --fifth",
    )

    def identity(mutant: dict) -> tuple:
        return mutant["file"], mutant["line"], mutant["operator"], mutant["after"]

    samples = (
        (MUTANTS, mutants), (MUTANTS_HELD_OUT, held_out), (MUTANTS_FRESH, fresh), (MUTANTS_UNSEEN, unseen),
        (MUTANTS_FIFTH, fifth),
    )
    for index, (source, run) in enumerate(samples):
        for other_source, other in samples[index + 1:]:
            if {identity(mutant) for mutant in run["mutants"]} & {identity(mutant) for mutant in other["mutants"]}:
                raise ValueError(f"{other_source} shares a mutant with {source}; the samples must be disjoint")

    # A note that belongs to no surviving mutant has outlived what it describes.
    alive = {
        survivor_key(mutant)
        for run in (mutants, held_out, fresh, unseen, fifth)
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

    # The second sample as it came out while no test had been aimed at it: the run of the round before.
    before = re.search(
        r"MUTANTS: (\d+) of (\d+) caught(?: \(\d+%\))?, (\d+) not caught",
        "\n".join(evidence_lines("mutants-held-out-before-tests.log")),
    )
    for key, value in zip(("caught", "compiled", "not_caught"), before.groups()):
        put(f"mutants.held_out.before.{key}", int(value), MUTANTS_HELD_OUT_BEFORE)

    # The third sample as it came out while no test had been aimed at it, and how many of the mutants that
    # got away then are caught now. The kernel has not changed since, so file, line and change identify one.
    fresh_before = evidence_lines("mutants-fresh-before-tests.log")
    before = re.search(
        r"MUTANTS: (\d+) of (\d+) caught(?: \(\d+%\))?, (\d+) not caught", "\n".join(fresh_before),
    )
    for key, value in zip(("caught", "compiled", "not_caught"), before.groups()):
        put(f"mutants.fresh.before.{key}", int(value), MUTANTS_FRESH_BEFORE)
    got_away = set()
    for line in fresh_before:
        found = re.match(r"mutant\s+\d+\s+(\S+):(\d+)\s+(.+): not caught\s*$", line)
        if found:
            got_away.add((found.group(1), int(found.group(2)), found.group(3)))
    if len(got_away) != int(before.group(3)):
        raise ValueError(f"{MUTANTS_FRESH_BEFORE}: its lines and its summary disagree")
    now = {
        (mutant["file"], mutant["line"], mutant["operator"]): mutant["verdict"]
        for mutant in fresh["mutants"]
    }
    if not got_away <= set(now):
        raise ValueError(f"{MUTANTS_FRESH_BEFORE} names mutants the present fresh sample does not hold")
    source = f"{MUTANTS_FRESH_BEFORE} against {MUTANTS_FRESH}"
    put("mutants.fresh.closed", sum(1 for key in got_away if now[key] == "caught"), source)
    put("mutants.fresh.left_open", sum(1 for key in got_away if now[key] == "not caught"), source)

    # The fourth sample as it came out while no test had been aimed at it: the run of the round before. Tests
    # were then written against the holes it showed, so its present figure is no longer that of an unseen sample.
    unseen_before = evidence_lines("mutants-unseen-before-tests.log")
    before = re.search(
        r"MUTANTS: (\d+) of (\d+) caught(?: \(\d+%\))?, (\d+) not caught", "\n".join(unseen_before),
    )
    for key, value in zip(("caught", "compiled", "not_caught"), before.groups()):
        put(f"mutants.unseen.before.{key}", int(value), MUTANTS_UNSEEN_BEFORE)
    got_away = set()
    for line in unseen_before:
        found = re.match(r"mutant\s+\d+\s+(\S+):(\d+)\s+(.+): not caught\s*$", line)
        if found:
            got_away.add((found.group(1), int(found.group(2)), found.group(3)))
    if len(got_away) != int(before.group(3)):
        raise ValueError(f"{MUTANTS_UNSEEN_BEFORE}: its lines and its summary disagree")
    now = {
        (mutant["file"], mutant["line"], mutant["operator"]): mutant["verdict"]
        for mutant in unseen["mutants"]
    }
    if not got_away <= set(now):
        raise ValueError(f"{MUTANTS_UNSEEN_BEFORE} names mutants the present unseen sample does not hold")
    source = f"{MUTANTS_UNSEEN_BEFORE} against {MUTANTS_UNSEEN}"
    put("mutants.unseen.closed", sum(1 for key in got_away if now[key] == "caught"), source)
    put("mutants.unseen.left_open", sum(1 for key in got_away if now[key] == "not caught"), source)

    # All samples are positions in the list of places, so they are drawn anew whenever that list changes. How
    # many of the places tried now were also tried a round ago is counted here and not guessed. A place is matched
    # by file, kind of change and the text of the line; identical lines can match each other, so these are upper
    # bounds.
    previous = gates.load("mutants-previous.json")
    if not previous:
        raise KeyError(f"{MUTANTS_PREVIOUS} is missing; it lists the places tried in the round before")
    put("mutants.previous.sites", previous["sites"], MUTANTS_PREVIOUS)
    for name in ("first", "held_out"):
        for key, value in previous[f"{name}_summary"].items():
            put(f"mutants.previous.{name}.{key}", value, MUTANTS_PREVIOUS)

    def place(mutant: dict) -> tuple:
        return mutant["file"], mutant["operator"], mutant["before"].strip()

    for prefix, run in (
        ("mutants", mutants), ("mutants.held_out", held_out), ("mutants.fresh", fresh),
        ("mutants.unseen", unseen), ("mutants.fifth", fifth),
    ):
        for name in ("first", "held_out", "held_out_not_caught"):
            known = {tuple(item) for item in previous[name]}
            count = sum(1 for mutant in run["mutants"] if place(mutant) in known)
            put(f"{prefix}.from_previous_{name}", count, f"{MUTANTS_PREVIOUS} against the present sample")
    put(
        "mutants.new_places",
        len(mutants["mutants"]) - sum(
            1 for mutant in mutants["mutants"]
            if place(mutant) in {tuple(item) for name in ("first", "held_out") for item in previous[name]}
        ),
        f"{MUTANTS_PREVIOUS} against the present sample",
    )


QEMU_VERDICTS = {
    "caught": "caught twice",
    "not repeated": "failed once, passed when run again; not counted",
    "not counted": "the unchanged kernel failed beside it; not counted",
    "same image": "builds the same image",
    "not caught": "not caught",
    "does not build": "does not build",
    "differing": "failed twice, on different checks; not counted",
}


def check_qemu_catch(mutant: dict) -> bool:
    """A catch is counted only as the tool's rule has it: the changed kernel failed, the unchanged kernel
    passed the same suite straight after, and the changed kernel failed again. And this page is stricter
    than the tool: False is returned for a mutant whose two failed runs share no failed check and did not
    pass their time limit. Two failures with nothing in common are not evidence of one defect, so such a
    mutant is listed as failing on different checks and is not counted as caught."""
    place = survivor_key(mutant)
    kinds = [(run["kind"], run["passed"]) for run in mutant.get("runs", [])]
    if kinds != [("mutant", False), ("unchanged", True), ("mutant", False)]:
        raise ValueError(f"{MUTANTS_QEMU}: {place} is called caught, but its runs are {kinds}")
    if mutant["how"] == "failed check" and not mutant["by"]:
        first, repeat = mutant["runs"][0], mutant["runs"][2]
        return bool(first["timed_out"] or repeat["timed_out"])
    return True


def add_mutants_qemu(put: Put, qemu: dict, runs: tuple[dict, ...]) -> None:
    """The mutants the host model let through, booted in QEMU: python build.py mutants-qemu."""
    if not qemu:
        raise KeyError(f"{MUTANTS_QEMU} is missing; run python build.py mutants-qemu")
    summary = qemu["summary"]
    keys = ("tried", "caught", "not_repeated", "not_counted", "same_image", "not_caught", "does_not_build")
    for key in keys:
        put(f"mutants.qemu.{key}", summary[key], MUTANTS_QEMU)
    if summary["tried"] != len(qemu["mutants"]) or summary["tried"] != sum(summary[key] for key in keys[1:]):
        raise ValueError(f"{MUTANTS_QEMU}: the list of mutants does not match its summary")
    for key in keys[1:]:
        if summary[key] != sum(1 for mutant in qemu["mutants"] if mutant["verdict"] == key.replace("_", " ")):
            raise ValueError(f"{MUTANTS_QEMU}: the verdicts of the mutants do not match the summary's {key}")
    survivors = sorted(
        survivor_key(mutant) for run in runs for mutant in run["mutants"] if mutant["verdict"] == "not caught"
    )
    if sorted(survivor_key(mutant) for mutant in qemu["mutants"]) != survivors:
        raise ValueError(f"{MUTANTS_QEMU} does not hold exactly the mutants the five samples did not catch")
    put("mutants.qemu.fuzz", qemu["options"]["fuzz"], MUTANTS_QEMU)
    put("mutants.qemu.runs", qemu["options"]["runs"], MUTANTS_QEMU)
    put("mutants.qemu.time_limit_s", round(qemu["time_limit_s"]), MUTANTS_QEMU)
    for when in ("before", "after"):
        unchanged = qemu["unchanged"][when]
        if not unchanged["passed"]:
            raise ValueError(f"{MUTANTS_QEMU}: the unchanged kernel did not pass {when} the mutants")
        put(f"mutants.qemu.unchanged_{when}_checks", unchanged["checks"], MUTANTS_QEMU)
    called_caught = [mutant for mutant in qemu["mutants"] if mutant["verdict"] == "caught"]
    caught = [mutant for mutant in called_caught if check_qemu_catch(mutant)]
    differing = [mutant for mutant in called_caught if not check_qemu_catch(mutant)]
    put("mutants.qemu.differing", len(differing), MUTANTS_QEMU)
    put(
        "mutants.qemu.caught_by_time_limit",
        sum(1 for mutant in caught if mutant["how"] == "time limit"), MUTANTS_QEMU,
    )
    put(
        "mutants.qemu.caught_by_failed_check",
        sum(1 for mutant in caught if mutant["how"] == "failed check"), MUTANTS_QEMU,
    )
    # A catch is a mutant that failed twice with the unchanged kernel passing in between; the tool applies
    # that rule itself. Taken out of its count here: the mutants whose two failures share no check.
    put("mutants.qemu.noticed", len(caught), MUTANTS_QEMU)
    put(
        "mutants.qemu.noticed_by_several_checks",
        sum(1 for mutant in caught if len(mutant["by"]) > 1 or mutant["how"] == "time limit"),
        MUTANTS_QEMU,
    )
    # How many runs beside the unchanged kernel there were, and how many of those the unchanged kernel failed.
    beside = [
        run for mutant in qemu["mutants"] for run in mutant.get("runs", []) if run["kind"] == "unchanged"
    ]
    put("mutants.qemu.unchanged_beside_runs", len(beside), MUTANTS_QEMU)
    put("mutants.qemu.unchanged_beside_failed", sum(1 for run in beside if not run["passed"]), MUTANTS_QEMU)
    # Why the unchanged kernel failed beside a mutant; every such failure must be of a kind named here.
    failed_beside = [run for run in beside if not run["passed"]]
    by_kind = {
        "time_limit": [run for run in failed_beside if run["timed_out"]],
        "clock": [
            run for run in failed_beside
            if not run["timed_out"] and any("guest clock" in name for name in run["failed"])
        ],
        "fuzz": [
            run for run in failed_beside
            if not run["timed_out"] and any(name.startswith("fuzz:") for name in run["failed"])
        ],
    }
    if sum(len(found) for found in by_kind.values()) != len(failed_beside):
        raise ValueError(f"{MUTANTS_QEMU}: a failure of the unchanged kernel is of no kind, or of two")
    for kind, found in by_kind.items():
        put(f"mutants.qemu.unchanged_failed_{kind}", len(found), MUTANTS_QEMU)
    if any("selftest" in name for run in failed_beside for name in run["failed"]):
        raise ValueError(f"{MUTANTS_QEMU}: the unchanged kernel failed the self-test; the text says it did not")
    # Mutants that lost their verdict to a failing unchanged kernel although they failed one check twice.
    put(
        "mutants.qemu.not_counted_same_check_twice",
        sum(
            1 for mutant in qemu["mutants"]
            if mutant["verdict"] == "not counted" and len(mutant["runs"]) == 3
            and set(mutant["runs"][0]["failed"]) & set(mutant["runs"][2]["failed"])
        ),
        MUTANTS_QEMU,
    )
    # Every run of the suite in the batch, and the share measurements those runs set aside as unsteady.
    every_run = [qemu["unchanged"]["before"], qemu["unchanged"]["after"]] + [
        run for mutant in qemu["mutants"] for run in mutant.get("runs", [])
    ]
    put("mutants.qemu.suite_runs", len(every_run), MUTANTS_QEMU)
    put("mutants.qemu.set_aside", sum(run["set_aside"] for run in every_run), MUTANTS_QEMU)
    # A mutant that neither the host model nor the booted system notices is the real remainder.
    put(
        "mutants.qemu.not_noticed",
        summary["not_caught"] + summary["same_image"] + summary["not_repeated"] + summary["not_counted"]
        + len(differing),
        MUTANTS_QEMU,
    )

    def code(text: str) -> str:
        return "`" + text.strip().replace("|", "\\|") + "`"

    rows = []
    for mutant in qemu["mutants"]:
        how = ""
        if mutant["verdict"] == "caught":
            if mutant["how"] == "time limit":
                how = "both runs passed their time limit"
            elif mutant["by"]:
                how = f"{len(mutant['by'])} failed in both runs, the first: {mutant['by'][0]}"
            else:
                first, repeat = mutant["runs"][0]["failed"], mutant["runs"][2]["failed"]
                how = (
                    f"first run: {first[0] if first else 'time limit'}; "
                    f"second run: {repeat[0] if repeat else 'time limit'}"
                )
        elif mutant["verdict"] == "not repeated":
            failed = mutant["runs"][0]["failed"]
            how = f"failed in the first run only: {failed[0]}" if failed else "the first run passed its time limit"
        elif mutant["verdict"] == "not counted":
            failed = mutant["runs"][1]["failed"]
            how = f"the unchanged kernel failed: {failed[0]}" if failed else "the unchanged kernel passed its time limit"
        rows.append([
            f"`{mutant['file']}`, line {mutant['line']}",
            mutant["sample"],
            mutant["operator"].replace("|", "\\|"),
            code(mutant["after"]),
            QEMU_VERDICTS["differing" if mutant in differing else mutant["verdict"]],
            how.replace("|", "\\|"),
        ])
    put(
        "table.mutants_qemu",
        table(["Where", "Sample", "Change", "After", "In QEMU", "Which checks"], rows), MUTANTS_QEMU,
    )


def add_miri(put: Put, miri: dict) -> None:
    """The model's directed checks under Miri: python build.py miri."""
    if not miri:
        raise KeyError(f"{MIRI} is missing; run python build.py miri")
    for key in (
        "miri", "rustc", "ran", "limit_s", "seconds", "checks_passed", "checks_failed", "undefined_behaviour",
        "sections_started", "last_section",
    ):
        put(f"miri.{key}", miri[key], MIRI)
    put("miri.minutes", round(miri["seconds"] / 60), MIRI)
    put("miri.limit_minutes", round(miri["limit_s"] / 60), MIRI)
    put("miri.ended", "ran to its end" if miri["finished"] else "was cut short at its time limit", MIRI)
    put("miri.finished", "yes" if miri["finished"] else "no", MIRI)


SHARE_TIMING = "evidence/share-timing.json"
LAUNCH_RACE = "evidence/fuzz-launch-race"
LAST_SUBTEST = "no task, endpoint, page frame or quota was lost"


def add_launch_race(put: Put) -> None:
    """The race between a program's first reading of free frames and its starter unmapping the file.

    Every figure is read from the serial logs; the record only names the delay that was put into the shell.
    """
    import json
    import pathlib

    evidence = pathlib.Path(str(gates.EVIDENCE))
    frames = re.compile(r"^FUZZ: page frames (\d+) -> (\d+)$", re.MULTILINE)
    failed_subtest = re.compile(r"^  FAIL (.+)$", re.MULTILINE)

    def text_of(path: pathlib.Path) -> str:
        return path.read_text(encoding="utf-8", errors="replace").replace("\r\n", "\n")

    batch = evidence / "mutants-qemu-logs"
    readings = set()
    runs = 0
    for path in sorted(batch.glob("*.unchanged.serial.log")):
        found = frames.findall(text_of(path))
        if found:
            runs += 1
            readings.update(found)
    if len(readings) != 1:
        raise ValueError(f"{batch}: the unchanged kernel's fuzz runs do not all show one pair of figures: {readings}")
    before, after = (int(figure) for figure in readings.pop())
    source = "evidence/mutants-qemu-logs"
    put("race.batch_runs", runs, source)
    put("race.frames_before", before, source)
    put("race.frames_after", after, source)
    put("race.frames_gained", after - before, source)

    stage = {path.name.split("-")[0]: text_of(path) for path in (evidence / "fuzz-launch-race").glob("*.serial.log")}
    if sorted(stage) != ["1", "2", "3"]:
        raise ValueError(f"{LAUNCH_RACE}: expected three stages, found {sorted(stage)}")
    size = re.search(r"^  fuzz\s+(\d+) bytes$", stage["1"], re.MULTILINE)
    pages = -(-int(size.group(1)) // 4096)
    if pages + 1 != after - before:
        raise ValueError(f"{LAUNCH_RACE}: the fuzzer's file and one page table do not explain {after - before} frames")
    put("race.image_pages", pages, f"{LAUNCH_RACE}/1-shell-delay-old-fuzzer.serial.log")

    if set(frames.findall(stage["1"])) != {(str(before), str(after))} or "FUZZ: FAIL" not in stage["1"]:
        raise ValueError(f"{LAUNCH_RACE}: stage one does not show the figures of the batch")
    if set(failed_subtest.findall(stage["1"])) != {LAST_SUBTEST}:
        raise ValueError(f"{LAUNCH_RACE}: stage one does not fail the self-test's last subtest alone")
    if "FUZZ: PASS" not in stage["2"] or "FUZZ: FAIL" in stage["2"]:
        raise ValueError(f"{LAUNCH_RACE}: stage two does not pass the fuzzer")
    if set(failed_subtest.findall(stage["2"])) != {LAST_SUBTEST}:
        raise ValueError(f"{LAUNCH_RACE}: stage two was expected to fail the self-test's last subtest still")
    if "FUZZ: PASS" not in stage["3"] or "SELFTEST: PASS" not in stage["3"]:
        raise ValueError(f"{LAUNCH_RACE}: stage three does not pass")
    if failed_subtest.findall(stage["3"]) or "FUZZ: FAIL" in stage["3"] or "SELFTEST: FAIL" in stage["3"]:
        raise ValueError(f"{LAUNCH_RACE}: stage three has a failure")

    record = json.loads((evidence / "fuzz-launch-race" / "record.json").read_text(encoding="utf-8"))
    put("race.delay_ms", record["shell_delay_ms"], f"{LAUNCH_RACE}/record.json")

    sleeps = set()
    for program in ("fuzz.rs", "selftest.rs"):
        source_text = (evidence.parent / "user" / "src" / "bin" / program).read_text(encoding="utf-8")
        found = re.findall(r"const STARTER_MS: u32 = (\d+);", source_text)
        if len(found) != 1 or "sys::sleep(STARTER_MS);" not in source_text:
            raise ValueError(f"user/src/bin/{program}: the sleep before the first reading is missing")
        sleeps.add(int(found[0]))
    if len(sleeps) != 1:
        raise ValueError("the fuzzer and the self-test do not sleep equally long before their first reading")
    put("race.starter_ms", sleeps.pop(), "user/src/bin/fuzz.rs")

    alone = [
        path.name
        for path in sorted(batch.glob("*.serial.log"))
        if ".fullmem." not in path.name and set(failed_subtest.findall(text_of(path))) == {LAST_SUBTEST}
    ]
    if alone != ["fifth-26.serial.log"]:
        raise ValueError(f"{batch}: the runs that fail the last subtest alone are {alone}; docs/src/design.md names one")
    put("race.selftest_batch_runs", len(alone), source)


def add_timing(put: Put, timing: dict) -> None:
    """The search for the cause of the timing checks failing now and then: evidence/share-timing.json."""
    if not timing:
        raise KeyError(f"{SHARE_TIMING} is missing")
    probe = timing["probe"]
    for key in ("boots", "selftest_runs", "selftest_runs_failed", "alone_stretches", "anomalies_reproduced"):
        put(f"timing.probe.{key}", probe[key], SHARE_TIMING)
    put("timing.earlier_failures", len(timing["earlier_failures"]), SHARE_TIMING)
    put("timing.hypotheses", len(timing["hypotheses"]), SHARE_TIMING)
    put("timing.calm.steadiness_min", probe["steadiness_min_percent"], SHARE_TIMING)

    load = timing["under_load"]
    first = load["first_batch_attempt"]
    put("timing.batch1.reached", first["mutants_reached"], SHARE_TIMING)
    put("timing.batch1.not_counted", first["not_counted_because_unchanged_kernel_failed"], SHARE_TIMING)
    priority = load["priority_probe"]
    if len(priority["failed_subtests"]) != 1 or "share" not in priority["failed_subtests"][0]:
        raise ValueError(f"{SHARE_TIMING}: the failed runs under load are not all in the share subtest")
    put("timing.load.runs", priority["runs"], SHARE_TIMING)
    put("timing.load.failed", priority["failed"], SHARE_TIMING)
    put("timing.load.high_runs", priority["classes"]["high"]["runs"], SHARE_TIMING)
    put("timing.load.high_failed", priority["classes"]["high"]["failed"], SHARE_TIMING)
    put("timing.load.failed_steadiness_max", priority["failed_steadiness_max_percent"], SHARE_TIMING)
    host = load["host"]
    put("timing.host.processors", host["logical_processors"], SHARE_TIMING)
    put("timing.host.queue_median", host["processor_queue_median"], SHARE_TIMING)
    put("timing.host.frequency_min", host["frequency_min_percent"], SHARE_TIMING)
    icount = load["instruction_clock"]
    # The documents say that these runs passed and that every stretch came to within a round of the others.
    if icount["failed"] != 0 or icount["alone_rounds_max"] - icount["alone_rounds_min"] > 1:
        raise ValueError(f"{SHARE_TIMING}: the runs with the instruction clock are not as the documents say")
    put("timing.icount.runs", icount["selftest_runs"], SHARE_TIMING)
    put("timing.icount.failed", icount["failed"], SHARE_TIMING)
    put("timing.icount.stretches", icount["alone_stretches"], SHARE_TIMING)
    put("timing.icount.rounds_min", icount["alone_rounds_min"], SHARE_TIMING)
    put("timing.icount.rounds_max", icount["alone_rounds_max"], SHARE_TIMING)
    put("timing.icount.plain_runs", icount["plain_clock_same_minutes"]["selftest_runs"], SHARE_TIMING)
    put("timing.icount.plain_failed", icount["plain_clock_same_minutes"]["failed"], SHARE_TIMING)

    guard = timing["guard"]
    steady = constant("user/src/bin/selftest.rs", "STEADY_PERCENT")
    if guard["steady_percent"] != steady:
        raise ValueError(f"{SHARE_TIMING}: the limit of the guard is not the self-test's STEADY_PERCENT")
    # The documents say that the limit lies between the least steady quiet run and the steadiest failed one.
    if not priority["failed_steadiness_max_percent"] < steady < probe["steadiness_min_percent"]:
        raise ValueError(f"{SHARE_TIMING}: STEADY_PERCENT does not lie between the two figures it is said to")
    put("timing.steady.percent", steady, "user/src/bin/selftest.rs")
    put("timing.tries", constant("user/src/bin/selftest.rs", "SHARE_TRIES"), "user/src/bin/selftest.rs")
    for name, key in (("guard3", "three_tries"), ("guard5", "five_tries")):
        tried = guard[key]
        put(f"timing.{name}.runs", tried["runs"], SHARE_TIMING)
        put(f"timing.{name}.failed", tried["failed_runs"], SHARE_TIMING)
        put(f"timing.{name}.runs_set_aside", tried["runs_with_set_aside"], SHARE_TIMING)
        put(f"timing.{name}.set_aside", tried["set_aside_measurements"], SHARE_TIMING)


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
    samples = tuple(
        gates.load(name)
        for name in (
            "mutants.json", "mutants-held-out.json", "mutants-fresh.json", "mutants-unseen.json",
            "mutants-fifth.json",
        )
    )
    add_mutants(put, *samples)
    add_mutants_qemu(put, gates.load("mutants-qemu.json"), samples)
    add_miri(put, gates.load("miri.json"))
    add_timing(put, gates.load("share-timing.json"))
    add_launch_race(put)
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
