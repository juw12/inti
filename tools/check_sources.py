"""Fetch every cited source and look for the words that are quoted from it.

    python tools/check_sources.py [--proxy http://host:port]

Reads docs/src/sources.json and writes evidence/sources.json. For each source the script
fetches the cited address and looks for the quoted words in its text: the visible text of a
web page, or the text of a PDF as far as the small reader below can extract it. An entry may
name a second copy (the paper's abstract in its OpenAlex record, or the same text on another
site) for an address that refuses scripts or cannot be reached; the script then looks there
and says so. A source that cannot be confirmed is reported as such, never dropped.

This needs the network, so `build.py gates` does not run it; the documents show the saved
result with its date.
"""
from __future__ import annotations

import argparse
import datetime
import gzip
import html
import json
import re
import sys
import urllib.error
import urllib.request
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SOURCES = ROOT / "docs" / "src" / "sources.json"
RESULT = ROOT / "evidence" / "sources.json"
TIMEOUT_S = 40
BROWSER = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36"
PDF_HEADER_WITHIN = 1024  # a PDF may begin with other bytes; its header has to lie in this range
WORD_GAP = -180  # a step this wide between two strings of a TJ array is a space (1/1000 of the font size)
PDF_ESCAPES = dict.fromkeys("nrtbf", " ")
TEX_LIGATURES = {"\x0b": "ff", "\x0c": "fi", "\x0d": "fl", "\x0e": "ffi", "\x0f": "ffl"}
PDF_STREAM = re.compile(rb"stream\r?\n(.*?)endstream", re.S)
PDF_STRING = rb"\(((?:[^()\\]|\\.)*)\)"
SHOW_TEXT = re.compile(rb"\[((?:[^\]\\]|\\.)*)\]\s*TJ|" + PDF_STRING + rb"\s*Tj", re.S)
ARRAY_ITEM = re.compile(PDF_STRING + rb"|(-?\d+(?:\.\d+)?)", re.S)


def squeeze(text: str) -> str:
    """Text in lower case with every run of white space as one space."""
    return re.sub(r"\s+", " ", text).lower()


def visible_text(page: bytes) -> str:
    """The text of an HTML page without scripts, styles and tags."""
    text = page.decode("utf-8", errors="replace")
    text = re.sub(r"(?is)<(script|style)\b.*?</\1>", " ", text)
    return html.unescape(re.sub(r"(?s)<[^>]+>", " ", text))


def pdf_string(raw: bytes) -> str:
    """A PDF literal string as text: escapes resolved, the ligature codes of TeX fonts spelled out."""
    def resolve(escape: re.Match) -> str:
        code = escape.group(1)
        if code[0] in "01234567":
            return chr(int(code, 8))
        return PDF_ESCAPES.get(code, code)

    text = re.sub(r"(?s)\\([0-7]{1,3}|.)", resolve, raw.decode("latin-1"))
    return "".join(TEX_LIGATURES.get(letter, letter) for letter in text)


def pdf_text(document: bytes) -> str:
    """The text a PDF shows, as far as a small reader can tell.

    Every Flate-compressed stream is inflated and the strings of its Tj and TJ operators are
    joined. That recovers ordinary text set in standard fonts. A font with its own encoding
    comes out as noise, and then the quoted words are simply not found.
    """
    pieces = []
    for stream in PDF_STREAM.finditer(document):
        try:
            content = zlib.decompress(stream.group(1))
        except zlib.error:
            continue
        for array, single in SHOW_TEXT.findall(content):
            if single:
                pieces.append(pdf_string(single))
            for string, step in ARRAY_ITEM.findall(array):
                if string:
                    pieces.append(pdf_string(string))
                elif step and float(step) < WORD_GAP:
                    pieces.append(" ")
            pieces.append(" ")
    return "".join(pieces)


