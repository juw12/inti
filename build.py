#!/usr/bin/env python3
"""Build Inti.

    python build.py          compile everything and write build/inti.img
    python build.py run      build, then boot the image in a QEMU window
    python build.py test     build, then run the whole test script in QEMU
    python build.py gates    build, test, and check every hard gate of rubric.md
    python build.py model    compile the kernel for this computer and run its model checks
    python build.py mutants  plant defects in a copy of the kernel, count how many the model catches
                             (--held-out: a second sample of places, which no test was written against)
    python build.py clean    remove build/ and target/

Needs Rust with the i586-unknown-linux-gnu target and Python 3.9 or newer. Nothing else:
no assembler, no C compiler, no make. `run`, `test` and `gates` also need QEMU. `model` and
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

    image = bytes(boot) + payload.ljust(chunks * CHUNK, b"\0")
    megabyte = 1024 * 1024
    image = image.ljust(max(2, -(-len(image) // megabyte)) * megabyte, b"\0")

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
        "files": {name: len(data) for name, data in files},
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
    if command not in ("build", "run", "test", "gates"):
        sys.exit(__doc__)
    image = build()
    if command == "run":
        qemu = [find_qemu(), "-cpu", QEMU_CPU, "-drive", f"format=raw,file={image},if=ide",
                "-m", "64", "-serial", "stdio"]
        return subprocess.run(qemu).returncode
    if command == "test":
        return run_tool("test.py", sys.argv[2:])
    if command == "gates":
        return run_tool("gates.py", sys.argv[2:])
    return 0


if __name__ == "__main__":
    sys.exit(main())
