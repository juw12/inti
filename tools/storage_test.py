"""Put the disk server and its file system through what a disk has to survive, in QEMU.

    python tools/storage_test.py                  the full run (several minutes)
    python tools/storage_test.py --cuts 8 --fuzz 6    a short run while developing

Every case boots a copy of build/inti.img that keeps what the machine writes, and then
reads the copy on this computer with tools/intifs.py, which shares no code with the disk
server. The cases:

    persistence   files written before the machine is switched off are there after it is
                  switched on again
    power cuts    QEMU is killed at a random moment during a change, many times in a row
                  on the same disk; each time the volume must mount and every file must be
                  its old or its new version
    refusals      a disk without an Inti volume, or with a damaged one, is not written to:
                  the image file is the same, byte for byte, after the attempts
    fallback      a torn newer meta block falls back to the older one
    differential  random changes to a meta block: the disk server and intifs.py must agree
                  on whether the volume is sound
    limits        a full directory and a full disk refuse the change and keep the old files
    faults        files survive a crash and a hang of the disk server
    slow          QEMU holds the drive's writes back: a write that takes many seconds goes
                  through while the supervisor's pings are answered; a write the drive does
                  not finish is reported as an error, and after it the drive is usable again
    recorded      QEMU records every sector the machine writes during a run of changes; this
                  computer then builds each state a cut could leave (after any one sector,
                  and with the writes since the last flush lost in any combination), checks
                  each with intifs.py and starts the machine on some of them

It writes evidence/storage.json and evidence/storage_serial.log, and exits with 0 only if
every check passed. In "power cuts" a cut is the emulator's process being killed: writes the
drive had taken are kept, a write in progress may be torn between two sectors. In "recorded"
a cut may also lose any of the sectors written since the last flush. A sector is written
whole or not at all, and what a flush has acknowledged stays: a drive that breaks either
rule is not modelled.
"""
from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import platform
import random
import re
import shutil
import statistics
import struct
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))
from vm import VM, VMError  # noqa: E402  (vm.py also puts the project root on sys.path)
import build  # noqa: E402
import intifs  # noqa: E402
from test import Run, qemu_version  # noqa: E402

EVIDENCE = ROOT / "evidence"
WORK = build.BUILD / "storage"
VOLUME_AT = intifs.VOLUME_START_SECTOR * intifs.SECTOR
VOLUME_BYTES = intifs.VOLUME_BLOCKS * intifs.BLOCK
NO_VOLUME = "no Inti volume on the disk"
DAMAGED = "the Inti volume is damaged"
DISK_FULL = "the disk is full"
DIRECTORY_FULL = "the directory is full"
DISK_LINE = re.compile(r"^disk: (.+)$", re.M)
FILE_LINE = re.compile(r"^  (\S+)\s+(\d+) bytes$", re.M)
SPACE = re.compile(r"disk: (\d+) files?, (\d+) of (\d+) blocks of 4 KiB free")
READY = re.compile(r"the disk server was ready (\d+) us after it started")
WORDS = ("amber", "birch", "cedar", "delta", "ember", "flint", "grove", "heron", "islet", "jade")


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def copy_of_image(name: str) -> Path:
    """A fresh copy of the built image for one case."""
    WORK.mkdir(parents=True, exist_ok=True)
    path = WORK / f"{name}.img"
    shutil.copyfile(build.BUILD / "inti.img", path)
    return path


def archive_files(image: bytes) -> dict[str, bytes]:
    """The files of the boot archive, read straight from the image."""
    kernel_bytes, archive_bytes = struct.unpack_from("<II", image, intifs.SECTOR + 12)
    start = intifs.SECTOR + kernel_bytes
    archive = image[start:start + archive_bytes]
    if archive[:8] != b"INTIARCH":
        raise ValueError("no boot archive where the kernel header says it is")
    files = {}
    for index in range(struct.unpack_from("<I", archive, 8)[0]):
        at = 12 + 32 * index
        offset, size = struct.unpack_from("<II", archive, at + 24)
        files[archive[at:at + 24].split(b"\0", 1)[0].decode("ascii")] = archive[offset:offset + size]
    return files


def host_files(path: Path) -> dict[str, bytes]:
    return intifs.Volume(path.read_bytes()).files()


def sizes(files: dict[str, bytes]) -> dict[str, int]:
    return {name: len(data) for name, data in files.items()}


def install(path: Path, volume: bytes) -> None:
    """Replace the volume in a disk image."""
    image = bytearray(path.read_bytes())
    image[VOLUME_AT:VOLUME_AT + len(volume)] = volume
    path.write_bytes(image)


def power_off(vm: VM) -> int | None:
    """Type `shutdown` and wait for the emulator to exit; its exit code, or None."""
    vm.send("shutdown\r")
    deadline = time.monotonic() + 15
    while vm.process.poll() is None and time.monotonic() < deadline:
        vm.pump()
    return vm.process.poll()


class Session:
    """One boot of a disk image that keeps what the machine writes; switched off at the end."""

    def __init__(self, run: Run, image: Path, label: str = "", drive: str | None = None):
        self.run = run
        self.image = image
        self.label = label
        self.vm = VM(image, snapshot=False, drive=drive)

    def __enter__(self) -> VM:
        try:
            self.vm.start()
            self.vm.wait_for_prompt()
        except BaseException:
            self.vm.close()  # `__exit__` is not called when `__enter__` fails
            raise
        return self.vm

    def __exit__(self, kind, *_) -> None:
        try:
            if kind is None and self.vm.process.poll() is None:
                code = power_off(self.vm)
                if code != 0:
                    self.run.check(f"{self.label or self.image.stem}: shutdown switches the machine off",
                                   False, f"exit code {code}")
        finally:
            self.vm.close()
            if self.label:
                self.run.logs.append((self.label, self.vm.log().replace("\r", "")))
            if "KERNEL PANIC" in self.vm.log():
                self.run.check(f"{self.label or self.image.stem}: no kernel panic", False)


def guest_view(vm: VM) -> tuple[str, dict[str, int]]:
    """What `ls` says about the disk: the text after "disk: " and the files with their sizes."""
    text = vm.command("ls")
    found = DISK_LINE.search(text)
    if found is None:
        return "", {}
    files = {name: int(size) for name, size in FILE_LINE.findall(text[found.end():])}
    return found[1], files


