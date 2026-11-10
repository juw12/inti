//! Physical frames and page tables.
//!
//! The kernel identity-maps the first 128 MiB for itself and never dereferences a user
//! virtual address. User memory is reached by walking the task's page tables in software and
//! touching the frame through the identity map, so a bad user pointer is an error code and
//! never a page fault in the kernel.

use crate::arch;
use crate::task::Kernel;
use crate::Global;
use abi::{E_BUSY, E_FAULT, E_INVAL, E_NOMEM, PAGE, USER_BASE, USER_END};
use core::ops::Range;

/// The kernel manages at most this much RAM (the size of its identity map).
const MAX_RAM: u32 = 128 << 20;
const FRAMES: usize = (MAX_RAM / PAGE) as usize;
const KERNEL_TABLES: usize = (MAX_RAM >> 22) as usize;
pub const ENTRIES: usize = 1024;

/// Page table entry bits: present, writable, user-accessible.
pub const P: u32 = 1;
pub const RW: u32 = 2;
pub const US: u32 = 4;
/// The processor sets this bit by itself in every entry it walks through.
pub const ACCESSED: u32 = 0x20;
/// Software bit: the frame comes from a Memory capability and is not owned by the task.
/// It is never freed, never charged, and the kernel never writes through it.
pub const FOREIGN: u32 = 0x200;
pub const ADDR: u32 = !(PAGE - 1);
/// Page directory slots that belong to user space.
pub const USER_PDES: Range<usize> = (USER_BASE >> 22) as usize..(USER_END >> 22) as usize;

#[repr(C, align(4096))]
struct Table([u32; ENTRIES]);

const EMPTY_TABLE: Table = Table([0; ENTRIES]);
static KERNEL_DIRECTORY: Global<Table> = Global::new(EMPTY_TABLE);
static KERNEL_MAP: Global<[Table; KERNEL_TABLES]> = Global::new([EMPTY_TABLE; KERNEL_TABLES]);

extern "C" {
    static __kernel_start: u8;
    static __rodata_end: u8;
}

