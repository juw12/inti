//! bench: how long the kernel's basic operations take.
//!
//! Every operation is repeated many times, three runs each, and the fastest run is
//! reported as `BENCH: <name>: <n> ns` per operation. The times come from the guest's own
//! clock, so under an emulator they describe the emulator as much as the kernel.
//!
//! Capabilities, handed over by the shell: 0 console, 1 files, 2 Info (the others that a
//! test program gets are not used).
#![no_std]
#![no_main]

use rt::abi::{nr, Msg, SpawnArgs, L_FAULT, R_RECV};
use rt::client::{self, Mapping};
use rt::io::{set_output, Sink};
use rt::proto::{self, mode};
use rt::{outln, sys};

const CONSOLE: u32 = 0;
const FILES: u32 = 1;
const INFO: u32 = 2;

/// Where the `tchild` program is loaded, and where the page for the mapping test goes.
const CHILD_AT: u32 = 0x5000_0000;
const PAGE_AT: u32 = 0x6000_0000;
/// Memory quota and stack of a child, in pages.
const CHILD_PAGES: u32 = 48;
const CHILD_STACK: u32 = 4;
/// Each measurement is the fastest of this many runs.
const RUNS: u32 = 3;
/// How long to wait for a child before giving up on it.
const PATIENCE_MS: u32 = 2000;

/// Times `rounds` repetitions of `step` and prints the time for one.
fn measure(name: &str, rounds: u32, mut step: impl FnMut() -> bool) {
    let mut best = u64::MAX;
    let mut ok = true;
    for _ in 0..RUNS {
        let start = sys::time();
        for _ in 0..rounds {
            ok &= step();
        }
        best = best.min(sys::time() - start);
    }
    if ok {
        outln!("BENCH: ", name, ": ", best * 1000 / u64::from(rounds), " ns");
    } else {
        outln!("BENCH: ", name, ": FAILED");
    }
}

/// Starts `tchild` and waits for children to end.
struct Children {
    image: Mapping,
    /// The endpoint on which the kernel reports the end of every child.
    notices: u32,
}

impl Children {
    fn start(&self, what: u32, caps: &[u32]) -> Result<u32, i32> {
        let mut spawn = SpawnArgs::new(b"tchild", CHILD_PAGES, CHILD_STACK);
        spawn.fault_ep = self.notices;
        spawn.fault_badge = 1;
        spawn.args = [what, 0, 0, 0];
        spawn.caps[..caps.len()].copy_from_slice(caps);
        sys::spawn(self.image.bytes(), &spawn)
    }

    /// Waits for a child to end and gives up its Task capability.
    fn reap(&self, task: u32) -> bool {
        let mut msg = Msg::new(0);
        let ended = sys::recv(self.notices, &mut msg, PATIENCE_MS).is_ok() && msg.label == L_FAULT;
        sys::cap_drop(task).is_ok() && ended
    }
}

/// A call to a server in another address space and its reply, with an empty message.
fn round_trips(children: &Children) -> Option<()> {
    let ep = sys::ep_create().ok()?;
    let server_end = sys::cap_derive(ep, R_RECV, 0, 0).ok()?;
    let server = children.start(mode::ECHO, &[server_end]).ok()?;
    let mut msg = Msg::new(0);
    measure("IPC call and reply", 20_000, || {
        msg.label = 0;
        msg.len = 0;
        msg.words[1] = 0;
        sys::call(ep, &mut msg, PATIENCE_MS).is_ok()
    });
    let mut stop = Msg::new(proto::ECHO_STOP);
    let _ = sys::call(ep, &mut stop, PATIENCE_MS);
    let ended = children.reap(server);
    let _ = sys::cap_drop(server_end);
    let _ = sys::cap_drop(ep);
    ended.then_some(())
}

#[no_mangle]
pub fn main() {
    set_output(Sink::Console(CONSOLE));
    outln!("Times are read from the guest's clock. Under an emulator they mostly");
    outln!("describe the emulator; compare them with each other, not with real hardware.");
    let children = client::load(FILES, b"tchild", CHILD_AT)
        .and_then(|image| Some(Children { image, notices: sys::ep_create().ok()? }));
    let Some(children) = children else {
        outln!("bench: could not set up (is tchild missing?)");
        return;
    };

    measure("system call entry and return", 100_000, || {
        // SAFETY: a system call number that does not exist; the kernel refuses it without
        // looking at the arguments.
        unsafe { sys::raw(nr::COUNT, 0, 0, 0, 0) };
        true
    });
    measure("read the clock", 20_000, || {
        sys::time();
        true
    });
    if round_trips(&children).is_none() {
        outln!("BENCH: IPC call and reply: FAILED");
    }
    measure("start, run and reap a task", 200, || {
        children.start(mode::EXIT, &[]).is_ok_and(|task| children.reap(task))
    });
    measure("map and unmap one page", 5_000, || {
        // SAFETY: nothing refers to the page: it is mapped and unmapped without being used.
        sys::map(PAGE_AT, 1, true).is_ok() && unsafe { sys::unmap(PAGE_AT, 1) }.is_ok()
    });
    measure("check every kernel invariant", 20, || sys::check(INFO) == Ok(0));
    outln!("BENCH: done");
}