def said(text: str, reason: str) -> bool:
    return reason in text


# --- persistence ---------------------------------------------------------------


def persistence(run: Run) -> None:
    image = copy_of_image("persistence")
    archive = archive_files(image.read_bytes())
    seeded = host_files(image)
    with Session(run, image, "persistence, first boot") as vm:
        space = SPACE.search(vm.command("df"))
        ready = READY.search(vm.raw.decode("utf-8", "replace"))
        vm.command("write note.txt first line")
        vm.command("append note.txt second line")
        vm.command("cp readme.txt copy.txt")
        vm.command("cp selftest big.bin")
        denied = vm.command("counter")
        first = vm.command("counter +disk")
        second = vm.command("counter +disk")
        vm.command("rm welcome.txt")
        run.check("a program that was not given the disk is refused it, and says so",
                  "not given the disk" in denied)
        run.check("a program that was given the disk counts its runs in a file",
                  "run number 1," in first and "run number 2," in second)
    expected = {
        "note.txt": b"first line\nsecond line\n",
        "copy.txt": archive["readme.txt"],
        "big.bin": archive["selftest"],
        "counter.txt": b"2\n",
    }
    try:
        after = host_files(image)
        detail = ", ".join(f"{name} {len(data)}" for name, data in after.items())
    except intifs.VolumeError as error:
        after, detail = None, str(error)
    run.check("after shutdown the image holds exactly the files that were written, byte for byte "
              "(read on the build machine)", after == expected, detail)
    with Session(run, image, "persistence, second boot") as vm:
        reason, files = guest_view(vm)
        note = vm.command("cat note.txt")
        third = vm.command("counter +disk")
        run.check("after switching off and on again the files are listed with the same sizes",
                  files == sizes(expected), reason)
        run.check("after switching off and on again the text is the text that was written",
                  "first line\nsecond line\n" in note)
        run.check("after switching off and on again the program continues its count",
                  "run number 3," in third)
    run.measured["persistence"] = {
        "seeded_files": sizes(seeded),
        "files": sizes(expected),
        "blocks": int(space[3]) if space else None,
        "free_blocks_at_start": int(space[2]) if space else None,
        "ready_us": int(ready[1]) if ready else None,
    }


# --- power cuts ----------------------------------------------------------------


def wait_exactly(seconds: float) -> None:
    """`time.sleep` is too coarse on some systems for a delay of a few milliseconds."""
    end = time.perf_counter() + seconds
    while time.perf_counter() < end:
        pass


def some_text(rng: random.Random) -> str:
    return " ".join(rng.choice(WORDS) for _ in range(rng.randint(1, 12)))


def next_change(rng: random.Random, files: dict[str, bytes], archive: dict[str, bytes]):
    """(kind, shell line, the files as they are if the change happened)."""
    after = dict(files)
    pick = rng.random()
    removable = sorted(files)
    if pick < 0.30:
        target = rng.choice(("big.bin", "big2.bin"))
        after[target] = archive["selftest"]
        return "copy 80 KiB from the archive", f"cp selftest {target}", after
    if pick < 0.40 and "big.bin" in files:
        after["big3.bin"] = files["big.bin"]
        return "copy 80 KiB from the disk", "cp big.bin big3.bin", after
    if pick < 0.60:
        target = rng.choice(("a.txt", "b.txt", "c.txt"))
        text = some_text(rng)
        after[target] = text.encode("ascii") + b"\n"
        return "write a small file", f"write {target} {text}", after
    if pick < 0.82 or not removable:
        text = some_text(rng)
        after["log.txt"] = files.get("log.txt", b"") + text.encode("ascii") + b"\n"
        return "append a line", f"append log.txt {text}", after
    target = rng.choice(removable)
    del after[target]
    return "remove a file", f"rm {target}", after


def clean_durations(run: Run, archive: dict[str, bytes]) -> dict[str, float]:
    """How long each kind of change takes from the typed line to the next prompt, in seconds."""
    image = copy_of_image("timing")
    lines = {
        "copy 80 KiB from the archive": "cp selftest big.bin",
        "copy 80 KiB from the disk": "cp big.bin big3.bin",
        "write a small file": "write a.txt amber birch cedar",
        "append a line": "append log.txt delta ember flint",
        "remove a file": "rm a.txt",
    }
    samples: dict[str, list[float]] = {kind: [] for kind in lines}
    with Session(run, image) as vm:
        for _ in range(5):
            for kind, line in lines.items():
                started = time.perf_counter()
                vm.command(line)
                samples[kind].append(time.perf_counter() - started)
    run.measured["timing"] = {
        "what": "host time from the typed line to the next prompt, serial port included",
        "median_ms": {kind: round(statistics.median(values) * 1000, 1) for kind, values in samples.items()},
        "max_ms": {kind: round(max(values) * 1000, 1) for kind, values in samples.items()},
        "runs": 5,
        "copy_bytes": len(archive["selftest"]),
    }
    return {kind: statistics.median(values) for kind, values in samples.items()}


