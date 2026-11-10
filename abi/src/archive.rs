//! The boot archive: a flat, read-only list of named files that follows the kernel on disk.
//!
//! The kernel uses it to find `init`; the supervisor and the file server use it for
//! everything else. All three share this one parser.

use crate::{ARCHIVE_ENTRY, ARCHIVE_HEADER, ARCHIVE_MAGIC, ARCHIVE_NAME};

fn word(bytes: &[u8], at: usize) -> Option<usize> {
    let field = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([field[0], field[1], field[2], field[3]]) as usize)
}

/// Number of files in the archive, or `None` if it does not start with the magic.
pub fn count(archive: &[u8]) -> Option<usize> {
    if archive.get(..ARCHIVE_MAGIC.len())? != ARCHIVE_MAGIC {
        return None;
    }
    word(archive, ARCHIVE_MAGIC.len())
}

/// Name and contents of file number `index`.
pub fn entry(archive: &[u8], index: usize) -> Option<(&[u8], &[u8])> {
    if index >= count(archive)? {
        return None;
    }
    let at = index.checked_mul(ARCHIVE_ENTRY)?.checked_add(ARCHIVE_HEADER)?;
    let name = archive.get(at..at.checked_add(ARCHIVE_NAME)?)?;
    let name = &name[..name.iter().position(|&byte| byte == 0).unwrap_or(ARCHIVE_NAME)];
    let offset = word(archive, at + ARCHIVE_NAME)?;
    let size = word(archive, at + ARCHIVE_NAME + 4)?;
    let data = archive.get(offset..offset.checked_add(size)?)?;
    Some((name, data))
}

/// Contents of the file called `name`.
pub fn find<'a>(archive: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    (0..).map_while(|index| entry(archive, index)).find(|(found, _)| *found == name).map(|(_, data)| data)
}
