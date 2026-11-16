#!/usr/bin/env python3
"""Build Inti.

    python build.py          compile everything and write build/inti.img
    python build.py run      build, then boot the image in a QEMU window; files written to
                             the disk are kept from one run to the next (--fresh: start again)
    python build.py test     build, then run the whole test script in QEMU
    python build.py gates    build, test, and check every hard gate of rubric.md
    python build.py storage  build, then test the disk server and the file system in QEMU: power
                             cuts, damaged volumes, limits (--cuts N, --fuzz N, --only CASE)
    python build.py storage-mutants  plant defects in a copy of the disk server and of the host's
                             reader of the format, count how many the storage test catches
    python build.py machines build, then boot the image on several emulated machines (other
                             processors, memory sizes, disk controllers) and record what works on each
    python build.py model    compile the kernel for this computer and run its model checks
    python build.py mutants  plant defects in a copy of the kernel, count how many the model catches
                             (--held-out: a second sample of places; --fresh: a third, drawn after tests
                             were aimed at the second; --unseen: a fourth, unseen until tests were
                             aimed at it; --fifth: a fifth, which no test was written against)
    python build.py mutants-qemu  boot the mutants the model did not catch and run the tests on them
    python build.py miri     the model's directed checks under Miri (needs a nightly toolchain with miri)
    python build.py all      model, then build, test and gates; --mutants also runs every sample again
    python build.py clean    remove build/ and target/

Needs Rust with the i586-unknown-linux-gnu target and Python 3.9 or newer. Nothing else:
no assembler, no C compiler, no make. `run`, `test`, `storage`, `machines` and `gates` also need QEMU. `model` and
`mutants` build an ordinary program, so they need a Rust toolchain that can link one for
this computer (on Windows the GNU toolchain, or the MSVC one with Visual Studio's linker);
INTI_HOST_TOOLCHAIN names the toolchain to use, for example stable-x86_64-pc-windows-gnu.
"""
from __future__ import annotations

import json
import os
import shutil
import struct
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT / "tools"))
import intifs  # noqa: E402

BUILD = ROOT / "build"
TARGET = "i586-unknown-linux-gnu"
PROGRAMS = sorted(path.stem for path in (ROOT / "user" / "src" / "bin").glob("*.rs"))

KERNEL_BASE = 0x100000
BOOT_BASE = 0x7C00
SECTOR = 512
CHUNK = 32 * 1024  # the boot sector reads the payload in 32 KiB pieces
MAX_CHUNKS = 17  # 0x10000..0x98000, the conventional memory the boot sector may use
CHUNK_COUNT_AT = 0x1B0  # offset of `chunk_count` in the boot sector
# QEMU's default 32-bit processor has no no-execute bit; "+nx" adds it, and with it W^X.
QEMU_CPU = "qemu32,+nx"

SHT_PROGBITS = 1
SHF_ALLOC = 2
PT_LOAD = 1


def find_tool(name: str, extra: list[Path]) -> str:
    found = shutil.which(name)
    if found:
        return found
    for candidate in extra:
        if candidate.exists():
            return str(candidate)
    sys.exit(f"build.py: cannot find {name}")


def cargo_build(package: str) -> Path:
    """Compile one package with its own linker script; returns the output directory."""
    cargo = find_tool("cargo", [Path.home() / ".cargo" / "bin" / "cargo.exe", Path.home() / ".cargo" / "bin" / "cargo"])
    target_dir = ROOT / "target" / package
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
    command = [cargo, "build", "--release", "-p", package, "--target-dir", str(target_dir)]
    subprocess.run(command, cwd=ROOT, env=env, check=True)
    return target_dir / TARGET / "release"


def model_command(root: Path) -> tuple[list[str], dict[str, str], Path, str]:
    """How to compile the kernel model in `root` for this computer.

    Returns the command, its environment, the program it produces and the target triple.
    The workspace builds for the 386 by default (.cargo/config.toml), so the triple of this
    computer is given explicitly, and none of the kernel's linker flags apply.
    """
    home = Path.home() / ".cargo" / "bin"
    cargo = find_tool("cargo", [home / "cargo.exe", home / "cargo"])
    rustc = find_tool("rustc", [home / "rustc.exe", home / "rustc"])
    toolchain = os.environ.get("INTI_HOST_TOOLCHAIN")
    if not toolchain and os.name == "nt" and shutil.which("rustup"):
        listed = subprocess.run(["rustup", "toolchain", "list"], capture_output=True, text=True).stdout
        gnu = [line.split()[0] for line in listed.splitlines() if "-pc-windows-gnu" in line]
        toolchain = gnu[0] if gnu else None
    plus = [f"+{toolchain}"] if toolchain else []
    described = subprocess.run([rustc, *plus, "-vV"], capture_output=True, text=True, check=True).stdout
    triple = next(line.split()[1] for line in described.splitlines() if line.startswith("host:"))
    env = dict(os.environ)
    env.pop("RUSTFLAGS", None)
    env.pop("CARGO_ENCODED_RUSTFLAGS", None)
    target_dir = root / "target" / "model"
    command = [cargo, *plus, "build", "--release", "-p", "model", "--target", triple,
               "--target-dir", str(target_dir)]
    program = target_dir / triple / "release" / ("model.exe" if os.name == "nt" else "model")
    return command, env, program, triple


