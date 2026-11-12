//! tchild: the child that the test programs start.
//!
//! `args = [mode, a, b, _]` picks one behaviour (see `rt::proto::mode`): end in a chosen
//! way, break one rule on purpose, play one side of an IPC exchange, or throw random system
//! calls at the kernel. It prints nothing; its parent judges it by how it ends.
#![no_std]
#![no_main]

use core::arch::asm;
use core::slice;
use core::sync::atomic::{AtomicU8, Ordering};

use rt::abi::{
    archive, nr, Msg, SpawnArgs, FOREVER, INFO_CHECK, NO_CAP, PAGE, STACK_TOP, USER_BASE, USER_END,
};
use rt::proto::{self, mode};
use rt::sys;

/// Where GRANDCHILD and FUZZ map the archive.
const ARCHIVE_AT: u32 = 0x7000_0000;
/// Four writable pages that fuzzed system calls may read and write freely.
const SCRATCH: u32 = 0x6000_0000;
const SCRATCH_PAGES: u32 = 4;
/// One page a fuzz kid can read but not write.
const READ_ONLY: u32 = 0x6010_0000;
/// 64 pages that a fuzz kid maps and unmaps at random.
const PLAY: u32 = 0x6100_0000;
const PLAY_PAGES: u32 = 64;
/// The one-byte instruction `ret`.
const RET: u8 = 0xC3;
/// A writable byte in the program's data; EXEC_DATA puts a `ret` there and calls it.
static IN_DATA: AtomicU8 = AtomicU8::new(0);

#[no_mangle]
pub fn main() {
    let [what, a, b, _] = rt::args();
    match what {
        mode::EXIT => sys::exit(a),
        mode::NULL_READ..=mode::STACK_OVERFLOW => misbehave(what),
        mode::PANIC => panic!("asked to panic"),
        mode::EXEC_STACK..=mode::EXEC_MAPPED => run_data(what),
        mode::SPIN => loop {
            core::hint::spin_loop();
        },
        mode::SLEEP => {
            sys::sleep(a);
            sys::exit(b)
        }
        mode::ECHO => echo(),
        mode::CALL_ONCE => call_once(a, b),
        mode::RECV_ONCE => {
            let mut msg = Msg::new(0);
            let _ = sys::recv(0, &mut msg, FOREVER);
            sys::exit(7)
        }
        mode::GRANDCHILD => grandchild(a),
        mode::ENDPOINTS => endpoints(),
        mode::AGENT => agent(),
        mode::HOGS => hogs(a, b),
        mode::FUZZ => fuzz(a, b),
        _ => sys::exit(0xBAD0),
    }
}

/// Does one thing that the CPU or the kernel must stop. Exits with 0xBAD1 if nothing did.
fn misbehave(what: u32) -> ! {
    let code = main as *const () as usize;
    let kernel = 0x10_0000usize;
    // SAFETY: none of this is safe, on purpose. Each arm breaks exactly one rule that
    // protects the rest of the system; the task is expected to be ended by the fault, and
    // its parent checks which fault it was.
    unsafe {
        match what {
            mode::NULL_READ => asm!("mov eax, dword ptr [{p}]", p = in(reg) 4usize, out("eax") _),
            mode::WRITE_CODE => asm!("mov byte ptr [{p}], 0x90", p = in(reg) code),
            mode::KERNEL_READ => asm!("mov eax, dword ptr [{p}]", p = in(reg) kernel, out("eax") _),
            mode::KERNEL_WRITE => asm!("mov dword ptr [{p}], 0", p = in(reg) kernel),
            mode::KERNEL_JUMP => asm!("jmp {p}", p = in(reg) kernel),
            mode::DIVIDE => asm!(
                "xor edx, edx",
                "xor ecx, ecx",
                "div ecx",
                out("eax") _,
                out("ecx") _,
                out("edx") _,
            ),
            mode::UD2 => asm!("ud2"),
            mode::CLI => asm!("cli"),
            mode::HLT => asm!("hlt"),
            mode::PORT_IN => asm!("in al, 0x60", out("al") _),
            mode::FPU => asm!("fninit"),
            mode::STACK_OVERFLOW => asm!("2:", "push eax", "jmp 2b"),
            _ => {}
        }
    }
    sys::exit(0xBAD1)
}

