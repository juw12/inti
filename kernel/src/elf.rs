//! Program loading: build an address space from a statically linked 32-bit ELF image.
//!
//! The image is untrusted input. Every field is range-checked, every read is bounds-checked,
//! and all memory the new program gets is charged to its own quota.

use crate::mem::{Mem, ADDR};
use crate::task::Kernel;
use crate::{get16, get32};
use abi::{E_BUSY, E_FAULT, E_INVAL, PAGE, STACK_TOP, USER_BASE};

/// Program segments must lie below this address; the stack sits above it.
const IMAGE_END: u32 = 0xB000_0000;
const MAX_STACK_PAGES: u32 = 256;
const PT_LOAD: u32 = 1;
const PF_WRITE: u32 = 2;

/// Where the bytes of a program image come from.
pub enum Image<'a> {
    /// The boot archive, inside the kernel's own memory.
    Kernel(&'a [u8]),
    /// `len` bytes at address `ptr` of the address space `pd` (the task that calls `spawn`).
    User { pd: u32, ptr: u32, len: u32 },
}

impl Image<'_> {
    /// Copy `out.len()` bytes starting at `offset` of the image.
    fn read(&self, mem: &Mem, offset: u32, out: &mut [u8]) -> Result<(), i32> {
        let end = offset as u64 + out.len() as u64;
        match *self {
            Image::Kernel(bytes) => {
                let source = bytes.get(offset as usize..end as usize).ok_or(E_INVAL)?;
                out.copy_from_slice(source);
                Ok(())
            }
            Image::User { pd, ptr, len } => {
                if end > len as u64 {
                    return Err(E_INVAL);
                }
                mem.copy_in(pd, ptr.checked_add(offset).ok_or(E_FAULT)?, out)
            }
        }
    }
}

impl Kernel {
    /// Give task `t` an address space holding the program `image` and a stack.
    /// Returns the entry point. On failure the caller frees whatever was built.
    pub fn load(&mut self, t: usize, image: &Image, stack_pages: u32) -> Result<u32, i32> {
        self.new_space(t)?;
        let mut header = [0; 52];
        image.read(&self.mem, 0, &mut header)?;
        // 32-bit, little-endian, version 1, executable, for the 386.
        let ident = header[..7] == [0x7F, b'E', b'L', b'F', 1, 1, 1];
        let (kind, machine) = (get16(&header, 16), get16(&header, 18));
        let (entry, table) = (get32(&header, 24), get32(&header, 28));
        let (entry_size, count) = (get16(&header, 42), get16(&header, 44));
        let sane = ident && kind == 2 && machine == 3 && entry_size == 32 && (1..=16).contains(&count);
        if !sane || !(USER_BASE..IMAGE_END).contains(&entry) {
            return Err(E_INVAL);
        }
        for index in 0..count {
            let mut segment = [0; 32];
            image.read(&self.mem, table.checked_add(index * 32).ok_or(E_INVAL)?, &mut segment)?;
            if get32(&segment, 0) == PT_LOAD {
                self.load_segment(t, image, &segment)?;
            }
        }
        if !(1..=MAX_STACK_PAGES).contains(&stack_pages) {
            return Err(E_INVAL);
        }
        for page in 1..=stack_pages {
            self.map_anon(t, STACK_TOP - page * PAGE, true)?;
        }
        Ok(entry)
    }

    /// Map one loadable segment and copy its bytes; memory beyond the file bytes stays zero.
    fn load_segment(&mut self, t: usize, image: &Image, segment: &[u8; 32]) -> Result<(), i32> {
        let (offset, start) = (get32(segment, 4), get32(segment, 8));
        let (file_size, size) = (get32(segment, 16), get32(segment, 20));
        let writable = get32(segment, 24) & PF_WRITE != 0;
        if size == 0 {
            return Ok(());
        }
        let end = start as u64 + size as u64;
        if file_size > size || start < USER_BASE || end > IMAGE_END as u64 {
            return Err(E_INVAL);
        }
        let mut page = start & ADDR;
        while (page as u64) < end {
            match self.map_anon(t, page, writable) {
                Ok(()) => {}
                // Two segments share this page; it is writable if either of them is.
                Err(E_BUSY) if writable => self.make_writable(t, page),
                Err(E_BUSY) => {}
                Err(code) => return Err(code),
            }
            page += PAGE;
        }
        let pd = self.tasks[t].pd;
        let mut done = 0;
        while done < file_size {
            let address = start + done;
            let in_page = address % PAGE;
            let piece = (file_size - done).min(PAGE - in_page).min(512);
            let mut chunk = [0; 512];
            let chunk = &mut chunk[..piece as usize];
            image.read(&self.mem, offset.checked_add(done).ok_or(E_INVAL)?, chunk)?;
            let entry = self.mem.lookup(pd, address).ok_or(E_INVAL)?;
            self.mem.write_frame(entry & ADDR, in_page, chunk);
            done += piece;
        }
        Ok(())
    }
}
