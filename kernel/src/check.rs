//! The invariant checker: a second, independent account of everything the kernel owns.
//!
//! `sysinfo(INFO_CHECK)` walks every table and every page table and reports the first rule
//! that does not hold. The self-test and the fuzzer call it between their rounds, so a
//! kernel bug that corrupts state is caught at once instead of crashing something later.
//! The checker only reads kernel state (its bitmap of seen frames aside) and never panics.

use crate::mem::{frame_of, Mem, ACCESSED, ADDR, DIRTY, ENTRIES, EXEC, FOREIGN, NX, P, RW, US, USER_ROOTS};
use crate::task::{Cap, Kernel, State, Tcb, NONE, ORIGINAL};
use abi::{CAP_SLOTS, MAX_ENDPOINTS, MAX_TASKS, MSG_SIZE, PAGE, R_GRANT, R_RECV, R_SEND};

impl Kernel {
    /// 0 if every invariant holds, otherwise the number of the first one that is violated.
    pub fn check(&mut self) -> u32 {
        let memory = self.check_memory();
        let all = memory.and_then(|()| self.check_objects()).and_then(|()| self.check_caps());
        all.err().unwrap_or(0)
    }

    /// Note that `frame` has an owner. Rule 1: it is a frame the allocator manages.
    /// Rule 2: nothing else owns it. Rule 3: the allocator does not think it is free.
    fn claim(&mut self, frame: u32) -> Result<(), u32> {
        if !self.mem.managed(frame) {
            return Err(1);
        }
        let number = (frame / PAGE) as usize;
        let (word, bit) = (number / 32, 1 << (number % 32));
        if self.scratch[word] & bit != 0 {
            return Err(2);
        }
        if self.mem.is_free(frame) {
            return Err(3);
        }
        self.scratch[word] |= bit;
        Ok(())
    }

    /// Rules 1-7, 18, 20, 22 and 23: every frame is free or owned by exactly one task, each
    /// task is charged for exactly what it owns, no address space can reach the kernel, device
    /// memory is mapped only where a Memory capability of the task covers it, and no memory is
    /// both writable and executable.
    fn check_memory(&mut self) -> Result<(), u32> {
        self.scratch.fill(0);
        // Rule 23: the kernel's own map is what was built at boot, so kernel code is still
        // read-only and nothing else in the kernel can be executed.
        if !self.mem.kernel_map_intact() {
            return Err(23);
        }
        let mut owned = 0;
        for t in 0..self.tasks.len() {
            let (alive, pd) = (self.tasks[t].alive(), self.tasks[t].pd);
            let (quota, charged) = (self.tasks[t].quota, self.tasks[t].charged);
            if !alive {
                // Rule 1: a task that ended holds no memory.
                if pd != 0 || charged != 0 {
                    return Err(1);
                }
                continue;
            }
            let (frames, foreign) = self.check_space(t, pd)?;
            // Rule 4: the charge equals what the page tables show. Rule 5: within quota.
            if frames != charged {
                return Err(4);
            }
            if charged > quota {
                return Err(5);
            }
            if foreign != self.tasks[t].foreign {
                return Err(20);
            }
            owned += frames;
        }
        // Rule 6: owned + free = all. Rule 7: the free counter matches the bitmap.
        if owned + self.mem.free != self.mem.total {
            return Err(6);
        }
        if self.mem.count_free() != self.mem.free {
            return Err(7);
        }
        Ok(())
    }

