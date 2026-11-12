//! The world around the kernel under test.
//!
//! A `World` is a booted kernel on the fake machine, plus the part that user programs play
//! on the real one: it puts a task on the processor, fills in the registers of a system
//! call, lets the kernel handle the trap and reads the result. It also raises interrupts,
//! lets time pass, takes and restores snapshots and - after every step - checks the list of
//! properties in `verify`.
//!
//! Every task of the model "runs" the same 128-byte program: an ELF file whose code is never
//! executed. What a task does is decided from outside, by the tests in `tables.rs` and by
//! the explorer. That is the point: the kernel must stay sound whatever its tasks do.

use crate::arch::{self, TrapFrame, ARCHIVE, E820_MAP, IRQ_BASE, KERNEL_END, PAGE_BYTES, SYSCALL_VECTOR};
use crate::state::{deadline_kind, NEAR};
use crate::task::{link, Cap, Kernel, State, IDLE, IRQ_LINES, NONE};
use crate::{get32, put32};
use abi::nr;
use abi::{
    ARCHIVE_HEADER, ARCHIVE_MAGIC, ARCHIVE_NAME, CAP_SLOTS, CAUSE_EXIT, CAUSE_KILLED, CAUSE_PARENT_DIED, L_FAULT,
    L_IRQ, L_KERNEL, MAX_TASKS, MSG_HEADER, NO_CAP, PAGE, R_RECV, SPAWN_ARGS_SIZE, SPAWN_CAPS, STACK_TOP, USER_BASE,
};
use std::mem::MaybeUninit;
use std::ptr;

/// Every task keeps its message buffer in the top page of its stack ...
pub const BUFFER: u32 = STACK_TOP - PAGE;
/// ... and the arguments of a spawn a little further up in the same page.
pub const SPAWN_ARGS: u32 = BUFFER + 0x200;
/// A timeout that never ends by itself during a run: about seven weeks, in milliseconds.
pub const LONG: u32 = 0xFFFF_FFFE;
/// The badge on every fault report in the model.
pub const FAULT_BADGE: u32 = 0xFA17;
/// The size of the program that every task runs.
pub const IMAGE_SIZE: u32 = 128;
/// Timer counts in 10 ms, the period of the kernel's tick.
pub const TICK: u32 = 11_932;
/// Frames that a task with one page of code and one page of stack occupies: three for the
/// top of its address space, two page tables, two pages.
pub const CHILD_FRAMES: u32 = 7;
/// The same for the root task, which has eight pages of stack.
pub const ROOT_FRAMES: u32 = 14;
/// Two sizes of machine, in frames of RAM beyond the kernel: 2 MiB and 8 MiB in total.
pub const SMALL: usize = 173;
pub const LARGE: usize = 1709;

// What the model's tasks write into their messages, so that `verify` can tell whether a
// message arrived whole and from whom.
const CALL_LABEL: u32 = 0x100;
const CALL_WORD: u32 = 0xC0DE_0000;
const REPLY_LABEL: u32 = 0x200;
const REPLY_WORD: u32 = 0x0BEE_0000;

/// What a task asks for when it spawns another.
#[derive(Clone, Copy)]
pub struct Spawn {
    /// Frames of memory the child may use.
    pub quota: u32,
    pub stack: u32,
    /// The slot of the endpoint that hears of the child's death, or `NO_CAP`.
    pub fault: u32,
    /// How many tasks and endpoints the child may create in turn.
    pub tasks: u32,
    pub eps: u32,
    /// Slots of the parent whose capabilities the child starts with.
    pub caps: [u32; SPAWN_CAPS],
    /// The child's share of the processor in percent; 0 asks for the parent's own.
    pub cpu: u32,
}

impl Spawn {
    /// A small task with no capability at all.
    pub const PLAIN: Spawn =
        Spawn { quota: CHILD_FRAMES + 5, stack: 1, fault: NO_CAP, tasks: 0, eps: 0, caps: [NO_CAP; SPAWN_CAPS], cpu: 0 };

    pub fn with_caps(mut self, slots: &[u32]) -> Spawn {
        self.caps[..slots.len()].copy_from_slice(slots);
        self
    }
}

