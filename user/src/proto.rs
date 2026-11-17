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

// Disk server. It also answers `FS_LIST`, `FS_STAT` and `FS_READ` for the files on the disk;
// an `FS_READ` reply can be shorter than asked for, and is empty only at the end of the file.
/// Start a new version of the file named in `data`; reply: `words[0]` = transaction number.
/// Nothing changes on the disk for readers until `DISK_COMMIT`.
pub const DISK_CREATE: u32 = 40;
/// `words[0]` = transaction number; add `data[..len]` to the end of the new version.
pub const DISK_WRITE: u32 = 41;
/// `words[0]` = transaction number; replace the file by the new version in one step.
pub const DISK_COMMIT: u32 = 42;
/// Add bytes to the end of a file in one step. `words[0]` = length of the name, `words[1]` =
/// the size the sender believes the file has (0 if it does not exist yet); `data` holds the
/// name and then the bytes. If the size is different, nothing is written and the reply is
/// `ERR` with the real size in `words[0]` and `D_MOVED` in `words[1]`.
pub const DISK_APPEND: u32 = 43;
/// Remove the file named in `data`.
pub const DISK_DELETE: u32 = 44;
/// Reply: `words` = data blocks, free blocks, files, microseconds the start-up took.
pub const DISK_INFO: u32 = 45;

/// Size of a disk block in bytes.
pub const DISK_BLOCK: u32 = 4096;

// Why the disk server answered `ERR`, in `words[1]` of the reply.
pub const D_NO_DRIVE: u32 = 1;
pub const D_NO_VOLUME: u32 = 2;
pub const D_DAMAGED: u32 = 3;
pub const D_NO_FILE: u32 = 4;
pub const D_FULL: u32 = 5;
pub const D_DIR_FULL: u32 = 6;
pub const D_BAD_NAME: u32 = 7;
/// The transaction number is not the open one: another writer started, or the server was
/// restarted. The whole file has to be sent again.
pub const D_STALE: u32 = 8;
pub const D_MOVED: u32 = 9;
pub const D_IO: u32 = 10;
// Two more that the client library reports and the server never sends.
/// The capability does not allow the request.
pub const D_DENIED: u32 = 11;
/// The server did not answer.
pub const D_GONE: u32 = 12;

// Display server: the screen in graphics mode, and the windows on it.
/// From the console only: write `data[..len]` in the terminal window, after clearing it if
/// `words[0]` is 1. `words[1]` = 1 says that more text follows at once, so the display can
/// wait with drawing. Reply: `words[0]` = how often the display has been restarted.
pub const DISP_TEXT: u32 = 50;
/// From the console only: the mouse moved by `words[0]`, `words[1]` (signed; up is
/// positive) and its buttons are `words[2]` (bit 0 = left).
pub const DISP_MOUSE: u32 = 51;
/// Open a window whose inside is `words[0]` by `words[1]` pixels, with the title in `data`.
/// The reply carries a capability for the window. The window is closed when no task holds
/// that capability any more.
pub const WIN_OPEN: u32 = 52;
/// Reply: `words` = windows of programs, frames drawn, restarts of the display, mouse
/// position (x in the low 16 bits, y in the high); `data` holds `WINDOW_RECORD` bytes for
/// each window, the lowest first.
pub const DISP_INFO: u32 = 53;
// The next three are sent through a window capability and act on that window alone.
/// Draw `data[..len]` at `words[0]`, `words[1]` in colour `words[2]` (text in the low four
/// bits, background in the next four).
pub const WIN_TEXT: u32 = 54;
/// Fill the rectangle at `words[0]`, `words[1]`, `words[2]` wide (low 16 bits) and high
/// (high 16 bits), with colour `words[3]`.
pub const WIN_FILL: u32 = 55;
pub const WIN_CLOSE: u32 = 56;

/// Layout of one window record in the `DISP_INFO` reply: title[12], then x and y (i16) and
/// width and height (u16) of the whole window in pixels, then its number (u8).
pub const WINDOW_RECORD: usize = 24;
/// The badge of a window capability: this, the restart count of the display that made it
/// (16 bits) and the number of the window (8 bits).
pub const WINDOW_BADGE: u32 = 0x5700_0000;

/// The sixteen colours of the screen.
pub mod colour {
    pub const BLACK: u8 = 0;
    pub const NAVY: u8 = 1;
    pub const GREEN: u8 = 2;
    pub const TEAL: u8 = 3;
    pub const RED: u8 = 4;
    pub const PURPLE: u8 = 5;
    pub const AMBER: u8 = 6;
    pub const SILVER: u8 = 7;
    pub const GREY: u8 = 8;
    pub const BLUE: u8 = 9;
    pub const LIME: u8 = 10;
    pub const CYAN: u8 = 11;
    pub const CORAL: u8 = 12;
    pub const PINK: u8 = 13;
    pub const YELLOW: u8 = 14;
    pub const WHITE: u8 = 15;
}

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
/// The console driver, which sends the display what to show in the terminal window.
pub const B_CONSOLE: u32 = 4;

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
    pub const TOUCH_UNMAPPED: u32 = 18;
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
