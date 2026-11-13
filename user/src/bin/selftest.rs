//! selftest: checks from user space that the kernel keeps its promises.
//!
//! Every check prints one line, `  ok   <name>` or `  FAIL <name>`. A failed check is
//! preceded by lines that say what was expected and what happened. A check that this
//! processor cannot make prints `  skip <name>` and counts neither way. The last line is
//! `SELFTEST: PASS n/n`, with the number of skipped checks if there are any, or
//! `SELFTEST: FAIL p/n`.
//!
//! Capabilities, handed over by the shell: 0 console, 1 files, 2 Info, 3 I/O port 0x80,
//! 4 the archive (read-only memory), 5 IRQ 5, 6 Log, 7 one writable page of device memory
//! (the last page of the text screen's memory, which the screen never shows).
#![no_std]
#![no_main]

use core::slice;

use rt::abi::{
    nr, CapInfo, KStats, Msg, SpawnArgs, TaskInfo, ARCHIVE_MAGIC, CAP_ENDPOINT, CAP_IOPORT,
    CAP_IRQ, CAP_MEMORY, CAP_NULL, CAP_SLOTS, CAUSE_EXIT, CAUSE_KILLED, E_BUSY, E_DEAD, E_FAULT, E_INVAL, E_NOCAP,
    E_NOMEM, E_NOSLOT, E_NOSYS, E_PERM, E_TIMEOUT, INFO_CAP, INFO_STATS, INFO_TASK, L_FAULT,
    L_IRQ, L_KERNEL, MAP_WRITE, MAX_TASKS, MSG_DATA, NO_CAP, PAGE, R_GRANT, R_RECV, R_SEND,
    STACK_TOP, ST_FREE, ST_RECV, ST_REPLY, ST_SEND, USER_BASE, USER_END,
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
const DEVICE: u32 = 7;
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
/// Where the sweep over all free page frames maps its two batches, and the most pages in
/// a batch. A batch has to span more than one 32-frame word of the kernel's frame map.
const SWEEP_AT: u32 = 0x8800_0000;
const SWEEP_GAP: u32 = 0x20_0000;
const SWEEP_BATCH: u32 = 256;
const SWEEP_LEAST: u32 = 64;
/// An address in a child's address space that the child program itself never uses.
const CHILD_SPARE: u32 = 0x7800_0000;
/// Where a child maps the page that it then tries to run (`PLAY` in tchild.rs).
const CHILD_PLAY: u32 = 0x6100_0000;
/// The kernel is loaded at 1 MiB; the text screen is at 0xB8000.
const KERNEL: u32 = 0x10_0000;
const VIDEO: u32 = 0xB_8000;

/// Memory quota and stack of an ordinary child.
const CHILD_PAGES: u32 = 48;
const CHILD_STACK: u32 = 4;
/// CPU exception numbers.
const PAGE_FAULT: u32 = 14;
const PROTECTION_FAULT: u32 = 13;
/// The error code of a page fault when a program fetches an instruction from a page that
/// is mapped but may not be executed: present, user mode, instruction fetch.
const EXECUTE_FAULT: u32 = 0x15;
/// The error code of a page fault when a program reads a page that is not mapped: not
/// present, user mode, a read.
const ABSENT_READ: u32 = 0x4;
/// The keyboard controller's command port; writing 0xFE to it resets the machine.
const RESET_PORT: u32 = 0x64;
/// The exit code of a child whose capability in slot 0 was taken away while it waited.
const GONE: u32 = 0xE000_0000 | E_NOCAP.unsigned_abs();

/// The running score.
struct Test {
    passed: u32,
    total: u32,
    /// Checks that this processor cannot make. They count neither as passed nor as failed.
    skipped: u32,
}

impl Test {
    fn skip(&mut self, name: &str) {
        self.skipped += 1;
        outln!("  skip ", name);
    }

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
    /// The error code that the processor gave with a fault.
    code: u32,
}

/// What must be the same before and after a group of checks: tasks, endpoints, page
/// frames (the free ones plus the ones this program holds itself), and what is left of
/// this program's quotas of tasks and endpoints.
#[derive(Clone, Copy)]
struct Usage {
    tasks: u32,
    endpoints: u32,
    frames: u32,
    task_quota: u32,
    endpoint_quota: u32,
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
        self.eq("quota of tasks", now.task_quota, before.task_quota);
        self.eq("quota of endpoints", now.endpoint_quota, before.endpoint_quota);
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

/// The number of the first kernel invariant that does not hold: 0 if all hold, -1 if the
/// kernel could not be asked.
fn broken_rule() -> i32 {
    sys::check(INFO).map_or(-1, |rule| rule as i32)
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
    let (now, mine) = (stats(), task(me));
    Usage {
        tasks: now.tasks,
        endpoints: now.endpoints,
        frames: now.free_frames + mine.charged,
        task_quota: mine.tasks,
        endpoint_quota: mine.endpoints,
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

/// Fills this program's capability table with copies of its console capability. Returns
/// the copies, how many there are, and the error that ended the filling.
fn fill_table() -> ([u32; CAP_SLOTS], usize, i32) {
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
    (copies, count, error)
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
    /// The quotas of tasks and endpoints that the next child is lent.
    lent: (u32, u32),
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
            lent: (0, 0),
            me,
            ended: [End { badge: 0, cause: 0, detail: 0, eip: 0, code: 0 }; 32],
            count: 0,
        })
    }

    /// Lends the child that is started next a share of this program's quotas.
    fn lend(&mut self, tasks: u32, endpoints: u32) {
        self.lent = (tasks, endpoints);
    }

    /// Arguments for a child whose end is reported under a badge of its own.
    fn spawn_args(&mut self, what: u32, a: u32, b: u32, caps: &[u32], quota: u32, stack: u32) -> SpawnArgs {
        self.launches += 1;
        let mut spawn = SpawnArgs::new(b"tchild", quota, stack);
        spawn.fault_ep = self.notices;
        spawn.fault_badge = self.launches;
        (spawn.tasks, spawn.endpoints) = core::mem::take(&mut self.lent);
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
                    code: msg.words[3],
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

/// Asks the agent behind `via` to make system call `number` with `args`. The request takes
/// the capability in this program's slot `slot` along.
fn ask(via: u32, number: u32, args: [u32; 4], slot: u32) -> Result<Msg, i32> {
    let mut msg = Msg::new(number);
    msg.words = args;
    msg.cap = slot;
    sys::call(via, &mut msg, 2000)?;
    Ok(msg)
}

/// Asks the agent behind `via` for the capability in its slot `slot`. `cap` of the answer
/// is the slot of this program that the copy went to. If the kernel refused to hand it
/// over, `cap` is `NO_CAP` and `words[3]` holds the error.
fn fetch(via: u32, slot: u32) -> Result<Msg, i32> {
    let mut msg = Msg::new(nr::YIELD);
    msg.len = 4;
    msg.data[..4].copy_from_slice(&slot.to_le_bytes());
    sys::call(via, &mut msg, 2000)?;
    Ok(msg)
}

/// A child that makes the system calls it is asked to make, so that a check can see what
/// another task may and may not do.
struct Agent {
    kid: Kid,
    /// This program's capability for the endpoint on which the agent takes requests.
    ep: u32,
    /// The agent's number in the task table.
    index: u32,
}

impl Agent {
    /// Starts an agent behind a new endpoint. Its slot 0 holds the endpoint with `rights`,
    /// and its slots from 1 on hold copies of `caps`.
    fn start(kids: &mut Kids, rights: u32, caps: &[u32]) -> Option<Agent> {
        let ep = sys::ep_create().ok()?;
        let agent = sys::cap_derive(ep, rights, 0, 0).ok().and_then(|door| {
            let agent = Agent::behind(kids, ep, door, caps);
            // From here on the agent's capability counts as a copy of `ep`.
            let _ = sys::cap_drop(door);
            agent
        });
        if agent.is_none() {
            let _ = sys::cap_drop(ep);
        }
        agent
    }

    /// Starts an agent whose slot 0 holds a copy of `door`, a capability for the endpoint
    /// of `ep`. The caller keeps both.
    fn behind(kids: &mut Kids, ep: u32, door: u32, caps: &[u32]) -> Option<Agent> {
        let mut handed = [NO_CAP; 4];
        handed[0] = door;
        handed[1..=caps.len()].copy_from_slice(caps);
        let kid = kids.start(mode::AGENT, 0, 0, &handed[..=caps.len()])?;
        Some(Agent { kid, ep, index: kids.index_of(kid) })
    }

    /// The result of one system call that the agent makes. If the agent cannot be asked,
    /// that is reported and the result is `i32::MIN`, which no check expects.
    fn does(&self, number: u32, args: [u32; 4]) -> i32 {
        match ask(self.ep, number, args, NO_CAP) {
            Ok(answer) => answer.words[0] as i32,
            Err(error) => {
                outln!("       the agent could not be asked: error ", error);
                i32::MIN
            }
        }
    }

    /// Ends the agent and gives up its endpoint.
    fn stop(self, kids: &mut Kids) -> Option<End> {
        let end = kids.kill(self.kid);
        let _ = sys::cap_drop(self.ep);
        end
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
    let own_code = main as *const () as u32;
    c.code("call", raw(nr::CALL, ep, READ_ONLY, 0, 0), E_FAULT);
    c.code("recv", raw(nr::RECV, ep, READ_ONLY, 0, 0), E_FAULT);
    c.code("call with the buffer in the program's code", raw(nr::CALL, ep, own_code, 0, 0), E_FAULT);
    c.code("recv with the buffer in the program's code", raw(nr::RECV, ep, own_code, 0, 0), E_FAULT);
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

    // Until `init` gave up its capability for every port, this check read "only init holds
    // a capability for the reset port". Now nobody does, and the walk must still be seen to
    // find the port capabilities that exist.
    let mut c = Check::new();
    let mut port_caps = 0;
    for index in 0..MAX_TASKS as u32 {
        if task(index).state == ST_FREE {
            continue;
        }
        for slot in 0..CAP_SLOTS as u32 {
            let found = cap(index, slot);
            if found.kind == CAP_IOPORT {
                port_caps += 1;
                if found.a <= RESET_PORT && found.b >= RESET_PORT {
                    c.eq("task that can reach port 0x64", index, MAX_TASKS as u32);
                }
            }
        }
    }
    c.yes("the walk finds the port capabilities that were handed out", port_caps >= 6);
    test.done("no task, init included, holds a capability for the reset port 0x64", c);

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

/// Revoking a capability takes back every copy that was made of it, in whatever task, and
/// with the copies what their holders did through them.
fn revocation(test: &mut Test, kids: &mut Kids) -> Option<()> {
    let me = kids.me;

    let mut c = Check::new();
    let first = sys::cap_derive(PORT, 0x80, 0x80, 0).ok()?;
    let second = sys::cap_derive(first, 0x80, 0x80, 0).ok()?;
    let third = sys::cap_derive(second, 0x80, 0x80, 0).ok()?;
    let bystander = sys::cap_derive(IRQ, 1 << 5, 0, 0).ok()?;
    c.eq("source of the first copy", cap(me, first).from, me << 8 | PORT);
    c.eq("source of the third copy", cap(me, third).from, me << 8 | second);
    c.works("dropping the second copy", sys::cap_drop(second));
    c.eq("source of the third copy after that", cap(me, third).from, me << 8 | first);
    c.works("revoking the first copy", sys::cap_revoke(first));
    c.works("reading the port through the first copy", sys::io_in(first, 0x80, 1));
    c.fails("reading it through the third copy", sys::io_in(third, 0x80, 1), E_NOCAP);
    c.works("revoking the capability itself", sys::cap_revoke(PORT));
    c.fails("reading the port through the first copy after that", sys::io_in(first, 0x80, 1), E_NOCAP);
    c.works("reading it through the capability itself", sys::io_in(PORT, 0x80, 1));
    c.works("revoking it with no copy left", sys::cap_revoke(PORT));
    c.works("dropping a copy of another capability", sys::cap_drop(bystander));
    c.fails("revoking an empty slot", sys::cap_revoke(EMPTY), E_NOCAP);
    c.fails("revoking slot 1000000", sys::cap_revoke(1_000_000), E_NOCAP);
    test.done("revoking a capability takes back its copies and theirs, and nothing else", c);

    let mut c = Check::new();
    let page = sys::cap_derive(ARCHIVE, 0, 1, 0).ok()?;
    let agent = Agent::start(kids, R_RECV, &[PORT, page])?;
    c.code("the agent maps the page", agent.does(nr::MAP_MEM, [2, CHILD_SPARE, 0, 1]), 0);
    c.code("memory of its own in the same place", agent.does(nr::MAP, [CHILD_SPARE, 1, 0, 0]), E_BUSY);
    c.works("revoking the page", sys::cap_revoke(page));
    c.eq("the agent's slot for the page", cap(agent.index, 2).kind, CAP_NULL);
    c.code("memory of its own in the same place afterwards", agent.does(nr::MAP, [CHILD_SPARE, 1, 0, 0]), 0);
    c.code("the agent reads the port", agent.does(nr::IO_IN, [1, 0x80, 1, 0]), 0);
    c.works("revoking the port", sys::cap_revoke(PORT));
    c.code("the agent reads the port afterwards", agent.does(nr::IO_IN, [1, 0x80, 1, 0]), E_NOCAP);
    c.works("reading the port oneself", sys::io_in(PORT, 0x80, 1));
    c.works("dropping the page", sys::cap_drop(page));
    c.ended("the agent", agent.stop(kids), CAUSE_KILLED, None);
    test.done("revoking reaches another task: its copy and what it mapped through it", c);

    let mut c = Check::new();
    let agent = Agent::start(kids, R_RECV, &[])?;
    c.yes("a child waits for a message", kids.wait_state(agent.kid, ST_RECV));
    c.works("revoking the endpoint it waits on", sys::cap_revoke(agent.ep));
    c.ended("the child that waited for a message", kids.end_of(agent.kid), CAUSE_EXIT, Some(GONE));
    c.works("dropping that endpoint", sys::cap_drop(agent.ep));
    let ep = sys::ep_create().ok()?;
    let send = sys::cap_derive(ep, R_SEND, 0, 0).ok()?;
    let caller = kids.start(mode::CALL_ONCE, 5, 6, &[send])?;
    c.yes("a child waits to send", kids.wait_state(caller, ST_SEND));
    c.works("revoking the send right it uses", sys::cap_revoke(send));
    c.ended("the child that waited to send", kids.end_of(caller), CAUSE_EXIT, Some(GONE));
    c.fails("receiving what it wanted to send", sys::recv(ep, &mut Msg::new(0), 0), E_TIMEOUT);
    c.works("dropping the send right", sys::cap_drop(send));
    c.works("dropping the endpoint", sys::cap_drop(ep));
    test.done("revoking wakes the tasks that waited through the copies", c);

    let mut c = Check::new();
    let agent = Agent::start(kids, R_RECV, &[IRQ])?;
    let mine = sys::ep_create().ok()?;
    c.code("the agent binds line 5", agent.does(nr::IRQ_BIND, [1, 5, 0, 0]), 0);
    c.fails("binding line 5 oneself", sys::irq_bind(IRQ, 5, mine), E_BUSY);
    c.works("revoking the interrupt capability", sys::cap_revoke(IRQ));
    c.works("binding line 5 oneself afterwards", sys::irq_bind(IRQ, 5, mine));
    c.works("dropping the bound endpoint", sys::cap_drop(mine));
    c.fails("acknowledging line 5 at the end", sys::irq_ack(IRQ, 5), E_PERM);
    c.ended("the agent", agent.stop(kids), CAUSE_KILLED, None);
    test.done("revoking an interrupt capability frees the line its holder had bound", c);

    // A server gives each client its own badged copy. Revoking one copy cuts off that
    // client and nobody else, and the server can let the client in again later.
    let mut c = Check::new();
    let service = sys::ep_create().ok()?;
    let for_a = sys::cap_derive(service, R_SEND, 1, 0).ok()?;
    let for_b = sys::cap_derive(service, R_SEND, 2, 0).ok()?;
    let client_a = kids.start(mode::CALL_ONCE, 5, 61, &[for_a])?;
    let client_b = kids.start(mode::CALL_ONCE, 5, 62, &[for_b])?;
    c.yes("the first client waits to send", kids.wait_state(client_a, ST_SEND));
    c.yes("the second client waits to send", kids.wait_state(client_b, ST_SEND));
    c.works("revoking the first client's capability", sys::cap_revoke(for_a));
    c.ended("the first client", kids.end_of(client_a), CAUSE_EXIT, Some(GONE));
    c.eq("state of the second client", task(kids.index_of(client_b)).state, ST_SEND);
    let mut msg = Msg::new(0);
    let mut answer = Msg::new(0);
    c.works("receiving the second client's call", sys::recv(service, &mut msg, 2000));
    c.eq("badge of the call", msg.badge, 2);
    c.eq("word of the call", msg.words[0], 62);
    answer.words[0] = 72;
    c.works("answering the second client", sys::reply(msg.token, &answer));
    c.ended("the second client", kids.end_of(client_b), CAUSE_EXIT, Some(72));
    c.fails("receiving anything from the first client", sys::recv(service, &mut msg, 0), E_TIMEOUT);
    let back = kids.start(mode::CALL_ONCE, 5, 63, &[for_a])?;
    c.works("receiving a call of a client let in again", sys::recv(service, &mut msg, 2000));
    c.eq("badge of that call", msg.badge, 1);
    c.eq("word of that call", msg.words[0], 63);
    answer.words[0] = 73;
    c.works("answering it", sys::reply(msg.token, &answer));
    c.ended("the client let in again", kids.end_of(back), CAUSE_EXIT, Some(73));
    for slot in [for_a, for_b, service] {
        c.works("dropping a capability of the service", sys::cap_drop(slot));
    }
    test.done("a server takes back one client's access and leaves the other's", c);
    Some(())
}

/// Capabilities travel in messages, but only between two ends that both hold the grant
/// right, and what arrives stays a copy that its sender can take back.
fn delegation(test: &mut Test, kids: &mut Kids) -> Option<()> {
    let me = kids.me;
    let agent = Agent::start(kids, R_RECV | R_GRANT, &[])?;
    let plain = sys::cap_derive(agent.ep, R_SEND, 0, 0).ok()?;
    // A request that does nothing, with the capability in `slot` attached.
    let send = |via: u32, slot: u32| ask(via, nr::YIELD, [0; 4], slot);

    let mut c = Check::new();
    let landed = c.works("the call", send(agent.ep, PORT)).map_or(NO_CAP, |answer| answer.words[2]);
    c.yes("the agent names the slot that the copy went to", landed < CAP_SLOTS as u32);
    c.eq("source of the agent's copy", cap(agent.index, landed).from, me << 8 | PORT);
    c.code("the agent reads the port", agent.does(nr::IO_IN, [landed, 0x80, 1, 0]), 0);
    test.done("a capability goes along with a call when both ends hold the grant right", c);

    let mut c = Check::new();
    let deaf = Agent::start(kids, R_RECV, &[])?;
    let held = (task(agent.index).caps, task(deaf.index).caps);
    c.fails("sending through a capability without the grant right", send(plain, PORT), E_PERM);
    c.fails("sending to a task that waits without it", send(deaf.ep, PORT), E_PERM);
    c.fails("sending an empty slot", send(agent.ep, EMPTY), E_NOCAP);
    c.fails("sending slot 1000000", send(agent.ep, 1_000_000), E_NOCAP);
    c.works("a plain message through the same capability", send(plain, NO_CAP));
    c.works("a plain message to the same task", send(deaf.ep, NO_CAP));
    c.eq("capabilities of the agent", task(agent.index).caps, held.0);
    c.eq("capabilities of the task without the grant right", task(deaf.index).caps, held.1);
    c.ended("the task without the grant right", deaf.stop(kids), CAUSE_KILLED, None);
    test.done("without the grant right at both ends nothing is handed over", c);

    let mut c = Check::new();
    c.works("revoking the port", sys::cap_revoke(PORT));
    c.eq("the agent's slot afterwards", cap(agent.index, landed).kind, CAP_NULL);
    c.code("the agent reads the port", agent.does(nr::IO_IN, [landed, 0x80, 1, 0]), E_NOCAP);
    c.works("reading the port oneself", sys::io_in(PORT, 0x80, 1));
    test.done("the sender takes back what it sent", c);

    let mut c = Check::new();
    let again = c.works("sending the port again", send(agent.ep, PORT));
    let held = again.map_or(NO_CAP, |answer| answer.words[2]);
    if let Some(answer) = c.works("asking for it without the grant right", fetch(plain, held)) {
        c.eq("slot that it came back to", answer.cap, NO_CAP);
        c.eq("why it did not come back", answer.words[3], E_PERM.unsigned_abs());
    }
    let back = c.works("asking for it", fetch(agent.ep, held)).map_or(NO_CAP, |answer| answer.cap);
    c.eq("source of what came back", cap(me, back).from, agent.index << 8 | held);
    c.works("reading the port through it", sys::io_in(back, 0x80, 1));
    c.works("revoking the port", sys::cap_revoke(PORT));
    c.fails("reading the port through it afterwards", sys::io_in(back, 0x80, 1), E_NOCAP);
    c.eq("the agent's slot afterwards", cap(agent.index, held).kind, CAP_NULL);
    c.works("dropping the capability without the grant right", sys::cap_drop(plain));
    c.ended("the agent", agent.stop(kids), CAUSE_KILLED, None);
    test.done("a reply carries a capability back, still linked to where it came from", c);

    let mut c = Check::new();
    let ep = sys::ep_create().ok()?;
    let lender = sys::cap_derive(ep, R_RECV | R_GRANT, 0, 0).ok()?;
    let quiet = sys::cap_derive(ep, R_RECV, 0, 0).ok()?;
    let agent = Agent::behind(kids, ep, lender, &[quiet])?;
    c.yes("the agent waits for a message", kids.wait_state(agent.kid, ST_RECV));
    c.works("revoking its capability with the grant right", sys::cap_revoke(lender));
    c.eq("state of the agent, which has another capability", task(agent.index).state, ST_RECV);
    c.fails("sending it a capability", send(ep, PORT), E_PERM);
    c.works("a plain message", send(ep, NO_CAP));
    c.ended("the agent, whose slot 0 is empty now", kids.end_of(agent.kid), CAUSE_EXIT, Some(GONE));
    for slot in [lender, quiet, ep] {
        c.works("dropping a capability for the endpoint", sys::cap_drop(slot));
    }
    test.done("a waiting task that loses the grant right takes no more capabilities", c);

    let mut c = Check::new();
    let agent = Agent::start(kids, R_RECV | R_GRANT, &[PORT])?;
    let (copies, count, error) = fill_table();
    c.code("a copy when every slot is taken", error, E_NOSLOT);
    if let Some(answer) = c.works("asking the agent for its port capability", fetch(agent.ep, 1)) {
        c.eq("slot that it came to", answer.cap, NO_CAP);
        c.eq("why it did not come", answer.words[3], E_NOSLOT.unsigned_abs());
    }
    for &slot in &copies[..count] {
        c.works("dropping a copy", sys::cap_drop(slot));
    }
    for _ in 0..CAP_SLOTS {
        if agent.does(nr::CAP_DERIVE, [0, R_RECV, 0, 0]) < 0 {
            break;
        }
    }
    c.eq("capabilities of the agent", task(agent.index).caps, CAP_SLOTS as u32);
    c.fails("sending the agent a capability", send(agent.ep, PORT), E_NOSLOT);
    c.works("a plain message", send(agent.ep, NO_CAP));
    c.ended("the agent", agent.stop(kids), CAUSE_KILLED, None);
    test.done("a capability is not handed to a task whose table is full", c);
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

    let mut c = Check::new();
    let end = kids.run(mode::TOUCH_UNMAPPED, 0, 0);
    c.ended("the child", end, PAGE_FAULT, Some(CHILD_PLAY));
    if let Some(end) = end {
        c.eq("error code of the fault", end.code, ABSENT_READ);
    }
    test.done("a page that was unmapped faults when it is touched again", c);

    sweep(test, me);
    Some(())
}

/// Takes every free page frame at least once and gives it back.
///
/// The kernel hands out frames in rising order from where it last found one, and starts
/// again at the bottom after the top. Two batches are held in turn, so each new batch
/// gets frames beyond the other one; once a quarter more frames have been taken than
/// were free, every free frame has been in a batch, the highest the kernel manages
/// among them. Each page must arrive zeroed, also when this sweep wrote to its frame on
/// an earlier lap, and must hold what is written to its first and last byte.
fn sweep(test: &mut Test, me: u32) {
    let mut c = Check::new();
    let before = usage(me);
    let memory = stats();
    let batch = (spare_pages(me).saturating_sub(8) / 2).min(SWEEP_BATCH);
    c.yes("this program's quota allows batches of 64 pages or more", batch >= SWEEP_LEAST);
    let target = memory.free_frames + memory.free_frames / 4;
    let mut held: [Option<Pages>; 2] = [None, None];
    let (mut taken, mut errors, mut zeroed, mut kept) = (0u32, 0u32, true, true);
    while batch >= SWEEP_LEAST && taken < target && errors == 0 {
        let place = (taken / batch % 2) as usize;
        if let Some(pages) = held[place].take() {
            errors += u32::from(pages.unmap().is_err());
        }
        match Pages::map(SWEEP_AT + place as u32 * SWEEP_GAP, batch) {
            Ok(pages) => {
                for page in 0..batch {
                    let value = pattern(taken + page) | 1;
                    for at in [page * PAGE, page * PAGE + PAGE - 1] {
                        zeroed &= pages.get(at) == 0;
                        pages.set(at, value);
                        kept &= pages.get(at) == value;
                    }
                }
                held[place] = Some(pages);
                taken += batch;
            }
            Err(_) => errors += 1,
        }
    }
    for pages in held.into_iter().flatten() {
        errors += u32::from(pages.unmap().is_err());
    }
    c.eq("maps and unmaps that failed", errors, 0);
    c.yes("the sweep took a quarter more frames than were free", taken >= target);
    c.yes("every page arrived zeroed", zeroed);
    c.yes("every page held what was written", kept);
    c.same(usage(me), before);
    outln!("       took ", taken, " page frames in batches of ", batch, "; ", memory.free_frames, " of ",
           memory.total_frames, " were free");
    test.done("every free page frame, up to the top of memory, can be taken and given back", c);
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

/// Tasks end without taking anything with them, and a full capability table recovers.
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
    kids.lend(1, 0);
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

    // The image has three loadable segments: constants, code and data, in that order.
    let (code_header, data_header) = (load + 32, load + 64);
    let mut c = Check::new();
    let before = usage(me);
    let spare = spare_pages(me);
    c.eq("kind of the second segment", word(copy, code_header), 1);
    c.eq("access of the second segment", word(copy, code_header + 24), 5);
    c.eq("kind of the third segment", word(copy, data_header), 1);
    c.eq("access of the third segment", word(copy, data_header + 24), 6);
    let code_start = word(copy, code_header + 8);
    let code_last_page = (code_start + word(copy, code_header + 20) - 1) & !(PAGE - 1);
    let damage = [
        ("code that can be written", code_header + 24, 7),
        ("data that can be executed", data_header + 24, 7),
        ("data in the first page of the code", data_header + 8, code_start),
        ("data in the last page of the code", data_header + 8, code_last_page),
    ];
    for (what, at, value) in damage {
        let original = word(copy, at);
        set_word(copy, at, value);
        kids.refuse(&mut c, what, copy, CHILD_PAGES, CHILD_STACK, E_INVAL);
        set_word(copy, at, original);
    }
    c.same(usage(me), before);
    c.eq("spare pages", spare_pages(me), spare);
    test.done("an image with writable code, or with two segments on one page, is refused", c);

    // Each smaller quota stops the loader at an earlier allocation: the tables of the
    // address space, the pages of each segment, the stack.
    let mut c = Check::new();
    let mut smallest = None;
    for quota in 0..=CHILD_PAGES {
        let spawn = kids.spawn_args(mode::EXIT, 33, 0, &[], quota, CHILD_STACK);
        match sys::spawn(copy, &spawn) {
            Ok(task) => {
                let end = kids.end_of(Kid { task, badge: spawn.fault_badge });
                c.ended("the first child with enough memory", end, CAUSE_EXIT, Some(33));
                smallest = Some(quota);
                break;
            }
            Err(error) => {
                c.code("starting a child with too little memory", error, E_NOMEM);
                c.same(usage(me), before);
                c.eq("spare pages", spare_pages(me), spare);
            }
        }
    }
    let enough = smallest.is_some_and(|pages: u32| pages > CHILD_STACK + 3);
    c.yes("a child needs more than its stack and three more pages", enough);
    c.same(usage(me), before);
    c.eq("spare pages", spare_pages(me), spare);
    test.done("starting a child fails cleanly at every point where memory can run out", c);

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
    let (copies, count, error) = fill_table();
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
    Some(())
}

/// Tasks and endpoints are made only within the quotas that a parent lends its children,
/// and every share comes back to the parent.
fn quotas(test: &mut Test, kids: &mut Kids) -> Option<()> {
    let me = kids.me;

    let mut c = Check::new();
    let before = task(me);
    kids.lend(0, 1);
    let agent = Agent::start(kids, R_RECV, &[])?;
    c.eq("quota of tasks that is left", task(me).tasks, before.tasks - 1);
    c.eq("quota of endpoints that is left", task(me).endpoints, before.endpoints - 2);
    c.eq("the child's quota of tasks", task(agent.index).tasks, 0);
    c.eq("the child's quota of endpoints", task(agent.index).endpoints, 1);
    let made = agent.does(nr::EP_CREATE, [0; 4]);
    c.yes("the child makes an endpoint", made >= 0);
    c.code("the child makes a second one", agent.does(nr::EP_CREATE, [0; 4]), E_NOSLOT);
    c.code("the child drops the first", agent.does(nr::CAP_DROP, [made as u32, 0, 0, 0]), 0);
    c.yes("the child makes another", agent.does(nr::EP_CREATE, [0; 4]) >= 0);
    let archive_pages = cap(me, ARCHIVE).b;
    let parent = kids.start_with(mode::GRANDCHILD, archive_pages, 0, &[NO_CAP, ARCHIVE], 96).ok()?;
    c.ended("a child that starts a child without a quota", kids.end_of(parent), CAUSE_EXIT, Some(0xE3));
    let mut spawn = kids.spawn_args(mode::EXIT, 0, 0, &[], CHILD_PAGES, CHILD_STACK);
    spawn.tasks = task(me).tasks;
    c.fails("lending a child every task that is left", sys::spawn(kids.image.bytes(), &spawn), E_NOSLOT);
    spawn.tasks = 0;
    spawn.endpoints = task(me).endpoints + 1;
    c.fails("lending a child one endpoint too many", sys::spawn(kids.image.bytes(), &spawn), E_NOSLOT);
    c.ended("the child", agent.stop(kids), CAUSE_KILLED, None);
    c.eq("quota of tasks afterwards", task(me).tasks, before.tasks);
    c.eq("quota of endpoints afterwards", task(me).endpoints, before.endpoints);
    test.done("a child gets the share of the quotas that its parent lends, and no more", c);

    let mut c = Check::new();
    let before = task(me).endpoints;
    kids.lend(0, 1);
    let agent = Agent::start(kids, R_RECV | R_GRANT, &[])?;
    let made = agent.does(nr::EP_CREATE, [0; 4]) as u32;
    let fetched = c.works("asking the child for its endpoint", fetch(agent.ep, made));
    let got = fetched.map_or(NO_CAP, |answer| answer.cap);
    c.eq("source of the copy", cap(me, got).from, agent.index << 8 | made);
    c.ended("the child", agent.stop(kids), CAUSE_KILLED, None);
    c.eq("source of the copy afterwards", cap(me, got).from, NO_CAP);
    c.fails("receiving from the endpoint", sys::recv(got, &mut Msg::new(0), 0), E_TIMEOUT);
    c.eq("quota of endpoints while it exists", task(me).endpoints, before - 1);
    c.works("dropping it", sys::cap_drop(got));
    c.eq("quota of endpoints afterwards", task(me).endpoints, before);
    test.done("an endpoint outlives the task that made it, and its parent pays for it", c);

    let mut c = Check::new();
    let endpoints_before = stats().endpoints;
    let quota = task(me).endpoints;
    let reports = sys::ep_create().ok()?;
    let door = sys::cap_derive(reports, R_SEND, 0, 0).ok()?;
    let third = (quota - 1) / 3;
    let mut makers = [None; 3];
    let mut msg = Msg::new(0);
    for (maker, share) in makers.iter_mut().zip([third, third, quota - 1 - 2 * third]) {
        kids.lend(0, share);
        *maker = kids.start(mode::ENDPOINTS, 0, 0, &[door]);
        if c.works("a report from a child", sys::recv(reports, &mut msg, 3000)).is_some() {
            c.eq("endpoints the child made", msg.words[0], share);
            c.eq("why the child stopped", msg.words[1], E_NOSLOT.unsigned_abs());
            c.works("reply", sys::reply(msg.token, &Msg::new(0)));
        }
    }
    c.eq("endpoints in use", stats().endpoints, endpoints_before + quota);
    c.eq("quota of endpoints that is left", task(me).endpoints, 0);
    c.fails("one more endpoint", sys::ep_create(), E_NOSLOT);
    for maker in makers.into_iter().flatten() {
        c.ended("a child", kids.kill(maker), CAUSE_KILLED, None);
    }
    c.works("dropping the send right", sys::cap_drop(door));
    c.works("dropping the endpoint", sys::cap_drop(reports));
    c.eq("endpoints in use afterwards", stats().endpoints, endpoints_before);
    c.eq("quota of endpoints afterwards", task(me).endpoints, quota);
    c.works("a new endpoint afterwards", sys::ep_create().and_then(sys::cap_drop));
    test.done("endpoints are created only within a quota, and the quota comes back", c);

    let mut c = Check::new();
    let tasks_before = stats().tasks;
    let quota = task(me).tasks;
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
    c.code("a child when the quota is used up", error, E_NOSLOT);
    c.eq("children started", started, quota);
    c.eq("tasks", stats().tasks, tasks_before + quota);
    c.eq("quota of tasks that is left", task(me).tasks, 0);
    for badge in first_badge..first_badge + started {
        c.ended("a child", kids.end_of_badge(badge, 3000), CAUSE_EXIT, Some(0));
    }
    c.eq("tasks afterwards", stats().tasks, tasks_before);
    c.eq("quota of tasks afterwards", task(me).tasks, quota);
    c.ended("one more child", kids.run(mode::EXIT, 9, 0), CAUSE_EXIT, Some(9));
    test.done("tasks are started only within a quota, and the quota comes back", c);
    Some(())
}

/// What one stretch of spinning saw: the rounds of arithmetic this program got through, and
/// what the kernel counted meanwhile. The counts tell a stalled machine (few timer ticks for
/// the time that passed) from another task taking the processor (ticks, but few of them
/// this program's) and from a slow emulator (all the ticks, and still few rounds).
#[derive(Clone, Copy)]
struct Stretch {
    rounds: u32,
    /// Timer interrupts the kernel counted, 100 a second.
    ticks: u32,
    /// Those of them that found this program running.
    own: u32,
    /// Times the kernel went from one task to another.
    switches: u32,
}

impl Stretch {
    fn show(&self, name: &str) {
        outln!("       ", name, ": ", self.rounds, " rounds, ", self.ticks, " ticks of which ", self.own,
               " found this program running, ", self.switches, " task switches");
    }

    /// Rounds per hundred of the ticks that found this program running: how fast the
    /// machine ran this program while it had the processor, whoever else was waiting.
    fn speed(&self) -> u64 {
        u64::from(self.rounds) * 100 / u64::from(self.own.max(1))
    }
}

/// How often the share measurement is taken at most before the subtest fails for want of
/// a steady one.
const SHARE_TRIES: u32 = 5;
/// A measurement is steady when the slowest of its five stretches ran this program at
/// this percentage of the speed of the fastest, or more.
const STEADY_PERCENT: u64 = 75;

/// This program's speed beside a group held to 20% and beside one without a share, and
/// alone before, between and after.
struct Measurement {
    before: Stretch,
    capped: Stretch,
    capped_used: u32,
    between: Stretch,
    free: Stretch,
    free_used: u32,
    after: Stretch,
}

impl Measurement {
    /// The slowest and the fastest of the five stretches, in rounds per hundred own ticks.
    fn speeds(&self) -> (u64, u64) {
        let all = [self.before.speed(), self.capped.speed(), self.between.speed(), self.free.speed(), self.after.speed()];
        let slowest = all.iter().copied().min().unwrap_or(0);
        let fastest = all.iter().copied().max().unwrap_or(0);
        (slowest, fastest)
    }

    /// Whether the machine ran this program at one speed throughout. The rounds a tick of
    /// its own buys do not depend on who else wants the processor, so a change is the
    /// machine's (an emulator on a busy host), and the figures of such a measurement
    /// compare stretches that were not run at the same speed.
    fn steady(&self) -> bool {
        let (slowest, fastest) = self.speeds();
        slowest * 100 >= fastest * STEADY_PERCENT
    }
}

/// Counts rounds of a fixed piece of arithmetic until `ms` milliseconds have passed: how
/// much of the processor this program, task `me`, got in that time.
fn spins_in(me: u32, ms: u32) -> Stretch {
    let (stats_before, own_before) = (stats(), task(me).cpu_ticks);
    let deadline = sys::time() + u64::from(ms) * 1000;
    let mut rounds = 0u32;
    let mut value = 1u32;
    while sys::time() < deadline {
        for step in 0..2000u32 {
            value = core::hint::black_box(value.wrapping_mul(1_664_525).wrapping_add(step));
        }
        rounds += 1;
    }
    core::hint::black_box(value);
    let stats_after = stats();
    Stretch {
        rounds,
        ticks: stats_after.ticks.wrapping_sub(stats_before.ticks),
        own: task(me).cpu_ticks.wrapping_sub(own_before),
        switches: stats_after.switches.wrapping_sub(stats_before.switches),
    }
}

/// Tasks that are alive and were started by task `parent`.
fn children_of(parent: u32) -> u32 {
    (0..MAX_TASKS as u32).filter(|&index| task(index).state != ST_FREE && task(index).parent == parent).count() as u32
}

/// A group of three spinning tasks beside this program, with a share of the processor
/// (0 = this program's own). Returns this program's rounds in two seconds and what the
/// kernel says the group used of the last full second, in thousandths.
fn beside_hogs(c: &mut Check, kids: &mut Kids, share: u32) -> Option<(Stretch, u32)> {
    let archive_pages = cap(kids.me, ARCHIVE).b;
    kids.lend(2, 0);
    let mut spawn = kids.spawn_args(mode::HOGS, archive_pages, 2, &[NO_CAP, ARCHIVE], 120, CHILD_STACK);
    spawn.cpu = share;
    let task_cap = c.works("starting the group", sys::spawn(kids.image.bytes(), &spawn))?;
    let group = Kid { task: task_cap, badge: spawn.fault_badge };
    let index = kids.index_of(group);
    c.yes("the group has its two children", wait_until(|| children_of(index) == 2));
    let want = if share == 0 { task(kids.me).cpu_limit } else { share };
    c.eq("share of the group", task(index).cpu_limit, want);
    for child in (0..MAX_TASKS as u32).filter(|&child| task(child).state != ST_FREE && task(child).parent == index) {
        c.eq("share of a child of the group", task(child).cpu_limit, want);
    }
    let stretch = spins_in(kids.me, 2000);
    let used = task(index).cpu_used;
    c.ended("the group", kids.kill(group), CAUSE_KILLED, None);
    Some((stretch, used))
}

/// A task's share of the processor: what it and everything it starts may use of each
/// second while somebody else wants the processor.
fn shares(test: &mut Test, kids: &mut Kids) -> Option<()> {
    let me = kids.me;

    let mut c = Check::new();
    let mine = task(me).cpu_limit;
    c.eq("this program's share, which the shell passed on", mine, 80);
    let mut spawn = kids.spawn_args(mode::EXIT, 0, 0, &[], CHILD_PAGES, CHILD_STACK);
    for share in [mine + 1, 100, 101, 256 + 20, u32::MAX] {
        spawn.cpu = share;
        c.fails("starting a child with a larger share", sys::spawn(kids.image.bytes(), &spawn), E_PERM);
    }
    let heir = kids.start(mode::SLEEP, 60_000, 0, &[])?;
    c.eq("share of a child that was given none", task(kids.index_of(heir)).cpu_limit, mine);
    c.ended("that child", kids.kill(heir), CAUSE_KILLED, None);
    let mut spawn = kids.spawn_args(mode::SLEEP, 60_000, 0, &[], CHILD_PAGES, CHILD_STACK);
    spawn.cpu = 30;
    let small = Kid { task: sys::spawn(kids.image.bytes(), &spawn).ok()?, badge: spawn.fault_badge };
    c.eq("share of a child that was given 30%", task(kids.index_of(small)).cpu_limit, 30);
    c.ended("that child", kids.kill(small), CAUSE_KILLED, None);
    test.done("a child gets its parent's share of the processor or a smaller one, never a larger", c);

    let mut c = Check::new();
    // This program's speed with nothing beside it, taken before, between and after the
    // two runs. Under an emulator on a busy host the machine can stall for a while, and
    // a stall only ever makes a figure smaller: the largest of the three is the reference.
    //
    // A measurement during which the machine changed speed is set aside, said so, and
    // taken again, SHARE_TRIES times at most. The conditions below are the same for every
    // try; without a steady measurement the subtest fails.
    let mut steady = None;
    for attempt in 1..=SHARE_TRIES {
        let before = spins_in(me, 500);
        let (capped, capped_used) = beside_hogs(&mut c, kids, 20)?;
        let between = spins_in(me, 500);
        let (free, free_used) = beside_hogs(&mut c, kids, 0)?;
        let after = spins_in(me, 500);
        let measurement = Measurement { before, capped, capped_used, between, free, free_used, after };
        if measurement.steady() {
            steady = Some(measurement);
            break;
        }
        let (slowest, fastest) = measurement.speeds();
        outln!("       try ", attempt, " of ", SHARE_TRIES, " set aside: the machine changed speed during it, from ",
               fastest as u32, " to ", slowest as u32, " rounds");
        outln!("       per hundred ticks of this program's own");
        measurement.before.show("set aside: by itself for half a second");
        measurement.capped.show("set aside: next to the group held to 20% for two seconds");
        measurement.between.show("set aside: by itself for half a second");
        measurement.free.show("set aside: next to the group without a share for two seconds");
        measurement.after.show("set aside: by itself for half a second");
    }
    let Measurement { before, capped, capped_used, between, free, free_used, after } = match steady {
        Some(measurement) => measurement,
        None => {
            c.yes("the machine kept one speed through at least one of the measurements", false);
            test.done("three spinning tasks with a 20% share leave the rest of the processor to others", c);
            return Some(());
        }
    };
    let alone = u64::from(before.rounds.max(between.rounds).max(after.rounds)).max(1);
    // Rounds per second, as a percentage of the rate with nothing else running.
    let capped_rate = (u64::from(capped.rounds) * 25 / alone) as u32;
    let free_rate = (u64::from(free.rounds) * 25 / alone) as u32;
    outln!("       beside three spinning tasks this program kept ", capped_rate, "% of its speed when they");
    outln!("       shared 20% of the processor, and ", free_rate, "% when they had no share of their own;");
    outln!("       the kernel charged the group ", capped_used / 10, "% and ", free_used / 10, "% of a second");
    before.show("alone, half a second");
    capped.show("beside the group held to 20%, two seconds");
    between.show("alone, half a second");
    free.show("beside the group without a share, two seconds");
    after.show("alone, half a second");
    c.yes("this program keeps at least 45% of its speed beside a group held to 20%", capped_rate >= 45);
    c.yes("it keeps at most 40% of its speed beside the same group without a share", free_rate <= 40);
    c.yes("the group held to 20% is charged less than the free one", capped_used < free_used);
    c.yes("the group held to 20% is charged at most 45% of a second", capped_used <= 450);
    c.yes("the free group is charged at least 55% of a second", free_used >= 550);
    test.done("three spinning tasks with a 20% share leave the rest of the processor to others", c);
    Some(())
}

/// What `init` handed out by name is all the device authority there is.
fn authority(test: &mut Test, kids: &mut Kids) -> Option<()> {
    // The keyboard data port, the test port, the cursor registers, the serial port and the
    // two power-off ports; the last two stay with `init`.
    const PORT_RANGES: [(u32, u32); 6] =
        [(0x60, 0x60), (0x80, 0x80), (0x3D4, 0x3D5), (0x3F8, 0x3FF), (0x604, 0x605), (0xB004, 0xB005)];
    let archive = cap(kids.me, ARCHIVE);
    let mut c = Check::new();
    let (mut ranges, mut lines, mut strays) = (0u32, 0u32, 0u32);
    for index in 0..MAX_TASKS as u32 {
        if task(index).state == ST_FREE {
            continue;
        }
        for slot in 0..CAP_SLOTS as u32 {
            let found = cap(index, slot);
            let listed = match found.kind {
                CAP_IOPORT => match PORT_RANGES.iter().position(|&(first, last)| found.a >= first && found.b <= last) {
                    Some(range) => {
                        ranges |= 1 << range;
                        range < 4 || index == 0
                    }
                    None => false,
                },
                CAP_IRQ => {
                    lines |= found.a;
                    true
                }
                CAP_MEMORY => {
                    let end = found.a + found.b * PAGE;
                    let screen = found.a >= VIDEO && end <= VIDEO + 8 * PAGE;
                    let files = found.a >= archive.a && end <= archive.a + archive.b * PAGE;
                    screen || files
                }
                _ => true,
            };
            if !listed {
                strays += 1;
                outln!("       task ", index, " slot ", slot, ": kind ", found.kind, ", ", Hex(found.a), " to ", Hex(found.b));
            }
        }
    }
    c.eq("device capabilities that are not on the list", strays, 0);
    c.eq("port ranges of the list that somebody holds", ranges, 0b11_1111);
    c.eq("interrupt lines that somebody holds", lines, 1 << 1 | 1 << 4 | 1 << 5);
    test.done("every port, interrupt line and page of device memory that any task holds was handed out by name", c);
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

/// The kernel writes a message into a task's buffer after the task has blocked, so the
/// buffer must still be the task's own writable memory by then. Memory of a device is the
/// only memory that can be taken from a blocked task, and it is refused as a buffer.
fn buffers(test: &mut Test, kids: &mut Kids) -> Option<()> {
    let ep = sys::ep_create().ok()?;

    let mut c = Check::new();
    let device_at = SCRATCH + 2 * PAGE;
    let across = device_at - 100;
    c.works("mapping the device page, writable", sys::map_mem(DEVICE, device_at, 0, 1, true));
    c.code("call", raw(nr::CALL, ep, device_at, 0, 0), E_FAULT);
    c.code("recv", raw(nr::RECV, ep, device_at, 0, 0), E_FAULT);
    c.code("call that ends in the device page", raw(nr::CALL, ep, across, 0, 0), E_FAULT);
    c.code("recv that ends in the device page", raw(nr::RECV, ep, across, 0, 0), E_FAULT);
    c.code("statistics", raw(nr::SYSINFO, INFO, INFO_STATS, 0, device_at), E_FAULT);
    // SAFETY: the page was mapped just above and nothing in this program refers to it.
    c.works("unmapping the device page", unsafe { sys::unmap(device_at, 1) });
    test.done("memory of a device is refused as a message buffer, also in part", c);

    let mut c = Check::new();
    let device = sys::cap_derive(DEVICE, 0, 1, 1).ok()?;
    let agent = Agent::start(kids, R_RECV | R_SEND, &[device])?;
    let map_device = [1, CHILD_SPARE, 0, 1 | MAP_WRITE];
    let map_own = [CHILD_SPARE, 1, 1, 0];
    c.code("the agent maps the device page, writable", agent.does(nr::MAP_MEM, map_device), 0);
    c.code("the agent receives into it", agent.does(nr::RECV, [0, CHILD_SPARE, 0, 0]), E_FAULT);
    c.code("the agent calls with it", agent.does(nr::CALL, [0, CHILD_SPARE, 0, 0]), E_FAULT);
    c.code("memory of its own in the same place", agent.does(nr::MAP, map_own), E_BUSY);
    c.yes("the agent waits for a message", kids.wait_state(agent.kid, ST_RECV));
    c.code("rule that is broken while it waits", broken_rule(), 0);
    c.works("revoking the device page", sys::cap_revoke(device));
    c.eq("the agent's slot for the page", cap(agent.index, 1).kind, CAP_NULL);
    c.eq("state of the agent", task(agent.index).state, ST_RECV);
    c.code("rule that is broken afterwards", broken_rule(), 0);
    c.code("memory of its own in the same place, now", agent.does(nr::MAP, map_own), 0);
    c.works("dropping the capability", sys::cap_drop(device));
    c.ended("the agent", agent.stop(kids), CAUSE_KILLED, None);
    test.done("a device page taken from a waiting task was never its message buffer", c);

    let mut c = Check::new();
    let anomalies = stats().anomalies;
    let send = sys::cap_derive(ep, R_SEND, 0, 0).ok()?;
    let caller = kids.start(mode::CALL_ONCE, 5, 6, &[send])?;
    let mut msg = Msg::new(0);
    c.works("receiving the child's call", sys::recv(ep, &mut msg, 1000));
    c.yes("the child waits for the reply", kids.wait_state(caller, ST_REPLY));
    c.works("revoking the capability it called through", sys::cap_revoke(send));
    c.eq("state of the child", task(kids.index_of(caller)).state, ST_REPLY);
    c.code("rule that is broken while the reply is due", broken_rule(), 0);
    let mut answer = Msg::new(0);
    answer.words[0] = 77;
    c.works("replying", sys::reply(msg.token, &answer));
    c.ended("the child", kids.end_of(caller), CAUSE_EXIT, Some(77));
    c.eq("anomalies", stats().anomalies, anomalies);
    c.works("dropping the send capability", sys::cap_drop(send));
    test.done("a reply reaches a caller whose capability was revoked in between", c);

    sys::cap_drop(ep).ok()?;
    Some(())
}

/// Only program code can be executed: the stack, data and mapped memory cannot (W^X).
/// A processor without the no-execute bit cannot enforce this, so the checks are skipped.
fn execution(test: &mut Test, kids: &mut Kids) -> Option<()> {
    type Place = fn(u32) -> bool;
    let on_stack: Place = |at| (STACK_TOP - CHILD_STACK * PAGE..STACK_TOP).contains(&at);
    let in_data: Place = |at| (USER_BASE..CHILD_PLAY).contains(&at);
    let in_mapped: Place = |at| at == CHILD_PLAY;
    let attempts = [
        ("a program cannot run code from its stack", mode::EXEC_STACK, on_stack),
        ("a program cannot run code from its data", mode::EXEC_DATA, in_data),
        ("a program cannot run code from memory it mapped", mode::EXEC_MAPPED, in_mapped),
    ];
    let enforced = stats().nx != 0;
    for (name, what, in_place) in attempts {
        if !enforced {
            test.skip(name);
            continue;
        }
        let mut c = Check::new();
        let end = kids.run(what, 0, 0);
        c.ended("the child", end, PAGE_FAULT, None);
        if let Some(end) = end {
            c.yes("the fault is where the data is", in_place(end.detail));
            c.eq("address of the instruction", end.eip, end.detail);
            c.eq("error code of the fault", end.code, EXECUTE_FAULT);
        }
        test.done(name, c);
    }
    Some(())
}

type Group = fn(&mut Test, &mut Kids) -> Option<()>;

/// How long the selftest sleeps before its first reading of what is in use, so that its
/// starter has given back the pages it loaded the selftest's file into.
const STARTER_MS: u32 = 50;

#[no_mangle]
pub fn main() {
    set_output(Sink::Console(CONSOLE));
    outln!("Each line is one promise of the kernel, checked from user space.");
    let mut test = Test { passed: 0, total: 0, skipped: 0 };
    test.check("the kernel's invariants hold at the start", sys::check(INFO) == Ok(0));
    test.check("the kernel has met nothing unexpected so far", stats().anomalies == 0);
    let scratch = sys::map(SCRATCH, 2, true).and_then(|()| sys::map(READ_ONLY, 1, false));
    match (Kids::new(), scratch) {
        (Some(mut kids), Ok(())) => {
            // The program that started this one may still hold the pages it loaded this
            // program's file into, and unmaps them as soon as it runs again. Let it run
            // first: otherwise those pages count as taken before and as free after.
            sys::sleep(STARTER_MS);
            let before = usage(kids.me);
            let groups: [(&str, Group); 13] = [
                ("argument", arguments),
                ("capability", capabilities),
                ("authority", authority),
                ("message", ipc),
                ("revocation", revocation),
                ("delegation", delegation),
                ("buffer", buffers),
                ("memory", memory),
                ("execution", execution),
                ("task", tasks),
                ("quota", quotas),
                ("processor share", shares),
                ("clock and file", time_and_files),
            ];
            for (name, group) in groups {
                if group(&mut test, &mut kids).is_none() {
                    outln!("       the ", name, " checks stopped early: a step that prepares one failed");
                    test.check(name, false);
                }
            }
            let mut c = Check::new();
            c.code("rule that is broken", broken_rule(), 0);
            test.done("the kernel's invariants hold at the end", c);
            let mut c = Check::new();
            c.eq("anomalies", stats().anomalies, 0);
            test.done("the kernel met nothing unexpected", c);
            let mut c = Check::new();
            c.same(usage(kids.me), before);
            test.done("no task, endpoint, page frame or quota was lost", c);
        }
        _ => test.check("loading the child program and mapping scratch pages", false),
    }
    if test.passed != test.total {
        outln!("SELFTEST: FAIL ", test.passed, "/", test.total);
    } else if test.skipped == 0 {
        outln!("SELFTEST: PASS ", test.passed, "/", test.total);
    } else {
        let (passed, skipped) = (test.passed, test.skipped);
        outln!("SELFTEST: PASS ", passed, "/", test.total, ", ", skipped, " skipped (no no-execute bit)");
    }
}
