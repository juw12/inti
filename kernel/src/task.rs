//! Tasks, capabilities, scheduling and death.
//!
//! All kernel state is one `Kernel` value made of fixed-size tables: 32 tasks, 64 endpoints,
//! 16 interrupt lines. Nothing is allocated at run time, so nothing can leak, and every loop
//! over kernel state has a bound known at compile time.

use crate::arch::{self, TrapFrame, CLOCK_HZ, IRQ_BASE, SYSCALL_VECTOR};
use crate::elf::Image;
use crate::mem::{page_up, Mem};
use abi::{
    CAP_ENDPOINT, CAP_INFO, CAP_IOPORT, CAP_IRQ, CAP_LOG, CAP_MEMORY, CAP_NULL, CAP_SLOTS,
    CAP_TASK, CAUSE_PARENT_DIED, E_DEAD, E_NOCAP, E_NOSLOT, E_TIMEOUT, FOREVER, MAX_ENDPOINTS,
    MAX_TASKS, NO_TASK, PAGE, R_GRANT, R_RECV, R_SEND, STACK_TOP, ST_FREE, ST_READY, ST_RECV,
    ST_REPLY, ST_SEND, ST_SLEEP, ST_ZOMBIE,
};

/// "No task" / "no endpoint" in the one-byte links between kernel objects.
pub const NONE: u8 = 0xFF;
/// Value of `Kernel::cur` while no task is running.
pub const IDLE: usize = usize::MAX;
pub const IRQ_LINES: usize = 16;
const ROOT_STACK_PAGES: u32 = 8;
/// User space is restarted at most once per this many microseconds.
const ROOT_MIN_US: u64 = 1_000_000;
/// `Tcb::from` of a capability that is not a copy of another one.
pub const ORIGINAL: u16 = 0xFFFF;

// Reply tokens carry a task index in 5 bits and `kill` keeps a set of tasks in a `u32`.
const _: () = assert!(MAX_TASKS == 32 && MAX_ENDPOINTS < NONE as usize && NO_TASK == NONE as u32);
// `cap_revoke` keeps a set of capability slots in a `u32`; a link to a slot fits in a `u16`.
const _: () = assert!(CAP_SLOTS == 32 && MAX_TASKS * CAP_SLOTS < ORIGINAL as usize);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    Free,
    Ready,
    /// Waiting for a message on this endpoint.
    Recv(u8),
    /// Queued on this endpoint until its server receives.
    Send(u8),
    /// Message delivered; waiting for this server task to reply.
    Reply(u8),
    Sleep,
    /// Ended. Its fault message waits on this endpoint (`NONE` while the task is being
    /// built or torn down inside one kernel entry).
    Zombie(u8),
}

impl State {
    pub fn code(self) -> u32 {
        match self {
            State::Free => ST_FREE,
            State::Ready => ST_READY,
            State::Recv(_) => ST_RECV,
            State::Send(_) => ST_SEND,
            State::Reply(_) => ST_REPLY,
            State::Sleep => ST_SLEEP,
            State::Zombie(_) => ST_ZOMBIE,
        }
    }
}

/// A capability: the only way a task can name anything outside its own memory.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Cap {
    Null,
    Endpoint { ep: u8, rights: u8, badge: u32 },
    IoPort { first: u16, last: u16 },
    Irq { mask: u16 },
    /// Physical page frames `page .. page + pages` that are not ordinary RAM of any task.
    Memory { page: u32, pages: u32, write: bool },
    Task { index: u8, gen: u64 },
    Log,
    Info,
}

impl Cap {
    /// The `CapInfo` words: kind, a, b, c.
    pub fn describe(self) -> [u32; 4] {
        match self {
            Cap::Null => [CAP_NULL, 0, 0, 0],
            Cap::Endpoint { ep, rights, badge } => [CAP_ENDPOINT, ep as u32, rights as u32, badge],
            Cap::IoPort { first, last } => [CAP_IOPORT, first as u32, last as u32, 0],
            Cap::Irq { mask } => [CAP_IRQ, mask as u32, 0, 0],
            Cap::Memory { page, pages, write } => [CAP_MEMORY, page * PAGE, pages, write as u32],
            Cap::Task { index, gen } => [CAP_TASK, index as u32, gen as u32, 0],
            Cap::Log => [CAP_LOG, 0, 0, 0],
            Cap::Info => [CAP_INFO, 0, 0, 0],
        }
    }
}

