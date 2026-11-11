//! Bounded exploration: every enabled step from a small start, breadth first.
//!
//! A scenario (`Profile`) boots the kernel on the fake machine and sets up a few tasks. From
//! there the explorer tries every step that is enabled: each system call the ready tasks
//! could make (chosen from a fixed menu, see `offers`), a fault, a device interrupt, a
//! deadline that passes. After every single step it checks
//!
//! * what `World::verify` checks: the kernel's own invariant checker reports nothing, the
//!   machine agrees with the kernel, no wake-up is lost, every delivered message and reply is
//!   the one that was sent;
//! * what that step promises (`step`): a derived capability is no wider than its source, a
//!   revocation empties exactly the copies, fresh memory is zero, a killed task is gone;
//! * atomicity (`judge`): a system call that fails changes nothing, with one documented
//!   exception;
//! * restart (`judge`): when the root task dies, what is left is a freshly booted system.
//!
//! Two states are the same if their summaries (`state.rs`) hash alike, and a state seen
//! before is not expanded again. The search stops at a depth bound, and it does not expand a
//! state that holds more tasks, endpoints or capabilities than the scenario's budget. This
//! is a search of what short runs of small systems can do. It is not a proof.

use crate::arch;
use crate::mem::{frame_of, RW};
use crate::state::{canonical, deadline_kind, fingerprint, FAR, NEAR};
use crate::task::{link, Cap, State, ORIGINAL};
use crate::world::{Spawn, World, BUFFER, CHILD_FRAMES, LONG, SMALL};
use abi::nr;
use abi::{
    CAP_SLOTS, E_DEAD, E_FAULT, E_INVAL, E_NOCAP, E_NOSYS, E_PERM, E_TIMEOUT, FOREVER, MAP_WRITE, MAX_TASKS, NO_CAP,
    PAGE, R_GRANT, R_RECV, R_SEND,
};
use std::collections::HashSet;
use std::fmt;
use std::time::Instant;

/// The one device interrupt line that the explorer raises.
const LINE: u32 = 1;
/// Where tasks map device memory: read-only at this page, writable at the next.
const DEVICE_VA: u32 = 0x6000_0000;
/// Where tasks map fresh memory, as (address, pages). The second place is the stack page,
/// which is mapped already. The third takes two pages in a region without a page table.
const PLACES: [(u32, u32); 3] = [(0x5000_0000, 1), (BUFFER, 1), (0x5040_0000, 2)];
/// Where tasks unmap: the same places, and the two pages of device memory.
const UNPLACES: [(u32, u32); 4] = [(0x5000_0000, 1), (BUFFER, 1), (0x5040_0000, 2), (DEVICE_VA, 2)];
/// A machine so small that the third task no longer fits.
const TIGHT: usize = 54;
/// Every this many steps, the snapshot machinery itself is checked.
const SELF_CHECK: u64 = 2048;
/// How many violations are printed with their path.
const SHOWN: u64 = 5;

// The families of steps. A scenario turns on the ones it wants.
const IPC: u32 = 1 << 0;
const TIMEOUT: u32 = 1 << 1;
const PASS: u32 = 1 << 2;
const CAPS: u32 = 1 << 3;
const DROP: u32 = 1 << 4;
const REVOKE: u32 = 1 << 5;
const SPAWN: u32 = 1 << 6;
const KILL: u32 = 1 << 7;
const DEATH: u32 = 1 << 8;
const ROOT_DEATH: u32 = 1 << 9;
const MEMORY: u32 = 1 << 10;
const IRQ: u32 = 1 << 11;
const NEGATIVE: u32 = 1 << 12;
const SLEEP: u32 = 1 << 13;
const DEVICE: u32 = 1 << 14;

/// One step. The first number is the task that takes it.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Action {
    EpCreate(u8),
    /// Slot, and which copy: 0 send-only, 1 send and grant with a badge.
    Derive(u8, u8, u8),
    Drop(u8, u8),
    Revoke(u8, u8),
    /// Slot, the capability offered along (see `offered`), and whether with a timeout.
    Call(u8, u8, u8, bool),
    /// Slot, and how long to wait: 0 not at all, 1 forever, 2 with a timeout.
    Recv(u8, u8, u8),
    /// A poll whose buffer lies in device memory.
    RecvDevice(u8, u8),
    /// The client answered, and the capability offered along.
    Reply(u8, u8, u8),
    /// The shape of the new task (see `shape_of`).
    Spawn(u8, u8),
    Kill(u8, u8),
    Exit(u8),
    Fault(u8),
    /// An index into `PLACES`.
    Map(u8, u8),
    /// An index into `UNPLACES`.
    Unmap(u8, u8),
    /// Slot, and whether writable.
    MapMem(u8, u8, bool),
    Sleep(u8),
    /// The slot of the interrupt capability and the slot of the endpoint.
    IrqBind(u8, u8, u8),
    IrqAck(u8, u8),
    /// A request that the kernel must refuse (see `refusal`).
    Refused(u8, u8),
    /// The deadline of a waiting task passes.
    Expire(u8),
    /// The restart delay of the root task passes.
    WakeRoot,
    Interrupt,
}

