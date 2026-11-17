//! `console`: the keyboard, mouse and serial-port driver, running as an ordinary task.
//!
//! In a monolithic kernel this code would run with full privileges and a bug in it could
//! overwrite anything. Here it holds five capabilities (its request endpoint, the serial
//! ports, the data port of the keyboard and the mouse, three interrupt lines and a send
//! right to the display); when it crashes or hangs, `init` starts a new copy and the callers simply send their request again.
//!
//! It does not touch the screen. What it prints goes down the serial line and, as
//! requests, to the `display` server, which shows it in the terminal window. The console
//! remembers the last four kilobytes it printed; when the display has been restarted, it
//! sends them again, so the terminal window comes back with its text.
//!
//! While it runs it is the only writer on the serial line. The supervisor's reports come
//! here as requests too (`CON_REPORT`), so that a line is never cut in two by another.
#![no_std]
#![no_main]

use rt::abi::{Msg, FOREVER, L_IRQ, L_KERNEL, MSG_DATA};
use rt::proto::{BUSY, B_SHELL, B_SUPERVISOR, CON_INTR, CON_READLINE, CON_REPORT, CON_WRITE};
use rt::proto::{DBG_CRASH, DBG_HANG, DENIED, DISP_MOUSE, DISP_TEXT, OK, PING};
use rt::sys;

// Capability slots, filled by `init`.
const REQUESTS: u32 = 0;
const SERIAL: u32 = 1;
const KEYBOARD: u32 = 2;
const IRQS: u32 = 3;
const DISPLAY: u32 = 4;

const COM1: u32 = 0x3F8;
const KEYBOARD_PORT: u32 = 0x60;
const KEYBOARD_IRQ: u32 = 1;
const SERIAL_IRQ: u32 = 4;
const MOUSE_IRQ: u32 = 12;

/// How much of what was printed the console keeps, to send it to a restarted display.
const KEPT: usize = 4096;
/// How long the console waits for the display to take a request. A display that does not
/// answer in this time is left alone until the next ping from `init`. This is shorter than
/// the 50 ms that `init` waits for the console's own answer to a ping.
const DISPLAY_WAIT_MS: u32 = 40;
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

struct Reader {
    token: u32,
    badge: u32,
}

struct Console {
    /// The last bytes printed, as a ring: `kept_len` bytes that end before `kept_end`.
    kept: [u8; KEPT],
    kept_end: usize,
    kept_len: usize,
    /// How many of the newest bytes the display has not been sent yet.
    unsent: usize,
    /// Does the terminal window show everything printed up to the unsent bytes?
    in_step: bool,
    /// The restart count the display last answered with.
    display_restarts: Option<u32>,
    /// `init` has pinged: time to try a display again that did not answer.
    retry: bool,
    /// The bytes of the mouse packet that has begun, and how many there are.
    packet: [u8; 3],
    packet_len: usize,
    /// Movement and buttons that the display has not been told yet.
    moved: (i32, i32),
    buttons: u32,
    mouse_news: bool,
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
    /// Remember a printed byte for the display.
    fn keep(&mut self, byte: u8) {
        self.kept[self.kept_end] = byte;
        self.kept_end = (self.kept_end + 1) % KEPT;
        self.kept_len = (self.kept_len + 1).min(KEPT);
        self.unsent += 1;
        if self.unsent > KEPT {
            // More was printed than is kept: the display has to start from a clear window.
            self.unsent = KEPT;
            self.in_step = false;
        }
    }

    /// Send the display the newest `count` kept bytes, after a clear if `clear` is set.
    /// `Err` means the display did not take them; `Ok(false)` that it has been restarted
    /// since it last answered, and so shows none of the older text.
    fn send_kept(&mut self, count: usize, clear: bool) -> Result<bool, ()> {
        let mut at = (self.kept_end + KEPT - count) % KEPT;
        let mut left = count;
        let mut first = clear;
        let mut same = true;
        while left > 0 || first {
            let mut msg = Msg::new(DISP_TEXT);
            msg.words[0] = first as u32;
            first = false;
            let len = left.min(MSG_DATA);
            for byte in &mut msg.data[..len] {
                *byte = self.kept[at];
                at = (at + 1) % KEPT;
            }
            msg.len = len as u32;
            left -= len;
            // With more to come, the display waits with drawing until it has all of it.
            msg.words[1] = (left > 0) as u32;
            if sys::call(DISPLAY, &mut msg, DISPLAY_WAIT_MS).is_err() || msg.label != OK {
                return Err(());
            }
            let restarts = msg.words[0];
            same &= self.display_restarts.is_none_or(|known| known == restarts);
            self.display_restarts = Some(restarts);
        }
        Ok(same)
    }

