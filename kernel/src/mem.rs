//! Physical frames and page tables.
//!
//! The kernel identity-maps the first 128 MiB for itself and never dereferences a user
//! virtual address. User memory is reached by walking the task's page tables in software and
//! touching the frame through the identity map, so a bad user pointer is an error code and
//! never a page fault in the kernel.
//!
//! Paging is the three-level PAE form with 64-bit entries, because only that form has the
//! no-execute bit. Where the processor has that bit, memory is writable or executable and
//! never both (W^X): kernel code and program code are read-only, and nothing else can be
//! executed. A processor without the bit still runs Inti, without that protection.

use crate::arch;
use crate::task::Kernel;
use abi::{E_BUSY, E_FAULT, E_INVAL, E_NOMEM, PAGE, USER_BASE, USER_END};
use core::ops::Range;

/// The kernel manages at most this much RAM (the size of its identity map).
const MAX_RAM: u32 = 128 << 20;
const FRAMES: usize = (MAX_RAM / PAGE) as usize;
/// Page tables of the identity map; each maps 2 MiB.
const KERNEL_TABLES: usize = (MAX_RAM >> 21) as usize;
pub const ENTRIES: usize = 512;

/// Page table entry bits: present, writable, user-accessible.
pub const P: u64 = 1;
pub const RW: u64 = 2;
pub const US: u64 = 4;
/// The processor sets these two bits by itself: in every entry it walks through, and in the
/// entry of a page it writes to.
pub const ACCESSED: u64 = 0x20;
pub const DIRTY: u64 = 0x40;
/// Software bit: the frame comes from a Memory capability and is not owned by the task.
/// It is never freed, never charged, and the kernel never writes through it.
pub const FOREIGN: u64 = 0x200;
/// Software bit: the page holds program code. Only such a page may be executed, and it is
/// never writable.
pub const EXEC: u64 = 0x400;
/// No-execute. Only valid on a processor that has the bit (see `Mem::nx`).
pub const NX: u64 = 1 << 63;
/// The frame address in an entry. Inti uses physical memory below 4 GiB only.
pub const ADDR: u64 = 0xFFFF_F000;
/// Root table slots that belong to user space; each covers 1 GiB through one directory.
pub const USER_ROOTS: Range<usize> = (USER_BASE >> 30) as usize..(USER_END >> 30) as usize;
/// Page tables that the user range can hold.
pub const USER_TABLES: usize = ((USER_END - USER_BASE) >> 21) as usize;

// The kernel map fills root slot 0; the user range is whole gigabytes above it.
const _: () = assert!(MAX_RAM <= 1 << 30 && USER_BASE >= 1 << 30);
const _: () = assert!(USER_BASE % (1 << 30) == 0 && USER_END % (1 << 30) == 0);

/// The kernel's own map, shared by every address space: tables `0..KERNEL_TABLES`
/// identity-map the RAM, then come the directory that points to them and the root table.
/// `arch` reserves the memory for them: this many bytes.
pub const KERNEL_MAP_BYTES: usize = (KERNEL_TABLES + 2) * PAGE as usize;
const DIRECTORY: usize = KERNEL_TABLES;
const ROOT: usize = KERNEL_TABLES + 1;

pub fn page_up(value: u32) -> u32 {
    (value + PAGE - 1) & !(PAGE - 1)
}

/// The frame address in a page table entry.
pub fn frame_of(entry: u64) -> u32 {
    (entry & ADDR) as u32
}

/// The address of table `table` of the kernel's own map.
fn kernel_table(table: usize) -> u32 {
    assert!(table < KERNEL_TABLES + 2);
    arch::kernel_map() + table as u32 * PAGE
}

/// Entry `index` of table `table` of the kernel's own map.
fn kernel_slot(table: usize, index: usize) -> *mut u64 {
    assert!(index < ENTRIES);
    arch::phys(kernel_table(table) + index as u32 * 8).cast()
}