    /// Walk the three levels of task `t`'s page tables. Returns how many frames the task
    /// owns (tables included) and how many pages of device memory it has mapped.
    fn check_space(&mut self, t: usize, root: u32) -> Result<(u32, u32), u32> {
        self.claim(root)?;
        let (mut frames, mut foreign) = (1, 0);
        for slot in 0..ENTRIES {
            // QEMU marks a root entry it has walked through as accessed. A real processor
            // keeps the root's entries in registers and writes nothing back. Either way the
            // mark is the processor's and not the kernel's, so it is not compared.
            let entry = self.mem.read_entry(root, slot) & !ACCESSED;
            if !USER_ROOTS.contains(&slot) {
                // Rule 18: outside the user range a root table holds the shared kernel map
                // (which has no user-accessible entry) and nothing else.
                if entry != Mem::kernel_root(slot) {
                    return Err(18);
                }
                continue;
            }
            // Rule 18: a user slot of the root names a directory and carries no other bit.
            let directory = frame_of(entry);
            if entry != directory as u64 | P {
                return Err(18);
            }
            self.claim(directory)?;
            frames += 1;
            for index in 0..ENTRIES {
                let pde = self.mem.read_entry(directory, index);
                if pde & P == 0 {
                    continue;
                }
                // Rule 18: a directory entry names a page table and carries no other bit
                // (the processor's accessed mark aside).
                let table = frame_of(pde);
                if pde & !ACCESSED != table as u64 | P | RW | US {
                    return Err(18);
                }
                self.claim(table)?;
                frames += 1;
                for index in 0..ENTRIES {
                    let pte = self.mem.read_entry(table, index);
                    if pte & P == 0 {
                        continue;
                    }
                    if !page_ok(pte, self.mem.nx) {
                        return Err(22);
                    }
                    if pte & FOREIGN == 0 {
                        self.claim(frame_of(pte))?;
                        frames += 1;
                    } else if self.mem.managed(frame_of(pte)) {
                        return Err(18); // device memory is never ordinary RAM
                    } else if !self.tasks[t].has_memory(frame_of(pte), pte & RW != 0) {
                        return Err(20); // Rule 20: no device page without its capability
                    } else {
                        foreign += 1;
                    }
                }
            }
        }
        Ok((frames, foreign))
    }

    /// Rules 8-17, 20, 21 and 24: reference counts, wait queues, the task tree, interrupt
    /// bindings, the quotas for the task table and the endpoint table, and the buffers of
    /// waiting tasks.
    fn check_objects(&self) -> Result<(), u32> {
        let mut refs = [0u16; MAX_ENDPOINTS];
        let mut recv_refs = [0u16; MAX_ENDPOINTS];
        let mut roots = 0;
        let mut quota = 0u64;
        // Slots of the two tables that are accounted for: in use, or unused quota.
        let (mut task_slots, mut ep_slots) = (0, 0);
        let receivable = |ep: u8| self.eps.get(ep as usize).is_some_and(|endpoint| endpoint.recv_refs > 0);
        let alive = |index: u8| self.tasks.get(index as usize).is_some_and(Tcb::alive);

        for (index, task) in self.tasks.iter().enumerate() {
            quota += task.quota as u64;
            task_slots += task.task_quota as usize + (index != 0 && task.state != State::Free) as usize;
            ep_slots += task.ep_quota as usize;
            for cap in task.caps.iter() {
                if let Cap::Endpoint { ep, rights, .. } = *cap {
                    *refs.get_mut(ep as usize).ok_or(8u32)? += 1;
                    if rights as u32 & R_RECV != 0 {
                        recv_refs[ep as usize] += 1;
                    }
                }
            }
            if task.fault_ep != NONE && task.state != State::Free {
                *refs.get_mut(task.fault_ep as usize).ok_or(8u32)? += 1;
            }
            let grant = if task.grant { R_GRANT } else { 0 };
            // Rule 24: a task that waits for a message still owns the writable buffer it
            // named, so the kernel can deliver into it later.
            let waits = matches!(task.state, State::Send(_) | State::Recv(_) | State::Reply(_));
            if waits && self.mem.check_user(task.pd, task.msg_ptr, MSG_SIZE, true).is_err() {
                return Err(24);
            }
            match task.state {
                // Rule 10: a task blocked in `recv` is the registered waiter of its endpoint.
                // Rule 20: a task waits on an endpoint only while a capability lets it, and
                // it may pass a capability along only while that capability has the grant right.
                State::Recv(ep) => {
                    if self.eps.get(ep as usize).map(|endpoint| endpoint.waiter as usize) != Some(index) {
                        return Err(10);
                    }
                    if !task.reaches(ep, R_RECV | grant, None) {
                        return Err(20);
                    }
                }
                // Rule 11: nothing queues on an endpoint that nobody can receive from, and
                // an ended task queues only on its own fault endpoint.
                State::Send(ep) if !receivable(ep) => return Err(11),
                State::Send(ep) if !task.reaches(ep, R_SEND | grant, Some(task.send_badge)) => return Err(20),
                State::Zombie(ep) if ep != task.fault_ep || !receivable(ep) => return Err(11),
                // Rule 21: an ended task that waits to be noticed is charged to a living one.
                State::Zombie(_) if !alive(task.parent) => return Err(21),
                // Rule 12: a caller waits for a reply only from a task that can still give it.
                State::Reply(server) if !alive(server) => return Err(12),
                _ => {}
            }
            if !task.alive() {
                // Rule 14: a task that ended holds no quota of any kind and no capabilities.
                let quotas = task.quota != 0 || task.task_quota != 0 || task.ep_quota != 0;
                if quotas || task.caps.iter().any(|cap| *cap != Cap::Null) {
                    return Err(14);
                }
            } else if task.parent == NONE {
                roots += 1;
            } else if !alive(task.parent) {
                return Err(13); // Rule 13: no task outlives its parent
            }
        }
        // Rule 15: one root task, and quota is neither created nor lost.
        if roots != 1 || !self.tasks[0].alive() || quota != self.root_quota as u64 {
            return Err(15);
        }
        for (index, endpoint) in self.eps.iter().enumerate() {
            // Rules 8 and 9: reference counts match a recount.
            if endpoint.refs != refs[index] {
                return Err(8);
            }
            if endpoint.recv_refs != recv_refs[index] {
                return Err(9);
            }
            let waiter = self.tasks.get(endpoint.waiter as usize).map(|task| task.state);
            if endpoint.waiter != NONE && waiter != Some(State::Recv(index as u8)) {
                return Err(10);
            }
            if endpoint.refs != 0 {
                // Rule 21: an endpoint in use is paid for by a living task.
                if !alive(endpoint.owner) {
                    return Err(21);
                }
                ep_slots += 1;
            }
        }
        // Rule 21: slots in use and unused quota add up to each table, no more and no less.
        if task_slots != MAX_TASKS - 1 || ep_slots != MAX_ENDPOINTS {
            return Err(21);
        }
        for (line, bind) in self.irqs.iter().enumerate() {
            if bind.task == NONE {
                continue;
            }
            // Rule 16: an interrupt is bound to a living task and a receivable endpoint.
            if !alive(bind.task) || !receivable(bind.ep) {
                return Err(16);
            }
            // Rule 20: the task still holds the capabilities the binding was made with.
            let task = &self.tasks[bind.task as usize];
            if !task.has_irq(line) || !task.reaches(bind.ep, R_RECV, None) {
                return Err(20);
            }
        }
        // Rule 17: the kernel never met a state it considered impossible.
        if self.stats.anomalies != 0 {
            return Err(17);
        }
        Ok(())
    }

