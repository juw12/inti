//! Directed tests of the kernel on the host: its limits, its clock, and its own checker.
//!
//! The explorer (`explore.rs`) visits every state within a few steps of some starting
//! points. These tests go where it does not get: to the far end of every table (all 32
//! tasks, all 64 endpoints, the last free frame, the last capability slot), to the edges of
//! every range and across page boundaries, through whole seconds of time, and into states
//! that only a broken kernel could be in - to see whether the kernel's invariant checker,
//! on which the explorer leans, would notice them.

use crate::arch::{self, KERNEL_MAP};
use crate::mem::{frame_of, user_range, FOREIGN, NX, RW, US};
use crate::task::{link, Cap, State, CPU_WINDOW_TICKS, IDLE, NONE, ORIGINAL};
use crate::world::{
    elf_headers, Spawn, World, BUFFER, CHILD_FRAMES, FAULT_BADGE, IMAGE_SIZE, LARGE, ROOT_FRAMES, SMALL, TICK,
};
use crate::{get32, put32};
use abi::nr;
use abi::{
    E_FAULT, E_INVAL, E_NOCAP, E_NOMEM, E_NOSLOT, E_PERM, E_TIMEOUT, FOREVER, INFO_TASK, L_FAULT, L_KERNEL, MAP_WRITE,
    MAX_ENDPOINTS, MAX_TASKS, MSG_DATA, MSG_HEADER, NO_CAP, PAGE, R_GRANT, R_SEND, ST_READY, USER_BASE, USER_END,
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
        let verdict = w.verify();
        self.verdict(verdict, name);
    }

    /// What `World::verify` found, now or after earlier steps.
    fn verdict(&mut self, verdict: Result<(), String>, name: &str) {
        match verdict {
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

    // The deadline is set by hand, so that one tick falls a microsecond short of it and the
    // next one exactly on it.
    let asleep = w.sleep(0, 1000);
    arch::with(|machine| machine.advance(TICK));
    let now = arch::with(|machine| machine.micros());
    w.k.tasks[0].deadline = now + 1;
    w.interrupt(0);
    let early = w.k.tasks[0].state == State::Sleep;
    arch::with(|machine| machine.advance(TICK));
    let now = arch::with(|machine| machine.micros());
    w.k.tasks[0].deadline = now;
    w.interrupt(0);
    tally.check(
        "a sleeper is left asleep a microsecond before its deadline, and woken by the tick that falls exactly on it",
        asleep.is_none() && early && woke_with(&w, 0, 0),
    );

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

// ---- Edges, whole messages, a larger program, reports, turns ------------------------------------

fn word_at(w: &World, t: usize, va: u32) -> u32 {
    let mut word = [0; 4];
    w.peek(t, va, &mut word);
    u32::from_le_bytes(word)
}

/// The slot that a system call returned, if it returned one.
fn slot_of(result: Option<i32>) -> Option<u32> {
    result.filter(|slot| *slot >= 0).map(|slot| slot as u32)
}

/// The capability that the root task holds in the slot that a system call returned.
fn held(w: &World, result: Option<i32>) -> Cap {
    slot_of(result).map_or(Cap::Null, |slot| w.k.tasks[0].caps[slot as usize])
}

/// The root task reads `width` ports from `port` on, or writes them.
fn port_in(w: &mut World, slot: u32, port: u32, width: u32) -> Option<i32> {
    w.syscall(0, nr::IO_IN, [slot, port, width, 0])
}

fn port_out(w: &mut World, slot: u32, port: u32, width: u32) -> Option<i32> {
    w.syscall(0, nr::IO_OUT, [slot, port, width, 0])
}

fn edges(tally: &mut Tally) {
    tally.section("The edges of every range");
    let mut w = World::boot(SMALL, true);
    let Cap::Memory { page: screen, pages: 8, write: true } = w.k.tasks[0].caps[4] else {
        tally.check("the root task holds the eight pages of the screen, to write", false);
        return;
    };
    let pd = w.k.tasks[0].pd;

    // User space: a range may end at its top, and not one page beyond.
    let last = USER_END - PAGE;
    let across = [w.map(0, last, 2, true), w.unmap(0, last, 2), w.map_mem(0, 4, last, 0, 2)];
    tally.check(
        "two pages from the last page of user space on are refused with E_INVAL: to map, to unmap, and for a device",
        across == [Some(E_INVAL); 3] && w.k.mem.lookup(pd, last).is_none(),
    );
    let mapped = w.map(0, last, 1, true) == Some(0) && w.poke(0, USER_END - 1, &[0x5A]);
    tally.check(
        "the last page itself can be mapped, and written to its last byte",
        mapped && word_at(&w, 0, USER_END - 4) == 0x5A00_0000,
    );
    let unmapped = w.unmap(0, last, 1) == Some(0) && w.k.mem.lookup(pd, last).is_none();
    tally.check("and unmapped again", unmapped);
    tally.sound(&mut w, "the state afterwards is sound");

    // Parts of a Memory capability.
    let middle = w.derive(0, 4, 2, 3, 1);
    let tail = w.derive(0, 4, 5, 3, 0);
    tally.check(
        "three pages from the middle of the screen can be derived from its capability, to write",
        held(&w, middle) == Cap::Memory { page: screen + 2, pages: 3, write: true },
    );
    tally.check(
        "and its last three pages, to read only",
        held(&w, tail) == Cap::Memory { page: screen + 5, pages: 3, write: false },
    );
    let Some(tail) = slot_of(tail) else { return };
    let refused = [w.derive(0, 4, 6, 3, 0), w.derive(0, 4, 8, 1, 0), w.derive(0, 4, 0, 0, 0), w.derive(0, tail, 0, 1, 1)];
    tally.check(
        "a part that ends beyond the screen, starts at its end or has no pages is refused with E_PERM, \
         and so is a part to write from a part to read",
        refused == [Some(E_PERM); 4],
    );

    // Mapping through a Memory capability.
    let whole = w.map_mem(0, 4, DEVICE_VA, 6, 2 | MAP_WRITE);
    let frame = w.k.mem.lookup(pd, DEVICE_VA + PAGE).map(frame_of);
    tally.check(
        "the last two pages of the screen can be mapped, and the second is its last page",
        whole == Some(0) && frame == Some((screen + 7) * PAGE),
    );
    let beyond = w.map_mem(0, 4, DEVICE_VA + 2 * PAGE, 7, 2);
    tally.check(
        "two pages from its last page on are refused with E_INVAL, and nothing is mapped",
        beyond == Some(E_INVAL) && w.k.mem.lookup(pd, DEVICE_VA + 2 * PAGE).is_none(),
    );
    let to_write = w.map_mem(0, tail, DEVICE_VA + 2 * PAGE, 0, 1 | MAP_WRITE);
    let to_read = w.map_mem(0, tail, DEVICE_VA + 2 * PAGE, 0, 1);
    let mapping = w.k.mem.lookup(pd, DEVICE_VA + 2 * PAGE);
    tally.check(
        "through the part to read a page can be mapped to read (it cannot be written), and not to write (E_PERM)",
        to_write == Some(E_PERM) && to_read == Some(0) && mapping.is_some_and(|mapping| mapping & RW == 0),
    );
    tally.sound(&mut w, "the state with these mappings is sound");

    // Ports: an access is `width` ports wide, and every one of them must be allowed.
    let serial = w.derive(0, 2, 0x3F8, 0x3FF, 0);
    tally.check(
        "the eight ports of the serial line can be derived from the capability for every port",
        held(&w, serial) == Cap::IoPort { first: 0x3F8, last: 0x3FF },
    );
    let Some(serial) = slot_of(serial) else { return };
    let inside = [(0x3F8, 1), (0x3FF, 1), (0x3FC, 4)].map(|(port, width)| port_in(&mut w, serial, port, width));
    tally.check("through it the first port can be read, the last, and the last four at once", inside == [Some(0); 3]);
    let outside = [(0x3F7, 1), (0x400, 1), (0x3FF, 2), (0x3FD, 4)].map(|(port, width)| port_in(&mut w, serial, port, width));
    tally.check(
        "the port below, the port above, and two or four ports that begin inside and end above are refused with E_PERM",
        outside == [Some(E_PERM); 4],
    );
    let wrapped = port_in(&mut w, 2, 0xFFFF, 2);
    tally.check("two ports from the last port there is are refused with E_PERM", wrapped == Some(E_PERM));
    let wider = [(0x3F0, 0x3FF), (0x3F8, 0x400), (0x3FF, 0x3F8)].map(|(first, last)| w.derive(0, serial, first, last, 0));
    tally.check(
        "a range that begins below, ends above, or ends before it begins cannot be derived from it (E_PERM)",
        wider == [Some(E_PERM); 3],
    );
    let widths = [0, 3, 8].map(|width| port_in(&mut w, 2, 0x3F8, width));
    tally.check("an access that is 0, 3 or 8 ports wide is refused with E_INVAL", widths == [Some(E_INVAL); 3]);

    let kept = [(0x20, 1), (0x21, 1), (0x1F, 2), (0x40, 1), (0x43, 1), (0x3D, 4), (0x61, 1), (0xA0, 1), (0xA1, 1)];
    let beside = [(0x1F, 1), (0x22, 2), (0x3C, 4), (0x44, 1), (0x60, 1), (0x62, 2), (0x9F, 1), (0xA2, 1)];
    let refused = kept.map(|(port, width)| port_out(&mut w, 2, port, width));
    let allowed = beside.map(|(port, width)| port_out(&mut w, 2, port, width));
    tally.check(
        "the ports the kernel keeps to itself are refused with E_PERM even to the capability for every port, \
         also to an access that only ends in them",
        refused == [Some(E_PERM); 9],
    );
    tally.check("the ports beside them, up to the very next one, are not", allowed == [Some(0); 8]);
    tally.sound(&mut w, "the state afterwards is sound");
}

/// A message of the largest size there is, whose header claims still more: that it comes
/// from the kernel, and that it holds more data than a message can.
fn oversize(label: u32, first: u8) -> [u8; MSG_HEADER + MSG_DATA] {
    let mut message = [0; MSG_HEADER + MSG_DATA];
    put32(&mut message, 0, L_KERNEL | label);
    put32(&mut message, 12, u32::MAX);
    put32(&mut message, 32, NO_CAP);
    for (i, byte) in message[MSG_HEADER..].iter_mut().enumerate() {
        *byte = first.wrapping_add(i as u8);
    }
    message
}

fn messages(tally: &mut Tally) {
    tally.section("Messages: their order, their size, and buffers that lie across two pages");
    let mut w = World::boot(SMALL, true);
    let mut built = w.ep_create(0) == Some(6);
    // All three copies first: each spawn takes a slot of the root task for the handle.
    for (i, badge) in [0xA1, 0xA2, 0xA3].into_iter().enumerate() {
        built &= w.derive(0, 6, R_SEND, badge, 0) == Some(7 + i as i32);
    }
    for client in 1..=3 {
        built &= w.spawn_task(0, &Spawn::PLAIN.with_caps(&[6 + client as u32])) == Ok(client);
    }
    built &= w.map(0, HEAP, 2, true) == Some(0) && w.map(1, HEAP, 2, true) == Some(0);
    tally.check("three clients start, each with a send-only copy of one endpoint under a badge of its own", built);
    if !built {
        return;
    }

    // Callers are served in the order in which they called, whatever their numbers.
    let mut served = true;
    for client in [3, 1, 2] {
        served &= w.call(client, 0, NO_CAP, FOREVER).is_none();
    }
    let (mut heard, mut intact) = ([0; 3], Ok(()));
    for badge in &mut heard {
        served &= w.recv(0, 6, FOREVER) == Some(0);
        intact = intact.and(w.verify());
        *badge = word_at(&w, 0, BUFFER + 4);
        let token = w.token(0);
        served &= w.reply(0, token, NO_CAP) == Some(0);
        intact = intact.and(w.verify());
    }
    tally.check(
        "clients that call in the order 3, 1, 2 are heard in that order, each under its badge",
        served && heard == [0xA3, 0xA1, 0xA2],
    );
    tally.verdict(intact, "each of the three messages and each answer arrived intact");

    // A message of the full size that claims still more, in buffers that lie across two pages.
    let (sent_at, received_at, answered_at) = (HEAP + PAGE - 100, HEAP + PAGE - 200, HEAP + PAGE - 30);
    let sent = oversize(0x77, 3);
    let called = w.poke(1, sent_at, &sent) && w.syscall(1, nr::CALL, [0, sent_at, FOREVER, 0]).is_none();
    let received = w.syscall(0, nr::RECV, [6, received_at, FOREVER, 0]) == Some(0);
    let mut arrived = [0; MSG_HEADER + MSG_DATA];
    w.peek(0, received_at, &mut arrived);
    tally.check("a call and a receive with such buffers succeed", called && received);
    tally.check("all 256 bytes of the data arrive", arrived[MSG_HEADER..] == sent[MSG_HEADER..]);
    tally.check(
        "the label arrives without the bit that marks a message from the kernel, under the badge of the caller's copy",
        get32(&arrived, 0) == 0x77 && get32(&arrived, 4) == 0xA1,
    );
    tally.check("a length that claims more than a message can hold arrives as 256", get32(&arrived, 12) == MSG_DATA as u32);
    let answer = oversize(0x78, 9);
    let token = get32(&arrived, 8);
    let answered = w.poke(0, answered_at, &answer) && w.syscall(0, nr::REPLY, [token, answered_at, 0, 0]) == Some(0);
    let mut back = [0; MSG_HEADER + MSG_DATA];
    w.peek(1, sent_at, &mut back);
    let whole = get32(&back, 0) == 0x78 && get32(&back, 12) == MSG_DATA as u32 && back[MSG_HEADER..] == answer[MSG_HEADER..];
    tally.check("the answer arrives in the same way, in the buffer of the call", answered && whole && woke_with(&w, 1, 0));
    tally.sound(&mut w, "the state afterwards is sound");
}

fn larger_program(tally: &mut Tally) {
    // More than one page of file, and memory beyond the end of the file.
    const FILE: u32 = 5000;
    const MEMORY: u32 = 9000;
    tally.section("A program of more than one page");
    let mut w = World::boot(SMALL, true);
    let mut program = vec![0; FILE as usize];
    elf_headers(&mut program, FILE, MEMORY);
    for (i, byte) in program.iter_mut().enumerate().skip(IMAGE_SIZE as usize) {
        *byte = (i % 251) as u8 | 1;
    }
    let placed = w.map(0, HEAP, 2, true) == Some(0) && w.poke(0, HEAP + 0x100, &program);
    let started = w.spawn_from(0, &Spawn::PLAIN, HEAP + 0x100, FILE);
    let name = format!("a program of {FILE} bytes that asks for {MEMORY} bytes of memory starts");
    let Some(slot) = slot_of(started).filter(|_| placed) else {
        tally.check(&format!("{name}: {started:?}"), false);
        return;
    };
    tally.check(&name, true);
    let child = w.child(0, slot);
    let mut loaded = vec![0xEE; 3 * PAGE as usize];
    let read = w.peek(child, USER_BASE, &mut loaded);
    let (file, rest) = loaded.split_at(FILE as usize);
    tally.check("every byte of the file is in the memory of the new task", read && file == &program[..]);
    tally.check("the memory beyond the file is zero, to the end of its last page", read && rest.iter().all(|byte| *byte == 0));
    tally.check(
        "the task holds two frames more than a task with one page of program",
        w.k.tasks[child].charged == CHILD_FRAMES + 2,
    );
    tally.sound(&mut w, "the state afterwards is sound");
}

fn reports(tally: &mut Tally) {
    // Where the root task has its reports written: in its stack page, clear of its messages.
    const REPORT: u32 = BUFFER + 0x400;
    tally.section("What the kernel reports about a task, and about its end");
    let mut w = World::boot(SMALL, true);
    let endpoint = w.ep_create(0);
    let listening = Spawn { fault: 6, ..Spawn::PLAIN };
    let spec = Spawn { tasks: 2, eps: 3, ..listening }.with_caps(&[0, 1]);
    let (Ok(child), Ok(plain)) = (w.spawn_task(0, &spec), w.spawn_task(0, &Spawn::PLAIN)) else {
        tally.check("two tasks start", false);
        return;
    };
    w.time(child);
    w.time(child);
    let task = &w.k.tasks[child];
    let expected = [ST_READY, 0, task.gen as u32, spec.quota, task.charged, task.cpu_ticks, 2, 2, 2, 3];
    let asked = w.syscall(0, nr::SYSINFO, [1, INFO_TASK, child as u32, REPORT]);
    let mut report = [0; 60];
    w.peek(0, REPORT, &mut report);
    let words: Vec<u32> = (0..10).map(|i| get32(&report, 12 + 4 * i)).collect();
    if words != expected {
        println!("  note  the kernel reports {words:?} where this model expects {expected:?}");
    }
    tally.check(
        "the root task is told the name under which a task was started",
        endpoint == Some(6) && asked == Some(0) && report[..12] == *b"model\0\0\0\0\0\0\0",
    );
    tally.check(
        "and its state, parent, generation, memory quota, frames, timer ticks, system calls and capabilities, \
         and how many tasks and endpoints it may create",
        words == expected,
    );
    let refused = [
        w.syscall(plain, nr::SYSINFO, [0, INFO_TASK, child as u32, REPORT]),
        w.syscall(0, nr::SYSINFO, [0, INFO_TASK, child as u32, REPORT]),
        w.syscall(0, nr::SYSINFO, [1, INFO_TASK, MAX_TASKS as u32, REPORT]),
        w.syscall(0, nr::SYSINFO, [1, INFO_TASK, child as u32, HEAP]),
    ];
    tally.check(
        "a task with an empty slot is told nothing (E_NOCAP), nor is one that shows another capability (E_NOCAP); \
         a number beyond the task table is refused (E_INVAL), and so is a place that is not mapped (E_FAULT)",
        refused == [Some(E_NOCAP), Some(E_NOCAP), Some(E_INVAL), Some(E_FAULT)],
    );

    // The end of a task that was started with an endpoint for the news of it.
    let eip = w.k.tasks[child].frame.eip;
    w.fault(child, 0xDEAD_B000);
    let told = w.syscall(0, nr::RECV, [6, BUFFER, FOREVER, 0]) == Some(0);
    let mut notice = [0; MSG_HEADER];
    w.peek(0, BUFFER, &mut notice);
    tally.check(
        "after a page fault that endpoint has a message from the kernel, under the badge chosen at the start",
        told && get32(&notice, 0) == L_FAULT && get32(&notice, 4) == FAULT_BADGE,
    );
    let words = [16, 20, 24, 28].map(|at| get32(&notice, at));
    tally.check(
        "the message names the exception, the address, where the task was, and the error code",
        words == [14, 0xDEAD_B000, eip, 4],
    );
    let Ok(second) = w.spawn_task(0, &listening) else {
        tally.check("another task starts", false);
        return;
    };
    let eip = w.k.tasks[second].frame.eip;
    w.exception(second, 13, 0xDEAD_B000);
    let told = w.syscall(0, nr::RECV, [6, BUFFER, FOREVER, 0]) == Some(0);
    w.peek(0, BUFFER, &mut notice);
    let words = [16, 20, 24, 28].map(|at| get32(&notice, at));
    tally.check(
        "after another exception the message names no address, whatever the last page fault left behind",
        told && get32(&notice, 0) == L_FAULT && words == [13, 0, eip, 0],
    );

    // What a task used, told with the news of its end. The task that never listens is ended
    // first, so that the next one is alone on the processor while the root task sleeps.
    w.fault(plain, 0x10);
    let Ok(third) = w.spawn_task(0, &listening) else {
        tally.check("a third task starts", false);
        return;
    };
    w.time(third);
    w.sleep(0, 50);
    w.pass(60_000);
    let (calls, ticks) = (w.k.tasks[third].syscalls, w.k.tasks[third].cpu_ticks);
    w.fault(third, 0xDEAD_B000);
    let told = w.syscall(0, nr::RECV, [6, BUFFER, FOREVER, 0]) == Some(0);
    let mut used = [0; MSG_HEADER + 8];
    w.peek(0, BUFFER, &mut used);
    println!("  note  the task that ended had made {calls} system call(s) and run for {ticks} timer tick(s)");
    tally.check(
        "the message also carries the system calls the task made and, after them, the timer ticks it ran for",
        told && ticks > 0
            && ticks != calls
            && get32(&used, MSG_HEADER) == calls
            && get32(&used, MSG_HEADER + 4) == ticks,
    );
    tally.sound(&mut w, "the state afterwards is sound");
}

// ---- Shares of the processor ---------------------------------------------------------------

/// Let the current window of the accounting end, then a whole window in which no system call
/// is made - the next one's shares are measured against its length - and then the window
/// whose figures are read afterwards.
fn measured_window(w: &mut World) {
    loop {
        w.ticks(1);
        if w.k.ticks % CPU_WINDOW_TICKS == 0 {
            break;
        }
    }
    w.ticks(2 * CPU_WINDOW_TICKS as u32);
}

/// Who the scheduler runs if task `t` has just been woken.
fn chosen_after_waking(w: &mut World, t: usize) -> usize {
    w.k.ready(t);
    w.k.schedule();
    w.k.cur
}

/// A spawn with this share is refused with E_PERM and leaves no task behind.
fn share_refused(w: &mut World, t: usize, cpu: u32) -> bool {
    let before = w.tasks();
    w.spawn(t, &Spawn { cpu, ..Spawn::PLAIN }) == Some(E_PERM) && w.tasks() == before
}

fn shares(tally: &mut Tally) {
    const REPORT: u32 = BUFFER + 0x400;
    // A group: a task held to 20% of the processor that may start four more.
    let group = Spawn { quota: 220, tasks: 4, cpu: 20, ..Spawn::PLAIN };
    let limit = |w: &World, t: usize| w.k.tasks[t].cpu_limit;
    let used = |w: &World, t: usize| w.k.share_used(t);

    tally.section("A share of the processor: who may have which");
    let mut w = World::boot(LARGE, true);
    let (Ok(a), Ok(b)) = (w.spawn_task(0, &group), w.spawn_task(0, &Spawn::PLAIN)) else {
        tally.check("two tasks start", false);
        return;
    };
    tally.check(
        "the root task has the whole processor; a task started with 20% has 20%, one started with no figure has its parent's",
        limit(&w, 0) == 100 && limit(&w, a) == 20 && limit(&w, b) == 100,
    );
    tally.check(
        "the root task cannot give 101%, nor 256% or 276%: the figure is not cut down to eight bits (E_PERM, no task started)",
        [101, 256, 276, u32::MAX].iter().all(|&cpu| share_refused(&mut w, 0, cpu)),
    );
    let heirs: Vec<u8> = [0, 20, 10]
        .iter()
        .map(|&cpu| w.spawn_task(a, &Spawn { cpu, ..Spawn::PLAIN }).map_or(0, |heir| limit(&w, heir)))
        .collect();
    tally.check("a task with 20% passes on 20% when it names no figure, and may give 20% or 10%", heirs == [20, 20, 10]);
    tally.check(
        "it cannot give 21% or 100%, though it may still start a task (E_PERM, no task started)",
        w.k.tasks[a].task_quota == 1 && share_refused(&mut w, a, 21) && share_refused(&mut w, a, 100),
    );
    tally.sound(&mut w, "all that must hold after any step holds with shares handed down");

    tally.section("A share of the processor: what it does, in whole timer periods of the fake machine");
    let mut w = World::boot(LARGE, true);
    let (Ok(a), Ok(b)) = (w.spawn_task(0, &group), w.spawn_task(0, &Spawn::PLAIN)) else {
        tally.check("two tasks start", false);
        return;
    };
    measured_window(&mut w);
    println!(
        "  note  of one window, in thousandths: the task held to 20% used {}, the other task {}, the root task with all it started {}",
        used(&w, a),
        used(&w, b),
        used(&w, 0)
    );
    tally.check(
        "beside two tasks that never stop, a task held to 20% that never stops gets exactly 20% of a window",
        used(&w, a) == 200,
    );
    tally.check("the other two get the rest: the second task at least 39%", used(&w, b) >= 390 && used(&w, b) <= 410);
    tally.check("a task is charged for all that it started: the root task for the whole window", used(&w, 0) == 1000);
    let asked = w.syscall(0, nr::SYSINFO, [1, INFO_TASK, a as u32, REPORT]);
    let mut report = [0; 60];
    w.peek(0, REPORT, &mut report);
    tally.check(
        "the kernel reports a task's share and what it used of the last whole window",
        asked == Some(0) && get32(&report, 12 + 4 * 10) == 20 && get32(&report, 12 + 4 * 11) == 200,
    );

    measured_window(&mut w);
    tally.check(
        "at the start of a window the task is within its share: when it wakes it runs at once",
        !w.k.over_share(a) && chosen_after_waking(&mut w, a) == a,
    );
    w.ticks(70);
    tally.check(
        "70 periods later it has used its share: when it wakes, a task within its share runs first",
        w.k.over_share(a) && chosen_after_waking(&mut w, a) != a,
    );

    measured_window(&mut w);
    let Ok(g) = w.spawn_task(a, &Spawn::PLAIN) else {
        tally.check("the task held to 20% starts a task", false);
        return;
    };
    measured_window(&mut w);
    println!("  note  with a task of its own, the group used {} thousandths, of which that task {}", used(&w, a), used(&w, g));
    tally.check(
        "a task and the task it started get 20% between them, not 20% each",
        limit(&w, g) == 20 && used(&w, a) == 200 && used(&w, g) > 0 && used(&w, g) < 200,
    );

    // Nobody else wants the processor: the root task and the second task sleep for a minute.
    let asleep = w.sleep(0, 60_000).is_none() && w.sleep(b, 60_000).is_none();
    w.ticks(30);
    let both_over = w.k.over_share(a) && w.k.over_share(g);
    let Ok(c) = w.spawn_task(a, &Spawn::PLAIN) else {
        tally.check("a task that has used its share starts a task", false);
        return;
    };
    tally.check(
        "a task started by one that has used its share is over the share from its first moment: starting tasks is no way round it",
        asleep && both_over && w.k.over_share(c),
    );
    measured_window(&mut w);
    tally.check(
        "while nobody else wants the processor the group has all of it: a share never leaves the processor idle",
        used(&w, a) == 1000 && used(&w, b) == 0,
    );

    let all_asleep = [a, g, c].iter().all(|&t| w.sleep(t, 60_000).is_none());
    let idle = w.k.cur == IDLE;
    measured_window(&mut w);
    tally.check(
        "with every task asleep the processor idles, and the idle time is charged to nobody",
        all_asleep && idle && [0, a, b, g, c].iter().all(|&t| used(&w, t) == 0),
    );
    tally.sound(&mut w, "all that must hold after any step holds at the end");
}

fn turns(tally: &mut Tally) {
    tally.section("Turns");
    let mut w = World::boot(SMALL, true);
    let (Ok(a), Ok(b)) = (w.spawn_task(0, &Spawn::PLAIN), w.spawn_task(0, &Spawn::PLAIN)) else {
        tally.check("two tasks start", false);
        return;
    };
    let tasks = [0, a, b];
    let before = tasks.map(|t| w.k.tasks[t].cpu_ticks);
    let mut ran = [0; 6];
    for turn in &mut ran {
        *turn = w.k.cur;
        w.pass(10_000);
    }
    let used: Vec<u32> = (0..3).map(|i| w.k.tasks[tasks[i]].cpu_ticks.wrapping_sub(before[i])).collect();
    println!("  note  six timer ticks find these tasks running: {ran:?}; of the ticks, the three tasks are charged {used:?}");
    tally.check(
        "each of three tasks that never wait has a turn within three timer ticks",
        tasks.iter().all(|t| ran[..3].contains(t)),
    );
    tally.check("the turns come round again in the same order", ran[..3] == ran[3..]);
    tally.check("each task is charged the two ticks that ended its turns", used == [2, 2, 2]);
    tally.sound(&mut w, "the state afterwards is sound");
}

fn derived_powers(tally: &mut Tally) {
    tally.section("Badges, interrupt lines and the edges of user space");
    let mut w = World::boot(SMALL, true);
    let made = w.ep_create(0);
    let Cap::Endpoint { ep, badge: 0, .. } = held(&w, made) else {
        tally.check("the root task makes an endpoint, and its capability has no badge", false);
        return;
    };
    let whole = 6;
    let sender = |badge| Cap::Endpoint { ep, rights: R_SEND as u8, badge };
    let stamped = w.derive(0, whole, R_SEND, 0xA1, 0);
    let stamped_slot = slot_of(stamped).unwrap_or(NO_CAP);
    tally.check("a holder of the grant right stamps a badge on a capability without one", held(&w, stamped) == sender(0xA1));
    let granting = w.derive(0, whole, R_SEND | R_GRANT, 0xB1, 0);
    let granting_slot = slot_of(granting).unwrap_or(NO_CAP);
    tally.check(
        "a badge cannot be changed, even by a holder of the grant right",
        granting_slot != NO_CAP && w.derive(0, granting_slot, R_SEND, 0xB2, 0) == Some(E_PERM),
    );
    let plain = w.derive(0, whole, R_SEND, 0, 0);
    let plain_slot = slot_of(plain).unwrap_or(NO_CAP);
    tally.check("a copy made with no badge named has none", held(&w, plain) == sender(0));
    tally.check(
        "a capability without the grant right cannot be stamped",
        plain_slot != NO_CAP && w.derive(0, plain_slot, R_SEND, 0xC1, 0) == Some(E_PERM),
    );
    let same = w.derive(0, stamped_slot, R_SEND, 0xA1, 0);
    tally.check("a copy that names the badge it already has keeps it", held(&w, same) == sender(0xA1));
    let kept = w.derive(0, stamped_slot, R_SEND, 0, 0);
    tally.check("a copy of a badged capability that names no badge keeps the badge", held(&w, kept) == sender(0xA1));

    let Cap::Irq { mask } = w.k.tasks[0].caps[3] else {
        tally.check("the root task holds the interrupt lines", false);
        return;
    };
    let line = w.derive(0, 3, 0x0002, 0, 0);
    let line_slot = slot_of(line).unwrap_or(NO_CAP);
    tally.check("one interrupt line can be split off the lines a task holds", held(&w, line) == Cap::Irq { mask: 0x0002 });
    tally.check("a capability for no interrupt line at all is refused", w.derive(0, 3, 0, 0, 0) == Some(E_PERM));
    tally.check(
        "the line of the timer, which no task holds, cannot be derived",
        mask & 1 == 0 && w.derive(0, 3, 0x0001, 0, 0) == Some(E_PERM),
    );
    tally.check(
        "a capability for one line cannot be widened to two",
        line_slot != NO_CAP && w.derive(0, line_slot, 0x0006, 0, 0) == Some(E_PERM),
    );
    tally.sound(&mut w, "the state afterwards is sound");

    tally.check("the first page of user space is a range a task may name", user_range(USER_BASE, 1).is_ok());
    tally.check("the page below user space is not", user_range(USER_BASE - PAGE, 1).is_err());
    tally.check("the last page of user space is a range a task may name", user_range(USER_END - PAGE, 1).is_ok());
    tally.check("a range that runs one page past the end is not", user_range(USER_END - PAGE, 2).is_err());
    tally.check("a range of no pages, or one that starts inside a page, is not", {
        user_range(USER_BASE, 0).is_err() && user_range(USER_BASE + 1, 1).is_err()
    });
}

fn refused_programs(tally: &mut Tally) {
    tally.section("Programs with the right first bytes and a wrong header");
    let mut w = World::boot(SMALL, true);
    let mut good = vec![0; IMAGE_SIZE as usize];
    elf_headers(&mut good, IMAGE_SIZE, IMAGE_SIZE);
    let mapped = w.map(0, HEAP, 1, true) == Some(0);
    let wrong: [(&str, usize, u8); 5] = [
        ("that is not an executable", 16, 3),
        ("for another processor", 18, 40),
        ("whose segment entries have the wrong size", 42, 0),
        ("with no segments", 44, 0),
        ("with seventeen segments", 44, 17),
    ];
    for (what, offset, value) in wrong {
        let mut program = good.clone();
        program[offset] = value;
        let before = w.tasks();
        let placed = mapped && w.poke(0, HEAP + 0x100, &program);
        let started = w.spawn_from(0, &Spawn::PLAIN, HEAP + 0x100, IMAGE_SIZE);
        tally.check(
            &format!("a program {what} is refused, and no task is left behind"),
            placed && started == Some(E_INVAL) && w.tasks() == before,
        );
    }
    let before = w.tasks();
    let placed = mapped && w.poke(0, HEAP + 0x100, &good);
    let started = w.spawn_from(0, &Spawn::PLAIN, HEAP + 0x100, IMAGE_SIZE);
    tally.check(
        "the same program with none of those bytes changed starts",
        placed && slot_of(started).is_some() && w.tasks() == before + 1,
    );
    tally.sound(&mut w, "the state afterwards is sound");
}

fn counts(tally: &mut Tally) {
    tally.section("What the kernel counts");
    let mut w = World::boot(LARGE, true);
    let calls = w.k.stats.syscalls;
    let made = w.ep_create(0).is_some();
    tally.check("a system call that succeeds is counted once", made && w.k.stats.syscalls == calls.wrapping_add(1));
    let refused = w.derive(0, NO_CAP, 1, 0, 0);
    tally.check(
        "a system call that is refused is counted once",
        refused.is_some_and(|result| result < 0) && w.k.stats.syscalls == calls.wrapping_add(2),
    );
    let spawns = w.k.stats.spawns;
    let Ok(child) = w.spawn_task(0, &Spawn::PLAIN) else {
        tally.check("a task starts", false);
        return;
    };
    tally.check("a task that is started is counted once", w.k.stats.spawns == spawns.wrapping_add(1));
    let faults = w.k.stats.faults;
    w.fault(child, 0x10);
    tally.check("a page fault of a task is counted once", w.k.stats.faults == faults.wrapping_add(1));

    // The root task runs for all but the last timer period of a window and then sleeps, so
    // that the window ends while nobody runs. Its figure for that window must hold exactly
    // the periods it ran, measured against the whole window.
    loop {
        w.ticks(1);
        if w.k.ticks % CPU_WINDOW_TICKS == 0 {
            break;
        }
    }
    let periods = CPU_WINDOW_TICKS as u32;
    w.ticks(periods - 1);
    let asleep = w.sleep(0, 1000).is_none();
    w.ticks(1);
    let ended = w.k.ticks % CPU_WINDOW_TICKS == 0;
    let ran = w.k.tasks[0].cpu_last as u32;
    let expected = (periods - 1) * 1000 / periods;
    tally.check(
        &format!(
            "a task that ran all but the last of the {periods} periods of a window is reported \
             at {expected} thousandths of it: {ran}"
        ),
        asleep && ended && ran == expected,
    );
    tally.sound(&mut w, "the state afterwards is sound");
}

// ---- Places that a changed kernel got through unnoticed in round 4 ----------------------------

/// A sender that waits with a capability to hand over, and loses the grant right meanwhile.
fn grant_taken_from_a_waiting_sender(tally: &mut Tally) {
    tally.section("A waiting sender that loses the right to hand a capability over");
    for revoked in [false, true] {
        let mut w = World::boot(SMALL, true);
        let made = w.ep_create(0);
        let Cap::Endpoint { ep, .. } = held(&w, made) else {
            tally.check("the root task holds an endpoint", false);
            return;
        };
        let plain = slot_of(w.derive(0, 6, R_SEND, 0, 0));
        let granting = slot_of(w.derive(0, 6, R_SEND | R_GRANT, 0, 0));
        let offered = slot_of(w.derive(0, 6, R_SEND, 0, 0));
        let (Some(plain), Some(granting), Some(offered)) = (plain, granting, offered) else {
            tally.check("three capabilities are derived", false);
            return;
        };
        let Ok(client) = w.spawn_task(0, &Spawn::PLAIN.with_caps(&[plain, granting, offered])) else {
            tally.check("a client starts", false);
            return;
        };
        let blocked = w.call(client, 1, 2, FOREVER).is_none() && w.k.tasks[client].state == State::Send(ep);
        let taken = !revoked || w.revoke(0, granting) == Some(0);
        let still = w.k.tasks[client].state == State::Send(ep);
        let lost = w.k.tasks[client].caps[1] == Cap::Null;
        let before = w.caps();
        let received = w.recv(0, 6, 0);
        if revoked {
            tally.check(
                "a sender whose granting capability is revoked while it waits stays in the queue,                  for it still holds a plain one",
                blocked && taken && still && lost,
            );
            tally.check(
                "when its turn comes it is refused (E_PERM); the receiver gets no message and no capability",
                woke_with(&w, client, E_PERM) && received == Some(E_TIMEOUT) && w.caps() == before,
            );
        } else {
            tally.check(
                "a sender that keeps its granting capability hands the offered one over with its message",
                blocked && taken && still && !lost && received == Some(0) && w.caps() == before + 1
                    && matches!(w.k.tasks[client].state, State::Reply(_)),
            );
        }
        tally.sound(&mut w, "the state afterwards is sound");
    }
}

/// Device memory held in two pieces: each capability covers its own frames and no frame more.
fn device_memory_ends_where_it_ends(tally: &mut Tally) {
    tally.section("Where a piece of device memory ends");
    let mut w = World::boot(SMALL, true);
    let Cap::Memory { page: screen, .. } = w.k.tasks[0].caps[4] else {
        tally.check("the root task holds the screen's memory", false);
        return;
    };
    let lower = slot_of(w.derive(0, 4, 0, 1, 1));
    let upper = slot_of(w.derive(0, 4, 1, 1, 1));
    let (Some(lower), Some(upper)) = (lower, upper) else {
        tally.check("two one-page pieces are derived", false);
        return;
    };
    let Ok(child) = w.spawn_task(0, &Spawn::PLAIN.with_caps(&[lower, upper])) else {
        tally.check("a task starts with both pieces", false);
        return;
    };
    let task = &w.k.tasks[child];
    tally.check(
        "a task with the first two pages holds the last byte of the second, and nothing of the page after it",
        task.has_memory((screen + 2) * PAGE - 1, true) && !task.has_memory((screen + 2) * PAGE, false),
    );
    let pd = w.k.tasks[child].pd;
    let mapped = w.map_mem(child, 1, DEVICE_VA, 0, 1 | MAP_WRITE) == Some(0);
    let placed = w.k.mem.lookup(pd, DEVICE_VA).map(frame_of) == Some((screen + 1) * PAGE);
    tally.check("the second piece maps the second page", mapped && placed);
    let dropped = w.drop_cap(child, 1) == Some(0);
    tally.check(
        "when that piece is dropped its page is unmapped, though the task still holds the page before it",
        dropped && w.k.mem.lookup(pd, DEVICE_VA).is_none() && w.k.tasks[child].has_memory(screen * PAGE, true),
    );
    tally.sound(&mut w, "the state afterwards is sound");
}

/// Segments that do not begin at the first page, or not at the start of a page.
fn segments_off_the_first_page(tally: &mut Tally) {
    tally.section("Programs whose segment is not at the start of the first page");
    const INSIDE: u32 = 0x11;
    let mut w = World::boot(SMALL, true);
    let mapped = w.map(0, HEAP, 1, true) == Some(0);

    let mut second = vec![0; IMAGE_SIZE as usize];
    elf_headers(&mut second, IMAGE_SIZE, IMAGE_SIZE);
    put32(&mut second, 24, USER_BASE + PAGE + 84);
    put32(&mut second, 52 + 8, USER_BASE + PAGE);
    put32(&mut second, 52 + 12, USER_BASE + PAGE);
    let placed = mapped && w.poke(0, HEAP + 0x100, &second);
    let started = slot_of(w.spawn_from(0, &Spawn::PLAIN, HEAP + 0x100, IMAGE_SIZE));
    let Some(slot) = started else {
        tally.check("a program whose segment is on the second page starts", false);
        return;
    };
    let child = w.child(0, slot);
    let pd = w.k.tasks[child].pd;
    let mut loaded = vec![0; IMAGE_SIZE as usize];
    let read = w.peek(child, USER_BASE + PAGE, &mut loaded);
    tally.check(
        "a program whose segment is on the second page is loaded there, and the first page stays unmapped",
        placed && w.k.mem.lookup(pd, USER_BASE).is_none() && read && loaded == second,
    );
    tally.check("it is charged the same frames as any small program", w.k.tasks[child].charged == CHILD_FRAMES);

    let mut inside = vec![0; IMAGE_SIZE as usize];
    elf_headers(&mut inside, IMAGE_SIZE, IMAGE_SIZE);
    put32(&mut inside, 52 + 8, USER_BASE + INSIDE);
    put32(&mut inside, 52 + 12, USER_BASE + INSIDE);
    let placed = mapped && w.poke(0, HEAP + 0x100, &inside);
    let started = slot_of(w.spawn_from(0, &Spawn::PLAIN, HEAP + 0x100, IMAGE_SIZE));
    let Some(slot) = started else {
        tally.check("a program whose segment starts inside a page starts", false);
        return;
    };
    let child = w.child(0, slot);
    let mut page = vec![0xFF; (INSIDE + IMAGE_SIZE) as usize];
    let read = w.peek(child, USER_BASE, &mut page);
    let (gap, body) = page.split_at(INSIDE as usize);
    tally.check(
        "a segment that starts inside a page is loaded at its address, and the bytes before it are zero",
        placed && read && gap.iter().all(|&byte| byte == 0) && body == inside.as_slice(),
    );
    tally.sound(&mut w, "the state afterwards is sound");
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

/// A corruption, the rule of the checker that describes it (0 if none does), and how to plant it.
type Seed = (&'static str, u32, fn(&mut World));

/// States no sequence of system calls should lead to. The checker must report each, and under
/// the rule that describes it: a corruption that is reported only for what it does to something
/// else is not yet known to be seen.
const SEEDS: &[Seed] = &[
    ("an endpoint counts one capability too many", 8, |w| w.k.eps[SHARED].refs += 1),
    ("an endpoint counts one receiver too many", 9, |w| w.k.eps[SHARED].recv_refs += 1),
    ("a copy has a right that its source lacks", 19, |w| {
        w.k.tasks[CLIENT].caps[0] = Cap::Endpoint { ep: SHARED as u8, rights: (R_SEND | R_GRANT) as u8, badge: 0x10 }
    }),
    ("a copy has another badge than its source", 19, |w| {
        w.k.tasks[CLIENT].caps[5] = Cap::Endpoint { ep: SHARED as u8, rights: R_SEND as u8, badge: 0x11 };
        w.k.tasks[CLIENT].from[5] = link(0, 7);
        w.k.eps[SHARED].refs += 1;
    }),
    ("a copy names an empty slot as its source", 19, |w| w.k.tasks[CLIENT].from[0] = link(NOBODY, 3)),
    ("a copy names itself as its source", 19, |w| w.k.tasks[CLIENT].from[0] = link(CLIENT, 0)),
    ("two copies name each other as their source", 19, |w| w.k.tasks[0].from[7] = link(CLIENT, 0)),
    ("a capability names an endpoint beyond the table", 8, |w| {
        w.k.tasks[0].caps[6] = Cap::Endpoint { ep: 200, rights: 7, badge: 0 }
    }),
    ("the count of free frames is one too high", 6, |w| w.k.mem.free += 1),
    ("a frame that a task uses is marked free", 3, |w| {
        let frame = frame_of(entry(w, pte(w, 0, HEAP)));
        w.k.mem.free_frame(frame);
    }),
    ("a frame is taken that no task owns", 6, |w| {
        w.k.mem.alloc();
    }),
    ("one frame is mapped by two tasks", 2, |w| {
        let (theirs, ours) = (entry(w, pte(w, 0, BUFFER)), pte(w, CLIENT, BUFFER - PAGE));
        set_entry(ours, theirs);
    }),
    ("a page of program can be written", 22, |w| {
        let at = pte(w, 0, USER_BASE);
        set_entry(at, entry(w, at) | RW);
    }),
    ("a page of stack can be executed", 22, |w| {
        let at = pte(w, 0, BUFFER);
        set_entry(at, entry(w, at) & !NX);
    }),
    ("a page-table entry has a bit the kernel never sets", 22, |w| {
        let at = pte(w, 0, BUFFER);
        set_entry(at, entry(w, at) | 0x800);
    }),
    ("a page table is closed to its own task", 18, |w| {
        let at = pde(w, 0, BUFFER);
        set_entry(at, entry(w, at) & !US);
    }),
    ("a page table is kept although it maps nothing", 25, |w| {
        let at = pte(w, 0, HEAP);
        let frame = frame_of(entry(w, at));
        set_entry(at, 0);
        w.k.mem.free_frame(frame);
        w.k.tasks[0].charged -= 1;
    }),
    ("the kernel half of a task's address space differs", 18, |w| {
        let at = w.k.tasks[CLIENT].pd;
        set_entry(at, entry(w, at) ^ 0x200);
    }),
    ("an entry of the kernel's own page tables changed", 23, |_| {
        let at = KERNEL_MAP + 0x3008;
        arch::with(|machine| {
            let flipped = machine.byte(at) ^ RW as u8;
            machine.load(at, &[flipped]);
        });
    }),
    ("device memory is mapped as if it were the task's own", 1, |w| {
        let at = pte(w, 0, DEVICE_VA);
        set_entry(at, entry(w, at) & !FOREIGN);
    }),
    ("device memory stays mapped without its capability", 20, |w| w.k.tasks[0].caps[4] = Cap::Null),
    ("a task may create one task too many", 21, |w| w.k.tasks[0].task_quota += 1),
    ("a task may create one endpoint too many", 21, |w| w.k.tasks[0].ep_quota += 1),
    ("a task's memory quota grew by itself", 15, |w| w.k.tasks[CLIENT].quota += 1),
    ("a task is charged one frame too few", 4, |w| w.k.tasks[CLIENT].charged -= 1),
    ("an unused task has a memory quota", 14, |w| w.k.tasks[NOBODY].quota = 1),
    ("an unused task has an address space", 1, |w| w.k.tasks[NOBODY].pd = 0x1000),
    ("a second task has no parent", 15, |w| w.k.tasks[CLIENT].parent = NONE),
    ("a task's parent does not exist", 13, |w| w.k.tasks[CLIENT].parent = NOBODY as u8),
    ("an endpoint names a waiter that does not wait", 10, |w| w.k.eps[SHARED].waiter = 0),
    ("a task waits on an endpoint that does not know of it", 10, |w| w.k.eps[OWN].waiter = NONE),
    ("a sender's badge is on none of its capabilities", 20, |w| w.k.tasks[CLIENT].send_badge = 0x99),
    ("a client waits for the answer of a task that does not exist", 12, |w| {
        w.k.tasks[CLIENT].state = State::Reply(NOBODY as u8)
    }),
    ("an endpoint's owner does not exist", 21, |w| w.k.eps[OWN].owner = NOBODY as u8),
    ("an interrupt stays bound without its capability", 20, |w| w.k.tasks[0].caps[3] = Cap::Null),
    ("an interrupt is bound to a task that does not exist", 16, |w| w.k.irqs[1].task = NOBODY as u8),
    ("an interrupt is bound to an endpoint that nobody can receive from", 16, |w| {
        w.k.irqs[1].ep = (MAX_ENDPOINTS - 1) as u8
    }),
    ("a waiting task's message buffer is not mapped", 24, |w| w.k.tasks[SERVER].msg_ptr = 0x7000_0000),
    ("the kernel has counted an anomaly", 17, |w| w.k.stats.anomalies = 1),
    ("a task holds a capability for the kernel's own memory", 26, |w| {
        forge(w, Cap::Memory { page: 0x100, pages: 16, write: true })
    }),
    ("a task holds a capability for the timer interrupt", 26, |w| forge(w, Cap::Irq { mask: 1 })),
    ("a task has no share of the processor at all", 27, |w| w.k.tasks[CLIENT].cpu_limit = 0),
    ("a task has a share of 101%", 27, |w| w.k.tasks[CLIENT].cpu_limit = 101),
    ("a task has a larger share than the task that started it", 27, |w| w.k.tasks[0].cpu_limit = 50),
];

/// Forged capabilities that look exactly like ones a task may come to hold honestly: the
/// kernel keeps no record that could tell them apart.
const UNSEEN: &[Seed] = &[
    ("a forged capability for every I/O port", 0, |w| forge(w, Cap::IoPort { first: 0, last: 0xFFFF })),
    ("a forged capability to write to the log", 0, |w| forge(w, Cap::Log)),
    ("a forged handle for a task that is not the holder's child", 0, |w| {
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
    for (name, expected, plant) in SEEDS {
        crate::trail(format!("tables: planted corruption: {name}"));
        plant(&mut w);
        let rule = w.k.check();
        w.restore_base();
        reported += (rule != 0) as usize;
        let seen = match rule {
            0 => "not noticed".to_string(),
            rule if rule == *expected => format!("rule {rule}"),
            rule => format!("rule {rule}, where rule {expected} describes it"),
        };
        tally.check(&format!("{name}: {seen}"), rule == *expected);
    }
    let mut unseen = 0;
    for (name, _, plant) in UNSEEN {
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
    edges(&mut tally);
    messages(&mut tally);
    larger_program(&mut tally);
    reports(&mut tally);
    shares(&mut tally);
    turns(&mut tally);
    derived_powers(&mut tally);
    refused_programs(&mut tally);
    counts(&mut tally);
    grant_taken_from_a_waiting_sender(&mut tally);
    device_memory_ends_where_it_ends(&mut tally);
    segments_off_the_first_page(&mut tally);
    checker_sensitivity(&mut tally);
    let total = tally.passed + tally.failed;
    println!("MODEL tables: {} {}/{total}", if tally.failed == 0 { "PASS" } else { "FAIL" }, tally.passed);
    tally.failed == 0
}
