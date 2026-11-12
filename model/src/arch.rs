//! The fake machine under the kernel.
//!
//! The real `kernel/src/arch.rs` is the only kernel module that touches hardware. This file
//! stands in for it: the same functions with the same signatures, but behind them an array
//! for RAM and a few variables for the paging switch, the interrupt mask and the timer.
//! The other seven kernel modules are compiled unchanged and cannot tell the difference.
//!
//! Two things here exist only for the checking:
//!
//! * `phys` refuses an address outside RAM. The kernel turns addresses into pointers in
//!   exactly one place, so every memory access of the seven modules passes this check.
//! * Snapshots. The explorer tries a step, looks, and goes back. A full copy of RAM is made
//!   once ("base"); after that only the frames written since are copied ("parent").

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::cell::UnsafeCell;
use std::ptr;

pub use crate::frame::{TrapFrame, CLOCK_HZ, IRQ_BASE, SYSCALL_VECTOR};

pub const PAGE_BYTES: usize = 4096;

/// Where the boot loader leaves the BIOS memory map.
pub const E820_MAP: u32 = 0x8000;
/// The pretend kernel image: its start, the end of its code, the end of its constants.
pub const KERNEL_IMAGE: (u32, u32, u32) = (0x10_0000, 0x10_4000, 0x10_6000);
/// Room for the kernel's own page tables.
pub const KERNEL_MAP: u32 = 0x11_0000;
/// The boot archive follows; it is shorter than one page.
pub const ARCHIVE: u32 = KERNEL_MAP + crate::mem::KERNEL_MAP_BYTES as u32;
/// The first frame that the kernel hands out.
pub const KERNEL_END: u32 = ARCHIVE + PAGE_BYTES as u32;
/// The memory of the one device whose memory tasks can map: the text screen. Whoever holds
/// its capability may write there, so it is the one part of low memory that may change.
pub const SCREEN: std::ops::Range<u32> = 0xB_8000..0xC_0000;
/// Interrupt lines that the real `arch::init` leaves open: the timer and the cascade.
const OPEN_AT_BOOT: u16 = 0xFFFA;

/// The registers of the machine that the kernel can see or change.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Cpu {
    /// The timer's 16-bit counter. It counts down and wraps, like the real one.
    pub counter: u16,
    /// Timer counts since power-on: the model's own idea of the time.
    pub counts: u64,
    pub cr2: u32,
    pub cr3: u32,
    pub paging: bool,
    /// One bit per interrupt line; a set bit means the line is masked.
    pub masked: u16,
    /// Where the processor would push the next trap frame.
    pub trap_stack: usize,
    pub eois: u32,
    pub serial_bytes: u32,
}

pub struct Machine {
    live: *mut u8,
    base: *mut u8,
    frames: usize,
    /// Per frame: bit 0 = may differ from `base`, bit 1 = may differ from the parent.
    mark: Vec<u8>,
    touched: [Vec<u32>; 2],
    /// The parent snapshot: the frames that differed from `base` when it was taken.
    parent_slot: Vec<u32>,
    parent_frames: Vec<u32>,
    parent_pages: Vec<u8>,
    parent_taken: bool,
    pub cpu: Cpu,
    base_cpu: Cpu,
    parent_cpu: Cpu,
    pub nx: bool,
    restores: u64,
}

struct Slot(UnsafeCell<Option<Machine>>);

// SAFETY: the model runs on one thread, and `with` and `power_on` below are the only code
// that looks inside. Neither is ever entered while the other, or itself, is running.
unsafe impl Sync for Slot {}

static MACHINE: Slot = Slot(UnsafeCell::new(None));

/// Run `f` on the machine.
pub fn with<R>(f: impl FnOnce(&mut Machine) -> R) -> R {
    // SAFETY: one thread, and no closure passed here calls `with` or `power_on` again
    // (they only touch the machine they are given), so this is the only reference.
    let machine = unsafe { &mut *MACHINE.0.get() };
    f(machine.as_mut().expect("the machine is powered on"))
}

/// Replace the machine by a new one with `frames` frames of zeroed RAM.
pub fn power_on(frames: usize, nx: bool) {
    // SAFETY: one thread, and no reference into the old machine is alive: `with` hands
    // none out, and the kernel keeps no pointer between two calls of `phys`.
    unsafe { *MACHINE.0.get() = Some(Machine::new(frames, nx)) };
}

impl Machine {
    fn layout(frames: usize) -> Layout {
        Layout::from_size_align(frames * PAGE_BYTES, PAGE_BYTES).expect("a sane amount of RAM")
    }

