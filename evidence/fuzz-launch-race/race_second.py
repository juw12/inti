
"""Stage 3: the shell delay again, with the selftest corrected as well: expected 96/96 and FUZZ: PASS.
Then the delay is taken out and the default full test is run; a failed full run is kept and repeated."""
import os
import shutil
import subprocess
import sys

worktree = sys.argv[1]
shell_path = os.path.join(worktree, "user", "src", "bin", "shell.rs")
evidence = os.path.join(worktree, "evidence")
keep = os.path.join(evidence, "fuzz-launch-race")


def swap(path, old, new):
    content = open(path, encoding="utf-8", newline="").read()
    newline = "\r\n" if "\r\n" in content else "\n"
    old_text = old.replace("\n", newline)
    new_text = new.replace("\n", newline)
    assert content.count(old_text) == 1, (path, content.count(old_text))
    with open(path, "w", encoding="utf-8", newline="") as handle:
        handle.write(content.replace(old_text, new_text))


def run_suite(extra):
    done = subprocess.run(
        [sys.executable, "build.py", "test", *extra],
        cwd=worktree,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    serial = open(os.path.join(evidence, "test_serial.log"), encoding="utf-8", errors="replace").read().splitlines()
    for row in serial:
        if row.startswith(("FUZZ:", "SELFTEST:")) or row.strip().startswith("FAIL "):
            print("   ", row.strip()[:200])
    for row in done.stdout.splitlines():
        if row.startswith("test: FAIL") or row.startswith("test: PASS") or "test: not ok" in row or row.startswith("test: fail"):
            print("   |", row[:260])
    print("   | last:", done.stdout.splitlines()[-1][:200] if done.stdout.splitlines() else "(nothing)")
    sys.stdout.flush()
    return done.returncode


plain = "        let started = sys::spawn(image.bytes(), &spawn);\n        drop(image);\n"
delayed = "        let started = sys::spawn(image.bytes(), &spawn);\n        sys::sleep(20);\n        drop(image);\n"

swap(shell_path, plain, delayed)
try:
    print("STAGE 3-shell-delay-both-corrected exit", run_suite(["--fuzz", "20000", "--runs", "1"]))
    shutil.copyfile(
        os.path.join(evidence, "test_serial.log"),
        os.path.join(keep, "3-shell-delay-both-corrected.serial.log"),
    )
finally:
    swap(shell_path, delayed, plain)

for attempt in (1, 2, 3):
    code = run_suite([])
    print("FULL TEST attempt", attempt, "exit", code)
    if code == 0:
        break
    failed = os.path.join(evidence, "full-test-failed-attempts", str(attempt))
    os.makedirs(failed, exist_ok=True)
    for name in ("test.json", "test_serial.log", "test_nonx_serial.log", "test_fullmem_serial.log"):
        source = os.path.join(evidence, name)
        if os.path.exists(source):
            shutil.copyfile(source, os.path.join(failed, name))
print("SECOND_DONE")
