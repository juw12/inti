//! Directed tests of the kernel on the host: its limits, its clock, and its own checker.
//!
//! The explorer (`explore.rs`) visits every state within a few steps of some starting
//! points. These tests go where it does not get: to the far end of every table (all 32
//! tasks, all 64 endpoints, the last free frame, the last capability slot), through whole
//! seconds of time, and into states that only a broken kernel could be in - to see whether
//! the kernel's invariant checker, on which the explorer leans, would notice them.

use crate::arch::{self, KERNEL_MAP};
use crate::mem::{frame_of, FOREIGN, NX, RW, US};
use crate::task::{link, Cap, State, NONE, ORIGINAL};
use crate::world::{Spawn, World, BUFFER, CHILD_FRAMES, LARGE, ROOT_FRAMES, SMALL};
use abi::{
    E_FAULT, E_INVAL, E_NOMEM, E_NOSLOT, E_TIMEOUT, FOREVER, MAP_WRITE, MAX_ENDPOINTS, MAX_TASKS, NO_CAP, PAGE, R_GRANT,
    R_SEND, USER_BASE,
};

/// Where the tasks of these tests map memory of their own, and the memory of a device.
const HEAP: u32 = 0x5000_0000;
const DEVICE_VA: u32 = 0x6000_0000;

/// Counts what held and what did not, and prints a line for each.
struct Tally {
    passed: u32,
    failed: u32,
}

impl Tally {
    fn section(&mut self, title: &str) {
        println!("{title}");
        crate::trail(format!("tables: {title}"));
    }

    fn check(&mut self, name: &str, holds: bool) {
        if holds {
            self.passed += 1;
            println!("  ok    {name}");
        } else {
            self.failed += 1;
            println!("  FAIL  {name}");
        }
    }

    /// Everything that must hold after any step of any task holds in this state.
    fn sound(&mut self, w: &mut World, name: &str) {
        match w.verify() {
            Ok(()) => self.check(name, true),
            Err(problem) => self.check(&format!("{name}: {problem}"), false),
        }
    }
}

fn woke_with(w: &World, t: usize, result: i32) -> bool {
    w.k.tasks[t].state == State::Ready && w.k.tasks[t].frame.eax as i32 == result
}

// ---- Boot ----------------------------------------------------------------------------------

fn boot_facts(tally: &mut Tally) {
    tally.section("Boot");
    let mut w = World::boot(SMALL, true);
    let (total, free, root) = (w.k.mem.total, w.k.mem.free, w.k.tasks[0].charged);
    println!("  note  {total} frames are managed, the root task holds {root} of them, {free} are free");
    tally.check("every frame beyond the kernel is managed", total as usize == SMALL);
    tally.check("the root task holds the frames this model expects", root == ROOT_FRAMES && free == total - root);
    tally.check("the root task owns the whole memory quota", w.k.tasks[0].quota == total);
    tally.sound(&mut w, "the state after boot is sound");
    let Ok(child) = w.spawn_task(0, &Spawn::PLAIN) else {
        tally.check("the root task spawns a task", false);
        return;
    };
    let charged = w.k.tasks[child].charged;
    println!("  note  a task with one page of program and one of stack holds {charged} frames");
    tally.check("a small task holds the frames this model expects", charged == CHILD_FRAMES);
    tally.check("its frames come out of the free memory", w.k.mem.free == free - charged);
    let (mut ours, mut theirs) = ([0; 128], [1; 128]);
    let copied = w.peek(0, USER_BASE, &mut ours) && w.peek(child, USER_BASE, &mut theirs) && ours == theirs;
    tally.check("its program is a copy of its parent's, its stack is all zero", copied && w.page_is_zero(child, BUFFER));
    tally.sound(&mut w, "the state after a spawn is sound");
}

// ---- The tables ----------------------------------------------------------------------------

