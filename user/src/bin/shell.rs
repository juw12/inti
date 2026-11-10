//! `shell`: the command line.
//!
//! It is an ordinary task. It can inspect the system through a read-only Info capability,
//! start programs from the file server, and ask the servers to fail on purpose so that the
//! recovery can be watched and timed. A program it starts gets one capability by default:
//! the right to print.
#![no_std]
#![no_main]

use rt::abi::{CapInfo, Msg, SpawnArgs, CAP_ENDPOINT, CAP_INFO, CAP_IOPORT, CAP_IRQ, CAP_LOG};
use rt::abi::{CAP_MEMORY, CAP_SLOTS, CAP_TASK, CAUSE_EXIT, CAUSE_KILLED, E_DEAD, FOREVER, L_FAULT};
use rt::abi::{MAX_TASKS, MSG_DATA, NO_CAP, NO_TASK, ST_FREE, ST_READY, ST_RECV, ST_REPLY};
use rt::abi::{ST_SEND, ST_SLEEP, ST_ZOMBIE, USER_BASE};
use rt::io::{cause_name, errname, name, set_output, Emit, Hex, Line, Num, Pad, Sink};
use rt::proto::{CON_INTR, DBG_CRASH, DBG_HANG, INIT_SHUTDOWN, INIT_STATUS, OK, PING};
use rt::proto::{ROGUE_HW_MODES, ROGUE_ONLY_IN_ALL, SERVICE_RECORD};
use rt::{client, outln, sys};

// Capability slots, filled by `init`.
const CONSOLE: u32 = 0;
const FILES: u32 = 1;
const INFO: u32 = 2;
const CONTROL: u32 = 3;
/// Console and file capabilities with the "program" badge, handed on to programs.
const APP_CONSOLE: u32 = 4;
const APP_FILES: u32 = 5;
/// Harmless capabilities that only the test programs receive, as objects to test with.
const TEST_PORT: u32 = 6;
const TEST_MEMORY: u32 = 7;
const TEST_IRQ: u32 = 8;
const TEST_LOG: u32 = 9;

/// Where a program file is loaded before it is started.
const IMAGE_BASE: u32 = USER_BASE + 0x1000_0000;
/// Memory quota of an ordinary program, in pages.
const APP_PAGES: u32 = 256;
const APP_STACK_PAGES: u32 = 16;
const EMPTY: &[u8] = &[];

/// A task number, or a dash for "none", right-aligned.
struct TaskRef(u32, usize);

impl Emit for TaskRef {
    fn emit(&self, line: &mut Line) {
        if self.0 == NO_TASK {
            Pad(b"", self.1 - 1).emit(line);
            line.push(b"-");
        } else {
            Num(self.0, self.1).emit(line);
        }
    }
}

/// The interrupt lines in a mask, as "1, 4".
struct Lines(u32);

impl Emit for Lines {
    fn emit(&self, line: &mut Line) {
        let mut first = true;
        for irq in 0..16u32 {
            if self.0 & (1 << irq) != 0 {
                if !first {
                    line.push(b", ");
                }
                first = false;
                irq.emit(line);
            }
        }
    }
}

/// Where a capability was copied from, as "  <- task 3 slot 6"; nothing for an original.
struct Origin(u32);

impl Emit for Origin {
    fn emit(&self, line: &mut Line) {
        if self.0 != NO_CAP {
            line.push(b"  <- task ");
            (self.0 >> 8).emit(line);
            line.push(b" slot ");
            (self.0 & 0xFF).emit(line);
        }
    }
}

/// Microseconds as seconds with three decimals.
struct Seconds(u64);

impl Emit for Seconds {
    fn emit(&self, line: &mut Line) {
        let millis = (self.0 / 1000 % 1000) as u32;
        (self.0 / 1_000_000).emit(line);
        let digits = [b'0' + (millis / 100) as u8, b'0' + (millis / 10 % 10) as u8, b'0' + (millis % 10) as u8];
        line.push(b".");
        line.push(&digits);
    }
}

fn words(line: &[u8]) -> [&[u8]; 4] {
    let mut words = [EMPTY; 4];
    let mut parts = line.split(|&byte| byte == b' ' || byte == b'\t').filter(|word| !word.is_empty());
    for slot in words.iter_mut() {
        match parts.next() {
            Some(word) => *slot = word,
            None => break,
        }
    }
    words
}

