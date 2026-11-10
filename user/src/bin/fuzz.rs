//! fuzz: throws random system calls at the kernel and checks that it stays whole.
//!
//! `fuzz N [seed]` keeps four kids running (`tchild` in fuzz mode) until they have made at
//! least N system calls with random or almost-right arguments. A kid that ends itself,
//! faults or gets stuck is replaced by a new one. While they run, the fuzzer asks the
//! kernel to check its own invariants, and at the end it checks that no task, endpoint,
//! page frame or quota went missing. The last line is `FUZZ: PASS` or `FUZZ: FAIL`.
//!
//! The count is a lower bound. It is the kernel's own count of each kid's system calls,
//! less the calls a kid makes to set itself up and to report. Calls made by the kids'
//! children are not counted at all.
//!
//! Capabilities, handed over by the shell: 0 console, 1 files, 2 Info, 3 I/O port 0x80,
//! 4 the archive (read-only memory), 5 IRQ 5, 6 Log. A kid gets a send-only capability to
//! the fuzzer and copies of 2 to 5. It gets no Log capability, so that it cannot bury the
//! kernel log in noise. It also gets a share of the fuzzer's quota of tasks and endpoints,
//! so that it can start children and create endpoints of its own. Its capability to the
//! fuzzer carries no grant right, so no capability of a kid can reach the fuzzer.
#![no_std]
#![no_main]

use rt::abi::{
    nr, KStats, Msg, SpawnArgs, TaskInfo, CAUSE_EXIT, CAUSE_KILLED, L_FAULT, R_SEND, ST_READY,
};
use rt::client::{self, Mapping};
use rt::io::{set_output, Num, Pad, Sink};
use rt::proto::{self, mode};
use rt::{outln, sys};

const CONSOLE: u32 = 0;
const FILES: u32 = 1;
const INFO: u32 = 2;
const PORT: u32 = 3;
const ARCHIVE: u32 = 4;
const IRQ: u32 = 5;

/// Where the `tchild` program is loaded in this program's address space.
const CHILD_AT: u32 = 0x5000_0000;
/// How many system calls to make when the command line gives no number.
const DEFAULT_CALLS: u32 = 20_000;
/// How many kids run at the same time.
const PLACES: usize = 4;
/// Memory quota and stack of a kid, in pages.
const KID_PAGES: u32 = 128;
const KID_STACK: u32 = 4;
/// How many tasks a kid may start and how many endpoints it may create.
const KID_TASKS: u32 = 5;
const KID_ENDPOINTS: u32 = 8;
/// The badge on every kid's capability to the fuzzer.
const KID_BADGE: u32 = 1;
/// A kid is ended when it is this old,
const KID_LIFE_US: u64 = 400_000;
/// or when two looks this far apart find it waiting with no new system call made.
const LOOK_EVERY_US: u64 = 20_000;
/// The longest the fuzzer goes without checking the kernel's invariants.
const CHECK_EVERY_US: u64 = 25_000;
/// How long the fuzzer waits for a message before it looks at its kids again,
const LISTEN_MS: u32 = 10;
/// and how many messages it handles before it does.
const BATCH: u32 = 64;
/// With no new calls counted for this long, the fuzzer gives up.
const STALL_US: u64 = 10_000_000;
/// How long the fuzzer waits at the end for kids to end and for the counts to settle.
const SETTLE_US: u64 = 2_000_000;

/// The system calls by number. Numbers that do not exist share the last entry.
const NAMES: [&str; 23] = [
    "yield",
    "exit",
    "call",
    "recv",
    "reply",
    "ep_create",
    "cap_derive",
    "cap_drop",
    "map",
    "unmap",
    "map_mem",
    "io_in",
    "io_out",
    "irq_bind",
    "irq_ack",
    "spawn",
    "task_kill",
    "sleep",
    "time",
    "log",
    "sysinfo",
    "cap_revoke",
    "no such call",
];
const _: () = assert!(nr::COUNT as usize + 1 == NAMES.len());
const _: () = assert!(proto::FUZZ_NUMBERS >= NAMES.len());

/// What must be the same before and after: tasks, endpoints, page frames (the free ones
/// plus the ones this program holds itself), and this program's own quota of tasks and
/// endpoints, which the kids borrow from.
#[derive(Clone, Copy, PartialEq)]
struct Usage {
    tasks: u32,
    endpoints: u32,
    frames: u32,
    task_quota: u32,
    endpoint_quota: u32,
}

fn stats() -> KStats {
    sys::stats(INFO).unwrap_or(KStats::ZERO)
}