/// A copy of a kernel, bit for bit.
struct Frozen {
    copy: Box<MaybeUninit<Kernel>>,
    taken: bool,
}

impl Frozen {
    fn new() -> Frozen {
        Frozen { copy: Box::new(MaybeUninit::uninit()), taken: false }
    }

    fn take(&mut self, k: &Kernel) {
        // SAFETY: source and target are two distinct allocations of one `Kernel` each. The
        // kernel's state is plain data - no pointer, no heap, no `Drop` - so its bytes are
        // all there is to copy.
        unsafe { ptr::copy_nonoverlapping(k as *const Kernel, self.copy.as_mut_ptr(), 1) };
        self.taken = true;
    }

    fn put(&self, k: &mut Kernel) {
        assert!(self.taken, "no snapshot of the kernel was taken");
        // SAFETY: as in `take`; the copy was initialized there, and overwriting `*k` loses
        // nothing because `Kernel` owns nothing.
        unsafe { ptr::copy_nonoverlapping(self.copy.as_ptr(), k as *mut Kernel, 1) };
    }
}

/// A reply that the kernel accepted in the step just taken.
#[derive(Clone, Copy)]
struct Replied {
    server: usize,
    client: usize,
    cap: u32,
}

pub struct World {
    /// The kernel under test. It is boxed so that it never moves: the machine remembers
    /// where the trap frame of the running task is.
    pub k: Box<Kernel>,
    base: Frozen,
    parent: Frozen,
    // What `verify` must know about the step just taken.
    before: [(State, u64); MAX_TASKS],
    receiver: Option<usize>,
    replied: Option<Replied>,
    eois: u32,
    interrupts: u32,
    /// The first expectation that did not hold in this step; `verify` reports it.
    pub problem: Option<String>,
    /// Set by the explorer, which decides itself when a timeout ends.
    pub exploring: bool,
    /// How many delivered messages and replies `verify` has compared with what was sent.
    pub deliveries: u64,
    pub replies: u64,
}

/// The BIOS memory map of a PC with RAM up to `top`.
fn memory_map(top: u32) -> Vec<u8> {
    let regions: [(u64, u64, u32); 4] =
        [(0, 0x9_FC00, 1), (0x9_FC00, 0x400, 2), (0xF_0000, 0x1_0000, 2), (0x10_0000, top as u64 - 0x10_0000, 1)];
    let mut map = (regions.len() as u32).to_le_bytes().to_vec();
    for (start, length, kind) in regions {
        map.extend_from_slice(&start.to_le_bytes());
        map.extend_from_slice(&length.to_le_bytes());
        map.extend_from_slice(&kind.to_le_bytes());
        map.extend_from_slice(&[0; 4]);
    }
    map
}

/// The program of every task: a valid ELF file with one segment, the file itself. The
/// kernel only ever looks at the headers.
fn image() -> [u8; IMAGE_SIZE as usize] {
    let mut elf = [0; IMAGE_SIZE as usize];
    elf_headers(&mut elf, IMAGE_SIZE, IMAGE_SIZE);
    elf
}

/// Write the headers of an ELF file with one segment into `elf`: the first `file` bytes of
/// the file, loaded at the bottom of user space into `memory` bytes, to read and execute.
pub fn elf_headers(elf: &mut [u8], file: u32, memory: u32) {
    elf[..7].copy_from_slice(&[0x7F, b'E', b'L', b'F', 1, 1, 1]);
    elf[16] = 2; // an executable ...
    elf[18] = 3; // ... for the 386
    put32(elf, 20, 1);
    put32(elf, 24, USER_BASE + 84); // where it starts to run
    put32(elf, 28, 52); // the segment table follows the 52-byte header
    elf[40] = 52;
    elf[42] = 32;
    elf[44] = 1;
    for (i, word) in [1, 0, USER_BASE, USER_BASE, file, memory, 5, PAGE].into_iter().enumerate() {
        put32(elf, 52 + 4 * i, word);
    }
}

