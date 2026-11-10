//! rogue: a hostile program.
//!
//! The shell starts it the way it starts any program: with one capability, the right to
//! send text to the console. Mode 0 then tries 27 things through the system call interface
//! that such a program must not be able to do, and exits with `denied << 16 | attempts`.
//! Modes 1 to 18 each try one thing directly on the hardware; the CPU must stop the task,
//! and the shell reports how it ended.
#![no_std]
#![no_main]

use core::arch::asm;

use rt::abi::{nr, Msg, SpawnArgs, CAP_SLOTS, E_NOSYS, R_RECV, R_SEND};
use rt::io::{errname, set_output, Num, Pad, Sink};
use rt::proto::{self, ROGUE_HW_MODES};
use rt::{out, outln, sys};

/// The only capability this program holds.
const CONSOLE: u32 = 0;
/// Width of the description column; a whole line stays under 80 characters.
const WIDTH: usize = 43;
/// The first megabyte above 1 MiB holds the kernel.
const KERNEL: u32 = 0x10_0000;
/// Free user address space for the mapping attempts.
const SPARE: u32 = 0x6000_0000;
/// How many attempts mode 0 makes.
const SOFTWARE_ATTEMPTS: u32 = 27;

struct Tally {
    denied: u32,
    total: u32,
}

impl Tally {
    /// Records one attempt; `refusal` says why it failed, or is `None` if it worked.
    fn attempt(&mut self, what: &str, refusal: Option<&str>) {
        self.total += 1;
        out!(Num(self.total, 2), ". ", Pad(what.as_bytes(), WIDTH));
        match refusal {
            Some(reason) => {
                self.denied += 1;
                outln!("denied: ", reason);
            }
            None => outln!("NOT DENIED"),
        }
    }
}

/// Tries something with every capability slot in turn. It counts as denied only if the
/// kernel refuses all 32; the reason shown is the one for slot 0, the slot that is in use.
fn every_slot(mut attempt: impl FnMut(u32) -> Option<i32>) -> Option<&'static str> {
    let mut reason = "";
    for slot in 0..CAP_SLOTS as u32 {
        let code = attempt(slot)?;
        if slot == CONSOLE {
            reason = errname(code);
        }
    }
    Some(reason)
}

/// Denied only if every one of `codes` is an error.
fn all_refused(codes: &[i32]) -> Option<&'static str> {
    if codes.iter().all(|&code| code < 0) {
        Some(errname(codes[0]))
    } else {
        None
    }
}

fn refusal<T>(result: Result<T, i32>) -> Option<&'static str> {
    result.err().map(errname)
}

/// A system call with arguments that the safe wrappers cannot express.
fn raw(number: u32, a: u32, b: u32, c: u32) -> i32 {
    // SAFETY: every call made through here hands the kernel a pointer or a number it must
    // refuse. If the kernel wrongly accepted one, the damage would be the finding.
    unsafe { sys::raw(number, a, b, c, 0).0 }
}

/// Asks the console driver to crash or hang; drivers accept that only from the shell.
fn ask_console(label: u32) -> Option<&'static str> {
    let mut msg = Msg::new(label);
    match sys::call(CONSOLE, &mut msg, 2000) {
        Ok(()) if msg.label == proto::DENIED => Some("refused by the driver"),
        _ => None,
    }
}

/// Answers calls it never received: token 0, every task index, and some random tokens.
fn forged_replies() -> Option<&'static str> {
    let msg = Msg::new(0);
    let mut token = 0u32;
    let mut reason = "";
    for round in 0..48u32 {
        reason = errname(sys::reply(token, &msg).err()?);
        token = if round < 32 {
            0x8000_0000 | round
        } else {
            token.wrapping_mul(0x9E37_79B9).wrapping_add(round) | 0x8000_0000
        };
    }
    Some(reason)
}

/// Tries to give itself I/O privilege and to switch interrupts off by loading the flags
/// register. In user mode the CPU ignores both changes without raising an exception.
fn flags_attempt() -> Option<&'static str> {
    let flags: u32;
    // SAFETY: the sequence leaves the stack as it found it and restores the saved flags.
    // In between it asks for flags that user mode may not have; the CPU keeps the old
    // interrupt flag and I/O privilege level, which is what the test then observes.
    unsafe {
        asm!(
            "pushfd",
            "pushfd",
            "and dword ptr [esp], 0xFFFFFDFF",
            "or dword ptr [esp], 0x3000",
            "popfd",
            "pushfd",
            "pop {flags}",
            "popfd",
            flags = out(reg) flags,
        );
    }
    let interrupts_on = flags & 0x200 != 0;
    let io_privilege = flags >> 12 & 3;
    if interrupts_on && io_privilege == 0 {
        Some("ignored by the CPU")
    } else {
        None
    }
}

