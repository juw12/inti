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
PLACEHOLDER = re.compile(r"\{\{([a-z0-9_.]+)(:raw)?\}\}")
ELLIPSIS = "..."  # stands for lines left out of a log excerpt

TEST = "evidence/test.json"
WEB = "evidence/web.json"
GATES = "evidence/gates.json"
LOG = "evidence/test_serial.log"
SOURCES = "evidence/sources.json"

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
ROGUE_SAMPLE = (1, 2, 3, 4, 5, 9, 11, 15, 20, 26, 30, 31, 35, 41)  # attempts shown in the short excerpt

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
    put("fuzz.kid_life_ms", constant(fuzz, "KID_LIFE_US") // 1000, fuzz)
    put("fuzz.check_every_ms", constant(fuzz, "CHECK_EVERY_US") // 1000, fuzz)
    put("fuzz.default_calls", constant(fuzz, "DEFAULT_CALLS"), fuzz)
    put("fuzz.kids", constant(fuzz, "PLACES"), fuzz)
    put("supervisor.services", constant(init, "SERVICES"), init)
    put("supervisor.server_pages", constant(init, "SERVER_PAGES"), init)

    arch = "kernel/src/arch.rs"
    clock_hz = constant(arch, "CLOCK_HZ")
    (timer_hz,) = pattern_numbers(arch, r"const PIT_RELOAD: u32 = \(CLOCK_HZ / (\d+)\)")
    put("clock.hz", clock_hz, arch)
    put("clock.resolution_ns", round(1e9 / clock_hz), arch)
    put("clock.wrap_ms", round(65536 / clock_hz * 1000, 1), arch)
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


def add_logs(put: Put, log: list[str], web: dict) -> None:
    def put_log(key: str, lines: list[str], source: str = LOG) -> None:
        put(key, "\n".join(lines), source)

    put_log("log.boot", block(log, r"^Inti \S+ kernel$", r"^Type help"))
    selftest = block(log, r"^inti> selftest$", r"^SELFTEST:")
    put_log("log.selftest", selftest)
    put_log("log.selftest_short", selftest[:6] + [ELLIPSIS] + selftest[-3:])

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
    add_logs(put, SERIAL_LOG.read_text(encoding="utf-8").splitlines(), web)
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