def power_cuts(run: Run, count: int, seed: int) -> None:
    image = copy_of_image("cuts")
    archive = archive_files(image.read_bytes())
    typical = clean_durations(run, archive)
    rng = random.Random(seed)
    files = host_files(image)
    outcomes = {"before": 0, "during": 0, "after": 0, "broken": 0}
    by_kind: dict[str, dict[str, int]] = {}
    broken: list[str] = []
    views_differ: list[str] = []
    texts_differ: list[str] = []
    texts_compared = 0
    for index in range(count):
        kind, line, after = next_change(rng, files, archive)
        delay = rng.uniform(0, 1.2 * typical[kind])
        volume_before = image.read_bytes()[VOLUME_AT:]
        vm = VM(image, snapshot=False)
        vm.start()
        try:
            vm.wait_for_prompt()
            reason, listed = guest_view(vm)
            if listed != sizes(files):
                views_differ.append(f"cut {index}: the machine lists {listed} ({reason}), the image holds {sizes(files)}")
            texts = sorted(name for name in files if name.endswith(".txt") and files[name])
            if texts:
                name = texts[index % len(texts)]
                texts_compared += 1
                if files[name].decode("ascii") not in vm.command(f"cat {name}"):
                    texts_differ.append(f"cut {index}: {name}")
            vm.send(line + "\r")
            wait_exactly(delay)
            vm.process.kill()
            vm.process.wait()
        finally:
            vm.close()
        try:
            found = host_files(image)
        except intifs.VolumeError as error:
            found = None
            broken.append(f"cut {index} ({line!r} after {delay * 1000:.1f} ms): {error}")
        if found is None:
            outcome = "broken"
            install(image, volume_before)
        elif found == after and found != files:
            outcome = "after"
            files = after
        elif found == files:
            outcome = "during" if image.read_bytes()[VOLUME_AT:] != volume_before else "before"
        else:
            outcome = "broken"
            changed = sorted(name for name in set(found) | set(files) | set(after)
                             if found.get(name) not in (files.get(name), after.get(name)))
            broken.append(f"cut {index} ({line!r} after {delay * 1000:.1f} ms): neither old nor new: {changed}")
            files = found
        outcomes[outcome] += 1
        by_kind.setdefault(kind, {"before": 0, "during": 0, "after": 0, "broken": 0})[outcome] += 1
        print(f"storage: cut {index + 1}/{count} {outcome:<6} {line[:40]}", flush=True)
    run.check(f"power cut during a change, {count} times in a row on one disk: every time the volume "
              "mounts and every file is its old or its new version", outcomes["broken"] == 0,
              "; ".join(broken) or f"{outcomes['before']} cuts before the first write, {outcomes['during']} "
              f"between the first write and the switch to the new version, {outcomes['after']} after it")
    run.check("the cuts landed in all three phases of a change (before, during, after)",
              min(outcomes["before"], outcomes["during"], outcomes["after"]) >= 1 or count < 20,
              json.dumps(outcomes))
    run.check("after every cut the machine lists the same files as the image holds",
              not views_differ, "; ".join(views_differ[:3]))
    with Session(run, image, "after the power cuts") as vm:
        reason, listed = guest_view(vm)
        shown = {name: vm.command(f"cat {name}") for name in sorted(files) if name.endswith(".txt")}
    texts_compared += len(shown)
    texts_differ += [f"at the end: {name}" for name, text in shown.items() if files[name].decode("ascii") not in text]
    run.check("after the cuts the machine shows a text file with the content the image holds, every time "
              "one was there to show", listed == sizes(files) and not texts_differ and texts_compared > 0,
              f"{texts_compared} comparisons ({len(shown)} of them after the last cut); wrong: {texts_differ[:4]}")
    run.measured["power_cuts"] = {"cuts": count, "seed": seed, **outcomes, "by_kind": by_kind,
                                  "texts_compared": texts_compared, "files_at_the_end": sizes(files)}


# --- volumes made on the build machine -----------------------------------------


def rich_files() -> list[tuple[str, bytes]]:
    rng = random.Random(1)
    return [
        ("empty.txt", b""),
        ("one.txt", b"1"),
        ("block.bin", bytes(range(256)) * 16),
        ("two.bin", b"ab" * 2049),
        ("big.bin", bytes(rng.getrandbits(8) for _ in range(20000))),
        ("note.txt", b"kept\n"),
    ]


def entry(index: int) -> int:
    return intifs.DIRECTORY_AT + intifs.ENTRY * index


def set_chain(meta: bytearray, block: int, value: int) -> None:
    struct.pack_into("<H", meta, intifs.CHAINS_AT + 2 * block, value)


def set_size(meta: bytearray, index: int, size: int) -> None:
    struct.pack_into("<I", meta, entry(index) + 24, size)


def set_first(meta: bytearray, index: int, block: int) -> None:
    struct.pack_into("<H", meta, entry(index) + 28, block)


def without_note(meta: bytearray) -> None:
    """Remove note.txt (entry 5, block 11) the way the disk server would."""
    meta[entry(5):entry(5) + intifs.ENTRY] = bytes(intifs.ENTRY)
    set_chain(meta, 11, intifs.FREE)


def same_name_twice(meta: bytearray) -> None:
    meta[entry(6):entry(6) + 8] = b"note.txt"


def bad_character(meta: bytearray) -> None:
    meta[entry(5):entry(5) + 9] = b"no/te.txt"


def too_many_blocks(meta: bytearray) -> None:
    struct.pack_into("<I", meta, intifs.TOTAL_AT, intifs.VOLUME_BLOCKS + 1)


# Each makes a meta block with a correct checksum that describes an impossible volume.
DAMAGE = (
    ("a chain of blocks that loops", lambda meta: set_chain(meta, 6, 6)),
    ("a file with a block outside the volume", lambda meta: set_chain(meta, 4, 300)),
    ("the same file name twice", same_name_twice),
    ("one block in two files", lambda meta: set_first(meta, 5, 2)),
    # The same with the block that note.txt gave up marked free, so that nothing but the
    # rule "a block belongs to one file" is broken.
    ("one block in two files and no block left over",
     lambda meta: (set_first(meta, 5, 2), set_chain(meta, 11, intifs.FREE))),
    ("a block marked as used that no file uses", lambda meta: set_chain(meta, 40, intifs.END)),
    ("a file longer than its chain of blocks", lambda meta: set_size(meta, 3, 3 * intifs.BLOCK)),
    ("a file shorter than its chain of blocks", lambda meta: set_size(meta, 4, intifs.BLOCK)),
    ("a file name with a character that is not allowed", bad_character),
    ("more blocks than the partition has", too_many_blocks),
    ("meta blocks that are not marked in the chain table", lambda meta: set_chain(meta, 0, intifs.FREE)),
)


def rich_volume(edit=None, sequence: int = 7) -> bytearray:
    volume = bytearray(intifs.format_volume(intifs.VOLUME_BLOCKS, rich_files(), sequence))
    if edit:
        meta = bytearray(volume[:intifs.BLOCK])
        edit(meta)
        volume[:intifs.BLOCK] = intifs.seal(meta)
    return volume


def set_partition_type(path: Path, kind: int) -> None:
    image = bytearray(path.read_bytes())
    image[intifs.PARTITION_TABLE + 4] = kind
    path.write_bytes(image)


