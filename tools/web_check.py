"""Boots the disk image in the browser demo (v86 in headless Chromium) and checks it.

    python tools/web_check.py [--timeout SECONDS] [--no-evidence] [command ...]

The page in docs/ is served from a local port, opened in headless Chromium, and watched
until the shell prompt arrives on the emulated serial port. Each command is then typed
and its output checked. Results go to evidence/web.json and a picture of the page to
docs/img/web-demo.png. Exit code 0 means everything held.

Needs the Python package `playwright` and its Chromium (`playwright install chromium`).
"""
import argparse
import functools
import hashlib
import http.server
import json
import os
import re
import sys
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DOCS = ROOT / "docs"
EVIDENCE = ROOT / "evidence"
PROMPT = "inti> "
DEFAULT_COMMANDS = ["hello", "selftest", "rogue", "crash console", "hang fs", "df",
                    "write note.txt kept", "crash disk", "cat note.txt", "tour", "uptime",
                    "desktop", "@wait 5", "crash display", "@wait 5", "windows"]
# The colour of the empty desktop (user/src/bin/display.rs, PALETTE[1]) and of the frames.
DESKTOP_COLOUR = (0x1E, 0x38, 0x5C)
TOUR_LIMIT_S = 60  # the page says the guided tour takes under a minute

# What a command's output must contain. A command not listed here only has to come back
# to the prompt without a kernel panic.
EXPECT = {
    # Nothing may follow the count: the self-test appends what it skipped, and in v86, which has
    # the no-execute bit, it must skip nothing.
    "selftest": r"SELFTEST: PASS (\d+)/\1\s*\n",
    "rogue": r"ROGUE: (\d+)/\1 attempts denied",
    "uptime": r"kernel invariants: all hold",
    "crash console": r"RECOVERY: console crash -> answering again after \d+ us",
    "hang fs": r"RECOVERY: fs hang -> answering again after \d+ us",
    # The disk in the browser is the image held in memory: what is written lasts until the page
    # is loaded again. It must still outlast the disk server, which is crashed in between.
    "df": r"disk: \d+ files?, \d+ of \d+ blocks",
    "crash disk": r"RECOVERY: disk crash -> answering again after \d+ us",
    "cat note.txt": r"(?m)^kept\s*$",
    "tour": r"TOUR: done",
    "crash display": r"RECOVERY: display crash -> answering again after \d+ us",
    # Both programs have opened a window again after the display was restarted.
    "windows": r"WINDOWS: 2 of programs, \d+ frames drawn, display restarted [1-9]\d* times",
}


class Quiet(http.server.SimpleHTTPRequestHandler):
    extensions_map = {
        **http.server.SimpleHTTPRequestHandler.extensions_map,
        ".wasm": "application/wasm",
        ".js": "text/javascript",
        ".img": "application/octet-stream",
        ".bin": "application/octet-stream",
    }

    def log_message(self, *_):
        pass


def serve(directory: Path) -> http.server.ThreadingHTTPServer:
    handler = functools.partial(Quiet, directory=str(directory))
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def launch_browser(playwright):
    """Playwright's own Chromium first; an installed Edge or Chrome if that is missing."""
    problems = []
    for channel in (None, "msedge", "chrome"):
        try:
            if channel is None:
                return playwright.chromium.launch(headless=True)
            return playwright.chromium.launch(headless=True, channel=channel)
        except Exception as error:  # the next browser may still work
            problems.append(f"{channel or 'chromium'}: {str(error).splitlines()[0]}")
    sys.exit("web: no browser could be started: " + " | ".join(problems))


