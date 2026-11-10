//! System calls: the complete kernel interface.
//!
//! Every call either starts from a capability slot of the calling task or touches only the
//! caller's own memory. No call takes a global name (a process id, a path, a physical
//! address): what a task was not given, it cannot even mention.

use crate::arch::{self, TrapFrame};
use crate::elf::Image;
use crate::mem::{user_range, FOREIGN, RW, US};
use crate::task::{link, Cap, Endpoint, IrqBind, Kernel, State, Tcb, IRQ_LINES, NONE, ORIGINAL};
use crate::{get32, put32};
use abi::{
    nr, CAP_SLOTS, CAUSE_EXIT, CAUSE_KILLED, E_BUSY, E_DEAD, E_INVAL, E_NOCAP, E_NOMEM, E_NOSLOT,
    E_NOSYS, E_PERM, INFO_CAP, INFO_CHECK, INFO_STATS, INFO_TASK, MAP_WRITE, MAX_TASKS, MSG_DATA,
    NO_CAP, PAGE, R_GRANT, R_RECV, R_SEND, SPAWN_ARGS_SIZE, SPAWN_CAPS, STACK_TOP,
};

/// What a system call hands back: a value for EAX, or nothing because the caller now waits
/// (or is gone) and its EAX is written when it is woken.
pub enum Out {
    Value(u32),
    Blocked,
}

pub type Sys = Result<Out, i32>;

const DONE: Sys = Ok(Out::Value(0));

/// The interrupt controllers and the timer are driven by the kernel alone; no capability
/// reaches these ports.
const KERNEL_PORTS: [(u32, u32); 4] =
    [(0x20, 0x21), (0x40, 0x43), (0x61, 0x61), (0xA0, 0xA1)];

impl Kernel {
    /// The running task executed `int 0x80`.
    pub fn syscall(&mut self) {
        let me = self.cur;
        let frame = self.tasks[me].frame;
        let gen = self.tasks[me].gen;
        self.stats.syscalls = self.stats.syscalls.wrapping_add(1);
        self.tasks[me].syscalls = self.tasks[me].syscalls.wrapping_add(1);
        let result = self.dispatch(me, frame.eax, [frame.ebx, frame.ecx, frame.edx, frame.edi]);
        let value = match result {
            Ok(Out::Value(value)) => value,
            Ok(Out::Blocked) => return,
            Err(code) => code as u32,
        };
        // Write the result only if the caller is still the same task and still running.
        let task = &mut self.tasks[me];
        if task.gen == gen && task.state == State::Ready {
            task.frame.eax = value;
        }
    }

    fn dispatch(&mut self, me: usize, number: u32, [a, b, c, d]: [u32; 4]) -> Sys {
        match number {
            nr::YIELD => {
                self.preempt(me);
                DONE
            }
            nr::EXIT => {
                self.kill(me, [CAUSE_EXIT, a, 0, 0]);
                Ok(Out::Blocked)
            }
            nr::CALL => self.sys_call(me, a, b, c),
            nr::RECV => self.sys_recv(me, a, b, c),
            nr::REPLY => self.sys_reply(me, a, b),
            nr::EP_CREATE => self.sys_ep_create(me),
            nr::CAP_DERIVE => self.sys_cap_derive(me, a, b, c, d),
            nr::CAP_DROP => {
                let cap = self.cap(me, a)?;
                self.drop_cap(me, a as usize);
                self.release(me, cap);
                DONE
            }
            nr::CAP_REVOKE => self.sys_cap_revoke(me, a),
            nr::MAP => self.sys_map(me, a, b, c != 0),
            nr::UNMAP => {
                user_range(a, b)?;
                self.unmap_range(me, a, b);
                DONE
            }
            nr::MAP_MEM => self.sys_map_mem(me, a, b, c, d),
            nr::IO_IN => {
                let port = self.io_port(me, a, b, c)?;
                self.tasks[me].frame.edx = arch::port_in(port, c);
                DONE
            }
            nr::IO_OUT => {
                let port = self.io_port(me, a, b, c)?;
                arch::port_out(port, c, d);
                DONE
            }
            nr::IRQ_BIND => self.sys_irq_bind(me, a, b, c),
            nr::IRQ_ACK => {
                let line = self.irq_line(me, a, b)?;
                if self.irqs[line].task as usize != me {
                    return Err(E_PERM);
                }
                arch::irq_mask(b, false);
                DONE
            }
            nr::SPAWN => self.sys_spawn(me, a, b, c),
            nr::TASK_KILL => self.sys_task_kill(me, a),
            nr::SLEEP => self.sys_sleep(me, a),
            nr::TIME => {
                let now = self.micros();
                self.tasks[me].frame.edx = (now >> 32) as u32;
                Ok(Out::Value(now as u32))
            }
            nr::LOG => self.sys_log(me, a, b, c),
            nr::SYSINFO => self.sys_info(me, a, b, c, d),
            _ => Err(E_NOSYS),
        }
    }

