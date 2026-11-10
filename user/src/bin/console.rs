//! `console`: the screen, keyboard and serial-port driver, running as an ordinary task.
//!
//! In a monolithic kernel this code would run with full privileges and a bug in it could
//! overwrite anything. Here it holds six capabilities (its request endpoint, three small
//! port ranges, two interrupt lines and one page of screen memory); when it crashes or
//! hangs, `init` starts a new copy and the callers simply send their request again.
#![no_std]
#![no_main]

use rt::abi::{Msg, FOREVER, L_IRQ, L_KERNEL, MSG_DATA, USER_BASE};
use rt::proto::{BUSY, B_SHELL, CON_INTR, CON_READLINE, CON_WRITE, DBG_CRASH, DBG_HANG, DENIED};
use rt::proto::{OK, PING};
use rt::sys;

// Capability slots, filled by `init`.
const REQUESTS: u32 = 0;
const SERIAL: u32 = 1;
const KEYBOARD: u32 = 2;
const CURSOR: u32 = 3;
const IRQS: u32 = 4;
const SCREEN: u32 = 5;

const SCREEN_BASE: u32 = USER_BASE + 0x3000_0000;
const COLS: usize = 80;
const ROWS: usize = 25;
const CELLS: usize = COLS * ROWS;
/// Light grey on black.
const COLOUR: u16 = 0x0700;

const COM1: u32 = 0x3F8;
const KEYBOARD_PORT: u32 = 0x60;
const CRTC_INDEX: u32 = 0x3D4;
const CRTC_DATA: u32 = 0x3D5;
const KEYBOARD_IRQ: u32 = 1;
const SERIAL_IRQ: u32 = 4;
/// How often the transmitter is polled before a byte is sent regardless.
const SERIAL_TRIES: u32 = 20_000;

const QUEUE: usize = 128;
const LINE_MAX: usize = 120;
const CTRL_C: u8 = 3;

/// Scancode set 1, keys 0x00..=0x39, without and with Shift. Zero means "no character".
static PLAIN: &[u8; 58] =
    b"\x00\x1b1234567890-=\x08\tqwertyuiop[]\r\x00asdfghjkl;'`\x00\\zxcvbnm,./\x00*\x00 ";
static SHIFTED: &[u8; 58] =
    b"\x00\x1b!@#$%^&*()_+\x08\tQWERTYUIOP{}\r\x00ASDFGHJKL:\"~\x00|ZXCVBNM<>?\x00*\x00 ";

fn port_in(slot: u32, port: u32) -> u32 {
    sys::io_in(slot, port, 1).unwrap_or(0)
}

fn port_out(slot: u32, port: u32, value: u32) {
    let _ = sys::io_out(slot, port, 1, value);
}

fn cell_write(index: usize, value: u16) {
    if index < CELLS {
        // SAFETY: `main` mapped the screen page writable at SCREEN_BASE before any call
        // and never unmaps it; `index` is inside its 2000 two-byte cells; no Rust
        // reference covers this device memory, so a volatile store cannot break aliasing.
        unsafe { ((SCREEN_BASE as usize + index * 2) as *mut u16).write_volatile(value) }
    }
}

fn cell_read(index: usize) -> u16 {
    if index >= CELLS {
        return COLOUR | b' ' as u16;
    }
    // SAFETY: as in `cell_write`: mapped for the life of the task, in bounds, unaliased.
    unsafe { ((SCREEN_BASE as usize + index * 2) as *const u16).read_volatile() }
}

struct Reader {
    token: u32,
    badge: u32,
}

struct Console {
    cursor: usize,
    cursor_moved: bool,
    shift: bool,
    ctrl: bool,
    extended: bool,
    /// Bytes typed while nobody was reading a line.
    queue: [u8; QUEUE],
    queue_head: usize,
    queue_len: usize,
    /// The caller waiting for a line, if any, and the line typed so far.
    reader: Option<Reader>,
    line: [u8; LINE_MAX],
    line_len: usize,
    after_return: bool,
    interrupted: bool,
}

impl Console {
    fn scroll(&mut self) {
        for index in 0..CELLS - COLS {
            cell_write(index, cell_read(index + COLS));
        }
        for index in CELLS - COLS..CELLS {
            cell_write(index, COLOUR | b' ' as u16);
        }
        self.cursor -= COLS;
    }

    fn clear(&mut self) {
        for index in 0..CELLS {
            cell_write(index, COLOUR | b' ' as u16);
        }
        self.cursor = 0;
        self.cursor_moved = true;
    }