def no_partition_table(path: Path) -> None:
    image = bytearray(path.read_bytes())
    image[intifs.PARTITION_TABLE:intifs.PARTITION_TABLE + 64] = bytes(64)
    path.write_bytes(image)


def partition_past_the_end(path: Path) -> None:
    image = bytearray(path.read_bytes())
    sectors = (len(image) - VOLUME_AT) // intifs.SECTOR + 8
    struct.pack_into("<I", image, intifs.PARTITION_TABLE + 12, sectors)
    path.write_bytes(image)


def wrong_checksums(path: Path) -> None:
    volume = rich_volume()
    volume[intifs.BLOCK:2 * intifs.BLOCK] = volume[:intifs.BLOCK]
    volume[100] ^= 1
    volume[intifs.BLOCK + 100] ^= 1
    install(path, bytes(volume))


def refused(run: Run, what: str, prepare, expected: str, host_must_refuse: bool = True) -> None:
    """A disk the server must not write to: try every way to write, compare the image file."""
    image = copy_of_image("refusal")
    prepare(image)
    try:
        intifs.Volume(image.read_bytes())
        host = "sound"
    except intifs.VolumeError as error:
        host = str(error)
    before = sha256(image)
    with Session(run, image) as vm:
        answers = [
            vm.command("df"),
            vm.command("write probe.txt some text"),
            vm.command("append probe.txt more text"),
            vm.command("cp readme.txt probe2.txt"),
            vm.command("rm note.txt"),
            vm.command("counter +disk"),
        ]
        time.sleep(2.6)  # a server that ends soon after it started counts towards the restart back-off
        vm.command("crash disk", timeout=20)
        answers.append(vm.command("write probe.txt after the restart"))
        answers.append(vm.command("df"))
    unchanged = sha256(image) == before
    all_said = all(said(answer, expected) for answer in answers)
    if host_must_refuse:
        run.check(f"{what}: the reader on the build machine refuses it too", host != "sound", host)
    run.check(f"{what}: every attempt to write is answered with \"{expected}\", also after the disk "
              "server was restarted, and the image file is unchanged, byte for byte",
              unchanged and all_said,
              f"image unchanged: {unchanged}; answers with the reason: {sum(said(a, expected) for a in answers)} "
              f"of {len(answers)}")
    run.measured.setdefault("refusals", []).append({"what": what, "says": expected, "image_unchanged": unchanged,
                                                    "attempts": len(answers), "host": host})


def refusals(run: Run) -> None:
    refused(run, "a partition of another type (0x83) where the volume was",
            lambda path: set_partition_type(path, 0x83), NO_VOLUME)
    refused(run, "a disk with an empty partition table", no_partition_table, NO_VOLUME)
    refused(run, "an Inti partition that is all zeros", lambda path: install(path, bytes(VOLUME_BYTES)), NO_VOLUME)
    noise = random.Random(2)
    refused(run, "an Inti partition full of other data",
            lambda path: install(path, bytes(noise.getrandbits(8) for _ in range(VOLUME_BYTES))), NO_VOLUME)
    refused(run, "an Inti partition that reaches past the end of the disk", partition_past_the_end, NO_VOLUME)
    refused(run, "both meta blocks with a wrong checksum", wrong_checksums, DAMAGED)
    for what, edit in DAMAGE:
        refused(run, f"a meta block with {what}", lambda path, edit=edit: install(path, bytes(rich_volume(edit))),
                DAMAGED)


# --- fallback to the older meta block -------------------------------------------


def two_slots(first_sequence: int, second_sequence: int, tear_second: bool) -> bytes:
    """The rich volume in slot 0, and in slot 1 the same volume without note.txt."""
    volume = rich_volume(sequence=first_sequence)
    second = rich_volume(without_note, sequence=second_sequence)[:intifs.BLOCK]
    if tear_second:
        second[-intifs.SECTOR:] = bytes(intifs.SECTOR)  # the last sector never reached the disk
    volume[intifs.BLOCK:2 * intifs.BLOCK] = second
    return bytes(volume)


def fallback(run: Run) -> None:
    with_note = sizes(dict(rich_files()))
    less = {name: size for name, size in with_note.items() if name != "note.txt"}

    image = copy_of_image("fallback")
    install(image, two_slots(7, 8, tear_second=True))
    with Session(run, image, "fallback to the older meta block") as vm:
        reason, listed = guest_view(vm)
        vm.command("write after.txt written after the fallback")
    try:
        volume = intifs.Volume(image.read_bytes())
        state = f"meta block {volume.active}, sequence {volume.sequence}, {len(volume.files())} files"
        good = (volume.active, volume.sequence) == (1, 8) and sizes(volume.files()) == {**with_note, "after.txt": 27}
    except intifs.VolumeError as error:
        state, good = str(error), False
    run.check("a newer meta block whose last sector was never written is ignored: the older one is used, "
              "no file is lost", listed == with_note, reason)
    run.check("the next change then overwrites the torn meta block and the volume is sound", good, state)

    for what, sequences in (("of two whole meta blocks the one with the higher sequence number is used", (7, 8)),
                            ("the sequence number may wrap around: 0 is newer than 4294967295", (0xFFFFFFFF, 0))):
        install(image, two_slots(*sequences, tear_second=False))
        with Session(run, image) as vm:
            reason, listed = guest_view(vm)
        run.check(what, listed == less, reason)


# --- the disk server and the reader on the build machine must agree ---------------