fn endpoint_table(tally: &mut Tally) {
    tally.section("The endpoint table");
    let mut w = World::boot(LARGE, true);
    tally.check("the root task may create every endpoint there is", w.k.tasks[0].ep_quota as usize == MAX_ENDPOINTS);
    let own = [w.ep_create(0), w.ep_create(0)];
    let half = Spawn { eps: 31, quota: 40, ..Spawn::PLAIN };
    let (Ok(a), Ok(b)) = (w.spawn_task(0, &half), w.spawn_task(0, &half)) else {
        tally.check("two tasks start that may create 31 endpoints each", false);
        return;
    };
    tally.check("the root task has given away all it had", own == [Some(6), Some(7)] && w.k.tasks[0].ep_quota == 0);
    let mut created = 0;
    for t in [a, b] {
        for _ in 0..31 {
            created += w.ep_create(t).is_some_and(|slot| slot >= 0) as usize;
        }
    }
    tally.check("all 64 endpoints exist", created == 62 && w.endpoints() == MAX_ENDPOINTS);
    let refused = [a, b, 0].map(|t| w.ep_create(t));
    tally.check("a 65th is refused to each of the three tasks with E_NOSLOT", refused == [Some(E_NOSLOT); 3]);
    tally.sound(&mut w, "the state with a full endpoint table is sound");

    // The quotas always add up to the size of the table, so no task can reach the branch
    // in which the kernel finds the table itself full. A forged quota gets there.
    w.k.tasks[0].ep_quota = 1;
    let forged = w.ep_create(0);
    let (anomalies, rule) = (w.k.stats.anomalies, w.k.check());
    println!("  note  with a forged quota the kernel counts {anomalies} anomaly, and its checker reports rule {rule}");
    let held = forged == Some(E_NOSLOT) && anomalies == 1 && rule != 0 && w.endpoints() == MAX_ENDPOINTS;
    tally.check("with a forged quota the full table itself refuses, and the kernel counts an anomaly", held);
    w.k.tasks[0].ep_quota = 0;
    w.k.stats.anomalies = 0;
    tally.sound(&mut w, "with the forgery undone the state is sound again");

    let killed = w.kill(0, 8);
    let returned = killed == Some(0) && w.k.tasks[0].ep_quota == 31 && w.endpoints() == 33;
    tally.check("killing one of the two returns its 31 endpoints to the root task", returned);
    tally.check("the root task can create an endpoint again", w.ep_create(0).is_some_and(|slot| slot >= 0));
    tally.sound(&mut w, "the state afterwards is sound");
}

fn task_table(tally: &mut Tally) {
    tally.section("The task table");
    let mut w = World::boot(LARGE, true);
    let free = w.k.mem.free;
    tally.check("the root task may create every other task there is", w.k.tasks[0].task_quota as usize == MAX_TASKS - 1);
    let Ok(a) = w.spawn_task(0, &Spawn { quota: 220, tasks: 15, ..Spawn::PLAIN }) else {
        tally.check("a task starts that may create 15 tasks", false);
        return;
    };
    let mut spawned = 0;
    for t in [a, 0] {
        for _ in 0..15 {
            spawned += w.spawn_task(t, &Spawn::PLAIN).is_ok() as usize;
        }
    }
    tally.check("all 32 tasks exist", spawned == 30 && w.tasks() == MAX_TASKS);
    let refused = [w.spawn(0, &Spawn::PLAIN), w.spawn(a, &Spawn::PLAIN)];
    tally.check("a 33rd is refused to the root task and to its child with E_NOSLOT", refused == [Some(E_NOSLOT); 2]);
    tally.sound(&mut w, "the state with a full task table is sound");

    // As for endpoints: only a forged quota reaches the kernel's own "table full" branch.
    w.k.tasks[0].task_quota = 1;
    let forged = w.spawn(0, &Spawn::PLAIN);
    let (anomalies, rule) = (w.k.stats.anomalies, w.k.check());
    println!("  note  with a forged quota the kernel counts {anomalies} anomaly, and its checker reports rule {rule}");
    let held = forged == Some(E_NOSLOT) && anomalies == 1 && rule != 0 && w.tasks() == MAX_TASKS;
    tally.check("with a forged quota the full table itself refuses, and the kernel counts an anomaly", held);
    w.k.tasks[0].task_quota = 0;
    w.k.stats.anomalies = 0;
    tally.sound(&mut w, "with the forgery undone the state is sound again");

    let killed = w.kill(0, 6);
    let returned = killed == Some(0) && w.tasks() == 16 && w.k.tasks[0].task_quota == 16;
    tally.check("killing the child kills its 15 children and returns 16 places to the root task", returned);
    tally.check("the root task can spawn again", w.spawn_task(0, &Spawn::PLAIN).is_ok());
    tally.sound(&mut w, "the state afterwards is sound");

    w.fault(0, 0x10);
    let woke = w.wake_root();
    tally.check("when the root task dies, all 16 others die with it and a new root task starts", woke && w.tasks() == 1);
    tally.check("the new root task finds as much free memory as the first", w.k.mem.free == free);
    tally.sound(&mut w, "the state after the restart is sound");
}