/// Mode 0: everything that goes through the kernel's system call interface.
fn software(quota: u32) -> ! {
    outln!("Attempts that need a capability are made with each of the 32 slots.");
    let mut tally = Tally { denied: 0, total: 0 };
    let mut msg = Msg::new(0);
    let garbage = [0xCCu8; 256];

    tally.attempt(
        "read the keyboard port 0x60",
        every_slot(|slot| sys::io_in(slot, 0x60, 1).err()),
    );
    tally.attempt(
        "reset the machine through port 0x64",
        every_slot(|slot| sys::io_out(slot, 0x64, 1, 0xFE).err()),
    );
    tally.attempt(
        "take over the keyboard interrupt",
        every_slot(|slot| sys::irq_bind(slot, 1, CONSOLE).err()),
    );
    tally.attempt(
        "map physical memory",
        every_slot(|slot| sys::map_mem(slot, SPARE, 0, 1, false).err()),
    );
    tally.attempt("kill another task", every_slot(|slot| sys::task_kill(slot).err()));
    tally.attempt(
        "read kernel and task information",
        every_slot(|slot| sys::stats(slot).err()),
    );
    tally.attempt(
        "write to the kernel log",
        every_slot(|slot| sys::log(slot, b"rogue was here\n").err()),
    );
    tally.attempt(
        "use capability number 1000000",
        refusal(sys::call(1_000_000, &mut msg, 0)),
    );
    tally.attempt(
        "add the receive right to its console cap",
        refusal(sys::cap_derive(CONSOLE, R_SEND | R_RECV, 0, 0)),
    );
    tally.attempt(
        "receive other programs' console requests",
        refusal(sys::recv(CONSOLE, &mut msg, 0)),
    );
    tally.attempt("answer others' calls with forged tokens", forged_replies());
    tally.attempt(
        "re-badge its console cap as the shell's",
        refusal(sys::cap_derive(CONSOLE, R_SEND, proto::B_SHELL, 0)),
    );
    tally.attempt("ask the console driver to crash", ask_console(proto::DBG_CRASH));
    tally.attempt("ask the console driver to hang", ask_console(proto::DBG_HANG));
    tally.attempt(
        "have the kernel write into kernel memory",
        all_refused(&[raw(nr::CALL, CONSOLE, KERNEL, 0), raw(nr::CALL, CONSOLE, 0xB_8000, 0)]),
    );
    tally.attempt(
        "pass a message pointer that wraps around",
        all_refused(&[raw(nr::CALL, CONSOLE, 0xFFFF_FF00, 0)]),
    );
    let child = SpawnArgs::new(b"rogue-child", 8, 1);
    tally.attempt(
        "start a task from kernel memory",
        all_refused(&[raw(nr::SPAWN, KERNEL, 0x4000, &raw const child as u32)]),
    );
    tally.attempt(
        "map pages over kernel and video memory",
        all_refused(&[
            raw(nr::MAP, KERNEL, 1, 1),
            raw(nr::MAP, 0, 1, 1),
            raw(nr::MAP, 0xB_8000, 1, 1),
        ]),
    );
    tally.attempt(
        "unmap kernel pages",
        all_refused(&[raw(nr::UNMAP, KERNEL, 16, 0), raw(nr::UNMAP, 0, 256, 0)]),
    );
    tally.attempt(
        "take more memory than its quota",
        refusal(sys::map(SPARE, quota + 1, true)),
    );
    tally.attempt(
        "start a task from a garbage image",
        refusal(sys::spawn(&garbage, &child)),
    );
    tally.attempt(
        "give a child more memory than it has",
        refusal(sys::spawn(&garbage, &SpawnArgs::new(b"rogue-child", quota * 4, 1))),
    );
    let mut gifted = child;
    gifted.caps[0] = 5;
    tally.attempt(
        "give a child a capability it does not hold",
        refusal(sys::spawn(&garbage, &gifted)),
    );
    tally.attempt("create an endpoint with no endpoint quota", refusal(sys::ep_create()));
    let mut greedy = child;
    greedy.tasks = 1;
    tally.attempt(
        "give a child more task quota than it has",
        refusal(sys::spawn(&garbage, &greedy)),
    );
    let unknown = [nr::COUNT, nr::COUNT + 1, 100, 0x7FFF_FFFF, 0xFFFF_FFFF];
    tally.attempt(
        "call system calls that do not exist",
        if unknown.iter().all(|&number| raw(number, 0, 0, 0) == E_NOSYS) {
            Some(errname(E_NOSYS))
        } else {
            None
        },
    );
    tally.attempt("set IOPL 3 and clear the interrupt flag", flags_attempt());

    sys::exit(tally.denied << 16 | tally.total)
}