fn number(word: &[u8]) -> Option<u32> {
    if word.is_empty() {
        return None;
    }
    let mut value: u32 = 0;
    for &byte in word {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add((byte - b'0') as u32)?;
    }
    Some(value)
}

fn help() {
    outln!("Commands:");
    outln!("  help                    this list");
    outln!("  ls                      files in the boot archive");
    outln!("  cat FILE                print a text file");
    outln!("  ps                      tasks with their memory quota and CPU time");
    outln!("  caps TASK               capabilities a task holds (task number or name)");
    outln!("  mem                     memory use");
    outln!("  uptime                  kernel counters and the invariant check");
    outln!("  svc                     supervised services and their restarts");
    outln!("  crash console|fs|init   make that component fail with a page fault");
    outln!("  hang console|fs         make that server stop answering");
    outln!("  rogue [all]             run a hostile program and count what it is denied");
    outln!("  shutdown                switch the machine off");
    outln!("  NAME [N ...]            run a program: hello, selftest, bench, fuzz 100000");
    outln!("Ctrl-C stops the running program.");
}

fn list() {
    let mut index = 0;
    while let Some((file, size)) = client::list(FILES, index) {
        outln!("  ", Pad(name(&file), 12), Num(size, 8), " bytes");
        index += 1;
    }
}

fn cat(file: &[u8]) {
    let mut chunk = [0; MSG_DATA];
    let mut offset = 0;
    loop {
        match client::read(FILES, file, offset, &mut chunk) {
            None => return outln!("cat: ", file, ": no such file"),
            Some(0) => return,
            Some(count) => {
                if offset == 0 && chunk.starts_with(b"\x7FELF") {
                    return outln!("cat: ", file, " is a program; type its name to run it");
                }
                rt::io::write(&chunk[..count]);
                offset += count as u32;
            }
        }
    }
}

fn state_name(state: u32) -> &'static str {
    match state {
        ST_READY => "ready",
        ST_RECV => "receive",
        ST_SEND => "send",
        ST_REPLY => "reply",
        ST_SLEEP => "sleep",
        ST_ZOMBIE => "ended",
        _ => "free",
    }
}

fn tasks() {
    outln!("task  name       state    parent  pages  quota  cpu ticks  syscalls  caps");
    for index in 0..MAX_TASKS as u32 {
        let Ok(task) = sys::task_info(INFO, index) else { continue };
        if task.state == ST_FREE {
            continue;
        }
        outln!(
            Num(index, 4), "  ", Pad(name(&task.name), 11), Pad(state_name(task.state).as_bytes(), 8),
            TaskRef(task.parent, 7), Num(task.charged, 7), Num(task.quota, 7),
            Num(task.cpu_ticks, 11), Num(task.syscalls, 10), Num(task.caps, 6)
        );
    }
}

fn find_task(word: &[u8]) -> Option<u32> {
    if let Some(index) = number(word) {
        return (index < MAX_TASKS as u32).then_some(index);
    }
    (0..MAX_TASKS as u32).find(|&index| {
        sys::task_info(INFO, index)
            .is_ok_and(|task| task.state != ST_FREE && name(&task.name) == word)
    })
}

fn rights_name(rights: u32) -> &'static str {
    match rights & 7 {
        1 => "send",
        2 => "receive",
        3 => "send + receive",
        4 => "grant",
        5 => "send + grant",
        6 => "receive + grant",
        7 => "send + receive + grant",
        _ => "no rights",
    }
}

fn describe(slot: u32, cap: &CapInfo) {
    let (slot, from) = (Num(slot, 4), Origin(cap.from));
    match cap.kind {
        CAP_ENDPOINT => {
            let rights = rights_name(cap.b);
            outln!(slot, "  endpoint ", cap.a, ": ", rights, ", badge ", cap.c, from)
        }
        CAP_IOPORT => outln!(slot, "  I/O ports ", Hex(cap.a), " to ", Hex(cap.b), from),
        CAP_IRQ => outln!(slot, "  interrupt lines ", Lines(cap.a), from),
        CAP_MEMORY => {
            let access = if cap.c != 0 { "read and write" } else { "read only" };
            outln!(slot, "  memory at ", Hex(cap.a), ", ", cap.b, " pages, ", access, from)
        }
        CAP_TASK => outln!(slot, "  task ", cap.a, " (may end it)", from),
        CAP_LOG => outln!(slot, "  kernel log (write)", from),
        CAP_INFO => outln!(slot, "  kernel statistics (read)", from),
        _ => {}
    }
}