/// The value of `Tcb::from` that names capability `slot` of task `t`.
pub fn link(t: usize, slot: usize) -> u16 {
    (t * CAP_SLOTS + slot) as u16
}

/// Everything the kernel knows about one task.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Tcb {
    /// Saved user registers. Must stay the first field: the CPU pushes straight into it.
    pub frame: TrapFrame,
    pub state: State,
    /// Incremented when the slot is recycled, so that old Task capabilities go stale.
    pub gen: u64,
    pub parent: u8,
    /// Endpoint that receives the `L_FAULT` message when this task ends.
    pub fault_ep: u8,
    pub name: [u8; 12],
    /// Root table of its address space (physical address), 0 if the task has none.
    pub pd: u32,
    /// Memory limit and current use, in page frames.
    pub quota: u32,
    pub charged: u32,
    pub caps: [Cap; CAP_SLOTS],
    /// For every capability, the one it was copied from (see `link`), or `ORIGINAL`.
    /// `cap_revoke` follows these links to find all copies of a capability.
    pub from: [u16; CAP_SLOTS],
    /// How many more tasks it may start, and how many more endpoints it may create.
    pub task_quota: u8,
    pub ep_quota: u8,
    /// Pages of device memory in its address space, mapped through Memory capabilities.
    pub foreign: u32,
    /// Scheduling order: the ready task with the highest rank runs.
    pub rank: i64,
    /// Tick at which a blocked task gives up (`u64::MAX` = never).
    pub deadline: u64,
    /// Arrival order on an endpoint queue.
    pub seq: u64,
    /// User address of the message buffer of the blocking call in progress.
    pub msg_ptr: u32,
    pub token: u32,
    pub send_badge: u32,
    /// While it waits in `call` or `recv`: its capability for the endpoint carries the
    /// grant right. Once its call is delivered: so did the receiver's.
    pub grant: bool,
    pub fault_badge: u32,
    /// Why the task ended: cause, address, eip, error code.
    pub fault: [u32; 4],
    pub cpu_ticks: u32,
    pub syscalls: u32,
}

impl Tcb {
    pub const EMPTY: Tcb = Tcb {
        frame: TrapFrame::ZERO,
        state: State::Free,
        gen: 0,
        parent: NONE,
        fault_ep: NONE,
        name: [0; 12],
        pd: 0,
        quota: 0,
        charged: 0,
        caps: [Cap::Null; CAP_SLOTS],
        from: [ORIGINAL; CAP_SLOTS],
        task_quota: 0,
        ep_quota: 0,
        foreign: 0,
        rank: 0,
        deadline: u64::MAX,
        seq: 0,
        msg_ptr: 0,
        token: 0,
        send_badge: 0,
        grant: false,
        fault_badge: 0,
        fault: [0; 4],
        cpu_ticks: 0,
        syscalls: 0,
    };

    /// True for a task that runs or can run again (not free, not ended).
    pub fn alive(&self) -> bool {
        !matches!(self.state, State::Free | State::Zombie(_))
    }

    /// Does it hold one capability for endpoint `ep` that carries every right in `rights`
    /// and, if asked, `badge`?
    pub fn reaches(&self, ep: u8, rights: u32, badge: Option<u32>) -> bool {
        self.caps.iter().any(|cap| match *cap {
            Cap::Endpoint { ep: to, rights: held, badge: stamp } => {
                to == ep && held as u32 & rights == rights && badge.is_none_or(|wanted| wanted == stamp)
            }
            _ => false,
        })
    }

    /// Does it hold an Irq capability for interrupt line `irq`?
    pub fn has_irq(&self, irq: usize) -> bool {
        self.caps.iter().any(|cap| matches!(*cap, Cap::Irq { mask } if mask & (1 << irq) != 0))
    }

    /// Does it hold a Memory capability for the page frame at `address` (a writable one,
    /// if `write` is set)?
    pub fn has_memory(&self, address: u32, write: bool) -> bool {
        let frame = address / PAGE;
        self.caps.iter().any(|cap| match *cap {
            Cap::Memory { page, pages, write: writable } => {
                frame >= page && frame - page < pages && (writable || !write)
            }
            _ => false,
        })
    }
}