def openalex_abstract(page: bytes) -> str | None:
    """The abstract in an OpenAlex work record, stored there as word -> positions; None without one."""
    try:
        record = json.loads(page)
    except ValueError:
        return None
    index = record.get("abstract_inverted_index") if isinstance(record, dict) else None
    if not index:
        return None
    placed = {position: word for word, positions in index.items() for position in positions}
    return " ".join(placed[position] for position in sorted(placed))


def readable(page: bytes) -> tuple[str, str]:
    """(what kind of document this is, its text squeezed for searching)."""
    if b"%PDF" in page[:PDF_HEADER_WITHIN]:
        return "PDF", squeeze(pdf_text(page))
    abstract = openalex_abstract(page) if page.lstrip().startswith(b"{") else None
    if abstract is not None:
        return "abstract", squeeze(abstract)
    return "page", squeeze(visible_text(page))


def look(opener: urllib.request.OpenerDirector, url: str, phrases: list[str]) -> tuple[bool, str]:
    """(every phrase found, what happened in words) for one address."""
    request = urllib.request.Request(url, headers={"User-Agent": BROWSER, "Accept": "*/*"})
    try:
        with opener.open(request, timeout=TIMEOUT_S) as response:
            page = response.read()
    except urllib.error.HTTPError as error:
        return False, f"answered HTTP {error.code} to the script"
    except (urllib.error.URLError, OSError) as error:
        return False, f"could not be reached from the machine that ran the check ({type(error).__name__})"
    if page.startswith(b"\x1f\x8b"):
        page = gzip.decompress(page)
    kind, text = readable(page)
    if all(squeeze(phrase) in text for phrase in phrases):
        return True, kind
    return False, f"was fetched, but the quoted words were not found in the text of the {kind}"


def check(opener: urllib.request.OpenerDirector, source: dict) -> tuple[str, str]:
    """(status, result in words) for one source; the status is confirmed, copy or unconfirmed."""
    found, detail = look(opener, source["url"], source["phrases"])
    if found:
        return "confirmed", f"confirmed: the quoted words are in the text of the cited {detail}"
    copy = source.get("copy")
    if copy is None:
        return "unconfirmed", f"not confirmed: the cited address {detail}"
    found, copy_detail = look(opener, copy["url"], source["phrases"])
    if found:
        return "copy", (
            f"confirmed on a second copy: the cited address {detail}; "
            f"the quoted words are in {copy['what']} ({copy['url']})"
        )
    return "unconfirmed", f"not confirmed: the cited address {detail}; the second copy ({copy['url']}) {copy_detail}"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--proxy", help="HTTP proxy to use, for example http://localhost:3128")
    options = parser.parse_args()
    proxies = {"http": options.proxy, "https": options.proxy} if options.proxy else None
    opener = urllib.request.build_opener(urllib.request.ProxyHandler(proxies))

    sources = json.loads(SOURCES.read_text(encoding="utf-8"))
    unquoted = [source["title"] for source in sources if not source["phrases"]]
    if unquoted:
        sys.exit(f"check_sources: no quoted words given for: {'; '.join(unquoted)}")

    entries = []
    for source in sources:
        status, result = check(opener, source)
        entries.append({**source, "status": status, "result": result})
        print(f"{status:<12} {source['url']}", flush=True)
    document = {
        "date": datetime.datetime.now().astimezone().isoformat(timespec="seconds"),
        "sources": entries,
    }
    RESULT.parent.mkdir(parents=True, exist_ok=True)
    RESULT.write_text(json.dumps(document, indent=1, ensure_ascii=False) + "\n", encoding="utf-8", newline="\n")
    cited = sum(1 for entry in entries if entry["status"] == "confirmed")
    copies = sum(1 for entry in entries if entry["status"] == "copy")
    print(
        f"sources: {cited + copies} of {len(entries)} confirmed "
        f"({cited} at the cited address, {copies} on a second copy) -> evidence/sources.json",
        flush=True,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