fn capabilities(word: &[u8]) {
    let Some(index) = find_task(word) else {
        return outln!("usage: caps TASK, with a task number or name from ps");
    };
    let Ok(task) = sys::task_info(INFO, index) else { return };
    if task.state == ST_FREE {
        return outln!("caps: there is no task ", index);
    }
    outln!("Task ", index, " (", name(&task.name), ") holds ", task.caps, " capabilities:");
    let mut copies = false;
    for slot in 0..CAP_SLOTS as u32 {
        if let Ok(cap) = sys::cap_info(INFO, index, slot) {
            describe(slot, &cap);
            copies |= cap.from != NO_CAP;
        }
    }
    if copies {
        outln!("An arrow marks a copy: revoking the capability it names takes the copy away.");
    }
    let (tasks, endpoints) = (task.tasks, task.endpoints);
    outln!("It may start ", tasks, " more tasks and create ", endpoints, " more endpoints.");
}

fn invariants() {
    match sys::check(INFO) {
        Ok(0) => outln!("kernel invariants: all hold"),
        Ok(rule) => outln!("kernel invariants: RULE ", rule, " VIOLATED"),
        Err(code) => outln!("kernel invariants: cannot check (", errname(code), ")"),
    }
}

fn memory(quota: u32) {
    let Ok(stats) = sys::stats(INFO) else { return };
    outln!(
        "user memory: ", stats.total_frames, " pages of 4 KiB, ", stats.free_frames, " free"
    );
    outln!("kernel and boot archive: ", stats.kernel_pages, " pages");
    outln!("this shell may use up to ", quota, " pages; an ordinary program ", APP_PAGES);
}

fn uptime() {
    let Ok(stats) = sys::stats(INFO) else { return };
    outln!("up ", Seconds(sys::time()), " s, ", stats.ticks, " timer interrupts");
    outln!(
        stats.tasks, " tasks, ", stats.endpoints, " endpoints, ", stats.syscalls,
        " system calls, ", stats.switches, " task switches"
    );
    outln!(stats.irqs, " device interrupts, ", stats.faults, " faults contained");
    outln!(stats.spawns, " tasks started, ", stats.anomalies, " kernel anomalies");
    invariants();
}

fn services() {
    let mut msg = Msg::new(INIT_STATUS);
    if client::request(CONTROL, &mut msg).is_err() || msg.label != OK {
        return outln!("svc: the supervisor did not answer");
    }
    outln!("service   state  restarts  last end                  last restart took");
    let count = (msg.words[0] as usize).min(MSG_DATA / SERVICE_RECORD);
    for index in 0..count {
        let record = &msg.data[index * SERVICE_RECORD..][..SERVICE_RECORD];
        let word = |at: usize| {
            let field = &record[8 + at * 4..][..4];
            u32::from_le_bytes([field[0], field[1], field[2], field[3]])
        };
        let state = if word(3) != 0 { "up" } else { "down" };
        let service = Pad(name(&record[..8]), 10);
        if word(0) == 0 {
            outln!(service, Pad(state.as_bytes(), 7), Num(0, 8), "  -");
        } else {
            outln!(
                service, Pad(state.as_bytes(), 7), Num(word(0), 8), "  ",
                Pad(cause_name(word(1)).as_bytes(), 26), word(2), " us"
            );
        }
    }
}

/// Was Ctrl-C pressed since this was last asked?
fn interrupted() -> bool {
    let mut msg = Msg::new(CON_INTR);
    client::request(CONSOLE, &mut msg).is_ok() && msg.label == OK && msg.words[0] != 0
}