/// A message rendezvous point. Senders queue; one task at a time receives.
#[derive(Clone, Copy)]
pub struct Endpoint {
    /// Capabilities and fault registrations that name this endpoint. 0 = the slot is free.
    pub refs: u16,
    /// How many of those capabilities carry the receive right.
    pub recv_refs: u16,
    /// The task blocked in `recv` here, or `NONE`.
    pub waiter: u8,
    /// Interrupt lines that fired and were not delivered yet.
    pub irq_pending: u16,
    /// The task whose endpoint quota pays for this endpoint.
    pub owner: u8,
}

impl Endpoint {
    pub const EMPTY: Endpoint =
        Endpoint { refs: 0, recv_refs: 0, waiter: NONE, irq_pending: 0, owner: NONE };
}

/// Which task receives an interrupt line, and on which endpoint.
#[derive(Clone, Copy)]
pub struct IrqBind {
    pub task: u8,
    pub ep: u8,
}

impl IrqBind {
    pub const UNBOUND: IrqBind = IrqBind { task: NONE, ep: NONE };
}

pub struct Stats {
    pub syscalls: u32,
    pub switches: u32,
    pub irqs: u32,
    pub faults: u32,
    pub spawns: u32,
    /// "Cannot happen" conditions that were handled instead of ignored. Must stay 0.
    pub anomalies: u32,
}

pub struct Kernel {
    pub mem: Mem,
    pub tasks: [Tcb; MAX_TASKS],
    pub eps: [Endpoint; MAX_ENDPOINTS],
    pub irqs: [IrqBind; IRQ_LINES],
    /// Index of the running task, or `IDLE`.
    pub cur: usize,
    /// The page directory loaded in the CPU.
    pub space: u32,
    /// Set when the ready set changed and the scheduler must look again.
    resched: bool,
    /// Timer interrupts handled. Statistics only: time comes from `now`.
    pub ticks: u64,
    rank_high: i64,
    rank_low: i64,
    pub seq: u64,
    pub token_counter: u32,
    pub stats: Stats,
    /// All managed memory; the quota of the root task.
    pub root_quota: u32,
    /// Earliest time (microseconds) at which user space may be started again.
    root_not_before: u64,
    root_starts: u32,
    /// Address and length of the boot archive.
    archive: (u32, u32),
    /// Clock counts since boot, and the counter reading they were last brought up to.
    clock: u64,
    clock_last: u32,
    /// Work area of the invariant checker: one bit per page frame.
    pub scratch: [u32; 1024],
}

impl Kernel {
    pub const fn new() -> Kernel {
        Kernel {
            mem: Mem::new(),
            tasks: [Tcb::EMPTY; MAX_TASKS],
            eps: [Endpoint::EMPTY; MAX_ENDPOINTS],
            irqs: [IrqBind::UNBOUND; IRQ_LINES],
            cur: IDLE,
            space: 0,
            resched: true,
            ticks: 0,
            rank_high: 0,
            rank_low: 0,
            seq: 0,
            token_counter: 0,
            stats: Stats { syscalls: 0, switches: 0, irqs: 0, faults: 0, spawns: 0, anomalies: 0 },
            root_quota: 0,
            root_not_before: 0,
            root_starts: 0,
            archive: (0, 0),
            clock: 0,
            clock_last: 0,
            scratch: [0; 1024],
        }
    }

    /// Take over the machine: memory, the clock, and the first user task.
    pub fn boot(&mut self, e820: u32, archive: u32, archive_len: u32) {
        let kernel_end = page_up(archive + archive_len);
        assert!(archive % PAGE == 0 && kernel_end <= 16 << 20, "bad boot archive location");
        self.archive = (archive, archive_len);
        self.mem.init(e820, kernel_end);
        self.space = self.mem.kernel_pd;
        self.root_quota = self.mem.total;
        self.clock_last = arch::clock_count();
        kprintln!(
            "kernel: {} KiB kernel and archive, {} MiB for user space",
            self.mem.kernel_pages * 4,
            self.mem.total / 256
        );
        if self.mem.nx != 0 {
            kprintln!("kernel: PAE paging, W^X enforced (the processor has the no-execute bit)");
        } else {
            kprintln!("kernel: PAE paging, W^X NOT enforced (this processor has no no-execute bit)");
        }
        self.start_root();
    }