def random_edit(rng: random.Random):
    """(description, edit) for one random change to the meta block of the rich volume."""
    kind = rng.randrange(8)
    if kind == 0:
        block, value = rng.randrange(0, 16), rng.choice((0, 2, 5, 7, 11, 12, 255, 256, 1023, intifs.END, intifs.META))
        return f"chain[{block}] = {value}", lambda meta: set_chain(meta, block, value)
    if kind == 1:
        index, size = rng.randrange(0, 7), rng.choice((0, 1, 4095, 4096, 4097, 8192, 16385, 20480, 20481, 2 ** 32 - 1))
        return f"size of entry {index} = {size}", lambda meta: set_size(meta, index, size)
    if kind == 2:
        index, block = rng.randrange(0, 7), rng.choice((0, 1, 2, 3, 6, 11, 12, 255, 256, 65535))
        return f"first block of entry {index} = {block}", lambda meta: set_first(meta, index, block)
    if kind == 3:
        total = rng.choice((0, 2, 3, 11, 12, 13, 100, 255, 256, 257, 1024, 1025))
        return f"total blocks = {total}", lambda meta: struct.pack_into("<I", meta, intifs.TOTAL_AT, total)
    if kind == 4:
        index, at = rng.randrange(0, 6), rng.randrange(0, 24)
        byte = rng.choice((0, 32, 47, 65, 95, 122, 127, 255))
        return f"byte {at} of the name of entry {index} = {byte}", lambda meta: meta.__setitem__(entry(index) + at, byte)
    if kind == 5:
        index, other = rng.randrange(0, 6), rng.randrange(0, 6)
        return (f"entry {index} gets the name of entry {other}",
                lambda meta: meta.__setitem__(slice(entry(index), entry(index) + 24), meta[entry(other):entry(other) + 24]))
    if kind == 6:
        at, byte = rng.randrange(intifs.DIRECTORY_AT, intifs.CHAINS_AT + 64), rng.randrange(256)
        return f"byte {at} = {byte}", lambda meta: meta.__setitem__(at, byte)
    index = rng.randrange(0, 6)
    return (f"entry {index} moves to the last place of the directory",
            lambda meta: (meta.__setitem__(slice(entry(47), entry(48)), meta[entry(index):entry(index + 1)]),
                          meta.__setitem__(slice(entry(index), entry(index + 1)), bytes(intifs.ENTRY))))


def differential(run: Run, count: int, seed: int) -> None:
    rng = random.Random(seed)
    image = copy_of_image("differential")
    counts = {"sound": 0, "damaged": 0}
    disagreements: list[str] = []
    for index in range(count):
        what, edit = random_edit(rng)
        install(image, bytes(rich_volume(edit, sequence=9)))
        try:
            host = sizes(intifs.Volume(image.read_bytes()).files())
        except intifs.VolumeError as error:
            host = str(error)
        before = sha256(image)
        with Session(run, image) as vm:
            reason, listed = guest_view(vm)
            if isinstance(host, str):
                vm.command("write probe.txt some text")
        if isinstance(host, str):
            counts["damaged"] += 1
            agree = reason in (DAMAGED, NO_VOLUME) and sha256(image) == before
        else:
            counts["sound"] += 1
            agree = listed == host
        if not agree:
            disagreements.append(f"{what}: the build machine says {host}, the disk server says {reason} {listed}")
        print(f"storage: differential {index + 1}/{count} {'agree' if agree else 'DISAGREE'}  {what}", flush=True)
    run.check(f"{count} random changes to a meta block: the disk server and the reader on the build machine "
              "agree each time on whether the volume is sound, list the same files when it is, and nothing "
              "is written when it is not", not disagreements,
              "; ".join(disagreements[:4]) or f"{counts['sound']} sound, {counts['damaged']} damaged")
    run.measured["differential"] = {"cases": count, "seed": seed, **counts, "disagreements": len(disagreements)}


# --- limits ---------------------------------------------------------------------


def limits(run: Run) -> None:
    image = copy_of_image("limits")
    full = [(f"f{index:02}.txt", b"x\n") for index in range(intifs.ENTRIES)]
    install(image, intifs.format_volume(intifs.VOLUME_BLOCKS, full))
    with Session(run, image, "a full directory") as vm:
        refusal = vm.command("write extra.txt one more")
        vm.command("write f00.txt changed")
        vm.command("rm f01.txt")
        accepted = vm.command("write extra.txt one more")
    expected = {**dict(full), "f00.txt": b"changed\n", "extra.txt": b"one more\n"}
    del expected["f01.txt"]
    run.check(f"a directory with {intifs.ENTRIES} files refuses one more, still replaces a file, and takes "
              "a new one after one was removed",
              said(refusal, DIRECTORY_FULL) and "extra.txt:" not in accepted and host_files(image) == expected,
              refusal.strip().splitlines()[-2] if refusal.count("\n") > 1 else refusal)

    data_blocks = intifs.VOLUME_BLOCKS - 2
    fill = bytes(intifs.BLOCK * (data_blocks - 3))
    install(image, intifs.format_volume(intifs.VOLUME_BLOCKS, [("fill.bin", fill), ("keep.txt", b"old text\n")]))
    with Session(run, image, "a full disk") as vm:
        too_big = vm.command("cp selftest big.bin")
        space = SPACE.search(vm.command("df"))
        vm.command("write small.txt hi")
        vm.command("write keep.txt new text")
        vm.command("write last.txt takes the last block")
        no_room = vm.command("write keep.txt third text")
        kept = vm.command("cat keep.txt")
    expected = {"fill.bin": fill, "keep.txt": b"new text\n", "small.txt": b"hi\n",
                "last.txt": b"takes the last block\n"}
    try:
        volume = intifs.Volume(image.read_bytes())
        found, free = volume.files(), volume.free_blocks()
    except intifs.VolumeError as error:
        found, free = str(error), None
    run.check("a file that does not fit is refused with \"the disk is full\", and the blocks it had taken "
              "are free again", said(too_big, DISK_FULL) and space is not None and int(space[2]) == 2,
              space[0] if space else too_big[-200:])
    run.check("on a disk with no free block a file cannot be replaced (the new version needs room next to "
              "the old one); the old version stays", said(no_room, DISK_FULL) and "new text" in kept
              and found == expected and free == 0, f"{free} blocks free at the end")
    run.measured["limits"] = {"directory_entries": intifs.ENTRIES, "data_blocks": data_blocks,
                              "max_blocks": intifs.MAX_BLOCKS, "block_bytes": intifs.BLOCK}


# --- faults of the disk server --------------------------------------------------