fn memory_and_slots(tally: &mut Tally) {
    tally.section("The last frame and the last slot");
    let mut w = World::boot(64, true);
    let (free, quota) = (w.k.mem.free, w.k.tasks[0].quota);
    let mut all_back = true;
    for frames in 0..CHILD_FRAMES {
        let result = w.spawn(0, &Spawn { quota: frames, ..Spawn::PLAIN });
        let untouched = w.k.mem.free == free && w.k.tasks[0].quota == quota && w.tasks() == 1;
        all_back &= result == Some(E_NOMEM) && untouched && w.verify().is_ok();
    }
    tally.check("a spawn that runs out of memory after 0 to 6 frames leaves nothing behind", all_back);
    let exact = w.spawn_task(0, &Spawn { quota: CHILD_FRAMES, ..Spawn::PLAIN });
    tally.check("with exactly 7 frames it succeeds", exact.is_ok_and(|child| w.k.tasks[child].charged == CHILD_FRAMES));
    let killed = w.kill(0, 6) == Some(0) && w.drop_cap(0, 6) == Some(0);
    tally.check("killing the task returns its frames", killed && w.k.mem.free == free);

    let mut pages = 0;
    let last = loop {
        let result = w.map(0, HEAP + pages * PAGE, 1, true);
        if result != Some(0) || pages == 4096 {
            break result;
        }
        pages += 1;
    };
    println!("  note  {pages} pages and their page table took the {free} free frames");
    let exhausted = last == Some(E_NOMEM) && w.k.mem.free == 0 && pages + 1 == free;
    tally.check("mapping page after page ends with E_NOMEM when the last frame is taken", exhausted);
    tally.sound(&mut w, "the state without a free frame is sound");
    tally.check("an endpoint can still be created, it needs no memory", w.ep_create(0) == Some(6));

    // Two frames are free again. Two pages in a part of the address space that has no page
    // table yet need three, so the call must fail - and must leave nothing behind.
    let unmapped = w.unmap(0, HEAP + (pages - 2) * PAGE, 2);
    let before = (w.k.mem.free, w.k.tasks[0].charged);
    let short = w.map(0, HEAP + 0x40_0000, 2, true);
    let after = (w.k.mem.free, w.k.tasks[0].charged);
    println!("  note  (free frames, frames charged) before the failing MAP {before:?}, after it {after:?}");
    let atomic = unmapped == Some(0) && before.0 == 2 && short == Some(E_NOMEM) && after == before;
    tally.check("a MAP that fails at its second page gives back all it took for the first", atomic);
    tally.sound(&mut w, "the state after the failed MAP is sound");
    tally.check("a spawn is refused with E_NOMEM", w.spawn(0, &Spawn::PLAIN) == Some(E_NOMEM));

    let mut copies = 0;
    let full = loop {
        let result = w.derive(0, 6, R_SEND, 0, 0);
        if !result.is_some_and(|slot| slot >= 0) || copies == 64 {
            break result;
        }
        copies += 1;
    };
    let no_slot = full == Some(E_NOSLOT) && copies == 25 && w.ep_create(0) == Some(E_NOSLOT);
    tally.check("25 copies of a capability fill the 32 slots, then copying and creating give E_NOSLOT", no_slot);
    tally.sound(&mut w, "the state without a free slot is sound");
    let caps = w.caps();
    tally.check("one revocation removes all 25 copies", w.revoke(0, 6) == Some(0) && w.caps() == caps - copies);

    w.fault(0, 0x10);
    let woke = w.wake_root();
    tally.check("the death of the root task returns every frame and endpoint", woke && w.k.mem.free == free && w.endpoints() == 0);
    tally.sound(&mut w, "the state after the restart is sound");
}

// ---- Time ----------------------------------------------------------------------------------