    // ---------------------------------------------------------------- capabilities

    /// `ep_create`: a new endpoint, paid for from the caller's endpoint quota; the caller
    /// gets the capability with every right.
    fn sys_ep_create(&mut self, me: usize) -> Sys {
        let slot = self.free_slot(me)?;
        if self.tasks[me].ep_quota == 0 {
            return Err(E_NOSLOT);
        }
        let Some(ep) = self.eps.iter().position(|endpoint| endpoint.refs == 0) else {
            self.stats.anomalies += 1; // the endpoint quotas add up to the free slots
            return Err(E_NOSLOT);
        };
        self.tasks[me].ep_quota -= 1;
        self.eps[ep] = Endpoint { owner: me as u8, ..Endpoint::EMPTY };
        let rights = (R_SEND | R_RECV | R_GRANT) as u8;
        self.install(me, slot, Cap::Endpoint { ep: ep as u8, rights, badge: 0 }, ORIGINAL);
        Ok(Out::Value(slot as u32))
    }

    /// `cap_derive`: copy a capability into a free slot with equal or fewer powers.
    /// There is no way to make a capability stronger.
    fn sys_cap_derive(&mut self, me: usize, slot: u32, a: u32, b: u32, c: u32) -> Sys {
        let derived = match self.cap(me, slot)? {
            Cap::Endpoint { ep, rights, badge } => {
                if a == 0 || a & !(rights as u32) != 0 {
                    return Err(E_PERM);
                }
                // A badge is stamped once, by a holder of the grant right, and then sticks.
                let stamp = badge == 0 && rights as u32 & R_GRANT != 0;
                if b != 0 && b != badge && !stamp {
                    return Err(E_PERM);
                }
                let badge = if b == 0 { badge } else { b };
                Cap::Endpoint { ep, rights: a as u8, badge }
            }
            Cap::IoPort { first, last } => {
                if a > b || a < first as u32 || b > last as u32 {
                    return Err(E_PERM);
                }
                Cap::IoPort { first: a as u16, last: b as u16 }
            }
            Cap::Irq { mask } => {
                if a == 0 || a & !(mask as u32) != 0 {
                    return Err(E_PERM);
                }
                Cap::Irq { mask: a as u16 }
            }
            Cap::Memory { page, pages, write } => {
                let inside = b != 0 && a as u64 + b as u64 <= pages as u64;
                if !inside || (c != 0 && !write) {
                    return Err(E_PERM);
                }
                Cap::Memory { page: page + a, pages: b, write: c != 0 }
            }
            same => same,
        };
        let free = self.free_slot(me)?;
        self.install(me, free, derived, link(me, slot as usize));
        Ok(Out::Value(free as u32))
    }