/// Check that `pages` pages starting at `va` are page-aligned and inside user space.
pub fn user_range(va: u32, pages: u32) -> Result<(), i32> {
    let end = va as u64 + pages as u64 * PAGE as u64;
    if va % PAGE != 0 || pages == 0 || va < USER_BASE || end > USER_END as u64 {
        return Err(E_INVAL);
    }
    Ok(())
}

/// Read one word of the BIOS memory map that the boot sector stored in low memory.
fn boot_word(address: u32) -> u32 {
    assert!((0x8000..0x8400).contains(&address));
    // SAFETY: the boot sector wrote the memory map to 0x8000..0x83C4. That memory is mapped,
    // belongs to no Rust object and is not written again after boot.
    unsafe { arch::phys(address).cast::<u32>().read_unaligned() }
}

/// The frame allocator: one bit per frame, set while the frame is free.
pub struct Mem {
    bitmap: [u32; FRAMES / 32],
    /// Managed frames lie in `first..end` (frame numbers).
    first: u32,
    end: u32,
    pub free: u32,
    pub total: u32,
    hint: usize,
    /// The kernel's own root table, in use while no task's address space is.
    pub kernel_pd: u32,
    /// Size of the kernel image plus the boot archive, in pages.
    pub kernel_pages: u32,
    /// `NX` if the processor has the no-execute bit, else 0: the bit to put in every entry
    /// that maps something other than code. Without it, W^X is not enforced.
    pub nx: u64,
}

impl Mem {
    pub const fn new() -> Mem {
        Mem {
            bitmap: [0; FRAMES / 32],
            first: u32::MAX,
            end: 0,
            free: 0,
            total: 0,
            hint: 0,
            kernel_pd: 0,
            kernel_pages: 0,
            nx: 0,
        }
    }

    /// Take over all usable RAM above `kernel_end` and turn on paging.
    pub fn init(&mut self, e820: u32, kernel_end: u32) {
        let count = (boot_word(e820) & 0xFFFF).min(40);
        for i in 0..count {
            let entry = e820 + 4 + i * 24;
            let start = boot_word(entry) as u64 | (boot_word(entry + 4) as u64) << 32;
            let length = boot_word(entry + 8) as u64 | (boot_word(entry + 12) as u64) << 32;
            if boot_word(entry + 16) != 1 {
                continue; // not usable RAM
            }
            let page = PAGE as u64;
            let first = ((start + page - 1) / page).max((kernel_end / PAGE) as u64);
            let end = (start.saturating_add(length) / page).min(FRAMES as u64);
            for frame in first..end {
                self.add(frame as u32);
            }
        }
        assert!(self.total > 0, "the BIOS reported no usable memory");
        self.free = self.total;

        let (pae, nx) = arch::cpu_features();
        assert!(pae, "this processor has no PAE paging, which Inti needs");
        self.nx = if nx { NX } else { 0 };
        let (kernel_start, _, _) = arch::kernel_image();
        self.kernel_pages = (kernel_end - kernel_start) / PAGE;
        for table in 0..KERNEL_TABLES + 2 {
            for index in 0..ENTRIES {
                let entry = self.kernel_entry(table, index);
                // SAFETY: the slot lies inside the memory that `arch` reserves for the
                // kernel's page tables (`kernel_slot` checks both indices) and is aligned
                // for a u64. Boot time, single CPU, interrupts off: nothing else uses that
                // memory yet.
                unsafe { kernel_slot(table, index).write_volatile(entry) }
            }
        }
        self.kernel_pd = kernel_table(ROOT);
        arch::enable_paging(self.kernel_pd, nx);
    }

    fn add(&mut self, frame: u32) {
        let (word, bit) = (frame as usize / 32, 1u32 << (frame % 32));
        if self.bitmap[word] & bit == 0 {
            self.bitmap[word] |= bit;
            self.total += 1;
            self.first = self.first.min(frame);
            self.end = self.end.max(frame + 1);
        }
    }