/// Puts a `ret` instruction into memory that holds data (the stack, a static, or a page
/// mapped just now) and calls it. The no-execute bit must stop the call; on a processor
/// without that bit the call returns and the task exits with 0xBAD1.
fn run_data(what: u32) -> ! {
    let mut on_stack = [RET; 16];
    let target = match what {
        mode::EXEC_STACK => core::hint::black_box(&mut on_stack).as_mut_ptr() as usize,
        mode::EXEC_DATA => {
            IN_DATA.store(RET, Ordering::Relaxed);
            IN_DATA.as_ptr() as usize
        }
        _ => {
            if sys::map(PLAY, 1, true).is_err() {
                sys::exit(0xBAD2)
            }
            // SAFETY: the page at PLAY was mapped writable just above; nothing else uses it.
            unsafe { (PLAY as *mut u8).write(RET) };
            PLAY as usize
        }
    };
    // SAFETY: not safe, on purpose: `target` is a `ret` instruction in a page that holds
    // data, and the no-execute bit must stop the call. Where the processor lacks that bit
    // the call comes straight back, having used one word of stack and nothing else.
    unsafe { asm!("call {target}", target = in(reg) target, clobber_abi("C")) };
    sys::exit(0xBAD1)
}

/// A server that answers every request with what it saw, until it is told to stop.
///
/// The reply carries `words = [w0 + 1, badge seen, label seen, length seen]`, the label the
/// client asked for in `words[1]`, and the request's data with every bit inverted, so that a
/// client can tell a reply from a buffer nobody wrote to. It also sets a badge and a token,
/// which the kernel must not pass on.
fn echo() -> ! {
    let mut msg = Msg::new(0);
    loop {
        if sys::recv(0, &mut msg, FOREVER).is_err() {
            sys::exit(0xE0);
        }
        let mut answer = msg;
        answer.label = msg.words[1];
        answer.words = [msg.words[0].wrapping_add(1), msg.badge, msg.label, msg.len];
        let len = (msg.len as usize).min(answer.data.len());
        for byte in answer.data[..len].iter_mut() {
            *byte = !*byte;
        }
        answer.badge = 0xDEAD_BEEF;
        answer.token = 0xFEED_F00D;
        let _ = sys::reply(msg.token, &answer);
        if msg.label == proto::ECHO_STOP {
            sys::exit(0);
        }
    }
}

/// Makes one call on slot 0 and exits with the first word of the reply.
fn call_once(label: u32, word: u32) -> ! {
    let mut msg = Msg::new(label);
    msg.words[0] = word;
    match sys::call(0, &mut msg, FOREVER) {
        Ok(()) => sys::exit(msg.words[0]),
        Err(code) => sys::exit(0xE000_0000 | code.unsigned_abs()),
    }
}

/// Maps the archive through the Memory capability in `slot`.
fn map_archive(slot: u32, pages: u32) -> Option<&'static [u8]> {
    sys::map_mem(slot, ARCHIVE_AT, 0, pages, false).ok()?;
    // SAFETY: the kernel has just mapped `pages` readable pages at ARCHIVE_AT. They are
    // read-only, nothing else in this program refers to them, and the kernel never frees
    // pages mapped from a Memory capability.
    Some(unsafe { slice::from_raw_parts(ARCHIVE_AT as *const u8, pages as usize * PAGE as usize) })
}

/// Starts a spinning child of its own and waits to be killed; the grandchild must die too.
fn grandchild(archive_pages: u32) -> ! {
    let Some(archive) = map_archive(1, archive_pages) else {
        sys::exit(0xE1);
    };
    let Some(image) = archive::find(archive, b"tchild") else {
        sys::exit(0xE2);
    };
    let mut spawn = SpawnArgs::new(b"grandchild", 24, 2);
    spawn.args = [mode::SPIN, 0, 0, 0];
    if sys::spawn(image, &spawn).is_err() {
        sys::exit(0xE3);
    }
    loop {
        sys::sleep(1000);
    }
}

/// Starts `count` spinning children and then spins itself: a group of tasks that takes all
/// the processor time it is given.
fn hogs(archive_pages: u32, count: u32) -> ! {
    let Some(archive) = map_archive(1, archive_pages) else {
        sys::exit(0xE1);
    };
    let Some(image) = archive::find(archive, b"tchild") else {
        sys::exit(0xE2);
    };
    for _ in 0..count {
        let mut spawn = SpawnArgs::new(b"hog", 24, 2);
        spawn.args = [mode::SPIN, 0, 0, 0];
        if sys::spawn(image, &spawn).is_err() {
            sys::exit(0xE3);
        }
    }
    loop {
        core::hint::spin_loop();
    }
}

