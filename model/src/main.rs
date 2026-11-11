//! Inti's kernel, checked on a fake machine.
//!
//! This host program compiles seven of the kernel's eight modules exactly as they are
//! (`task`, `ipc`, `sys`, `mem`, `elf`, `check`, `frame`) and puts a simulated PC
//! (`model/src/arch.rs`) where the eighth, `arch`, would be. On top of that it does two
//! things that a test inside QEMU cannot do:
//!
//! * `tables` - directed tests that push the kernel into its corners: full task and
//!   endpoint tables, exhausted memory, forged kernel state. Among them is a test of the
//!   invariant checker itself: it must report every corruption that is planted.
//! * `explore` - starting from a small configuration, try *every* enabled system call,
//!   fault, interrupt and timeout, then every one after each of those, and so on up to a
//!   depth bound, and check a list of properties after every single step.
//!
//! What this is not: a proof. It visits the states that short runs of small configurations
//! can reach, on a simulated machine. `arch.rs`, the assembly and the boot loader are not
//! part of it, `usize` is 64 bits here and 32 on the real target, a timeout "expires" when
//! the explorer says so, and two states count as the same when their summaries
//! (`state.rs`) have the same 128-bit hash. The README lists these limits.
//!
//! Usage: `model tables | explore <scenario> [depth] | all | list`
#![allow(dead_code)]

/// The kernel prints its boot messages through this. The model has nowhere to show them,
/// but still type-checks the arguments.
macro_rules! kprintln {
    ($($arg:tt)*) => {{
        if false {
            let _ = format_args!($($arg)*);
        }
    }};
}

mod arch;
#[path = "../../kernel/src/check.rs"]
mod check;
#[path = "../../kernel/src/elf.rs"]
mod elf;
#[path = "../../kernel/src/frame.rs"]
mod frame;
#[path = "../../kernel/src/ipc.rs"]
mod ipc;
#[path = "../../kernel/src/mem.rs"]
mod mem;
#[path = "../../kernel/src/sys.rs"]
mod sys;
#[path = "../../kernel/src/task.rs"]
mod task;

mod explore;
mod state;
mod tables;
mod world;

use std::sync::Mutex;

// The kernel's modules expect these three helpers at the crate root. They are copied from
// `kernel/src/main.rs`, which cannot be compiled here (it is the real entry code).

/// The little-endian 16-bit value at `bytes[at..]`.
pub fn get16(bytes: &[u8], at: usize) -> u32 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]]) as u32
}

/// The little-endian 32-bit value at `bytes[at..]`.
pub fn get32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Store `value` at `bytes[at..]`, little-endian.
pub fn put32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// What the model was doing, for the report if the kernel panics (an assertion or an
/// arithmetic overflow in kernel code is a finding, and its path must not be lost).
static TRAIL: Mutex<String> = Mutex::new(String::new());

pub fn trail(text: String) {
    if let Ok(mut trail) = TRAIL.lock() {
        *trail = text;
    }
}

fn usage() -> ! {
    eprintln!("usage: model tables | explore <scenario> [depth] | all | list");
    std::process::exit(2);
}

fn main() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Ok(trail) = TRAIL.try_lock() {
            println!("MODEL PANIC while: {trail}");
        }
        println!("MODEL: FAIL");
        default_hook(info);
    }));
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let words: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let passed = match words.as_slice() {
        ["tables"] => tables::run(),
        ["explore", name] => explore::run(name, None),
        ["explore", name, depth] => explore::run(name, Some(depth.parse().unwrap_or_else(|_| usage()))),
        ["all"] => {
            let tables = tables::run();
            let explored = explore::run_all();
            tables && explored
        }
        ["list"] => {
            explore::list();
            return;
        }
        _ => usage(),
    };
    println!("MODEL: {}", if passed { "PASS" } else { "FAIL" });
    std::process::exit(if passed { 0 } else { 1 });
}