    /// What entry `index` of table `table` of the kernel's own map must hold. `init` fills
    /// the map from this, and the invariant checker compares the map with it ever after.
    fn kernel_entry(&self, table: usize, index: usize) -> u64 {
        match table {
            ROOT => Self::kernel_root(index),
            DIRECTORY if index < KERNEL_TABLES => kernel_table(index) as u64 | P | RW,
            DIRECTORY => 0,
            _ => self.kernel_pte((table * ENTRIES + index) as u32 * PAGE),
        }
    }

    /// The entry that identity-maps `address` for the kernel. Kernel code is read-only;
    /// constants are read-only and cannot be executed; all other memory is writable and
    /// cannot be executed. No entry is user-accessible.
    fn kernel_pte(&self, address: u32) -> u64 {
        let (start, text_end, rodata_end) = arch::kernel_image();
        let entry = address as u64 | P;
        if address == 0 {
            0 // page 0 stays unmapped so that a null pointer in the kernel faults
        } else if (start..text_end).contains(&address) {
            entry
        } else if (text_end..rodata_end).contains(&address) {
            entry | self.nx
        } else {
            entry | RW | self.nx
        }
    }

    /// What slot `index` of every root table must hold outside the user range: the shared
    /// kernel map in slot 0 and nothing anywhere else. Used by the invariant checker.
    pub fn kernel_root(index: usize) -> u64 {
        if index == 0 {
            kernel_table(DIRECTORY) as u64 | P
        } else {
            0
        }
    }

    /// True if the kernel's own map still holds exactly what `init` put there.
    pub fn kernel_map_intact(&self) -> bool {
        (0..KERNEL_TABLES + 2).all(|table| {
            (0..ENTRIES).all(|index| {
                // SAFETY: the slot lies inside the memory that `arch` reserves for the
                // kernel's page tables (`kernel_slot` checks both indices) and is aligned
                // for a u64. After boot only the processor writes there (the accessed and
                // dirty bits), hence a volatile read through a raw pointer and no reference.
                let entry = unsafe { kernel_slot(table, index).read_volatile() };
                entry & !(ACCESSED | DIRTY) == self.kernel_entry(table, index)
            })
        })
    }

    /// True if `frame` is the address of a page of RAM this allocator hands out.
    pub fn managed(&self, frame: u32) -> bool {
        frame % PAGE == 0 && (self.first..self.end).contains(&(frame / PAGE))
    }

    pub fn is_free(&self, frame: u32) -> bool {
        let number = (frame / PAGE) as usize;
        self.bitmap[number / 32] & (1 << (number % 32)) != 0
    }

    pub fn count_free(&self) -> u32 {
        self.bitmap.iter().map(|word| word.count_ones()).sum()
    }

    /// Allocate a zeroed frame.
    pub fn alloc(&mut self) -> Option<u32> {
        let words = self.bitmap.len();
        for step in 0..words {
            let word = (self.hint + step) % words;
            if self.bitmap[word] != 0 {
                let bit = self.bitmap[word].trailing_zeros();
                self.bitmap[word] &= !(1 << bit);
                self.hint = word;
                self.free -= 1;
                let frame = (word as u32 * 32 + bit) * PAGE;
                self.zero(frame);
                return Some(frame);
            }
        }
        None
    }

    pub fn free_frame(&mut self, frame: u32) {
        assert!(self.managed(frame) && !self.is_free(frame), "bad free of frame {frame:#x}");
        let number = (frame / PAGE) as usize;
        self.bitmap[number / 32] |= 1 << (number % 32);
        self.free += 1;
    }

    // ---- the only five places where the kernel touches memory it does not own as a Rust
    // ---- object; each checks that the access stays inside one legitimate page and gets
    // ---- its pointer from `arch::phys`

