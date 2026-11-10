"""Developer helper: compile one package and print a compact list of compiler messages.

    python tools/cb.py kernel [max_messages]
"""
import json
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
import build  # noqa: E402


def main() -> int:
    package = sys.argv[1]
    limit = int(sys.argv[2]) if len(sys.argv) > 2 else 12
    cargo = build.find_tool("cargo", [Path.home() / ".cargo" / "bin" / "cargo.exe"])
    flags = [
        "-Clinker=rust-lld",
        "-Clinker-flavor=ld.lld",
        "-Crelocation-model=static",
        "-Clink-arg=--gc-sections",
        f"-Clink-arg=-T{ROOT / package / 'link.ld'}",
    ]
    env = dict(os.environ)
    env.pop("RUSTFLAGS", None)
    env["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(flags)
    command = [
        cargo, "build", "--release", "-p", package,
        "--target-dir", str(ROOT / "target" / package),
        "--message-format=json-diagnostic-short",
    ]
    result = subprocess.run(command, cwd=ROOT, env=env, capture_output=True, text=True, encoding="utf-8")
    errors = 0
    warnings = 0
    shown = 0
    for line in result.stdout.splitlines():
        try:
            record = json.loads(line)
        except ValueError:
            continue
        message = record.get("message")
        if record.get("reason") != "compiler-message" or not message:
            continue
        level = message.get("level")
        if level == "error":
            errors += 1
        elif level == "warning":
            warnings += 1
        else:
            continue
        if shown < limit:
            shown += 1
            text = message.get("rendered") or message.get("message")
            print(text.strip()[:700])
            for child in message.get("children", []):
                if child.get("level") in ("help", "note") and child.get("message"):
                    print("    " + child["message"][:300])
    tail = [line for line in result.stderr.splitlines() if line.startswith(("error", "warning: unused"))]
    for line in tail[:6]:
        print(line[:400])
    print(f"cb: {package}: exit {result.returncode}, {errors} errors, {warnings} warnings")
    return result.returncode


if __name__ == "__main__":
    sys.exit(main())