/// The boot archive, holding one program: `init`.
fn archive() -> Vec<u8> {
    const IMAGE_AT: usize = 0x100;
    let mut archive = vec![0; IMAGE_AT + IMAGE_SIZE as usize];
    archive[..8].copy_from_slice(&ARCHIVE_MAGIC);
    put32(&mut archive, 8, 1);
    archive[ARCHIVE_HEADER..ARCHIVE_HEADER + 4].copy_from_slice(b"init");
    put32(&mut archive, ARCHIVE_HEADER + ARCHIVE_NAME, IMAGE_AT as u32);
    put32(&mut archive, ARCHIVE_HEADER + ARCHIVE_NAME + 4, IMAGE_SIZE);
    archive[IMAGE_AT..].copy_from_slice(&image());
    archive
}

fn check(holds: bool, otherwise: &str) -> Result<(), String> {
    if holds {
        Ok(())
    } else {
        Err(otherwise.to_string())
    }
}

impl World {
    /// Power on a machine with `managed` frames of RAM beyond the kernel, and boot.
    pub fn boot(managed: usize, nx: bool) -> World {
        let frames = KERNEL_END as usize / PAGE_BYTES + managed;
        arch::power_on(frames, nx);
        let archive = archive();
        arch::with(|machine| {
            machine.load(E820_MAP, &memory_map((frames * PAGE_BYTES) as u32));
            machine.load(ARCHIVE, &archive);
        });
        let mut k = Box::new(Kernel::new());
        k.boot(E820_MAP, ARCHIVE, archive.len() as u32);
        k.schedule();
        let mut world = World {
            k,
            base: Frozen::new(),
            parent: Frozen::new(),
            before: [(State::Free, 0); MAX_TASKS],
            receiver: None,
            replied: None,
            eois: 0,
            interrupts: 0,
            problem: None,
            exploring: false,
            deliveries: 0,
            replies: 0,
        };
        world.save_base();
        world
    }

    // ---- Steps ---------------------------------------------------------------------------

    /// A step begins: note what `verify` will compare with afterwards.
    fn begin(&mut self) {
        for (t, task) in self.k.tasks.iter().enumerate() {
            self.before[t] = (task.state, task.gen);
        }
        self.receiver = None;
        self.replied = None;
        self.eois = arch::with(|machine| machine.cpu.eois);
        self.interrupts = 0;
    }

    /// Put task `t` on the processor, let it trap into the kernel with the registers that
    /// `prepare` sets, and let the kernel choose who runs next. Any ready task can be made
    /// to run this way; the real scheduler would choose only some of them at a given moment.
    fn trap(&mut self, t: usize, prepare: impl FnOnce(&mut TrapFrame)) {
        assert!(self.k.tasks[t].state == State::Ready, "task {t} cannot take a step: it is not ready");
        self.k.ready(t);
        self.k.schedule();
        assert!(self.k.cur == t, "the scheduler did not choose the task that was just made ready");
        arch::with(|machine| machine.advance(2));
        prepare(&mut self.k.tasks[t].frame);
        self.k.on_trap();
        self.k.schedule();
    }

    /// Task `t` makes a system call. The result is `None` if the call left the task blocked
    /// or dead.
    pub fn syscall(&mut self, t: usize, number: u32, args: [u32; 4]) -> Option<i32> {
        self.begin();
        let gen = self.k.tasks[t].gen;
        self.trap(t, |frame| {
            frame.vector = SYSCALL_VECTOR;
            frame.eax = number;
            frame.ebx = args[0];
            frame.ecx = args[1];
            frame.edx = args[2];
            frame.edi = args[3];
        });
        let task = &self.k.tasks[t];
        (task.gen == gen && task.state == State::Ready).then_some(task.frame.eax as i32)
    }

    /// Task `t` touches `address`, which it may not: a page fault.
    pub fn fault(&mut self, t: usize, address: u32) {
        self.begin();
        arch::with(|machine| machine.cpu.cr2 = address);
        self.trap(t, |frame| {
            frame.vector = 14;
            frame.error = 4;
        });
    }

    /// Task `t` does something else the processor refuses: exception `vector`. The register
    /// that holds the address of the last page fault still holds `stale`.
    pub fn exception(&mut self, t: usize, vector: u32, stale: u32) {
        self.begin();
        arch::with(|machine| machine.cpu.cr2 = stale);
        self.trap(t, |frame| {
            frame.vector = vector;
            frame.error = 0;
        });
    }

