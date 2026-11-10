//! 32-bit x86: I/O ports, descriptor tables, interrupt entry and exit, interrupt controller, timer.
//!
//! Everything that needs an instruction Rust cannot express lives in this file.

use crate::Global;
use core::arch::{asm, global_asm};

// ---------------------------------------------------------------- I/O ports

/// Read `width` (1, 2 or 4) bytes from an I/O port.
pub fn port_in(port: u16, width: u32) -> u32 {
    let value: u32;
    // SAFETY: a port read touches no memory. Callers pass ports of devices the kernel drives
    // itself (PIC, PIT, COM1) or ports for which the calling task holds a capability.
    unsafe {
        match width {
            1 => asm!("xor eax, eax", "in al, dx", in("dx") port, out("eax") value, options(nomem, nostack)),
            2 => asm!("xor eax, eax", "in ax, dx", in("dx") port, out("eax") value, options(nomem, nostack)),
            _ => asm!("in eax, dx", in("dx") port, out("eax") value, options(nomem, nostack)),
        }
    }
    value
}

/// Write `width` (1, 2 or 4) bytes to an I/O port.
pub fn port_out(port: u16, width: u32, value: u32) {
    // SAFETY: as for `port_in`; a port write touches no memory the kernel relies on.
    unsafe {
        match width {
            1 => asm!("out dx, al", in("dx") port, in("eax") value, options(nomem, nostack)),
            2 => asm!("out dx, ax", in("dx") port, in("eax") value, options(nomem, nostack)),
            _ => asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack)),
        }
    }
}

fn outb(port: u16, value: u8) {
    port_out(port, 1, value as u32)
}

fn inb(port: u16) -> u8 {
    port_in(port, 1) as u8
}

// ---------------------------------------------------------------- serial log (COM1)

const COM1: u16 = 0x3F8;

pub fn serial_init() {
    outb(COM1 + 1, 0x00); // no interrupts (the console driver enables them later)
    outb(COM1 + 3, 0x80); // divisor latch
    outb(COM1, 0x01); // 115200 baud
    outb(COM1 + 1, 0x00);
    outb(COM1 + 3, 0x03); // 8N1
    outb(COM1 + 2, 0xC7); // FIFO on
    outb(COM1 + 4, 0x0B); // DTR, RTS, OUT2
}

pub fn serial_put(byte: u8) {
    let mut spins = 0;
    while inb(COM1 + 5) & 0x20 == 0 && spins < 100_000 {
        spins += 1;
    }
    outb(COM1, byte);
}

/// Last words on the screen, for people without a serial cable.
pub fn panic_banner() {
    const TEXT: &[u8] = b" INTI KERNEL PANIC - details on the serial port ";
    for (i, &ch) in TEXT.iter().enumerate() {
        // SAFETY: 0xB8000 is VGA text memory, identity-mapped and owned by no Rust object;
        // `i` stays inside the first text row. The kernel is halting.
        unsafe { ((0xB8000 + i * 2) as *mut u16).write_volatile(0x4F00 | ch as u16) }
    }
}

// ---------------------------------------------------------------- CPU state

/// The registers of an interrupted user task, in the order the entry code pushes them.
/// It is the first field of every `Tcb`: the CPU saves user state straight into the task.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TrapFrame {
    pub edi: u32,
    pub esi: u32,
    pub ebp: u32,
    pub esp_dummy: u32,
    pub ebx: u32,
    pub edx: u32,
    pub ecx: u32,
    pub eax: u32,
    pub vector: u32,
    pub error: u32,
    pub eip: u32,
    pub cs: u32,
    pub eflags: u32,
    pub esp: u32,
    pub ss: u32,
}

impl TrapFrame {
    pub const ZERO: TrapFrame = TrapFrame {
        edi: 0,
        esi: 0,
        ebp: 0,
        esp_dummy: 0,
        ebx: 0,
        edx: 0,
        ecx: 0,
        eax: 0,
        vector: 0,
        error: 0,
        eip: 0,
        cs: 0,
        eflags: 0,
        esp: 0,
        ss: 0,
    };

    /// A fresh user context: ring 3, interrupts on, nothing else.
    pub fn user(entry: u32, stack: u32, args: [u32; 4]) -> TrapFrame {
        TrapFrame {
            eax: args[0],
            ebx: args[1],
            ecx: args[2],
            edx: args[3],
            eip: entry,
            cs: USER_CS,
            eflags: 0x202,
            esp: stack,
            ss: USER_DS,
            ..TrapFrame::ZERO
        }
    }
}

pub const FRAME_SIZE: u32 = core::mem::size_of::<TrapFrame>() as u32;
const USER_CS: u32 = 0x1B;
const USER_DS: u32 = 0x23;
const VECTORS: usize = 49; // exceptions 0-31, IRQs 32-47, system call 0x80
pub const SYSCALL_VECTOR: u32 = 0x80;
pub const IRQ_BASE: u32 = 32;

