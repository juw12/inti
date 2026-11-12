"""Check that every number the documents show is measured, cited, or taken from the source code.

    python tools/check_claims.py

This is gate G9. It prints each problem as `file:line: what is wrong` and exits with 0 only
if there is none. It checks six things:

1. evidence/claims.json is what tools/claims.py computes right now from the evidence files
   and the source code.
2. README.md and the documents in docs/ are their templates in docs/src/, rendered right now.
3. Prose (the templates, and the visible text of docs/index.html) contains no bare number.
   A number is a {{placeholder}}, or part of a name (x86, seL4, ring 0, ...), or it stands
   in a paragraph, list item or table row that carries a source URL.
4. A number in code formatting occurs as a literal in the source code or the build scripts.
5. Every line of a log excerpt is a line of the raw log it was cut from.
6. The figures of the model and of all three samples of mutants were measured on the sources as
   they are now. Each of those results carries a fingerprint of every Rust file in
   kernel/src, abi/src and model/src; a result with another fingerprint is refused.
"""
from __future__ import annotations

import html
import json
import re
import sys
from pathlib import Path
from typing import Iterator

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))
import claims  # noqa: E402
import gates  # noqa: E402

PAGE = "docs/index.html"
SOURCE_FOLDERS = ("kernel", "abi", "boot", "user")
# Names that contain a number. They are taken out of prose before it is searched for numbers.
NAMES = (
    "ring 0", "ring 3", "16-bit", "32-bit", "64-bit", "MINIX 3", "Python 3", "Windows 10",
    "BSD 2-Clause", "channel 0", "channel 2", "8254", "8259", "16550",
)
NUMBER = re.compile(r"(?<![\w.])(0x[0-9A-Fa-f_]+|\d[\d_]*)(?![\w.])")
SOURCE_NUMBER = re.compile(r"(?<![\w.])(0x[0-9A-Fa-f_]+|\d[\d_]*?)(?:_?[iu](?:8|16|32|64|size))?(?![\w.])")
FENCE = re.compile(r"^\s*```\s*(\w*)\s*$")
INLINE_CODE = re.compile(r"`[^`]*`")
BLOCK_START = re.compile(r"^\s*(\||#|[-*+] |\d+\. )")
LIST_NUMBER = re.compile(r"\d+\.")
# Fence label -> the claims it may show.
LOG_FENCES = {"log": "log.", "weblog": "weblog.", "nonxlog": "nonxlog.", "modellog": "modellog."}

Problem = tuple[str, int, str]
Lines = list[tuple[int, str]]


def value_of(literal: str) -> int:
    digits = literal.replace("_", "")
    return int(digits, 16) if digits.lower().startswith("0x") else int(digits)


def source_numbers() -> set[int]:
    """Every integer literal in the Rust sources, the linker scripts and the build scripts."""
    paths = [ROOT / "build.py", *sorted((ROOT / "tools").glob("*.py"))]
    for folder in SOURCE_FOLDERS:
        paths += sorted((ROOT / folder).rglob("*.rs")) + sorted((ROOT / folder).glob("*.ld"))
    return {
        value_of(literal)
        for path in paths
        for literal in SOURCE_NUMBER.findall(path.read_text(encoding="utf-8"))
    }


def blocks(text: str) -> Iterator[tuple[str | None, Lines]]:
    """Split Markdown into blocks of numbered lines.

    A block is a paragraph, a list item, a table row or a heading (label None), or the inside
    of a fenced block (label: the word after the opening fence, "" if there is none).
    """
    label: str | None = None
    current: Lines = []
    for number, line in enumerate(text.splitlines(), start=1):
        fence = FENCE.match(line)
        if label is not None:
            if fence and not fence.group(1):
                yield label, current
                label, current = None, []
            else:
                current.append((number, line))
            continue
        starts_block = bool(fence) or not line.strip() or bool(BLOCK_START.match(line))
        if starts_block and current:
            yield None, current
            current = []
        if fence:
            label = fence.group(1)
        elif line.strip():
            current.append((number, line))
    if current:
        yield label, current