    fn raise(&mut self, line: u32) {
        let masked = arch::with(|machine| machine.cpu.masked);
        assert!(masked & (1 << line) == 0, "interrupt line {line} is masked and cannot fire");
        self.interrupts += 1;
        if self.k.cur == IDLE {
            self.k.on_irq(line);
        } else {
            let running = self.k.cur;
            self.k.tasks[running].frame.vector = IRQ_BASE + line;
            self.k.on_trap();
        }
        self.k.schedule();
    }

    /// A device raises interrupt `line`.
    pub fn interrupt(&mut self, line: u32) {
        self.begin();
        self.raise(line);
    }

    /// The deadline of task `t` passes. The clock is not moved that far: the deadline is
    /// pulled to the present, and one timer tick lets the kernel notice.
    pub fn expire(&mut self, t: usize) {
        self.begin();
        self.k.tasks[t].deadline = 0;
        arch::with(|machine| machine.advance(2));
        self.raise(0);
    }

    fn tick(&mut self) {
        arch::with(|machine| machine.advance(TICK));
        self.raise(0);
    }

    /// Let `count` timer periods pass: whoever the kernel chose runs for each of them.
    pub fn ticks(&mut self, count: u32) {
        self.begin();
        for _ in 0..count {
            self.tick();
        }
    }

    /// Let `micros` microseconds pass, with a timer interrupt every 10 ms.
    pub fn pass(&mut self, micros: u64) {
        self.begin();
        let end = arch::with(|machine| machine.micros()) + micros;
        while arch::with(|machine| machine.micros()) < end {
            self.tick();
        }
    }

    /// Let time pass until the restart delay of the root task is over. False if it is not
    /// over within two seconds.
    pub fn wake_root(&mut self) -> bool {
        self.begin();
        for _ in 0..200 {
            if self.k.tasks[0].state != State::Sleep {
                return true;
            }
            self.tick();
        }
        self.k.tasks[0].state != State::Sleep
    }

    // ---- The tasks' memory ---------------------------------------------------------------

    /// Write `data` into the memory of task `t`, as the task itself could.
    pub fn poke(&mut self, t: usize, va: u32, data: &[u8]) -> bool {
        let pd = self.k.tasks[t].pd;
        self.k.mem.copy_out(pd, va, data).is_ok()
    }

    /// Read the memory of task `t`, as the task itself could.
    pub fn peek(&self, t: usize, va: u32, data: &mut [u8]) -> bool {
        self.k.mem.copy_in(self.k.tasks[t].pd, va, data).is_ok()
    }

    pub fn page_is_zero(&self, t: usize, va: u32) -> bool {
        let mut page = [0xFF; PAGE_BYTES];
        self.peek(t, va, &mut page) && page.iter().all(|&byte| byte == 0)
    }

    // ---- Bookkeeping for tests -----------------------------------------------------------

    /// Note that something expected did not happen. `verify` reports the first such note.
    pub fn expect(&mut self, holds: bool, otherwise: &str) {
        if !holds && self.problem.is_none() {
            self.problem = Some(otherwise.to_string());
        }
    }

    /// The task that the handle in `slot` of task `t` names.
    pub fn child(&self, t: usize, slot: u32) -> usize {
        match self.k.tasks[t].caps[slot as usize] {
            Cap::Task { index, .. } => index as usize,
            _ => panic!("slot {slot} of task {t} holds no task handle"),
        }
    }

    /// Tasks that exist, counting those that died and still wait to be reported.
    pub fn tasks(&self) -> usize {
        self.k.tasks.iter().filter(|task| task.state != State::Free).count()
    }

    pub fn endpoints(&self) -> usize {
        self.k.eps.iter().filter(|endpoint| endpoint.refs != 0).count()
    }

    /// All capabilities in the system.
    pub fn caps(&self) -> usize {
        self.k.tasks.iter().map(|task| task.caps.iter().filter(|&&cap| cap != Cap::Null).count()).sum()
    }

    /// How many tasks other than `actor` are in a different state than before the step.
    pub fn disturbed(&self, actor: usize) -> usize {
        let changed = |t: &usize| (self.k.tasks[*t].state, self.k.tasks[*t].gen) != self.before[*t];
        (0..MAX_TASKS).filter(|&t| t != actor).filter(changed).count()
    }