static GDT: Global<[u64; 6]> = Global::new([
    0,
    0x00CF_9A00_0000_FFFF, // 0x08 kernel code
    0x00CF_9200_0000_FFFF, // 0x10 kernel data
    0x00CF_FA00_0000_FFFF, // 0x18 user code
    0x00CF_F200_0000_FFFF, // 0x20 user data
    0,                     // 0x28 task state segment, filled in by `init`
]);
static TSS: Global<[u32; 26]> = Global::new([0; 26]);
static IDT: Global<[u64; 129]> = Global::new([0; 129]);

#[repr(C, packed)]
struct TablePtr {
    limit: u16,
    base: u32,
}

extern "C" {
    static VECTOR_STUBS: [u32; VECTORS];
    /// Load a user context and continue in ring 3. Defined in the assembly below.
    pub fn enter_user(frame: *const TrapFrame) -> !;
}

/// Set up segments, the interrupt table, the interrupt controller and the 100 Hz timer.
pub fn init() {
    // SAFETY: runs once on the boot CPU with interrupts off, before anything else uses these
    // tables, so the exclusive references are unique. The tables are statics and stay valid
    // for as long as the CPU refers to them.
    unsafe {
        let tss = &mut *TSS.get();
        tss[2] = 0x10; // ss0: kernel stack segment
        tss[25] = 104 << 16; // no I/O permission bitmap: user port access always faults
        let tss_base = tss.as_ptr() as u64;
        let gdt = &mut *GDT.get();
        gdt[5] = 103 | ((tss_base & 0xFF_FFFF) << 16) | (0x89 << 40) | ((tss_base >> 24) << 56);

        let idt = &mut *IDT.get();
        for (i, &stub) in VECTOR_STUBS.iter().enumerate() {
            let vector = if i == VECTORS - 1 { SYSCALL_VECTOR as usize } else { i };
            // Interrupt gates: the kernel always runs with interrupts off.
            // Only the system call gate may be invoked from ring 3.
            let attr: u64 = if vector == SYSCALL_VECTOR as usize { 0xEE } else { 0x8E };
            let stub = stub as u64;
            idt[vector] = (stub & 0xFFFF) | (0x08 << 16) | (attr << 40) | ((stub >> 16) << 48);
        }

        let gdt_ptr = TablePtr { limit: 6 * 8 - 1, base: gdt.as_ptr() as u32 };
        let idt_ptr = TablePtr { limit: 129 * 8 - 1, base: idt.as_ptr() as u32 };
        asm!(
            "lgdt ({gdt})",
            "lidt ({idt})",
            "pushl $0x08",
            "pushl $2f",
            "lret",
            "2:",
            "movw $0x10, %ax",
            "movw %ax, %ds",
            "movw %ax, %es",
            "movw %ax, %ss",
            "movw %ax, %fs",
            "movw %ax, %gs",
            "movw $0x28, %ax",
            "ltr %ax",
            "pushl $0", // clear IOPL, NT and every other flag
            "popfl",
            gdt = in(reg) &gdt_ptr,
            idt = in(reg) &idt_ptr,
            out("eax") _,
            options(att_syntax),
        );
    }

    // Interrupt controller: IRQ 0-15 become vectors 32-47; only the timer is unmasked.
    outb(0x20, 0x11);
    outb(0xA0, 0x11);
    outb(0x21, IRQ_BASE as u8);
    outb(0xA1, IRQ_BASE as u8 + 8);
    outb(0x21, 4);
    outb(0xA1, 2);
    outb(0x21, 1);
    outb(0xA1, 1);
    outb(0x21, 0xFA);
    outb(0xA1, 0xFF);

    // Timer channel 0, rate generator, 100 Hz.
    outb(0x43, 0x34);
    outb(0x40, (PIT_RELOAD & 0xFF) as u8);
    outb(0x40, (PIT_RELOAD >> 8) as u8);

    // Timer channel 2 is the clock: gate on, speaker off, free-running over all 65536 counts.
    outb(0x61, (inb(0x61) & 0xFC) | 1);
    outb(0x43, 0xB4);
    outb(0x42, 0);
    outb(0x42, 0);
}

/// The frequency of the crystal that drives all three timer channels.
pub const CLOCK_HZ: u64 = 1_193_182;
const PIT_RELOAD: u32 = (CLOCK_HZ / 100) as u32;

/// Tell the CPU where to save user state on the next interrupt: the end of a task's frame.
pub fn set_trap_stack(top: u32) {
    // SAFETY: a plain store into the static TSS; interrupts are off, nothing else holds a
    // reference to it, and the CPU only reads it on the next ring 3 -> ring 0 transition.
    unsafe { (*TSS.get())[1] = top }
}

