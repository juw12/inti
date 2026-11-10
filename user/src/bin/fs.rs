//! `fs`: a read-only file server over the boot archive.
//!
//! It holds two capabilities: its request endpoint and the read-only archive memory. It
//! cannot print, cannot touch a device, and keeps no state between requests, so a restart
//! loses nothing.
#![no_std]
#![no_main]

use rt::abi::archive;
use rt::abi::{Msg, FOREVER, L_KERNEL, MSG_DATA, PAGE, USER_BASE};
use rt::proto::{B_SHELL, DBG_CRASH, DBG_HANG, DENIED, ERR, FS_LIST, FS_READ, FS_STAT, OK, PING};
use rt::{io, sys};

/// Capability slot 0: receive right on the request endpoint.
const REQUESTS: u32 = 0;
/// Capability slot 1: the boot archive, read-only.
const ARCHIVE: u32 = 1;
const ARCHIVE_BASE: u32 = USER_BASE + 0x2000_0000;

fn answer(files: &[u8], msg: &Msg) -> Msg {
    let mut reply = Msg::new(ERR);
    let name = io::name(&msg.data[..(msg.len as usize).min(MSG_DATA)]);
    match msg.label {
        FS_LIST => {
            if let Some((name, data)) = archive::entry(files, msg.words[0] as usize) {
                reply.data[..name.len()].copy_from_slice(name);
                reply.len = name.len() as u32;
                reply.words[0] = data.len() as u32;
                reply.label = OK;
            }
        }
        FS_STAT => {
            if let Some(data) = archive::find(files, name) {
                reply.words[0] = data.len() as u32;
                reply.label = OK;
            }
        }
        FS_READ => {
            if let Some(data) = archive::find(files, name) {
                let offset = (msg.words[0] as usize).min(data.len());
                let count = (msg.words[1] as usize).min(MSG_DATA).min(data.len() - offset);
                reply.data[..count].copy_from_slice(&data[offset..offset + count]);
                reply.len = count as u32;
                reply.label = OK;
            }
        }
        PING => reply.label = OK,
        // Fault injection for the recovery tests, honoured only for the shell.
        DBG_CRASH if msg.badge == B_SHELL => rt::crash(),
        DBG_HANG if msg.badge == B_SHELL => loop {
            core::hint::spin_loop();
        },
        _ => reply.label = DENIED,
    }
    reply
}

#[no_mangle]
pub fn main() {
    let pages = rt::args()[2];
    if sys::map_mem(ARCHIVE, ARCHIVE_BASE, 0, pages, false).is_err() {
        sys::exit(1);
    }
    // SAFETY: the kernel just mapped `pages` read-only pages at ARCHIVE_BASE; this task
    // never unmaps them and nobody holds a writable capability for them, so the bytes
    // stay valid and unchanged for the life of this task.
    let files: &'static [u8] =
        unsafe { core::slice::from_raw_parts(ARCHIVE_BASE as *const u8, (pages * PAGE) as usize) };
    loop {
        let mut msg = Msg::new(0);
        if sys::recv(REQUESTS, &mut msg, FOREVER).is_err() {
            sys::exit(2);
        }
        if msg.label & L_KERNEL == 0 {
            let reply = answer(files, &msg);
            let _ = sys::reply(msg.token, &reply);
        }
    }
}