    // ---- System calls, as a task would make them -------------------------------------------

    /// Fill a message buffer of task `t`. The fields that only the kernel may set are filled
    /// with rubbish, and the label claims to come from the kernel: none of it may arrive.
    fn write_message(&mut self, t: usize, buffer: u32, label: u32, words: [u32; 2], cap: u32) {
        let mut message = [0; MSG_HEADER + 4];
        put32(&mut message, 0, L_KERNEL | label);
        put32(&mut message, 4, 0xBAD0_BAD0);
        put32(&mut message, 8, 0xBAD0_BAD1);
        put32(&mut message, 12, 4);
        put32(&mut message, 16, words[0]);
        put32(&mut message, 20, words[1]);
        put32(&mut message, 32, cap);
        message[MSG_HEADER..].fill(t as u8 ^ 0x5A);
        // If the buffer is not mapped, nothing is written and the kernel must refuse the call.
        self.poke(t, buffer, &message);
    }

    /// Task `t` calls through `slot`, offering the capability in slot `cap` (or `NO_CAP`).
    pub fn call_at(&mut self, t: usize, slot: u32, buffer: u32, cap: u32, timeout: u32) -> Option<i32> {
        // The badge that the receiver must see. The model looks it up here and sends it
        // along as ordinary data; `verify` compares it with the badge the kernel delivers.
        let badge = match self.k.tasks[t].caps.get(slot as usize) {
            Some(&Cap::Endpoint { badge, .. }) => badge,
            _ => 0,
        };
        self.write_message(t, buffer, CALL_LABEL + t as u32, [CALL_WORD + t as u32, badge], cap);
        self.syscall(t, nr::CALL, [slot, buffer, timeout, 0])
    }

    pub fn call(&mut self, t: usize, slot: u32, cap: u32, timeout: u32) -> Option<i32> {
        self.call_at(t, slot, BUFFER, cap, timeout)
    }

    pub fn recv_at(&mut self, t: usize, slot: u32, buffer: u32, timeout: u32) -> Option<i32> {
        let result = self.syscall(t, nr::RECV, [slot, buffer, timeout, 0]);
        if result == Some(0) {
            self.receiver = Some(t);
        }
        result
    }

    pub fn recv(&mut self, t: usize, slot: u32, timeout: u32) -> Option<i32> {
        self.recv_at(t, slot, BUFFER, timeout)
    }

    /// Task `t` answers the call with `token`, offering the capability in slot `cap`.
    pub fn reply(&mut self, t: usize, token: u32, cap: u32) -> Option<i32> {
        self.write_message(t, BUFFER, REPLY_LABEL + t as u32, [REPLY_WORD + t as u32, 0], cap);
        let result = self.syscall(t, nr::REPLY, [token, BUFFER, 0, 0]);
        if result == Some(0) {
            self.replied = Some(Replied { server: t, client: (token & 31) as usize, cap });
        }
        result
    }

    /// The reply token of the message that task `t` received last.
    pub fn token(&self, t: usize) -> u32 {
        let mut token = [0; 4];
        self.peek(t, self.k.tasks[t].msg_ptr + 8, &mut token);
        u32::from_le_bytes(token)
    }

    /// Task `t` spawns a task from the `image_len` bytes at `image` in its own memory.
    pub fn spawn_from(&mut self, t: usize, spec: &Spawn, image: u32, image_len: u32) -> Option<i32> {
        let mut args = [0; SPAWN_ARGS_SIZE];
        args[..5].copy_from_slice(b"model");
        let words = [spec.quota, spec.stack, spec.fault, FAULT_BADGE, spec.tasks, spec.eps];
        for (i, word) in words.iter().chain(&spec.caps).enumerate() {
            put32(&mut args, 12 + 4 * i, *word);
        }
        // After the six words above, the capabilities and four start arguments: the share.
        put32(&mut args, 12 + 4 * (6 + SPAWN_CAPS + 4), spec.cpu);
        self.poke(t, SPAWN_ARGS, &args);
        self.syscall(t, nr::SPAWN, [image, image_len, SPAWN_ARGS, 0])
    }