/// Modes 1 to 18: one attempt on the hardware itself. Each must end in a CPU exception that
/// the kernel turns into the end of this task; reaching the `exit` means it was not stopped.
/// With `attempt` false the task only prints what it would have tried (the shell skips it).
fn hardware(what: u32, attempt: bool) -> ! {
    let text = match what {
        1 => "disable interrupts (cli)",
        2 => "halt the processor (hlt)",
        3 => "read the keyboard port directly (in)",
        4 => "reset the machine directly (out 0x64)",
        5 => "read kernel memory",
        6 => "write kernel memory",
        7 => "jump into kernel code",
        8 => "load the kernel's data segment",
        9 => "raise the timer interrupt (int 0x20)",
        10 => "replace the page tables (mov cr3)",
        11 => "turn off paging and protection (mov cr0)",
        12 => "replace the interrupt table (lidt)",
        13 => "far jump to the kernel's code segment",
        14 => "overwrite its own code",
        15 => "overflow its stack",
        16 => "return to ring 0 with a forged iret frame",
        17 => "write a model-specific register (wrmsr)",
        18 => "enter the kernel through sysenter",
        _ => sys::exit(0),
    };
    out!(Num(SOFTWARE_ATTEMPTS + what, 2), ". ", Pad(text.as_bytes(), WIDTH));
    if !attempt {
        sys::exit(0)
    }
    let kernel = KERNEL as usize;
    let code = main as *const () as usize;
    let table = [0u8; 6];
    // SAFETY: none of this is safe, on purpose. Each arm is one way of taking over the
    // machine that ring 3, paging or the interrupt table must stop; the task is expected to
    // die in the attempt, and the shell counts a survivor as a failure of the system.
    unsafe {
        match what {
            1 => asm!("cli"),
            2 => asm!("hlt"),
            3 => asm!("in al, 0x60", out("al") _),
            4 => asm!("mov al, 0xFE", "out 0x64, al", out("al") _),
            5 => asm!("mov eax, dword ptr [{p}]", p = in(reg) kernel, out("eax") _),
            6 => asm!("mov dword ptr [{p}], 0", p = in(reg) kernel),
            7 => asm!("jmp {p}", p = in(reg) kernel),
            8 => asm!("mov ax, 0x10", "mov ds, ax", out("eax") _),
            9 => asm!("int 0x20"),
            10 => asm!("mov cr3, {zero}", zero = in(reg) 0usize),
            11 => asm!("mov cr0, {zero}", zero = in(reg) 0usize),
            12 => asm!("lidt ({table})", table = in(reg) &raw const table, options(att_syntax)),
            13 => asm!("ljmp $0x08, $0x100000", options(att_syntax)),
            14 => asm!("mov byte ptr [{p}], 0x90", p = in(reg) code),
            15 => asm!("2:", "push eax", "jmp 2b"),
            16 => asm!("push 0x2", "push 0x08", "push 0x100000", "iretd"),
            17 => asm!("wrmsr", in("ecx") 0x174, in("eax") 0, in("edx") 0),
            _ => asm!("sysenter"),
        }
    }
    sys::exit(0)
}

#[no_mangle]
pub fn main() {
    set_output(Sink::Console(CONSOLE));
    let [what, describe_only, _, quota] = rt::args();
    match what {
        0 => software(quota),
        1..=ROGUE_HW_MODES => hardware(what, describe_only == 0),
        _ => outln!("rogue: no such attempt"),
    }
}