    /// Rule 19: authority only narrows. Every capability is an original, or a copy of a
    /// capability that still exists and is at least as strong.
    fn check_caps(&self) -> Result<(), u32> {
        for (t, task) in self.tasks.iter().enumerate() {
            for (slot, &cap) in task.caps.iter().enumerate() {
                let from = task.from[slot] as usize;
                if from == ORIGINAL as usize {
                    continue;
                }
                let source = self.tasks.get(from / CAP_SLOTS).map(|task| task.caps[from % CAP_SLOTS]);
                let narrowed = source.is_some_and(|source| covers(source, cap));
                if cap == Cap::Null || from == t * CAP_SLOTS + slot || !narrowed {
                    return Err(19);
                }
            }
        }
        Ok(())
    }
}

/// Rule 22 (W^X): a mapped user page is user-accessible and has no unknown bit; it is never
/// both writable and executable; device memory is never executable; and where the processor
/// has the no-execute bit (`nx`), every page that is not program code carries it.
fn page_ok(pte: u64, nx: u64) -> bool {
    let known = P | RW | US | ACCESSED | DIRTY | FOREIGN | EXEC | NX | ADDR;
    let code = pte & EXEC != 0;
    let writable_code = code && pte & (RW | FOREIGN) != 0;
    let no_execute = if code { 0 } else { nx };
    pte & !known == 0 && pte & US != 0 && !writable_code && pte & NX == no_execute
}

/// True if `copy` could have been made from `source`: the same object and no added power.
fn covers(source: Cap, copy: Cap) -> bool {
    match (source, copy) {
        (Cap::Endpoint { ep, rights, badge }, Cap::Endpoint { ep: to, rights: kept, badge: stamp }) => {
            let stamped = badge == 0 && rights as u32 & R_GRANT != 0;
            to == ep && kept != 0 && kept & !rights == 0 && (stamp == badge || stamped)
        }
        (Cap::IoPort { first, last }, Cap::IoPort { first: low, last: high }) => first <= low && low <= high && high <= last,
        (Cap::Irq { mask }, Cap::Irq { mask: lines }) => lines != 0 && lines & !mask == 0,
        (Cap::Memory { page, pages, write }, Cap::Memory { page: start, pages: count, write: writable }) => {
            let inside = start >= page && count != 0 && (start - page) as u64 + count as u64 <= pages as u64;
            inside && (write || !writable)
        }
        (Cap::Null, _) => false,
        (source, copy) => source == copy,
    }
}