def faults(run: Run) -> None:
    image = copy_of_image("faults")
    with Session(run, image, "crash and hang of the disk server") as vm:
        time.sleep(2.6)
        vm.command("write one.txt written before the crash")
        vm.command("crash disk", timeout=20)
        first = vm.command("cat one.txt")
        vm.command("write two.txt written after the crash")
        time.sleep(2.6)
        vm.command("hang disk", timeout=20)
        second = vm.command("cat two.txt")
        vm.command("append two.txt and after the hang")
        log = vm.log()
    restarts = len(re.findall(r"init: disk restarted after \d+ us", log))
    expected = {"one.txt": b"written before the crash\n",
                "two.txt": b"written after the crash\nand after the hang\n"}
    found = host_files(image)
    found.pop("welcome.txt", None)
    run.check("files written before a crash and before a hang of the disk server are there when the "
              "restarted server answers, and it takes new writes",
              "written before the crash" in first and "written after the crash" in second
              and found == expected and restarts == 2, f"{restarts} restarts by the supervisor")


# --- a drive that is slow, or stops in the middle of a write -----------------------

DRIVE = "ide0-hd0"
THROTTLES = ("bps", "bps_rd", "bps_wr", "iops", "iops_rd", "iops_wr")
DISK_ERROR = "the disk reported an error"
NOT_ANSWERING = re.compile(r"init: disk did not answer")
RESTARTED = re.compile(r"init: disk restarted after \d+ us")


def throttle(vm: VM, **limits: int) -> None:
    """Make QEMU hold back what the machine writes to its drive; without limits, lift that.

    The first write after a limit is set goes through at once, the later ones wait their turn.
    Every sector the disk server writes is one write to QEMU."""
    arguments = dict.fromkeys(THROTTLES, 0)
    arguments.update(limits)
    vm.qmp("block_set_io_throttle", device=DRIVE, **arguments)


def timed(vm: VM, line: str, timeout: float) -> tuple[float, str]:
    began = time.monotonic()
    text = vm.command(line, timeout=timeout)
    return round(time.monotonic() - began, 2), text


def supervisor_left_it_alone(log: str) -> bool:
    return NOT_ANSWERING.search(log) is None and RESTARTED.search(log) is None


def slow_write(run: Run) -> dict:
    """One write per second: far longer than the supervisor waits for an answer to a ping."""
    image = copy_of_image("slow")
    with Session(run, image, "a drive that takes a second for every sector") as vm:
        vm.command("write a.txt first")
        throttle(vm, iops_wr=1)
        seconds, answer = timed(vm, "write c.txt slow", 90)
        throttle(vm)
        shown = vm.command("cat c.txt")
        log = vm.log()
    found = host_files(image)
    found.pop("welcome.txt", None)
    run.check("a write that takes many seconds because the drive is slow goes through, and the supervisor "
              "does not take the waiting disk server for a hung one",
              seconds > 5 and not said(answer, DISK_ERROR) and "slow" in shown
              and found == {"a.txt": b"first\n", "c.txt": b"slow\n"} and supervisor_left_it_alone(log),
              f"the write took {seconds} s")
    return {"slow_write_s": seconds}


def stuck_write(run: Run, label: str, lift_first: bool, **limits: int) -> dict:
    """A write the drive does not finish in time, and what the disk server does after it.

    With `lift_first` the drive is free again before the next request and still waits for the
    rest of the abandoned write. Without it the drive is still busy with that write when the
    next request arrives."""
    image = copy_of_image("stuck")
    with Session(run, image, label) as vm:
        vm.command("write a.txt first")
        vm.command("write b.txt second")
        vm.command("cat a.txt")
        throttle(vm, **limits)
        seconds, refusal = timed(vm, "write d.txt stuck", 90)
        if lift_first:
            throttle(vm)
        read_seconds, shown = timed(vm, "cat b.txt", 60)
        throttle(vm)
        listing, _files = guest_view(vm)
        accepted = vm.command("write e.txt after", timeout=60)
        log = vm.log()
    found = host_files(image)
    found.pop("welcome.txt", None)
    expected = {"a.txt": b"first\n", "b.txt": b"second\n", "e.txt": b"after\n"}
    when = "is free again" if lift_first else "is still busy with it"
    run.check(f"a write the drive does not finish is reported as an error once the patience for a step is "
              f"over ({label})", said(refusal, DISK_ERROR) and 20 < seconds < 60,
              f"reported after {seconds} s")
    run.check(f"after that error, when the drive {when}, the next read gives the right file, the unfinished "
              "file is not there, a new write lands, and the disk server was not restarted",
              "second" in shown and not said(shown, DISK_ERROR) and "d.txt" not in listing
              and not said(accepted, DISK_ERROR) and found == expected and supervisor_left_it_alone(log),
              f"the read took {read_seconds} s; files on the disk: {', '.join(sorted(found))}")
    return {"error_after_s": seconds, "next_read_s": read_seconds}


def slow(run: Run) -> None:
    measured = {}
    sessions = (
        ("slow", lambda: slow_write(run)),
        # One byte a second: the second sector of the write never gets its turn.
        ("stopped", lambda: stuck_write(run, "the drive stops and is free again later", True, bps_wr=1)),
        # Sixteen bytes a second: the second sector is through two seconds after the error.
        ("busy", lambda: stuck_write(run, "the drive is still busy afterwards", False, bps_wr=16)),
    )
    for name, session in sessions:
        try:
            measured[name] = session()
        except VMError as error:
            run.check(f"slow, {name}: the session reached its end", False, f"{error}"[-400:])
    run.measured["slow"] = measured


# --- every state a recorded run can leave on the disk -----------------------------


def logged_drive(image: Path, log: Path) -> str:
    """A QEMU drive that also appends every write and every flush of the machine to `log`."""
    names = [str(path).replace(",", ",,") for path in (image, log)]
    return (f"if=ide,driver=blklogwrites,file.driver=file,file.filename={names[0]},"
            f"log.driver=file,log.filename={names[1]}")


