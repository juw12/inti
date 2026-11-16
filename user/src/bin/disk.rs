//! `disk`: an ATA disk driver and a small crash-safe file system, in one user task.
//!
//! It holds three capabilities: its request endpoint, the eight I/O ports of the primary
//! ATA channel and that channel's control port. It cannot print and cannot touch any other
//! device or any memory but its own.
//!
//! The files live in a partition of type 0x7F (see `docs/storage.md` for the layout). Two
//! rules protect whatever else is on the disk. This server never creates a volume: without
//! a valid one it writes nothing at all. And every write goes through `Volume::put`, which
//! refuses a block outside the mounted volume.
//!
//! A change appears in one step. New data only ever goes to blocks that no file uses. Then
//! a new meta block (the directory and the block chains, with a sequence number and a
//! checksum) is written to whichever of the two meta slots does not hold the current one.
//! After a power cut the valid meta block with the higher sequence number wins, so every
//! file is either its old version or its new one.
#![no_std]
#![no_main]

use rt::abi::{Msg, FOREVER, L_KERNEL, MAX_TASKS, MSG_DATA, PAGE, USER_BASE};
use rt::proto::{B_SHELL, DBG_CRASH, DBG_HANG, DENIED, ERR, FS_LIST, FS_READ, FS_STAT, OK, PING};
use rt::proto::{DISK_APPEND, DISK_BLOCK, DISK_COMMIT, DISK_CREATE, DISK_DELETE, DISK_INFO};
use rt::proto::{DISK_WRITE, D_BAD_NAME, D_DAMAGED, D_DIR_FULL, D_FULL, D_IO, D_MOVED};
use rt::proto::{D_NO_DRIVE, D_NO_FILE, D_NO_VOLUME, D_STALE};
use rt::{io, sys};

/// Capability slot 0: receive right on the request endpoint.
const REQUESTS: u32 = 0;
/// Capability slot 1: I/O ports 0x1F0 to 0x1F7, the primary ATA channel.
const PORTS: u32 = 1;
/// Capability slot 2: I/O port 0x3F6, the device control register of that channel.
const CONTROL: u32 = 2;
/// Where the four block buffers and the held requests are mapped.
const BUFFERS: u32 = USER_BASE + 0x3000_0000;

// The ATA registers and bits this driver uses.
const DATA: u32 = 0x1F0;
const COUNT: u32 = 0x1F2;
const LBA_LOW: u32 = 0x1F3;
const LBA_MID: u32 = 0x1F4;
const LBA_HIGH: u32 = 0x1F5;
const DEVICE: u32 = 0x1F6;
/// The command register when written, the status register when read.
const COMMAND: u32 = 0x1F7;
const DEVICE_CONTROL: u32 = 0x3F6;
const BUSY: u32 = 0x80;
const FAULT: u32 = 0x20;
const WANTS_DATA: u32 = 0x08;
const ERROR: u32 = 0x01;
/// What the status register reads when no drive is attached, and what a refused port
/// access counts as.
const NOTHING: u32 = 0xFF;
const READ_SECTORS: u32 = 0x20;
const WRITE_SECTORS: u32 = 0x30;
const FLUSH_CACHE: u32 = 0xE7;
const IDENTIFY: u32 = 0xEC;
/// Device register: the first drive of the channel, addressed by sector number.
const FIRST_DRIVE: u32 = 0xE0;
/// Device control: reset both drives of the channel.
const RESET: u32 = 0x04;
/// Device control: the drive raises no interrupt. This driver polls.
const NO_INTERRUPT: u32 = 0x02;
/// How long a drive that works gets for one step of a command. A disk that was put to
/// sleep has to spin up before it answers, and a write cache can take a while to empty.
const STEP_US: u64 = 30_000_000;
/// How long the drive gets while it has not identified itself, and to settle after a reset.
const FIND_US: u64 = 2_000_000;
/// For this long a step is polled without a pause. Nearly every step ends well inside it.
const QUICK_US: u64 = 20_000;
/// While steps are polled without a pause, the request endpoint is looked at this often.
const LISTEN_US: u64 = 100_000;
/// Requests that can wait for their turn while the drive is busy. A task has at most one
/// request under way, so there are never more.
const HELD: usize = MAX_TASKS;
const SECTOR: usize = 512;
/// Sector numbers are 28 bits wide in the commands used here.
const MAX_SECTORS: u32 = 1 << 28;

