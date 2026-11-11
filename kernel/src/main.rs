//! Inti: a capability microkernel for 32-bit x86 PCs.
//!
//! The kernel does five things: it hands out memory, switches between tasks, passes messages,
//! checks capabilities and forwards interrupts. Device drivers, the file server, the
//! supervisor and the shell are ordinary user programs.
//!
//! Module map: `arch` (CPU specifics), `frame` (the saved register layout), `mem` (frames
//! and page tables), `task` (tasks,
//! capabilities, scheduling, death), `ipc` (messages and interrupts), `sys` (system calls),
//! `elf` (program loading), `check` (invariant checker).
#![no_std]
#![no_main]

/// Print a line on the serial port (COM1).
macro_rules! kprintln {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let _ = writeln!($crate::Serial, $($arg)*);
    }};
}

mod arch;
mod check;
mod elf;
mod frame;
mod ipc;
mod mem;
mod sys;
mod task;

use arch::{TrapFrame, IRQ_BASE};
use core::arch::global_asm;
use core::cell::UnsafeCell;
use core::fmt;
use task::{Kernel, IDLE};

/// A kernel global. All kernel state lives in a handful of these; there is no heap.
pub struct Global<T>(UnsafeCell<T>);

impl<T> Global<T> {
    pub const fn new(value: T) -> Self {
        Global(UnsafeCell::new(value))
    }

    pub fn get(&self) -> *mut T {
        self.0.get()
    }
}

// SAFETY: there is one CPU and kernel code runs with interrupts off. The only place where an
// interrupt can arrive in kernel mode is `arch::idle_wait`, and no reference into a `Global`
// is live across it. A `Global` is therefore never accessed from two places at once.
unsafe impl<T> Sync for Global<T> {}

static KERNEL: Global<Kernel> = Global::new(Kernel::new());

/// The one way to reach the kernel state.
///
/// # Safety
/// No other reference obtained from this function may still be in use. The three callers
/// below each take it once per kernel entry and let it go before returning or idling.
unsafe fn kernel() -> &'static mut Kernel {
    // SAFETY: the caller guarantees exclusivity (see above); the static lives forever.
    unsafe { &mut *KERNEL.get() }
}

pub struct Serial;

impl fmt::Write for Serial {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            if byte == b'\n' {
                arch::serial_put(b'\r');
            }
            arch::serial_put(byte);
        }
        Ok(())
    }
}

pub fn get16(bytes: &[u8], at: usize) -> u32 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]]) as u32
}

pub fn get32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

pub fn put32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

// The first bytes of the kernel image. The boot sector jumps here in 32-bit protected mode
// with EAX = "INTI" and EBX = address of the BIOS memory map. `build.py` fills in where the
// boot archive sits behind the image; it is moved out of the way before `.bss` is cleared.
global_asm!(
    r#"
    .section .head, "ax", @progbits
    .global _start
_start:
    jmp 1f
    .balign 8
    .ascii "INTK"
archive_off:
    .long 0
archive_len:
    .long 0
1:
    cli
    cld
    cmpl $0x49544E49, %eax
    jne 3f
    movl archive_len, %ecx
    movl $__kernel_start, %esi
    addl archive_off, %esi
    addl %ecx, %esi
    decl %esi
    movl $__bss_end, %edi
    addl %ecx, %edi
    decl %edi
    std
    rep movsb
    cld
    movl $__bss_start, %edi
    movl $__bss_end, %ecx
    subl %edi, %ecx
    xorl %eax, %eax
    rep stosb
    movl $__stack_top, %esp
    pushl archive_len
    pushl $__bss_end
    pushl %ebx
    call kmain
3:
    hlt
    jmp 3b

    .section .bss.kstack, "aw", @nobits
    .balign 16
    .skip 16384
    .global __stack_top
__stack_top:
"#,
    options(att_syntax)
);

#[no_mangle]
extern "C" fn kmain(e820: u32, archive: u32, archive_len: u32) -> ! {
    arch::serial_init();
    kprintln!("\nInti {} kernel", env!("CARGO_PKG_VERSION"));
    arch::init();
    {
        // SAFETY: the boot CPU with interrupts off; no other kernel reference exists yet.
        let k = unsafe { kernel() };
        k.boot(e820, archive, archive_len);
    }
    let frame = resume();
    // SAFETY: `frame` is the saved context of a ready task whose address space is loaded.
    unsafe { arch::enter_user(frame) }
}

/// Every interrupt, exception and system call that arrives from user mode ends up here.
/// Returns the saved registers of the task to continue with.
#[no_mangle]
extern "C" fn trap_handler(frame: *mut TrapFrame) -> *const TrapFrame {
    {
        // SAFETY: entered from user mode with interrupts off. The reference taken by the
        // previous kernel entry ended when that entry returned, so this one is unique.
        let k = unsafe { kernel() };
        let current = k.cur != IDLE && core::ptr::eq(frame, &k.tasks[k.cur].frame);
        assert!(current, "trap frame does not belong to the running task");
        k.on_trap();
    }
    resume()
}

/// Pick the next task; wait for an interrupt while there is none.
fn resume() -> *const TrapFrame {
    loop {
        {
            // SAFETY: interrupts are off and no other kernel reference is live in this scope.
            let k = unsafe { kernel() };
            if let Some(frame) = k.schedule() {
                return frame;
            }
        }
        // Nothing to run. The reference above is gone, so `kernel_trap` may take its own.
        arch::idle_wait();
    }
}

/// An interrupt or exception arrived while the CPU was in kernel mode. A device interrupt
/// during the idle wait is normal; anything else is a kernel bug and stops the machine.
#[no_mangle]
extern "C" fn kernel_trap(frame: *const TrapFrame) {
    // SAFETY: the entry code passes a pointer to the registers it just pushed on the kernel
    // stack; all fields up to `eflags` are valid, and only those three are read.
    let (vector, error, eip) = unsafe { ((*frame).vector, (*frame).error, (*frame).eip) };
    if (IRQ_BASE..IRQ_BASE + 16).contains(&vector) {
        // SAFETY: interrupts are enabled only inside `arch::idle_wait`, and `resume` holds
        // no kernel reference there, so this one is unique.
        let k = unsafe { kernel() };
        k.on_irq(vector - IRQ_BASE);
    } else {
        let address = arch::read_cr2();
        panic!("exception {vector} in kernel mode: eip={eip:#x} error={error:#x} address={address:#x}");
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    kprintln!("KERNEL PANIC: {}", info);
    arch::panic_banner();
    arch::halt_forever()
}