def sections(elf: bytes) -> list[tuple[int, int, int, int, int]]:
    """(type, flags, address, file offset, size) of every section of a 32-bit ELF."""
    if elf[:6] != b"\x7fELF\x01\x01":
        sys.exit("build.py: not a 32-bit little-endian ELF file")
    shoff = struct.unpack_from("<I", elf, 32)[0]
    shentsize, shnum = struct.unpack_from("<HH", elf, 46)
    found = []
    for index in range(shnum):
        fields = struct.unpack_from("<10I", elf, shoff + index * shentsize)
        found.append((fields[1], fields[2], fields[3], fields[4], fields[5]))
    return found


def flatten(elf: bytes, base: int) -> bytes:
    """The memory image of an ELF file from address `base`: what objcopy -O binary writes."""
    image = bytearray()
    for kind, flags, address, offset, size in sections(elf):
        if kind != SHT_PROGBITS or not flags & SHF_ALLOC or size == 0:
            continue
        if address < base:
            sys.exit(f"build.py: section at {address:#x} lies below the load address {base:#x}")
        end = address - base + size
        if len(image) < end:
            image.extend(bytes(end - len(image)))
        image[address - base:end] = elf[offset:offset + size]
    return bytes(image)


def strip(elf: bytes) -> bytes:
    """Cut an ELF program down to what the kernel loader reads: headers and segments."""
    phoff = struct.unpack_from("<I", elf, 28)[0]
    phentsize, phnum = struct.unpack_from("<HH", elf, 42)
    end = phoff + phentsize * phnum
    for index in range(phnum):
        kind, offset, _, _, filesz = struct.unpack_from("<5I", elf, phoff + index * phentsize)
        if kind == PT_LOAD:
            end = max(end, offset + filesz)
    stripped = bytearray(elf[:end])
    struct.pack_into("<I", stripped, 32, 0)  # e_shoff
    struct.pack_into("<HH", stripped, 48, 0, 0)  # e_shnum, e_shstrndx
    return bytes(stripped)