// The volume layout. `build.py` and `tools/storage_test.py` hold the same numbers.
const BLOCK: usize = DISK_BLOCK as usize;
const SECTORS_PER_BLOCK: u32 = (BLOCK / SECTOR) as u32;
const PARTITION_TABLE: usize = 0x1BE;
const PARTITION_TYPE: u8 = 0x7F;
const MAGIC: &[u8; 8] = b"INTIFS01";
const SEQUENCE_AT: usize = 8;
const TOTAL_AT: usize = 12;
const DIRECTORY_AT: usize = 32;
const ENTRY: usize = 32;
const ENTRIES: usize = 48;
/// A name field; a name is at most one byte shorter, so it always ends in a NUL.
const NAME: usize = 24;
const SIZE_IN_ENTRY: usize = 24;
const FIRST_IN_ENTRY: usize = 28;
const CHAINS_AT: usize = DIRECTORY_AT + ENTRY * ENTRIES;
/// The most blocks a volume can have, a power of two.
const MAX_BLOCKS: usize = 1024;
const CHECKSUM_AT: usize = BLOCK - 4;
/// Chain value of a block that no file uses.
const FREE: u32 = 0;
/// Chain value of the last block of a file.
const END: u32 = 0xFFFF;
/// Chain value of the two meta blocks.
const META: u32 = 0xFFFE;
/// "No block" for the one-block read cache.
const NONE: u32 = u32::MAX;

const _: () = assert!(CHAINS_AT + 2 * MAX_BLOCKS <= CHECKSUM_AT && MAX_BLOCKS.is_power_of_two());

type Block = [u8; BLOCK];

/// Everything this server keeps outside its stack.
#[repr(C)]
struct Memory {
    /// The meta block on the disk, the one being prepared, the read cache and the
    /// unfinished block of a draft.
    blocks: [Block; 4],
    held: [Msg; HELD],
}

const MEMORY_PAGES: u32 = (core::mem::size_of::<Memory>() as u32).div_ceil(PAGE);

fn get16(bytes: &[u8], at: usize) -> u32 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]]) as u32
}

fn get32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn put16(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 2].copy_from_slice(&(value as u16).to_le_bytes());
}

