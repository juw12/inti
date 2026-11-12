//! What counts as "the same state".
//!
//! The explorer must recognize a state it has seen before, or it would walk in circles.
//! Two kernels are treated as the same when the summary written here is the same. The
//! summary keeps everything that decides what the kernel can do next - who waits for whom,
//! every capability and where it was copied from, every quota, every page mapping - and
//! leaves out what only names things: frame numbers, token values, generation counters,
//! the clock, statistics, saved registers. A task's share of the processor is kept; what it
//! has used of the share in the current window is left out like the clock, and so is the
//! order among ready tasks: the explorer lets any ready task take the next step.
//!
//! The explorer stores a 128-bit hash of the summary, not the summary. Two different states
//! with the same hash would be taken for one, and the second would not be explored further.

use crate::arch;
use crate::mem::{ACCESSED, ADDR, DIRTY, ENTRIES, FOREIGN, P, USER_TABLES};
use crate::task::{Cap, Kernel, State, NONE};
use crate::world::World;
use abi::{CAP_SLOTS, MSG_HEADER};
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;

/// A deadline is one of three kinds. Its exact value is not part of the state.
pub const NEVER: u8 = 0;
/// Further away than any run of the model lasts: it ends when the explorer says so.
pub const FAR: u8 = 1;
/// Within seconds. Only the restart delay of the root task is of this kind.
pub const NEAR: u8 = 2;

pub fn deadline_kind(deadline: u64, now: u64) -> u8 {
    if deadline == u64::MAX {
        NEVER
    } else if deadline > now + 3_000_000 {
        FAR
    } else {
        NEAR
    }
}

fn put(out: &mut Vec<u8>, words: &[u32]) {
    for word in words {
        out.extend_from_slice(&word.to_le_bytes());
    }
}

/// Write the summary of the kernel's state to `out`.
pub fn canonical(world: &World, out: &mut Vec<u8>) {
    out.clear();
    let k: &Kernel = &world.k;
    let now = arch::with(|machine| machine.micros());
    for (t, task) in k.tasks.iter().enumerate() {
        let (code, operand) = match task.state {
            State::Free => {
                out.push(0);
                continue;
            }
            State::Ready => (1, NONE),
            State::Recv(ep) => (2, ep),
            State::Send(ep) => (3, ep),
            State::Reply(server) => (4, server),
            State::Sleep => (5, NONE),
            State::Zombie(ep) => (6, ep),
        };
        out.extend_from_slice(&[code, operand, task.parent, task.fault_ep, task.task_quota, task.ep_quota]);
        out.push(task.cpu_limit);
        put(out, &[task.fault_badge, task.quota, task.charged, task.foreign]);
        if matches!(task.state, State::Recv(_) | State::Send(_) | State::Reply(_) | State::Sleep) {
            out.push(deadline_kind(task.deadline, now));
        }
        if matches!(task.state, State::Recv(_) | State::Send(_) | State::Reply(_)) {
            out.push(task.grant as u8);
            put(out, &[task.msg_ptr]);
        }
        if let State::Send(ep) | State::Zombie(ep) = task.state {
            // The place in the queue, not the sequence number that orders it.
            let queued = |other: &&crate::task::Tcb| other.state == State::Send(ep) || other.state == State::Zombie(ep);
            let ahead = k.tasks.iter().filter(queued).filter(|other| other.seq < task.seq).count();
            out.push(ahead as u8);
        }
        if let State::Send(_) = task.state {
            // The message that will be delivered: the kernel reads it when a receiver comes.
            put(out, &[task.send_badge]);
            let mut header = [0; MSG_HEADER];
            let readable = world.peek(t, task.msg_ptr, &mut header);
            out.push(readable as u8);
            out.extend_from_slice(&header);
        }
        for slot in 0..CAP_SLOTS {
            capability(k, task.caps[slot], task.from[slot], out);
        }
        if task.alive() {
            address_space(k, task.pd, out);
        }
    }
    for endpoint in &k.eps {
        if endpoint.refs == 0 {
            out.push(0);
            continue;
        }
        out.push(1);
        out.extend_from_slice(&endpoint.refs.to_le_bytes());
        out.extend_from_slice(&endpoint.recv_refs.to_le_bytes());
        out.extend_from_slice(&endpoint.irq_pending.to_le_bytes());
        out.extend_from_slice(&[endpoint.waiter, endpoint.owner]);
    }
    for bind in &k.irqs {
        out.extend_from_slice(&if bind.task == NONE { [NONE, NONE] } else { [bind.task, bind.ep] });
    }
    out.extend_from_slice(&arch::with(|machine| machine.cpu.masked).to_le_bytes());
    put(out, &[k.mem.free]);
}

fn capability(k: &Kernel, cap: Cap, from: u16, out: &mut Vec<u8>) {
    match cap {
        Cap::Null => {
            out.push(0);
            return;
        }
        Cap::Endpoint { ep, rights, badge } => {
            out.extend_from_slice(&[1, ep, rights]);
            put(out, &[badge]);
        }
        Cap::IoPort { first, last } => {
            out.push(2);
            put(out, &[first as u32, last as u32]);
        }
        Cap::Irq { mask } => {
            out.push(3);
            put(out, &[mask as u32]);
        }
        Cap::Memory { page, pages, write } => {
            out.push(4);
            put(out, &[page, pages, write as u32]);
        }
        Cap::Task { index, gen } => {
            // A handle either still names the task it was made for, or it is stale.
            let fresh = k.tasks[index as usize].gen == gen;
            out.extend_from_slice(&[5, index, fresh as u8]);
        }
        Cap::Log => out.push(6),
        Cap::Info => out.push(7),
    }
    out.extend_from_slice(&from.to_le_bytes());
}

/// Every user page of the address space `pd`: where it is and how it is mapped. Which frame
/// backs it is left out, except for device memory, where the frame is the point.
fn address_space(k: &Kernel, pd: u32, out: &mut Vec<u8>) {
    for number in 0..USER_TABLES as usize {
        let Some(table) = k.mem.user_table(pd, number) else { continue };
        out.extend_from_slice(&(number as u16).to_le_bytes());
        for index in 0..ENTRIES {
            let entry = k.mem.read_entry(table, index);
            if entry & P == 0 {
                continue;
            }
            out.extend_from_slice(&(index as u16).to_le_bytes());
            out.extend_from_slice(&(entry & !(ADDR | ACCESSED | DIRTY)).to_le_bytes());
            if entry & FOREIGN != 0 {
                put(out, &[(entry & ADDR) as u32]);
            }
        }
        out.extend_from_slice(&[0xFF, 0xFF]);
    }
}

/// A 128-bit hash of a summary: SipHash twice, with two different first bytes.
pub fn fingerprint(summary: &[u8]) -> u128 {
    let half = |prefix: u8| {
        let mut hasher = DefaultHasher::new();
        hasher.write_u8(prefix);
        hasher.write(summary);
        hasher.finish() as u128
    };
    half(0x51) << 64 | half(0xA7)
}