    /// Bring the terminal window up to date. While the display is in step, that means
    /// sending it the bytes printed since; otherwise everything that is kept.
    fn show(&mut self) {
        let retry = core::mem::replace(&mut self.retry, false);
        if !self.in_step && !retry {
            return; // the display is away; `init`'s next ping brings the console back here
        }
        if self.in_step {
            if self.unsent == 0 {
                return;
            }
            self.in_step = self.send_kept(self.unsent, false) == Ok(true);
        }
        if !self.in_step {
            self.in_step = self.send_kept(self.kept_len, true).is_ok();
        }
        if self.in_step {
            self.unsent = 0;
        }
    }

    /// Tell the display what the mouse did since it was last told.
    fn show_mouse(&mut self) {
        if !core::mem::replace(&mut self.mouse_news, false) {
            return;
        }
        let (dx, dy) = core::mem::replace(&mut self.moved, (0, 0));
        if self.in_step {
            let mut msg = Msg::new(DISP_MOUSE);
            msg.words = [dx as u32, dy as u32, self.buttons, 0];
            if sys::call(DISPLAY, &mut msg, DISPLAY_WAIT_MS).is_err() {
                self.in_step = false;
            }
        }
    }

    /// One byte from the mouse. A packet is three bytes: buttons and signs, then the
    /// movement to the right and the movement up.
    fn mouse_byte(&mut self, byte: u8) {
        if self.packet_len == 0 && byte & 0x08 == 0 {
            return; // not the first byte of a packet: wait for one that is
        }
        self.packet[self.packet_len] = byte;
        self.packet_len += 1;
        if self.packet_len == 3 {
            self.packet_len = 0;
            let [flags, x, y] = self.packet;
            if flags & 0xC0 == 0 {
                self.moved.0 += x as i32 - ((flags as i32) << 4 & 0x100);
                self.moved.1 += y as i32 - ((flags as i32) << 3 & 0x100);
            }
            self.buttons = (flags & 7) as u32;
            self.mouse_news = true;
        }
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
            self.keep(byte);
            if byte == b'\n' {
                self.serial_put(b'\r');
            }
            self.serial_put(byte);
        }
    }

    /// Send a line of the supervisor's report down the serial line. It belongs in the log,
    /// not on the screen.
    fn report(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if byte == b'\n' {
                self.serial_put(b'\r');
            }
            self.serial_put(byte);
        }
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

    /// The keyboard and the mouse deliver through the same data port. The status port that
    /// says whose byte is waiting is also the port that can reset the machine, so the
    /// console does not hold it. It goes by the interrupt line instead: the controller
    /// raises line 1 for a byte of the keyboard and line 12 for a byte of the mouse, and
    /// holds one byte at a time until it is read.
    fn interrupts(&mut self, pending: u32) {
        if pending & (1 << KEYBOARD_IRQ) != 0 {
            let byte = port_in(KEYBOARD, KEYBOARD_PORT) as u8;
            if let Some(character) = self.translate(byte) {
                self.input(character);
            }
            let _ = sys::irq_ack(IRQS, KEYBOARD_IRQ);
        } else if pending & (1 << MOUSE_IRQ) != 0 {
            let byte = port_in(KEYBOARD, KEYBOARD_PORT) as u8;
            self.mouse_byte(byte);
        }
        if pending & (1 << MOUSE_IRQ) != 0 {
            let _ = sys::irq_ack(IRQS, MOUSE_IRQ);
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
            CON_REPORT if msg.badge == B_SUPERVISOR => {
                self.report(&msg.data[..(msg.len as usize).min(MSG_DATA)])
            }
            CON_READLINE => return self.read_line(msg),
            CON_INTR if msg.badge == B_SHELL => {
                reply.words[0] = core::mem::replace(&mut self.interrupted, false) as u32;
            }
            // `init` pings five times a second; that doubles as a poll of the serial
            // port in case an interrupt was lost, and as the moment to try a display
            // again that did not answer.
            // The answer goes out first: a display that is slow must not make the
            // console look as if it had stopped.
            PING => {
                self.drain_serial();
                self.retry = true;
            }
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
        let _ = sys::irq_bind(IRQS, MOUSE_IRQ, REQUESTS);
    }
}

#[no_mangle]
pub fn main() {
    let [restarts, root_starts, _, _] = rt::args();
    let mut console = Console {
        kept: [0; KEPT],
        kept_end: 0,
        kept_len: 0,
        unsent: 0,
        in_step: true,
        display_restarts: None,
        retry: false,
        packet: [0; 3],
        packet_len: 0,
        moved: (0, 0),
        buttons: 0,
        mouse_news: false,
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
    console.start_devices();
    // The terminal window keeps what the previous console printed.
    if restarts > 0 {
        console.print(b"[console restarted]\n");
    } else if root_starts > 0 {
        console.print(b"[user space restarted]\n");
    }
    console.show();
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
        console.show();
        console.show_mouse();
    }
}