const SHAPES: [&str; 5] =
    ["a plain task", "a task with its lowest endpoint", "a task with its highest endpoint", "a task that may spawn", "a task with too small a quota"];

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const OFFER: [&str; 3] = ["", " offering its lowest endpoint", " offering its highest endpoint"];
        match *self {
            Action::EpCreate(t) => write!(f, "t{t} creates an endpoint"),
            Action::Derive(t, slot, variant) => {
                let what = if variant == 1 { "a badged copy" } else { "a copy" };
                write!(f, "t{t} derives {what} of slot {slot}")
            }
            Action::Drop(t, slot) => write!(f, "t{t} drops slot {slot}"),
            Action::Revoke(t, slot) => write!(f, "t{t} revokes the copies of slot {slot}"),
            Action::Call(t, slot, offer, timed) => {
                let limit = if timed { " with a timeout" } else { "" };
                write!(f, "t{t} calls through slot {slot}{}{limit}", OFFER[offer as usize])
            }
            Action::Recv(t, slot, wait) => {
                let how = ["polls", "receives on", "receives with a timeout on"][wait as usize];
                write!(f, "t{t} {how} slot {slot}")
            }
            Action::RecvDevice(t, slot) => write!(f, "t{t} polls slot {slot} into device memory"),
            Action::Reply(t, client, offer) => write!(f, "t{t} replies to t{client}{}", OFFER[offer as usize]),
            Action::Spawn(t, shape) => write!(f, "t{t} spawns {}", SHAPES[shape as usize]),
            Action::Kill(t, slot) => write!(f, "t{t} kills the task of slot {slot}"),
            Action::Exit(t) => write!(f, "t{t} exits"),
            Action::Fault(t) => write!(f, "t{t} faults"),
            Action::Map(t, place) => write!(f, "t{t} maps place {place}"),
            Action::Unmap(t, place) => write!(f, "t{t} unmaps place {place}"),
            Action::MapMem(t, slot, writable) => {
                let access = if writable { "writable" } else { "read-only" };
                write!(f, "t{t} maps the memory of slot {slot} {access}")
            }
            Action::Sleep(t) => write!(f, "t{t} sleeps"),
            Action::IrqBind(t, slot, ep) => write!(f, "t{t} binds the line of slot {slot} to slot {ep}"),
            Action::IrqAck(t, slot) => write!(f, "t{t} acknowledges the line of slot {slot}"),
            Action::Refused(t, which) => write!(f, "t{t} makes bad request {which}"),
            Action::Expire(t) => write!(f, "the deadline of t{t} passes"),
            Action::WakeRoot => write!(f, "the restart delay passes"),
            Action::Interrupt => write!(f, "the device interrupts"),
        }
    }
}

/// How a step ended for the task that took it.
#[derive(Clone, Copy, PartialEq)]
enum Outcome {
    Returned(i32),
    /// The system call did not return: the task waits, or it is gone.
    Blocked,
    /// Not a system call: a fault, an interrupt, time passing.
    Event,
}

fn outcome(result: Option<i32>) -> Outcome {
    match result {
        Some(code) => Outcome::Returned(code),
        None => Outcome::Blocked,
    }
}

// ---- What the explorer reads off the state ---------------------------------------------------

/// The slots of task `t` that hold an endpoint capability with all of `rights`.
fn endpoint_slots(w: &World, t: usize, rights: u32) -> Vec<u8> {
    let mut slots = Vec::new();
    for (slot, cap) in w.k.tasks[t].caps.iter().enumerate() {
        if let Cap::Endpoint { rights: held, .. } = *cap {
            if held as u32 & rights == rights {
                slots.push(slot as u8);
            }
        }
    }
    slots
}

/// The capability a message offers along: none, the sender's lowest endpoint, its highest.
fn offered(w: &World, t: usize, choice: u8) -> u32 {
    let slots = endpoint_slots(w, t, 0);
    let slot = match choice {
        1 => slots.first(),
        2 => slots.last(),
        _ => None,
    };
    slot.map_or(NO_CAP, |&slot| slot as u32)
}

fn line_open() -> bool {
    arch::with(|machine| machine.cpu.masked & (1 << LINE) == 0)
}

/// The explorer's own account of "no wider": what a copy of `source` may be.
fn narrowed(source: Cap, copy: Cap) -> bool {
    match (source, copy) {
        (Cap::Endpoint { ep, rights, badge }, Cap::Endpoint { ep: to, rights: kept, badge: worn }) => {
            let badged = worn == badge || (badge == 0 && rights as u32 & R_GRANT != 0);
            to == ep && kept != 0 && kept & !rights == 0 && badged
        }
        (Cap::IoPort { first, last }, Cap::IoPort { first: from, last: to }) => first <= from && from <= to && to <= last,
        (Cap::Irq { mask }, Cap::Irq { mask: kept }) => kept != 0 && kept & !mask == 0,
        (Cap::Memory { page, pages, write }, Cap::Memory { page: at, pages: count, write: writable }) => {
            count != 0 && at >= page && at + count <= page + pages && (write || !writable)
        }
        (source, copy) => source == copy,
    }
}

/// The arguments with which the explorer derives from `source`.
fn derive_arguments(source: Cap, t: usize, variant: u8) -> [u32; 3] {
    match source {
        Cap::Endpoint { .. } if variant == 1 => [R_SEND | R_GRANT, 0x10 + t as u32, 0],
        Cap::Endpoint { .. } => [R_SEND, 0, 0],
        Cap::Memory { .. } => [0, 1, 0],
        Cap::Irq { mask } if mask & (1 << LINE) != 0 => [1 << LINE, 0, 0],
        Cap::Irq { mask } => [mask as u32, 0, 0],
        Cap::IoPort { .. } => [0x60, 0x64, 0],
        _ => [0, 0, 0],
    }
}

/// Is some capability a direct copy of the one that `origin` names?
fn has_copies(w: &World, origin: u16) -> bool {
    w.k.tasks.iter().any(|task| (0..CAP_SLOTS).any(|slot| task.caps[slot] != Cap::Null && task.from[slot] == origin))
}

/// Every capability copied from the one that `origin` names, directly or through copies.
fn descendants(w: &World, origin: u16) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut sources = vec![origin];
    let mut next = 0;
    while next < sources.len() && sources.len() <= MAX_TASKS * CAP_SLOTS {
        let source = sources[next];
        next += 1;
        for (t, task) in w.k.tasks.iter().enumerate() {
            for slot in 0..CAP_SLOTS {
                if task.caps[slot] != Cap::Null && task.from[slot] == source {
                    found.push((t, slot));
                    sources.push(link(t, slot));
                }
            }
        }
    }
    found
}

/// Is `ancestor` the task `t` itself or one of the tasks it descends from?
fn in_line(w: &World, ancestor: usize, t: usize) -> bool {
    let mut at = t;
    for _ in 0..MAX_TASKS {
        if at == ancestor {
            return true;
        }
        if at == 0 {
            return false;
        }
        at = w.k.tasks[at].parent as usize;
        if at >= MAX_TASKS {
            return false;
        }
    }
    false
}