def bare_numbers(line: str) -> list[str]:
    """The numbers in a line of prose that are neither a placeholder nor part of a name."""
    text = claims.PLACEHOLDER.sub(" ", line)
    text = INLINE_CODE.sub(" ", text)
    text = re.sub(r"\]\([^)]*\)", "] ", text)          # link targets
    text = re.sub(r"<!--.*?-->|<[^>]+>", " ", text)    # comments and tags
    for name in NAMES:
        text = re.sub(rf"(?<!\w){re.escape(name)}(?!\w)", " ", text)
    found = []
    for position, token in enumerate(text.split()):
        word = token.strip(".,;:!?()[]\"'*_|")
        if not any(character.isdigit() for character in word):
            continue
        if word[0].isalpha():                           # x86, seL4, i586, COM1, G1
            continue
        if position == 0 and LIST_NUMBER.fullmatch(token):
            continue
        found.append(word)
    return found


def code_numbers(path: str, number: int, code: str, known: set[int]) -> list[Problem]:
    """Problems for the numbers in a piece of code formatting that the source code does not contain."""
    return [
        (path, number, f'the number "{literal}" is in code formatting but not in the source code')
        for literal in NUMBER.findall(claims.PLACEHOLDER.sub(" ", code))
        if value_of(literal) not in known
    ]


def check_claims_file(computed: dict) -> list[Problem]:
    path = claims.CLAIMS.relative_to(ROOT).as_posix()
    if not claims.CLAIMS.exists():
        return [(path, 0, "missing; run tools/claims.py")]
    on_disk = json.loads(claims.CLAIMS.read_text(encoding="utf-8"))
    return [
        (path, 0, f"the claim {key} is not what the evidence gives now; run tools/claims.py")
        for key in sorted(set(computed) | set(on_disk))
        if computed.get(key) != on_disk.get(key)
    ]


def check_documents(computed: dict) -> list[Problem]:
    problems: list[Problem] = []
    for output, expected in claims.render_all(computed).items():
        path = ROOT / output
        if not path.exists():
            problems.append((output, 0, "missing; run tools/claims.py"))
            continue
        on_disk = path.read_text(encoding="utf-8").replace("\r\n", "\n").splitlines()
        wanted = expected.splitlines()
        if on_disk != wanted:
            differs = next(
                (index for index, pair in enumerate(zip(on_disk, wanted), start=1) if pair[0] != pair[1]),
                min(len(on_disk), len(wanted)) + 1,
            )
            problems.append((output, differs, "differs from its template rendered now; run tools/claims.py"))
    return problems


def check_template(path: str, known: set[int]) -> tuple[list[Problem], int]:
    """Rules 3 and 4 for one template. Also returns how many placeholders it uses."""
    text = claims.source_text(path)
    problems: list[Problem] = []
    for label, lines in blocks(text):
        if label is None:
            joined = " ".join(line for _, line in lines)
            cited = "http://" in joined or "https://" in joined
            for number, line in lines:
                if not cited:
                    problems += [
                        (path, number, f'the number "{word}" is neither a placeholder nor cited with a URL')
                        for word in bare_numbers(line)
                    ]
                for span in INLINE_CODE.findall(line):
                    problems += code_numbers(path, number, span, known)
        elif label in LOG_FENCES:
            for number, line in lines:
                keys = [match.group(1) for match in claims.PLACEHOLDER.finditer(line)]
                only_placeholders = not claims.PLACEHOLDER.sub("", line).strip()
                if not only_placeholders or any(not key.startswith(LOG_FENCES[label]) for key in keys):
                    problems.append((path, number, f"a {label} block may hold only {LOG_FENCES[label]}* placeholders"))
        else:
            for number, line in lines:
                problems += code_numbers(path, number, line, known)
    return problems, len(claims.PLACEHOLDER.findall(text))