def run(commands: list[str], timeout: float, save: bool) -> int:
    # The local page must not go through a proxy.
    for name in ("HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy", "ALL_PROXY"):
        os.environ.pop(name, None)
    os.environ["NO_PROXY"] = "127.0.0.1,localhost"
    # Playwright's dependency checker (winldd) is a separate download; the browser runs without it.
    os.environ.setdefault("PLAYWRIGHT_SKIP_VALIDATE_HOST_REQUIREMENTS", "1")
    from playwright.sync_api import Error, sync_playwright

    image = DOCS / "inti.img"
    built = ROOT / "build" / "inti.img"
    result = {
        "emulator": "v86 0.5.473 (WebAssembly)",
        "image_sha256": sha256(image),
        "same_image_as_build": built.exists() and sha256(built) == sha256(image),
        "commands": {},
        "page_errors": [],
    }
    failures = []
    server = serve(DOCS)
    url = f"http://127.0.0.1:{server.server_address[1]}/index.html"
    with sync_playwright() as playwright:
        browser = launch_browser(playwright)
        result["browser"] = f"{browser.browser_type.name} {browser.version} (headless)"
        page = browser.new_page(viewport={"width": 1500, "height": 1150})
        page.on("pageerror", lambda error: result["page_errors"].append(str(error)))
        started = time.perf_counter()
        page.goto(url)
        try:
            page.wait_for_function("window.inti && window.inti.ready", timeout=timeout * 1000)
            result["boot_seconds"] = round(time.perf_counter() - started, 2)
            print(f"web: shell prompt after {result['boot_seconds']} s", flush=True)
            result["wx_enforced"] = "W^X enforced" in page.evaluate("window.inti.serial")
            if not result["wx_enforced"]:
                failures.append("the boot log does not say that W^X is enforced")
            for line in commands:
                if line.startswith("@wait "):
                    page.wait_for_timeout(float(line.split()[1]) * 1000)
                    continue
                mark = page.evaluate("window.inti.serial.length")
                began = time.perf_counter()
                page.evaluate("line => window.inti.send(line)", line)
                page.wait_for_function(
                    "mark => window.inti.serial.slice(mark).includes('inti> ')",
                    arg=mark,
                    timeout=timeout * 1000,
                )
                output = page.evaluate("mark => window.inti.serial.slice(mark)", mark)
                seconds = round(time.perf_counter() - began, 2)
                wanted = EXPECT.get(line.strip())
                good = "KERNEL PANIC" not in output
                if wanted:
                    good = good and re.search(wanted, output) is not None
                if line.strip() == "tour":
                    good = good and seconds < TOUR_LIMIT_S
                result["commands"][line] = {"ok": good, "seconds": seconds, "output": output}
                print(f"web: {line}: {'ok' if good else 'FAILED'} ({seconds} s)", flush=True)
                if not good:
                    failures.append(line)
        except Error as error:
            failures.append(f"timeout or browser error: {str(error).splitlines()[0]}")
        result["serial"] = page.evaluate("window.inti ? window.inti.serial : ''")
        if failures:
            print("web: the serial log ended with:", flush=True)
            for row in result["serial"].replace("\r", "").split("\n")[-25:]:
                print(f"web:   | {row}", flush=True)
        # The screen is in graphics mode: the emulator draws it on the canvas.
        result["canvas"] = page.evaluate(
            """() => {
                const canvas = document.querySelector('#screen canvas');
                const pixel = (x, y) => Array.from(canvas.getContext('2d').getImageData(x, y, 1, 1).data.slice(0, 3));
                return {width: canvas.width, height: canvas.height, shown: canvas.offsetWidth > 0,
                        corner: pixel(canvas.width - 4, canvas.height - 4)};
            }"""
        )
        canvas = result["canvas"]
        if (canvas["width"], canvas["height"]) != (640, 480) or not canvas["shown"] or any(
                abs(got - wanted) > 6 for got, wanted in zip(canvas["corner"], DESKTOP_COLOUR)):
            failures.append(f"the screen is not the 640 by 480 desktop: {canvas}")
        if save:
            (DOCS / "img").mkdir(exist_ok=True)
            page.screenshot(path=str(DOCS / "img" / "web-demo.png"))
    if result["page_errors"]:
        failures.append("page errors: " + "; ".join(result["page_errors"][:3]))
    if not result["same_image_as_build"]:
        failures.append("docs/inti.img is not the image in build/ (run build.py)")
    result["ok"] = not failures
    result["failures"] = failures
    if save:
        EVIDENCE.mkdir(exist_ok=True)
        (EVIDENCE / "web.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    for failure in failures:
        print(f"web: FAILED: {failure}", flush=True)
    print("web: PASS" if not failures else "web: FAIL", flush=True)
    return 0 if not failures else 1


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--timeout", type=float, default=180, help="seconds to wait for each step")
    parser.add_argument("--no-evidence", action="store_true", help="do not write evidence or pictures")
    parser.add_argument("commands", nargs="*", default=DEFAULT_COMMANDS)
    options = parser.parse_args()
    code = run(options.commands, options.timeout, not options.no_evidence)
    sys.stdout.flush()
    # Closing the browser politely can hang on Windows; everything is already written.
    os._exit(code)


if __name__ == "__main__":
    main()
