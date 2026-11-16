"""The Inti volume format, INTIFS01, on the host: write a fresh volume, read and check one.

`build.py` uses this to put an empty, formatted volume into the disk image, and the
storage tests use it to check from outside what the `disk` server left on the disk.
The same numbers are in `user/src/bin/disk.rs`; `docs/storage.md` describes the layout.

    python tools/intifs.py IMAGE          list the files of the volume in a disk image
    python tools/intifs.py IMAGE NAME     write one file to standard output
"""
from __future__ import annotations

import re
import struct
import sys
import zlib
from pathlib import Path

SECTOR = 512
BLOCK = 4096
SECTORS_PER_BLOCK = BLOCK // SECTOR
PARTITION_TABLE = 0x1BE
PARTITION_TYPE = 0x7F
MAGIC = b"INTIFS01"
SEQUENCE_AT = 8
TOTAL_AT = 12
DIRECTORY_AT = 32
ENTRY = 32
ENTRIES = 48
NAME = 24
CHAINS_AT = DIRECTORY_AT + ENTRY * ENTRIES
MAX_BLOCKS = 1024
CHECKSUM_AT = BLOCK - 4
FREE = 0
END = 0xFFFF
META = 0xFFFE
VALID_NAME = re.compile(rb"[A-Za-z0-9._-]{1,23}")

# Where build.py puts the volume in the disk image.
VOLUME_START_SECTOR = 2048
VOLUME_BLOCKS = 256


class VolumeError(Exception):
    """The bytes are not a sound Inti volume."""


def partition_entry(start_sector: int, sectors: int) -> bytes:
    """One 16-byte entry of an MBR partition table for an Inti volume."""

    def chs(sector: int) -> bytes:
        # The geometry every BIOS assumes for a disk addressed by sector number.
        cylinder, rest = divmod(sector, 255 * 63)
        head, in_track = divmod(rest, 63)
        if cylinder > 1023:
            return b"\xfe\xff\xff"
        return bytes([head, (in_track + 1) | (cylinder >> 8) << 6, cylinder & 0xFF])

    first = chs(start_sector)
    last = chs(start_sector + sectors - 1)
    return bytes([0]) + first + bytes([PARTITION_TYPE]) + last + struct.pack("<II", start_sector, sectors)


def seal(meta: bytearray) -> bytes:
    """Put the checksum on a meta block."""
    struct.pack_into("<I", meta, CHECKSUM_AT, zlib.crc32(bytes(meta[:CHECKSUM_AT])))
    return bytes(meta)


