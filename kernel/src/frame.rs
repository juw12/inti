//! The saved registers of a user task and the numbering of traps.
//!
//! This is all that the portable modules (`task`, `ipc`, `sys`, `mem`, `elf`, `check`) need to
//! know about how the processor enters the kernel. It is kept apart from `arch.rs` so that the
//! host model in `model/` compiles this very file next to its own stand-in for the hardware,
//! and not a copy that could drift.

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

/// The size of a saved frame: the kernel stack of a task is exactly this large.
pub const FRAME_SIZE: u32 = core::mem::size_of::<TrapFrame>() as u32;
const USER_CS: u32 = 0x1B;
const USER_DS: u32 = 0x23;
/// The interrupt vector of a system call.
pub const SYSCALL_VECTOR: u32 = 0x80;
/// The interrupt vector of hardware interrupt line 0; the other lines follow.
pub const IRQ_BASE: u32 = 32;
/// The frequency of the crystal that drives all three timer channels.
pub const CLOCK_HZ: u64 = 1_193_182;
