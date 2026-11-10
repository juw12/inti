//! selftest: checks from user space that the kernel keeps its promises.
//!
//! Every check prints one line, `  ok   <name>` or `  FAIL <name>`. A failed check is
//! preceded by lines that say what was expected and what happened. The last line is
//! `SELFTEST: PASS n/n` or `SELFTEST: FAIL p/n`.
//!
//! Capabilities, handed over by the shell: 0 console, 1 files, 2 Info, 3 I/O port 0x80,
//! 4 the archive (read-only memory), 5 IRQ 5, 6 Log.
#![no_std]
#![no_main]

use core::slice;

use rt::abi::{
    nr, CapInfo, KStats, Msg, SpawnArgs, TaskInfo, ARCHIVE_MAGIC, CAP_ENDPOINT, CAP_IOPORT,
    CAP_NULL, CAP_SLOTS, CAUSE_EXIT, CAUSE_KILLED, E_BUSY, E_DEAD, E_FAULT, E_INVAL, E_NOCAP,
    E_NOMEM, E_NOSLOT, E_NOSYS, E_PERM, E_TIMEOUT, INFO_CAP, INFO_STATS, INFO_TASK, L_FAULT,
    L_IRQ, L_KERNEL, MAX_ENDPOINTS, MAX_TASKS, MSG_DATA, NO_CAP, PAGE, R_GRANT, R_RECV, R_SEND,
    ST_FREE, ST_RECV, ST_SEND, USER_BASE, USER_END,
};
use rt::client::{self, Mapping};
use rt::io::{set_output, Hex, Sink};
use rt::proto::{self, mode};
use rt::{outln, sys};

const CONSOLE: u32 = 0;
const FILES: u32 = 1;
const INFO: u32 = 2;
const PORT: u32 = 3;
const ARCHIVE: u32 = 4;
const IRQ: u32 = 5;
const LOG: u32 = 6;
/// A capability slot that stays empty.
const EMPTY: u32 = 31;

/// Where things go in this program's address space.
const CHILD_AT: u32 = 0x5000_0000;
const COPY_AT: u32 = 0x5800_0000;
/// Two writable pages, an unmapped gap of two pages, and one read-only page. They are
/// only ever handed to the kernel as addresses.
const SCRATCH: u32 = 0x6000_0000;
const READ_ONLY: u32 = SCRATCH + 4 * PAGE;
const MEMORY: u32 = 0x6200_0000;
const UNMAPPED: u32 = 0x6800_0000;
const ARCHIVE_AT: u32 = 0x7000_0000;
const QUOTA_AT: u32 = 0x8000_0000;
/// The kernel is loaded at 1 MiB; the text screen is at 0xB8000.
const KERNEL: u32 = 0x10_0000;
const VIDEO: u32 = 0xB_8000;

/// Memory quota and stack of an ordinary child.
const CHILD_PAGES: u32 = 48;
const CHILD_STACK: u32 = 4;
/// CPU exception numbers.
const PAGE_FAULT: u32 = 14;
const PROTECTION_FAULT: u32 = 13;
/// The keyboard controller's command port; writing 0xFE to it resets the machine.
const RESET_PORT: u32 = 0x64;

/// The running score.
struct Test {
    passed: u32,
    total: u32,
}

impl Test {
    fn check(&mut self, name: &str, ok: bool) {
        self.total += 1;
        if ok {
            self.passed += 1;
            outln!("  ok   ", name);
        } else {
            outln!("  FAIL ", name);
        }
    }

    fn done(&mut self, name: &str, check: Check) {
        self.check(name, check.ok);
    }
}

/// How a child ended, as the kernel reported it.
#[derive(Clone, Copy)]
struct End {
    badge: u32,
    cause: u32,
    /// The exit code, or the address that a page fault was about.
    detail: u32,
    eip: u32,
}

/// What must be the same before and after a group of checks: tasks, endpoints, and page
/// frames (the free ones plus the ones this program holds itself).
#[derive(Clone, Copy)]
struct Usage {
    tasks: u32,
    endpoints: u32,
    frames: u32,
}

/// One check in the making. Every expectation that fails is printed and remembered.
struct Check {
    ok: bool,
}

impl Check {
    fn new() -> Check {
        Check { ok: true }
    }

    fn yes(&mut self, what: &str, condition: bool) {
        if !condition {
            outln!("       not true: ", what);
            self.ok = false;
        }
    }

    fn eq(&mut self, what: &str, got: u32, want: u32) {
        if got != want {
            outln!("       ", what, ": got ", got, " (", Hex(got), "), expected ", want);
            self.ok = false;
        }
    }

    fn code(&mut self, what: &str, got: i32, want: i32) {
        if got != want {
            outln!("       ", what, ": got ", got, ", expected ", want);
            self.ok = false;
        }
    }

    /// Expects a system call to fail with the error `want`.
    fn fails<T>(&mut self, what: &str, result: Result<T, i32>, want: i32) {
        self.code(what, code(result), want);
    }

    /// Expects a system call to succeed, and hands on its result.
    fn works<T>(&mut self, what: &str, result: Result<T, i32>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                self.code(what, error, 0);
                None
            }
        }
    }

    /// Expects a child to have ended with `cause` and, if one is given, `detail`.
    fn ended(&mut self, what: &str, end: Option<End>, cause: u32, detail: Option<u32>) {
        match end {
            Some(end) => {
                self.eq(what, end.cause, cause);
                if let Some(detail) = detail {
                    self.eq(what, end.detail, detail);
                }
            }
            None => self.yes(what, false),
        }
    }

    fn same(&mut self, now: Usage, before: Usage) {
        self.eq("tasks", now.tasks, before.tasks);
        self.eq("endpoints", now.endpoints, before.endpoints);
        self.eq("page frames", now.frames, before.frames);
    }
}

/// The result code of a system call: 0 for success, otherwise the error.
fn code<T>(result: Result<T, i32>) -> i32 {
    match result {
        Ok(_) => 0,
        Err(error) => error,
    }
}

/// A system call with arguments that the safe wrappers cannot express.
fn raw(number: u32, a: u32, b: u32, c: u32, d: u32) -> i32 {
    // SAFETY: the callers pass values that the kernel must refuse, or addresses inside the
    // scratch pages, which this program reaches only through raw addresses. If the kernel
    // wrongly accepted a bad argument, the damage would be the finding.
    unsafe { sys::raw(number, a, b, c, d).0 }
}

fn stats() -> KStats {
    sys::stats(INFO).unwrap_or(KStats::ZERO)
}

fn task(index: u32) -> TaskInfo {
    sys::task_info(INFO, index).unwrap_or(TaskInfo::ZERO)
}

fn cap(task: u32, slot: u32) -> CapInfo {
    sys::cap_info(INFO, task, slot).unwrap_or(CapInfo::ZERO)
}

fn usage(me: u32) -> Usage {
    let now = stats();
    Usage {
        tasks: now.tasks,
        endpoints: now.endpoints,
        frames: now.free_frames + task(me).charged,
    }
}

/// Pages of its quota that task `me` has not used.
fn spare_pages(me: u32) -> u32 {
    let info = task(me);
    info.quota - info.charged
}

/// Polls `condition` for up to a second.
fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = sys::time() + 1_000_000;
    loop {
        if condition() {
            return true;
        }
        if sys::time() >= deadline {
            return false;
        }
        sys::sleep(1);
    }
}

/// A byte pattern that differs from zero and from its neighbours.
fn pattern(index: u32) -> u8 {
    (index.wrapping_mul(7).wrapping_add(3)) as u8
}

/// A 32-bit little-endian word of a program image.
fn word(image: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([image[at], image[at + 1], image[at + 2], image[at + 3]])
}

