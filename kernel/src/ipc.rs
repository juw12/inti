//! Messages and interrupts.
//!
//! One mechanism carries everything: a client `call`s an endpoint and blocks until the
//! server `reply`s. Interrupts and the deaths of supervised tasks arrive on endpoints as
//! messages from the kernel. A message is copied through a kernel buffer; tasks never share
//! memory, so a sender cannot change a message after the kernel has checked it.
//!
//! A message can carry one capability. It goes through only if the sender's and the
//! receiver's capability for the endpoint both hold the grant right. The receiver gets a
//! copy that stays linked to the sender's capability, so the sender can revoke it.

use crate::arch;
use crate::sys::{Out, Sys};
use crate::task::{link, Cap, Kernel, State, NONE};
use crate::{get32, put32};
use abi::{
    E_BUSY, E_DEAD, E_FAULT, E_INVAL, E_NOCAP, E_PERM, E_TIMEOUT, L_FAULT, L_IRQ, L_KERNEL,
    MSG_DATA, MSG_HEADER, MSG_SIZE, NO_CAP, R_GRANT, R_RECV, R_SEND,
};

// Offsets of the `abi::Msg` fields.
const LABEL: usize = 0;
const BADGE: usize = 4;
const TOKEN: usize = 8;
const LEN: usize = 12;
const WORDS: usize = 16;
const CAP: usize = 32;

type Buffer = [u8; MSG_SIZE as usize];

impl Kernel {
    /// The endpoint behind capability `slot` of task `t`, if the capability carries `right`:
    /// the endpoint, the badge, and whether the capability also carries the grant right.
    pub fn endpoint(&self, t: usize, slot: u32, right: u32) -> Result<(usize, u32, bool), i32> {
        match self.cap(t, slot)? {
            Cap::Endpoint { ep, rights, badge } if rights as u32 & right != 0 => {
                Ok((ep as usize, badge, rights as u32 & R_GRANT != 0))
            }
            Cap::Endpoint { .. } => Err(E_PERM),
            _ => Err(E_NOCAP),
        }
    }

    /// `call`: queue the message at `ptr` on an endpoint and block until the reply arrives.
    pub fn sys_call(&mut self, me: usize, slot: u32, ptr: u32, timeout: u32) -> Sys {
        let (ep, badge, grant) = self.endpoint(me, slot, R_SEND)?;
        self.mem.check_user(self.tasks[me].pd, ptr, MSG_SIZE, true)?;
        if self.eps[ep].recv_refs == 0 {
            return Err(E_DEAD);
        }
        self.token_counter = self.token_counter.wrapping_add(1);
        self.seq += 1;
        let deadline = self.deadline(timeout);
        let task = &mut self.tasks[me];
        // Bit 31 keeps a token from ever being 0; the low 5 bits name the caller.
        task.token = 0x8000_0000 | ((self.token_counter << 5) & 0x7FFF_FFE0) | me as u32;
        task.send_badge = badge;
        task.grant = grant;
        task.msg_ptr = ptr;
        task.seq = self.seq;
        task.state = State::Send(ep as u8);
        task.deadline = deadline; // covers the wait for the server and for its reply
        self.ep_ready(ep);
        Ok(Out::Blocked)
    }

    /// `recv`: take the next message from an endpoint, waiting up to `timeout` ms.
    pub fn sys_recv(&mut self, me: usize, slot: u32, ptr: u32, timeout: u32) -> Sys {
        let (ep, _, grant) = self.endpoint(me, slot, R_RECV)?;
        self.mem.check_user(self.tasks[me].pd, ptr, MSG_SIZE, true)?;
        if self.eps[ep].waiter != NONE {
            return Err(E_BUSY);
        }
        self.tasks[me].msg_ptr = ptr;
        self.tasks[me].grant = grant;
        if self.deliver(me, ep) {
            return Ok(Out::Value(0));
        }
        if timeout == 0 {
            return Err(E_TIMEOUT);
        }
        self.tasks[me].state = State::Recv(ep as u8);
        self.tasks[me].deadline = self.deadline(timeout);
        self.eps[ep].waiter = me as u8;
        Ok(Out::Blocked)
    }

    /// `reply`: answer the caller identified by `token`. A token is valid once, and only
    /// for the task that received the call. If the capability named in the reply cannot be
    /// handed over, nothing is sent and the token stays valid.
    pub fn sys_reply(&mut self, me: usize, token: u32, ptr: u32) -> Sys {
        let client = (token & 31) as usize;
        if self.tasks[client].state != State::Reply(me as u8) || self.tasks[client].token != token {
            return Err(E_INVAL);
        }
        let mut buffer = [0; MSG_SIZE as usize];
        let len = self.read_msg(me, ptr, &mut buffer)?;
        let cap = self.pass_cap(me, get32(&buffer, CAP), client, self.tasks[client].grant)?;
        put32(&mut buffer, BADGE, 0);
        put32(&mut buffer, TOKEN, 0);
        put32(&mut buffer, CAP, cap);
        self.write_msg(client, &buffer[..len]);
        self.wake(client, 0);
        Ok(Out::Value(0))
    }

    /// Hand the capability in `slot` of task `from` to task `to` along with a message: a
    /// copy goes into a free slot of `to` and stays linked to the original, so that `from`
    /// can revoke it. Returns the slot the copy went to; `NO_CAP` in, `NO_CAP` out.
    fn pass_cap(&mut self, from: usize, slot: u32, to: usize, granted: bool) -> Result<u32, i32> {
        if slot == NO_CAP {
            return Ok(NO_CAP);
        }
        if !granted {
            return Err(E_PERM);
        }
        let cap = self.cap(from, slot)?;
        let free = self.free_slot(to)?;
        self.install(to, free, cap, link(from, slot as usize));
        Ok(free as u32)
    }