def make_archive(files: list[tuple[str, bytes]]) -> bytes:
    """The boot archive: "INTIARCH", count, {name[24], offset, size} entries, file data."""
    data_start = -(-(12 + 32 * len(files)) // 16) * 16
    entries = bytearray()
    body = bytearray()
    for name, data in files:
        encoded = name.encode("ascii")
        if len(encoded) > 23:
            sys.exit(f"build.py: archive name too long: {name}")
        entries += encoded.ljust(24, b"\0") + struct.pack("<II", data_start + len(body), len(data))
        body += data + bytes(-len(data) % 16)
    header = b"INTIARCH" + struct.pack("<I", len(files)) + bytes(entries)
    return header.ljust(data_start, b"\0") + bytes(body)


def build() -> Path:
    boot_elf = (cargo_build("boot") / "boot").read_bytes()
    kernel_elf = (cargo_build("kernel") / "kernel").read_bytes()
    user_dir = cargo_build("user")

    boot = bytearray(flatten(boot_elf, BOOT_BASE))
    if len(boot) != SECTOR or boot[510:512] != b"\x55\xaa":
        sys.exit(f"build.py: the boot sector is {len(boot)} bytes or lacks the 0xAA55 signature")

    files = [(name, strip((user_dir / name).read_bytes())) for name in PROGRAMS]
    for path in sorted((ROOT / "files").glob("*")):
        files.append((path.name, path.read_bytes().replace(b"\r\n", b"\n")))
    archive = make_archive(files)

    kernel = bytearray(flatten(kernel_elf, KERNEL_BASE))
    kernel += bytes(-len(kernel) % 16)
    if kernel[8:12] != b"INTK":
        sys.exit("build.py: the kernel image does not start with its header")
    struct.pack_into("<II", kernel, 12, len(kernel), len(archive))

    payload = bytes(kernel) + archive
    chunks = -(-len(payload) // CHUNK)
    if chunks > MAX_CHUNKS:
        sys.exit(f"build.py: kernel + archive is {len(payload)} bytes; the boot sector can load {MAX_CHUNKS * CHUNK}")
    struct.pack_into("<H", boot, CHUNK_COUNT_AT, chunks)

    # The second half of the image is a partition with a formatted volume, for the files
    # that the `disk` server keeps. The boot sector is also the partition table.
    volume_at = intifs.VOLUME_START_SECTOR * SECTOR
    table = slice(intifs.PARTITION_TABLE, intifs.PARTITION_TABLE + 64)
    if any(boot[CHUNK_COUNT_AT + 2:510]) or SECTOR + chunks * CHUNK > volume_at:
        sys.exit("build.py: the boot sector or the payload reaches into the partition table or the volume")
    sectors = intifs.VOLUME_BLOCKS * intifs.SECTORS_PER_BLOCK
    boot[table] = intifs.partition_entry(intifs.VOLUME_START_SECTOR, sectors).ljust(64, b"\0")
    seed = [(path.name, path.read_bytes().replace(b"\r\n", b"\n")) for path in sorted((ROOT / "disk").glob("*"))]
    volume = intifs.format_volume(intifs.VOLUME_BLOCKS, seed)

    image = (bytes(boot) + payload).ljust(volume_at, b"\0") + volume

    BUILD.mkdir(exist_ok=True)
    image_path = BUILD / "inti.img"
    image_path.write_bytes(image)
    # The browser demo in docs/ must serve exactly the image the tests ran against.
    demo_dir = ROOT / "docs"
    if demo_dir.is_dir():
        (demo_dir / "inti.img").write_bytes(image)
    sizes = {
        "boot_sector_bytes": len(boot),
        "kernel_bytes": len(kernel),
        "archive_bytes": len(archive),
        "payload_bytes": len(payload),
        "image_bytes": len(image),
        "volume_bytes": len(volume),
        "files": {name: len(data) for name, data in files},
        "disk_files": {name: len(data) for name, data in seed},
    }
    (BUILD / "sizes.json").write_text(json.dumps(sizes, indent=2) + "\n", encoding="utf-8")
    print(f"build.py: kernel {len(kernel)} bytes, archive {len(archive)} bytes ({len(files)} files), "
          f"{chunks} of {MAX_CHUNKS} chunks -> {image_path}")
    return image_path


def find_qemu() -> str:
    override = os.environ.get("INTI_QEMU")
    if override:
        return override
    return find_tool("qemu-system-i386", [
        ROOT.parent / "tools" / "qemu" / "qemu-system-i386.exe",
        Path(r"C:\Program Files\qemu\qemu-system-i386.exe"),
    ])


def run_tool(script: str, arguments: list[str]) -> int:
    return subprocess.run([sys.executable, str(ROOT / "tools" / script), *arguments], cwd=ROOT).returncode


def main() -> int:
    command = sys.argv[1] if len(sys.argv) > 1 else "build"
    if command == "clean":
        shutil.rmtree(BUILD, ignore_errors=True)
        shutil.rmtree(ROOT / "target", ignore_errors=True)
        return 0
    if command == "model":
        compile_model, env, program, triple = model_command(ROOT)
        subprocess.run(compile_model, cwd=ROOT, env=env, check=True)
        return run_tool("model.py", [str(program), triple])
    if command == "mutants":
        return run_tool("mutants.py", sys.argv[2:])
    if command == "mutants-qemu":
        return run_tool("mutants_qemu.py", sys.argv[2:])
    if command == "storage-mutants":
        return run_tool("storage_mutants.py", sys.argv[2:])
    if command == "miri":
        return run_tool("miri.py", sys.argv[2:])
    if command == "all":
        return run_tool("all.py", sys.argv[2:])
    if command not in ("build", "run", "test", "storage", "machines", "gates"):
        sys.exit(__doc__)
    image = build()
    if command == "run":
        # build/inti.img is rewritten by every build and must stay exactly what the tests
        # measured, so the machine runs on a copy. The copy keeps its volume from run to
        # run and takes only the system (everything in front of the volume) from the build.
        disk = BUILD / "run.img"
        built = image.read_bytes()
        volume_at = intifs.VOLUME_START_SECTOR * SECTOR
        kept = disk.read_bytes()[volume_at:] if disk.exists() and "--fresh" not in sys.argv else b""
        disk.write_bytes(built[:volume_at] + (kept if len(kept) == len(built) - volume_at else built[volume_at:]))
        qemu = [find_qemu(), "-cpu", QEMU_CPU, "-drive", f"format=raw,file={disk},if=ide",
                "-m", "64", "-serial", "stdio"]
        return subprocess.run(qemu).returncode
    if command == "test":
        return run_tool("test.py", sys.argv[2:])
    if command == "storage":
        return run_tool("storage_test.py", sys.argv[2:])
    if command == "machines":
        return run_tool("machines.py", sys.argv[2:])
    if command == "gates":
        return run_tool("gates.py", sys.argv[2:])
    return 0


if __name__ == "__main__":
    sys.exit(main())