def read_write_log(path: Path) -> list[tuple[int, bytes] | None]:
    """QEMU's write log in order: (sector, its 512 bytes) for a sector written, None for a flush."""
    data = path.read_bytes()
    _magic, _version, recorded, sector_bytes = struct.unpack_from("<QQQI", data, 0)
    if sector_bytes != intifs.SECTOR:
        raise ValueError(f"the write log has sectors of {sector_bytes} bytes")
    events: list[tuple[int, bytes] | None] = []
    entries = 0
    position = intifs.SECTOR
    while position + intifs.SECTOR <= len(data):
        sector, count, flags, _length = struct.unpack_from("<QQQQ", data, position)
        position += intifs.SECTOR
        entries += 1
        if count == 0:
            if not flags & 1:
                raise ValueError(f"write log entry {entries} is neither a write nor a flush")
            events.append(None)
            continue
        if position + count * intifs.SECTOR > len(data):
            raise ValueError("the write log ends in the middle of an entry")
        for step in range(count):
            at = position + step * intifs.SECTOR
            events.append((sector + step, data[at:at + intifs.SECTOR]))
        position += count * intifs.SECTOR
    if entries < recorded:
        raise ValueError(f"the write log names {recorded} entries and holds {entries}")
    return events


def put_sector(disk: bytearray, sector: int, data: bytes) -> None:
    disk[sector * intifs.SECTOR:(sector + 1) * intifs.SECTOR] = data


def version_of(disk: bytearray, first_sequence: int):
    """(how many changes the volume on `disk` has taken, its files), or the reason it is not sound."""
    try:
        volume = intifs.Volume(bytes(disk))
    except intifs.VolumeError as error:
        return str(error)
    return (volume.sequence - first_sequence) & 0xFFFFFFFF, volume.files()


def recorded_changes(archive: dict[str, bytes], files: dict[str, bytes]):
    """The shell lines of the recorded run, and the files as they are after each of them."""
    lines = []
    states = [dict(files)]

    def change(line: str, name: str, data: bytes | None) -> None:
        after = dict(states[-1])
        if data is None:
            del after[name]
        else:
            after[name] = data
        lines.append(line)
        states.append(after)

    change("write a.txt amber birch cedar", "a.txt", b"amber birch cedar\n")
    change("append a.txt delta ember", "a.txt", b"amber birch cedar\ndelta ember\n")
    change("cp selftest big.bin", "big.bin", archive["selftest"])
    change("cp shell big.bin", "big.bin", archive["shell"])
    change("cp big.bin big2.bin", "big2.bin", archive["shell"])
    change("rm a.txt", "a.txt", None)
    change("counter +disk", "counter.txt", b"1\n")
    change("write a.txt jade", "a.txt", b"jade\n")
    change("rm big.bin", "big.bin", None)
    change("cp selftest big3.bin", "big3.bin", archive["selftest"])
    change("append welcome.txt I was here", "welcome.txt", files["welcome.txt"] + b"I was here\n")
    return lines, states


