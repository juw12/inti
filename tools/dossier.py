"""Assemble dossier.md: everything a reviewer needs, in one file, generated from the repository.

    python tools/dossier.py

The dossier is the four rendered documents, a few pieces of the source code copied verbatim,
and the raw measurements behind the summary figures. Nothing in it is written by hand; run
tools/claims.py first so that the documents are current.
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))
import claims  # noqa: E402
import gates  # noqa: E402

OUTPUT = ROOT / "dossier.md"
DOCUMENTS = ("README.md", "docs/weaknesses.md", "docs/scorecard.md", "docs/design.md")
# (file, function, why it is shown)
EXCERPTS = (
    ("kernel/src/check.rs", "check", "The invariant checker's entry point: what one check does."),
    ("kernel/src/check.rs", "claim", "How the checker accounts for one frame of memory."),
    ("kernel/src/sys.rs", "io_port", "Every access to an I/O port goes through this function."),
    ("kernel/src/sys.rs", "sys_cap_derive", "Copying a capability: the only way one changes, and only downwards."),
    ("kernel/src/ipc.rs", "sys_call", "Sending a message and waiting for the reply."),
    ("kernel/src/ipc.rs", "pass_cap", "Handing a capability over in a message: both ends need the grant right."),
    ("kernel/src/sys.rs", "sys_cap_revoke", "Taking back every copy of a capability, in every task."),
    ("kernel/src/task.rs", "release", "What losing a capability undoes: mappings, interrupt bindings, waits."),
    ("kernel/src/check.rs", "check_caps", "Rule 19 of the invariant checker: authority only narrows."),
    ("kernel/src/elf.rs", "load_segment", "Loading a program: a segment that is writable and executable is refused."),
    ("kernel/src/check.rs", "page_ok", "Rule 22 of the invariant checker, applied to every mapped user page."),
    ("kernel/src/mem.rs", "drop_empty_table", "New in round 3: a page table that maps nothing goes back at once."),
    ("model/src/explore.rs", "judge", "The explorer's verdict on one step: an error changes nothing, a restart is a fresh boot."),
    ("user/src/bin/init.rs", "ping_all", "The supervisor's health check, which keeps handling other failures while it waits."),
)
WHOLE_FILES = (
    ("user/src/bin/hello.rs", "A complete user program."),
)


def function(path: str, name: str) -> str:
    """The source of one function, with the comments and attributes directly above it."""
    lines = claims.source_text(path).splitlines()
    start = next(index for index, line in enumerate(lines) if re.search(rf"\bfn {name}\b", line))
    indent = lines[start][: len(lines[start]) - len(lines[start].lstrip())]
    first = start
    while first > 0 and lines[first - 1].strip().startswith(("///", "//", "#[")):
        first -= 1
    last = next(index for index in range(start, len(lines)) if lines[index] == indent + "}")
    return "\n".join(lines[first:last + 1])


def demoted(text: str) -> str:
    """A rendered document with every heading one level lower and the generator's note removed."""
    result = []
    fenced = False
    for line in text.splitlines():
        if line.lstrip().startswith("```"):
            fenced = not fenced
        if line.startswith("<!-- Generated"):
            continue
        result.append("#" + line if line.startswith("#") and not fenced else line)
    return "\n".join(result).strip()


def samples_table(test: dict) -> str:
    rows = []
    for case, label in claims.FAULTS.items():
        times = test["measured"]["recovery"][case]
        rows.append([
            label,
            ", ".join(str(sample) for sample in times["guest_us"]["samples"]),
            ", ".join(str(sample) for sample in times["host_ms"]["samples"]),
        ])
    return claims.table(
        ["Injected fault", "Every run, system clock (microseconds)", "Every run, host stopwatch (milliseconds)"],
        rows,
    )


def double_table(test: dict) -> str:
    double = test["measured"]["double"]
    servers = (("fs_us", "File server, which crashed"), ("console_us", "Console driver, which hung"))
    return claims.table(
        ["Answering again", "Every run, system clock (microseconds)"],
        [[label, ", ".join(str(sample) for sample in double[key]["samples"])] for key, label in servers],
    )


def no_nx_table(test: dict) -> str:
    run = test["measured"]["no_nx"]
    return claims.table(
        ["Item", "Value"],
        [
            ["Processor model given to QEMU", f"`{test['cpu_without_nx']}`"],
            ["Self-test checks passed", run["selftest_passed"]],
            ["Self-test checks skipped", run["selftest_skipped"]],
            ["Hostile attempts denied", f"{run['rogue_denied']} of {run['rogue_total']}"],
            ["Attempts not denied", "; ".join(run["not_denied"])],
        ],
    )