    /// Task `t` spawns a task from the `image_len` bytes at the start of its own program.
    pub fn spawn_image(&mut self, t: usize, spec: &Spawn, image_len: u32) -> Option<i32> {
        self.spawn_from(t, spec, USER_BASE, image_len)
    }

    /// Task `t` spawns a copy of itself. The result is the slot of the new task's handle.
    pub fn spawn(&mut self, t: usize, spec: &Spawn) -> Option<i32> {
        self.spawn_image(t, spec, IMAGE_SIZE)
    }

    /// The same, returning the new task's number.
    pub fn spawn_task(&mut self, t: usize, spec: &Spawn) -> Result<usize, i32> {
        match self.spawn(t, spec) {
            Some(slot) if slot >= 0 => Ok(self.child(t, slot as u32)),
            Some(error) => Err(error),
            None => panic!("a spawn left its caller blocked"),
        }
    }

    pub fn ep_create(&mut self, t: usize) -> Option<i32> {
        self.syscall(t, nr::EP_CREATE, [0; 4])
    }

    pub fn derive(&mut self, t: usize, slot: u32, a: u32, b: u32, c: u32) -> Option<i32> {
        self.syscall(t, nr::CAP_DERIVE, [slot, a, b, c])
    }

    pub fn drop_cap(&mut self, t: usize, slot: u32) -> Option<i32> {
        self.syscall(t, nr::CAP_DROP, [slot, 0, 0, 0])
    }

    pub fn revoke(&mut self, t: usize, slot: u32) -> Option<i32> {
        self.syscall(t, nr::CAP_REVOKE, [slot, 0, 0, 0])
    }

    pub fn kill(&mut self, t: usize, slot: u32) -> Option<i32> {
        self.syscall(t, nr::TASK_KILL, [slot, 0, 0, 0])
    }

    pub fn exit(&mut self, t: usize, code: u32) -> Option<i32> {
        self.syscall(t, nr::EXIT, [code, 0, 0, 0])
    }

    pub fn map(&mut self, t: usize, va: u32, pages: u32, writable: bool) -> Option<i32> {
        self.syscall(t, nr::MAP, [va, pages, writable as u32, 0])
    }

    pub fn unmap(&mut self, t: usize, va: u32, pages: u32) -> Option<i32> {
        self.syscall(t, nr::UNMAP, [va, pages, 0, 0])
    }

    /// `count` may carry `MAP_WRITE`.
    pub fn map_mem(&mut self, t: usize, slot: u32, va: u32, offset: u32, count: u32) -> Option<i32> {
        self.syscall(t, nr::MAP_MEM, [slot, va, offset, count])
    }

    pub fn sleep(&mut self, t: usize, millis: u32) -> Option<i32> {
        self.syscall(t, nr::SLEEP, [millis, 0, 0, 0])
    }

    pub fn irq_bind(&mut self, t: usize, slot: u32, irq: u32, ep_slot: u32) -> Option<i32> {
        self.syscall(t, nr::IRQ_BIND, [slot, irq, ep_slot, 0])
    }

    pub fn irq_ack(&mut self, t: usize, slot: u32, irq: u32) -> Option<i32> {
        self.syscall(t, nr::IRQ_ACK, [slot, irq, 0, 0])
    }

    /// Microseconds since boot, as task `t` is told.
    pub fn time(&mut self, t: usize) -> u64 {
        let low = self.syscall(t, nr::TIME, [0; 4]).expect("asking for the time does not block") as u32;
        (self.k.tasks[t].frame.edx as u64) << 32 | low as u64
    }

    // ---- Snapshots -----------------------------------------------------------------------

    pub fn save_base(&mut self) {
        arch::with(|machine| machine.save_base());
        self.base.take(&self.k);
    }

    pub fn restore_base(&mut self) {
        arch::with(|machine| machine.restore_base());
        self.base.put(&mut self.k);
        self.problem = None;
    }

    pub fn save_parent(&mut self) {
        arch::with(|machine| machine.save_parent());
        self.parent.take(&self.k);
    }

    pub fn restore_parent(&mut self) {
        arch::with(|machine| machine.restore_parent());
        self.parent.put(&mut self.k);
        self.problem = None;
    }