def recorded(run: Run, seed: int) -> None:
    image = copy_of_image("recorded")
    log = WORK / "recorded.log"
    log.write_bytes(b"")
    first = image.read_bytes()
    archive = archive_files(first)
    start = intifs.Volume(first)
    lines, states = recorded_changes(archive, start.files())
    with Session(run, image, "the recorded run", drive=logged_drive(image, log)) as vm:
        for line in lines:
            vm.command(line)
    events = read_write_log(log)
    rng = random.Random(seed)

    # The log is the whole story: nothing reached the image that is not in it.
    disk = bytearray(first)
    for event in events:
        if event:
            put_sector(disk, *event)
    run.check("the recorded writes, replayed on the starting image, give the image the machine left, "
              "byte for byte", bytes(disk) == image.read_bytes(),
              f"{sum(1 for event in events if event)} sector writes, {events.count(None)} flushes")

    def judge(found, allowed: tuple[int, ...]) -> str:
        """Empty if `found` is one of the `allowed` versions with exactly its files."""
        if isinstance(found, str):
            return found
        version, files = found
        if version not in allowed or version >= len(states):
            return f"version {version}, expected one of {allowed}"
        if files != states[version]:
            wrong = sorted(name for name in set(files) | set(states[version])
                           if files.get(name) != states[version].get(name))
            return f"version {version} with other content in {wrong}"
        return ""

    # Walk the log one flush period at a time. `disk` is the image at the last flush.
    disk = bytearray(first)
    version = 0
    faults = {"outside": [], "mixed": [], "taken": [], "slot": [], "cut": [], "lost": []}
    counted = {"data_periods": 0, "meta_periods": 0, "cut_states": 1, "lost_meta_states": 0,
               "lost_data_states": 0}
    boots: list[tuple[str, bytes]] = []
    periods: list[list[tuple[int, bytes]]] = [[]]
    for event in events:
        if event is None:
            periods.append([])
        else:
            periods[-1].append(event)
    for number, writes in enumerate(period for period in periods if period):
        where = f"flush period {number}"
        mounted = intifs.Volume(bytes(disk))
        chain = intifs.chains(mounted.meta)
        blocks = []
        for sector, _ in writes:
            if not start.start <= sector < start.start + start.sectors:
                faults["outside"].append(f"{where}: sector {sector}")
                continue
            blocks.append((sector - start.start) // intifs.SECTORS_PER_BLOCK)
        meta = [block for block in blocks if block < 2]
        if meta and len(meta) != len(blocks):
            faults["mixed"].append(f"{where}: meta and data blocks {sorted(set(blocks))}")
        if meta:
            counted["meta_periods"] += 1
            if set(meta) != {1 - mounted.active} or len(writes) != intifs.SECTORS_PER_BLOCK:
                faults["slot"].append(f"{where}: {len(writes)} sectors to meta block {sorted(set(meta))} "
                                      f"while block {mounted.active} is in use")
        else:
            counted["data_periods"] += 1
            taken = sorted({block for block in blocks if chain[block] != intifs.FREE})
            if taken:
                faults["taken"].append(f"{where}: blocks {taken} belong to a file of the version in use")

        # A cut after each sector, with every earlier write on the disk.
        after = bytearray(disk)
        reached = version
        for index, write in enumerate(writes):
            put_sector(after, *write)
            found = version_of(after, start.sequence)
            counted["cut_states"] += 1
            problem = judge(found, (reached, version + 1) if meta else (version,))
            if problem:
                faults["cut"].append(f"{where}, after sector {index + 1} of {len(writes)}: {problem}")
            else:
                reached = found[0]
            if meta and number == boot_period(periods):
                boots.append((f"{where}, cut after sector {index + 1} of {len(writes)}", bytes(after)))

        # Writes since the last flush that never reached the disk, in any combination.
        if meta and len(writes) <= 10:
            subsets = range(1, 2 ** len(writes) - 1)
        else:
            subsets = [rng.getrandbits(len(writes)) for _ in range(24)]
        for subset in subsets:
            partial = bytearray(disk)
            for index, write in enumerate(writes):
                if subset >> index & 1:
                    put_sector(partial, *write)
            counted["lost_meta_states" if meta else "lost_data_states"] += 1
            problem = judge(version_of(partial, start.sequence), (version, version + 1) if meta else (version,))
            if problem:
                faults["lost"].append(f"{where}, sectors {subset:b}: {problem}")
            elif rng.random() < (0.004 if meta else 0.01):
                boots.append((f"{where}, only the sectors {subset:b} written", bytes(partial)))

        disk = after
        version = reached
    run.check("the machine wrote only inside its volume, and between two flushes either data blocks or "
              "one meta block, never both", not faults["outside"] and not faults["mixed"],
              "; ".join((faults["outside"] + faults["mixed"])[:3])
              or f"{counted['data_periods']} flush periods of data, {counted['meta_periods']} of a meta block")
    run.check("every data write went to a block that no file of the version on the disk used",
              not faults["taken"], "; ".join(faults["taken"][:3]))
    run.check("every new meta block went, whole, to the slot that did not hold the version in use",
              not faults["slot"], "; ".join(faults["slot"][:3]))
    run.check(f"each of the {len(lines)} changes switched the volume to its next version exactly once",
              version == len(lines) and counted["meta_periods"] == len(lines),
              f"version {version} after {counted['meta_periods']} meta blocks")
    run.check(f"a cut after any one sector write ({counted['cut_states']} states): the volume is sound, holds "
              "exactly the files from before or from after the change under way, and never an older "
              "version again", not faults["cut"], "; ".join(faults["cut"][:3]))
    run.check("writes since the last flush lost in any combination: all "
              f"{counted['lost_meta_states']} ways to lose part of a meta block and "
              f"{counted['lost_data_states']} random ways to lose data writes leave the volume sound with "
              "the files from before or from after the change", not faults["lost"], "; ".join(faults["lost"][:3]))

    # The machine itself must read these states the way the reader on this computer does.
    state = WORK / "state.img"
    differ = []
    for what, content in boots:
        state.write_bytes(content)
        expected = intifs.Volume(content).files()
        with VM(state) as vm:
            vm.wait_for_prompt()
            reason, listed = guest_view(vm)
            shown = vm.command("cat a.txt") if "a.txt" in expected else ""
        if listed != sizes(expected) or expected.get("a.txt", b"").decode("ascii") not in shown:
            differ.append(f"{what}: the machine lists {listed} ({reason})")
    run.check(f"started on {len(boots)} of these states, the machine lists the same files as the reader on "
              "the build machine", not differ and len(boots) >= intifs.SECTORS_PER_BLOCK, "; ".join(differ[:3]))
    run.measured["recorded"] = {"changes": lines, "sector_writes": sum(1 for event in events if event),
                                "flushes": events.count(None), **counted, "machine_starts": len(boots),
                                "seed": seed}


def boot_period(periods: list[list[tuple[int, bytes]]]) -> int:
    """The flush period whose every cut the machine is started on: the largest file being replaced."""
    filled = [period for period in periods if period]
    meta = [index for index, period in enumerate(filled) if len(period) == intifs.SECTORS_PER_BLOCK]
    return meta[3] if len(meta) > 3 else -1


CASES = {
    "persistence": lambda run, options: persistence(run),
    "cuts": lambda run, options: power_cuts(run, options.cuts, options.seed),
    "refusals": lambda run, options: refusals(run),
    "fallback": lambda run, options: fallback(run),
    "differential": lambda run, options: differential(run, options.fuzz, options.seed),
    "limits": lambda run, options: limits(run),
    "faults": lambda run, options: faults(run),
    "slow": lambda run, options: slow(run),
    "recorded": lambda run, options: recorded(run, options.seed),
}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--cuts", type=int, default=60, help="power cuts in a row")
    parser.add_argument("--fuzz", type=int, default=40, help="random changes to a meta block")
    parser.add_argument("--seed", type=int, default=20261114, help="seed of the random choices")
    parser.add_argument("--only", choices=sorted(CASES), action="append", help="run only this case")
    options = parser.parse_args()

    began = time.monotonic()
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    image_sha256 = sha256(build.BUILD / "inti.img")
    run = Run(None)
    run.logs = []
    for name, case in CASES.items():
        if options.only and name not in options.only:
            continue
        try:
            case(run, options)
        except (VMError, OSError, KeyError, ValueError, intifs.VolumeError) as error:
            run.check(f"{name}: the case reached its end", False, f"{type(error).__name__}: {error}"[-600:])
    run.check("the built image was not changed by any case", sha256(build.BUILD / "inti.img") == image_sha256)
    shutil.rmtree(WORK, ignore_errors=True)

    passed = all(check["ok"] for check in run.checks)
    failed = [check["name"] for check in run.checks if not check["ok"]]
    report = {
        "passed": passed,
        "date": datetime.datetime.now().astimezone().isoformat(timespec="seconds"),
        "duration_s": round(time.monotonic() - began),
        "emulator": f"QEMU {qemu_version()} (TCG, no hardware virtualisation)",
        "host": f"{platform.system()} {platform.release()}",
        "image_sha256": image_sha256,
        "options": {"cuts": options.cuts, "fuzz": options.fuzz, "seed": options.seed, "only": options.only},
        "checks": run.checks,
        "measured": run.measured,
    }
    print(f"storage: {'PASS' if passed else 'FAIL'} {len(run.checks) - len(failed)}/{len(run.checks)} checks, "
          f"{report['duration_s']} s", flush=True)
    if options.only:
        print("storage: a partial run; evidence/storage.json is left as it was", flush=True)
        return 0 if passed else 1
    (EVIDENCE / "storage.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8", newline="\n")
    serial = "".join(f"===== {label} =====\n{log}\n" for label, log in run.logs)
    (EVIDENCE / "storage_serial.log").write_text(serial, encoding="utf-8", newline="\n")
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