    fn screen_put(&mut self, byte: u8) {
        match byte {
            b'\n' => self.cursor = (self.cursor / COLS + 1) * COLS,
            b'\r' => self.cursor -= self.cursor % COLS,
            8 => self.cursor = self.cursor.saturating_sub(1),
            b'\t' => self.cursor = (self.cursor / 8 + 1) * 8,
            32..=126 => {
                cell_write(self.cursor, COLOUR | byte as u16);
                self.cursor += 1;
            }
            _ => return,
        }
        while self.cursor >= CELLS {
            self.scroll();
        }
        self.cursor_moved = true;
    }

    fn serial_put(&mut self, byte: u8) {
        for _ in 0..SERIAL_TRIES {
            if port_in(SERIAL, COM1 + 5) & 0x20 != 0 {
                break;
            }
        }
        port_out(SERIAL, COM1, byte as u32);
    }

    fn print(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.screen_put(byte);
            if byte == b'\n' {
                self.serial_put(b'\r');
            }
            self.serial_put(byte);
        }
    }

    /// Move the hardware cursor to where the next character will appear.
    fn show_cursor(&mut self) {
        if self.cursor_moved {
            self.cursor_moved = false;
            port_out(CURSOR, CRTC_INDEX, 14);
            port_out(CURSOR, CRTC_DATA, (self.cursor >> 8) as u32);
            port_out(CURSOR, CRTC_INDEX, 15);
            port_out(CURSOR, CRTC_DATA, (self.cursor & 0xFF) as u32);
        }
    }

    /// Continue on the first row below everything the previous console left on the screen.
    /// Its hardware cursor is no guide: it may have been stopped before it moved the cursor.
    fn continue_below_text(&mut self) {
        let mut used = CELLS;
        while used > 0 && cell_read(used - 1) as u8 == b' ' {
            used -= 1;
        }
        self.cursor = used.div_ceil(COLS) * COLS;
        while self.cursor >= CELLS {
            self.scroll();
        }
        self.cursor_moved = true;
    }

    /// The character for a keyboard scancode, if it produces one.
    fn translate(&mut self, code: u8) -> Option<u8> {
        if code == 0xE0 {
            self.extended = true;
            return None;
        }
        if core::mem::replace(&mut self.extended, false) {
            return None; // arrow keys and the like: not used
        }
        let pressed = code & 0x80 == 0;
        match code & 0x7F {
            0x2A | 0x36 => self.shift = pressed,
            0x1D => self.ctrl = pressed,
            key if pressed => {
                let table = if self.shift { SHIFTED } else { PLAIN };
                let character = *table.get(key as usize)?;
                if character == 0 {
                    return None;
                }
                if self.ctrl && character.is_ascii_alphabetic() {
                    return Some(character & 0x1F);
                }
                return Some(character);
            }
            _ => {}
        }
        None
    }

    fn answer_line(&mut self, reader: Reader, label: u32) {
        let mut reply = Msg::new(label);
        if label == OK {
            reply.data[..self.line_len].copy_from_slice(&self.line[..self.line_len]);
            reply.len = self.line_len as u32;
        }
        self.line_len = 0;
        // The reader may have died while waiting; then its token is stale and the reply
        // fails, which is fine.
        let _ = sys::reply(reader.token, &reply);
    }

    /// One byte of the line being typed for the waiting reader.
    fn edit(&mut self, byte: u8) {
        let after_return = core::mem::replace(&mut self.after_return, byte == b'\r');
        match byte {
            b'\n' if after_return => {}
            b'\r' | b'\n' => {
                self.print(b"\n");
                if let Some(reader) = self.reader.take() {
                    self.answer_line(reader, OK);
                }
            }
            8 | 127 => {
                if self.line_len > 0 {
                    self.line_len -= 1;
                    self.print(b"\x08 \x08");
                }
            }
            32..=126 => {
                if self.line_len < LINE_MAX {
                    self.line[self.line_len] = byte;
                    self.line_len += 1;
                    self.print(&[byte]);
                }
            }
            _ => {}
        }
    }

    /// One byte from the keyboard or the serial port.
    fn input(&mut self, byte: u8) {
        if byte == CTRL_C {
            self.interrupted = true;
            self.queue_len = 0;
            if let Some(reader) = self.reader.take() {
                self.print(b"^C\n");
                self.line_len = 0;
                self.answer_line(reader, OK);
            }
        } else if self.reader.is_some() {
            self.edit(byte);
        } else if self.queue_len < QUEUE {
            self.queue[(self.queue_head + self.queue_len) % QUEUE] = byte;
            self.queue_len += 1;
        }
    }

    fn drain_serial(&mut self) {
        for _ in 0..64 {
            if port_in(SERIAL, COM1 + 5) & 1 == 0 {
                break;
            }
            let byte = port_in(SERIAL, COM1) as u8;
            self.input(byte);
        }
    }

    fn interrupts(&mut self, pending: u32) {
        if pending & (1 << KEYBOARD_IRQ) != 0 {
            let code = port_in(KEYBOARD, KEYBOARD_PORT) as u8;
            if let Some(byte) = self.translate(code) {
                self.input(byte);
            }
            let _ = sys::irq_ack(IRQS, KEYBOARD_IRQ);
        }
        if pending & (1 << SERIAL_IRQ) != 0 {
            self.drain_serial();
            let _ = sys::irq_ack(IRQS, SERIAL_IRQ);
        }
    }

    fn read_line(&mut self, msg: &Msg) {
        if let Some(waiting) = self.reader.take() {
            if waiting.badge == B_SHELL && msg.badge != B_SHELL {
                // A program may not take the keyboard away from the shell.
                self.reader = Some(waiting);
                let _ = sys::reply(msg.token, &Msg::new(BUSY));
                return;
            }
            // Otherwise the newer reader wins. If the old one is dead (the usual reason
            // for two readers) this keeps a dead task from holding the keyboard forever.
            self.answer_line(waiting, BUSY);
        }
        self.print(&msg.data[..(msg.len as usize).min(MSG_DATA)]);
        self.reader = Some(Reader { token: msg.token, badge: msg.badge });
        self.line_len = 0;
        self.after_return = false;
        while self.reader.is_some() && self.queue_len > 0 {
            let byte = self.queue[self.queue_head];
            self.queue_head = (self.queue_head + 1) % QUEUE;
            self.queue_len -= 1;
            self.edit(byte);
        }
    }

    fn request(&mut self, msg: &Msg) {
        let mut reply = Msg::new(OK);
        match msg.label {
            CON_WRITE => self.print(&msg.data[..(msg.len as usize).min(MSG_DATA)]),
            CON_READLINE => return self.read_line(msg),
            CON_INTR if msg.badge == B_SHELL => {
                reply.words[0] = core::mem::replace(&mut self.interrupted, false) as u32;
            }
            // `init` pings five times a second; that doubles as a poll of the serial
            // port in case an interrupt was lost.
            PING => self.drain_serial(),
            // Fault injection for the recovery tests, honoured only for the shell.
            DBG_CRASH if msg.badge == B_SHELL => rt::crash(),
            DBG_HANG if msg.badge == B_SHELL => loop {
                core::hint::spin_loop();
            },
            _ => reply.label = DENIED,
        }
        let _ = sys::reply(msg.token, &reply);
    }

    fn start_devices(&mut self) {
        port_out(SERIAL, COM1 + 3, 0x03); // 8 data bits, no parity, one stop bit
        port_out(SERIAL, COM1 + 1, 0x00); // no interrupts while setting up
        port_out(SERIAL, COM1 + 2, 0x07); // FIFO on and emptied, interrupt at one byte
        port_out(SERIAL, COM1 + 4, 0x0B); // DTR, RTS, and OUT2 which gates the interrupt
        for _ in 0..64 {
            if port_in(SERIAL, COM1 + 5) & 1 == 0 {
                break;
            }
            port_in(SERIAL, COM1);
        }
        port_in(KEYBOARD, KEYBOARD_PORT); // discard a stale scancode
        port_out(SERIAL, COM1 + 1, 0x01); // interrupt when a byte arrives
        let _ = sys::irq_bind(IRQS, KEYBOARD_IRQ, REQUESTS);
        let _ = sys::irq_bind(IRQS, SERIAL_IRQ, REQUESTS);
    }
}

