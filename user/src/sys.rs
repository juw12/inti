//! The 22 system calls, as safe functions wherever the kernel makes them safe.

use abi::{nr, CapInfo, KStats, Msg, SpawnArgs, TaskInfo};
use abi::{INFO_CAP, INFO_CHECK, INFO_STATS, INFO_TASK, MAP_WRITE};
use core::arch::asm;

/// One system call with raw arguments. Returns EAX (negative = error) and EDX.
///
/// # Safety
/// The kernel checks every argument, so no call can damage the kernel or another task.
/// The caller answers for its own memory: pointers the kernel writes through must point to
/// memory the caller may overwrite, and `unmap` must not remove memory still in use.
pub unsafe fn raw(number: u32, a: u32, b: u32, c: u32, d: u32) -> (i32, u32) {
    let result: i32;
    let second: u32;
    asm!(
        "int 0x80",
        inlateout("eax") number => result,
        in("ebx") a,
        in("ecx") b,
        inlateout("edx") c => second,
        in("edi") d,
        options(nostack),
    );
    (result, second)
}

fn checked(number: u32, a: u32, b: u32, c: u32, d: u32) -> Result<(u32, u32), i32> {
    // SAFETY: private to this module. Every wrapper below passes either plain numbers or
    // pointers taken from live references of the size and mutability the call needs, and
    // none of them unmaps memory except `unmap`, which is itself `unsafe`.
    let (result, second) = unsafe { raw(number, a, b, c, d) };
    if result < 0 {
        Err(result)
    } else {
        Ok((result as u32, second))
    }
}

fn done(number: u32, a: u32, b: u32, c: u32, d: u32) -> Result<(), i32> {
    checked(number, a, b, c, d).map(drop)
}

fn address<T>(value: &T) -> u32 {
    value as *const T as u32
}

fn address_mut<T>(value: &mut T) -> u32 {
    value as *mut T as u32
}

pub fn yield_now() {
    let _ = done(nr::YIELD, 0, 0, 0, 0);
}

pub fn exit(code: u32) -> ! {
    let _ = done(nr::EXIT, code, 0, 0, 0);
    loop {
        core::hint::spin_loop();
    }
}

/// Send `msg` to an endpoint and wait up to `timeout_ms` for the reply, which replaces it.
pub fn call(ep: u32, msg: &mut Msg, timeout_ms: u32) -> Result<(), i32> {
    done(nr::CALL, ep, address_mut(msg), timeout_ms, 0)
}

/// Wait up to `timeout_ms` for the next message on an endpoint (0 = only look).
pub fn recv(ep: u32, msg: &mut Msg, timeout_ms: u32) -> Result<(), i32> {
    done(nr::RECV, ep, address_mut(msg), timeout_ms, 0)
}

/// Answer the call that arrived with `token`.
pub fn reply(token: u32, msg: &Msg) -> Result<(), i32> {
    done(nr::REPLY, token, address(msg), 0, 0)
}

/// Create an endpoint; returns the slot of a capability with all rights.
pub fn ep_create() -> Result<u32, i32> {
    checked(nr::EP_CREATE, 0, 0, 0, 0).map(|(slot, _)| slot)
}

/// Copy a capability with fewer rights or a smaller range; returns the new slot.
pub fn cap_derive(slot: u32, a: u32, b: u32, c: u32) -> Result<u32, i32> {
    checked(nr::CAP_DERIVE, slot, a, b, c).map(|(slot, _)| slot)
}

pub fn cap_drop(slot: u32) -> Result<(), i32> {
    done(nr::CAP_DROP, slot, 0, 0, 0)
}

/// Take back every copy that was made of a capability, in every task. The capability
/// itself stays.
pub fn cap_revoke(slot: u32) -> Result<(), i32> {
    done(nr::CAP_REVOKE, slot, 0, 0, 0)
}

/// Map `pages` fresh, zeroed pages at `va`. Fails with `E_BUSY` if anything is mapped there.
pub fn map(va: u32, pages: u32, writable: bool) -> Result<(), i32> {
    done(nr::MAP, va, pages, writable as u32, 0)
}