    /// `cap_revoke`: take back every copy that was made of the capability in `slot`, and
    /// every copy of those copies, in whichever task they are now. The capability itself
    /// stays. What a holder had only through its copy (a place in a queue, an interrupt
    /// line, mapped device memory) is taken back with it.
    fn sys_cap_revoke(&mut self, me: usize, slot: u32) -> Sys {
        self.cap(me, slot)?;
        // The set of copies, one bit per slot. Each pass adds the copies of what is in the
        // set already; the set only grows, so the loop ends.
        let mut copies = [0u32; MAX_TASKS];
        copies[me] = 1 << slot;
        let mut grew = true;
        while grew {
            grew = false;
            for t in 0..MAX_TASKS {
                for s in 0..CAP_SLOTS {
                    let from = self.tasks[t].from[s] as usize;
                    let source = copies.get(from / CAP_SLOTS).is_some_and(|set| set & (1 << (from % CAP_SLOTS)) != 0);
                    if source && copies[t] & (1 << s) == 0 {
                        copies[t] |= 1 << s;
                        grew = true;
                    }
                }
            }
        }
        copies[me] &= !(1 << slot);
        for t in 0..MAX_TASKS {
            for s in (0..CAP_SLOTS).filter(|s| copies[t] & (1 << s) != 0) {
                let cap = self.tasks[t].caps[s];
                self.drop_cap(t, s);
                self.release(t, cap);
            }
        }
        DONE
    }

    // ---------------------------------------------------------------- memory

    /// `map`: add zeroed pages of the caller's own quota to its address space.
    fn sys_map(&mut self, me: usize, va: u32, pages: u32, writable: bool) -> Sys {
        user_range(va, pages)?;
        let flags = if writable { US | RW } else { US };
        for done in 0..pages {
            if let Err(code) = self.map_anon(me, va + done * PAGE, flags) {
                self.unmap_range(me, va, done); // all or nothing
                return Err(code);
            }
        }
        DONE
    }

    /// `map_mem`: map device or archive memory that a Memory capability names.
    fn sys_map_mem(&mut self, me: usize, slot: u32, va: u32, offset: u32, count: u32) -> Sys {
        let Cap::Memory { page, pages, write } = self.cap(me, slot)? else { return Err(E_NOCAP) };
        let (writable, count) = (count & MAP_WRITE != 0, count & !MAP_WRITE);
        if writable && !write {
            return Err(E_PERM);
        }
        user_range(va, count)?;
        if offset as u64 + count as u64 > pages as u64 {
            return Err(E_INVAL);
        }
        let flags = if writable { US | RW | FOREIGN } else { US | FOREIGN };
        for done in 0..count {
            let frame = (page + offset + done) * PAGE;
            if let Err(code) = self.map_page(me, va + done * PAGE, frame, flags) {
                self.unmap_range(me, va, done);
                return Err(code);
            }
        }
        DONE
    }

    // ---------------------------------------------------------------- devices

    /// Check a port access of `width` bytes against the I/O capability in `slot`.
    fn io_port(&self, me: usize, slot: u32, port: u32, width: u32) -> Result<u16, i32> {
        let Cap::IoPort { first, last } = self.cap(me, slot)? else { return Err(E_NOCAP) };
        if !matches!(width, 1 | 2 | 4) {
            return Err(E_INVAL);
        }
        let end = port as u64 + width as u64 - 1;
        let reserved = KERNEL_PORTS.iter().any(|&(low, high)| port <= high && end >= low as u64);
        if port < first as u32 || end > last as u64 || reserved {
            return Err(E_PERM);
        }
        Ok(port as u16)
    }

    /// Check that the IRQ capability in `slot` covers interrupt line `irq`.
    fn irq_line(&self, me: usize, slot: u32, irq: u32) -> Result<usize, i32> {
        let Cap::Irq { mask } = self.cap(me, slot)? else { return Err(E_NOCAP) };
        if irq >= IRQ_LINES as u32 {
            return Err(E_INVAL);
        }
        if mask as u32 & (1 << irq) == 0 {
            return Err(E_PERM);
        }
        Ok(irq as usize)
    }

    /// `irq_bind`: deliver an interrupt line as messages on one of the caller's endpoints.
    fn sys_irq_bind(&mut self, me: usize, slot: u32, irq: u32, ep_slot: u32) -> Sys {
        let line = self.irq_line(me, slot, irq)?;
        let (ep, _, _) = self.endpoint(me, ep_slot, R_RECV)?;
        let owner = self.irqs[line].task;
        if owner != NONE && owner as usize != me {
            return Err(E_BUSY);
        }
        self.irq_unbind(line);
        self.irqs[line] = IrqBind { task: me as u8, ep: ep as u8 };
        arch::irq_mask(irq, false);
        DONE
    }