    // ---- What must hold after every step ---------------------------------------------------

    /// Check the state that the last step left behind.
    pub fn verify(&mut self) -> Result<(), String> {
        if let Some(problem) = self.problem.take() {
            return Err(problem);
        }
        let rule = self.k.check();
        if rule != 0 {
            return Err(format!("the kernel's own invariant checker reports rule {rule}"));
        }
        self.machine_agrees()?;
        self.nothing_left_waiting()?;
        self.deliveries_intact()?;
        self.reply_intact()
    }

    /// The kernel's idea of the machine and the machine itself say the same.
    fn machine_agrees(&self) -> Result<(), String> {
        let k = &*self.k;
        let cpu = arch::with(|machine| machine.cpu);
        if cpu.cr3 != k.space {
            let (used, believed) = (cpu.cr3, k.space);
            return Err(format!("the processor uses address space {used:#x}, the kernel believes {believed:#x}"));
        }
        if k.cur == IDLE {
            check(!k.tasks.iter().any(|task| task.state == State::Ready), "the processor idles while a task is ready")?;
            let known = k.space == k.mem.kernel_pd || k.tasks.iter().any(|task| task.alive() && task.pd == k.space);
            check(known, "the idle processor is left in an address space that no longer exists")?;
        } else {
            let task = &k.tasks[k.cur];
            check(task.state == State::Ready, "the running task is not ready to run")?;
            check(task.pd == k.space, "the running task is not in its own address space")?;
            let frame = &task.frame as *const TrapFrame as usize;
            check(cpu.trap_stack == frame, "the next trap would not save the registers of the running task")?;
        }
        for line in 0..IRQ_LINES {
            let open = cpu.masked & (1 << line) == 0;
            let bound = k.irqs[line].task != NONE;
            if line == 0 || line == 2 {
                check(open, "the timer or cascade interrupt line is masked")?;
            } else if open && !bound {
                return Err(format!("interrupt line {line} is open, but no task is bound to it"));
            }
        }
        let acknowledged = cpu.eois.wrapping_sub(self.eois);
        check(acknowledged == self.interrupts, "an interrupt was not acknowledged exactly once")?;
        if let Some(address) = arch::with(|machine| machine.damage_below(KERNEL_END)) {
            return Err(format!("memory below the first user frame changed after boot, at {address:#x}"));
        }
        if self.exploring {
            // A check of the model, not of the kernel: the explorer may only produce
            // deadlines that it ends itself, apart from the restart delay of the root task.
            let now = arch::with(|machine| machine.micros());
            for (t, task) in k.tasks.iter().enumerate() {
                let waits = matches!(task.state, State::Recv(_) | State::Send(_) | State::Reply(_) | State::Sleep);
                let near = waits && deadline_kind(task.deadline, now) == NEAR;
                if near && !(t == 0 && task.state == State::Sleep) {
                    return Err(format!("task {t} has a deadline within seconds, which the explorer cannot follow"));
                }
            }
        }
        Ok(())
    }

    /// No lost wake-up: a receiver never waits on an endpoint that has something for it.
    fn nothing_left_waiting(&self) -> Result<(), String> {
        for (index, endpoint) in self.k.eps.iter().enumerate() {
            if endpoint.refs == 0 || endpoint.waiter == NONE {
                continue;
            }
            let ep = index as u8;
            let queued = self.k.tasks.iter().any(|task| task.state == State::Send(ep) || task.state == State::Zombie(ep));
            if queued || endpoint.irq_pending != 0 {
                let waiter = endpoint.waiter;
                return Err(format!("task {waiter} waits on endpoint {index} although something is there to receive"));
            }
        }
        Ok(())
    }

    /// Every message that reached a receiver in this step is the message that was sent.
    fn deliveries_intact(&mut self) -> Result<(), String> {
        for r in 0..MAX_TASKS {
            let (state, gen) = self.before[r];
            let task = &self.k.tasks[r];
            let waited = matches!(state, State::Recv(_)) && task.gen == gen;
            if !(waited || self.receiver == Some(r)) || task.state != State::Ready || task.frame.eax != 0 {
                continue;
            }
            self.deliveries += 1;
            self.delivery_intact(r).map_err(|what| format!("task {r} received a wrong message: {what}"))?;
        }
        Ok(())
    }

