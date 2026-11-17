//! Client side of the server protocols, with retry across server restarts.

use crate::proto::{BUSY, CON_READLINE, CON_REPORT, CON_WRITE, FS_LIST, FS_READ, FS_STAT, OK};
use crate::proto::{DISK_APPEND, DISK_COMMIT, DISK_CREATE, DISK_DELETE, DISK_INFO, DISK_WRITE};
use crate::proto::{D_BAD_NAME, D_DAMAGED, D_DENIED, D_DIR_FULL, D_FULL, D_GONE, D_IO, D_MOVED};
use crate::proto::{D_NO_DRIVE, D_NO_FILE, D_NO_VOLUME, D_STALE, ERR};
use crate::proto::{WIN_CLOSE, WIN_FILL, WIN_OPEN, WIN_TEXT};
use crate::sys;
use abi::{Msg, ARCHIVE_NAME, NO_CAP, E_DEAD, E_NOCAP, E_PERM, FOREVER, MAX_TASKS, MSG_DATA, PAGE, ST_FREE};
use abi::ST_ZOMBIE;

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

/// How long the supervisor waits for the console to take one line of a report.
pub const REPORT_WAIT_MS: u32 = 100;

/// Write up to 256 bytes to the serial line through the console server: one attempt with
/// a time limit and no second try. This is for the supervisor, which must never wait
/// long for a server that it supervises.
pub fn con_report(con: u32, bytes: &[u8]) -> Result<(), i32> {
    let mut msg = with_data(CON_REPORT, bytes);
    sys::call(con, &mut msg, REPORT_WAIT_MS)?;
    if msg.label == OK {
        Ok(())
    } else {
        Err(E_PERM)
    }
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

/// Why a disk request failed, in words.
pub fn disk_error(reason: u32) -> &'static str {
    match reason {
        D_NO_DRIVE => "no ATA disk found",
        D_NO_VOLUME => "no Inti volume on the disk",
        D_DAMAGED => "the Inti volume is damaged",
        D_NO_FILE => "no such file",
        D_FULL => "the disk is full",
        D_DIR_FULL => "the directory is full",
        D_BAD_NAME => "bad file name (1 to 23 of A-Z a-z 0-9 . _ -)",
        D_STALE => "another writer started first",
        D_MOVED => "the file changed in the meantime",
        D_IO => "the disk reported an error",
        D_DENIED => "no capability for the disk",
        D_GONE => "the disk server does not answer",
        _ => "unknown error",
    }
}

/// One request to the disk server. Like `request`, but it also tells whether the request
/// had to be sent again because the server died while it held it. Such a request may have
/// been carried out by the server that died.
fn disk_request(disk: u32, msg: &mut Msg) -> Result<bool, u32> {
    let mut resent = false;
    let mut retries = 0;
    loop {
        match sys::call(disk, msg, FOREVER) {
            Ok(()) => break,
            Err(E_DEAD) if retries < 8 => {
                resent = true;
                retries += 1;
                sys::sleep(2);
            }
            Err(E_PERM | E_NOCAP) => return Err(D_DENIED),
            Err(_) => return Err(D_GONE),
        }
    }
    match msg.label {
        OK => Ok(resent),
        ERR if resent && msg.words[1] != 0 => Err(msg.words[1] | RESENT),
        ERR if msg.words[1] != 0 => Err(msg.words[1]),
        _ => Err(D_DENIED),
    }
}

/// Marks, inside this file only, the error of a request that was sent twice.
const RESENT: u32 = 1 << 31;

/// Start a new version of a file on the disk; returns the transaction number for `write`
/// and `commit`. Readers see the old version until `commit` succeeds.
pub fn create(disk: u32, name: &[u8]) -> Result<u32, u32> {
    let mut msg = with_data(DISK_CREATE, name);
    disk_request(disk, &mut msg).map_err(|reason| reason & !RESENT)?;
    Ok(msg.words[0])
}

/// Add bytes to the new version of a file.
pub fn write(disk: u32, transaction: u32, bytes: &[u8]) -> Result<(), u32> {
    for chunk in bytes.chunks(MSG_DATA) {
        let mut msg = with_data(DISK_WRITE, chunk);
        msg.words[0] = transaction;
        disk_request(disk, &mut msg).map_err(|reason| reason & !RESENT)?;
    }
    Ok(())
}