fn set_word(image: &mut [u8], at: usize, value: u32) {
    image[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// Where in an ELF image the program header of its first loadable segment is.
fn first_load_header(image: &[u8]) -> Option<usize> {
    let table = word(image, 28) as usize;
    let count = word(image, 44) as usize & 0xFFFF;
    (0..count)
        .map(|index| table + 32 * index)
        .find(|&at| at + 32 <= image.len() && word(image, at) == 1)
}

/// Pages that a check mapped for itself and reaches only through this handle.
struct Pages {
    base: u32,
    bytes: u32,
}

impl Pages {
    /// Maps `count` new writable pages at `base`.
    fn map(base: u32, count: u32) -> Result<Pages, i32> {
        sys::map(base, count, true)?;
        Ok(Pages { base, bytes: count * PAGE })
    }

    fn get(&self, offset: u32) -> u8 {
        assert!(offset < self.bytes);
        // SAFETY: `map` mapped these pages and they stay mapped until `unmap` consumes the
        // handle; nothing else refers to them.
        unsafe { ((self.base + offset) as *const u8).read_volatile() }
    }

    fn set(&self, offset: u32, value: u8) {
        assert!(offset < self.bytes);
        // SAFETY: as in `get`, and the pages are writable.
        unsafe { ((self.base + offset) as *mut u8).write_volatile(value) }
    }

    fn fill(&self, bytes: impl Fn(u32) -> u8) {
        for offset in 0..self.bytes {
            self.set(offset, bytes(offset));
        }
    }

    /// True if every byte is what `bytes` says it should be.
    fn holds(&self, bytes: impl Fn(u32) -> u8) -> bool {
        (0..self.bytes).all(|offset| self.get(offset) == bytes(offset))
    }

    fn unmap(self) -> Result<(), i32> {
        // SAFETY: the handle is consumed, so nothing can reach the pages afterwards.
        unsafe { sys::unmap(self.base, self.bytes / PAGE) }
    }
}

/// A child that was started: its Task capability and the badge its end will carry.
#[derive(Clone, Copy)]
struct Kid {
    task: u32,
    badge: u32,
}

/// Starts `tchild` in its many modes and collects how each child ended.
struct Kids {
    image: Mapping,
    /// The endpoint on which the kernel reports the end of every child.
    notices: u32,
    launches: u32,
    /// This program's own number in the task table.
    me: u32,
    /// Ends that arrived while waiting for another child's.
    ended: [End; 32],
    count: usize,
}

impl Kids {
    fn new() -> Option<Kids> {
        let image = client::load(FILES, b"tchild", CHILD_AT)?;
        let notices = sys::ep_create().ok()?;
        let me = client::task_named(INFO, b"selftest")?;
        Some(Kids {
            image,
            notices,
            launches: 0,
            me,
            ended: [End { badge: 0, cause: 0, detail: 0, eip: 0 }; 32],
            count: 0,
        })
    }

    /// Arguments for a child whose end is reported under a badge of its own.
    fn spawn_args(&mut self, what: u32, a: u32, b: u32, caps: &[u32], quota: u32, stack: u32) -> SpawnArgs {
        self.launches += 1;
        let mut spawn = SpawnArgs::new(b"tchild", quota, stack);
        spawn.fault_ep = self.notices;
        spawn.fault_badge = self.launches;
        spawn.args = [what, a, b, 0];
        spawn.caps[..caps.len()].copy_from_slice(caps);
        spawn
    }

    fn start_with(&mut self, what: u32, a: u32, b: u32, caps: &[u32], quota: u32) -> Result<Kid, i32> {
        let spawn = self.spawn_args(what, a, b, caps, quota, CHILD_STACK);
        let task = sys::spawn(self.image.bytes(), &spawn)?;
        Ok(Kid { task, badge: spawn.fault_badge })
    }

    fn start(&mut self, what: u32, a: u32, b: u32, caps: &[u32]) -> Option<Kid> {
        match self.start_with(what, a, b, caps, CHILD_PAGES) {
            Ok(kid) => Some(kid),
            Err(error) => {
                outln!("       could not start a child: error ", error);
                None
            }
        }
    }

    /// Waits up to `limit_ms` for the end of the child whose fault badge is `badge`.
    fn end_of_badge(&mut self, badge: u32, limit_ms: u32) -> Option<End> {
        let deadline = sys::time() + u64::from(limit_ms) * 1000;
        loop {
            let known = self.ended[..self.count].iter().position(|end| end.badge == badge);
            if let Some(at) = known {
                let end = self.ended[at];
                self.count -= 1;
                self.ended[at] = self.ended[self.count];
                return Some(end);
            }
            let now = sys::time();
            if now >= deadline {
                return None;
            }
            let wait_ms = ((deadline - now) / 1000) as u32 + 1;
            let mut msg = Msg::new(0);
            let arrived = sys::recv(self.notices, &mut msg, wait_ms).is_ok();
            if arrived && msg.label == L_FAULT && self.count < self.ended.len() {
                self.ended[self.count] = End {
                    badge: msg.badge,
                    cause: msg.words[0],
                    detail: msg.words[1],
                    eip: msg.words[2],
                };
                self.count += 1;
            }
        }
    }

    /// Waits for a child to end and gives up its Task capability. A child that has not
    /// ended after three seconds is killed, and the check that waited for it fails.
    fn end_of(&mut self, kid: Kid) -> Option<End> {
        let end = self.end_of_badge(kid.badge, 3000);
        if end.is_none() {
            outln!("       a child did not end in time");
            let _ = sys::task_kill(kid.task);
            let _ = self.end_of_badge(kid.badge, 1000);
        }
        let _ = sys::cap_drop(kid.task);
        end
    }

    fn kill(&mut self, kid: Kid) -> Option<End> {
        let _ = sys::task_kill(kid.task);
        self.end_of(kid)
    }

    /// Runs a child that holds no capabilities to its end.
    fn run(&mut self, what: u32, a: u32, b: u32) -> Option<End> {
        let kid = self.start(what, a, b, &[])?;
        self.end_of(kid)
    }

    /// The child's number in the task table.
    fn index_of(&self, kid: Kid) -> u32 {
        cap(self.me, kid.task).a
    }

    /// Waits up to a second for the child to reach `state`.
    fn wait_state(&self, kid: Kid, state: u32) -> bool {
        let index = self.index_of(kid);
        wait_until(|| task(index).state == state)
    }

    /// Tries to start a child that the kernel must refuse with the error `want`.
    fn refuse(&mut self, c: &mut Check, what: &str, image: &[u8], quota: u32, stack: u32, want: i32) {
        let spawn = self.spawn_args(mode::EXIT, 33, 0, &[], quota, stack);
        match sys::spawn(image, &spawn) {
            Ok(task) => {
                c.yes(what, false);
                let _ = self.end_of(Kid { task, badge: spawn.fault_badge });
            }
            Err(error) => c.code(what, error, want),
        }
    }
}

/// System calls check their numbers, their pointers and their lengths.
fn arguments(test: &mut Test, kids: &mut Kids) -> Option<()> {
    let ep = sys::ep_create().ok()?;

    let mut c = Check::new();
    for number in [nr::COUNT, nr::COUNT + 1, 100, 0x7FFF_FFFF, 0xFFFF_FFFF] {
        c.code("result", raw(number, 0, 0, 0, 0), E_NOSYS);
    }
    test.done("system call numbers that do not exist give E_NOSYS", c);

    let mut c = Check::new();
    for pointer in [0, KERNEL, VIDEO, UNMAPPED, 0xFFFF_FF00, USER_END - 16, USER_END] {
        c.code("call", raw(nr::CALL, ep, pointer, 0, 0), E_FAULT);
        c.code("recv", raw(nr::RECV, ep, pointer, 0, 0), E_FAULT);
    }
    test.done("a message buffer outside the caller's memory gives E_FAULT", c);

    let mut c = Check::new();
    c.code("call", raw(nr::CALL, ep, READ_ONLY, 0, 0), E_FAULT);
    c.code("recv", raw(nr::RECV, ep, READ_ONLY, 0, 0), E_FAULT);
    test.done("a message buffer the caller cannot write gives E_FAULT", c);

    let mut c = Check::new();
    let into_gap = SCRATCH + 2 * PAGE - 100;
    let across_pages = SCRATCH + PAGE - 100;
    c.code("call into an unmapped page", raw(nr::CALL, ep, into_gap, 0, 0), E_FAULT);
    c.code("recv into an unmapped page", raw(nr::RECV, ep, into_gap, 0, 0), E_FAULT);
    c.code("recv across two mapped pages", raw(nr::RECV, ep, across_pages, 0, 0), E_TIMEOUT);
    test.done("a message buffer is checked up to its last byte", c);

    let mut c = Check::new();
    let text_into_gap = SCRATCH + 2 * PAGE - 8;
    c.code("257 bytes", raw(nr::LOG, LOG, SCRATCH, 257, 0), E_INVAL);
    c.code("4 GiB", raw(nr::LOG, LOG, SCRATCH, 0xFFFF_FFFF, 0), E_INVAL);
    c.code("text in kernel memory", raw(nr::LOG, LOG, KERNEL, 16, 0), E_FAULT);
    c.code("text at address 0", raw(nr::LOG, LOG, 0, 16, 0), E_FAULT);
    c.code("text running into an unmapped page", raw(nr::LOG, LOG, text_into_gap, 16, 0), E_FAULT);
    test.done("log checks the length and the address of the text", c);

    let mut c = Check::new();
    for pointer in [0, KERNEL, VIDEO, UNMAPPED, READ_ONLY] {
        c.code("statistics", raw(nr::SYSINFO, INFO, INFO_STATS, 0, pointer), E_FAULT);
        c.code("task", raw(nr::SYSINFO, INFO, INFO_TASK, 0, pointer), E_FAULT);
        c.code("capability", raw(nr::SYSINFO, INFO, INFO_CAP, 0, pointer), E_FAULT);
    }
    c.code("request 4", raw(nr::SYSINFO, INFO, 4, 0, SCRATCH), E_INVAL);
    c.code("request 0xFFFFFFFF", raw(nr::SYSINFO, INFO, 0xFFFF_FFFF, 0, SCRATCH), E_INVAL);
    test.done("sysinfo checks where it writes and what it is asked", c);

    let mut c = Check::new();
    let args = SpawnArgs::new(b"nothing", 16, 2);
    let args_at = &raw const args as u32;
    let image = kids.image.bytes();
    let (image_at, image_len) = (image.as_ptr() as u32, image.len() as u32);
    c.code("image in kernel memory", raw(nr::SPAWN, KERNEL, PAGE, args_at, 0), E_FAULT);
    c.code("image at address 0", raw(nr::SPAWN, 0, PAGE, args_at, 0), E_FAULT);
    c.code("image in unmapped memory", raw(nr::SPAWN, UNMAPPED, PAGE, args_at, 0), E_FAULT);
    c.code("arguments in kernel memory", raw(nr::SPAWN, image_at, image_len, KERNEL, 0), E_FAULT);
    c.code("arguments at address 0", raw(nr::SPAWN, image_at, image_len, 0, 0), E_FAULT);
    c.code("arguments past user space", raw(nr::SPAWN, image_at, image_len, USER_END - 8, 0), E_FAULT);
    test.done("spawn checks the addresses of the image and of its arguments", c);

    let _ = sys::cap_drop(ep);
    Some(())
}

/// A capability gives exactly the access it describes, and copies never give more.
fn capabilities(test: &mut Test, kids: &mut Kids) -> Option<()> {
    let me = kids.me;
    let ep = sys::ep_create().ok()?;
    let mut msg = Msg::new(0);

    let mut c = Check::new();
    for slot in [EMPTY, CAP_SLOTS as u32, 1_000_000, 0xFFFF_FFFF] {
        c.code("call", raw(nr::CALL, slot, SCRATCH, 0, 0), E_NOCAP);
        c.code("recv", raw(nr::RECV, slot, SCRATCH, 0, 0), E_NOCAP);
        c.code("cap_derive", raw(nr::CAP_DERIVE, slot, 1, 0, 0), E_NOCAP);
        c.code("cap_drop", raw(nr::CAP_DROP, slot, 0, 0, 0), E_NOCAP);
        c.code("map_mem", raw(nr::MAP_MEM, slot, MEMORY, 0, 1), E_NOCAP);
        c.code("io_in", raw(nr::IO_IN, slot, 0x80, 1, 0), E_NOCAP);
        c.code("io_out", raw(nr::IO_OUT, slot, 0x80, 1, 0), E_NOCAP);
        c.code("irq_bind", raw(nr::IRQ_BIND, slot, 5, ep, 0), E_NOCAP);
        c.code("irq_bind to the slot", raw(nr::IRQ_BIND, IRQ, 5, slot, 0), E_NOCAP);
        c.code("irq_ack", raw(nr::IRQ_ACK, slot, 5, 0, 0), E_NOCAP);
        c.code("task_kill", raw(nr::TASK_KILL, slot, 0, 0, 0), E_NOCAP);
        c.code("log", raw(nr::LOG, slot, SCRATCH, 4, 0), E_NOCAP);
        c.code("sysinfo", raw(nr::SYSINFO, slot, INFO_STATS, 0, SCRATCH), E_NOCAP);
    }
    let mut spawn = kids.spawn_args(mode::EXIT, 0, 0, &[EMPTY], CHILD_PAGES, CHILD_STACK);
    let image = kids.image.bytes();
    c.fails("spawn that hands over an empty slot", sys::spawn(image, &spawn), E_NOCAP);
    spawn.caps[0] = 1_000_000;
    c.fails("spawn that hands over slot 1000000", sys::spawn(image, &spawn), E_NOCAP);
    spawn.caps[0] = NO_CAP;
    spawn.fault_ep = EMPTY;
    c.fails("spawn with an empty slot for the end notice", sys::spawn(image, &spawn), E_NOCAP);
    test.done("an empty or impossible capability slot gives E_NOCAP", c);

    let mut c = Check::new();
    c.code("call on Info", raw(nr::CALL, INFO, SCRATCH, 0, 0), E_NOCAP);
    c.code("recv on an I/O port", raw(nr::RECV, PORT, SCRATCH, 0, 0), E_NOCAP);
    c.code("map_mem on an endpoint", raw(nr::MAP_MEM, CONSOLE, MEMORY, 0, 1), E_NOCAP);
    c.code("io_in on memory", raw(nr::IO_IN, ARCHIVE, 0x80, 1, 0), E_NOCAP);
    c.code("io_out on an interrupt", raw(nr::IO_OUT, IRQ, 0x80, 1, 0), E_NOCAP);
    c.code("irq_bind on an I/O port", raw(nr::IRQ_BIND, PORT, 5, ep, 0), E_NOCAP);
    c.code("irq_bind to the log", raw(nr::IRQ_BIND, IRQ, 5, LOG, 0), E_NOCAP);
    c.code("irq_ack on the log", raw(nr::IRQ_ACK, LOG, 5, 0, 0), E_NOCAP);
    c.code("task_kill on an endpoint", raw(nr::TASK_KILL, CONSOLE, 0, 0, 0), E_NOCAP);
    c.code("log on Info", raw(nr::LOG, INFO, SCRATCH, 4, 0), E_NOCAP);
    c.code("sysinfo on the log", raw(nr::SYSINFO, LOG, INFO_STATS, 0, SCRATCH), E_NOCAP);
    test.done("a capability of the wrong kind gives E_NOCAP", c);

    let mut c = Check::new();
    let send = sys::cap_derive(ep, R_SEND, 0, 0).ok()?;
    let receive = sys::cap_derive(ep, R_RECV, 0, 0).ok()?;
    c.eq("rights of the send copy", cap(me, send).b, R_SEND);
    c.eq("rights of the receive copy", cap(me, receive).b, R_RECV);
    c.fails("adding the receive right", sys::cap_derive(send, R_SEND | R_RECV, 0, 0), E_PERM);
    c.fails("adding the grant right", sys::cap_derive(send, R_SEND | R_GRANT, 0, 0), E_PERM);
    c.fails("swapping send for receive", sys::cap_derive(send, R_RECV, 0, 0), E_PERM);
    c.fails("a copy with no rights", sys::cap_derive(ep, 0, 0, 0), E_PERM);
    c.fails("a right that does not exist", sys::cap_derive(ep, 8, 0, 0), E_PERM);
    c.fails("the receive right on the console", sys::cap_derive(CONSOLE, R_RECV, 0, 0), E_PERM);
    c.fails("recv through the send copy", sys::recv(send, &mut msg, 0), E_PERM);
    c.fails("call through the receive copy", sys::call(receive, &mut msg, 0), E_PERM);
    spawn.fault_ep = send;
    c.fails("end notices through the send copy", sys::spawn(image, &spawn), E_PERM);
    c.works("dropping the send copy", sys::cap_drop(send));
    c.works("dropping the receive copy", sys::cap_drop(receive));
    test.done("a copy of a capability has the same rights or fewer", c);

    let mut c = Check::new();
    let badged = sys::cap_derive(ep, R_SEND | R_GRANT, 7, 0).ok()?;
    let plain = sys::cap_derive(ep, R_SEND, 0, 0).ok()?;
    c.eq("badge", cap(me, badged).c, 7);
    c.fails("a second badge", sys::cap_derive(badged, R_SEND, 9, 0), E_PERM);
    c.fails("a badge without the grant right", sys::cap_derive(plain, R_SEND, 7, 0), E_PERM);
    for badge in [0, 7] {
        let copy = c.works("copying a badged capability", sys::cap_derive(badged, R_SEND, badge, 0));
        if let Some(copy) = copy {
            c.eq("badge of the copy", cap(me, copy).c, 7);
            c.works("dropping the copy", sys::cap_drop(copy));
        }
    }
    c.works("dropping the badged capability", sys::cap_drop(badged));
    c.works("dropping the plain capability", sys::cap_drop(plain));
    test.done("a badge is set once, and only by a holder of the grant right", c);

    let mut c = Check::new();
    let gone = sys::cap_derive(ep, R_SEND, 0, 0).ok()?;
    c.works("dropping", sys::cap_drop(gone));
    c.fails("call after the drop", sys::call(gone, &mut msg, 0), E_NOCAP);
    c.fails("a second drop", sys::cap_drop(gone), E_NOCAP);
    test.done("a dropped capability is gone", c);

    let mut c = Check::new();
    c.works("writing port 0x80", sys::io_out(PORT, 0x80, 1, 0x5A));
    c.works("reading port 0x80", sys::io_in(PORT, 0x80, 1));
    let other_ports = [
        (0x81, 1),
        (0x7F, 1),
        (0x60, 1),
        (RESET_PORT, 1),
        (0x80, 2),
        (0x7F, 2),
        (0x7D, 4),
        (0x1_0080, 1),
        (0xFFFF_FFFF, 4),
        (0xFFFF_FFFF, 2),
        (0xFFFF_FFFE, 4),
    ];
    for (port, width) in other_ports {
        c.fails("reading a port outside the capability", sys::io_in(PORT, port, width), E_PERM);
        c.fails("writing a port outside the capability", sys::io_out(PORT, port, width, 0), E_PERM);
    }
    for width in [0, 3, 5, 8, 0xFFFF_FFFF] {
        c.fails("reading with an impossible width", sys::io_in(PORT, 0x80, width), E_INVAL);
        c.fails("writing with an impossible width", sys::io_out(PORT, 0x80, width, 0), E_INVAL);
    }
    let other_ranges = [(0x80, 0x81), (0x7F, 0x80), (0x60, 0x60), (0, 0xFFFF), (0x81, 0x80), (0x64, 0x64)];
    for (first, last) in other_ranges {
        c.fails("a copy for other ports", sys::cap_derive(PORT, first, last, 0), E_PERM);
    }
    if let Some(copy) = c.works("a copy for the same port", sys::cap_derive(PORT, 0x80, 0x80, 0)) {
        c.works("reading through the copy", sys::io_in(copy, 0x80, 1));
        c.works("dropping the copy", sys::cap_drop(copy));
    }
    test.done("an I/O port capability reaches its ports and no others", c);

    let mut c = Check::new();
    let bound = sys::ep_create().ok()?;
    let send = sys::cap_derive(bound, R_SEND, 0, 0).ok()?;
    c.fails("binding to an endpoint it cannot receive from", sys::irq_bind(IRQ, 5, send), E_PERM);
    c.fails("binding line 1", sys::irq_bind(IRQ, 1, bound), E_PERM);
    c.fails("binding line 16", sys::irq_bind(IRQ, 16, bound), E_INVAL);
    c.fails("binding line 0xFFFFFFFF", sys::irq_bind(IRQ, 0xFFFF_FFFF, bound), E_INVAL);
    c.fails("acknowledging a line that is not bound", sys::irq_ack(IRQ, 5), E_PERM);
    c.works("binding line 5", sys::irq_bind(IRQ, 5, bound));
    c.works("acknowledging line 5", sys::irq_ack(IRQ, 5));
    c.fails("acknowledging line 1", sys::irq_ack(IRQ, 1), E_PERM);
    for lines in [0, 1 << 1, (1 << 5) | (1 << 1), 0xFFFF, 1 << 16] {
        c.fails("a copy for other lines", sys::cap_derive(IRQ, lines, 0, 0), E_PERM);
    }
    if let Some(copy) = c.works("a copy for line 5", sys::cap_derive(IRQ, 1 << 5, 0, 0)) {
        c.works("dropping the copy", sys::cap_drop(copy));
    }
    test.done("an interrupt capability reaches its lines and no others", c);

    let mut c = Check::new();
    c.works("dropping the receive right of the bound endpoint", sys::cap_drop(bound));
    c.fails("acknowledging after that", sys::irq_ack(IRQ, 5), E_PERM);
    c.works("dropping the send right", sys::cap_drop(send));
    let fresh = sys::ep_create().ok()?;
    c.works("binding line 5 to a new endpoint", sys::irq_bind(IRQ, 5, fresh));
    c.works("dropping the new endpoint", sys::cap_drop(fresh));
    c.fails("acknowledging at the end", sys::irq_ack(IRQ, 5), E_PERM);
    test.done("an interrupt line is released when its endpoint goes away", c);

    let mut c = Check::new();
    let line = b"selftest: this line was written through the Log capability\n";
    c.works("writing", sys::log(LOG, line));
    test.done("the Log capability writes to the kernel log", c);

    let mut c = Check::new();
    c.yes("this task is called selftest", rt::io::name(&task(me).name) == b"selftest");
    let console = cap(me, CONSOLE);
    c.eq("kind of slot 0", console.kind, CAP_ENDPOINT);
    c.eq("rights of slot 0", console.b, R_SEND);
    c.eq("badge of slot 0", console.c, proto::B_APP);
    c.eq("kind of an empty slot", cap(me, EMPTY).kind, CAP_NULL);
    c.fails("task 32", sys::task_info(INFO, MAX_TASKS as u32), E_INVAL);
    c.fails("task 0xFFFFFFFF", sys::task_info(INFO, 0xFFFF_FFFF), E_INVAL);
    c.fails("capability 32", sys::cap_info(INFO, me, CAP_SLOTS as u32), E_INVAL);
    c.fails("a capability of task 32", sys::cap_info(INFO, MAX_TASKS as u32, 0), E_INVAL);
    test.done("the Info capability describes tasks and their capabilities", c);

    let mut c = Check::new();
    let mut init_holds_it = false;
    for index in 0..MAX_TASKS as u32 {
        if task(index).state == ST_FREE {
            continue;
        }
        for slot in 0..CAP_SLOTS as u32 {
            let found = cap(index, slot);
            if found.kind == CAP_IOPORT && found.a <= RESET_PORT && found.b >= RESET_PORT {
                init_holds_it |= index == 0;
                c.eq("task that can reach port 0x64", index, 0);
            }
        }
    }
    c.yes("init can reach port 0x64", init_holds_it);
    test.done("only init holds a capability for the reset port 0x64", c);

    let mut c = Check::new();
    for server in [CONSOLE, FILES] {
        for label in [proto::DBG_CRASH, proto::DBG_HANG] {
            let mut request = Msg::new(label);
            c.works("asking a driver to fail", sys::call(server, &mut request, 2000));
            c.eq("the driver's answer", request.label, proto::DENIED);
        }
        let mut ping = Msg::new(proto::PING);
        c.works("ping", sys::call(server, &mut ping, 2000));
        c.eq("answer to the ping", ping.label, proto::OK);
    }
    test.done("the drivers refuse to crash or hang for an ordinary program", c);

    let _ = sys::cap_drop(ep);
    Some(())
}

/// Messages arrive whole, in order, with badges and tokens that cannot be forged.
fn ipc(test: &mut Test, kids: &mut Kids) -> Option<()> {
    let ep = sys::ep_create().ok()?;
    let mut msg = Msg::new(0);

    let mut c = Check::new();
    c.fails("poll", sys::recv(ep, &mut msg, 0), E_TIMEOUT);
    let before = sys::time();
    c.fails("recv with a limit of 30 ms", sys::recv(ep, &mut msg, 30), E_TIMEOUT);
    let waited = sys::time() - before;
    c.yes("it waited at least 30 ms", waited >= 30_000);
    c.yes("it waited less than 200 ms", waited < 200_000);
    test.done("recv can poll, and can give up after a time limit", c);

    let mut c = Check::new();
    let mut request = Msg::new(5);
    request.words[0] = 77;
    c.fails("call", sys::call(ep, &mut request, 20), E_TIMEOUT);
    c.yes("the message is untouched", request.label == 5 && request.words[0] == 77);
    c.fails("recv afterwards", sys::recv(ep, &mut msg, 0), E_TIMEOUT);
    test.done("a call that nobody receives times out and leaves nothing queued", c);

    let mut c = Check::new();
    let lonely = sys::ep_create().ok()?;
    let send = sys::cap_derive(lonely, R_SEND, 0, 0).ok()?;
    c.works("dropping the receive right", sys::cap_drop(lonely));
    c.fails("call", sys::call(send, &mut msg, 100), E_DEAD);
    c.works("dropping the send right", sys::cap_drop(send));
    test.done("a call to an endpoint that nobody can receive from gives E_DEAD", c);

    // An echo server in a child. It receives on the endpoint; this program keeps a send
    // right with badge 0x51.
    let server_end = sys::ep_create().ok()?;
    let client = sys::cap_derive(server_end, R_SEND, 0x51, 0).ok()?;
    let server = kids.start(mode::ECHO, 0, 0, &[server_end])?;

    let mut c = Check::new();
    c.yes("the server is waiting for a message", kids.wait_state(server, ST_RECV));
    c.fails("a second recv", sys::recv(server_end, &mut msg, 0), E_BUSY);
    c.works("dropping the receive right", sys::cap_drop(server_end));
    test.done("an endpoint has one receiver at a time", c);

    let mut c = Check::new();
    let mut request = Msg::new(5);
    request.words = [41, 3, 0, 0];
    request.badge = 0x1234;
    request.token = 0x5678;
    c.works("call", sys::call(client, &mut request, 2000));
    c.eq("label of the reply", request.label, 3);
    c.eq("word of the reply", request.words[0], 42);
    c.eq("badge the server saw", request.words[1], 0x51);
    c.eq("label the server saw", request.words[2], 5);
    c.eq("badge in the reply", request.badge, 0);
    c.eq("token in the reply", request.token, 0);
    test.done("a call carries its label, words and badge; a reply carries no badge", c);

    let mut c = Check::new();
    let mut request = Msg::new(L_FAULT);
    request.words = [0, L_IRQ, 0, 0];
    request.len = 1000;
    c.works("call", sys::call(client, &mut request, 2000));
    c.eq("label the server saw", request.words[2], L_FAULT & !L_KERNEL);
    c.eq("length the server saw", request.words[3], MSG_DATA as u32);
    c.eq("label of the reply", request.label, L_IRQ & !L_KERNEL);
    c.eq("length of the reply", request.len, MSG_DATA as u32);
    test.done("a program cannot send the kernel's labels or an oversized message", c);

    let mut c = Check::new();
    let across = (SCRATCH + PAGE - 100) as *mut Msg;
    let mut request = Msg::new(5);
    request.len = MSG_DATA as u32;
    for (index, byte) in (0u32..).zip(request.data.iter_mut()) {
        *byte = pattern(index);
    }
    // SAFETY: SCRATCH is two writable pages that `main` mapped and that are reached only
    // through raw addresses. This address is a multiple of four, and the 288 bytes of a
    // message end 188 bytes into the second page.
    unsafe { across.write_volatile(request) };
    c.code("call", raw(nr::CALL, client, across as u32, 2000, 0), 0);
    // SAFETY: as above; the kernel has written the reply into the same 288 bytes.
    let reply = unsafe { across.read_volatile() };
    c.eq("length of the reply", reply.len, MSG_DATA as u32);
    let inverted = (0u32..).zip(reply.data.iter()).all(|(index, &byte)| byte == !pattern(index));
    c.yes("every byte came back inverted", inverted);
    test.done("a 256-byte message crosses a page boundary in both directions", c);

    let mut c = Check::new();
    let mut stop = Msg::new(proto::ECHO_STOP);
    c.works("call", sys::call(client, &mut stop, 2000));
    c.ended("the server", kids.end_of(server), CAUSE_EXIT, Some(0));
    c.fails("call after the server ended", sys::call(client, &mut msg, 100), E_DEAD);
    c.works("dropping the send right", sys::cap_drop(client));
    test.done("a server can end in an orderly way, and its clients are told", c);

    // Now this program is the server; children call it through a capability badged 0x77.
    let door = sys::cap_derive(ep, R_SEND, 0x77, 0).ok()?;

    let mut c = Check::new();
    let caller = kids.start(mode::CALL_ONCE, 9, 1234, &[door])?;
    c.works("recv", sys::recv(ep, &mut msg, 2000));
    c.eq("label", msg.label, 9);
    c.eq("word", msg.words[0], 1234);
    c.eq("badge", msg.badge, 0x77);
    c.yes("the token has its top bit set", msg.token & 0x8000_0000 != 0);
    let token = msg.token;
    let mut answer = Msg::new(0);
    answer.words[0] = 4321;
    for forged in [token ^ 0x100, token ^ 1, 0, 0x8000_0000 | kids.me] {
        c.fails("a reply with a forged token", sys::reply(forged, &answer), E_INVAL);
    }
    c.works("a reply with the real token", sys::reply(token, &answer));
    c.fails("a second reply with the real token", sys::reply(token, &answer), E_INVAL);
    c.ended("the caller", kids.end_of(caller), CAUSE_EXIT, Some(4321));
    test.done("a reply needs the token of the call, and the token works once", c);

    let mut c = Check::new();
    let mut callers = [None; 3];
    for (number, caller) in (1u32..).zip(callers.iter_mut()) {
        let kid = kids.start(mode::CALL_ONCE, 9, number, &[door])?;
        c.yes("the caller is waiting to be received", kids.wait_state(kid, ST_SEND));
        *caller = Some(kid);
    }
    for number in 1..=3 {
        c.works("recv", sys::recv(ep, &mut msg, 2000));
        c.eq("caller that is served", msg.words[0], number);
        let mut answer = Msg::new(0);
        answer.words[0] = 100 + number;
        c.works("reply", sys::reply(msg.token, &answer));
    }
    for (number, caller) in (1u32..).zip(callers) {
        let end = caller.and_then(|kid| kids.end_of(kid));
        c.ended("a caller", end, CAUSE_EXIT, Some(100 + number));
    }
    test.done("callers are served in the order in which they called", c);

    let mut c = Check::new();
    let held = sys::ep_create().ok()?;
    let send = sys::cap_derive(held, R_SEND, 0, 0).ok()?;
    let server = kids.start(mode::RECV_ONCE, 0, 0, &[held])?;
    c.works("leaving the receive right to the server alone", sys::cap_drop(held));
    c.fails("call", sys::call(send, &mut msg, 2000), E_DEAD);
    c.ended("the server", kids.end_of(server), CAUSE_EXIT, Some(7));
    c.works("dropping the send right", sys::cap_drop(send));
    test.done("a caller is told when the server dies with its request", c);

    let mut c = Check::new();
    let caller = kids.start(mode::CALL_ONCE, 9, 1, &[door])?;
    c.yes("the caller is waiting to be received", kids.wait_state(caller, ST_SEND));
    c.ended("the caller", kids.kill(caller), CAUSE_KILLED, None);
    c.fails("recv afterwards", sys::recv(ep, &mut msg, 0), E_TIMEOUT);
    test.done("a caller that is killed while it waits leaves nothing queued", c);

    let mut c = Check::new();
    let caller = kids.start(mode::CALL_ONCE, 9, 1, &[door])?;
    c.works("recv", sys::recv(ep, &mut msg, 2000));
    c.ended("the caller", kids.kill(caller), CAUSE_KILLED, None);
    c.fails("reply", sys::reply(msg.token, &Msg::new(0)), E_INVAL);
    test.done("a reply to a caller that was killed gives E_INVAL", c);

    let _ = sys::cap_drop(door);
    let _ = sys::cap_drop(ep);
    Some(())
}

/// Memory is zeroed, bounded by a quota, and kept inside user space.
fn memory(test: &mut Test, kids: &mut Kids) -> Option<()> {
    let me = kids.me;

    let mut c = Check::new();
    let pages = Pages::map(MEMORY, 2).ok()?;
    c.yes("new pages are zero", pages.holds(|_| 0));
    pages.fill(pattern);
    c.yes("they hold what was written", pages.holds(pattern));
    test.done("map gives zeroed pages that can be written", c);

    let mut c = Check::new();
    let charged = task(me).charged;
    c.fails("mapping a page twice", sys::map(MEMORY, 1, true), E_BUSY);
    let obstacle = Pages::map(MEMORY + 6 * PAGE, 1).ok()?;
    c.fails("mapping three pages, the last one taken", sys::map(MEMORY + 4 * PAGE, 3, true), E_BUSY);
    c.eq("pages charged after the failures", task(me).charged, charged + 1);
    match Pages::map(MEMORY + 4 * PAGE, 2) {
        Ok(free) => {
            c.works("unmapping the two free pages", free.unmap());
        }
        Err(error) => c.code("mapping the two free pages", error, 0),
    }
    c.works("unmapping the third page", obstacle.unmap());
    c.yes("the first pages are untouched", pages.holds(pattern));
    c.eq("pages charged at the end", task(me).charged, charged);
    test.done("a map that fails part-way changes nothing", c);

    let mut c = Check::new();
    let free_area = MEMORY + 16 * PAGE;
    let outside = [
        (KERNEL, 1),
        (0, 1),
        (VIDEO, 1),
        (USER_END, 1),
        (USER_END - PAGE, 2),
        (USER_BASE - PAGE, 2),
        (0xFFFF_F000, 1),
        (free_area + 1, 1),
        (free_area, 0),
        (free_area, 0xFFFF_FFFF),
        (free_area, 0x10_0000),
    ];
    for (address, count) in outside {
        c.code("map", raw(nr::MAP, address, count, 1, 0), E_INVAL);
        c.code("unmap", raw(nr::UNMAP, address, count, 0, 0), E_INVAL);
    }
    test.done("map and unmap accept only whole pages inside user space", c);

    let mut c = Check::new();
    let free = stats().free_frames;
    let charged = task(me).charged;
    let more = Pages::map(MEMORY + 8 * PAGE, 8).ok()?;
    c.eq("free frames while mapped", stats().free_frames, free - 8);
    c.eq("pages charged while mapped", task(me).charged, charged + 8);
    c.works("unmapping", more.unmap());
    c.eq("free frames afterwards", stats().free_frames, free);
    c.eq("pages charged afterwards", task(me).charged, charged);
    test.done("unmap gives every page frame back", c);

    let mut c = Check::new();
    let info = task(me);
    let too_many = info.quota - info.charged + 1;
    c.fails("mapping one page more than the quota allows", sys::map(QUOTA_AT, too_many, true), E_NOMEM);
    let after = task(me).charged;
    let kept = after.wrapping_sub(info.charged);
    c.yes("at most 16 pages, for page tables, stay charged", kept <= 16);
    match Pages::map(QUOTA_AT, 4) {
        Ok(usable) => {
            usable.fill(pattern);
            c.yes("memory mapped afterwards works", usable.holds(pattern));
            c.works("unmapping", usable.unmap());
        }
        Err(error) => c.code("mapping four pages afterwards", error, 0),
    }
    test.done("a task cannot take more memory than its quota", c);

    let mut c = Check::new();
    let archive_pages = cap(me, ARCHIVE).b;
    let spare = MEMORY + 32 * PAGE;
    let mapped = sys::map_mem(ARCHIVE, ARCHIVE_AT, 0, archive_pages, false);
    if c.works("mapping the archive", mapped).is_some() {
        // SAFETY: the kernel has just mapped the archive's pages here, readable, and it
        // never takes away pages that were mapped from a Memory capability.
        let start = unsafe { slice::from_raw_parts(ARCHIVE_AT as *const u8, ARCHIVE_MAGIC.len()) };
        c.yes("it starts with INTIARCH", start == &ARCHIVE_MAGIC[..]);
    }
    c.fails("a writable mapping", sys::map_mem(ARCHIVE, spare, 0, 1, true), E_PERM);
    let refused = [
        (spare, archive_pages, 1),
        (spare, 0, archive_pages + 1),
        (spare, 0xFFFF_FFFF, 1),
        (spare, 1, 0x7FFF_FFFF),
        (spare, 0, 0),
        (KERNEL, 0, 1),
        (spare + 1, 0, 1),
    ];
    for (address, offset, count) in refused {
        c.code("map_mem", raw(nr::MAP_MEM, ARCHIVE, address, offset, count), E_INVAL);
    }
    test.done("shared memory keeps the rights and the bounds of its capability", c);

    let mut c = Check::new();
    let beyond = [
        (0, 1, 1),
        (0, archive_pages + 1, 0),
        (archive_pages, 1, 0),
        (0, 0, 0),
        (0xFFFF_FFFF, 2, 0),
        (1, 0xFFFF_FFFF, 0),
    ];
    for (offset, count, writable) in beyond {
        c.fails("a copy that reaches further", sys::cap_derive(ARCHIVE, offset, count, writable), E_PERM);
    }
    if let Some(part) = c.works("a copy for the first page", sys::cap_derive(ARCHIVE, 0, 1, 0)) {
        c.eq("pages of the copy", cap(me, part).b, 1);
        c.fails("mapping a second page through it", sys::map_mem(part, spare, 1, 1, false), E_INVAL);
        c.fails("mapping two pages through it", sys::map_mem(part, spare, 0, 2, false), E_INVAL);
        if c.works("mapping its one page", sys::map_mem(part, spare, 0, 1, false)).is_some() {
            // SAFETY: nothing refers to the page that was just mapped at `spare`.
            c.works("unmapping it", unsafe { sys::unmap(spare, 1) });
        }
        c.works("dropping the copy", sys::cap_drop(part));
    }
    c.works("unmapping the first pages", pages.unmap());
    test.done("a copy of a memory capability covers the same pages or fewer", c);
    Some(())
}

/// How each kind of misbehaving child must end: name, mode, cause, exit code or address.
const ENDS: [(&str, u32, u32, Option<u32>); 14] = [
    ("the exit code of a child reaches its parent", mode::EXIT, CAUSE_EXIT, Some(42)),
    ("a panic ends the program that panicked", mode::PANIC, CAUSE_EXIT, Some(0xDEAD)),
    ("reading address 4 is a page fault at address 4", mode::NULL_READ, PAGE_FAULT, Some(4)),
    ("a program cannot write to its own code", mode::WRITE_CODE, PAGE_FAULT, None),
    ("a program cannot read kernel memory", mode::KERNEL_READ, PAGE_FAULT, Some(KERNEL)),
    ("a program cannot write kernel memory", mode::KERNEL_WRITE, PAGE_FAULT, Some(KERNEL)),
    ("a program cannot jump into the kernel", mode::KERNEL_JUMP, PAGE_FAULT, Some(KERNEL)),
    ("a stack overflow ends the program with a page fault", mode::STACK_OVERFLOW, PAGE_FAULT, None),
    ("a division by zero ends the program", mode::DIVIDE, 0, None),
    ("an invalid instruction ends the program", mode::UD2, 6, None),
    ("a program cannot switch interrupts off", mode::CLI, PROTECTION_FAULT, None),
    ("a program cannot halt the processor", mode::HLT, PROTECTION_FAULT, None),
    ("a program cannot use I/O ports without a capability", mode::PORT_IN, PROTECTION_FAULT, None),
    ("floating-point instructions are refused", mode::FPU, 7, None),
];

/// Tasks end without taking anything with them, and the kernel's tables fill and recover.
fn tasks(test: &mut Test, kids: &mut Kids) -> Option<()> {
    let me = kids.me;
    kids.run(mode::EXIT, 0, 0)?;
    let before = usage(me);
    let faults = stats().faults;
    for (name, what, cause, detail) in ENDS {
        let mut c = Check::new();
        let end = kids.run(what, 42, 0);
        c.ended("the child", end, cause, detail);
        if what == mode::KERNEL_JUMP {
            c.eq("address of the instruction", end.map_or(0, |end| end.eip), KERNEL);
        }
        test.done(name, c);
    }

    let mut c = Check::new();
    c.same(usage(me), before);
    c.eq("faults the kernel counted", stats().faults, faults + 12);
    test.done("fourteen children ended, twelve by a fault, and left nothing behind", c);

    let mut c = Check::new();
    let spinner = kids.start(mode::SPIN, 0, 0, &[])?;
    let index = kids.index_of(spinner);
    sys::sleep(20);
    c.yes("the child that never yields used the processor", task(index).cpu_ticks > 0);
    c.works("killing it", sys::task_kill(spinner.task));
    c.ended("the child", kids.end_of_badge(spinner.badge, 3000), CAUSE_KILLED, None);
    c.fails("killing it again", sys::task_kill(spinner.task), E_DEAD);
    let next = kids.start(mode::SLEEP, 300, 5, &[])?;
    c.eq("place of the next child in the task table", kids.index_of(next), index);
    c.fails("the old capability, with the place reused", sys::task_kill(spinner.task), E_DEAD);
    c.ended("the next child", kids.end_of(next), CAUSE_EXIT, Some(5));
    c.works("dropping the old capability", sys::cap_drop(spinner.task));
    test.done("a task that never yields is pre-empted and can be killed, once", c);

    let mut c = Check::new();
    let tasks_before = stats().tasks;
    let archive_pages = cap(me, ARCHIVE).b;
    let parent = kids.start_with(mode::GRANDCHILD, archive_pages, 0, &[NO_CAP, ARCHIVE], 96).ok()?;
    c.yes("a child and its own child are running", wait_until(|| stats().tasks == tasks_before + 2));
    c.ended("the child", kids.kill(parent), CAUSE_KILLED, None);
    c.yes("both are gone", wait_until(|| stats().tasks == tasks_before));
    test.done("the children of a task end with it", c);

    let mut c = Check::new();
    let spare = spare_pages(me);
    let child = kids.start_with(mode::SLEEP, 5000, 0, &[], CHILD_PAGES).ok()?;
    c.eq("spare pages while the child lives", spare_pages(me), spare - CHILD_PAGES);
    c.ended("the child", kids.kill(child), CAUSE_KILLED, None);
    c.eq("spare pages afterwards", spare_pages(me), spare);
    let spawn = kids.spawn_args(mode::EXIT, 0, 0, &[], spare + 1, CHILD_STACK);
    c.fails("a child with a larger quota than is spare", sys::spawn(kids.image.bytes(), &spawn), E_NOMEM);
    test.done("the memory of a child comes out of its parent's quota and goes back", c);

    // A copy of the child's image that can be damaged, one field at a time.
    let size = kids.image.bytes().len();
    let copy_pages = (size as u32).div_ceil(PAGE);
    sys::map(COPY_AT, copy_pages, true).ok()?;
    // SAFETY: the pages were mapped just above, are writable and hold at least `size`
    // bytes. This slice is the only way they are reached until they are unmapped below.
    let copy = unsafe { slice::from_raw_parts_mut(COPY_AT as *mut u8, size) };
    copy.copy_from_slice(kids.image.bytes());
    let load = first_load_header(copy)?;
    let memory_size = word(copy, load + 20);
    let header_counts = word(copy, 44) & 0xFFFF_0000;

    let mut c = Check::new();
    let before = usage(me);
    let spare = spare_pages(me);
    let damage = [
        ("entry point in the kernel", 24, KERNEL),
        ("entry point past user space", 24, USER_END),
        ("segment in the kernel", load + 8, KERNEL),
        ("segment at the top of user space", load + 8, USER_END - PAGE),
        ("segment size that wraps around", load + 20, 0xFFFF_FFFF),
        ("more bytes in the file than in memory", load + 16, memory_size + 1),
        ("segment data past the end of the image", load + 4, 0xFFFF_FFF0),
        ("no program headers", 44, header_counts),
        ("seventeen program headers", 44, header_counts | 17),
        ("wrong magic number", 0, 0x464C_457E),
        ("wrong machine", 16, 0x0028_0002),
    ];
    for (what, at, value) in damage {
        let original = word(copy, at);
        set_word(copy, at, value);
        kids.refuse(&mut c, what, copy, CHILD_PAGES, CHILD_STACK, E_INVAL);
        set_word(copy, at, original);
    }
    kids.refuse(&mut c, "only 40 bytes", &copy[..40], CHILD_PAGES, CHILD_STACK, E_INVAL);
    kids.refuse(&mut c, "only 51 bytes", &copy[..51], CHILD_PAGES, CHILD_STACK, E_INVAL);
    kids.refuse(&mut c, "the first half", &copy[..size / 2], CHILD_PAGES, CHILD_STACK, E_INVAL);
    kids.refuse(&mut c, "the second half", &copy[size / 2..], CHILD_PAGES, CHILD_STACK, E_INVAL);
    kids.refuse(&mut c, "no stack", copy, CHILD_PAGES, 0, E_INVAL);
    kids.refuse(&mut c, "a stack of 257 pages", copy, 1024, 257, E_INVAL);
    kids.refuse(&mut c, "a quota of two pages", copy, 2, CHILD_STACK, E_NOMEM);
    c.same(usage(me), before);
    c.eq("spare pages", spare_pages(me), spare);
    test.done("damaged program images are refused and leave nothing behind", c);

    let mut c = Check::new();
    let spawn = kids.spawn_args(mode::EXIT, 33, 0, &[], CHILD_PAGES, CHILD_STACK);
    match sys::spawn(copy, &spawn) {
        Ok(task) => {
            let end = kids.end_of(Kid { task, badge: spawn.fault_badge });
            c.ended("the child", end, CAUSE_EXIT, Some(33));
        }
        Err(error) => c.code("spawn", error, 0),
    }
    // SAFETY: `copy` is not used after this line, and nothing else refers to its pages.
    c.works("unmapping the copy", unsafe { sys::unmap(COPY_AT, copy_pages) });
    test.done("the same image, repaired, starts and runs", c);

    let mut c = Check::new();
    let mut copies = [NO_CAP; CAP_SLOTS];
    let mut count = 0;
    let error = loop {
        match sys::cap_derive(CONSOLE, R_SEND, 0, 0) {
            Ok(slot) if count < CAP_SLOTS => {
                copies[count] = slot;
                count += 1;
            }
            Ok(_) => break 0,
            Err(error) => break error,
        }
    };
    c.code("a copy when every slot is taken", error, E_NOSLOT);
    c.fails("a new endpoint", sys::ep_create(), E_NOSLOT);
    let spawn = kids.spawn_args(mode::EXIT, 0, 0, &[], CHILD_PAGES, CHILD_STACK);
    c.fails("a new child", sys::spawn(kids.image.bytes(), &spawn), E_NOSLOT);
    for &slot in &copies[..count] {
        c.works("dropping a copy", sys::cap_drop(slot));
    }
    let again = sys::cap_derive(CONSOLE, R_SEND, 0, 0).and_then(sys::cap_drop);
    c.works("a copy after the drops", again);
    test.done("a full capability table gives E_NOSLOT and recovers", c);

    let mut c = Check::new();
    let endpoints_before = stats().endpoints;
    let reports = sys::ep_create().ok()?;
    let door = sys::cap_derive(reports, R_SEND, 0, 0).ok()?;
    let mut makers = [None; 3];
    let mut made = 0;
    let mut msg = Msg::new(0);
    for maker in makers.iter_mut() {
        *maker = kids.start(mode::ENDPOINTS, 0, 0, &[door]);
        if c.works("a report from a child", sys::recv(reports, &mut msg, 3000)).is_some() {
            made += msg.words[0];
            c.eq("why the child stopped", msg.words[1], E_NOSLOT.unsigned_abs());
            c.works("reply", sys::reply(msg.token, &Msg::new(0)));
        }
    }
    let all = MAX_ENDPOINTS as u32;
    c.eq("endpoints the children made", made, all - endpoints_before - 1);
    c.eq("endpoints in use", stats().endpoints, all);
    c.fails("one more endpoint", sys::ep_create(), E_NOSLOT);
    for maker in makers.into_iter().flatten() {
        c.ended("a child", kids.kill(maker), CAUSE_KILLED, None);
    }
    c.works("dropping the send right", sys::cap_drop(door));
    c.works("dropping the endpoint", sys::cap_drop(reports));
    c.eq("endpoints in use afterwards", stats().endpoints, endpoints_before);
    c.works("a new endpoint afterwards", sys::ep_create().and_then(sys::cap_drop));
    test.done("a full endpoint table gives E_NOSLOT and recovers", c);

    let mut c = Check::new();
    let tasks_before = stats().tasks;
    let first_badge = kids.launches + 1;
    let mut started = 0;
    let error = loop {
        let spawn = kids.spawn_args(mode::SLEEP, 500, 0, &[], 24, 2);
        match sys::spawn(kids.image.bytes(), &spawn) {
            Ok(task) if started < MAX_TASKS as u32 => {
                started += 1;
                let _ = sys::cap_drop(task);
            }
            Ok(_) => break 0,
            Err(error) => break error,
        }
    };
    c.code("a child when every place is taken", error, E_NOSLOT);
    c.eq("tasks", stats().tasks, MAX_TASKS as u32);
    c.eq("children started", started, MAX_TASKS as u32 - tasks_before);
    for badge in first_badge..first_badge + started {
        c.ended("a child", kids.end_of_badge(badge, 3000), CAUSE_EXIT, Some(0));
    }
    c.eq("tasks afterwards", stats().tasks, tasks_before);
    c.ended("one more child", kids.run(mode::EXIT, 9, 0), CAUSE_EXIT, Some(9));
    test.done("a full task table gives E_NOSLOT and recovers", c);
    Some(())
}

/// The clock and the file server.
fn time_and_files(test: &mut Test, _kids: &mut Kids) -> Option<()> {
    let mut c = Check::new();
    for ms in [1, 10, 25] {
        let before = sys::time();
        sys::sleep(ms);
        c.yes("sleep lasts at least as long as asked", sys::time() - before >= u64::from(ms) * 1000);
    }
    let before = sys::time();
    sys::sleep(20);
    c.yes("a sleep of 20 ms is over within 200 ms", sys::time() - before < 200_000);
    test.done("sleep lasts as long as asked, and not much longer", c);

    let mut c = Check::new();
    let first = sys::time();
    let mut last = first;
    let mut backwards = 0;
    for _ in 0..1000 {
        let now = sys::time();
        if now < last {
            backwards += 1;
        }
        last = now;
    }
    c.eq("times the clock went backwards", backwards, 0);
    c.yes("the clock moved", last > first);
    test.done("the clock never goes backwards", c);

    let mut c = Check::new();
    let mut text = [0u8; 64];
    let count = client::read(FILES, b"readme.txt", 0, &mut text);
    c.yes("readme.txt has text", count.is_some_and(|count| count > 0));
    c.yes("a file that does not exist is not found", client::stat(FILES, b"no-such-file").is_none());
    test.done("files are read through the file server", c);
    Some(())
}

type Group = fn(&mut Test, &mut Kids) -> Option<()>;

#[no_mangle]
pub fn main() {
    set_output(Sink::Console(CONSOLE));
    outln!("Each line is one promise of the kernel, checked from user space.");
    let mut test = Test { passed: 0, total: 0 };
    test.check("the kernel's invariants hold at the start", sys::check(INFO) == Ok(0));
    test.check("the kernel has met nothing unexpected so far", stats().anomalies == 0);
    let scratch = sys::map(SCRATCH, 2, true).and_then(|()| sys::map(READ_ONLY, 1, false));
    match (Kids::new(), scratch) {
        (Some(mut kids), Ok(())) => {
            let before = usage(kids.me);
            let groups: [(&str, Group); 6] = [
                ("argument", arguments),
                ("capability", capabilities),
                ("message", ipc),
                ("memory", memory),
                ("task", tasks),
                ("clock and file", time_and_files),
            ];
            for (name, group) in groups {
                if group(&mut test, &mut kids).is_none() {
                    outln!("       the ", name, " checks stopped early: a step that prepares one failed");
                    test.check(name, false);
                }
            }
            let mut c = Check::new();
            c.code("rule that is broken", sys::check(INFO).map_or(-1, |rule| rule as i32), 0);
            test.done("the kernel's invariants hold at the end", c);
            let mut c = Check::new();
            c.eq("anomalies", stats().anomalies, 0);
            test.done("the kernel met nothing unexpected", c);
            let mut c = Check::new();
            c.same(usage(kids.me), before);
            test.done("no task, endpoint or page frame was lost", c);
        }
        _ => test.check("loading the child program and mapping scratch pages", false),
    }
    if test.passed == test.total {
        outln!("SELFTEST: PASS ", test.passed, "/", test.total);
    } else {
        outln!("SELFTEST: FAIL ", test.passed, "/", test.total);
    }
}