    /// Microseconds since boot.
    ///
    /// The 16-bit clock counter wraps every 54.9 ms, so this must run more often than that;
    /// the 10 ms timer interrupt calls it. If the machine stalls for longer, whole wraps go
    /// unseen and the clock falls behind: it can run slow, never fast, so no timeout fires
    /// early.
    pub fn micros(&mut self) -> u64 {
        let count = arch::clock_count();
        self.clock += (self.clock_last.wrapping_sub(count) & 0xFFFF) as u64;
        self.clock_last = count;
        self.clock / CLOCK_HZ * 1_000_000 + self.clock % CLOCK_HZ * 1_000_000 / CLOCK_HZ
    }

    // ---------------------------------------------------------------- scheduling

    /// Choose the task to run and return its saved registers; `None` if nothing is ready.
    pub fn schedule(&mut self) -> Option<*const TrapFrame> {
        let stay = self.cur != IDLE && !self.resched && self.tasks[self.cur].state == State::Ready;
        if !stay {
            self.resched = false;
            let mut best = IDLE;
            for (index, task) in self.tasks.iter().enumerate() {
                let better = best == IDLE || task.rank > self.tasks[best].rank;
                if task.state == State::Ready && better {
                    best = index;
                }
            }
            if best != self.cur && best != IDLE {
                self.stats.switches = self.stats.switches.wrapping_add(1);
            }
            self.cur = best;
            if best == IDLE {
                return None;
            }
        }
        let task = &self.tasks[self.cur];
        if task.pd != self.space {
            self.space = task.pd;
            arch::write_cr3(task.pd);
        }
        let frame = &raw const task.frame;
        arch::set_trap_stack(frame);
        Some(frame)
    }

    /// Make task `t` runnable ahead of everything else that is ready, so that a task woken
    /// by a message or an interrupt answers at once.
    pub fn ready(&mut self, t: usize) {
        self.rank_high += 1;
        let task = &mut self.tasks[t];
        task.state = State::Ready;
        task.deadline = u64::MAX;
        task.rank = self.rank_high;
        self.resched = true;
    }

    /// Finish the blocking system call of task `t` with `result` and make it runnable.
    pub fn wake(&mut self, t: usize, result: i32) {
        self.tasks[t].frame.eax = result as u32;
        self.ready(t);
    }

    /// Send task `t` to the back of the ready queue.
    pub fn preempt(&mut self, t: usize) {
        self.rank_low -= 1;
        self.tasks[t].rank = self.rank_low;
        self.resched = true;
    }

    /// The time in microseconds at which a timeout of `ms` milliseconds, starting now, expires.
    pub fn deadline(&mut self, ms: u32) -> u64 {
        if ms == FOREVER {
            return u64::MAX;
        }
        self.micros() + ms as u64 * 1000
    }

    /// The 100 Hz timer: bring the clock up to date, expire timeouts, and move on to the
    /// next ready task. A waiter wakes at the first timer interrupt after its deadline.
    pub fn tick(&mut self) {
        self.ticks += 1;
        let now = self.micros();
        for t in 0..MAX_TASKS {
            if self.tasks[t].deadline > now {
                continue;
            }
            match self.tasks[t].state {
                State::Sleep => self.wake(t, 0),
                State::Recv(ep) => {
                    self.eps[ep as usize].waiter = NONE;
                    self.wake(t, E_TIMEOUT);
                }
                State::Send(_) | State::Reply(_) => self.wake(t, E_TIMEOUT),
                State::Free | State::Ready | State::Zombie(_) => {
                    self.tasks[t].deadline = u64::MAX;
                    continue;
                }
            }
            // A timer wakeup queues at the back: sleeping is not a way to jump the queue.
            self.preempt(t);
        }
        if self.cur != IDLE && self.tasks[self.cur].state == State::Ready {
            let task = &mut self.tasks[self.cur];
            task.cpu_ticks = task.cpu_ticks.wrapping_add(1);
            self.preempt(self.cur);
        }
    }

    // ---------------------------------------------------------------- capabilities