/// What task `t` asks for when it spawns a task of `shape`.
fn shape_of(w: &World, t: usize, shape: u8) -> Spawn {
    let held = endpoint_slots(w, t, 0);
    let fault = endpoint_slots(w, t, R_RECV).first().map_or(NO_CAP, |&slot| slot as u32);
    match shape {
        1 => Spawn { fault, ..Spawn::PLAIN }.with_caps(&[held[0] as u32]),
        2 => Spawn { fault, ..Spawn::PLAIN }.with_caps(&[held[held.len() - 1] as u32]),
        3 => Spawn { quota: 20, tasks: 1, eps: 1, ..Spawn::PLAIN },
        4 => Spawn { quota: CHILD_FRAMES - 1, ..Spawn::PLAIN },
        _ => Spawn::PLAIN,
    }
}

/// How many bad requests `refusal` knows.
const REFUSALS: u8 = 12;

/// Bad request `which` of task `t`: the system call, its arguments and the error it must
/// give (`None`: any error). The result is `None` if the task holds nothing to ask with.
fn refusal(w: &World, t: usize, which: u8) -> Option<(u32, [u32; 4], Option<i32>)> {
    let caps = &w.k.tasks[t].caps;
    let empty = (0..CAP_SLOTS).rev().find(|&slot| caps[slot] == Cap::Null).map(|slot| slot as u32);
    let send_only = (0..CAP_SLOTS)
        .find(|&slot| matches!(caps[slot], Cap::Endpoint { rights, .. } if rights as u32 & R_RECV == 0))
        .map(|slot| slot as u32);
    let sender = endpoint_slots(w, t, R_SEND).first().map(|&slot| slot as u32);
    Some(match which {
        0 => (99, [0; 4], Some(E_NOSYS)),
        1 => (nr::CALL, [empty?, BUFFER, FOREVER, 0], Some(E_NOCAP)),
        2 => (nr::RECV, [send_only?, BUFFER, 0, 0], Some(E_PERM)),
        3 => (nr::CALL, [sender?, 0x7000_0000, FOREVER, 0], Some(E_FAULT)),
        4 => (nr::CAP_DERIVE, [send_only?, R_SEND | R_RECV, 0, 0], Some(E_PERM)),
        5 => (nr::TASK_KILL, [empty?, 0, 0, 0], Some(E_NOCAP)),
        6 => (nr::MAP, [0x1000, 1, 1, 0], Some(E_INVAL)),
        7 => (nr::MAP_MEM, [empty?, DEVICE_VA, 0, 1], Some(E_NOCAP)),
        // A program image of no bytes at all; `step` makes this request through `spawn_image`.
        8 => (nr::SPAWN, [0; 4], None),
        9 => (nr::REPLY, [0, BUFFER, 0, 0], Some(E_INVAL)),
        10 => (nr::IRQ_BIND, [empty?, LINE, 0, 0], Some(E_NOCAP)),
        11 => (nr::CAP_DROP, [empty?, 0, 0, 0], Some(E_NOCAP)),
        _ => return None,
    })
}

fn hash_of(w: &World, summary: &mut Vec<u8>) -> u128 {
    canonical(w, summary);
    fingerprint(summary)
}

// ---- One step, and what it promises ----------------------------------------------------------

/// What fresh memory is filled with after it was seen to be zero: if the kernel ever hands
/// out a frame again without clearing it, the next task that maps it sees this.
static MARK: [u8; 4096] = [0xAA; 4096];

