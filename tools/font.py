#!/usr/bin/env python3
"""Turn the Spleen 6x12 bitmap font into user/src/font.rs.

    python tools/font.py          write user/src/font.rs
    python tools/font.py --check  fail if user/src/font.rs is not what this script writes

The font is Spleen by Frederic Cambus (assets/spleen/LICENSE, two-clause BSD). Only the 95
printable ASCII characters are taken. The result is checked in, so a build needs neither
this script nor the font file.
"""
from __future__ import annotations

import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "assets" / "spleen" / "spleen-6x12.bdf"
TARGET = ROOT / "user" / "src" / "font.rs"
WIDTH, HEIGHT = 6, 12
FIRST, LAST = 32, 126


def glyphs() -> dict[int, list[int]]:
    """The bitmap rows of every wanted character: one byte per row, leftmost pixel in bit 7."""
    found: dict[int, list[int]] = {}
    code = None
    rows: list[int] | None = None
    for line in SOURCE.read_text(encoding="utf-8").splitlines():
        word, _, rest = line.partition(" ")
        if word == "ENCODING":
            code = int(rest)
        elif word == "BBX" and code is not None and FIRST <= code <= LAST:
            if rest.split()[:2] != [str(WIDTH), str(HEIGHT)]:
                sys.exit(f"font.py: character {code} is not {WIDTH}x{HEIGHT}")
        elif word == "BITMAP":
            rows = []
        elif word == "ENDCHAR":
            if code is not None and FIRST <= code <= LAST and rows is not None:
                if len(rows) != HEIGHT:
                    sys.exit(f"font.py: character {code} has {len(rows)} rows")
                found[code] = rows
            code, rows = None, None
        elif rows is not None:
            rows.append(int(line.strip(), 16) & 0xFC)
    missing = [code for code in range(FIRST, LAST + 1) if code not in found]
    if missing:
        sys.exit(f"font.py: the font lacks the characters {missing}")
    return found


def render() -> str:
    found = glyphs()
    lines = [
        "//! The screen font: Spleen 6x12 by Frederic Cambus (two-clause BSD licence, see",
        "//! assets/spleen/LICENSE), the 95 printable ASCII characters.",
        "//!",
        "//! Written by tools/font.py; do not edit by hand.",
        "",
        "pub const WIDTH: usize = 6;",
        "pub const HEIGHT: usize = 12;",
        "pub const FIRST: u8 = 32;",
        "pub const LAST: u8 = 126;",
        "",
        "/// One byte per row of a character, the leftmost pixel in bit 7.",
        "pub static GLYPHS: [[u8; HEIGHT]; 95] = [",
    ]
    for code in range(FIRST, LAST + 1):
        row = ", ".join(f"0x{value:02X}" for value in found[code])
        lines.append(f"    [{row}], // {chr(code)!r}")
    lines += [
        "];",
        "",
        "/// The rows of `byte`; anything that is not printable ASCII is drawn as a blank.",
        "pub fn glyph(byte: u8) -> &'static [u8; HEIGHT] {",
        "    let index = if (FIRST..=LAST).contains(&byte) { byte - FIRST } else { 0 };",
        "    &GLYPHS[index as usize]",
        "}",
        "",
    ]
    return "\n".join(lines)


def main() -> int:
    text = render()
    if "--check" in sys.argv:
        if TARGET.read_text(encoding="utf-8") != text:
            sys.exit("font.py: user/src/font.rs is out of date; run python tools/font.py")
        return 0
    TARGET.write_text(text, encoding="utf-8", newline="\n")
    print(f"font.py: wrote {TARGET.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