    fn new(frames: usize, nx: bool) -> Machine {
        assert!(frames > KERNEL_END as usize / PAGE_BYTES, "RAM must reach past the kernel");
        // SAFETY: the layout has a non-zero size.
        let (live, base) = unsafe { (alloc_zeroed(Self::layout(frames)), alloc_zeroed(Self::layout(frames))) };
        assert!(!live.is_null() && !base.is_null(), "the host has no memory for the fake RAM");
        let cpu = Cpu {
            counter: 0xFFFF,
            counts: 0,
            cr2: 0,
            cr3: 0,
            paging: false,
            masked: OPEN_AT_BOOT,
            trap_stack: 0,
            eois: 0,
            serial_bytes: 0,
        };
        Machine {
            live,
            base,
            frames,
            mark: vec![0; frames],
            touched: [Vec::new(), Vec::new()],
            parent_slot: vec![0; frames],
            parent_frames: Vec::new(),
            parent_pages: Vec::new(),
            parent_taken: false,
            cpu,
            base_cpu: cpu,
            parent_cpu: cpu,
            nx,
            restores: 0,
        }
    }

    /// The host address of physical address `address`, noting that its frame may change.
    fn touch(&mut self, address: u32) -> *mut u8 {
        let frame = address as usize / PAGE_BYTES;
        assert!(frame < self.frames, "the kernel used physical address {address:#x}, which is not RAM");
        if self.mark[frame] != 3 {
            if self.mark[frame] & 1 == 0 {
                self.touched[0].push(frame as u32);
            }
            if self.mark[frame] & 2 == 0 {
                self.touched[1].push(frame as u32);
            }
            self.mark[frame] = 3;
        }
        // SAFETY: `frame < self.frames`, so the address lies inside the `live` allocation.
        unsafe { self.live.add(address as usize) }
    }

    /// Copy `bytes` into RAM at `address` (the boot loader's job on the real machine).
    pub fn load(&mut self, address: u32, bytes: &[u8]) {
        assert!(address as usize + bytes.len() <= self.frames * PAGE_BYTES, "the boot data fits in RAM");
        for offset in (0..bytes.len()).step_by(PAGE_BYTES) {
            self.touch(address + offset as u32);
        }
        // SAFETY: the range was just checked to lie inside `live`; `bytes` is host memory
        // outside it.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), self.live.add(address as usize), bytes.len()) };
    }

    /// Let `counts` timer counts pass.
    pub fn advance(&mut self, counts: u32) {
        assert!(counts < 0x1_0000, "the kernel must see the counter at least once per wrap");
        self.cpu.counter = self.cpu.counter.wrapping_sub(counts as u16);
        self.cpu.counts += counts as u64;
    }

    /// Microseconds since power-on, by the same formula as the kernel's clock.
    pub fn micros(&self) -> u64 {
        let counts = self.cpu.counts;
        counts / CLOCK_HZ * 1_000_000 + counts % CLOCK_HZ * 1_000_000 / CLOCK_HZ
    }

    fn forget_marks(&mut self) {
        for level in 0..2 {
            for &frame in &self.touched[level] {
                self.mark[frame as usize] = 0;
            }
            self.touched[level].clear();
        }
    }

    fn forget_parent(&mut self) {
        for &frame in &self.parent_frames {
            self.parent_slot[frame as usize] = 0;
        }
        self.parent_frames.clear();
        self.parent_pages.clear();
        self.parent_taken = false;
    }

    /// Remember everything: all of RAM and the registers.
    pub fn save_base(&mut self) {
        // SAFETY: both allocations are `frames` frames long and distinct.
        unsafe { ptr::copy_nonoverlapping(self.live, self.base, self.frames * PAGE_BYTES) };
        self.forget_marks();
        self.forget_parent();
        self.base_cpu = self.cpu;
    }

    /// Go back to `save_base`: only the frames touched since need copying.
    pub fn restore_base(&mut self) {
        for index in 0..self.touched[0].len() {
            let start = self.touched[0][index] as usize * PAGE_BYTES;
            // SAFETY: a touched frame lies inside both allocations (`touch` checked it).
            unsafe { ptr::copy_nonoverlapping(self.base.add(start), self.live.add(start), PAGE_BYTES) };
        }
        self.forget_marks();
        self.forget_parent();
        self.cpu = self.base_cpu;
        self.restores += 1;
    }

    /// Remember the present state as "parent": a copy of the frames that differ from base.
    pub fn save_parent(&mut self) {
        self.forget_parent();
        for index in 0..self.touched[0].len() {
            let frame = self.touched[0][index];
            self.parent_frames.push(frame);
            self.parent_slot[frame as usize] = self.parent_frames.len() as u32;
            let start = frame as usize * PAGE_BYTES;
            // SAFETY: a touched frame lies inside `live`; the slice ends before the next use
            // of `live`.
            let page = unsafe { std::slice::from_raw_parts(self.live.add(start), PAGE_BYTES) };
            self.parent_pages.extend_from_slice(page);
        }
        for &frame in &self.touched[1] {
            self.mark[frame as usize] &= !2;
        }
        self.touched[1].clear();
        self.parent_cpu = self.cpu;
        self.parent_taken = true;
    }