/// Take one step, and note (through `World::expect`) if it did not do what it promises.
fn step(w: &mut World, action: Action) -> Outcome {
    match action {
        Action::EpCreate(t) => {
            let t = t as usize;
            let result = w.ep_create(t);
            if let Some(slot) = result.filter(|&slot| slot >= 0) {
                let task = &w.k.tasks[t];
                let all_rights = matches!(task.caps[slot as usize], Cap::Endpoint { rights: 7, badge: 0, .. });
                let fresh = all_rights && task.from[slot as usize] == ORIGINAL;
                w.expect(fresh, "a new endpoint's capability is not an original with all rights and no badge");
            }
            outcome(result)
        }
        Action::Derive(t, slot, variant) => {
            let (t, slot) = (t as usize, slot as usize);
            let source = w.k.tasks[t].caps[slot];
            let [a, b, c] = derive_arguments(source, t, variant);
            let result = w.derive(t, slot as u32, a, b, c);
            if let Some(copy) = result.filter(|&copy| copy >= 0) {
                let task = &w.k.tasks[t];
                let linked = task.from[copy as usize] == link(t, slot);
                let no_wider = narrowed(source, task.caps[copy as usize]);
                w.expect(linked, "a derived capability is not linked to its source");
                w.expect(no_wider, "a derived capability is wider than its source");
            }
            outcome(result)
        }
        Action::Drop(t, slot) => {
            let (t, slot) = (t as usize, slot as usize);
            let result = w.drop_cap(t, slot as u32);
            let emptied = w.k.tasks[t].caps[slot] == Cap::Null;
            w.expect(result == Some(0) && emptied, "dropping a capability did not empty its slot");
            outcome(result)
        }
        Action::Revoke(t, slot) => {
            let (t, slot) = (t as usize, slot as usize);
            let copies = descendants(w, link(t, slot));
            let before = w.caps();
            let result = w.revoke(t, slot as u32);
            let emptied = copies.iter().all(|&(holder, at)| w.k.tasks[holder].caps[at] == Cap::Null);
            let exact = w.caps() + copies.len() == before;
            let kept = w.k.tasks[t].caps[slot] != Cap::Null;
            w.expect(result == Some(0), "revoking the copies of a held capability failed");
            w.expect(emptied, "a copy survived the revocation of its source");
            w.expect(exact, "a revocation removed a capability that was not a copy of the revoked one");
            w.expect(kept, "a revocation removed the capability it was asked to keep");
            outcome(result)
        }
        Action::Call(t, slot, offer, timed) => {
            let t = t as usize;
            let cap = offered(w, t, offer);
            let result = w.call(t, slot as u32, cap, if timed { LONG } else { FOREVER });
            let early = matches!(result, Some(code) if code >= 0);
            w.expect(!early, "a call returned success before anyone replied");
            outcome(result)
        }
        Action::Recv(t, slot, wait) => {
            let timeout = [0, FOREVER, LONG][wait as usize];
            outcome(w.recv(t as usize, slot as u32, timeout))
        }
        Action::RecvDevice(t, slot) => {
            let result = w.recv_at(t as usize, slot as u32, DEVICE_VA + PAGE, 0);
            w.expect(result == Some(E_FAULT), "a receive into device memory was not refused");
            outcome(result)
        }
        Action::Reply(t, client, offer) => {
            let t = t as usize;
            let token = w.k.tasks[client as usize].token;
            let cap = offered(w, t, offer);
            outcome(w.reply(t, token, cap))
        }
        Action::Spawn(t, shape) => {
            let t = t as usize;
            let spec = shape_of(w, t, shape);
            let result = w.spawn(t, &spec);
            if let Some(slot) = result.filter(|&slot| slot >= 0) {
                let started = match w.k.tasks[t].caps[slot as usize] {
                    Cap::Task { index, gen } => {
                        let child = &w.k.tasks[index as usize];
                        child.state == State::Ready && child.gen == gen && child.parent as usize == t
                    }
                    _ => false,
                };
                w.expect(started, "a spawn did not return a handle for a ready child of the caller");
            }
            if shape == 4 {
                let refused = matches!(result, Some(code) if code < 0);
                w.expect(refused, "a spawn whose quota is too small for its own program succeeded");
            }
            outcome(result)
        }
        Action::Kill(t, slot) => {
            let t = t as usize;
            let Cap::Task { index, gen } = w.k.tasks[t].caps[slot as usize] else {
                panic!("the explorer chose a slot that holds no task handle");
            };
            let victim = index as usize;
            let fresh = w.k.tasks[victim].gen == gen && w.k.tasks[victim].alive();
            let own_line = in_line(w, victim, t);
            let result = w.kill(t, slot as u32);
            // A stale handle is refused; killing oneself or an ancestor does not return.
            let expected = if !fresh {
                Some(E_DEAD)
            } else if own_line {
                None
            } else {
                Some(0)
            };
            let gone = !(w.k.tasks[victim].gen == gen && w.k.tasks[victim].alive());
            w.expect(result == expected, "killing a task did not end the way it must");
            w.expect(gone, "a killed task is still alive");
            outcome(result)
        }
        Action::Exit(t) | Action::Fault(t) => {
            let t = t as usize;
            let gen = w.k.tasks[t].gen;
            if matches!(action, Action::Exit(_)) {
                w.exit(t, 7);
            } else {
                w.fault(t, 0x10);
            }
            let gone = !(w.k.tasks[t].gen == gen && w.k.tasks[t].alive());
            w.expect(gone, "a task that exited or faulted is still alive");
            Outcome::Event
        }
        Action::Map(t, place) => {
            let t = t as usize;
            let (va, pages) = PLACES[place as usize];
            let result = w.map(t, va, pages, true);
            if result == Some(0) {
                let zero = (0..pages).all(|page| w.page_is_zero(t, va + page * PAGE));
                w.expect(zero, "freshly mapped memory is not zero");
                for page in 0..pages {
                    w.poke(t, va + page * PAGE, &MARK);
                }
            }
            outcome(result)
        }
        Action::Unmap(t, place) => {
            let t = t as usize;
            let (va, pages) = UNPLACES[place as usize];
            let result = w.unmap(t, va, pages);
            let pd = w.k.tasks[t].pd;
            let gone = (0..pages).all(|page| w.k.mem.lookup(pd, va + page * PAGE).is_none());
            w.expect(result == Some(0) && gone, "unmapping left a page mapped");
            outcome(result)
        }
        Action::MapMem(t, slot, writable) => {
            let t = t as usize;
            let va = if writable { DEVICE_VA + PAGE } else { DEVICE_VA };
            let count = if writable { 1 | MAP_WRITE } else { 1 };
            let source = w.k.tasks[t].caps[slot as usize];
            let result = w.map_mem(t, slot as u32, va, 0, count);
            if result == Some(0) {
                let right = match (source, w.k.mem.lookup(w.k.tasks[t].pd, va)) {
                    (Cap::Memory { page, .. }, Some(entry)) => {
                        frame_of(entry) == page * PAGE && (entry & RW != 0) == writable
                    }
                    _ => false,
                };
                w.expect(right, "device memory was mapped to the wrong frame or with the wrong access");
            }
            outcome(result)
        }
        Action::Sleep(t) => {
            let result = w.sleep(t as usize, LONG);
            w.expect(result.is_none(), "a task that went to sleep kept running");
            outcome(result)
        }
        Action::IrqBind(t, slot, ep) => {
            let t = t as usize;
            let result = w.irq_bind(t, slot as u32, LINE, ep as u32);
            if result == Some(0) {
                let bound = w.k.irqs[LINE as usize].task as usize == t && line_open();
                w.expect(bound, "a bound interrupt line is not open, or not bound to the caller");
            }
            outcome(result)
        }
        Action::IrqAck(t, slot) => {
            let result = w.irq_ack(t as usize, slot as u32, LINE);
            if result == Some(0) {
                let open = line_open();
                w.expect(open, "an acknowledged interrupt line stayed masked");
            }
            outcome(result)
        }
        Action::Refused(t, which) => {
            let t = t as usize;
            let Some((number, args, expected)) = refusal(w, t, which) else {
                panic!("the explorer chose a bad request that the task cannot make");
            };
            let result =
                if number == nr::SPAWN { w.spawn_image(t, &Spawn::PLAIN, 0) } else { w.syscall(t, number, args) };
            let refused = match expected {
                Some(code) => result == Some(code),
                None => matches!(result, Some(code) if code < 0),
            };
            w.expect(refused, "a bad request was not refused, or was refused with another error than it must be");
            outcome(result)
        }
        Action::Expire(t) => {
            let t = t as usize;
            let slept = w.k.tasks[t].state == State::Sleep;
            w.expire(t);
            let task = &w.k.tasks[t];
            let woke = task.state == State::Ready && task.frame.eax as i32 == if slept { 0 } else { E_TIMEOUT };
            w.expect(woke, "a task whose deadline passed did not wake with the right result");
            Outcome::Event
        }
        Action::WakeRoot => {
            let woke = w.wake_root();
            w.expect(woke, "the root task did not start again within two seconds");
            Outcome::Event
        }
        Action::Interrupt => {
            w.interrupt(LINE);
            let masked = !line_open();
            w.expect(masked, "an interrupt line stayed open after it fired");
            Outcome::Event
        }
    }
}