fn usage(me: u32) -> Usage {
    let now = stats();
    let own = sys::task_info(INFO, me).unwrap_or(TaskInfo::ZERO);
    Usage {
        tasks: now.tasks,
        endpoints: now.endpoints,
        frames: now.free_frames + own.charged,
        task_quota: own.tasks,
        endpoint_quota: own.endpoints,
    }
}

/// A different, unrelated seed for every kid (the finishing steps of MurmurHash3).
fn kid_seed(seed: u32, launch: u32) -> u32 {
    let mut x = seed.wrapping_add(launch.wrapping_mul(0x9E37_79B9));
    x ^= x >> 16;
    x = x.wrapping_mul(0x85EB_CA6B);
    x ^= x >> 13;
    x = x.wrapping_mul(0xC2B2_AE35);
    x ^ (x >> 16)
}

/// A running kid.
#[derive(Clone, Copy)]
struct Kid {
    /// The slot of its Task capability, and its number in the task table.
    task: u32,
    index: u32,
    /// The badge its end will be reported under.
    badge: u32,
    born: u64,
    /// When the fuzzer last looked at it, and how many system calls it had made by then.
    looked: u64,
    calls_seen: u32,
    /// True once the fuzzer has asked the kernel to end it.
    killed: bool,
}

struct Fuzzer {
    image: Mapping,
    /// This program's own number in the task table.
    me: u32,
    /// Where kids' reports arrive and where the kernel reports their ends.
    inbox: u32,
    /// The send-only capability to the inbox that every kid gets.
    kid_cap: u32,
    archive_pages: u32,
    seed: u32,
    launches: u32,
    places: [Option<Kid>; PLACES],
    /// Random system calls made by the kids that have ended.
    calls: u32,
    ended: u32,
    exited: u32,
    killed: u32,
    crashed: u32,
    /// Invariant checks run by the fuzzer and by the kids.
    checks: u32,
    checked_at: u64,
    violations: u32,
    /// The first rule that a check found broken.
    rule: u32,
    /// What the kids said they tried, and what the kernel accepted, by system call.
    tried: [u32; NAMES.len()],
    accepted: [u32; NAMES.len()],
}

impl Fuzzer {
    fn new(seed: u32) -> Option<Fuzzer> {
        let image = client::load(FILES, b"tchild", CHILD_AT)?;
        let me = client::task_named(INFO, b"fuzz")?;
        let archive_pages = sys::cap_info(INFO, me, ARCHIVE).ok()?.b;
        let inbox = sys::ep_create().ok()?;
        let kid_cap = sys::cap_derive(inbox, R_SEND, KID_BADGE, 0).ok()?;
        Some(Fuzzer {
            image,
            me,
            inbox,
            kid_cap,
            archive_pages,
            seed,
            launches: 0,
            places: [None; PLACES],
            calls: 0,
            ended: 0,
            exited: 0,
            killed: 0,
            crashed: 0,
            checks: 0,
            checked_at: 0,
            violations: 0,
            rule: 0,
            tried: [0; NAMES.len()],
            accepted: [0; NAMES.len()],
        })
    }

    /// Starts a kid in every empty place, each with a share of this program's quota of
    /// tasks and endpoints. A place that cannot be filled now is tried again on the next
    /// round.
    fn fill(&mut self) {
        for at in 0..PLACES {
            if self.places[at].is_some() {
                continue;
            }
            self.launches += 1;
            let mut spawn = SpawnArgs::new(b"fuzz-kid", KID_PAGES, KID_STACK);
            spawn.fault_ep = self.inbox;
            spawn.fault_badge = self.launches;
            spawn.tasks = KID_TASKS;
            spawn.endpoints = KID_ENDPOINTS;
            spawn.caps[..5].copy_from_slice(&[self.kid_cap, INFO, PORT, ARCHIVE, IRQ]);
            spawn.args = [mode::FUZZ, kid_seed(self.seed, self.launches), self.archive_pages, 0];
            let Ok(task) = sys::spawn(self.image.bytes(), &spawn) else {
                continue;
            };
            let now = sys::time();
            self.places[at] = Some(Kid {
                task,
                index: sys::cap_info(INFO, self.me, task).map_or(0, |cap| cap.a),
                badge: self.launches,
                born: now,
                looked: now,
                calls_seen: 0,
                killed: false,
            });
        }
    }

    fn violation(&mut self, rule: u32) {
        self.violations += 1;
        if self.rule == 0 {
            self.rule = rule;
        }
    }

    /// Asks the kernel to check its invariants.
    fn audit(&mut self) {
        self.checked_at = sys::time();
        self.checks += 1;
        match sys::check(INFO) {
            Ok(0) => {}
            Ok(rule) => self.violation(rule),
            Err(_) => self.violation(u32::MAX),
        }
    }

