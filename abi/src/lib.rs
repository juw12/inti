//! Inti ABI: everything the kernel and user programs have to agree on.
//!
//! The whole kernel interface is in this file: 22 system calls, 7 capability types,
//! one message layout. There is nothing else to learn.
#![no_std]

pub mod archive;
pub mod mem;

// ---------------------------------------------------------------- address space

pub const PAGE: u32 = 4096;
/// User programs live in [USER_BASE, USER_END). Everything below is kernel-only.
pub const USER_BASE: u32 = 0x4000_0000;
pub const USER_END: u32 = 0xC000_0000;
/// Initial stack pointer of a new task; the stack grows down from here.
pub const STACK_TOP: u32 = 0xBFFF_0000;

// ---------------------------------------------------------------- system calls

/// System call numbers (in EAX; arguments in EBX, ECX, EDX, EDI; result in EAX).
pub mod nr {
    pub const YIELD: u32 = 0; // let another task run
    pub const EXIT: u32 = 1; // end the calling task
    pub const CALL: u32 = 2; // send a message through an endpoint and wait for the reply, with a time limit
    pub const RECV: u32 = 3; // wait for the next message on an endpoint, with a time limit
    pub const REPLY: u32 = 4; // answer a received call, once, through its reply token
    pub const EP_CREATE: u32 = 5; // create an endpoint and receive a capability for it with all rights
    pub const CAP_DERIVE: u32 = 6; // copy a capability with the same rights and range, or fewer
    pub const CAP_DROP: u32 = 7; // give a capability up
    pub const MAP: u32 = 8; // map fresh, zeroed pages, charged to the caller's memory quota
    pub const UNMAP: u32 = 9; // unmap pages and return their frames
    pub const MAP_MEM: u32 = 10; // map pages of a Memory capability (device memory, the file archive)
    pub const IO_IN: u32 = 11; // read an I/O port through an IoPort capability
    pub const IO_OUT: u32 = 12; // write an I/O port through an IoPort capability
    pub const IRQ_BIND: u32 = 13; // have an interrupt line delivered as messages on an endpoint
    pub const IRQ_ACK: u32 = 14; // re-enable an interrupt line after handling it
    pub const SPAWN: u32 = 15; // start a task from an ELF image with part of the caller's quota and capabilities
    pub const TASK_KILL: u32 = 16; // end a task through its Task capability
    pub const SLEEP: u32 = 17; // pause the calling task for a given time
    pub const TIME: u32 = 18; // read the clock: microseconds since boot
    pub const LOG: u32 = 19; // write a line to the kernel log through a Log capability
    pub const SYSINFO: u32 = 20; // read kernel counters and tables, or run the invariant checker (Info capability)
    pub const CAP_REVOKE: u32 = 21; // take back every copy that was made of a capability, in every task
    pub const COUNT: u32 = 22;
}

pub const E_INVAL: i32 = -1; // bad argument
pub const E_NOCAP: i32 = -2; // empty slot or wrong capability type
pub const E_PERM: i32 = -3; // capability lacks the right
pub const E_FAULT: i32 = -4; // bad user pointer
pub const E_NOMEM: i32 = -5; // memory quota exhausted
pub const E_NOSLOT: i32 = -6; // a fixed kernel table is full, or the caller's quota of it is used up
pub const E_DEAD: i32 = -7; // peer or object is gone
pub const E_TIMEOUT: i32 = -8; // the time limit passed before the call could finish
pub const E_BUSY: i32 = -9; // already in use
pub const E_NOSYS: i32 = -10; // no such system call

/// Timeout value meaning "wait forever".
pub const FOREVER: u32 = 0xFFFF_FFFF;
/// Flag in the page count of `map_mem`: map the pages writable.
pub const MAP_WRITE: u32 = 0x8000_0000;

// ---------------------------------------------------------------- capabilities

pub const CAP_SLOTS: usize = 32;
/// "No capability" marker in `SpawnArgs` and `Msg`.
pub const NO_CAP: u32 = 0xFFFF_FFFF;

