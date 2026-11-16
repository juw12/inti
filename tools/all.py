#!/usr/bin/env python3
"""Everything that backs a number in the documents, with one command.

    python build.py all             the kernel model, then build, the storage test, the emulated
                                    machines, test and every gate
    python build.py all --mutants   first the five samples of mutants on the build machine, the
                                    survivors in QEMU and the faults planted in the disk server
                                    (hours, not minutes), then the same

The steps, in this order:

1. `python build.py model`           the kernel's own source on a fake machine: directed checks,
                                     planted corruptions, bounded exploration
2. with --mutants only:              `python build.py mutants`, `--held-out`, `--fresh`, `--unseen`,
                                     `--fifth`, then `python build.py mutants-qemu` and
                                     `python build.py storage-mutants`
3. `python build.py storage`         the disk server and the file system in QEMU: power cuts,
                                     damaged volumes, limits
4. `python build.py machines`        the same image on emulated machines that differ in processor,
                                     memory and disk controller
5. `python build.py gates`           clean build, the whole test in QEMU, gates G1 to G9

The build gives the same image byte for byte every time, so the image the storage test and
the machines measured is the one the gates judge; gate G9 compares the SHA-256 values and
refuses a result if they differ.

Without --mutants the results of the mutants are not measured again, but they are not taken on
trust either: gate G9 (tools/check_claims.py) refuses any result whose fingerprint is not that
of the sources as they are now.

A step that fails does not stop the later ones; each is reported, and the exit status is 0 only
if every step passed.
"""
from __future__ import annotations

import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SAMPLES = ([], ["--held-out"], ["--fresh"], ["--unseen"], ["--fifth"])


def step(arguments: list[str]) -> tuple[str, int, float]:
    name = "python build.py " + " ".join(arguments)
    print(f"ALL start: {name}", flush=True)
    started = time.monotonic()
    code = subprocess.run([sys.executable, str(ROOT / "build.py"), *arguments], cwd=ROOT).returncode
    return name, code, time.monotonic() - started


def main() -> int:
    arguments = sys.argv[1:]
    if any(argument != "--mutants" for argument in arguments):
        sys.exit(__doc__)
    done = [step(["model"])]
    if "--mutants" in arguments:
        done += [step(["mutants", *sample]) for sample in SAMPLES]
        done.append(step(["mutants-qemu"]))
        done.append(step(["storage-mutants"]))
    done.append(step(["storage"]))
    done.append(step(["machines"]))
    done.append(step(["gates"]))
    for name, code, seconds in done:
        print(f"ALL {'PASS' if code == 0 else 'FAIL'}  {seconds:6.0f} s  {name}", flush=True)
    failed = sum(code != 0 for _, code, _ in done)
    print(f"ALL: {len(done) - failed} of {len(done)} steps passed", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