    /// The kernel says that a kid has ended.
    fn kid_ended(&mut self, msg: &Msg) {
        let found = self
            .places
            .iter()
            .position(|place| place.is_some_and(|kid| kid.badge == msg.badge));
        let Some(at) = found else {
            return;
        };
        if let Some(kid) = self.places[at].take() {
            let _ = sys::cap_drop(kid.task);
        }
        let made = u32::from_le_bytes([msg.data[0], msg.data[1], msg.data[2], msg.data[3]]);
        let not_random = proto::FUZZ_SETUP_CALLS + made / proto::FUZZ_REPORT_EVERY + 2;
        self.calls = self.calls.saturating_add(made.saturating_sub(not_random));
        self.ended += 1;
        match msg.words[0] {
            CAUSE_EXIT => self.exited += 1,
            CAUSE_KILLED => self.killed += 1,
            _ => self.crashed += 1,
        }
        // Ending a task is the most involved thing the kernel does, so look right after it.
        self.audit();
    }

    /// A kid says what it tried since its last report. Anything that does not add up is
    /// not a report but one of the kid's random calls, and is ignored.
    fn report(&mut self, msg: &Msg) {
        let calls = msg.words[0];
        if msg.len as usize != 4 * proto::FUZZ_NUMBERS || calls > proto::FUZZ_REPORT_EVERY {
            return;
        }
        let mut tried = [0u32; proto::FUZZ_NUMBERS];
        let mut accepted = [0u32; proto::FUZZ_NUMBERS];
        let records = msg.data.chunks_exact(4).take(proto::FUZZ_NUMBERS);
        for (index, record) in records.enumerate() {
            tried[index] = u32::from(u16::from_le_bytes([record[0], record[1]]));
            accepted[index] = u32::from(u16::from_le_bytes([record[2], record[3]]));
        }
        let adds_up = tried.iter().sum::<u32>() == calls
            && tried.iter().zip(&accepted).all(|(tried, accepted)| accepted <= tried)
            && msg.words[1] <= calls;
        if !adds_up {
            return;
        }
        for index in 0..proto::FUZZ_NUMBERS {
            let name = index.min(NAMES.len() - 1);
            self.tried[name] = self.tried[name].saturating_add(tried[index]);
            self.accepted[name] = self.accepted[name].saturating_add(accepted[index]);
        }
        self.checks = self.checks.saturating_add(msg.words[1]);
    }

    fn handle(&mut self, msg: &Msg) {
        match msg.label {
            // Only the kernel can send this label; it expects no reply.
            L_FAULT => return self.kid_ended(msg),
            proto::FUZZ_REPORT => self.report(msg),
            proto::FUZZ_VIOLATION => self.violation(msg.words[0]),
            _ => {}
        }
        let _ = sys::reply(msg.token, &Msg::new(proto::OK));
    }

    /// Waits a moment for a message, then handles what has arrived.
    fn listen(&mut self) {
        let mut msg = Msg::new(0);
        let mut wait_ms = LISTEN_MS;
        for _ in 0..BATCH {
            if sys::recv(self.inbox, &mut msg, wait_ms).is_err() {
                return;
            }
            self.handle(&msg);
            wait_ms = 0;
        }
    }

    /// Ends the kids that are old, and the ones that wait for something that will not come.
    fn cull(&mut self) {
        let now = sys::time();
        for kid in self.places.iter_mut().flatten() {
            if kid.killed {
                continue;
            }
            let mut end = now >= kid.born + KID_LIFE_US;
            if !end && now >= kid.looked + LOOK_EVERY_US {
                let info = sys::task_info(INFO, kid.index).unwrap_or(TaskInfo::ZERO);
                end = info.state != ST_READY && info.syscalls == kid.calls_seen;
                kid.looked = now;
                kid.calls_seen = info.syscalls;
            }
            if end {
                let _ = sys::task_kill(kid.task);
                kid.killed = true;
            }
        }
    }

    /// Ends every kid and waits until the kernel has reported each end.
    fn stop_kids(&mut self) {
        for kid in self.places.iter_mut().flatten() {
            if !kid.killed {
                let _ = sys::task_kill(kid.task);
                kid.killed = true;
            }
        }
        let deadline = sys::time() + SETTLE_US;
        while self.places.iter().any(Option::is_some) && sys::time() < deadline {
            self.listen();
        }
    }