    pub fn read_entry(&self, table: u32, index: usize) -> u64 {
        assert!(self.managed(table) && index < ENTRIES);
        // SAFETY: `table` is a managed RAM page inside the identity map and `index` keeps the
        // read inside it. Managed frames are never part of a Rust object.
        unsafe { arch::phys(table + index as u32 * 8).cast::<u64>().read_volatile() }
    }

    fn write_entry(&mut self, table: u32, index: usize, value: u64) {
        assert!(self.managed(table) && index < ENTRIES);
        // SAFETY: as in `read_entry`; managed frames are mapped writable for the kernel and
        // contain no kernel code or data, so the write cannot corrupt the kernel itself.
        unsafe { arch::phys(table + index as u32 * 8).cast::<u64>().write_volatile(value) }
    }

    fn zero(&mut self, frame: u32) {
        assert!(self.managed(frame));
        // SAFETY: the whole page belongs to the managed frame (see `write_entry`).
        unsafe { core::ptr::write_bytes(arch::phys(frame), 0, PAGE as usize) }
    }

    fn read_frame(&self, frame: u32, offset: u32, out: &mut [u8]) {
        let inside = offset as usize + out.len() <= PAGE as usize;
        assert!(frame % PAGE == 0 && (PAGE..MAX_RAM).contains(&frame) && inside);
        // SAFETY: the source lies inside one identity-mapped page. `frame` was taken from a
        // user page table entry, and those only ever hold managed frames or frames of a
        // Memory capability (screen, boot archive), never the kernel image, so the source
        // cannot overlap `out`, which is a kernel buffer.
        unsafe {
            let source = arch::phys(frame + offset).cast_const();
            core::ptr::copy_nonoverlapping(source, out.as_mut_ptr(), out.len());
        }
    }

    pub fn write_frame(&mut self, frame: u32, offset: u32, data: &[u8]) {
        assert!(self.managed(frame) && offset as usize + data.len() <= PAGE as usize);
        // SAFETY: the destination lies inside one managed frame (see `write_entry`), which
        // cannot overlap the kernel buffer `data`.
        unsafe {
            let target = arch::phys(frame + offset);
            core::ptr::copy_nonoverlapping(data.as_ptr(), target, data.len());
        }
    }

    // ---- user address spaces

    /// The `n`-th page table of the user range of address space `root`, if it exists.
    /// Each maps 2 MiB.
    pub fn user_table(&self, root: u32, n: usize) -> Option<u32> {
        if n >= USER_TABLES {
            return None;
        }
        let directory = self.read_entry(root, USER_ROOTS.start + n / ENTRIES);
        if directory & P == 0 {
            return None;
        }
        let entry = self.read_entry(frame_of(directory), n % ENTRIES);
        (entry & P != 0).then(|| frame_of(entry))
    }

    /// The page table entry for user address `va` in address space `root`, if present.
    pub fn lookup(&self, root: u32, va: u32) -> Option<u64> {
        if !(USER_BASE..USER_END).contains(&va) {
            return None;
        }
        let table = self.user_table(root, ((va - USER_BASE) >> 21) as usize)?;
        let pte = self.read_entry(table, (va >> 12) as usize % ENTRIES);
        (pte & P != 0).then_some(pte)
    }

    /// The frame behind user address `va` if the task itself may read (or write) there.
    fn user_frame(&self, pd: u32, va: u32, write: bool) -> Option<u32> {
        let pte = self.lookup(pd, va)?;
        let frame = frame_of(pte);
        let allowed = if write {
            pte & (RW | FOREIGN) == RW && self.managed(frame)
        } else {
            (PAGE..MAX_RAM).contains(&frame)
        };
        (pte & US != 0 && allowed).then_some(frame)
    }