    /// The capability in `slot` of task `t`.
    pub fn cap(&self, t: usize, slot: u32) -> Result<Cap, i32> {
        match self.tasks[t].caps.get(slot as usize) {
            Some(&cap) if cap != Cap::Null => Ok(cap),
            _ => Err(E_NOCAP),
        }
    }

    pub fn free_slot(&self, t: usize) -> Result<usize, i32> {
        self.tasks[t].caps.iter().position(|cap| *cap == Cap::Null).ok_or(E_NOSLOT)
    }

    /// Put `cap` into the empty `slot` of task `t`. `from` is the link to the capability it
    /// is a copy of, or `ORIGINAL`.
    pub fn install(&mut self, t: usize, slot: usize, cap: Cap, from: u16) {
        if let Cap::Endpoint { ep, rights, .. } = cap {
            let endpoint = &mut self.eps[ep as usize];
            endpoint.refs += 1;
            if rights as u32 & R_RECV != 0 {
                endpoint.recv_refs += 1;
            }
        }
        self.tasks[t].caps[slot] = cap;
        self.tasks[t].from[slot] = from;
    }

    /// Empty `slot` of task `t`. Copies made of the capability live on; they now count as
    /// copies of whatever it was copied from itself. When the last receive right of an
    /// endpoint goes away, everything that waits on that endpoint is released.
    pub fn drop_cap(&mut self, t: usize, slot: usize) {
        let cap = core::mem::replace(&mut self.tasks[t].caps[slot], Cap::Null);
        if cap == Cap::Null {
            return;
        }
        let (here, source) = (link(t, slot), self.tasks[t].from[slot]);
        self.tasks[t].from[slot] = ORIGINAL;
        for from in self.tasks.iter_mut().flat_map(|task| task.from.iter_mut()) {
            if *from == here {
                *from = source;
            }
        }
        if let Cap::Endpoint { ep, rights, .. } = cap {
            let ep = ep as usize;
            if rights as u32 & R_RECV != 0 {
                self.eps[ep].recv_refs -= 1;
                if self.eps[ep].recv_refs == 0 {
                    self.orphan(ep);
                }
            }
            self.ep_unref(ep);
        }
    }

    /// One reference to endpoint `ep` is gone. After the last one the slot is free, and the
    /// endpoint quota that paid for it gets it back.
    fn ep_unref(&mut self, ep: usize) {
        self.eps[ep].refs -= 1;
        let owner = self.eps[ep].owner;
        if self.eps[ep].refs == 0 && owner != NONE {
            self.tasks[owner as usize].ep_quota += 1;
        }
    }

    /// Task `t` lives on without `cap`, which was dropped or revoked. What it held only
    /// through that capability goes too: its place in an endpoint's queue, an interrupt
    /// line, mapped device memory.
    pub fn release(&mut self, t: usize, cap: Cap) {
        let task = self.tasks[t];
        match (cap, task.state) {
            (Cap::Endpoint { ep, .. }, State::Recv(on)) if on == ep => {
                if !task.reaches(ep, R_RECV, None) {
                    self.eps[ep as usize].waiter = NONE;
                    self.wake(t, E_NOCAP);
                }
                // A waiting task takes capabilities along only while it holds the grant right.
                self.tasks[t].grant &= task.reaches(ep, R_RECV | R_GRANT, None);
            }
            (Cap::Endpoint { ep, .. }, State::Send(on)) if on == ep => {
                let badge = Some(task.send_badge);
                if !task.reaches(ep, R_SEND, badge) {
                    self.wake(t, E_NOCAP);
                }
                self.tasks[t].grant &= task.reaches(ep, R_SEND | R_GRANT, badge);
            }
            (Cap::Memory { .. }, _) => self.unmap_unbacked(t),
            _ => {}
        }
        for irq in 0..IRQ_LINES {
            let (bind, task) = (self.irqs[irq], &self.tasks[t]);
            let backed = task.has_irq(irq) && task.reaches(bind.ep, R_RECV, None);
            if bind.task as usize == t && !backed {
                self.irq_unbind(irq);
            }
        }
    }