    /// Go back to `save_parent`.
    pub fn restore_parent(&mut self) {
        assert!(self.parent_taken, "no parent snapshot was taken");
        for index in 0..self.touched[1].len() {
            let frame = self.touched[1][index] as usize;
            let start = frame * PAGE_BYTES;
            let source = match self.parent_slot[frame] {
                // Not in the parent snapshot: the frame still had its base contents then.
                // SAFETY (both arms): the offsets lie inside the respective allocations.
                0 => unsafe { self.base.add(start) as *const u8 },
                slot => unsafe { self.parent_pages.as_ptr().add((slot as usize - 1) * PAGE_BYTES) },
            };
            // SAFETY: `source` covers one page of `base` or of the parent store; the target
            // frame lies inside `live`; the three are distinct allocations.
            unsafe { ptr::copy_nonoverlapping(source, self.live.add(start), PAGE_BYTES) };
            self.mark[frame] &= !2;
        }
        self.touched[1].clear();
        self.cpu = self.parent_cpu;
    }

    fn ram(&self, base: bool) -> &[u8] {
        let start = if base { self.base } else { self.live };
        // SAFETY: both allocations are `frames` frames long and initialized (zeroed at birth).
        unsafe { std::slice::from_raw_parts(start, self.frames * PAGE_BYTES) }
    }

    /// The address of the first changed byte below `limit` since `save_base`, if any. The
    /// kernel image, its page tables, the archive and all memory below them must never
    /// change after boot - except the screen memory, which is a device.
    pub fn damage_below(&self, limit: u32) -> Option<u32> {
        let (live, base) = (self.ram(false), self.ram(true));
        let fixed = |start: &usize| *start < limit as usize && !SCREEN.contains(&(*start as u32));
        self.touched[0].iter().map(|&frame| frame as usize * PAGE_BYTES).filter(fixed).find_map(
            |start| (0..PAGE_BYTES).find(|&offset| live[start + offset] != base[start + offset]).map(|offset| (start + offset) as u32),
        )
    }

    /// Is all of RAM equal to the base snapshot? (A check of the bookkeeping above.)
    pub fn matches_base(&self) -> bool {
        self.ram(false) == self.ram(true) && self.cpu == self.base_cpu
    }

    /// A full copy of RAM, and a comparison with one: checks of the bookkeeping above.
    pub fn copy_ram(&self) -> Vec<u8> {
        self.ram(false).to_vec()
    }

    pub fn ram_equals(&self, copy: &[u8]) -> bool {
        self.ram(false) == copy
    }

    /// One byte of RAM, wherever it lies: for tests that plant a change in memory the
    /// kernel's own routines refuse to read.
    pub fn byte(&self, address: u32) -> u8 {
        self.ram(false)[address as usize]
    }

    pub fn restores(&self) -> u64 {
        self.restores
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        // SAFETY: both pointers came from `alloc_zeroed` with this layout and are freed once.
        unsafe {
            dealloc(self.live, Self::layout(self.frames));
            dealloc(self.base, Self::layout(self.frames));
        }
    }
}

// ---- What the kernel sees: the same functions as the real `arch` ---------------------------

/// A pointer to physical memory at `address`.
pub fn phys(address: u32) -> *mut u8 {
    with(|machine| machine.touch(address))
}

pub fn kernel_map() -> u32 {
    KERNEL_MAP
}

pub fn kernel_image() -> (u32, u32, u32) {
    KERNEL_IMAGE
}

/// (PAE paging, no-execute bit). The model's processor always has PAE.
pub fn cpu_features() -> (bool, bool) {
    with(|machine| (true, machine.nx))
}

pub fn enable_paging(root: u32, nx: bool) {
    with(|machine| {
        assert!(nx == machine.nx, "the kernel turned on a no-execute bit that the processor lacks");
        machine.cpu.cr3 = root;
        machine.cpu.paging = true;
    });
}

pub fn write_cr3(root: u32) {
    with(|machine| {
        assert!(machine.cpu.paging, "an address space was loaded before paging was on");
        machine.cpu.cr3 = root;
    });
}

pub fn read_cr2() -> u32 {
    with(|machine| machine.cpu.cr2)
}

pub fn set_trap_stack(frame: *const TrapFrame) {
    with(|machine| machine.cpu.trap_stack = frame as usize);
}

pub fn clock_count() -> u32 {
    with(|machine| machine.cpu.counter as u32)
}

/// The model's cycle counter runs at the rate of the timer: one count per timer count.
pub fn cycles() -> u64 {
    with(|machine| machine.cpu.counts)
}

pub fn irq_mask(irq: u32, masked: bool) {
    with(|machine| {
        assert!(irq < 16, "the kernel masked interrupt line {irq}, which does not exist");
        if masked {
            machine.cpu.masked |= 1 << irq;
        } else {
            machine.cpu.masked &= !(1 << irq);
        }
    });
}

pub fn irq_eoi(_irq: u32) {
    with(|machine| machine.cpu.eois += 1);
}

pub fn irq_spurious(_irq: u32) -> bool {
    false
}

pub fn port_in(_port: u16, _width: u32) -> u32 {
    0
}

pub fn port_out(_port: u16, _width: u32, _value: u32) {}

pub fn serial_put(_byte: u8) {
    with(|machine| machine.cpu.serial_bytes += 1);
}