/// Creates endpoints until the kernel refuses, reports how many, and waits to be killed.
fn endpoints() -> ! {
    let mut count = 0u32;
    let error = loop {
        match sys::ep_create() {
            Ok(_) => count += 1,
            Err(code) => break code,
        }
    };
    let mut msg = Msg::new(1);
    msg.words[0] = count;
    msg.words[1] = error.unsigned_abs();
    let _ = sys::call(0, &mut msg, FOREVER);
    loop {
        sys::sleep(1000);
    }
}

/// Does what its parent asks. Each request is one system call: the label is its number and
/// the words are its arguments. The reply carries `[result, second result, slot, error]`,
/// where `slot` is where the capability that came with the request landed. Four bytes of
/// data name a slot of the agent whose capability the reply should carry back; if the
/// kernel refuses that, the agent answers without it and `error` says why.
fn agent() -> ! {
    let mut msg = Msg::new(0);
    loop {
        if let Err(code) = sys::recv(0, &mut msg, FOREVER) {
            sys::exit(0xE000_0000 | code.unsigned_abs());
        }
        let [a, b, c, d] = msg.words;
        // SAFETY: the parent, a test program, chooses the call. Whatever it does to this
        // task is the experiment, and the kernel must keep every other task safe.
        let (result, second) = unsafe { sys::raw(msg.label, a, b, c, d) };
        let mut answer = Msg::new(0);
        answer.words = [result as u32, second, msg.cap, 0];
        if msg.len == 4 {
            answer.cap = u32::from_le_bytes([msg.data[0], msg.data[1], msg.data[2], msg.data[3]]);
        }
        if let Err(code) = sys::reply(msg.token, &answer) {
            answer.cap = NO_CAP;
            answer.words[3] = code.unsigned_abs();
            let _ = sys::reply(msg.token, &answer);
        }
    }
}

/// xorshift32: small, fast and good enough to pick system call arguments.
struct Rng(u32);

impl Rng {
    fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    fn below(&mut self, limit: u32) -> u32 {
        self.next() % limit
    }
}

/// Memory on the kid's stack that system calls may write to. A message is the largest thing
/// the kernel writes, so a write that starts at `spawn` ends inside `slack`.
#[repr(C)]
struct Buffers {
    msg: Msg,
    spawn: SpawnArgs,
    slack: [u8; core::mem::size_of::<Msg>()],
}

/// Values that tend to sit on the edge of what the kernel accepts.
const EDGES: [u32; 27] = [
    0,
    1,
    2,
    3,
    4,
    5,
    6,
    7,
    8,
    15,
    16,
    31,
    32,
    33,
    255,
    256,
    257,
    0x1000,
    0x7FFF_FFFF,
    0x8000_0000,
    0xFFFF_FFFF,
    0xFFFF_F000,
    USER_BASE,
    USER_END,
    STACK_TOP,
    0x10_0000,
    0xB_8000,
];

/// What the children of a fuzz kid do: most end quickly, a few block or spin.
const CHILD_MODES: [u32; 16] = [
    mode::EXIT,
    mode::EXIT,
    mode::EXIT,
    mode::EXIT,
    mode::NULL_READ,
    mode::NULL_READ,
    mode::SLEEP,
    mode::SLEEP,
    mode::SLEEP,
    mode::SLEEP,
    mode::CALL_ONCE,
    mode::CALL_ONCE,
    mode::CALL_ONCE,
    mode::CALL_ONCE,
    mode::RECV_ONCE,
    mode::SPIN,
];

/// One fuzz kid: it makes system calls with random and almost-right arguments for as long
/// as it lives, and tells the fuzzer (slot 0) what it tried.
///
/// Capabilities: 0 send to the fuzzer, 1 Info, 2 I/O port 0x80, 3 the archive, 4 IRQ 5.
struct Kid {
    rng: Rng,
    /// Slot of the kid's own endpoint.
    own: u32,
    /// Address and length of the `tchild` image inside the mapped archive.
    image: u32,
    image_len: u32,
    /// Addresses of `Buffers::msg` and `Buffers::spawn`.
    msg: u32,
    spawn: u32,
    /// The token of the last message the kid received.
    token: u32,
    tried: [u16; proto::FUZZ_NUMBERS],
    succeeded: [u16; proto::FUZZ_NUMBERS],
    /// Calls and invariant checks made since the last report.
    calls: u32,
    checks: u32,
}