// ---- Which steps are enabled -----------------------------------------------------------------

/// A scenario: where the search starts and what it tries.
struct Profile {
    name: &'static str,
    about: &'static str,
    /// Frames of RAM beyond the kernel.
    managed: usize,
    nx: bool,
    /// The families of steps that are tried.
    families: u32,
    /// The spawn shapes that are tried, one bit each.
    shapes: u8,
    /// A state that holds more than this is checked but not expanded.
    max_tasks: usize,
    max_eps: usize,
    max_caps: usize,
    depth: usize,
    setup: fn(&mut World),
    /// Which of the root task's steps are tried.
    root: fn(&Action) -> bool,
}

/// Append every step that ready task `t` is offered in this scenario.
fn offers(w: &World, profile: &Profile, t: usize, out: &mut Vec<Action>) {
    let on = |family: u32| profile.families & family != 0;
    let me = t as u8;
    let task = &w.k.tasks[t];
    let held = endpoint_slots(w, t, 0);
    let receivers = endpoint_slots(w, t, R_RECV);
    if on(IPC) {
        for slot in endpoint_slots(w, t, R_SEND) {
            out.push(Action::Call(me, slot, 0, false));
            if on(TIMEOUT) {
                out.push(Action::Call(me, slot, 0, true));
            }
            if on(PASS) {
                out.push(Action::Call(me, slot, 1, false));
                if held.first() != held.last() {
                    out.push(Action::Call(me, slot, 2, false));
                }
            }
        }
        for &slot in &receivers {
            out.push(Action::Recv(me, slot, 0));
            out.push(Action::Recv(me, slot, 1));
            if on(TIMEOUT) {
                out.push(Action::Recv(me, slot, 2));
            }
        }
        for client in 0..MAX_TASKS {
            if w.k.tasks[client].state == State::Reply(me) {
                out.push(Action::Reply(me, client as u8, 0));
                if on(PASS) && !held.is_empty() {
                    out.push(Action::Reply(me, client as u8, 1));
                }
            }
        }
    }
    if on(CAPS) {
        out.push(Action::EpCreate(me));
        for (slot, cap) in task.caps.iter().enumerate() {
            match *cap {
                Cap::Null => {}
                Cap::Endpoint { rights, badge, .. } => {
                    if rights as u32 & R_SEND != 0 {
                        out.push(Action::Derive(me, slot as u8, 0));
                        if rights as u32 & R_GRANT != 0 && badge == 0 {
                            out.push(Action::Derive(me, slot as u8, 1));
                        }
                    }
                }
                _ => out.push(Action::Derive(me, slot as u8, 0)),
            }
        }
    }
    for (slot, cap) in task.caps.iter().enumerate() {
        if *cap == Cap::Null {
            continue;
        }
        // The root task keeps the six capabilities it was booted with.
        if on(DROP) && !(t == 0 && slot < 6) {
            out.push(Action::Drop(me, slot as u8));
        }
        if on(REVOKE) && has_copies(w, link(t, slot)) {
            out.push(Action::Revoke(me, slot as u8));
        }
        if on(KILL) && matches!(cap, Cap::Task { .. }) {
            out.push(Action::Kill(me, slot as u8));
        }
    }
    if on(SPAWN) {
        for shape in 0..SHAPES.len() as u8 {
            let wanted = profile.shapes & (1 << shape) != 0;
            let possible = match shape {
                1 => !held.is_empty(),
                2 => held.first() != held.last(),
                _ => true,
            };
            if wanted && possible {
                out.push(Action::Spawn(me, shape));
            }
        }
    }
    if on(if t == 0 { ROOT_DEATH } else { DEATH }) {
        out.push(Action::Exit(me));
        out.push(Action::Fault(me));
    }
    if on(MEMORY) {
        for place in 0..PLACES.len() {
            out.push(Action::Map(me, place as u8));
        }
        for place in 0..UNPLACES.len() {
            out.push(Action::Unmap(me, place as u8));
        }
        for (slot, cap) in task.caps.iter().enumerate() {
            if matches!(cap, Cap::Memory { .. }) {
                out.push(Action::MapMem(me, slot as u8, false));
                out.push(Action::MapMem(me, slot as u8, true));
            }
        }
    }
    if on(DEVICE) && w.k.mem.lookup(task.pd, DEVICE_VA + PAGE).is_some_and(|entry| entry & RW != 0) {
        for &slot in &receivers {
            out.push(Action::RecvDevice(me, slot));
        }
    }
    if on(SLEEP) {
        out.push(Action::Sleep(me));
    }
    if on(IRQ) {
        for (slot, cap) in task.caps.iter().enumerate() {
            if matches!(cap, Cap::Irq { .. }) {
                out.push(Action::IrqAck(me, slot as u8));
                for &ep in &receivers {
                    out.push(Action::IrqBind(me, slot as u8, ep));
                }
            }
        }
    }
    if on(NEGATIVE) {
        for which in 0..REFUSALS {
            if refusal(w, t, which).is_some() {
                out.push(Action::Refused(me, which));
            }
        }
    }
}

