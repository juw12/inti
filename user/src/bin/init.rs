//! `init`: the root task and supervisor.
//!
//! The kernel starts exactly one task and gives it every capability that exists. `init`
//! hands each server the few it needs, starts them, and from then on does one job: when
//! a server crashes or stops answering, it starts a fresh copy with the same capabilities.
//! Clients keep their capabilities across the restart, because the endpoints belong to
//! `init`, not to the server that died.
#![no_std]
#![no_main]

use rt::abi::archive;
use rt::abi::{Msg, SpawnArgs, E_TIMEOUT, L_FAULT, L_KERNEL, NO_CAP, PAGE, R_RECV, R_SEND};
use rt::abi::{SPAWN_CAPS, USER_BASE};
use rt::io::{cause_name, errname, set_output, Hex, Sink};
use rt::proto::{B_APP, B_CLIENT, B_SHELL, DBG_CRASH, DENIED, INIT_SHUTDOWN, INIT_STATUS};
use rt::proto::{OK, PING, SERVICE_RECORD};
use rt::{outln, sys};

// The capability slots the kernel fills for the root task.
const LOG: u32 = 0;
const INFO: u32 = 1;
const PORTS: u32 = 2;
const IRQS: u32 = 3;
const VGA: u32 = 4;
const ARCHIVE: u32 = 5;

/// Where the boot archive is mapped (read-only).
const ARCHIVE_BASE: u32 = USER_BASE + 0x2000_0000;
/// Memory quota of each server, in pages.
const SERVER_PAGES: u32 = 64;
/// Pages `init` keeps for itself.
const RESERVE_PAGES: u32 = 256;
const STACK_PAGES: u32 = 8;
/// Service number `n` reports its end with fault badge `FAULT_BADGE + n`.
const FAULT_BADGE: u32 = 0x100;

const POLL_MS: u32 = 50;
const PING_EVERY_US: u64 = 200_000;
const PING_TIMEOUT_MS: u32 = 500;
/// A freshly started server is not pinged for this long.
const GRACE_US: u64 = 500_000;
/// A server that ends sooner than this after starting counts as a "quick end".
const QUICK_END_US: u64 = 2_000_000;
const RETRY_US: u64 = 100_000;

const SERVICES: usize = 3;

struct Service {
    name: &'static str,
    pages: u32,
    /// The capabilities the service starts with (slot numbers in `init`).
    caps: [u32; SPAWN_CAPS],
    /// Capability used to ping the service, or `NO_CAP` if it is not pinged.
    ping: u32,
    /// Task capability while the service runs, otherwise `NO_CAP`.
    task: u32,
    starts: u32,
    last_cause: u32,
    started_at: u64,
    ended_at: u64,
    /// Earliest time of the next start attempt.
    start_at: u64,
    respawn_us: u32,
    quick_ends: u32,
}

impl Service {
    fn new(name: &'static str, pages: u32, grants: &[u32], ping: u32) -> Service {
        let mut caps = [NO_CAP; SPAWN_CAPS];
        caps[..grants.len()].copy_from_slice(grants);
        Service {
            name,
            pages,
            caps,
            ping,
            task: NO_CAP,
            starts: 0,
            last_cause: 0,
            started_at: 0,
            ended_at: 0,
            start_at: 0,
            respawn_us: 0,
            quick_ends: 0,
        }
    }
}

struct Supervisor {
    archive: &'static [u8],
    archive_pages: u32,
    root_starts: u32,
    /// Endpoint for end-of-task notices and for requests from the shell.
    control: u32,
    last_ping: u64,
    services: [Service; SERVICES],
}

/// Unwrap a start-up step. If it fails `init` cannot do its job, so it says why and
/// exits; the kernel then starts user space again.
fn need<T>(result: Result<T, i32>, what: &str) -> T {
    match result {
        Ok(value) => value,
        Err(code) => {
            outln!("init: cannot ", what, ": ", errname(code));
            sys::exit(1)
        }
    }
}