/// Make a server fail on purpose and time how long it takes until it answers again.
fn inject(label: u32, target: &[u8]) {
    let verb = if label == DBG_CRASH { "crash" } else { "hang" };
    let endpoint = match target {
        b"console" => CONSOLE,
        b"fs" => FILES,
        b"init" if label == DBG_CRASH => {
            outln!("Crashing the root task. The kernel will start user space again.");
            let _ = sys::call(CONTROL, &mut Msg::new(DBG_CRASH), FOREVER);
            return;
        }
        _ => return outln!("usage: crash console|fs|init, or hang console|fs"),
    };
    let start = sys::time();
    // The server dies while it holds this request, so the kernel answers E_DEAD.
    let sent = sys::call(endpoint, &mut Msg::new(label), FOREVER);
    // The next request waits in the endpoint until the restarted server takes it.
    let answered = client::request(endpoint, &mut Msg::new(PING));
    let elapsed = sys::time() - start;
    match (sent, answered) {
        (Err(E_DEAD), Ok(())) => {
            outln!("RECOVERY: ", target, " ", verb, " -> answering again after ", elapsed, " us")
        }
        (Ok(()), _) => outln!(verb, ": ", target, " refused the request"),
        (Err(code), _) => outln!(verb, " ", target, ": unexpected result ", errname(code)),
    }
}

fn shutdown() {
    let _ = client::request(CONTROL, &mut Msg::new(INIT_SHUTDOWN));
    sys::sleep(500);
    outln!("shutdown: this machine cannot be switched off by software; it is still running");
}

fn report(program: &[u8], end: &Msg) {
    let [cause, detail, eip, _] = end.words;
    match cause {
        CAUSE_EXIT if detail == 0 => {}
        CAUSE_EXIT if detail == 0xDEAD => outln!("[", program, " stopped after a panic]"),
        CAUSE_EXIT => outln!("[", program, " exited with code ", detail, "]"),
        CAUSE_KILLED => outln!("[", program, " was stopped]"),
        _ if cause < 32 => outln!(
            "[", program, " ended: ", cause_name(cause), " at address ", Hex(detail), ", eip ",
            Hex(eip), "]"
        ),
        _ => outln!("[", program, " ended: ", cause_name(cause), "]"),
    }
}

struct Shell {
    /// Endpoint on which the kernel reports the end of a started program.
    notices: u32,
    launches: u32,
    quota: u32,
    /// The shell's own number in the task table, where its unused quotas can be read.
    me: u32,
}

impl Shell {
    /// Start a program and wait until it ends; returns the kernel's end-of-task notice.
    fn run(&mut self, program: &[u8], args: [u32; 3]) -> Option<Msg> {
        if client::stat(FILES, program).is_none() {
            outln!(program, ": no such command or program; type help");
            return None;
        }
        let Some(image) = client::load(FILES, program, IMAGE_BASE) else {
            outln!(program, ": cannot be loaded");
            return None;
        };
        if !image.bytes().starts_with(b"\x7FELF") {
            outln!(program, " is not a program; try cat ", program);
            return None;
        }
        // Least privilege: a program gets the console and nothing else. Only the three
        // test programs get more, because their job is to probe the kernel.
        let tester = matches!(program, b"selftest" | b"fuzz" | b"bench");
        let quota = if tester { self.quota / 2 } else { APP_PAGES };
        let mut spawn = SpawnArgs::new(program, quota, APP_STACK_PAGES);
        spawn.caps[0] = APP_CONSOLE;
        if tester {
            let extra = [APP_FILES, INFO, TEST_PORT, TEST_MEMORY, TEST_IRQ, TEST_LOG];
            spawn.caps[1..7].copy_from_slice(&extra);
            // One test program runs at a time. It gets every task and endpoint the shell may
            // still make; the kernel hands them back to the shell when the program ends.
            if let Ok(left) = sys::task_info(INFO, self.me) {
                spawn.tasks = left.tasks.saturating_sub(1);
                spawn.endpoints = left.endpoints;
            }
        } else if program == b"rogue" {
            // Room for one task, so that the kernel judges the hostile program's attempts to
            // start one by what they ask for and not by an empty quota.
            spawn.tasks = 1;
        }
        self.launches += 1;
        let badge = self.launches;
        spawn.fault_ep = self.notices;
        spawn.fault_badge = badge;
        spawn.args = [args[0], args[1], args[2], quota];
        let _ = interrupted(); // forget a Ctrl-C from before this program
        let started = sys::spawn(image.bytes(), &spawn);
        drop(image);
        let task = match started {
            Ok(task) => task,
            Err(code) => {
                outln!(program, ": cannot be started: ", errname(code));
                return None;
            }
        };
        let end = loop {
            let mut msg = Msg::new(0);
            match sys::recv(self.notices, &mut msg, 100) {
                Ok(()) if msg.label == L_FAULT && msg.badge == badge => break msg,
                Ok(()) => {}
                Err(_) => {
                    if interrupted() {
                        let _ = sys::task_kill(task);
                    }
                }
            }
        };
        let _ = sys::cap_drop(task);
        Some(end)
    }