    // ---------------------------------------------------------------- tasks

    /// `spawn`: start a program from an ELF image in the caller's memory. The child gets
    /// copies of the capabilities the caller lists. Its memory, its place in the task table
    /// and any quota for tasks and endpoints of its own come out of the caller's quotas.
    fn sys_spawn(&mut self, me: usize, image: u32, len: u32, args: u32) -> Sys {
        let pd = self.tasks[me].pd;
        let mut raw = [0; SPAWN_ARGS_SIZE];
        self.mem.copy_in(pd, args, &mut raw)?;
        // Words of `abi::SpawnArgs` after the name: memory quota, stack, fault endpoint,
        // fault badge, task quota, endpoint quota, 16 capability slots, 4 start arguments.
        let word = |index: usize| get32(&raw, 12 + index * 4);
        let (quota, stack_pages, fault_slot, fault_badge) = (word(0), word(1), word(2), word(3));
        let (tasks, endpoints) = (word(4), word(5));
        let fault_ep = match fault_slot {
            NO_CAP => NONE,
            slot => self.endpoint(me, slot, R_RECV)?.0 as u8,
        };
        let mut caps = [(Cap::Null, ORIGINAL); SPAWN_CAPS];
        for (index, cap) in caps.iter_mut().enumerate() {
            let slot = word(6 + index);
            if slot != NO_CAP {
                *cap = (self.cap(me, slot)?, link(me, slot as usize));
            }
        }
        let handle = self.free_slot(me)?;
        let caller = &self.tasks[me];
        // One slot of the task table for the child itself, plus what it may use in turn.
        if tasks >= caller.task_quota as u32 || endpoints > caller.ep_quota as u32 {
            return Err(E_NOSLOT);
        }
        if quota > caller.quota - caller.charged {
            return Err(E_NOMEM);
        }
        let Some(child) = self.tasks.iter().position(|task| task.state == State::Free) else {
            self.stats.anomalies += 1; // the task quotas add up to the free slots
            return Err(E_NOSLOT);
        };

        self.tasks[me].quota -= quota;
        let mut name = [0; 12];
        name.copy_from_slice(&raw[..12]);
        let gen = self.tasks[child].gen;
        let parent = me as u8;
        self.tasks[child] = Tcb { gen, state: State::Zombie(NONE), parent, quota, name, ..Tcb::EMPTY };
        let entry = match self.load(child, &Image::User { pd, ptr: image, len }, stack_pages) {
            Ok(entry) => entry,
            Err(code) => {
                self.destroy_space(child);
                self.tasks[child].quota = 0;
                self.tasks[child].state = State::Free;
                self.tasks[me].quota += quota;
                return Err(code);
            }
        };

        let caller = &mut self.tasks[me];
        caller.task_quota -= 1 + tasks as u8;
        caller.ep_quota -= endpoints as u8;
        for (slot, (cap, from)) in caps.into_iter().enumerate() {
            if cap != Cap::Null {
                self.install(child, slot, cap, from);
            }
        }
        if fault_ep != NONE {
            self.eps[fault_ep as usize].refs += 1;
        }
        let start = [word(22), word(23), word(24), word(25)];
        let task = &mut self.tasks[child];
        task.task_quota = tasks as u8;
        task.ep_quota = endpoints as u8;
        task.fault_ep = fault_ep;
        task.fault_badge = fault_badge;
        task.frame = TrapFrame::user(entry, STACK_TOP, start);
        self.ready(child);
        self.preempt(child); // a new task waits its turn
        self.install(me, handle, Cap::Task { index: child as u8, gen }, ORIGINAL);
        self.stats.spawns = self.stats.spawns.wrapping_add(1);
        Ok(Out::Value(handle as u32))
    }