fn clock_and_timeouts(tally: &mut Tally) {
    tally.section("The clock, the restart delay and timeouts");
    let mut w = World::boot(SMALL, true);
    w.pass(123_456);
    let told = w.time(0);
    let machine = arch::with(|machine| machine.micros());
    println!("  note  after about 123,456 microseconds a task is told {told}, the machine says {machine}");
    tally.check("the time a task is told is the machine's time", told == machine && told >= 123_456);

    let mut w = World::boot(SMALL, true);
    w.fault(0, 0x10);
    tally.check("a root task that dies right after boot is not restarted at once", w.k.tasks[0].state == State::Sleep);
    tally.sound(&mut w, "the state while the root task waits to restart is sound");
    w.pass(990_000);
    let early = w.k.tasks[0].state == State::Sleep;
    w.pass(30_000);
    tally.check("it runs again one second after boot, not before", early && w.k.tasks[0].state == State::Ready);
    w.pass(1_200_000);
    w.fault(0, 0x10);
    tally.check("a root task that dies after running for a second restarts at once", w.k.tasks[0].state == State::Ready);
    tally.sound(&mut w, "the state after both restarts is sound");

    let asleep = w.sleep(0, 30);
    w.pass(20_000);
    let still = w.k.tasks[0].state == State::Sleep;
    w.pass(20_000);
    let slept = asleep.is_none() && still && woke_with(&w, 0, 0);
    tally.check("a task that sleeps 30 ms is asleep after 20 ms and awake after 40 ms", slept);

    let created = w.ep_create(0);
    let waiting = w.call(0, 6, NO_CAP, 50);
    w.pass(60_000);
    let timed_out = created == Some(6) && waiting.is_none() && woke_with(&w, 0, E_TIMEOUT);
    tally.check("a call that nobody receives within its 50 ms ends with E_TIMEOUT", timed_out);
    tally.sound(&mut w, "the state after the timeout is sound");

    let copy = w.derive(0, 6, R_SEND, 0x10, 0);
    let Ok(client) = w.spawn_task(0, &Spawn::PLAIN.with_caps(&[7])) else {
        tally.check("a client starts", false);
        return;
    };
    let called = w.call(client, 0, NO_CAP, 50);
    let received = w.recv(0, 6, 0);
    let delivered = w.verify();
    let token = w.token(0);
    w.pass(60_000);
    let arrived = copy == Some(7) && called.is_none() && received == Some(0) && delivered.is_ok();
    tally.check("a call that is received but not answered within its 50 ms ends with E_TIMEOUT", arrived && woke_with(&w, client, E_TIMEOUT));
    tally.check("the answer that comes too late is refused with E_INVAL", w.reply(0, token, NO_CAP) == Some(E_INVAL));
    tally.sound(&mut w, "the state after the late answer is sound");
}

// ---- Device memory taken away from a waiting task ----------------------------------------------

/// The root task creates an endpoint and starts a driver that holds the screen memory and
/// that endpoint. The driver maps the screen where it will put its messages, and creates
/// an endpoint of its own (its slot 2).
fn driver(w: &mut World) -> Option<usize> {
    let shared = w.ep_create(0);
    let driver = w.spawn_task(0, &Spawn { eps: 1, ..Spawn::PLAIN }.with_caps(&[4, 6])).ok()?;
    let mapped = w.map_mem(driver, 0, DEVICE_VA, 0, 1 | MAP_WRITE);
    let own = w.ep_create(driver);
    (shared == Some(6) && mapped == Some(0) && own == Some(2)).then_some(driver)
}