pub fn page_up(value: u32) -> u32 {
    (value + PAGE - 1) & ADDR
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
    unsafe { (address as *const u32).read_unaligned() }
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
    pub kernel_pd: u32,
    /// Size of the kernel image plus the boot archive, in pages.
    pub kernel_pages: u32,
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

        let kernel_start = &raw const __kernel_start as u32;
        let rodata_end = &raw const __rodata_end as u32;
        self.kernel_pages = (kernel_end - kernel_start) / PAGE;
        // SAFETY: boot time, single CPU, interrupts off: nothing else refers to the static
        // page tables yet, so the two exclusive references are unique.
        let (directory, tables) = unsafe { (&mut *KERNEL_DIRECTORY.get(), &mut *KERNEL_MAP.get()) };
        for (t, table) in tables.iter_mut().enumerate() {
            for (i, entry) in table.0.iter_mut().enumerate() {
                let address = ((t * ENTRIES + i) as u32) * PAGE;
                let read_only = (kernel_start..rodata_end).contains(&address);
                // Page 0 stays unmapped so that a null pointer in the kernel faults.
                *entry = match address {
                    0 => 0,
                    _ if read_only => address | P,
                    _ => address | P | RW,
                };
            }
            directory.0[t] = Self::kernel_pde(t);
        }
        self.kernel_pd = KERNEL_DIRECTORY.get() as u32;
        arch::enable_paging(self.kernel_pd);
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

    /// The directory entry that maps the `index`-th 4 MiB of the kernel half.
    fn kernel_pde(index: usize) -> u32 {
        (KERNEL_MAP.get() as u32 + (index as u32) * PAGE) | P | RW
    }

    /// What slot `index` of every page directory must hold outside the user range: the shared
    /// kernel mapping (never user-accessible) or nothing. Used by the invariant checker.
    pub fn kernel_slot(index: usize) -> u32 {
        if index < KERNEL_TABLES {
            Self::kernel_pde(index)
        } else {
            0
        }
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
    // ---- object; each checks that the access stays inside one legitimate page

    pub fn read_entry(&self, table: u32, index: usize) -> u32 {
        assert!(self.managed(table) && index < ENTRIES);
        // SAFETY: `table` is a managed RAM page inside the identity map and `index` keeps the
        // read inside it. Managed frames are never part of a Rust object.
        unsafe { ((table as usize + index * 4) as *const u32).read_volatile() }
    }

    fn write_entry(&mut self, table: u32, index: usize, value: u32) {
        assert!(self.managed(table) && index < ENTRIES);
        // SAFETY: as in `read_entry`; managed frames are mapped writable for the kernel and
        // contain no kernel code or data, so the write cannot corrupt the kernel itself.
        unsafe { ((table as usize + index * 4) as *mut u32).write_volatile(value) }
    }

    fn zero(&mut self, frame: u32) {
        assert!(self.managed(frame));
        // SAFETY: the whole page belongs to the managed frame (see `write_entry`).
        unsafe { core::ptr::write_bytes(frame as *mut u8, 0, PAGE as usize) }
    }

    fn read_frame(&self, frame: u32, offset: u32, out: &mut [u8]) {
        let inside = offset as usize + out.len() <= PAGE as usize;
        assert!(frame % PAGE == 0 && (PAGE..MAX_RAM).contains(&frame) && inside);
        // SAFETY: the source lies inside one identity-mapped page. `frame` was taken from a
        // user page table entry, and those only ever hold managed frames or frames of a
        // Memory capability (screen, boot archive), never the kernel image, so the source
        // cannot overlap `out`, which is a kernel buffer.
        unsafe {
            let source = (frame + offset) as *const u8;
            core::ptr::copy_nonoverlapping(source, out.as_mut_ptr(), out.len());
        }
    }

    pub fn write_frame(&mut self, frame: u32, offset: u32, data: &[u8]) {
        assert!(self.managed(frame) && offset as usize + data.len() <= PAGE as usize);
        // SAFETY: the destination lies inside one managed frame (see `write_entry`), which
        // cannot overlap the kernel buffer `data`.
        unsafe {
            let target = (frame + offset) as *mut u8;
            core::ptr::copy_nonoverlapping(data.as_ptr(), target, data.len());
        }
    }

    // ---- user address spaces

    /// The page table entry for user address `va` in address space `pd`, if present.
    pub fn lookup(&self, pd: u32, va: u32) -> Option<u32> {
        if !(USER_BASE..USER_END).contains(&va) {
            return None;
        }
        let pde = self.read_entry(pd, (va >> 22) as usize);
        if pde & P == 0 {
            return None;
        }
        let pte = self.read_entry(pde & ADDR, (va >> 12) as usize % ENTRIES);
        (pte & P != 0).then_some(pte)
    }

    /// The frame behind user address `va` if the task itself may read (or write) there.
    fn user_frame(&self, pd: u32, va: u32, write: bool) -> Option<u32> {
        let pte = self.lookup(pd, va)?;
        let frame = pte & ADDR;
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

    /// Give task `t` an empty address space: a page directory that shares the kernel half.
    pub fn new_space(&mut self, t: usize) -> Result<(), i32> {
        let pd = self.alloc_for(t)?;
        for index in 0..KERNEL_TABLES {
            self.mem.write_entry(pd, index, Mem::kernel_pde(index));
        }
        self.tasks[t].pd = pd;
        Ok(())
    }

    /// Map `frame` at user address `va` of task `t`. The page table is allocated on demand
    /// and charged to the task.
    pub fn map_page(&mut self, t: usize, va: u32, frame: u32, flags: u32) -> Result<(), i32> {
        assert!((USER_BASE..USER_END).contains(&va) && va % PAGE == 0);
        let pd = self.tasks[t].pd;
        let slot = (va >> 22) as usize;
        let pde = self.mem.read_entry(pd, slot);
        let table = if pde & P != 0 {
            pde & ADDR
        } else {
            let table = self.alloc_for(t)?;
            self.mem.write_entry(pd, slot, table | P | RW | US);
            table
        };
        let index = (va >> 12) as usize % ENTRIES;
        if self.mem.read_entry(table, index) & P != 0 {
            return Err(E_BUSY);
        }
        self.mem.write_entry(table, index, frame | flags | P);
        if flags & FOREIGN != 0 {
            self.tasks[t].foreign += 1;
        }
        Ok(())
    }

    /// Map a fresh zeroed page of the task's own memory.
    pub fn map_anon(&mut self, t: usize, va: u32, writable: bool) -> Result<(), i32> {
        if self.mem.lookup(self.tasks[t].pd, va).is_some() {
            return Err(E_BUSY);
        }
        let frame = self.alloc_for(t)?;
        let flags = if writable { US | RW } else { US };
        self.map_page(t, va, frame, flags).inspect_err(|_| self.free_for(t, frame))
    }

    /// Make an already mapped page of task `t` writable (used while loading a program).
    pub fn make_writable(&mut self, t: usize, va: u32) {
        let pde = self.mem.read_entry(self.tasks[t].pd, (va >> 22) as usize);
        let index = (va >> 12) as usize % ENTRIES;
        let pte = self.mem.read_entry(pde & ADDR, index);
        self.mem.write_entry(pde & ADDR, index, pte | RW);
    }

    /// Unmap `pages` pages starting at `va` (validated by the caller); holes are skipped.
    /// Frames the task owns go back to the allocator. Page tables are kept for reuse.
    pub fn unmap_range(&mut self, t: usize, va: u32, pages: u32) {
        let pd = self.tasks[t].pd;
        let end = va as u64 + pages as u64 * PAGE as u64;
        let mut address = va;
        while (address as u64) < end {
            let pde = self.mem.read_entry(pd, (address >> 22) as usize);
            if pde & P == 0 {
                address = (address | 0x3F_FFFF) + 1; // skip the whole 4 MiB
                continue;
            }
            let index = (address >> 12) as usize % ENTRIES;
            let pte = self.mem.read_entry(pde & ADDR, index);
            if pte & P != 0 {
                self.mem.write_entry(pde & ADDR, index, 0);
                if pte & FOREIGN == 0 {
                    self.free_for(t, pte & ADDR);
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
        for slot in USER_PDES {
            let pde = self.mem.read_entry(pd, slot);
            if pde & P == 0 {
                continue;
            }
            for index in 0..ENTRIES {
                let pte = self.mem.read_entry(pde & ADDR, index);
                let device = pte & (P | FOREIGN) == P | FOREIGN;
                if device && !self.tasks[t].has_memory(pte & ADDR, pte & RW != 0) {
                    self.mem.write_entry(pde & ADDR, index, 0);
                    self.tasks[t].foreign -= 1;
                }
            }
        }
        if pd == self.space {
            arch::write_cr3(pd); // drop stale translations
        }
    }

    /// Free everything task `t` owns: its pages, page tables and directory.
    /// The address space must not be the one loaded in the CPU.
    pub fn destroy_space(&mut self, t: usize) {
        let pd = self.tasks[t].pd;
        if pd == 0 {
            return;
        }
        assert!(pd != self.space);
        for slot in USER_PDES {
            let pde = self.mem.read_entry(pd, slot);
            if pde & P == 0 {
                continue;
            }
            for index in 0..ENTRIES {
                let pte = self.mem.read_entry(pde & ADDR, index);
                if pte & P != 0 && pte & FOREIGN == 0 {
                    self.free_for(t, pte & ADDR);
                }
            }
            self.free_for(t, pde & ADDR);
        }
        self.free_for(t, pd);
        self.tasks[t].pd = 0;
        self.tasks[t].foreign = 0;
        assert!(self.tasks[t].charged == 0, "memory accounting of task {t} is off");
    }
}