    /// Apply `step` to every (frame, offset, length, done) piece of a user byte range.
    fn walk(
        &self,
        pd: u32,
        va: u32,
        len: usize,
        write: bool,
        mut step: impl FnMut(u32, u32, usize, usize),
    ) -> Result<(), i32> {
        let mut done = 0;
        while done < len {
            let address = va.checked_add(done as u32).ok_or(E_FAULT)?;
            let offset = address % PAGE;
            let piece = (len - done).min((PAGE - offset) as usize);
            let frame = self.user_frame(pd, address, write).ok_or(E_FAULT)?;
            step(frame, offset, piece, done);
            done += piece;
        }
        Ok(())
    }

    /// Check that a task may read (or write) `len` bytes at `va`.
    pub fn check_user(&self, pd: u32, va: u32, len: u32, write: bool) -> Result<(), i32> {
        self.walk(pd, va, len as usize, write, |_, _, _, _| {})
    }

    /// Copy from user memory into a kernel buffer.
    pub fn copy_in(&self, pd: u32, va: u32, out: &mut [u8]) -> Result<(), i32> {
        self.walk(pd, va, out.len(), false, |frame, offset, piece, done| {
            self.read_frame(frame, offset, &mut out[done..done + piece]);
        })
    }

    /// Copy a kernel buffer into user memory. Nothing is written unless the whole range is
    /// writable.
    pub fn copy_out(&mut self, pd: u32, va: u32, data: &[u8]) -> Result<(), i32> {
        self.check_user(pd, va, data.len() as u32, true)?;
        let mut done = 0;
        while done < data.len() {
            let address = va + done as u32;
            let offset = address % PAGE;
            let piece = (data.len() - done).min((PAGE - offset) as usize);
            let frame = self.user_frame(pd, address, true).ok_or(E_FAULT)?;
            self.write_frame(frame, offset, &data[done..done + piece]);
            done += piece;
        }
        Ok(())
    }
}

impl Kernel {
    /// Take one frame out of task `t`'s quota.
    pub fn alloc_for(&mut self, t: usize) -> Result<u32, i32> {
        if self.tasks[t].charged >= self.tasks[t].quota {
            return Err(E_NOMEM);
        }
        let frame = self.mem.alloc().ok_or(E_NOMEM)?;
        self.tasks[t].charged += 1;
        Ok(frame)
    }

    fn free_for(&mut self, t: usize, frame: u32) {
        self.mem.free_frame(frame);
        self.tasks[t].charged -= 1;
    }

    /// Give task `t` an empty address space: a root table that shares the kernel map, and
    /// the directories of the user range. The root is never changed afterwards.
    pub fn new_space(&mut self, t: usize) -> Result<(), i32> {
        let root = self.alloc_for(t)?;
        self.tasks[t].pd = root;
        self.mem.write_entry(root, 0, Mem::kernel_root(0));
        for slot in USER_ROOTS {
            let directory = self.alloc_for(t)?;
            self.mem.write_entry(root, slot, directory as u64 | P);
        }
        Ok(())
    }

    /// Map `frame` at user address `va` of task `t`. The page table is allocated on demand
    /// and charged to the task. Only a page with `EXEC` can be executed, and such a page is
    /// never writable.
    pub fn map_page(&mut self, t: usize, va: u32, frame: u32, flags: u64) -> Result<(), i32> {
        assert!((USER_BASE..USER_END).contains(&va) && va % PAGE == 0);
        assert!(flags & (RW | EXEC) != RW | EXEC, "a page is never writable and executable");
        let directory = frame_of(self.mem.read_entry(self.tasks[t].pd, (va >> 30) as usize));
        let slot = (va >> 21) as usize % ENTRIES;
        let pde = self.mem.read_entry(directory, slot);
        let table = if pde & P != 0 {
            frame_of(pde)
        } else {
            let table = self.alloc_for(t)?;
            self.mem.write_entry(directory, slot, table as u64 | P | RW | US);
            table
        };
        let index = (va >> 12) as usize % ENTRIES;
        if self.mem.read_entry(table, index) & P != 0 {
            return Err(E_BUSY);
        }
        let no_execute = if flags & EXEC == 0 { self.mem.nx } else { 0 };
        self.mem.write_entry(table, index, frame as u64 | flags | P | no_execute);
        if flags & FOREIGN != 0 {
            self.tasks[t].foreign += 1;
        }
        Ok(())
    }

