//! `rt`: the small runtime that every Inti user program links.
//!
//! It provides the entry point, the system call wrappers (`sys`), text output without
//! `core::fmt` (`io`), the message protocols of the system servers (`proto`) and the client
//! side of those protocols (`client`). A program is a `#![no_std] #![no_main]` crate with a
//! `#[no_mangle] pub fn main()`.
#![no_std]

pub use abi;

pub mod client;
pub mod io;
pub mod proto;
pub mod sys;

use core::arch::{asm, naked_asm};
use core::cell::Cell;

/// A global variable for a program with exactly one thread.
#[repr(transparent)]
pub struct Single<T>(Cell<T>);

// SAFETY: an Inti task has one thread of control and no signal handlers, so a `Single` is
// never touched by two flows of control at once.
unsafe impl<T> Sync for Single<T> {}

impl<T: Copy> Single<T> {
    pub const fn new(value: T) -> Single<T> {
        Single(Cell::new(value))
    }

    pub fn get(&self) -> T {
        self.0.get()
    }

    pub fn set(&self, value: T) {
        self.0.set(value)
    }
}

static START_ARGS: Single<[u32; 4]> = Single::new([0; 4]);
static PANICKING: Single<bool> = Single::new(false);

extern "Rust" {
    fn main();
}

/// Entry point: the kernel starts every task here with its four start arguments in EAX,
/// EBX, ECX and EDX and the stack pointer at the top of a fresh stack.
///
/// # Safety
/// Only the kernel may jump here, once, at task start.
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn _start() -> ! {
    naked_asm!(
        "mov [{args}], eax",
        "mov [{args} + 4], ebx",
        "mov [{args} + 8], ecx",
        "mov [{args} + 12], edx",
        "call {run}",
        "ud2",
        args = sym START_ARGS,
        run = sym run,
    )
}

extern "C" fn run() -> ! {
    // SAFETY: every program that links `rt` defines `#[no_mangle] pub fn main()`; the link
    // fails otherwise.
    unsafe { main() };
    sys::exit(0)
}

/// The four numbers the parent passed in `SpawnArgs::args`.
pub fn args() -> [u32; 4] {
    START_ARGS.get()
}

/// End this task with a real CPU exception (used by the fault-injection commands).
pub fn crash() -> ! {
    // SAFETY: address 16 is never mapped in a user task (user memory starts at 1 GiB), so
    // this store cannot modify anything: the CPU raises a page fault and the kernel ends
    // the task.
    unsafe { asm!("mov dword ptr [16], 0", options(nostack)) };
    sys::exit(0xBAD)
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    if !PANICKING.get() {
        PANICKING.set(true);
        match info.location() {
            Some(location) => outln!("panic at ", location.file(), ":", location.line()),
            None => outln!("panic"),
        }
    }
    sys::exit(0xDEAD)
}