pub fn irq_mask(irq: u32, masked: bool) {
    let port = if irq < 8 { 0x21 } else { 0xA1 };
    let bit = 1u8 << (irq & 7);
    let old = inb(port);
    outb(port, if masked { old | bit } else { old & !bit });
}

pub fn irq_eoi(irq: u32) {
    if irq >= 8 {
        outb(0xA0, 0x20);
    }
    outb(0x20, 0x20);
}

/// True if IRQ 7 or 15 was raised by electrical noise rather than a device.
pub fn irq_spurious(irq: u32) -> bool {
    match irq {
        7 => {
            outb(0x20, 0x0B);
            inb(0x20) & 0x80 == 0
        }
        15 => {
            outb(0xA0, 0x0B);
            let spurious = inb(0xA0) & 0x80 == 0;
            if spurious {
                outb(0x20, 0x20);
            }
            spurious
        }
        _ => false,
    }
}

/// The clock counter: it counts down at `CLOCK_HZ` and wraps every 54.9 ms.
///
/// The kernel keeps time by adding up the differences between readings (see `Kernel::now`).
/// It does not count timer interrupts, which an emulator or a busy machine can drop, and it
/// does not trust the processor's cycle counter, whose rate has to be guessed.
pub fn clock_count() -> u32 {
    outb(0x43, 0x80); // latch channel 2
    let low = inb(0x42) as u32;
    let high = inb(0x42) as u32;
    (high << 8) | low
}

pub fn read_cr2() -> u32 {
    let value: u32;
    // SAFETY: reading CR2 (the last page-fault address) has no side effects.
    unsafe { asm!("mov {}, cr2", out(reg) value, options(nomem, nostack)) }
    value
}

/// Switch address space.
pub fn write_cr3(directory: u32) {
    // SAFETY: `directory` is a page directory built by `mem.rs`; all of them share the same
    // kernel mappings, so the running kernel code and data stay mapped across the switch.
    unsafe { asm!("mov cr3, {}", in(reg) directory, options(nostack)) }
}

/// Turn on paging, write protection in ring 0, and trapping of every FPU instruction.
pub fn enable_paging(directory: u32) {
    write_cr3(directory);
    // SAFETY: the directory identity-maps the kernel, so the next instruction is still mapped.
    unsafe {
        asm!(
            "mov {tmp}, cr0",
            "or {tmp}, 0x80010005", // PG | WP | EM | PE
            "and {tmp}, 0xFFFFFFF7", // clear TS
            "mov cr0, {tmp}",
            tmp = out(reg) _,
            options(nostack),
        )
    }
}

/// Sleep until the next interrupt. The interrupt handler runs before this returns.
pub fn idle_wait() {
    // SAFETY: STI takes effect after HLT starts, so no wakeup is lost; the handler runs on
    // the kernel stack through the `from_kernel` path below and interrupts are off again
    // when this returns. No kernel reference is live across this call (see `trap_handler`).
    unsafe { asm!("sti", "hlt", "cli", options(nomem, nostack)) }
}

pub fn halt_forever() -> ! {
    loop {
        // SAFETY: stops the CPU with interrupts off; nothing runs afterwards.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) }
    }
}

// Interrupt entry and exit.
//
// From ring 3 the CPU switches to the stack in the TSS, which points at the end of the
// current task's `TrapFrame`, so SS, ESP, EFLAGS, CS and EIP land directly in the task.
// The stub adds an error code and the vector, PUSHA adds the rest, and the frame is complete.
// The kernel then runs on its own single stack and returns the frame to resume.
global_asm!(
    r#"
    .section .text
    .macro VEC n
    .balign 8
vec\n:
    pushl $0
    pushl $\n
    jmp trap_common
    .endm
    .macro VEC_ERR n
    .balign 8
vec\n:
    pushl $\n
    jmp trap_common
    .endm

    .irp n, 0,1,2,3,4,5,6,7,9,15,16,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    VEC \n
    .endr
    .irp n, 8,10,11,12,13,14,17
    VEC_ERR \n
    .endr
    .irp n, 32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,128
    VEC \n
    .endr

trap_common:
    testb $3, 12(%esp)
    jz from_kernel
    pushal
    cld
    movl $0x10, %eax
    movw %ax, %ds
    movw %ax, %es
    movl %esp, %eax
    movl $__stack_top, %esp
    pushl %eax
    call trap_handler
resume_user:
    movl %eax, %esp
    movl $0x23, %eax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %fs
    movw %ax, %gs
    popal
    addl $8, %esp
    iretl

from_kernel:
    pushal
    cld
    pushl %esp
    call kernel_trap
    addl $4, %esp
    popal
    addl $8, %esp
    iretl

    .global enter_user
enter_user:
    movl 4(%esp), %eax
    jmp resume_user

    .section .rodata
    .balign 4
    .global VECTOR_STUBS
VECTOR_STUBS:
    .irp n, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,128
    .long vec\n
    .endr
"#,
    options(att_syntax)
);