    /// Map a fresh zeroed page of the task's own memory with the access that `flags` gives.
    pub fn map_anon(&mut self, t: usize, va: u32, flags: u64) -> Result<(), i32> {
        if self.mem.lookup(self.tasks[t].pd, va).is_some() {
            return Err(E_BUSY);
        }
        let frame = self.alloc_for(t)?;
        self.map_page(t, va, frame, flags).inspect_err(|_| self.free_for(t, frame))
    }

    /// Unmap `pages` pages starting at `va` (validated by the caller); holes are skipped.
    /// Frames the task owns go back to the allocator. Page tables are kept for reuse.
    pub fn unmap_range(&mut self, t: usize, va: u32, pages: u32) {
        let pd = self.tasks[t].pd;
        let end = va as u64 + pages as u64 * PAGE as u64;
        let mut address = va;
        while (address as u64) < end {
            let Some(table) = self.mem.user_table(pd, ((address - USER_BASE) >> 21) as usize) else {
                address = (address | 0x1F_FFFF) + 1; // skip the whole 2 MiB
                continue;
            };
            let index = (address >> 12) as usize % ENTRIES;
            let pte = self.mem.read_entry(table, index);
            if pte & P != 0 {
                self.mem.write_entry(table, index, 0);
                if pte & FOREIGN == 0 {
                    self.free_for(t, frame_of(pte));
                } else {
                    self.tasks[t].foreign -= 1;
                }
            }
            address += PAGE;
        }
        if pd == self.space {
            arch::write_cr3(pd); // drop stale translations
        }
    }

    /// Unmap every page of device memory in task `t` that no Memory capability of the task
    /// covers any more, after one was dropped or revoked.
    pub fn unmap_unbacked(&mut self, t: usize) {
        let pd = self.tasks[t].pd;
        if self.tasks[t].foreign == 0 {
            return;
        }
        for n in 0..USER_TABLES {
            let Some(table) = self.mem.user_table(pd, n) else { continue };
            for index in 0..ENTRIES {
                let pte = self.mem.read_entry(table, index);
                let device = pte & (P | FOREIGN) == P | FOREIGN;
                if device && !self.tasks[t].has_memory(frame_of(pte), pte & RW != 0) {
                    self.mem.write_entry(table, index, 0);
                    self.tasks[t].foreign -= 1;
                }
            }
        }
        if pd == self.space {
            arch::write_cr3(pd); // drop stale translations
        }
    }

    /// Free everything task `t` owns: its pages, page tables, directories and root table.
    /// The address space must not be the one loaded in the CPU.
    pub fn destroy_space(&mut self, t: usize) {
        let pd = self.tasks[t].pd;
        if pd == 0 {
            return;
        }
        assert!(pd != self.space);
        for n in 0..USER_TABLES {
            let Some(table) = self.mem.user_table(pd, n) else { continue };
            for index in 0..ENTRIES {
                let pte = self.mem.read_entry(table, index);
                if pte & P != 0 && pte & FOREIGN == 0 {
                    self.free_for(t, frame_of(pte));
                }
            }
            self.free_for(t, table);
        }
        for slot in USER_ROOTS {
            let directory = self.mem.read_entry(pd, slot);
            if directory & P != 0 {
                self.free_for(t, frame_of(directory));
            }
        }
        self.free_for(t, pd);
        self.tasks[t].pd = 0;
        self.tasks[t].foreign = 0;
        assert!(self.tasks[t].charged == 0, "memory accounting of task {t} is off");
    }
}