/// # Safety
/// Nothing may refer to the unmapped pages afterwards.
pub unsafe fn unmap(va: u32, pages: u32) -> Result<(), i32> {
    done(nr::UNMAP, va, pages, 0, 0)
}

/// Map `pages` pages of a Memory capability, starting `offset` pages into it, at `va`.
pub fn map_mem(slot: u32, va: u32, offset: u32, pages: u32, writable: bool) -> Result<(), i32> {
    let flag = if writable { MAP_WRITE } else { 0 };
    done(nr::MAP_MEM, slot, va, offset, pages | flag)
}

/// Read an I/O port (`width` 1, 2 or 4 bytes) through an IoPort capability.
pub fn io_in(slot: u32, port: u32, width: u32) -> Result<u32, i32> {
    checked(nr::IO_IN, slot, port, width, 0).map(|(_, value)| value)
}

pub fn io_out(slot: u32, port: u32, width: u32, value: u32) -> Result<(), i32> {
    done(nr::IO_OUT, slot, port, width, value)
}

/// Deliver interrupt line `irq` as `L_IRQ` messages on the endpoint in `ep_slot`.
pub fn irq_bind(irq_slot: u32, irq: u32, ep_slot: u32) -> Result<(), i32> {
    done(nr::IRQ_BIND, irq_slot, irq, ep_slot, 0)
}

/// Re-enable an interrupt line after handling it.
pub fn irq_ack(irq_slot: u32, irq: u32) -> Result<(), i32> {
    done(nr::IRQ_ACK, irq_slot, irq, 0, 0)
}

/// Start a new task from an ELF image; returns the slot of its Task capability.
pub fn spawn(image: &[u8], args: &SpawnArgs) -> Result<u32, i32> {
    checked(nr::SPAWN, image.as_ptr() as u32, image.len() as u32, address(args), 0).map(|(slot, _)| slot)
}

pub fn task_kill(slot: u32) -> Result<(), i32> {
    done(nr::TASK_KILL, slot, 0, 0, 0)
}

pub fn sleep(ms: u32) {
    let _ = done(nr::SLEEP, ms, 0, 0, 0);
}

/// Microseconds since boot.
pub fn time() -> u64 {
    // SAFETY: `time` takes no pointers and changes nothing. It bypasses `checked` because
    // its low word is a full 32-bit value, not an error code.
    let (low, high) = unsafe { raw(nr::TIME, 0, 0, 0, 0) };
    (high as u64) << 32 | low as u32 as u64
}

/// Write up to 256 bytes to the kernel log (the serial port) through a Log capability.
pub fn log(slot: u32, text: &[u8]) -> Result<(), i32> {
    done(nr::LOG, slot, text.as_ptr() as u32, text.len() as u32, 0)
}

pub fn stats(slot: u32) -> Result<KStats, i32> {
    let mut stats = KStats::ZERO;
    done(nr::SYSINFO, slot, INFO_STATS, 0, address_mut(&mut stats))?;
    Ok(stats)
}

pub fn task_info(slot: u32, index: u32) -> Result<TaskInfo, i32> {
    let mut info = TaskInfo::ZERO;
    done(nr::SYSINFO, slot, INFO_TASK, index, address_mut(&mut info))?;
    Ok(info)
}

/// Describe capability `cap` of task `task`.
pub fn cap_info(slot: u32, task: u32, cap: u32) -> Result<CapInfo, i32> {
    let mut info = CapInfo::ZERO;
    done(nr::SYSINFO, slot, INFO_CAP, task << 8 | cap, address_mut(&mut info))?;
    Ok(info)
}

/// Run the kernel's invariant checker: 0, or the number of the first violated invariant.
pub fn check(slot: u32) -> Result<u32, i32> {
    checked(nr::SYSINFO, slot, INFO_CHECK, 0, 0).map(|(violated, _)| violated)
}