/// Every step that can be taken in the present state.
fn enabled(w: &World, profile: &Profile, out: &mut Vec<Action>) {
    out.clear();
    let on = |family: u32| profile.families & family != 0;
    if on(IRQ) && line_open() {
        out.push(Action::Interrupt);
    }
    let now = arch::with(|machine| machine.micros());
    for (t, task) in w.k.tasks.iter().enumerate() {
        if !matches!(task.state, State::Recv(_) | State::Send(_) | State::Reply(_) | State::Sleep) {
            continue;
        }
        match deadline_kind(task.deadline, now) {
            FAR if on(TIMEOUT) || on(SLEEP) => out.push(Action::Expire(t as u8)),
            NEAR if t == 0 => out.push(Action::WakeRoot),
            _ => {}
        }
    }
    for t in 0..MAX_TASKS {
        if w.k.tasks[t].state != State::Ready {
            continue;
        }
        let first = out.len();
        offers(w, profile, t, out);
        if t == 0 {
            let mut kept = first;
            for index in first..out.len() {
                if (profile.root)(&out[index]) {
                    out[kept] = out[index];
                    kept += 1;
                }
            }
            out.truncate(kept);
        }
    }
}

// ---- What must hold for every step, whatever it was ------------------------------------------

const ERRORS: [&str; 11] = [
    "another error",
    "E_INVAL",
    "E_NOCAP",
    "E_PERM",
    "E_FAULT",
    "E_NOMEM",
    "E_NOSLOT",
    "E_DEAD",
    "E_TIMEOUT",
    "E_BUSY",
    "E_NOSYS",
];

fn error_index(code: i32) -> usize {
    let index = code.unsigned_abs() as usize;
    if index < ERRORS.len() {
        index
    } else {
        0
    }
}

/// The two properties that are checked by comparing whole states.
fn judge(action: Action, result: Outcome, state: u128, before: u128, restarted: bool, fresh: u128) -> Result<(), String> {
    if restarted {
        // Restart: the root task died, and with it everything it had started. What is left
        // must be the state of a machine that has only just booted.
        let clean = state == fresh;
        return if clean { Ok(()) } else { Err("the root task died and left something behind".to_string()) };
    }
    let Outcome::Returned(code) = result else { return Ok(()) };
    // Atomicity: a system call that fails changes nothing. The exception is a poll that
    // finds nothing to receive: on its way it may have failed a queued call whose capability
    // could not be passed on, and that caller now runs again with its error.
    let poll = matches!(action, Action::Recv(_, _, 0)) && code == E_TIMEOUT;
    if code < 0 && !poll && state != before {
        return Err(format!("a system call failed with {} and still changed the state", ERRORS[error_index(code)]));
    }
    Ok(())
}

const KINDS: usize = 7;
const KIND_NAMES: [&str; KINDS] = ["exit", "fault", "kill", "drop", "revoke", "timeout", "root death"];
const ROOT_DIED: usize = 6;

/// The kind of failure a step was, if it was one. Dropping or revoking a capability counts
/// only when it changed the state of another task. Call this before the state is restored.
fn failure_kind(w: &World, action: Action, result: Outcome) -> Option<usize> {
    match action {
        Action::Exit(0) | Action::Fault(0) => Some(ROOT_DIED),
        Action::Exit(_) => Some(0),
        Action::Fault(_) => Some(1),
        Action::Kill(..) if matches!(result, Outcome::Returned(0) | Outcome::Blocked) => Some(2),
        Action::Drop(t, _) if w.disturbed(t as usize) > 0 => Some(3),
        Action::Revoke(t, _) if w.disturbed(t as usize) > 0 => Some(4),
        Action::Expire(_) => Some(5),
        _ => None,
    }
}

/// What a search found, for the report.
struct Tally {
    transitions: u64,
    violations: u64,
    restarts: u64,
    beyond: u64,
    self_checks: u64,
    ok: u64,
    blocked: u64,
    events: u64,
    errors: [u64; ERRORS.len()],
    /// `pairs[a][b]`: failure steps of kind `b` on a path that had a failure of kind `a`.
    pairs: [[u64; KINDS]; KINDS],
    later: u64,
}

impl Tally {
    fn new() -> Tally {
        Tally {
            transitions: 0,
            violations: 0,
            restarts: 0,
            beyond: 0,
            self_checks: 0,
            ok: 0,
            blocked: 0,
            events: 0,
            errors: [0; ERRORS.len()],
            pairs: [[0; KINDS]; KINDS],
            later: 0,
        }
    }

    fn violation(&mut self, name: &str, message: &str, path: &str, action: Option<Action>) {
        self.violations += 1;
        if self.violations > SHOWN {
            return;
        }
        match action {
            Some(action) => println!("MODEL VIOLATION {name}: {message}; path: {path} then {action}"),
            None => println!("MODEL VIOLATION {name}: {message}; path: {path}"),
        }
    }

    fn count(&mut self, result: Outcome) {
        match result {
            Outcome::Returned(code) if code >= 0 => self.ok += 1,
            Outcome::Returned(code) => self.errors[error_index(code)] += 1,
            Outcome::Blocked => self.blocked += 1,
            Outcome::Event => self.events += 1,
        }
    }
}

/// The outcome of one scenario, for the total.
struct Report {
    states: u64,
    transitions: u64,
    violations: u64,
    pairs: [[bool; KINDS]; KINDS],
}

// ---- The search --------------------------------------------------------------------------------

/// A state that was reached, and the step that reached it first.
struct Node {
    parent: u32,
    action: Action,
    hash: u128,
    /// The kinds of failure on the path to this state, one bit each.
    failures: u8,
}

fn path_of(nodes: &[Node], mut index: u32) -> Vec<Action> {
    let mut path = Vec::new();
    while index != 0 {
        path.push(nodes[index as usize].action);
        index = nodes[index as usize].parent;
    }
    path.reverse();
    path
}

fn describe(path: &[Action]) -> String {
    if path.is_empty() {
        return "the start".to_string();
    }
    let steps: Vec<String> = path.iter().map(|action| action.to_string()).collect();
    steps.join(", ")
}