/// Replace the file by its new version, in one step.
pub fn commit(disk: u32, transaction: u32) -> Result<(), u32> {
    let mut msg = Msg::new(DISK_COMMIT);
    msg.words[0] = transaction;
    disk_request(disk, &mut msg).map_err(|reason| reason & !RESENT)?;
    Ok(())
}

/// Write a whole file. After a power cut or a crash of the disk server the file is either
/// the old version or this one, never a mixture. If the server is restarted on the way, or
/// another writer cuts in, everything is sent again.
pub fn save(disk: u32, name: &[u8], bytes: &[u8]) -> Result<(), u32> {
    let mut tries = 0;
    loop {
        let sent = create(disk, name).and_then(|transaction| {
            write(disk, transaction, bytes)?;
            commit(disk, transaction)
        });
        match sent {
            Err(D_STALE) if tries < 3 => tries += 1,
            result => return result,
        }
    }
}

/// Add bytes to the end of a file, creating it if needed. Each piece of up to 256 bytes
/// minus the length of the name is added in one step: after a power cut the file ends
/// before or after a piece, never inside one.
pub fn append(disk: u32, name: &[u8], bytes: &[u8]) -> Result<(), u32> {
    if name.is_empty() || name.len() >= ARCHIVE_NAME {
        return Err(D_BAD_NAME);
    }
    let mut size = stat(disk, name).unwrap_or(0);
    for chunk in bytes.chunks(MSG_DATA - name.len()) {
        let mut msg = with_data(DISK_APPEND, name);
        msg.data[name.len()..name.len() + chunk.len()].copy_from_slice(chunk);
        msg.len = (name.len() + chunk.len()) as u32;
        msg.words = [name.len() as u32, size, 0, 0];
        size += chunk.len() as u32;
        match disk_request(disk, &mut msg) {
            Ok(_) => {}
            // The server that died had already added this piece.
            Err(reason) if reason == D_MOVED | RESENT && msg.words[0] == size => {}
            Err(reason) => return Err(reason & !RESENT),
        }
    }
    Ok(())
}

/// Remove a file from the disk.
pub fn remove(disk: u32, name: &[u8]) -> Result<(), u32> {
    let mut msg = with_data(DISK_DELETE, name);
    match disk_request(disk, &mut msg) {
        Ok(_) => Ok(()),
        // The server that died had already removed it.
        Err(reason) if reason == D_NO_FILE | RESENT => Ok(()),
        Err(reason) => Err(reason & !RESENT),
    }
}

/// Data blocks, free blocks, number of files and the microseconds the disk server's
/// start-up took; or the reason why there is no volume.
pub fn disk_info(disk: u32) -> Result<[u32; 4], u32> {
    let mut msg = Msg::new(DISK_INFO);
    disk_request(disk, &mut msg).map_err(|reason| reason & !RESENT)?;
    Ok(msg.words)
}

/// Open a window whose inside is `width` by `height` pixels; returns the slot of the
/// capability for it. Whoever holds that capability may draw in the window, and nobody
/// else; the display closes the window when the last holder is gone.
pub fn win_open(display: u32, width: u32, height: u32, title: &[u8]) -> Option<u32> {
    let mut msg = with_data(WIN_OPEN, title);
    msg.words = [width, height, 0, 0];
    request(display, &mut msg).ok()?;
    (msg.label == OK && msg.cap != NO_CAP).then_some(msg.cap)
}

/// Draw text at `x`, `y` inside a window, in colour `text` on colour `background`. Returns
/// false when the window no longer exists, for example because the display was restarted;
/// the program then opens a new one.
pub fn win_text(window: u32, x: u32, y: u32, text: u8, background: u8, bytes: &[u8]) -> bool {
    let mut msg = with_data(WIN_TEXT, bytes);
    msg.words = [x, y, text as u32 | (background as u32) << 4, 0];
    request(window, &mut msg).is_ok() && msg.label == OK
}

/// Fill a rectangle inside a window; returns false when the window no longer exists.
pub fn win_fill(window: u32, x: u32, y: u32, width: u32, height: u32, colour: u8) -> bool {
    let mut msg = Msg::new(WIN_FILL);
    msg.words = [x, y, (width & 0xFFFF) | height << 16, colour as u32];
    request(window, &mut msg).is_ok() && msg.label == OK
}

/// Close a window and give its capability up.
pub fn win_close(window: u32) {
    let _ = request(window, &mut Msg::new(WIN_CLOSE));
    let _ = sys::cap_drop(window);
}