fn put32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// CRC-32 as used by zip and Ethernet.
fn checksum(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

/// The block that follows `block` in its file, or `FREE`, `END` or `META`.
fn next(meta: &Block, block: u32) -> u32 {
    get16(meta, CHAINS_AT + 2 * (block as usize & (MAX_BLOCKS - 1)))
}

fn set_next(meta: &mut Block, block: u32, value: u32) {
    put16(meta, CHAINS_AT + 2 * (block as usize & (MAX_BLOCKS - 1)), value);
}

/// Directory entry `index` (below `ENTRIES`): name, size, first block.
fn entry(meta: &Block, index: usize) -> &[u8] {
    &meta[DIRECTORY_AT + ENTRY * index..][..ENTRY]
}

fn in_use(meta: &Block, index: usize) -> bool {
    entry(meta, index)[0] != 0
}

fn size_of(meta: &Block, index: usize) -> u32 {
    get32(entry(meta, index), SIZE_IN_ENTRY)
}

fn first_of(meta: &Block, index: usize) -> u32 {
    get16(entry(meta, index), FIRST_IN_ENTRY)
}

fn find(meta: &Block, name: &[u8]) -> Option<usize> {
    (0..ENTRIES).find(|&index| in_use(meta, index) && io::name(&entry(meta, index)[..NAME]) == name)
}

fn vacant(meta: &Block) -> Option<usize> {
    (0..ENTRIES).find(|&index| !in_use(meta, index))
}

fn blocks_for(size: u32) -> u32 {
    size.div_ceil(DISK_BLOCK)
}

fn valid_name(name: &[u8]) -> bool {
    (1..NAME).contains(&name.len())
        && name.iter().all(|&byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// Is this a meta block that was written completely, for a volume that fits in a
/// partition of `sectors` sectors?
fn sealed(meta: &Block, sectors: u32) -> bool {
    let total = get32(meta, TOTAL_AT);
    meta.starts_with(MAGIC)
        && checksum(&meta[..CHECKSUM_AT]) == get32(meta, CHECKSUM_AT)
        && (3..=MAX_BLOCKS as u32).contains(&total)
        && total * SECTORS_PER_BLOCK <= sectors
}

/// Does this meta block describe a sound volume? Every file's chain has exactly the length
/// its size needs and stays inside the volume, no block belongs to two files, and every
/// block outside the chains is free. What is on the disk is not trusted: this never loops
/// for more steps than the volume has blocks, whatever the bytes are.
fn consistent(meta: &Block) -> bool {
    let total = get32(meta, TOTAL_AT);
    let mut used = [0u8; MAX_BLOCKS / 8];
    for index in 0..ENTRIES {
        if !in_use(meta, index) {
            continue;
        }
        let field = &entry(meta, index)[..NAME];
        let name = io::name(field);
        let padded = field[name.len()..].iter().all(|&byte| byte == 0);
        let twice = (0..index).any(|other| in_use(meta, other) && io::name(&entry(meta, other)[..NAME]) == name);
        if !valid_name(name) || !padded || twice {
            return false;
        }
        let blocks = blocks_for(size_of(meta, index));
        let mut block = first_of(meta, index);
        if blocks == 0 && block != 0 {
            return false;
        }
        for step in 0..blocks {
            if block < 2 || block >= total {
                return false;
            }
            let (byte, bit) = (block as usize / 8, 1 << (block % 8));
            if used[byte] & bit != 0 {
                return false;
            }
            used[byte] |= bit;
            block = next(meta, block);
            if step + 1 == blocks && block != END {
                return false;
            }
        }
    }
    next(meta, 0) == META
        && next(meta, 1) == META
        && (2..MAX_BLOCKS as u32).all(|block| {
            let chained = used[block as usize / 8] & (1 << (block % 8)) != 0;
            chained == (next(meta, block) != FREE)
        })
}

fn status() -> u32 {
    sys::io_in(PORTS, COMMAND, 1).unwrap_or(NOTHING)
}

fn register(port: u32) -> u32 {
    sys::io_in(PORTS, port, 1).unwrap_or(NOTHING)
}

fn out(port: u32, value: u32) {
    let _ = sys::io_out(PORTS, port, 1, value);
}

#[derive(Clone, Copy, PartialEq)]
enum Until {
    /// The drive is not busy; errors of an earlier command do not matter.
    Idle,
    /// The command is finished without an error.
    Done,
    /// The drive offers or wants the next sector.
    Data,
}

/// The first drive of the primary ATA channel, and the requests that arrived while it was
/// busy.
struct Ata {
    /// Sectors the drive has; 0 when there is no usable drive.
    sectors: u32,
    /// A step failed, so the drive may still be in the middle of that command. The channel
    /// is reset before the next command.
    unsure: bool,
    /// A ring of the requests that wait for their turn: `count` of them, the oldest at
    /// `oldest`.
    held: &'static mut [Msg; HELD],
    oldest: usize,
    count: usize,
    /// When the request endpoint is looked at next while steps are polled without a pause.
    listen_at: u64,
}

impl Ata {
    /// Look at the request endpoint for up to `wait_ms` while the drive is busy. A ping is
    /// answered at once: `init` ends a server that stays silent for half a second, and a
    /// drive may take far longer than that. Every other request is kept for the main loop.
    fn listen(&mut self, wait_ms: u32) {
        self.listen_at = sys::time() + LISTEN_US;
        if self.count == HELD {
            if wait_ms > 0 {
                sys::sleep(wait_ms);
            }
            return;
        }
        let mut msg = Msg::new(0);
        if sys::recv(REQUESTS, &mut msg, wait_ms).is_err() || msg.label & L_KERNEL != 0 {
            return;
        }
        if msg.label == PING {
            let _ = sys::reply(msg.token, &Msg::new(OK));
        } else {
            self.held[(self.oldest + self.count) % HELD] = msg;
            self.count += 1;
        }
    }

    /// The oldest request that arrived while the drive was busy.
    fn take_held(&mut self) -> Option<Msg> {
        if self.count == 0 {
            return None;
        }
        let msg = self.held[self.oldest];
        self.oldest = (self.oldest + 1) % HELD;
        self.count -= 1;
        Some(msg)
    }

    /// Note that the drive may be in the middle of a command, and report the failure.
    fn failed(&mut self) -> bool {
        self.unsure = true;
        false
    }

    /// Poll the status register until the drive is in the wanted state. False on an error
    /// report, on a missing drive and when the drive takes too long: longer than `STEP_US`
    /// if it has identified itself and its state is known, longer than `FIND_US` otherwise.
    fn wait(&mut self, until: Until) -> bool {
        if until != Until::Idle {
            // A drive needs 400 ns to show its new status; one discarded read takes longer.
            status();
        }
        let patience = if self.sectors == 0 || self.unsure { FIND_US } else { STEP_US };
        let began = sys::time();
        loop {
            let status = status();
            if status == NOTHING {
                return self.failed();
            }
            if status & BUSY == 0 {
                if until == Until::Idle {
                    return true;
                }
                if status & (ERROR | FAULT) != 0 {
                    return self.failed();
                }
                if until == Until::Done || status & WANTS_DATA != 0 {
                    return true;
                }
            }
            let now = sys::time();
            if now - began > patience {
                return self.failed();
            }
            if now - began > QUICK_US {
                // A slow step. Sleep on the request endpoint between two looks at the drive.
                self.listen(1);
            } else {
                if now >= self.listen_at {
                    self.listen(0);
                }
                sys::yield_now();
            }
        }
    }

    /// Reset the channel, which makes the drive drop the command it is in the middle of.
    fn reset(&mut self) -> bool {
        let _ = sys::io_out(CONTROL, DEVICE_CONTROL, 1, RESET | NO_INTERRUPT);
        sys::sleep(2);
        let _ = sys::io_out(CONTROL, DEVICE_CONTROL, 1, NO_INTERRUPT);
        sys::sleep(2);
        // The short patience applies here, whatever the drive did before.
        self.unsure = true;
        if !self.wait(Until::Idle) {
            return false;
        }
        self.unsure = false;
        true
    }

    /// Address the first drive and start a command. After a failed step the drive would
    /// take the new command for part of the old one, so the channel is reset first.
    fn command(&mut self, sector: u32, count: u32, code: u32) -> bool {
        if self.unsure && !self.reset() {
            return false;
        }
        if !self.wait(Until::Idle) {
            return false;
        }
        out(DEVICE, FIRST_DRIVE | (sector >> 24) & 0x0F);
        for _ in 0..4 {
            status();
        }
        if !self.wait(Until::Idle) {
            return false;
        }
        out(COUNT, count);
        out(LBA_LOW, sector & 0xFF);
        out(LBA_MID, (sector >> 8) & 0xFF);
        out(LBA_HIGH, (sector >> 16) & 0xFF);
        out(COMMAND, code);
        true
    }

    /// Look for the drive. After a restart of this server (`again`) the drive may still be
    /// in the middle of a command that the old server started, so the channel is reset
    /// first.
    fn probe(&mut self, again: bool) {
        self.sectors = 0;
        if status() == NOTHING {
            return;
        }
        if again {
            if !self.reset() {
                return;
            }
        } else {
            let _ = sys::io_out(CONTROL, DEVICE_CONTROL, 1, NO_INTERRUPT);
            sys::sleep(2);
        }
        if !self.command(0, 0, IDENTIFY) || status() == 0 || !self.wait(Until::Idle) {
            return;
        }
        // A CD-ROM drive answers IDENTIFY with a signature in these registers and no data.
        if register(LBA_MID) != 0 || register(LBA_HIGH) != 0 || !self.wait(Until::Data) {
            return;
        }
        let mut sectors = 0;
        for word in 0..SECTOR / 2 {
            let value = sys::io_in(PORTS, DATA, 2).unwrap_or(0) & 0xFFFF;
            match word {
                60 => sectors |= value,
                61 => sectors |= value << 16,
                _ => {}
            }
        }
        self.sectors = sectors.min(MAX_SECTORS);
    }

    /// Are `count` sectors from `sector` on inside the drive, and few enough for one command?
    fn inside(&self, sector: u32, count: u32) -> bool {
        (1..=255).contains(&count) && sector.checked_add(count).is_some_and(|end| end <= self.sectors)
    }

    /// Fill `into`, a whole number of sectors, from the drive.
    fn read(&mut self, sector: u32, into: &mut [u8]) -> bool {
        let count = (into.len() / SECTOR) as u32;
        if !self.inside(sector, count) || !self.command(sector, count, READ_SECTORS) {
            return false;
        }
        for part in into.chunks_exact_mut(SECTOR) {
            if !self.wait(Until::Data) {
                return false;
            }
            for pair in part.chunks_exact_mut(2) {
                let Ok(word) = sys::io_in(PORTS, DATA, 2) else {
                    return self.failed();
                };
                pair.copy_from_slice(&(word as u16).to_le_bytes());
            }
        }
        true
    }

    /// Write `from`, a whole number of sectors, to the drive. Only `Volume::put` calls this.
    fn write(&mut self, sector: u32, from: &[u8]) -> bool {
        let count = (from.len() / SECTOR) as u32;
        if !self.inside(sector, count) || !self.command(sector, count, WRITE_SECTORS) {
            return false;
        }
        for part in from.chunks_exact(SECTOR) {
            if !self.wait(Until::Data) {
                return false;
            }
            for pair in part.chunks_exact(2) {
                let word = u16::from_le_bytes([pair[0], pair[1]]) as u32;
                if sys::io_out(PORTS, DATA, 2, word).is_err() {
                    return self.failed();
                }
            }
        }
        self.wait(Until::Done)
    }

    /// Make the drive put everything written so far on the medium.
    fn flush(&mut self) -> bool {
        self.sectors != 0 && self.command(0, 0, FLUSH_CACHE) && self.wait(Until::Done)
    }
}

/// Which buffer `Volume::put` writes.
#[derive(Clone, Copy, PartialEq)]
enum Source {
    /// The new meta block.
    Work,
    /// The next data block of the file being written.
    Pending,
}

/// A new version of one file that is being put together.
#[derive(Clone, Copy)]
struct Draft {
    /// The transaction number the writer has to quote.
    id: u32,
    name: [u8; NAME],
    name_len: usize,
    /// First and last block of the new version so far, 0 for none.
    first: u32,
    last: u32,
    /// Bytes so far. The last `size % BLOCK` of them are still in `pending`.
    size: u32,
}

struct Volume {
    ata: Ata,
    /// 0 when a volume is mounted, otherwise the `D_*` reason why not.
    state: u32,
    /// First sector of the partition.
    start: u32,
    /// Blocks of the volume, the two meta blocks included.
    total: u32,
    /// Which meta slot (block 0 or 1) holds `meta`.
    active: u32,
    /// The meta block as it is on the disk. Readers see only this.
    meta: &'static mut Block,
    /// The meta block being prepared. Equal to `meta` unless a change is under way.
    work: &'static mut Block,
    /// The data block read last, and its number.
    cache: &'static mut Block,
    cached: u32,
    /// The unfinished last block of the draft.
    pending: &'static mut Block,
    draft: Option<Draft>,
    next_id: u32,
    start_up_us: u32,
}

impl Volume {
    /// Find the partition and read the newer of its two meta blocks. Writes nothing.
    fn mount(&mut self) -> u32 {
        self.draft = None;
        self.cached = NONE;
        self.total = 0;
        if self.ata.sectors == 0 {
            return D_NO_DRIVE;
        }
        if !self.ata.read(0, &mut self.cache[..SECTOR]) {
            return D_IO;
        }
        let table = &self.cache[..SECTOR];
        let found = (0..4)
            .map(|index| PARTITION_TABLE + 16 * index)
            .find(|&at| table[at + 4] == PARTITION_TYPE);
        let (Some(at), [0x55, 0xAA]) = (found, [table[510], table[511]]) else {
            return D_NO_VOLUME;
        };
        let (start, sectors) = (get32(table, at + 8), get32(table, at + 12));
        let fits = start.checked_add(sectors).is_some_and(|end| end <= self.ata.sectors);
        if start == 0 || sectors < 3 * SECTORS_PER_BLOCK || !fits {
            return D_NO_VOLUME;
        }
        self.start = start;
        let first = self.ata.read(start, &mut self.meta[..]) && sealed(self.meta, sectors);
        let second = self.ata.read(start + SECTORS_PER_BLOCK, &mut self.work[..]) && sealed(self.work, sectors);
        let newer = get32(&self.work[..], SEQUENCE_AT).wrapping_sub(get32(&self.meta[..], SEQUENCE_AT)) as i32 > 0;
        if second && (!first || newer) {
            self.meta.copy_from_slice(&self.work[..]);
            self.active = 1;
        } else if first {
            self.active = 0;
        } else if self.meta.starts_with(MAGIC) || self.work.starts_with(MAGIC) {
            return D_DAMAGED;
        } else {
            return D_NO_VOLUME;
        }
        if !consistent(self.meta) {
            return D_DAMAGED;
        }
        self.work.copy_from_slice(&self.meta[..]);
        self.total = get32(&self.meta[..], TOTAL_AT);
        0
    }

    /// The only place this server writes to the disk: a meta block to one of the two meta
    /// slots, or a data block to a block behind them, and only inside a mounted volume.
    fn put(&mut self, block: u32, from: Source) -> bool {
        if self.state != 0 || block >= self.total || (block < 2) != (from == Source::Work) {
            return false;
        }
        if self.cached == block {
            self.cached = NONE;
        }
        let bytes = if from == Source::Work { &self.work[..] } else { &self.pending[..] };
        self.ata.write(self.start + block * SECTORS_PER_BLOCK, bytes)
    }

    /// Bring a data block into `cache`.
    fn fetch(&mut self, block: u32) -> bool {
        if self.cached == block {
            return true;
        }
        self.cached = NONE;
        if block < 2 || block >= self.total {
            return false;
        }
        if !self.ata.read(self.start + block * SECTORS_PER_BLOCK, &mut self.cache[..]) {
            return false;
        }
        self.cached = block;
        true
    }

    /// Forget the change under way.
    fn rollback(&mut self) {
        self.draft = None;
        self.work.copy_from_slice(&self.meta[..]);
    }

    /// Make `work` the state on the disk: first everything the new chains point to, then
    /// the meta block itself, into the slot that does not hold the current one.
    fn publish(&mut self) -> Result<(), u32> {
        let sequence = get32(&self.meta[..], SEQUENCE_AT).wrapping_add(1);
        put32(&mut self.work[..], SEQUENCE_AT, sequence);
        let sum = checksum(&self.work[..CHECKSUM_AT]);
        put32(&mut self.work[..], CHECKSUM_AT, sum);
        // Never write a meta block that the next start would refuse.
        if !consistent(self.work) {
            return Err(D_DAMAGED);
        }
        if !self.ata.flush() {
            return Err(D_IO);
        }
        let slot = 1 - self.active;
        if !self.put(slot, Source::Work) || !self.ata.flush() {
            // What that slot holds now is unknown. Read the disk again instead of guessing.
            self.state = self.mount();
            return Err(D_IO);
        }
        self.meta.copy_from_slice(&self.work[..]);
        self.active = slot;
        Ok(())
    }

    /// Start a draft of the file `name`: empty, or holding what the file holds now.
    fn open(&mut self, name: &[u8], keep: bool) -> Result<Draft, u32> {
        self.rollback();
        if !valid_name(name) {
            return Err(D_BAD_NAME);
        }
        let mut draft = Draft { id: self.next_id, name: [0; NAME], name_len: name.len(), first: 0, last: 0, size: 0 };
        draft.name[..name.len()].copy_from_slice(name);
        // Odd numbers only, so that a transaction number is never 0.
        self.next_id = self.next_id.wrapping_add(2);
        let Some(index) = find(self.meta, name) else {
            return if vacant(self.meta).is_some() { Ok(draft) } else { Err(D_DIR_FULL) };
        };
        let size = size_of(self.meta, index);
        let mut block = first_of(self.meta, index);
        if !keep {
            // The new version shares nothing with the old one. The old blocks are released
            // in `work` only: they stay taken until the new version is on the disk.
            for _ in 0..blocks_for(size) {
                let following = next(self.work, block);
                set_next(self.work, block, FREE);
                block = following;
            }
            return Ok(draft);
        }
        // The new version shares every full block with the old one. A partly filled last
        // block is copied, because the old version must stay readable as it is.
        draft.size = size;
        draft.first = block;
        for _ in 0..size / DISK_BLOCK {
            draft.last = block;
            block = next(self.meta, block);
        }
        let tail = size as usize % BLOCK;
        if tail > 0 {
            if !self.fetch(block) {
                self.rollback();
                return Err(D_IO);
            }
            self.pending[..tail].copy_from_slice(&self.cache[..tail]);
            set_next(self.work, block, FREE);
            if draft.last == 0 {
                draft.first = 0;
            } else {
                set_next(self.work, draft.last, END);
            }
        }
        Ok(draft)
    }

    /// Write `pending` to a free block and hang it on the end of the draft. A block is
    /// free only if neither the state on the disk nor the one being prepared uses it.
    fn link(&mut self, draft: &mut Draft) -> Result<(), u32> {
        let free = (2..self.total).find(|&block| next(self.meta, block) == FREE && next(self.work, block) == FREE);
        let Some(block) = free else {
            return Err(D_FULL);
        };
        if !self.put(block, Source::Pending) {
            return Err(D_IO);
        }
        set_next(self.work, block, END);
        if draft.last == 0 {
            draft.first = block;
        } else {
            set_next(self.work, draft.last, block);
        }
        draft.last = block;
        Ok(())
    }

    /// Add bytes to the draft; at most 256, so at most one block is written.
    fn push(&mut self, draft: &mut Draft, bytes: &[u8]) -> Result<(), u32> {
        for &byte in bytes {
            let tail = draft.size as usize % BLOCK;
            self.pending[tail] = byte;
            draft.size += 1;
            if tail + 1 == BLOCK {
                self.link(draft)?;
            }
        }
        Ok(())
    }

    /// Put the draft in the place of the file.
    fn seal(&mut self, draft: &mut Draft) -> Result<(), u32> {
        let tail = draft.size as usize % BLOCK;
        if tail > 0 {
            self.pending[tail..].fill(0);
            self.link(draft)?;
        }
        let name = &draft.name[..draft.name_len];
        let Some(index) = find(self.work, name).or_else(|| vacant(self.work)) else {
            return Err(D_DIR_FULL);
        };
        let slot = &mut self.work[DIRECTORY_AT + ENTRY * index..][..ENTRY];
        slot.fill(0);
        slot[..name.len()].copy_from_slice(name);
        put32(slot, SIZE_IN_ENTRY, draft.size);
        put16(slot, FIRST_IN_ENTRY, draft.first);
        self.publish()
    }

    fn list(&self, number: u32, reply: &mut Msg) -> Result<(), u32> {
        let index = (0..ENTRIES).filter(|&index| in_use(self.meta, index)).nth(number as usize).ok_or(D_NO_FILE)?;
        let name = io::name(&entry(self.meta, index)[..NAME]);
        reply.data[..name.len()].copy_from_slice(name);
        reply.len = name.len() as u32;
        reply.words[0] = size_of(self.meta, index);
        Ok(())
    }

    fn stat(&self, name: &[u8], reply: &mut Msg) -> Result<(), u32> {
        reply.words[0] = size_of(self.meta, find(self.meta, name).ok_or(D_NO_FILE)?);
        Ok(())
    }

    fn read(&mut self, name: &[u8], offset: u32, count: u32, reply: &mut Msg) -> Result<(), u32> {
        let index = find(self.meta, name).ok_or(D_NO_FILE)?;
        let size = size_of(self.meta, index);
        let offset = offset.min(size);
        let within = offset as usize % BLOCK;
        let count = (count as usize).min(MSG_DATA).min((size - offset) as usize).min(BLOCK - within);
        if count > 0 {
            let mut block = first_of(self.meta, index);
            for _ in 0..offset / DISK_BLOCK {
                block = next(self.meta, block);
            }
            if !self.fetch(block) {
                return Err(D_IO);
            }
            reply.data[..count].copy_from_slice(&self.cache[within..within + count]);
        }
        reply.len = count as u32;
        Ok(())
    }

    fn create(&mut self, name: &[u8], reply: &mut Msg) -> Result<(), u32> {
        let draft = self.open(name, false)?;
        reply.words[0] = draft.id;
        self.draft = Some(draft);
        Ok(())
    }

    /// The draft with this transaction number, if it is the open one.
    fn take(&mut self, id: u32) -> Result<Draft, u32> {
        let draft = self.draft.filter(|draft| draft.id == id).ok_or(D_STALE)?;
        self.draft = None;
        Ok(draft)
    }

    fn write(&mut self, id: u32, bytes: &[u8]) -> Result<(), u32> {
        let mut draft = self.take(id)?;
        match self.push(&mut draft, bytes) {
            Ok(()) => self.draft = Some(draft),
            Err(reason) => {
                self.rollback();
                return Err(reason);
            }
        }
        Ok(())
    }

    fn commit(&mut self, id: u32) -> Result<(), u32> {
        let mut draft = self.take(id)?;
        let sealed = self.seal(&mut draft);
        if sealed.is_err() {
            self.rollback();
        }
        sealed
    }

    fn append(&mut self, text: &[u8], name_len: u32, expected: u32, reply: &mut Msg) -> Result<(), u32> {
        let name_len = name_len as usize;
        if name_len > text.len() {
            return Err(D_BAD_NAME);
        }
        let (name, bytes) = text.split_at(name_len);
        let known = find(self.meta, name);
        let size = known.map_or(0, |index| size_of(self.meta, index));
        if size != expected {
            reply.words[0] = size;
            return Err(D_MOVED);
        }
        if known.is_some() && bytes.is_empty() {
            return Ok(());
        }
        let mut draft = self.open(name, true)?;
        let mut added = self.push(&mut draft, bytes);
        if added.is_ok() {
            added = self.seal(&mut draft);
        }
        if added.is_err() {
            self.rollback();
        }
        added
    }

    fn delete(&mut self, name: &[u8]) -> Result<(), u32> {
        self.rollback();
        let index = find(self.meta, name).ok_or(D_NO_FILE)?;
        let mut block = first_of(self.meta, index);
        for _ in 0..blocks_for(size_of(self.meta, index)) {
            let following = next(self.work, block);
            set_next(self.work, block, FREE);
            block = following;
        }
        self.work[DIRECTORY_AT + ENTRY * index..][..ENTRY].fill(0);
        let published = self.publish();
        if published.is_err() {
            self.rollback();
        }
        published
    }

    fn info(&self, reply: &mut Msg) -> Result<(), u32> {
        let free = (2..self.total).filter(|&block| next(self.meta, block) == FREE).count() as u32;
        let files = (0..ENTRIES).filter(|&index| in_use(self.meta, index)).count() as u32;
        reply.words = [self.total - 2, free, files, self.start_up_us];
        Ok(())
    }

    fn answer(&mut self, msg: &Msg) -> Msg {
        let mut reply = Msg::new(OK);
        let text = &msg.data[..(msg.len as usize).min(MSG_DATA)];
        let done = match msg.label {
            PING => Ok(()),
            // Fault injection for the recovery tests, honoured only for the shell.
            DBG_CRASH if msg.badge == B_SHELL => rt::crash(),
            DBG_HANG if msg.badge == B_SHELL => loop {
                core::hint::spin_loop();
            },
            FS_LIST | FS_STAT | FS_READ | DISK_CREATE..=DISK_INFO if self.state != 0 => Err(self.state),
            FS_LIST => self.list(msg.words[0], &mut reply),
            FS_STAT => self.stat(io::name(text), &mut reply),
            FS_READ => self.read(io::name(text), msg.words[0], msg.words[1], &mut reply),
            DISK_CREATE => self.create(io::name(text), &mut reply),
            DISK_WRITE => self.write(msg.words[0], text),
            DISK_COMMIT => self.commit(msg.words[0]),
            DISK_APPEND => self.append(text, msg.words[0], msg.words[1], &mut reply),
            DISK_DELETE => self.delete(io::name(text)),
            DISK_INFO => self.info(&mut reply),
            _ => {
                reply.label = DENIED;
                Ok(())
            }
        };
        if let Err(reason) = done {
            reply.label = ERR;
            reply.len = 0;
            reply.words[1] = reason;
        }
        reply
    }
}

#[no_mangle]
pub fn main() {
    let began = sys::time();
    if sys::map(BUFFERS, MEMORY_PAGES, true).is_err() {
        sys::exit(1);
    }
    // SAFETY: the kernel just mapped MEMORY_PAGES fresh, zeroed, writable pages at BUFFERS.
    // That is at least the size of `Memory`, a page boundary satisfies its alignment, and
    // any bytes are a valid value of it: it holds nothing but integers. This is the only
    // reference ever made to those pages and they are never unmapped.
    let memory: &'static mut Memory = unsafe { &mut *(BUFFERS as *mut Memory) };
    let Memory { blocks: [meta, work, cache, pending], held } = memory;
    let [starts, root_starts, _, _] = rt::args();
    let mut ata = Ata { sectors: 0, unsure: false, held, oldest: 0, count: 0, listen_at: 0 };
    ata.probe(starts > 0 || root_starts > 0);
    let mut volume = Volume {
        ata,
        state: D_NO_DRIVE,
        start: 0,
        total: 0,
        active: 0,
        meta,
        work,
        cache,
        cached: NONE,
        pending,
        draft: None,
        // Transaction numbers start at the clock, so that a number handed out before a
        // restart of this server is never taken for an open one.
        next_id: began as u32 | 1,
        start_up_us: 0,
    };
    volume.state = volume.mount();
    volume.start_up_us = (sys::time() - began) as u32;
    loop {
        // Requests that arrived while the drive was busy come first, in their order.
        let msg = volume.ata.take_held().unwrap_or_else(|| {
            let mut msg = Msg::new(0);
            if sys::recv(REQUESTS, &mut msg, FOREVER).is_err() {
                sys::exit(2);
            }
            msg
        });
        if msg.label & L_KERNEL == 0 {
            let reply = volume.answer(&msg);
            let _ = sys::reply(msg.token, &reply);
        }
    }
}