impl Kid {
    fn play_page(&mut self) -> u32 {
        PLAY + self.rng.below(PLAY_PAGES) * PAGE
    }

    /// One argument: an edge value, an address or number that means something to this kid,
    /// or a random word.
    fn pool(&mut self) -> u32 {
        let pick = self.rng.below(EDGES.len() as u32 + 13) as usize;
        if pick < EDGES.len() {
            return EDGES[pick];
        }
        match pick - EDGES.len() {
            0 => SCRATCH,
            1 => SCRATCH + PAGE - 100,
            2 => SCRATCH + SCRATCH_PAGES * PAGE - 100,
            3 => READ_ONLY,
            4 => main as *const () as usize as u32,
            5 => self.msg,
            6 => self.token,
            7 => self.image,
            8 => self.image_len,
            9 => self.spawn,
            10 => self.play_page(),
            11 => self.own,
            _ => self.rng.next(),
        }
    }

    /// Decides what the message in `Buffers::msg` carries: mostly no capability, sometimes
    /// one of the kid's first eight.
    fn attach(&mut self) {
        let cap = if self.rng.below(4) == 0 { self.rng.below(8) } else { NO_CAP };
        let field = self.msg + core::mem::offset_of!(Msg, cap) as u32;
        // SAFETY: `self.msg` is the address of `Buffers::msg` on this task's stack, `cap` is
        // a 32-bit field of it, and the buffer is only reached through this address.
        unsafe { (field as *mut u32).write_volatile(cap) };
    }

    /// Fills `Buffers::spawn` with arguments for a child that would really start, and that
    /// asks for some of the kid's quota of tasks and endpoints.
    fn refresh_spawn(&mut self) {
        let quota = 8 + self.rng.below(17);
        let stack_pages = 1 + self.rng.below(3);
        let mut spawn = SpawnArgs::new(b"fuzz-child", quota, stack_pages);
        spawn.tasks = self.rng.below(3);
        spawn.endpoints = self.rng.below(3);
        let what = CHILD_MODES[self.rng.below(CHILD_MODES.len() as u32) as usize];
        spawn.args = [what, self.rng.below(8), self.rng.below(8), 0];
        if self.rng.below(2) == 0 {
            spawn.fault_ep = self.own;
            spawn.fault_badge = self.rng.next();
        }
        if self.rng.below(2) == 0 {
            spawn.caps[0] = self.own;
        }
        // SAFETY: `self.spawn` is the address of `Buffers::spawn`, which lives on this
        // task's stack for as long as the kid runs and is only reached through this address.
        unsafe { (self.spawn as *mut SpawnArgs).write_volatile(spawn) };
    }

    /// Arguments that are right, or nearly right, for system call `number`, so that the
    /// fuzzer gets past the first argument check and into the kernel's real work.
    fn template(&mut self, number: u32) -> [u32; 4] {
        let small = self.rng.below(8);
        match number {
            nr::CALL => {
                self.attach();
                let slot = if self.rng.below(16) == 0 { self.own } else { 0 };
                [slot, self.msg, 0, 0]
            }
            nr::RECV => [self.own, self.msg, 0, 0],
            nr::REPLY => {
                self.attach();
                [self.token, self.msg, 0, 0]
            }
            nr::CAP_DERIVE => [small, 1 + self.rng.below(7), self.rng.below(3), 0],
            nr::CAP_REVOKE => [small, 0, 0, 0],
            nr::CAP_DROP | nr::TASK_KILL => [1 + self.rng.below(31), 0, 0, 0],
            nr::MAP => [self.play_page(), 1 + small, small & 1, 0],
            nr::UNMAP => [self.play_page(), 1 + small, 0, 0],
            nr::MAP_MEM => [3, self.play_page(), small, 1 + self.rng.below(4)],
            nr::IO_IN | nr::IO_OUT => [2, 0x80, 1, small],
            nr::IRQ_BIND => [4, 5, self.own, 0],
            nr::IRQ_ACK => [4, 5, 0, 0],
            nr::SPAWN => {
                self.refresh_spawn();
                [self.image, self.image_len, self.spawn, 0]
            }
            nr::LOG => [small, self.msg, 16, 0],
            nr::SYSINFO => [1, self.rng.below(4), small, self.msg],
            _ => [0; 4],
        }
    }