pub const CAP_NULL: u32 = 0;
pub const CAP_ENDPOINT: u32 = 1;
pub const CAP_IOPORT: u32 = 2;
pub const CAP_IRQ: u32 = 3;
pub const CAP_MEMORY: u32 = 4;
pub const CAP_TASK: u32 = 5;
pub const CAP_LOG: u32 = 6;
pub const CAP_INFO: u32 = 7;

/// Endpoint rights.
pub const R_SEND: u32 = 1;
pub const R_RECV: u32 = 2;
/// May stamp a badge onto a derived copy. A capability travels in a message only if the
/// sender's and the receiver's capability for the endpoint both carry this right.
pub const R_GRANT: u32 = 4;

// ---------------------------------------------------------------- IPC

pub const MSG_DATA: usize = 256;

/// The one message format, used for requests, replies and kernel notifications.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Msg {
    /// Request or reply code. The top bit is reserved for the kernel.
    pub label: u32,
    /// Set by the kernel from the sender's capability; a sender cannot forge it.
    pub badge: u32,
    /// Set by the kernel; pass it to `reply`.
    pub token: u32,
    /// Number of valid bytes in `data`.
    pub len: u32,
    pub words: [u32; 4],
    /// A capability to hand over with the message: a slot of the sender, or `NO_CAP`.
    /// The receiver finds here the slot of its own in which the copy arrived.
    pub cap: u32,
    pub data: [u8; MSG_DATA],
}

impl Msg {
    pub const fn new(label: u32) -> Msg {
        Msg { label, badge: 0, token: 0, len: 0, words: [0; 4], cap: NO_CAP, data: [0; MSG_DATA] }
    }
}

pub const MSG_SIZE: u32 = core::mem::size_of::<Msg>() as u32;
/// Size of the fixed part of a message (everything before `data`).
pub const MSG_HEADER: usize = MSG_SIZE as usize - MSG_DATA;

/// Labels with this bit are produced only by the kernel.
pub const L_KERNEL: u32 = 0x8000_0000;
/// An interrupt bound to this endpoint fired; `words[0]` is the pending IRQ mask.
pub const L_IRQ: u32 = 0x8000_0001;
/// A supervised task ended; `badge` is its fault badge, `words` = [cause, address, eip, code],
/// `data[0..4]` = system calls it made, `data[4..8]` = timer ticks it ran.
pub const L_FAULT: u32 = 0x8000_0002;

/// `words[0]` of `L_FAULT`: a CPU exception vector (0..31) or one of these.
pub const CAUSE_EXIT: u32 = 0x100;
pub const CAUSE_KILLED: u32 = 0x101;
pub const CAUSE_PARENT_DIED: u32 = 0x102;

// ---------------------------------------------------------------- spawn

pub const SPAWN_CAPS: usize = 16;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SpawnArgs {
    pub name: [u8; 12],
    /// Memory quota in pages, taken out of the caller's own quota and returned on death.
    pub quota: u32,
    pub stack_pages: u32,
    /// Caller's endpoint capability (RECV right) that receives `L_FAULT` when the child ends.
    pub fault_ep: u32,
    pub fault_badge: u32,
    /// How many tasks the child may start in turn and how many endpoints it may create.
    /// Both come out of the caller's own quotas, like the memory, and return on death.
    pub tasks: u32,
    pub endpoints: u32,
    /// Caller's capability slots copied into the child's slots 0..16 (`NO_CAP` = leave empty).
    /// A child starts with these and nothing else.
    pub caps: [u32; SPAWN_CAPS],
    /// Passed to the child in EAX, EBX, ECX, EDX.
    pub args: [u32; 4],
}

impl SpawnArgs {
    /// Arguments for a child called `name` that starts with no capabilities, no quota for
    /// tasks or endpoints, and nobody watching it. Fill in the rest as needed.
    pub fn new(name: &[u8], quota: u32, stack_pages: u32) -> SpawnArgs {
        let mut args = SpawnArgs {
            name: [0; 12],
            quota,
            stack_pages,
            fault_ep: NO_CAP,
            fault_badge: 0,
            tasks: 0,
            endpoints: 0,
            caps: [NO_CAP; SPAWN_CAPS],
            args: [0; 4],
        };
        let len = name.len().min(11);
        args.name[..len].copy_from_slice(&name[..len]);
        args
    }
}