    fn delivery_intact(&self, r: usize) -> Result<(), String> {
        let mut message = [0; MSG_HEADER + 8];
        check(self.peek(r, self.k.tasks[r].msg_ptr, &mut message), "its buffer cannot be read")?;
        let field = |at: usize| get32(&message, at);
        let (label, badge, token, len, word, cap) = (field(0), field(4), field(8), field(12), field(16), field(32));
        if label == L_FAULT {
            let cause = [14, CAUSE_EXIT, CAUSE_KILLED, CAUSE_PARENT_DIED].contains(&word);
            let whole = badge == FAULT_BADGE && token == 0 && len == 8 && cap == NO_CAP && cause;
            return check(whole, "a fault report with a wrong badge, token, length, capability or cause");
        }
        if label == L_IRQ {
            let bound = |line: &usize| {
                let bind = self.k.irqs[*line];
                bind.task != NONE && self.k.tasks[r].reaches(bind.ep, R_RECV, None)
            };
            let lines = word != 0 && word < 1 << 16 && (0..IRQ_LINES).filter(|line| word & (1 << line) != 0).all(|line| bound(&line));
            let whole = lines && badge == 0 && token == 0 && len == 0 && cap == NO_CAP;
            return check(whole, "an interrupt notice with a line that is not bound to this endpoint, or with extras");
        }
        let client = (token & 31) as usize;
        let sender = &self.k.tasks[client];
        if token >> 31 == 0 || sender.state != State::Reply(r as u8) || sender.token != token {
            return Err(format!("its reply token {token:#x} names no task that waits for this receiver"));
        }
        let whole = label == CALL_LABEL + client as u32
            && badge == field(20)
            && len == 4
            && word == CALL_WORD + client as u32
            && message[MSG_HEADER..MSG_HEADER + 4] == [client as u8 ^ 0x5A; 4];
        check(whole, "label, badge, length or contents differ from what the caller sent")?;
        let mut named = [0; 4];
        check(self.peek(client, sender.msg_ptr + 32, &mut named), "the caller's buffer cannot be read")?;
        self.cap_arrived(client, u32::from_le_bytes(named), r, cap)
    }

    /// The reply that the kernel accepted in this step reached the caller unchanged.
    fn reply_intact(&mut self) -> Result<(), String> {
        let Some(Replied { server, client, cap }) = self.replied else { return Ok(()) };
        self.replies += 1;
        let task = &self.k.tasks[client];
        let woke = task.state == State::Ready && task.frame.eax == 0;
        check(woke, "a task was replied to, but did not return from its call with success")?;
        let mut message = [0; MSG_HEADER + 4];
        check(self.peek(client, task.msg_ptr, &mut message), "the caller's buffer cannot be read")?;
        let field = |at: usize| get32(&message, at);
        let whole = field(0) == REPLY_LABEL + server as u32
            && field(4) == 0
            && field(8) == 0
            && field(12) == 4
            && field(16) == REPLY_WORD + server as u32
            && message[MSG_HEADER..] == [server as u8 ^ 0x5A; 4];
        check(whole, "a reply arrived with a wrong label, badge, token, length or contents")?;
        self.cap_arrived(server, cap, client, field(32))
    }

    /// If the sender named a capability, the receiver got a copy of exactly that one, and
    /// the copy is recorded as coming from it; if not, the receiver got none.
    fn cap_arrived(&self, sender: usize, named: u32, receiver: usize, arrived: u32) -> Result<(), String> {
        if named == NO_CAP {
            return check(arrived == NO_CAP, "a capability arrived although none was sent");
        }
        let (named, arrived) = (named as usize, arrived as usize);
        let copied = named < CAP_SLOTS
            && arrived < CAP_SLOTS
            && self.k.tasks[sender].caps[named] != Cap::Null
            && self.k.tasks[receiver].caps[arrived] == self.k.tasks[sender].caps[named]
            && self.k.tasks[receiver].from[arrived] == link(sender, named);
        check(copied, "the capability that arrived is not a copy of the one that was sent")
    }
}