    /// Tells the fuzzer what was tried since the last report.
    fn report(&mut self) {
        let mut msg = Msg::new(proto::FUZZ_REPORT);
        msg.words[0] = self.calls;
        msg.words[1] = self.checks;
        for (index, record) in msg.data.chunks_exact_mut(4).take(proto::FUZZ_NUMBERS).enumerate() {
            record[..2].copy_from_slice(&self.tried[index].to_le_bytes());
            record[2..].copy_from_slice(&self.succeeded[index].to_le_bytes());
        }
        msg.len = 4 * proto::FUZZ_NUMBERS as u32;
        let _ = sys::call(0, &mut msg, 100);
        self.tried = [0; proto::FUZZ_NUMBERS];
        self.succeeded = [0; proto::FUZZ_NUMBERS];
        self.calls = 0;
        self.checks = 0;
    }

    /// Makes one system call.
    fn step(&mut self) {
        let number = self.rng.below(proto::FUZZ_NUMBERS as u32);
        let mut args = if self.rng.below(4) == 0 {
            let mut args = self.template(number);
            if self.rng.below(4) == 0 {
                let position = self.rng.below(4) as usize;
                args[position] = self.pool();
            }
            args
        } else {
            [self.pool(), self.pool(), self.pool(), self.pool()]
        };
        // A kid that waits or ends finds nothing, so 63 calls in 64 may not do either.
        let patience = self.rng.below(64);
        if patience != 0 {
            let longest = if patience == 1 { 3 } else { 1 };
            match number {
                nr::EXIT => return,
                nr::SLEEP => args[0] %= longest,
                nr::CALL | nr::RECV => args[2] %= longest,
                _ => {}
            }
        } else if number == nr::EXIT {
            self.tried[number as usize] += 1;
            self.succeeded[number as usize] += 1;
            self.calls += 1;
            self.report();
        }
        // SAFETY: there is no safe way to do this, and that is the point: the kernel must
        // refuse or survive any arguments. The addresses the kid passes on purpose lie in
        // memory set aside for it (the scratch and play pages and `Buffers`). A random word
        // that happens to point into the kid's own stack or code can only damage this task,
        // and a task that damages itself is one of the outcomes under test.
        let (result, _) = unsafe { sys::raw(number, args[0], args[1], args[2], args[3]) };
        self.calls += 1;
        self.tried[number as usize] += 1;
        if number == nr::TIME || result >= 0 {
            self.succeeded[number as usize] += 1;
            if number == nr::RECV && args[1] == self.msg {
                // SAFETY: `self.msg` is the address of `Buffers::msg` on this task's stack;
                // the token is the third 32-bit word of a message.
                self.token = unsafe { ((self.msg + 8) as *const u32).read_volatile() };
            }
            if number == nr::SYSINFO && args[1] == INFO_CHECK {
                self.checks += 1;
                if result > 0 {
                    let mut alarm = Msg::new(proto::FUZZ_VIOLATION);
                    alarm.words[0] = result as u32;
                    let _ = sys::call(0, &mut alarm, 1000);
                }
            }
        }
        if self.calls >= proto::FUZZ_REPORT_EVERY {
            self.report();
        }
    }
}

/// Runs a fuzz kid until it ends itself or the fuzzer ends it.
fn fuzz(seed: u32, archive_pages: u32) -> ! {
    if sys::map(SCRATCH, SCRATCH_PAGES, true).is_err() || sys::map(READ_ONLY, 1, false).is_err() {
        sys::exit(0xF1);
    }
    let Some(archive) = map_archive(3, archive_pages) else {
        sys::exit(0xF1);
    };
    let Some(image) = archive::find(archive, b"tchild") else {
        sys::exit(0xF2);
    };
    let Ok(own) = sys::ep_create() else {
        sys::exit(0xF3);
    };
    let mut buffers = Buffers {
        msg: Msg::new(0),
        spawn: SpawnArgs::new(b"fuzz-child", 8, 1),
        slack: [0; core::mem::size_of::<Msg>()],
    };
    let mut kid = Kid {
        rng: Rng(seed | 1),
        own,
        image: image.as_ptr() as u32,
        image_len: image.len() as u32,
        msg: &raw mut buffers.msg as u32,
        spawn: &raw mut buffers.spawn as u32,
        token: 0,
        tried: [0; proto::FUZZ_NUMBERS],
        succeeded: [0; proto::FUZZ_NUMBERS],
        calls: 0,
        checks: 0,
    };
    loop {
        kid.step();
    }
}