    /// Nobody can receive on endpoint `ep` any more.
    fn orphan(&mut self, ep: usize) {
        for t in 0..MAX_TASKS {
            match self.tasks[t].state {
                State::Send(queued) if queued as usize == ep => self.wake(t, E_DEAD),
                State::Zombie(queued) if queued as usize == ep => self.reap(t),
                _ => {}
            }
        }
        for irq in 0..IRQ_LINES {
            if self.irqs[irq].task != NONE && self.irqs[irq].ep as usize == ep {
                self.irq_unbind(irq);
            }
        }
        self.eps[ep].irq_pending = 0;
    }

    /// Mask interrupt line `irq` and forget who receives it.
    pub fn irq_unbind(&mut self, irq: usize) {
        arch::irq_mask(irq as u32, true);
        let bind = self.irqs[irq];
        if bind.task != NONE {
            self.eps[bind.ep as usize].irq_pending &= !(1 << irq);
        }
        self.irqs[irq] = IrqBind::UNBOUND;
    }

    // ---------------------------------------------------------------- traps and death

    /// The running task entered the kernel: a system call, an interrupt or a CPU exception.
    pub fn on_trap(&mut self) {
        let me = self.cur;
        let frame = self.tasks[me].frame;
        match frame.vector {
            SYSCALL_VECTOR => self.syscall(),
            vector if (IRQ_BASE..IRQ_BASE + IRQ_LINES as u32).contains(&vector) => {
                self.on_irq(vector - IRQ_BASE);
            }
            // NMI, double fault and machine check report broken hardware or a broken kernel.
            2 | 8 | 18 => panic!("fatal exception {} at eip={:#x}", frame.vector, frame.eip),
            // Any other exception is the task's own doing. It ends; nobody else notices.
            vector => {
                self.stats.faults = self.stats.faults.wrapping_add(1);
                let address = if vector == 14 { arch::read_cr2() } else { 0 };
                self.kill(me, [vector, address, frame.eip, frame.error]);
            }
        }
    }

    /// End task `victim` and every task it started, directly or indirectly. What they held
    /// of the three quotas (memory, tasks, endpoints) goes back to the victim's parent.
    pub fn kill(&mut self, victim: usize, fault: [u32; 4]) {
        let mut doomed = 1u32 << victim;
        loop {
            let before = doomed;
            for (index, task) in self.tasks.iter().enumerate() {
                if task.alive() && task.parent != NONE && doomed & (1 << task.parent) != 0 {
                    doomed |= 1 << index;
                }
            }
            if doomed == before {
                break;
            }
        }
        let is_doomed = |index: &usize| doomed & (1 << index) != 0;
        let refund: u32 = (0..MAX_TASKS).filter(is_doomed).map(|index| self.tasks[index].quota).sum();
        let parent = self.tasks[victim].parent;
        // What the doomed tasks answer for passes to the victim's parent: their unused
        // quotas, the endpoints they pay for, and their ended children that still wait to
        // be noticed. (If the root itself ends, `parent` is `NONE`: everything is torn down
        // and `start_root` begins again with full quotas.)
        let (mut tasks, mut endpoints) = (0, 0);
        for index in 0..MAX_TASKS {
            let task = &mut self.tasks[index];
            if doomed & (1 << index) != 0 {
                tasks += core::mem::take(&mut task.task_quota);
                endpoints += core::mem::take(&mut task.ep_quota);
            }
            if task.parent != NONE && doomed & (1 << task.parent) != 0 {
                task.parent = parent;
            }
        }
        for endpoint in self.eps.iter_mut() {
            if endpoint.owner != NONE && doomed & (1 << endpoint.owner) != 0 {
                endpoint.owner = parent;
            }
        }
        self.destroy(victim, fault);
        for index in (0..MAX_TASKS).filter(is_doomed) {
            if index != victim {
                self.destroy(index, [CAUSE_PARENT_DIED, 0, 0, 0]);
            }
        }
        self.resched = true;
        if parent == NONE {
            // The supervisor itself ended. Nothing in user space survives it, so start over.
            let now = self.micros();
            kprintln!("kernel: the root task ended (cause {:#x}); restarting user space at t={} us", fault[0], now);
            self.start_root();
        } else {
            let heir = &mut self.tasks[parent as usize];
            heir.quota += refund;
            heir.task_quota += tasks;
            heir.ep_quota += endpoints;
        }
    }

