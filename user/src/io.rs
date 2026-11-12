//! Text output without `core::fmt`, which would triple the size of every program.
//!
//! `out!("free ", pages, " pages\n")` builds one line in a buffer and sends it to the
//! program's sink: the console server, the kernel log, or nowhere.

use crate::{client, sys, Single};
use abi::{CAUSE_EXIT, CAUSE_KILLED, CAUSE_PARENT_DIED, MSG_DATA};

#[derive(Clone, Copy)]
pub enum Sink {
    None,
    /// The kernel log, through the Log capability in this slot.
    Log(u32),
    /// The console server, through the send capability in this slot.
    Console(u32),
    /// The serial line through the console server for as long as it takes each line in
    /// time, and from the first line it does not take, the kernel log. For the supervisor:
    /// the serial line must have one writer at a time, or two lines cut each other in two.
    Report { console: u32, log: u32 },
}

static SINK: Single<Sink> = Single::new(Sink::None);

pub fn set_output(sink: Sink) {
    SINK.set(sink);
}

pub fn write(bytes: &[u8]) {
    for chunk in bytes.chunks(MSG_DATA) {
        match SINK.get() {
            Sink::None => {}
            Sink::Log(slot) => drop(sys::log(slot, chunk)),
            Sink::Console(ep) => drop(client::con_write(ep, chunk)),
            Sink::Report { console, log } => {
                if client::con_report(console, chunk).is_err() {
                    // The console died or did not take the line in time. The kernel log
                    // is used until the supervisor selects the console again.
                    SINK.set(Sink::Log(log));
                    let _ = sys::log(log, chunk);
                }
            }
        }
    }
}

/// An output buffer: sent when it is full and when `flush` is called.
pub struct Line {
    bytes: [u8; MSG_DATA],
    len: usize,
}

impl Line {
    pub const fn new() -> Line {
        Line { bytes: [0; MSG_DATA], len: 0 }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.len == self.bytes.len() {
                self.flush();
            }
            self.bytes[self.len] = byte;
            self.len += 1;
        }
    }

    pub fn flush(&mut self) {
        write(&self.bytes[..self.len]);
        self.len = 0;
    }
}

impl Default for Line {
    fn default() -> Line {
        Line::new()
    }
}

/// Anything that can be written to a `Line`.
pub trait Emit {
    fn emit(&self, line: &mut Line);
}

impl<T: Emit + ?Sized> Emit for &T {
    fn emit(&self, line: &mut Line) {
        (**self).emit(line)
    }
}

impl Emit for str {
    fn emit(&self, line: &mut Line) {
        line.push(self.as_bytes())
    }
}

impl Emit for [u8] {
    fn emit(&self, line: &mut Line) {
        line.push(self)
    }
}

impl<const N: usize> Emit for [u8; N] {
    fn emit(&self, line: &mut Line) {
        line.push(self)
    }
}

/// Decimal digits of `value`, right-aligned in a 20-byte field; returns the first used index.
fn digits(mut value: u64, field: &mut [u8; 20]) -> usize {
    let mut at = field.len();
    loop {
        at -= 1;
        field[at] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            return at;
        }
    }
}

impl Emit for u64 {
    fn emit(&self, line: &mut Line) {
        let mut field = [0; 20];
        let at = digits(*self, &mut field);
        line.push(&field[at..]);
    }
}

impl Emit for u32 {
    fn emit(&self, line: &mut Line) {
        (*self as u64).emit(line)
    }
}

impl Emit for usize {
    fn emit(&self, line: &mut Line) {
        (*self as u64).emit(line)
    }
}

impl Emit for i32 {
    fn emit(&self, line: &mut Line) {
        if *self < 0 {
            line.push(b"-");
        }
        (self.unsigned_abs() as u64).emit(line)
    }
}

/// A number in hexadecimal with a `0x` prefix.
pub struct Hex(pub u32);

impl Emit for Hex {
    fn emit(&self, line: &mut Line) {
        line.push(b"0x");
        let mut started = false;
        for shift in (0..8).rev() {
            let nibble = (self.0 >> (shift * 4)) & 15;
            if nibble != 0 || started || shift == 0 {
                started = true;
                line.push(&[b"0123456789abcdef"[nibble as usize]]);
            }
        }
    }
}

/// A number right-aligned in a column of the given width.
pub struct Num(pub u32, pub usize);

impl Emit for Num {
    fn emit(&self, line: &mut Line) {
        let mut field = [0; 20];
        let at = digits(self.0 as u64, &mut field);
        for _ in field.len() - at..self.1 {
            line.push(b" ");
        }
        line.push(&field[at..]);
    }
}

/// Text left-aligned in a column of the given width.
pub struct Pad<'a>(pub &'a [u8], pub usize);

impl Emit for Pad<'_> {
    fn emit(&self, line: &mut Line) {
        line.push(self.0);
        for _ in self.0.len()..self.1 {
            line.push(b" ");
        }
    }
}

#[macro_export]
macro_rules! out {
    ($($item:expr),* $(,)?) => {{
        let mut line = $crate::io::Line::new();
        $( $crate::io::Emit::emit(&$item, &mut line); )*
        line.flush();
    }};
}

#[macro_export]
macro_rules! outln {
    ($($item:expr),* $(,)?) => {
        $crate::out!($($item,)* "\n")
    };
}

/// The part of a fixed-size name field before the first NUL.
pub fn name(bytes: &[u8]) -> &[u8] {
    &bytes[..bytes.iter().position(|&byte| byte == 0).unwrap_or(bytes.len())]
}

/// The name of a system call error code.
pub fn errname(code: i32) -> &'static str {
    match code {
        abi::E_INVAL => "E_INVAL",
        abi::E_NOCAP => "E_NOCAP",
        abi::E_PERM => "E_PERM",
        abi::E_FAULT => "E_FAULT",
        abi::E_NOMEM => "E_NOMEM",
        abi::E_NOSLOT => "E_NOSLOT",
        abi::E_DEAD => "E_DEAD",
        abi::E_TIMEOUT => "E_TIMEOUT",
        abi::E_BUSY => "E_BUSY",
        abi::E_NOSYS => "E_NOSYS",
        _ => "unknown error",
    }
}

/// Why a task ended: `words[0]` of an `L_FAULT` message in words.
pub fn cause_name(cause: u32) -> &'static str {
    match cause {
        0 => "divide error",
        6 => "invalid opcode",
        7 => "device not available",
        12 => "stack fault",
        13 => "general protection fault",
        14 => "page fault",
        CAUSE_EXIT => "exit",
        CAUSE_KILLED => "killed",
        CAUSE_PARENT_DIED => "parent died",
        _ => "CPU exception",
    }
}