fn revoked_buffer(tally: &mut Tally) {
    tally.section("Messages and device memory");
    let mut w = World::boot(SMALL, true);
    let Some(d) = driver(&mut w) else {
        tally.check("a driver starts and maps the screen", false);
        return;
    };
    // The kernel puts a message only into memory that belongs to the receiving task. So the
    // memory under a waiting task's message is never somebody else's to take away.
    tally.check(
        "receiving into device memory is refused with E_FAULT",
        w.recv_at(d, 2, DEVICE_VA, FOREVER) == Some(E_FAULT),
    );
    tally.check(
        "a call whose answer would land in device memory is refused with E_FAULT",
        w.call_at(d, 1, DEVICE_VA, NO_CAP, FOREVER) == Some(E_FAULT),
    );
    tally.sound(&mut w, "the state after both refusals is sound");

    // What can be taken away is the device memory itself, while its holder waits.
    for (waiting, for_answer) in [("to receive", false), ("for an answer", true)] {
        let mut w = World::boot(SMALL, true);
        let Some(d) = driver(&mut w) else {
            tally.check("a driver starts and maps the screen", false);
            return;
        };
        let mut token = 0;
        let waits = if for_answer {
            let called = w.call(d, 1, NO_CAP, FOREVER).is_none();
            let received = w.recv(0, 6, 0) == Some(0);
            token = w.token(0);
            called && received && matches!(w.k.tasks[d].state, State::Reply(_))
        } else {
            w.recv(d, 1, FOREVER).is_none() && matches!(w.k.tasks[d].state, State::Recv(_))
        };
        let before = w.k.tasks[d].state;
        let revoked = w.revoke(0, 4) == Some(0);
        let unmapped = w.k.mem.lookup(w.k.tasks[d].pd, DEVICE_VA).is_none() && w.k.tasks[d].foreign == 0;
        let waits_on = w.k.tasks[d].state == before;
        tally.check(
            &format!("revoking the screen unmaps it from a driver that waits {waiting}; the driver waits on"),
            waits && revoked && unmapped && waits_on,
        );
        tally.sound(&mut w, &format!("the state after revoking under a driver that waits {waiting} is sound"));
        let finished = if for_answer {
            w.reply(0, token, NO_CAP) == Some(0)
        } else {
            w.call(0, 6, NO_CAP, FOREVER).is_none()
        };
        tally.check(
            &format!("the driver that waited {waiting} then gets its message"),
            finished && woke_with(&w, d, 0),
        );
        tally.sound(&mut w, &format!("the message to the driver that waited {waiting} arrived intact"));
    }
}

// ---- Would the checker notice? -----------------------------------------------------------------

// The stage for the corruptions below. Tasks and endpoints get the lowest free numbers.
const CLIENT: usize = 1;
const SERVER: usize = 2;
const NOBODY: usize = 5;
const SHARED: usize = 0;
const OWN: usize = 1;

/// A state with a bit of everything: the root task holds an endpoint (slot 6), a send-only
/// copy of it with a badge (slot 7), a page of its own, the screen, and interrupt 1. A
/// client is queued to send on the endpoint; a server waits on an endpoint of its own.
fn stage() -> Option<World> {
    let mut w = World::boot(SMALL, true);
    let mut built = w.ep_create(0) == Some(6);
    built &= w.derive(0, 6, R_SEND, 0x10, 0) == Some(7);
    built &= w.spawn_task(0, &Spawn::PLAIN.with_caps(&[7])) == Ok(CLIENT);
    built &= w.spawn_task(0, &Spawn { eps: 1, ..Spawn::PLAIN }) == Ok(SERVER);
    built &= w.map(0, HEAP, 1, true) == Some(0);
    built &= w.map_mem(0, 4, DEVICE_VA, 0, 1 | MAP_WRITE) == Some(0);
    built &= w.irq_bind(0, 3, 1, 6) == Some(0);
    built &= w.call(CLIENT, 0, NO_CAP, FOREVER).is_none();
    built &= w.ep_create(SERVER) == Some(0);
    built &= w.recv(SERVER, 0, FOREVER).is_none();
    built &= w.k.tasks[CLIENT].state == State::Send(SHARED as u8) && w.k.tasks[SERVER].state == State::Recv(OWN as u8);
    (built && w.verify().is_ok()).then_some(w)
}

/// Where in memory the page-table entry for address `va` of task `t` is.
fn pte(w: &World, t: usize, va: u32) -> u32 {
    let table = w.k.mem.user_table(w.k.tasks[t].pd, ((va - USER_BASE) >> 21) as usize);
    table.expect("the address has no page table") + ((va >> 12) & 511) * 8
}

/// Where the entry that points to that page table is.
fn pde(w: &World, t: usize, va: u32) -> u32 {
    frame_of(w.k.mem.read_entry(w.k.tasks[t].pd, (va >> 30) as usize)) + ((va >> 21) & 511) * 8
}

fn entry(w: &World, at: u32) -> u64 {
    w.k.mem.read_entry(at & !0xFFF, (at as usize & 0xFFF) / 8)
}

