
"""Reproduce the fuzzer's page-frame count rising by five, then test the correction.

Stage 1: the shell waits 20 ms between starting a program and unmapping the file it loaded.
         Expected if the diagnosis is right: the fuzzer reports page frames N -> N + 5.
Stage 2: the same shell delay, and the fuzzer lets its starter run before the first reading.
         Expected: FUZZ: PASS.
The shell delay is taken out again at the end; the fuzzer's correction stays.
"""
import os
import shutil
import subprocess
import sys

worktree = sys.argv[1]
shell_path = os.path.join(worktree, "user", "src", "bin", "shell.rs")
fuzz_path = os.path.join(worktree, "user", "src", "bin", "fuzz.rs")
keep = os.path.join(worktree, "evidence", "fuzz-launch-race")
os.makedirs(keep, exist_ok=True)


def swap(path, old, new):
    content = open(path, encoding="utf-8", newline="").read()
    newline = "\r\n" if "\r\n" in content else "\n"
    old_text = old.replace("\n", newline)
    new_text = new.replace("\n", newline)
    assert content.count(old_text) == 1, (path, content.count(old_text))
    with open(path, "w", encoding="utf-8", newline="") as handle:
        handle.write(content.replace(old_text, new_text))


def run_suite(label):
    done = subprocess.run(
        [sys.executable, "build.py", "test", "--fuzz", "20000", "--runs", "1"],
        cwd=worktree,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    serial = os.path.join(worktree, "evidence", "test_serial.log")
    kept = os.path.join(keep, label + ".serial.log")
    shutil.copyfile(serial, kept)
    print("STAGE", label, "exit", done.returncode)
    for row in open(kept, encoding="utf-8", errors="replace").read().splitlines():
        if row.startswith("FUZZ:") or row.startswith("SELFTEST:"):
            print("   ", row)
    for row in done.stdout.splitlines()[-6:]:
        print("   |", row)
    sys.stdout.flush()


plain = "        let started = sys::spawn(image.bytes(), &spawn);\n        drop(image);\n"
delayed = "        let started = sys::spawn(image.bytes(), &spawn);\n        sys::sleep(20);\n        drop(image);\n"
first_reading = "        let before = usage(self.me);\n        let started = sys::time();\n"
corrected = (
    "        // The program that started this one may still hold the pages it loaded this\n"
    "        // program's file into, and unmaps them as soon as it runs again. Let it run first:\n"
    "        // otherwise those pages count as taken before and as free after.\n"
    "        sys::sleep(STARTER_MS);\n"
    "        let before = usage(self.me);\n"
    "        let started = sys::time();\n"
)
settle = "const SETTLE_US: u64 = 2_000_000;\n"
settle_and_starter = (
    "const SETTLE_US: u64 = 2_000_000;\n"
    "/// How long the fuzzer sleeps before its first reading, so that its starter has given back\n"
    "/// the pages it loaded the fuzzer's file into.\n"
    "const STARTER_MS: u32 = 50;\n"
)

swap(shell_path, plain, delayed)
try:
    run_suite("1-shell-delay-old-fuzzer")
    swap(fuzz_path, first_reading, corrected)
    swap(fuzz_path, settle, settle_and_starter)
    run_suite("2-shell-delay-corrected-fuzzer")
finally:
    swap(shell_path, delayed, plain)
print("EXPERIMENT_DONE")