impl Supervisor {
    fn start(&mut self, index: usize) {
        let now = sys::time();
        let service = &mut self.services[index];
        let Some(image) = archive::find(self.archive, service.name.as_bytes()) else {
            outln!("init: ", service.name, " is missing from the boot archive");
            service.start_at = now + 100 * RETRY_US;
            return;
        };
        let mut spawn = SpawnArgs::new(service.name.as_bytes(), service.pages, STACK_PAGES);
        spawn.fault_ep = self.control;
        spawn.fault_badge = FAULT_BADGE + index as u32;
        spawn.caps = service.caps;
        spawn.args = [service.starts, self.root_starts, self.archive_pages, service.pages];
        match sys::spawn(image, &spawn) {
            Ok(task) => {
                service.task = task;
                service.started_at = sys::time();
                if service.starts == 0 {
                    let granted = service.caps.iter().filter(|&&cap| cap != NO_CAP).count();
                    outln!(
                        "init: started ", service.name, " with ", granted, " capabilities, ",
                        service.pages, " pages"
                    );
                } else {
                    service.respawn_us = (service.started_at - service.ended_at) as u32;
                    outln!(
                        "init: ", service.name, " restarted after ", service.respawn_us,
                        " us (restart ", service.starts, ")"
                    );
                }
                service.starts += 1;
            }
            Err(code) => {
                outln!("init: cannot start ", service.name, ": ", errname(code));
                service.start_at = now + RETRY_US;
            }
        }
    }

    fn start_due(&mut self) {
        let now = sys::time();
        for index in 0..SERVICES {
            if self.services[index].task == NO_CAP && now >= self.services[index].start_at {
                self.start(index);
            }
        }
    }

    /// Ask every pinged server whether it is still answering; end the ones that are not.
    /// The end-of-task notice that follows restarts them like any other crash.
    fn ping_all(&mut self) {
        let now = sys::time();
        if now - self.last_ping < PING_EVERY_US {
            return;
        }
        self.last_ping = now;
        for service in &self.services {
            let settled = now - service.started_at >= GRACE_US;
            if service.ping == NO_CAP || service.task == NO_CAP || !settled {
                continue;
            }
            let mut msg = Msg::new(PING);
            if sys::call(service.ping, &mut msg, PING_TIMEOUT_MS) == Err(E_TIMEOUT) {
                outln!(
                    "init: ", service.name, " did not answer for ", PING_TIMEOUT_MS,
                    " ms; ending it"
                );
                let _ = sys::task_kill(service.task);
            }
        }
    }

    fn ended(&mut self, msg: &Msg) {
        let index = msg.badge.wrapping_sub(FAULT_BADGE) as usize;
        let Some(service) = self.services.get_mut(index) else { return };
        let now = sys::time();
        let cause = msg.words[0];
        if cause < 32 {
            outln!(
                "init: ", service.name, " crashed: ", cause_name(cause), " at address ",
                Hex(msg.words[1]), ", eip ", Hex(msg.words[2])
            );
        } else {
            outln!("init: ", service.name, " ended: ", cause_name(cause));
        }
        if service.task != NO_CAP {
            let _ = sys::cap_drop(service.task);
            service.task = NO_CAP;
        }
        service.last_cause = cause;
        service.ended_at = now;
        service.start_at = now;
        if now - service.started_at < QUICK_END_US {
            service.quick_ends += 1;
        } else {
            service.quick_ends = 0;
        }
        if service.quick_ends >= 3 {
            // A server that keeps dying right after it starts is restarted more slowly,
            // so that it cannot starve the rest of the system. It is never given up on.
            let delay_ms = (100 * (service.quick_ends - 2)).min(1000);
            outln!(
                "init: ", service.name, " ended ", service.quick_ends,
                " times in a row; next restart in ", delay_ms, " ms"
            );
            service.start_at = now + delay_ms as u64 * 1000;
        }
    }

    fn status(&self, reply: &mut Msg) {
        reply.label = OK;
        reply.words[0] = SERVICES as u32;
        for (index, service) in self.services.iter().enumerate() {
            let record = &mut reply.data[index * SERVICE_RECORD..][..SERVICE_RECORD];
            let name = service.name.as_bytes();
            let len = name.len().min(8);
            record[..len].copy_from_slice(&name[..len]);
            let running = (service.task != NO_CAP) as u32;
            let restarts = service.starts.saturating_sub(1);
            let values = [restarts, service.last_cause, service.respawn_us, running];
            for (at, value) in values.iter().enumerate() {
                record[8 + at * 4..][..4].copy_from_slice(&value.to_le_bytes());
            }
        }
        reply.len = (SERVICES * SERVICE_RECORD) as u32;
    }

    fn shutdown(&self) {
        outln!("init: shutdown requested; switching the machine off");
        // ACPI sleep state S5 on the virtual machines Inti runs on: QEMU (port 0x604) and
        // Bochs (port 0xB004).
        let _ = sys::io_out(PORTS, 0x604, 2, 0x2000);
        let _ = sys::io_out(PORTS, 0xB004, 2, 0x2000);
        sys::sleep(100);
        outln!("init: this machine has no power-off port that init knows; still running");
    }