    /// `task_kill`: end the task a Task capability names, with everything it started.
    fn sys_task_kill(&mut self, me: usize, slot: u32) -> Sys {
        let Cap::Task { index, gen } = self.cap(me, slot)? else { return Err(E_NOCAP) };
        let victim = &self.tasks[index as usize];
        if victim.gen != gen || !victim.alive() {
            return Err(E_DEAD);
        }
        self.kill(index as usize, [CAUSE_KILLED, 0, 0, 0]);
        DONE
    }

    fn sys_sleep(&mut self, me: usize, ms: u32) -> Sys {
        if ms == 0 {
            self.preempt(me);
            return DONE;
        }
        self.tasks[me].state = State::Sleep;
        self.tasks[me].deadline = self.deadline(ms);
        Ok(Out::Blocked)
    }

    // ---------------------------------------------------------------- log and inspection

    /// `log`: write text to the kernel's serial log.
    fn sys_log(&mut self, me: usize, slot: u32, ptr: u32, len: u32) -> Sys {
        if self.cap(me, slot)? != Cap::Log {
            return Err(E_NOCAP);
        }
        let mut text = [0; MSG_DATA];
        let text = text.get_mut(..len as usize).ok_or(E_INVAL)?;
        self.mem.copy_in(self.tasks[me].pd, ptr, text)?;
        for &byte in text.iter() {
            if byte == b'\n' {
                arch::serial_put(b'\r');
            }
            arch::serial_put(byte);
        }
        DONE
    }

    /// `sysinfo`: read-only view of kernel state (`abi::KStats`, `TaskInfo`, `CapInfo`), or
    /// a run of the invariant checker.
    fn sys_info(&mut self, me: usize, slot: u32, what: u32, arg: u32, ptr: u32) -> Sys {
        if self.cap(me, slot)? != Cap::Info {
            return Err(E_NOCAP);
        }
        let mut out = [0; 52];
        let len = match what {
            INFO_STATS => {
                let tasks = self.tasks.iter().filter(|task| task.state != State::Free).count();
                let endpoints = self.eps.iter().filter(|endpoint| endpoint.refs != 0).count();
                let words = [
                    self.ticks as u32,
                    self.mem.total,
                    self.mem.free,
                    tasks as u32,
                    endpoints as u32,
                    self.stats.syscalls,
                    self.stats.switches,
                    self.stats.irqs,
                    self.stats.faults,
                    self.stats.spawns,
                    self.stats.anomalies,
                    self.mem.kernel_pages,
                    (self.mem.nx != 0) as u32,
                ];
                for (index, word) in words.into_iter().enumerate() {
                    put32(&mut out, index * 4, word);
                }
                52
            }
            INFO_TASK => {
                let task = self.tasks.get(arg as usize).ok_or(E_INVAL)?;
                let caps = task.caps.iter().filter(|cap| **cap != Cap::Null).count();
                let words = [
                    task.state.code(),
                    task.parent as u32,
                    task.gen as u32,
                    task.quota,
                    task.charged,
                    task.cpu_ticks,
                    task.syscalls,
                    caps as u32,
                    task.task_quota as u32,
                    task.ep_quota as u32,
                ];
                out[..12].copy_from_slice(&task.name);
                for (index, word) in words.into_iter().enumerate() {
                    put32(&mut out, 12 + index * 4, word);
                }
                52
            }
            INFO_CAP => {
                let task = self.tasks.get((arg >> 8) as usize).ok_or(E_INVAL)?;
                let slot = (arg & 0xFF) as usize;
                let cap = task.caps.get(slot).ok_or(E_INVAL)?;
                let (from, slots) = (task.from[slot] as u32, CAP_SLOTS as u32);
                let source = if from == ORIGINAL as u32 { NO_CAP } else { (from / slots) << 8 | from % slots };
                for (index, word) in cap.describe().into_iter().chain([source]).enumerate() {
                    put32(&mut out, index * 4, word);
                }
                20
            }
            INFO_CHECK => return Ok(Out::Value(self.check())),
            _ => return Err(E_INVAL),
        };
        self.mem.copy_out(self.tasks[me].pd, ptr, &out[..len])?;
        DONE
    }
}