    /// Copy a message out of task `t`. The data length is clamped and the kernel bit of the
    /// label is cleared, so user code cannot fake a message from the kernel.
    fn read_msg(&self, t: usize, ptr: u32, buffer: &mut Buffer) -> Result<usize, i32> {
        let pd = self.tasks[t].pd;
        self.mem.copy_in(pd, ptr, &mut buffer[..MSG_HEADER])?;
        let len = (get32(buffer, LEN) as usize).min(MSG_DATA);
        let data = ptr.checked_add(MSG_HEADER as u32).ok_or(E_FAULT)?;
        self.mem.copy_in(pd, data, &mut buffer[MSG_HEADER..MSG_HEADER + len])?;
        let label = get32(buffer, LABEL) & !L_KERNEL;
        put32(buffer, LABEL, label);
        put32(buffer, LEN, len as u32);
        Ok(MSG_HEADER + len)
    }

    /// Copy a message into the buffer that the blocked task `t` passed to `call` or `recv`.
    /// The buffer was checked then and a blocked task cannot change its memory map.
    fn write_msg(&mut self, t: usize, message: &[u8]) {
        let (pd, ptr) = (self.tasks[t].pd, self.tasks[t].msg_ptr);
        if self.mem.copy_out(pd, ptr, message).is_err() {
            self.stats.anomalies += 1;
        }
    }

    /// Hand the next thing queued on endpoint `ep` to `receiver`: a pending interrupt, or
    /// the longest-waiting caller or ended task. Returns false if nothing is queued.
    fn deliver(&mut self, receiver: usize, ep: usize) -> bool {
        let mut notice = [0; MSG_HEADER + 8];
        put32(&mut notice, CAP, NO_CAP);
        let pending = self.eps[ep].irq_pending;
        if pending != 0 {
            self.eps[ep].irq_pending = 0;
            put32(&mut notice, LABEL, L_IRQ);
            put32(&mut notice, WORDS, pending as u32);
            self.write_msg(receiver, &notice[..MSG_HEADER]);
            return true;
        }
        loop {
            let mut next: Option<usize> = None;
            for (index, task) in self.tasks.iter().enumerate() {
                let queued = task.state == State::Send(ep as u8) || task.state == State::Zombie(ep as u8);
                if queued && next.is_none_or(|first| task.seq < self.tasks[first].seq) {
                    next = Some(index);
                }
            }
            let Some(sender) = next else { return false };
            let task = self.tasks[sender];
            if task.state == State::Zombie(ep as u8) {
                put32(&mut notice, LABEL, L_FAULT);
                put32(&mut notice, BADGE, task.fault_badge);
                put32(&mut notice, LEN, 8);
                for (i, word) in task.fault.into_iter().enumerate() {
                    put32(&mut notice, WORDS + i * 4, word);
                }
                put32(&mut notice, MSG_HEADER, task.syscalls);
                put32(&mut notice, MSG_HEADER + 4, task.cpu_ticks);
                self.reap(sender);
                self.write_msg(receiver, &notice);
                return true;
            }
            let mut buffer = [0; MSG_SIZE as usize];
            let Ok(len) = self.read_msg(sender, task.msg_ptr, &mut buffer) else {
                // Checked when the call was made, so this cannot happen. Fail the call.
                self.stats.anomalies += 1;
                self.wake(sender, E_FAULT);
                continue;
            };
            // A capability sent along goes through only if both ends hold the grant right.
            let granted = task.grant && self.tasks[receiver].grant;
            match self.pass_cap(sender, get32(&buffer, CAP), receiver, granted) {
                Ok(cap) => {
                    put32(&mut buffer, BADGE, task.send_badge);
                    put32(&mut buffer, TOKEN, task.token);
                    put32(&mut buffer, CAP, cap);
                    self.tasks[sender].state = State::Reply(receiver as u8);
                    self.tasks[sender].grant = granted;
                    self.write_msg(receiver, &buffer[..len]);
                    return true;
                }
                // The capability cannot go along: this call fails and the next one is tried.
                Err(code) => self.wake(sender, code),
            }
        }
    }

    /// Something was queued on endpoint `ep`: pass it on if a task is waiting there.
    pub fn ep_ready(&mut self, ep: usize) {
        let waiter = self.eps[ep].waiter;
        if waiter != NONE && self.deliver(waiter as usize, ep) {
            self.eps[ep].waiter = NONE;
            self.wake(waiter as usize, 0);
        }
    }

    /// A hardware interrupt. The timer drives the scheduler; every other line is masked and
    /// turned into a message for the driver task bound to it, which unmasks it when done.
    pub fn on_irq(&mut self, irq: u32) {
        self.stats.irqs = self.stats.irqs.wrapping_add(1);
        if irq == 0 {
            arch::irq_eoi(0);
            self.tick();
            return;
        }
        if arch::irq_spurious(irq) {
            return;
        }
        arch::irq_mask(irq, true);
        arch::irq_eoi(irq);
        let bind = self.irqs[irq as usize];
        if bind.task != NONE {
            self.eps[bind.ep as usize].irq_pending |= 1 << irq;
            self.ep_ready(bind.ep as usize);
        }
    }
}