    fn handle(&mut self, msg: &Msg) {
        if msg.label == L_FAULT {
            self.ended(msg);
            return;
        }
        if msg.label & L_KERNEL != 0 {
            return;
        }
        let mut reply = Msg::new(DENIED);
        match msg.label {
            PING => reply.label = OK,
            INIT_STATUS => self.status(&mut reply),
            INIT_SHUTDOWN if msg.badge == B_CLIENT => {
                reply.label = OK;
                let _ = sys::reply(msg.token, &reply);
                self.shutdown();
                return;
            }
            DBG_CRASH if msg.badge == B_CLIENT => rt::crash(),
            _ => {}
        }
        let _ = sys::reply(msg.token, &reply);
    }

    fn run(&mut self) -> ! {
        loop {
            let mut msg = Msg::new(0);
            match sys::recv(self.control, &mut msg, POLL_MS) {
                Ok(()) => self.handle(&msg),
                // Servers are pinged only when no notice is waiting, so that a crash
                // is always handled before `init` blocks in a ping.
                Err(E_TIMEOUT) => self.ping_all(),
                Err(code) => {
                    outln!("init: receive failed: ", errname(code));
                    sys::sleep(POLL_MS);
                }
            }
            self.start_due();
        }
    }
}

#[no_mangle]
pub fn main() {
    set_output(Sink::Log(LOG));
    let root_starts = rt::args()[1];

    let archive_cap = need(sys::cap_info(INFO, 0, ARCHIVE), "inspect the archive capability");
    let archive_pages = archive_cap.b;
    need(sys::map_mem(ARCHIVE, ARCHIVE_BASE, 0, archive_pages, false), "map the boot archive");
    // SAFETY: the kernel just mapped `archive_pages` read-only pages at ARCHIVE_BASE; they
    // are never unmapped or written (nobody holds a writable capability for them), so the
    // bytes stay valid and unchanged for the life of this task.
    let archive: &'static [u8] = unsafe {
        core::slice::from_raw_parts(ARCHIVE_BASE as *const u8, (archive_pages * PAGE) as usize)
    };

    // The shell gets what is left after the servers and a reserve. This is fixed once, so
    // a restarted shell can never take memory that a server needs to restart.
    let me = need(sys::task_info(INFO, 0), "read its own memory quota");
    let free = me.quota.saturating_sub(me.charged);
    let shell_pages = free.saturating_sub(2 * SERVER_PAGES + RESERVE_PAGES);

    let control = need(sys::ep_create(), "create the control endpoint");
    let console = need(sys::ep_create(), "create the console endpoint");
    let files = need(sys::ep_create(), "create the file endpoint");
    let derive = |slot: u32, a: u32, b: u32, c: u32| {
        need(sys::cap_derive(slot, a, b, c), "derive a capability")
    };

    // The console driver: its request endpoint, the serial port, the keyboard data port,
    // the cursor registers, two interrupt lines and the one page of screen memory.
    let console_caps = [
        derive(console, R_RECV, 0, 0),
        derive(PORTS, 0x3F8, 0x3FF, 0),
        derive(PORTS, 0x60, 0x60, 0),
        derive(PORTS, 0x3D4, 0x3D5, 0),
        derive(IRQS, (1 << 1) | (1 << 4), 0, 0),
        derive(VGA, 0, 1, 1),
    ];
    // The file server: its request endpoint and the read-only archive. Nothing else.
    let files_caps = [derive(files, R_RECV, 0, 0), ARCHIVE];
    // The shell: badged send rights to the servers for itself and for the programs it
    // starts, the read-only kernel statistics, the control endpoint of `init`, and four
    // harmless capabilities that the self-test and the fuzzer use as test objects.
    let shell_caps = [
        derive(console, R_SEND, B_SHELL, 0),
        derive(files, R_SEND, B_SHELL, 0),
        INFO,
        derive(control, R_SEND, B_CLIENT, 0),
        derive(console, R_SEND, B_APP, 0),
        derive(files, R_SEND, B_APP, 0),
        derive(PORTS, 0x80, 0x80, 0),
        ARCHIVE,
        derive(IRQS, 1 << 5, 0, 0),
        LOG,
    ];

    let mut supervisor = Supervisor {
        archive,
        archive_pages,
        root_starts,
        control,
        last_ping: 0,
        services: [
            Service::new("console", SERVER_PAGES, &console_caps, console),
            Service::new("fs", SERVER_PAGES, &files_caps, files),
            Service::new("shell", shell_pages, &shell_caps, NO_CAP),
        ],
    };
    supervisor.start_due();
    supervisor.run()
}