fn explore(profile: &Profile, depth: usize) -> Report {
    let started = Instant::now();
    let name = profile.name;
    println!("MODEL scenario {name}: {} (depth {depth})", profile.about);
    let mut tally = Tally::new();
    let mut summary = Vec::new();
    // What the death of the root task must leave behind: the state of a machine that has
    // just booted and whose root task then died once. There is only one machine at a time,
    // so this state is taken first.
    crate::trail(format!("{name}: booting the reference system"));
    let fresh = {
        let mut reference = World::boot(profile.managed, profile.nx);
        reference.fault(0, 0x10);
        hash_of(&reference, &mut summary)
    };
    crate::trail(format!("{name}: setting up"));
    let mut w = World::boot(profile.managed, profile.nx);
    w.exploring = true;
    (profile.setup)(&mut w);
    if let Err(problem) = w.verify() {
        tally.violation(name, &problem, "the setup", None);
    }
    w.save_base();
    let start = hash_of(&w, &mut summary);
    let mut nodes = vec![Node { parent: 0, action: Action::WakeRoot, hash: start, failures: 0 }];
    let mut seen = HashSet::new();
    seen.insert(start);
    let mut frontier: Vec<u32> = vec![0];
    let mut actions = Vec::new();
    let mut complete = false;
    let mut reached = 0;
    for level in 1..=depth {
        let mut next = Vec::new();
        let (states_before, transitions_before) = (nodes.len(), tally.transitions);
        for &index in &frontier {
            let text = describe(&path_of(&nodes, index));
            crate::trail(format!("{name}: replaying {text}"));
            w.restore_base();
            for action in path_of(&nodes, index) {
                step(&mut w, action);
            }
            w.problem = None;
            w.save_parent();
            let (before, earlier) = (nodes[index as usize].hash, nodes[index as usize].failures);
            if hash_of(&w, &mut summary) != before {
                tally.violation(name, "taking the same steps again led to another state", &text, None);
                continue;
            }
            let root_gen = w.k.tasks[0].gen;
            enabled(&w, profile, &mut actions);
            for position in 0..actions.len() {
                let action = actions[position];
                tally.transitions += 1;
                let ram = (tally.transitions % SELF_CHECK == 0).then(|| arch::with(|machine| machine.copy_ram()));
                crate::trail(format!("{name}: {text} then {action}"));
                let result = step(&mut w, action);
                let kind = failure_kind(&w, action, result);
                let verdict = w.verify();
                let state = hash_of(&w, &mut summary);
                let restarted = w.k.tasks[0].gen != root_gen;
                match verdict.and_then(|()| judge(action, result, state, before, restarted, fresh)) {
                    Err(message) => tally.violation(name, &message, &text, Some(action)),
                    Ok(()) => {
                        tally.count(result);
                        tally.restarts += restarted as u64;
                        // After a restart nothing of the earlier failures is left to meet.
                        let mut failures = if restarted { 0 } else { earlier };
                        if let Some(kind) = kind {
                            tally.later += (earlier != 0) as u64;
                            for first in 0..KINDS {
                                if earlier & (1 << first) != 0 {
                                    tally.pairs[first][kind] += 1;
                                }
                            }
                            failures |= 1 << kind;
                        }
                        if seen.insert(state) {
                            nodes.push(Node { parent: index, action, hash: state, failures });
                            let beyond = w.tasks() > profile.max_tasks
                                || w.endpoints() > profile.max_eps
                                || w.caps() > profile.max_caps;
                            if beyond {
                                tally.beyond += 1;
                            } else {
                                next.push((nodes.len() - 1) as u32);
                            }
                        }
                    }
                }
                w.restore_parent();
                if let Some(ram) = ram {
                    // A check of the model itself: going back really goes back.
                    tally.self_checks += 1;
                    let same = arch::with(|machine| machine.ram_equals(&ram)) && hash_of(&w, &mut summary) == before;
                    if !same {
                        tally.violation(name, "going back to a snapshot did not bring its state back", &text, Some(action));
                    }
                }
            }
        }
        let (states, transitions) = (nodes.len() - states_before, tally.transitions - transitions_before);
        println!("  depth {level}: {states} new states, {transitions} transitions");
        reached = level;
        frontier = next;
        if frontier.is_empty() {
            complete = true;
            break;
        }
    }
    crate::trail(format!("{name}: closing"));
    w.restore_base();
    if !arch::with(|machine| machine.matches_base()) {
        tally.violation(name, "the machine did not return to its first snapshot", "the end", None);
    }
    let seconds = started.elapsed().as_secs_f64();
    println!(
        "MODEL explore {name}: depth {reached}, {} states, {} transitions, {} violations, complete {}, \
         {} deliveries checked, {} replies checked, {} restarts, {} beyond budget, {seconds:.1} s",
        nodes.len(),
        tally.transitions,
        tally.violations,
        if complete { "yes" } else { "no" },
        w.deliveries,
        w.replies,
        tally.restarts,
        tally.beyond,
    );
    let mut results = format!("ok {}, did not return {}, events {}", tally.ok, tally.blocked, tally.events);
    for (index, error) in ERRORS.iter().enumerate().skip(1).chain(ERRORS.iter().enumerate().take(1)) {
        results.push_str(&format!(", {error} {}", tally.errors[index]));
    }
    println!("MODEL results {name}: {results}");
    let mut report = Report {
        states: nodes.len() as u64,
        transitions: tally.transitions,
        violations: tally.violations,
        pairs: [[false; KINDS]; KINDS],
    };
    let mut met = 0;
    for first in 0..KINDS {
        for second in 0..KINDS {
            report.pairs[first][second] = tally.pairs[first][second] != 0;
            met += report.pairs[first][second] as u32;
        }
    }
    println!(
        "MODEL pairs {name}: {met} of {} ordered pairs of failure kinds, {} failure steps after an earlier failure",
        KINDS * KINDS,
        tally.later,
    );
    report
}

// ---- The scenarios -----------------------------------------------------------------------------

fn nothing(_: &mut World) {}

fn anything(_: &Action) -> bool {
    true
}

