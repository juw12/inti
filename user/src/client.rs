//! Client side of the server protocols, with retry across server restarts.

use crate::proto::{BUSY, CON_READLINE, CON_WRITE, FS_LIST, FS_READ, FS_STAT, OK};
use crate::sys;
use abi::{Msg, ARCHIVE_NAME, E_DEAD, FOREVER, MAX_TASKS, MSG_DATA, PAGE, ST_FREE, ST_ZOMBIE};

/// The number of the first running task called `name`, read through an Info capability. The
/// test programs use it to look up their own entry in the task table; an ended task that
/// still waits to be noticed keeps its name, so it is skipped.
pub fn task_named(info: u32, name: &[u8]) -> Option<u32> {
    (0..MAX_TASKS as u32).find(|&index| {
        sys::task_info(info, index)
            .is_ok_and(|task| {
                !matches!(task.state, ST_FREE | ST_ZOMBIE) && crate::io::name(&task.name) == name
            })
    })
}

/// Send a request and wait for the reply. If the server dies while it holds the request,
/// the kernel returns `E_DEAD` and leaves the message untouched, so it is simply sent again
/// to the restarted server. A request can therefore be handled more than once.
pub fn request(ep: u32, msg: &mut Msg) -> Result<(), i32> {
    let mut retries = 0;
    loop {
        match sys::call(ep, msg, FOREVER) {
            Err(E_DEAD) if retries < 8 => {
                retries += 1;
                sys::sleep(2);
            }
            result => return result,
        }
    }
}

fn with_data(label: u32, data: &[u8]) -> Msg {
    let mut msg = Msg::new(label);
    let len = data.len().min(MSG_DATA);
    msg.data[..len].copy_from_slice(&data[..len]);
    msg.len = len as u32;
    msg
}

/// Print up to 256 bytes on the console.
pub fn con_write(con: u32, bytes: &[u8]) -> Result<(), i32> {
    request(con, &mut with_data(CON_WRITE, bytes))
}

/// Show `prompt` and read one line into `buf`; returns its length. The prompt travels with
/// the request so that a restarted console can show it again.
pub fn readline(con: u32, prompt: &[u8], buf: &mut [u8]) -> usize {
    loop {
        let mut msg = with_data(CON_READLINE, prompt);
        if request(con, &mut msg).is_err() {
            return 0;
        }
        if msg.label == BUSY {
            sys::sleep(20);
            continue;
        }
        let len = (msg.len as usize).min(buf.len());
        buf[..len].copy_from_slice(&msg.data[..len]);
        return len;
    }
}

/// Size of the file called `name`.
pub fn stat(fs: u32, name: &[u8]) -> Option<u32> {
    let mut msg = with_data(FS_STAT, name);
    request(fs, &mut msg).ok()?;
    (msg.label == OK).then_some(msg.words[0])
}

/// Read up to 256 bytes of a file from `offset` into `buf`; returns the count.
pub fn read(fs: u32, name: &[u8], offset: u32, buf: &mut [u8]) -> Option<usize> {
    let mut msg = with_data(FS_READ, name);
    msg.words = [offset, buf.len().min(MSG_DATA) as u32, 0, 0];
    request(fs, &mut msg).ok()?;
    if msg.label != OK {
        return None;
    }
    let len = (msg.len as usize).min(buf.len());
    buf[..len].copy_from_slice(&msg.data[..len]);
    Some(len)
}

/// Name and size of file number `index`, or `None` past the last file.
pub fn list(fs: u32, index: u32) -> Option<([u8; ARCHIVE_NAME], u32)> {
    let mut msg = Msg::new(FS_LIST);
    msg.words[0] = index;
    request(fs, &mut msg).ok()?;
    if msg.label != OK {
        return None;
    }
    let mut name = [0; ARCHIVE_NAME];
    let len = (msg.len as usize).min(ARCHIVE_NAME);
    name[..len].copy_from_slice(&msg.data[..len]);
    Some((name, msg.words[0]))
}

/// A whole file read into freshly mapped pages, unmapped again when dropped.
pub struct Mapping {
    base: u32,
    pages: u32,
    len: usize,
}

impl Mapping {
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: `load` mapped `pages` zero-filled pages at `base`, `len` bytes fit in
        // them, and they stay mapped until this `Mapping` is dropped.
        unsafe { core::slice::from_raw_parts(self.base as *const u8, self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: the only reference into these pages is the slice handed out by `bytes`,
        // which cannot outlive `self`.
        let _ = unsafe { sys::unmap(self.base, self.pages) };
    }
}

/// Read the file called `name` into new pages at address `base`.
pub fn load(fs: u32, name: &[u8], base: u32) -> Option<Mapping> {
    let size = stat(fs, name)?;
    let pages = size.div_ceil(PAGE).max(1);
    sys::map(base, pages, true).ok()?;
    let mapping = Mapping { base, pages, len: size as usize };
    // SAFETY: the pages were mapped writable just above and nothing else refers to them yet.
    let target = unsafe { core::slice::from_raw_parts_mut(base as *mut u8, size as usize) };
    let mut offset = 0;
    while offset < target.len() {
        let count = read(fs, name, offset as u32, &mut target[offset..])?;
        if count == 0 {
            return None;
        }
        offset += count;
    }
    Some(mapping)
}