    /// Run the hostile program: once for its system-call attacks, then once per
    /// hardware-level attack, each of which must end in a CPU exception. One attack makes
    /// the v86 browser emulator itself fail, so it runs only when `all` is asked for;
    /// otherwise it is printed and counted as skipped.
    fn rogue(&mut self, all: bool) {
        outln!("Starting a hostile program that holds one capability: console output.");
        let mut denied = 0;
        let mut total = 0;
        let mut skipped = 0u32;
        match self.run(b"rogue", [0; 3]) {
            Some(end) if end.words[0] == CAUSE_EXIT => {
                denied += end.words[1] >> 16;
                total += end.words[1] & 0xFFFF;
            }
            Some(end) => {
                report(b"rogue", &end);
                total += 1;
            }
            None => return,
        }
        for mode in 1..=ROGUE_HW_MODES {
            let skip = mode == ROGUE_ONLY_IN_ALL && !all;
            let end = self.run(b"rogue", [mode, skip as u32, 0]);
            if skip {
                skipped += 1;
                outln!("skipped");
                continue;
            }
            total += 1;
            match end {
                Some(end) if end.words[0] < 32 => {
                    denied += 1;
                    outln!("denied: ", cause_name(end.words[0]));
                }
                Some(_) => outln!("NOT DENIED"),
                None => {}
            }
        }
        if skipped == 0 {
            outln!("ROGUE: ", denied, "/", total, " attempts denied");
        } else {
            outln!("ROGUE: ", denied, "/", total, " attempts denied, ", skipped, " skipped");
            outln!("The skipped attempt stops the v86 browser emulator; \"rogue all\" runs it.");
        }
        invariants();
    }

    fn command(&mut self, line: &[u8]) {
        let [verb, first, second, third] = words(line);
        match verb {
            b"" => {}
            b"help" => help(),
            b"ls" => list(),
            b"cat" => cat(first),
            b"ps" => tasks(),
            b"caps" => capabilities(first),
            b"mem" => memory(self.quota),
            b"uptime" => uptime(),
            b"svc" => services(),
            b"crash" => inject(DBG_CRASH, first),
            b"hang" => inject(DBG_HANG, first),
            b"rogue" => self.rogue(first == b"all"),
            b"shutdown" => shutdown(),
            program => {
                let args = [first, second, third].map(|word| number(word).unwrap_or(0));
                if let Some(end) = self.run(program, args) {
                    report(program, &end);
                }
            }
        }
    }
}

#[no_mangle]
pub fn main() {
    set_output(Sink::Console(CONSOLE));
    let [restarts, _, _, quota] = rt::args();
    let Ok(notices) = sys::ep_create() else {
        outln!("shell: cannot create an endpoint");
        sys::exit(1)
    };
    let Some(me) = client::task_named(INFO, b"shell") else {
        outln!("shell: cannot find itself in the task table");
        sys::exit(1)
    };
    if restarts > 0 {
        outln!("[shell restarted]");
    }
    outln!("Inti ", env!("CARGO_PKG_VERSION"), " shell ready at t=", sys::time(), " us");
    outln!("Type help for the list of commands.");
    let mut shell = Shell { notices, launches: 0, quota, me };
    let mut line = [0; 120];
    loop {
        let len = client::readline(CONSOLE, b"inti> ", &mut line);
        shell.command(&line[..len]);
    }
}