    /// Runs kids until they have made `wanted` calls. Returns true if the kernel held up.
    fn run(&mut self, wanted: u32) -> bool {
        let before = usage(self.me);
        let started = sys::time();
        let mut progress_at = started;
        let mut shown = 0;
        let mut stalled = false;
        while self.calls < wanted && self.violations == 0 {
            let had = self.calls;
            self.fill();
            self.listen();
            self.cull();
            let now = sys::time();
            if now >= self.checked_at + CHECK_EVERY_US {
                self.audit();
            }
            if self.calls != had {
                progress_at = now;
            } else if now >= progress_at + STALL_US {
                stalled = true;
                break;
            }
            let tenths = (u64::from(self.calls) * 10 / u64::from(wanted)).min(10) as u32;
            if tenths > shown {
                shown = tenths;
                outln!(
                    "  ",
                    Num(tenths * 10, 3),
                    "%  ",
                    self.calls,
                    " calls, ",
                    self.ended,
                    " tasks ended, ",
                    self.checks,
                    " checks"
                );
            }
        }
        self.stop_kids();
        let elapsed_ms = (sys::time() - started) / 1000;
        self.audit();

        // The kids' children are reaped as their parents end; give the counts a moment.
        let deadline = sys::time() + SETTLE_US;
        let mut after = usage(self.me);
        while after != before && sys::time() < deadline {
            sys::sleep(10);
            after = usage(self.me);
        }
        let anomalies = stats().anomalies;
        let leaks = u32::from(after.tasks != before.tasks)
            + u32::from(after.endpoints != before.endpoints)
            + u32::from(after.frames != before.frames)
            + u32::from(after.task_quota != before.task_quota)
            + u32::from(after.endpoint_quota != before.endpoint_quota);
        let unstoppable = self.places.iter().flatten().count();

        if stalled {
            outln!("fuzz: no new calls were counted for 10 s; giving up");
        }
        outln!(
            "FUZZ: ",
            self.calls,
            " random system calls from ",
            self.ended,
            " tasks in ",
            elapsed_ms,
            " ms"
        );
        outln!(
            "FUZZ: ",
            self.checks,
            " invariant checks, ",
            self.violations,
            " violations, ",
            anomalies,
            " kernel anomalies, ",
            leaks,
            " leaks"
        );
        outln!(
            "FUZZ: tasks ended: ",
            self.exited,
            " exited, ",
            self.killed,
            " were stopped, ",
            self.crashed,
            " hit a CPU exception"
        );
        if self.violations != 0 {
            outln!("FUZZ: the first rule found broken was rule ", self.rule);
        }
        if leaks != 0 {
            outln!("FUZZ: tasks ", before.tasks, " -> ", after.tasks);
            outln!("FUZZ: endpoints ", before.endpoints, " -> ", after.endpoints);
            outln!("FUZZ: page frames ", before.frames, " -> ", after.frames);
            outln!("FUZZ: task quota ", before.task_quota, " -> ", after.task_quota);
            outln!("FUZZ: endpoint quota ", before.endpoint_quota, " -> ", after.endpoint_quota);
        }
        if unstoppable != 0 {
            outln!("FUZZ: ", unstoppable, " tasks did not end when they were stopped");
        }
        self.table();
        self.calls >= wanted
            && self.violations == 0
            && anomalies == 0
            && leaks == 0
            && unstoppable == 0
    }

    /// Prints what the kids tried, by system call.
    fn table(&self) {
        outln!("Calls the tasks reported before they ended (tried / accepted by the kernel):");
        let half = NAMES.len() / 2;
        for left in 0..half {
            let right = left + half;
            outln!(
                "  ",
                Pad(NAMES[left].as_bytes(), 13),
                Num(self.tried[left], 8),
                " /",
                Num(self.accepted[left], 8),
                "    ",
                Pad(NAMES[right].as_bytes(), 13),
                Num(self.tried[right], 8),
                " /",
                Num(self.accepted[right], 8)
            );
        }
    }
}

#[no_mangle]
pub fn main() {
    set_output(Sink::Console(CONSOLE));
    let [wanted, seed, _, _] = rt::args();
    let wanted = if wanted == 0 { DEFAULT_CALLS } else { wanted };
    let seed = if seed == 0 { sys::time() as u32 | 1 } else { seed };
    let Some(mut fuzzer) = Fuzzer::new(seed) else {
        outln!("fuzz: could not set up (is tchild missing, or the endpoint table full?)");
        outln!("FUZZ: FAIL");
        return;
    };
    outln!("Fuzzing the kernel with ", wanted, " random system calls, seed ", seed, ".");
    if fuzzer.run(wanted) {
        outln!("FUZZ: PASS");
    } else {
        outln!("FUZZ: FAIL");
    }
}