    /// Release everything task `t` holds and queue its fault message, if anyone listens.
    fn destroy(&mut self, t: usize, fault: [u32; 4]) {
        if let State::Recv(ep) = self.tasks[t].state {
            self.eps[ep as usize].waiter = NONE;
        }
        self.tasks[t].state = State::Zombie(NONE);
        self.tasks[t].deadline = u64::MAX;
        for client in 0..MAX_TASKS {
            if self.tasks[client].state == State::Reply(t as u8) {
                self.wake(client, E_DEAD);
            }
        }
        for irq in 0..IRQ_LINES {
            if self.irqs[irq].task as usize == t {
                self.irq_unbind(irq);
            }
        }
        for slot in 0..CAP_SLOTS {
            self.drop_cap(t, slot);
        }
        if self.space == self.tasks[t].pd {
            self.space = self.mem.kernel_pd;
            arch::write_cr3(self.space);
        }
        self.destroy_space(t);
        self.tasks[t].quota = 0;
        self.tasks[t].fault = fault;
        let ep = self.tasks[t].fault_ep;
        if ep != NONE && self.eps[ep as usize].recv_refs > 0 {
            self.seq += 1;
            self.tasks[t].seq = self.seq;
            self.tasks[t].state = State::Zombie(ep);
            self.ep_ready(ep as usize);
        } else {
            self.reap(t);
        }
    }

    /// Recycle the slot of an ended task whose fault message was delivered or is not wanted.
    /// The slot returns to the task quota of whoever answers for the task.
    pub fn reap(&mut self, t: usize) {
        let (ep, parent) = (self.tasks[t].fault_ep, self.tasks[t].parent);
        if ep != NONE {
            self.ep_unref(ep as usize);
        }
        if parent != NONE {
            self.tasks[parent as usize].task_quota += 1;
        }
        let task = &mut self.tasks[t];
        task.fault_ep = NONE;
        task.gen += 1;
        task.state = State::Free;
    }

    /// Start `init` from the boot archive as task 0 and hand it every resource there is.
    fn start_root(&mut self) {
        let (base, len) = self.archive;
        // SAFETY: `boot` checked that the archive lies inside the identity map, directly
        // behind the kernel image. That memory is never given to the frame allocator and
        // nothing writes to it after boot (user mappings of it are read-only), so a shared
        // slice over it is valid for as long as the kernel runs.
        let archive = unsafe { core::slice::from_raw_parts(arch::phys(base), len as usize) };
        let image = abi::archive::find(archive, b"init").expect("the boot archive has no init");
        let mut name = [0; 12];
        name[..4].copy_from_slice(b"init");
        let gen = self.tasks[0].gen;
        self.tasks[0] = Tcb {
            gen,
            quota: self.root_quota,
            task_quota: (MAX_TASKS - 1) as u8,
            ep_quota: MAX_ENDPOINTS as u8,
            state: State::Zombie(NONE),
            name,
            ..Tcb::EMPTY
        };
        let entry = match self.load(0, &Image::Kernel(image), ROOT_STACK_PAGES) {
            Ok(entry) => entry,
            Err(code) => panic!("cannot load init: error {code}"),
        };
        let root_caps = [
            Cap::Log,
            Cap::Info,
            Cap::IoPort { first: 0, last: 0xFFFF },
            Cap::Irq { mask: 0xFFFA }, // all lines except the timer and the cascade
            Cap::Memory { page: 0xB8, pages: 8, write: true }, // VGA text memory
            Cap::Memory { page: base / PAGE, pages: page_up(len) / PAGE, write: false },
        ];
        for (slot, cap) in root_caps.into_iter().enumerate() {
            self.install(0, slot, cap, ORIGINAL);
        }
        self.tasks[0].frame = TrapFrame::user(entry, STACK_TOP, [0, self.root_starts, 0, 0]);
        self.ready(0);
        let now = self.micros();
        if now < self.root_not_before {
            // It died young. Wait before trying again instead of spinning.
            self.tasks[0].state = State::Sleep;
            self.tasks[0].deadline = self.root_not_before;
        }
        self.root_not_before = now.max(self.root_not_before) + ROOT_MIN_US;
        self.root_starts = self.root_starts.wrapping_add(1);
    }
}