#[no_mangle]
pub fn main() {
    let [restarts, root_starts, _, _] = rt::args();
    if sys::map_mem(SCREEN, SCREEN_BASE, 0, 1, true).is_err() {
        sys::exit(1);
    }
    let mut console = Console {
        cursor: 0,
        cursor_moved: true,
        shift: false,
        ctrl: false,
        extended: false,
        queue: [0; QUEUE],
        queue_head: 0,
        queue_len: 0,
        reader: None,
        line: [0; LINE_MAX],
        line_len: 0,
        after_return: false,
        interrupted: false,
    };
    if restarts == 0 && root_starts == 0 {
        console.clear();
    } else {
        // Keep what the previous console left on the screen.
        console.continue_below_text();
    }
    console.start_devices();
    if restarts > 0 {
        console.print(b"[console restarted]\n");
    } else if root_starts > 0 {
        console.print(b"[user space restarted]\n");
    }
    console.show_cursor();
    loop {
        let mut msg = Msg::new(0);
        if sys::recv(REQUESTS, &mut msg, FOREVER).is_err() {
            sys::exit(2);
        }
        if msg.label == L_IRQ {
            console.interrupts(msg.words[0]);
        } else if msg.label & L_KERNEL == 0 {
            console.request(&msg);
        }
        console.show_cursor();
    }
}