pub const SPAWN_ARGS_SIZE: usize = core::mem::size_of::<SpawnArgs>();

// The kernel reads these two structures field by field at fixed offsets.
const _: () = assert!(MSG_SIZE == 292 && SPAWN_ARGS_SIZE == 116);
const _: () = assert!(core::mem::offset_of!(Msg, cap) == 32);

// ---------------------------------------------------------------- sysinfo

pub const INFO_STATS: u32 = 0;
pub const INFO_TASK: u32 = 1;
pub const INFO_CAP: u32 = 2;
/// Run the kernel's invariant checker; returns 0 or the number of the violated invariant.
pub const INFO_CHECK: u32 = 3;

pub const MAX_TASKS: usize = 32;
pub const MAX_ENDPOINTS: usize = 64;
pub const NO_TASK: u32 = 0xFF;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KStats {
    pub ticks: u32,
    pub total_frames: u32,
    pub free_frames: u32,
    pub tasks: u32,
    pub endpoints: u32,
    pub syscalls: u32,
    pub switches: u32,
    pub irqs: u32,
    pub faults: u32,
    pub spawns: u32,
    /// Internal "cannot happen" conditions seen by the kernel. Must stay 0.
    pub anomalies: u32,
    pub kernel_pages: u32,
}

impl KStats {
    pub const ZERO: KStats = KStats {
        ticks: 0,
        total_frames: 0,
        free_frames: 0,
        tasks: 0,
        endpoints: 0,
        syscalls: 0,
        switches: 0,
        irqs: 0,
        faults: 0,
        spawns: 0,
        anomalies: 0,
        kernel_pages: 0,
    };
}

pub const ST_FREE: u32 = 0;
pub const ST_READY: u32 = 1;
pub const ST_RECV: u32 = 2;
pub const ST_SEND: u32 = 3;
pub const ST_REPLY: u32 = 4;
pub const ST_SLEEP: u32 = 5;
pub const ST_ZOMBIE: u32 = 6;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TaskInfo {
    pub name: [u8; 12],
    pub state: u32,
    pub parent: u32,
    pub gen: u32,
    pub quota: u32,
    pub charged: u32,
    pub cpu_ticks: u32,
    pub syscalls: u32,
    pub caps: u32,
    /// Tasks it may still start and endpoints it may still create.
    pub tasks: u32,
    pub endpoints: u32,
}

/// Description of one capability. Endpoint: a = endpoint, b = rights, c = badge.
/// I/O ports: a = first, b = last. IRQ: a = mask. Memory: a = physical address, b = pages,
/// c = writable. Task: a = index, b = generation. `from` names the capability it was
/// copied from, as `task << 8 | slot`, or is `NO_CAP` for a capability that is not a copy.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CapInfo {
    pub kind: u32,
    pub a: u32,
    pub b: u32,
    pub c: u32,
    pub from: u32,
}

impl TaskInfo {
    pub const ZERO: TaskInfo = TaskInfo {
        name: [0; 12],
        state: ST_FREE,
        parent: NO_TASK,
        gen: 0,
        quota: 0,
        charged: 0,
        cpu_ticks: 0,
        syscalls: 0,
        caps: 0,
        tasks: 0,
        endpoints: 0,
    };
}

impl CapInfo {
    pub const ZERO: CapInfo = CapInfo { kind: CAP_NULL, a: 0, b: 0, c: 0, from: NO_CAP };
}

// ---------------------------------------------------------------- boot archive

/// The boot archive: magic, u32 count, then `count` entries, then file data.
pub const ARCHIVE_MAGIC: [u8; 8] = *b"INTIARCH";
pub const ARCHIVE_NAME: usize = 24;
/// Entry layout: name[24], u32 offset from archive start, u32 size.
pub const ARCHIVE_ENTRY: usize = 32;
pub const ARCHIVE_HEADER: usize = 12;