def check_page() -> list[Problem]:
    """Rule 3 for the visible text of the demo page."""

    def blank(match: re.Match) -> str:
        return " " + "\n" * match.group(0).count("\n")  # keeps the line numbers

    text = claims.source_text(PAGE)
    text = re.sub(r"(?s)<(script|style)\b.*?</\1>", blank, text)
    text = re.sub(r"(?s)<!--.*?-->|<[^>]+>", blank, text)
    return [
        (PAGE, number, f'the number "{word}" is on the page but is not measured')
        for number, line in enumerate(html.unescape(text).splitlines(), start=1)
        for word in bare_numbers(line)
    ]


def check_excerpts() -> tuple[list[Problem], int]:
    """Rule 5 for the rendered documents. Also returns how many excerpt lines were compared."""
    raw = {
        "log": {line.rstrip() for line in claims.SERIAL_LOG.read_text(encoding="utf-8").splitlines()},
        "weblog": {line.rstrip() for line in gates.load("web.json")["serial"].splitlines()},
        "nonxlog": {line.rstrip() for line in claims.NONX_SERIAL_LOG.read_text(encoding="utf-8").splitlines()},
        "modellog": {
            line.rstrip()
            for name in ("model.log", "model-before-fixes.log")
            for line in claims.evidence_lines(name)
        },
    }
    problems: list[Problem] = []
    compared = 0
    for output in claims.TEMPLATES.values():
        path = ROOT / output
        if not path.exists():
            continue
        for label, lines in blocks(path.read_text(encoding="utf-8")):
            if label not in raw:
                continue
            for number, line in lines:
                compared += 1
                if line.rstrip() != claims.ELLIPSIS and line.rstrip() not in raw[label]:
                    problems.append((output, number, f"this line is not in the raw {label}"))
    return problems, compared


def check_fresh() -> list[Problem]:
    """Rule 6: the model, all four samples of mutants, the mutants in QEMU and the run under Miri were made
    on the kernel and the model as they are now."""
    fingerprint = claims.model_fingerprint()
    problems: list[Problem] = []
    for source, name, command in (
        (claims.MODEL, "model.json", "python build.py model"),
        (claims.MUTANTS, "mutants.json", "python build.py mutants"),
        (claims.MUTANTS_HELD_OUT, "mutants-held-out.json", "python build.py mutants --held-out"),
        (claims.MUTANTS_FRESH, "mutants-fresh.json", "python build.py mutants --fresh"),
        (claims.MUTANTS_UNSEEN, "mutants-unseen.json", "python build.py mutants --unseen"),
        (claims.MUTANTS_QEMU, "mutants-qemu.json", "python build.py mutants-qemu"),
        (claims.MUTANTS_QEMU_REPEATS, "mutants-qemu-repeats.json", "the repeat runs beside the unchanged kernel"),
        (claims.MIRI, "miri.json", "python build.py miri"),
    ):
        if gates.load(name).get("source_sha256") != fingerprint:
            problems.append((
                source, 0,
                f"not measured on the kernel and model source as they are now; run {command}",
            ))
    return problems


def main() -> int:
    problems: list[Problem] = []
    computed: dict = {}
    try:
        computed = claims.collect()
        problems += check_claims_file(computed)
        problems += check_documents(computed)
        problems += check_fresh()
    except (KeyError, OSError, ValueError) as error:
        problems.append(("evidence", 0, f"the claims cannot be computed or rendered: {error}"))

    known = source_numbers()
    placeholders = 0
    for template in claims.TEMPLATES:
        try:
            found, used = check_template(template, known)
        except OSError as error:
            found, used = [(template, 0, f"cannot be read: {error}")], 0
        problems += found
        placeholders += used
    try:
        problems += check_page()
        found, compared = check_excerpts()
        problems += found
    except OSError as error:
        problems.append(("docs", 0, f"cannot be read: {error}"))
        compared = 0

    for path, line, message in problems:
        print(f"{path}:{line}: {message}", flush=True)
    if problems:
        print(f"check_claims: {len(problems)} problems", flush=True)
        return 1
    print(
        f"check_claims: ok: {len(computed)} claims recomputed, {len(claims.TEMPLATES)} documents re-rendered, "
        f"{placeholders} placeholders, {compared} log lines compared with the raw logs",
        flush=True,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