fn set_entry(at: u32, value: u64) {
    arch::with(|machine| machine.load(at, &value.to_le_bytes()));
}

fn forge(w: &mut World, cap: Cap) {
    w.k.tasks[CLIENT].caps[5] = cap;
    w.k.tasks[CLIENT].from[5] = ORIGINAL;
}

type Seed = (&'static str, fn(&mut World));

/// States no sequence of system calls should lead to. The checker must report each.
const SEEDS: &[Seed] = &[
    ("an endpoint counts one capability too many", |w| w.k.eps[SHARED].refs += 1),
    ("an endpoint counts one receiver too many", |w| w.k.eps[SHARED].recv_refs += 1),
    ("a copy has a right that its source lacks", |w| {
        w.k.tasks[CLIENT].caps[0] = Cap::Endpoint { ep: SHARED as u8, rights: (R_SEND | R_GRANT) as u8, badge: 0x10 }
    }),
    ("a copy has another badge than its source", |w| {
        w.k.tasks[CLIENT].caps[0] = Cap::Endpoint { ep: SHARED as u8, rights: R_SEND as u8, badge: 0x11 }
    }),
    ("a copy names an empty slot as its source", |w| w.k.tasks[CLIENT].from[0] = link(NOBODY, 3)),
    ("a copy names itself as its source", |w| w.k.tasks[CLIENT].from[0] = link(CLIENT, 0)),
    ("two copies name each other as their source", |w| w.k.tasks[0].from[7] = link(CLIENT, 0)),
    ("a capability names an endpoint beyond the table", |w| {
        w.k.tasks[0].caps[6] = Cap::Endpoint { ep: 200, rights: 7, badge: 0 }
    }),
    ("the count of free frames is one too high", |w| w.k.mem.free += 1),
    ("a frame that a task uses is marked free", |w| {
        let frame = frame_of(entry(w, pte(w, 0, HEAP)));
        w.k.mem.free_frame(frame);
    }),
    ("a frame is taken that no task owns", |w| {
        w.k.mem.alloc();
    }),
    ("one frame is mapped by two tasks", |w| {
        let (theirs, ours) = (entry(w, pte(w, 0, BUFFER)), pte(w, CLIENT, BUFFER - PAGE));
        set_entry(ours, theirs);
    }),
    ("a page of program can be written", |w| {
        let at = pte(w, 0, USER_BASE);
        set_entry(at, entry(w, at) | RW);
    }),
    ("a page of stack can be executed", |w| {
        let at = pte(w, 0, BUFFER);
        set_entry(at, entry(w, at) & !NX);
    }),
    ("a page-table entry has a bit the kernel never sets", |w| {
        let at = pte(w, 0, BUFFER);
        set_entry(at, entry(w, at) | 0x800);
    }),
    ("a page table is closed to its own task", |w| {
        let at = pde(w, 0, BUFFER);
        set_entry(at, entry(w, at) & !US);
    }),
    ("a page table is kept although it maps nothing", |w| {
        let at = pte(w, 0, HEAP);
        let frame = frame_of(entry(w, at));
        set_entry(at, 0);
        w.k.mem.free_frame(frame);
        w.k.tasks[0].charged -= 1;
    }),
    ("the kernel half of a task's address space differs", |w| {
        let at = w.k.tasks[CLIENT].pd;
        set_entry(at, entry(w, at) ^ 0x200);
    }),
    ("an entry of the kernel's own page tables changed", |_| {
        let at = KERNEL_MAP + 0x3008;
        arch::with(|machine| {
            let flipped = machine.byte(at) ^ RW as u8;
            machine.load(at, &[flipped]);
        });
    }),
    ("device memory is mapped as if it were the task's own", |w| {
        let at = pte(w, 0, DEVICE_VA);
        set_entry(at, entry(w, at) & !FOREIGN);
    }),
    ("device memory stays mapped without its capability", |w| w.k.tasks[0].caps[4] = Cap::Null),
    ("a task may create one task too many", |w| w.k.tasks[0].task_quota += 1),
    ("a task may create one endpoint too many", |w| w.k.tasks[0].ep_quota += 1),
    ("a task's memory quota grew by itself", |w| w.k.tasks[CLIENT].quota += 1),
    ("a task is charged one frame too few", |w| w.k.tasks[CLIENT].charged -= 1),
    ("an unused task has a memory quota", |w| w.k.tasks[NOBODY].quota = 1),
    ("an unused task has an address space", |w| w.k.tasks[NOBODY].pd = 0x1000),
    ("a second task has no parent", |w| w.k.tasks[CLIENT].parent = NONE),
    ("a task's parent does not exist", |w| w.k.tasks[CLIENT].parent = NOBODY as u8),
    ("an endpoint names a waiter that does not wait", |w| w.k.eps[SHARED].waiter = 0),
    ("a task waits on an endpoint that does not know of it", |w| w.k.eps[OWN].waiter = NONE),
    ("a sender's badge is on none of its capabilities", |w| w.k.tasks[CLIENT].send_badge = 0x99),
    ("a client waits for the answer of a task that does not exist", |w| {
        w.k.tasks[CLIENT].state = State::Reply(NOBODY as u8)
    }),
    ("an endpoint's owner does not exist", |w| w.k.eps[OWN].owner = NOBODY as u8),
    ("an interrupt stays bound without its capability", |w| w.k.tasks[0].caps[3] = Cap::Null),
    ("an interrupt is bound to a task that does not exist", |w| w.k.irqs[1].task = NOBODY as u8),
    ("a waiting task's message buffer is not mapped", |w| w.k.tasks[SERVER].msg_ptr = 0x7000_0000),
    ("the kernel has counted an anomaly", |w| w.k.stats.anomalies = 1),
    ("a task holds a capability for the kernel's own memory", |w| {
        forge(w, Cap::Memory { page: 0x100, pages: 16, write: true })
    }),
    ("a task holds a capability for the timer interrupt", |w| forge(w, Cap::Irq { mask: 1 })),
];

/// Forged capabilities that look exactly like ones a task may come to hold honestly: the
/// kernel keeps no record that could tell them apart.
const UNSEEN: &[Seed] = &[
    ("a forged capability for every I/O port", |w| forge(w, Cap::IoPort { first: 0, last: 0xFFFF })),
    ("a forged capability to write to the log", |w| forge(w, Cap::Log)),
    ("a forged handle for a task that is not the holder's child", |w| {
        let gen = w.k.tasks[SERVER].gen;
        forge(w, Cap::Task { index: SERVER as u8, gen })
    }),
];

fn checker_sensitivity(tally: &mut Tally) {
    tally.section("Would the kernel's checker notice? One corruption at a time, planted in a sound state");
    let Some(mut w) = stage() else {
        tally.check("the stage for the corruptions is set and sound", false);
        return;
    };
    w.save_base();
    let mut reported = 0;
    for (name, plant) in SEEDS {
        crate::trail(format!("tables: planted corruption: {name}"));
        plant(&mut w);
        let rule = w.k.check();
        w.restore_base();
        reported += (rule != 0) as usize;
        tally.check(&format!("{name}: {}", if rule != 0 { format!("rule {rule}") } else { "not noticed".to_string() }), rule != 0);
    }
    let mut unseen = 0;
    for (name, plant) in UNSEEN {
        crate::trail(format!("tables: planted corruption: {name}"));
        plant(&mut w);
        let rule = w.k.check();
        w.restore_base();
        unseen += (rule == 0) as usize;
        println!("  blind {name}: {}", if rule != 0 { format!("rule {rule}") } else { "not noticed, as expected".to_string() });
    }
    tally.check("without a corruption the checker reports nothing", w.k.check() == 0);
    println!("MODEL seeds: {reported} of {} planted corruptions reported by the kernel's checker", SEEDS.len());
    println!("MODEL blind: {unseen} of {} forged capabilities that the checker cannot tell from honest ones went unnoticed", UNSEEN.len());
}

pub fn run() -> bool {
    let mut tally = Tally { passed: 0, failed: 0 };
    boot_facts(&mut tally);
    endpoint_table(&mut tally);
    task_table(&mut tally);
    memory_and_slots(&mut tally);
    clock_and_timeouts(&mut tally);
    revoked_buffer(&mut tally);
    checker_sensitivity(&mut tally);
    let total = tally.passed + tally.failed;
    println!("MODEL tables: {} {}/{total}", if tally.failed == 0 { "PASS" } else { "FAIL" }, tally.passed);
    tally.failed == 0
}