def format_volume(blocks: int, files: list[tuple[str, bytes]], sequence: int = 1) -> bytes:
    """A fresh volume of `blocks` blocks holding `files`. The same input gives the same bytes."""
    if not 3 <= blocks <= MAX_BLOCKS:
        raise VolumeError(f"a volume has 3 to {MAX_BLOCKS} blocks, not {blocks}")
    if len(files) > ENTRIES:
        raise VolumeError(f"a volume holds {ENTRIES} files, not {len(files)}")
    meta = bytearray(BLOCK)
    meta[:8] = MAGIC
    struct.pack_into("<II", meta, SEQUENCE_AT, sequence, blocks)
    struct.pack_into("<HH", meta, CHAINS_AT, META, META)
    volume = bytearray(blocks * BLOCK)
    following = 2
    for index, (name, data) in enumerate(files):
        encoded = name.encode("ascii")
        if not VALID_NAME.fullmatch(encoded) or any(name == other for other, _ in files[:index]):
            raise VolumeError(f"not a usable file name: {name!r}")
        count = -(-len(data) // BLOCK)
        if following + count > blocks:
            raise VolumeError("the files do not fit in the volume")
        at = DIRECTORY_AT + ENTRY * index
        meta[at:at + len(encoded)] = encoded
        struct.pack_into("<IH", meta, at + 24, len(data), following if count else 0)
        for step in range(count):
            block = following + step
            struct.pack_into("<H", meta, CHAINS_AT + 2 * block, END if step == count - 1 else block + 1)
        volume[following * BLOCK:following * BLOCK + len(data)] = data
        following += count
    volume[:BLOCK] = seal(meta)
    return bytes(volume)


def find_partition(image: bytes) -> tuple[int, int]:
    """(first sector, sectors) of the first partition of the Inti type."""
    if len(image) < SECTOR or image[510:512] != b"\x55\xaa":
        raise VolumeError("no boot signature in sector 0")
    for index in range(4):
        at = PARTITION_TABLE + 16 * index
        if image[at + 4] == PARTITION_TYPE:
            start, sectors = struct.unpack_from("<II", image, at + 8)
            if start == 0 or sectors < 3 * SECTORS_PER_BLOCK or (start + sectors) * SECTOR > len(image):
                raise VolumeError("the Inti partition does not fit in the disk")
            return start, sectors
    raise VolumeError("no partition of the Inti type (0x7F)")


def sealed(meta: bytes, sectors: int) -> bool:
    """Was this meta block written completely, for a volume that fits in `sectors` sectors?"""
    total = struct.unpack_from("<I", meta, TOTAL_AT)[0]
    return (
        meta[:8] == MAGIC
        and zlib.crc32(meta[:CHECKSUM_AT]) == struct.unpack_from("<I", meta, CHECKSUM_AT)[0]
        and 3 <= total <= MAX_BLOCKS
        and total * SECTORS_PER_BLOCK <= sectors
    )


def chains(meta: bytes) -> list[int]:
    return list(struct.unpack_from(f"<{MAX_BLOCKS}H", meta, CHAINS_AT))


def directory(meta: bytes) -> list[tuple[bytes, int, int]]:
    """(name field, size, first block) of every directory entry in use."""
    found = []
    for index in range(ENTRIES):
        at = DIRECTORY_AT + ENTRY * index
        if meta[at]:
            size, first = struct.unpack_from("<IH", meta, at + 24)
            found.append((meta[at:at + NAME], size, first))
    return found


def problems(meta: bytes) -> list[str]:
    """Everything that keeps a sealed meta block from describing a sound volume.

    The same rules as `consistent` in the disk server, written separately on purpose: the
    tests compare what the server accepts with what this accepts.
    """
    total = struct.unpack_from("<I", meta, TOTAL_AT)[0]
    chain = chains(meta)
    used: set[int] = set()
    names: set[bytes] = set()
    found = []
    for field, size, first in directory(meta):
        name = field.split(b"\0", 1)[0]
        label = name.decode("ascii", "replace")
        if not VALID_NAME.fullmatch(name) or field[len(name):].strip(b"\0"):
            found.append(f"bad name {field!r}")
            continue
        if name in names:
            found.append(f"{label}: listed twice")
        names.add(name)
        count = -(-size // BLOCK)
        if count == 0 and first != 0:
            found.append(f"{label}: empty but has a first block")
        block = first
        for step in range(count):
            if not 2 <= block < total:
                found.append(f"{label}: block {block} is outside the volume")
                break
            if block in used:
                found.append(f"{label}: block {block} is used twice")
                break
            used.add(block)
            block = chain[block]
            if step == count - 1 and block != END:
                found.append(f"{label}: the chain does not end where the size says")
    if chain[0] != META or chain[1] != META:
        found.append("the meta blocks are not marked in the chain table")
    for block in range(2, MAX_BLOCKS):
        if (block in used) != (chain[block] != FREE):
            found.append(f"block {block} is {'in a file but marked free' if block in used else 'marked used by no file'}")
            break
    return found


class Volume:
    """The volume in a disk image, as the disk server would mount it."""

    def __init__(self, image: bytes):
        self.start, self.sectors = find_partition(image)
        base = self.start * SECTOR
        self.bytes = image[base:base + self.sectors * SECTOR]
        slots = [self.bytes[index * BLOCK:(index + 1) * BLOCK] for index in range(2)]
        self.sealed = [sealed(slot, self.sectors) for slot in slots]
        self.sequences = [struct.unpack_from("<I", slot, SEQUENCE_AT)[0] for slot in slots]
        newer = (self.sequences[1] - self.sequences[0]) & 0xFFFFFFFF
        if self.sealed[1] and (not self.sealed[0] or 0 < newer < 0x80000000):
            self.active = 1
        elif self.sealed[0]:
            self.active = 0
        elif any(slot[:8] == MAGIC for slot in slots):
            raise VolumeError("damaged: neither meta block is whole")
        else:
            raise VolumeError("the Inti partition holds no volume")
        self.meta = slots[self.active]
        self.sequence = self.sequences[self.active]
        self.total = struct.unpack_from("<I", self.meta, TOTAL_AT)[0]
        found = problems(self.meta)
        if found:
            raise VolumeError("damaged: " + "; ".join(found))

    def files(self) -> dict[str, bytes]:
        chain = chains(self.meta)
        result = {}
        for field, size, first in directory(self.meta):
            data = bytearray()
            block = first
            for _ in range(-(-size // BLOCK)):
                data += self.bytes[block * BLOCK:(block + 1) * BLOCK]
                block = chain[block]
            result[field.split(b"\0", 1)[0].decode("ascii")] = bytes(data[:size])
        return result

    def free_blocks(self) -> int:
        return sum(1 for value in chains(self.meta)[2:self.total] if value == FREE)


def main() -> int:
    if len(sys.argv) not in (2, 3):
        sys.exit(__doc__)
    try:
        volume = Volume(Path(sys.argv[1]).read_bytes())
    except VolumeError as error:
        sys.exit(f"intifs.py: {error}")
    files = volume.files()
    if len(sys.argv) == 3:
        if sys.argv[2] not in files:
            sys.exit(f"intifs.py: no file {sys.argv[2]}")
        sys.stdout.buffer.write(files[sys.argv[2]])
        return 0
    print(f"volume of {volume.total} blocks at sector {volume.start}, meta block {volume.active}, "
          f"sequence {volume.sequence}, {volume.free_blocks()} blocks free")
    for name, data in files.items():
        print(f"  {name:<24}{len(data):>8}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
