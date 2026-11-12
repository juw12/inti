//! The message protocols of the system servers. A request is a `Msg` with one of these
//! labels; the reply carries one of the reply labels.

// Console server.
/// Print `data[..len]`.
pub const CON_WRITE: u32 = 1;
/// Print `data[..len]` as a prompt, then reply with the next line typed (without newline).
pub const CON_READLINE: u32 = 2;
/// Was Ctrl-C pressed since the last time this was asked? `words[0]` = 1 or 0.
pub const CON_INTR: u32 = 3;
/// Write `data[..len]` to the serial line only, not to the screen. Honoured only for the
/// supervisor, which reports through the console so that the serial line has one writer.
pub const CON_REPORT: u32 = 4;

// File server.
/// `words[0]` = file number; reply: name in `data`, size in `words[0]`.
pub const FS_LIST: u32 = 10;
/// Name in `data`; reply: size in `words[0]`.
pub const FS_STAT: u32 = 11;
/// Name in `data`, `words[0]` = offset, `words[1]` = byte count; reply: bytes in `data`.
pub const FS_READ: u32 = 12;

// Understood by every supervised server.
pub const PING: u32 = 20;
/// Fault injection: end with a real CPU exception. Honoured only for the shell's badge.
pub const DBG_CRASH: u32 = 21;
/// Fault injection: stop answering. Honoured only for the shell's badge.
pub const DBG_HANG: u32 = 22;

// Supervisor (init).
/// Reply: `words[0]` = number of services, then `SERVICE_RECORD` bytes per service in `data`.
pub const INIT_STATUS: u32 = 30;
pub const INIT_SHUTDOWN: u32 = 31;

/// Layout of one service record in the `INIT_STATUS` reply: name[8], then four u32 values:
/// restarts, last cause, last respawn time in microseconds, task state.
pub const SERVICE_RECORD: usize = 24;

// Reply labels.
pub const OK: u32 = 0;
pub const ERR: u32 = 1;
pub const DENIED: u32 = 2;
pub const BUSY: u32 = 3;

// Badges that init stamps on the capabilities it hands out; servers use them to tell
// the shell from the programs the shell starts.
/// `init` itself sends with the capability it got when it created the endpoint: no badge.
pub const B_SUPERVISOR: u32 = 0;
pub const B_SHELL: u32 = 1;
pub const B_APP: u32 = 2;
pub const B_CLIENT: u32 = 3;

/// What the test child `tchild` does, chosen by its first start argument.
pub mod mode {
    pub const EXIT: u32 = 1;
    pub const NULL_READ: u32 = 2;
    pub const WRITE_CODE: u32 = 3;
    pub const KERNEL_READ: u32 = 4;
    pub const KERNEL_WRITE: u32 = 5;
    pub const KERNEL_JUMP: u32 = 6;
    pub const DIVIDE: u32 = 7;
    pub const UD2: u32 = 8;
    pub const CLI: u32 = 9;
    pub const HLT: u32 = 10;
    pub const PORT_IN: u32 = 11;
    pub const FPU: u32 = 12;
    pub const STACK_OVERFLOW: u32 = 13;
    pub const PANIC: u32 = 14;
    pub const EXEC_STACK: u32 = 15;
    pub const EXEC_DATA: u32 = 16;
    pub const EXEC_MAPPED: u32 = 17;
    pub const SPIN: u32 = 20;
    pub const SLEEP: u32 = 21;
    pub const ECHO: u32 = 22;
    pub const CALL_ONCE: u32 = 23;
    pub const RECV_ONCE: u32 = 24;
    pub const GRANDCHILD: u32 = 25;
    pub const ENDPOINTS: u32 = 26;
    pub const AGENT: u32 = 27;
    pub const HOGS: u32 = 28;
    pub const FUZZ: u32 = 30;
}

/// Sent to a `tchild` in `ECHO` mode to make it answer one last time and exit.
pub const ECHO_STOP: u32 = 99;

// What a fuzz kid tells the fuzzer.
/// `words[0]` = system calls made since the last report; `data` holds, for each of the
/// `FUZZ_NUMBERS` system call numbers, how many were tried and how many succeeded (two u16).
pub const FUZZ_REPORT: u32 = 0x4655_0001;
/// The kernel's invariant checker reported a violation: `words[0]` = the rule.
pub const FUZZ_VIOLATION: u32 = 0x4655_0002;
/// A fuzz kid draws system call numbers below this; the last two do not exist.
pub const FUZZ_NUMBERS: usize = 24;
/// A fuzz kid reports after this many system calls.
pub const FUZZ_REPORT_EVERY: u32 = 512;
/// Upper bound on the system calls a fuzz kid makes to set itself up; these are not random
/// and are not counted.
pub const FUZZ_SETUP_CALLS: u32 = 8;

/// Number of hardware-level attacks `rogue` knows (start argument 1..=ROGUE_HW_MODES).
pub const ROGUE_HW_MODES: u32 = 21;
/// The first of the three hardware-level attacks that only the no-execute bit stops:
/// running code from the stack, from data and from a fresh mapping. On a processor
/// without that bit they succeed, and the shell says so.
pub const ROGUE_FIRST_NX: u32 = 19;
/// The hardware-level attack that only `rogue all` runs: an `iret` frame forged to name
/// ring 0. A processor (and QEMU) answers it with a general protection fault, but the v86
/// emulator the web demo runs in stops with an internal error instead, so plain `rogue`
/// prints this attack, skips it and says so.
pub const ROGUE_ONLY_IN_ALL: u32 = 16;