/// A server and a client of one endpoint, and the root task as their supervisor.
///
/// Root: slot 6 endpoint E, slot 7 a badged send-and-grant copy of it, slot 8 endpoint F
/// (where deaths are reported), slots 9 and 10 the handles of server and client. The server
/// starts with E, the client with the badged copy.
fn ipc_setup(w: &mut World) {
    let e = w.ep_create(0);
    let badged = w.derive(0, 6, R_SEND | R_GRANT, 0x10, 0);
    let f = w.ep_create(0);
    let server = w.spawn(0, &Spawn { fault: 8, ..Spawn::PLAIN }.with_caps(&[6]));
    let client = w.spawn(0, &Spawn { fault: 8, ..Spawn::PLAIN }.with_caps(&[7]));
    let built = (e, badged, f, server, client) == (Some(6), Some(7), Some(8), Some(9), Some(10));
    w.expect(built, "the scenario could not be set up");
}

fn ipc_root(action: &Action) -> bool {
    matches!(action, Action::Recv(_, 8, _) | Action::Kill(..) | Action::Revoke(_, 6 | 7) | Action::Drop(_, 7))
}

/// A driver that holds the interrupt capability and an endpoint, and the root task.
///
/// Root: slot 6 endpoint E, slot 7 endpoint F (deaths), slot 8 the driver's handle. The
/// driver starts with the interrupt lines in slot 0 and E in slot 1.
fn irq_setup(w: &mut World) {
    let e = w.ep_create(0);
    let f = w.ep_create(0);
    let driver = w.spawn(0, &Spawn { fault: 7, ..Spawn::PLAIN }.with_caps(&[3, 6]));
    w.expect((e, f, driver) == (Some(6), Some(7), Some(8)), "the scenario could not be set up");
}

fn irq_root(action: &Action) -> bool {
    matches!(
        action,
        Action::Recv(_, 6 | 7, _) | Action::Kill(..) | Action::Revoke(_, 3 | 6) | Action::IrqBind(..) | Action::IrqAck(..)
    )
}

/// One task with the screen's memory and room for two more frames. Root: slot 6 its handle.
fn memory_setup(w: &mut World) {
    let child = w.spawn(0, &Spawn { quota: CHILD_FRAMES + 2, eps: 1, ..Spawn::PLAIN }.with_caps(&[4]));
    w.expect(child == Some(6), "the scenario could not be set up");
}

fn memory_root(action: &Action) -> bool {
    matches!(action, Action::Kill(..) | Action::Revoke(_, 4))
}

static PROFILES: [Profile; 5] = [
    Profile {
        name: "boot",
        about: "the root task alone, offered nearly every kind of step",
        managed: SMALL,
        nx: true,
        families: IPC | TIMEOUT | PASS | CAPS | DROP | REVOKE | SPAWN | KILL | DEATH | ROOT_DEATH | MEMORY | IRQ | SLEEP | DEVICE,
        shapes: 0b00111,
        max_tasks: 3,
        max_eps: 2,
        max_caps: 12,
        depth: 4,
        setup: nothing,
        root: anything,
    },
    Profile {
        name: "ipc",
        about: "a client, a server and their supervisor: calls, replies, capabilities sent along, deaths in between",
        managed: SMALL,
        nx: true,
        families: IPC | TIMEOUT | PASS | DROP | REVOKE | KILL | DEATH | NEGATIVE,
        shapes: 0,
        max_tasks: 3,
        max_eps: 2,
        max_caps: 16,
        depth: 8,
        setup: ipc_setup,
        root: ipc_root,
    },
    Profile {
        name: "irq",
        about: "a driver, its interrupt line and its supervisor",
        managed: SMALL,
        nx: true,
        families: IRQ | IPC | TIMEOUT | DROP | REVOKE | KILL | DEATH,
        shapes: 0,
        max_tasks: 2,
        max_eps: 2,
        max_caps: 12,
        depth: 12,
        setup: irq_setup,
        root: irq_root,
    },
    Profile {
        name: "memory",
        about: "a task that maps, unmaps and runs out of memory, with device memory that can be revoked",
        managed: SMALL,
        nx: true,
        families: MEMORY | CAPS | DROP | REVOKE | KILL | DEATH | DEVICE | NEGATIVE,
        shapes: 0,
        max_tasks: 2,
        max_eps: 1,
        max_caps: 12,
        depth: 6,
        setup: memory_setup,
        root: memory_root,
    },
    Profile {
        name: "spawn",
        about: "trees of tasks on a machine too small for them, and the root task dying",
        managed: TIGHT,
        nx: true,
        families: SPAWN | KILL | DEATH | ROOT_DEATH | DROP,
        shapes: 0b11001,
        max_tasks: 4,
        max_eps: 2,
        max_caps: 10,
        depth: 8,
        setup: nothing,
        root: anything,
    },
];

/// Explore one scenario, to its own depth unless another is given.
pub fn run(name: &str, depth: Option<usize>) -> bool {
    let Some(profile) = PROFILES.iter().find(|profile| profile.name == name) else {
        println!("no scenario is called {name}; these exist:");
        list();
        return false;
    };
    explore(profile, depth.unwrap_or(profile.depth)).violations == 0
}

/// Explore every scenario to its own depth.
pub fn run_all() -> bool {
    let (mut states, mut transitions, mut violations) = (0, 0, 0);
    let mut pairs = [[false; KINDS]; KINDS];
    for profile in &PROFILES {
        let report = explore(profile, profile.depth);
        states += report.states;
        transitions += report.transitions;
        violations += report.violations;
        for first in 0..KINDS {
            for second in 0..KINDS {
                pairs[first][second] |= report.pairs[first][second];
            }
        }
    }
    let met = pairs.iter().flatten().filter(|&&met| met).count();
    println!(
        "MODEL explore total: {} scenarios, {states} states, {transitions} transitions, {violations} violations, \
         {met} of {} ordered pairs of failure kinds",
        PROFILES.len(),
        KINDS * KINDS,
    );
    let mut missing = Vec::new();
    for first in 0..KINDS {
        for second in 0..KINDS {
            if !pairs[first][second] {
                missing.push(format!("{} after {}", KIND_NAMES[second], KIND_NAMES[first]));
            }
        }
    }
    if !missing.is_empty() {
        println!("MODEL pairs not met: {}", missing.join("; "));
    }
    violations == 0
}

pub fn list() {
    for profile in &PROFILES {
        println!("{:<7} depth {}  {}", profile.name, profile.depth, profile.about);
    }
}