def model_table(model: dict) -> str:
    scenarios = model["scenarios"]
    measures = (
        ("depth", "Depth reached"),
        ("states", "States"),
        ("transitions", "Steps"),
        ("violations", "Violations"),
        ("deliveries", "Deliveries compared with what was sent"),
        ("replies", "Replies compared with what was sent"),
        ("restarts", "Restarts compared with a fresh boot"),
        ("beyond_budget", "Steps left out because a budget was reached"),
        ("pairs", "Ordered pairs of failure kinds met"),
    )
    rows = [[label, *[scenario[key] for scenario in scenarios]] for key, label in measures]
    rows += [
        [f"Steps that ended with: {code}", *[scenario["results"][code] for scenario in scenarios]]
        for code in scenarios[0]["results"]
    ]
    return claims.table(["Measure", *[f"`{scenario['name']}`" for scenario in scenarios]], rows)


def mutants_table(first: dict, held_out: dict) -> str:
    """Both samples side by side: mutant N of each comes from stretch N of the same ordered list of places."""

    def place(mutant: dict) -> str:
        change = mutant["operator"].replace("|", "\\|")
        return f"`{mutant['file'].removeprefix('kernel/src/')}:{mutant['line']}` {change}"

    def outcome(mutant: dict) -> str:
        if mutant["verdict"] != "caught":
            return mutant["verdict"].upper()
        return f"{mutant['how']}, {mutant['by']}"

    return claims.table(
        ["Stretch", "First sample", "Outcome", "Held-out sample", "Outcome"],
        [
            [one["number"], place(one), outcome(one), place(other), outcome(other)]
            for one, other in zip(first["mutants"], held_out["mutants"], strict=True)
        ],
    )


def main() -> int:
    test = gates.load("test.json")
    result = gates.load("gates.json")
    web = gates.load("web.json")
    parts = [
        "# Inti: review dossier",
        "This file is generated by `tools/dossier.py` from the repository. It contains the four documents "
        "as rendered by `tools/claims.py`, pieces of the source code copied verbatim, and the raw measurements "
        "behind the summary figures. Links and images are relative to the repository.",
        claims.table(
            ["Item", "Value"],
            [
                ["Gate run", f"{result['date']} ({result['mode']} run)"],
                ["Gates passed", f"{result['passed']} of {result['total']}"],
                ["Disk image, SHA-256", f"`{result['image_sha256']}`"],
                ["Image tested in QEMU", f"`{test['image_sha256']}`"],
                ["Image tested in the browser", f"`{web['image_sha256']}`"],
                ["Emulator", test["emulator"]],
                ["Processor model given to QEMU", f"`{test['cpu']}`"],
                ["Toolchain", f"{result['toolchain']['rustc']}; Python {result['toolchain']['python']}"],
            ],
        ),
    ]
    for document in DOCUMENTS:
        parts.append(f"---\n\n# Document: `{document}`")
        parts.append(demoted(claims.source_text(document)))

    parts.append("---\n\n# Source code, verbatim")
    for path, name, why in EXCERPTS:
        parts.append(f"## `{name}` in `{path}`\n\n{why}\n\n```rust\n{function(path, name)}\n```")
    for path, why in WHOLE_FILES:
        parts.append(f"## `{path}`\n\n{why}\n\n```rust\n{claims.source_text(path).strip()}\n```")

    parts.append("---\n\n# Raw measurements")
    parts.append("## Every recovery time\n\n" + samples_table(test))
    parts.append("## Every double-failure time\n\n" + double_table(test))
    parts.append("## The boot without the no-execute bit\n\n" + no_nx_table(test))
    parts.append(
        "## The browser check\n\n"
        + claims.table(
            ["Command typed in the browser", "Result", "Seconds"],
            [[f"`{name}`", "pass" if entry["ok"] else "FAIL", entry["seconds"]] for name, entry in web["commands"].items()],
        )
    )
    parts.append("## The model run, scenario by scenario\n\n" + model_table(gates.load("model.json")))
    parts.append(
        "## Every sampled mutant\n\n"
        "Both samples, stretch by stretch; the files are in `kernel/src`. An outcome says how the changed kernel was "
        "caught and by which run of the model: `tables` are the directed tests, `explore` is an exploration. "
        "NOT CAUGHT marks a survivor, and each survivor is explained in the design document above.\n\n"
        + mutants_table(gates.load("mutants.json"), gates.load("mutants-held-out.json"))
    )
    text = "\n\n".join(parts) + "\n"
    OUTPUT.write_text(text, encoding="utf-8", newline="\n")
    print(f"dossier: {len(text):,} characters -> {OUTPUT.name}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
