//! `display`: the screen in graphics mode and the windows on it, running as an ordinary task.
//!
//! It switches the standard VGA card to 640 by 480 pixels in 16 colours, by writing the
//! card's registers; it needs no BIOS and no driver for one particular card. It draws a
//! desktop: a bar at the top, a terminal window that shows what the console prints, and the
//! windows that programs open.
//!
//! A window is a capability. `WIN_OPEN` answers with a send right on this server's own
//! endpoint whose badge names the new window; whoever holds that right may draw in that
//! window and in no other. The display asks the kernel once every two seconds who still
//! holds a copy, and closes a window that nobody holds: a program that crashes takes its
//! windows with it.
//!
//! The display holds five capabilities: its request endpoint, the VGA register ports, the
//! 64 KiB of VGA graphics memory, an unbadged right on its own endpoint from which the
//! window capabilities are made, and the read-only kernel tables. When it crashes or
//! hangs, `init` starts a new copy; the console then sends the text of the terminal window
//! again, and a program whose window is gone opens a new one.
#![no_std]
#![no_main]

use rt::abi::{Msg, CAP_SLOTS, E_TIMEOUT, L_KERNEL, MAX_TASKS, MSG_DATA, NO_CAP, PAGE, R_SEND};
use rt::abi::{ST_FREE, USER_BASE};
use rt::font;
use rt::proto::colour::{BLACK, BLUE, GREY, NAVY, SILVER, WHITE};
use rt::proto::{B_APP, B_CONSOLE, B_SHELL, DBG_CRASH, DBG_HANG, DENIED, DISP_INFO, DISP_MOUSE};
use rt::proto::{DISP_TEXT, ERR, OK, PING, WINDOW_BADGE, WINDOW_RECORD, WIN_CLOSE, WIN_FILL};
use rt::proto::{WIN_OPEN, WIN_TEXT};
use rt::{client, sys};

// Capability slots, filled by `init`.
const REQUESTS: u32 = 0;
const PORTS: u32 = 1;
const SCREEN: u32 = 2;
const MINT: u32 = 3;
const INFO: u32 = 4;

const WIDTH: usize = 640;
const HEIGHT: usize = 480;
/// Bytes in one row of one colour plane.
const STRIDE: usize = WIDTH / 8;
const PLANES: usize = 4;

/// Where the 64 KiB of the card's graphics memory are mapped.
const VGA_BASE: u32 = USER_BASE + 0x3000_0000;
const VGA_PAGES: u32 = 16;
/// The picture as it should be, and the picture as the card has it. Only bytes that differ
/// are written to the card, because every such write is slow.
const WANTED_BASE: u32 = USER_BASE + 0x3010_0000;
const SHOWN_BASE: u32 = USER_BASE + 0x3020_0000;
/// The pictures of the windows, each in a region of its own.
const WINDOW_BASE: u32 = USER_BASE + 0x3030_0000;
const WINDOW_REGION: u32 = 0x4_0000;

const MAX_WINDOWS: usize = 8;
/// Window 0 is the bar at the top, window 1 the terminal; the rest belong to programs.
const BAR: usize = 0;
const TERMINAL: usize = 1;
const FIRST_CLIENT: usize = 2;

const BAR_HEIGHT: usize = 16;
/// The frame of a window: this many pixels left, right and below, and `TOP` above.
const BORDER: usize = 4;
const TOP: usize = 18;
const TITLE_MAX: usize = 24;

const COLUMNS: usize = 80;
const LINES: usize = 25;
const TERMINAL_X: i32 = 8;
const TERMINAL_Y: i32 = 24;

const DESKTOP: u8 = NAVY;
/// A change of at most this many bytes per plane is shown at once; a larger one waits
/// until `FRAME_US` have passed since the last one, so that a burst of output costs one
/// redraw and not one per line.
const SMALL_CHANGE: usize = 1600;
const FRAME_US: u64 = 50_000;
/// How long the display waits for text that the console has announced.
const HOLD_US: u64 = 100_000;
/// How often the display looks for windows that nobody holds any more.
const SWEEP_US: u64 = 2_000_000;

const SEQUENCER: u32 = 0x3C4;
const GRAPHICS: u32 = 0x3CE;
const CRTC: u32 = 0x3D4;
const ATTRIBUTE: u32 = 0x3C0;
const MISC_WRITE: u32 = 0x3C2;
const DAC_INDEX: u32 = 0x3C8;
const DAC_DATA: u32 = 0x3C9;
const STATUS: u32 = 0x3DA;

// The register values of the standard VGA mode 640x480 in 16 colours ("mode 12h").
const MODE_MISC: u8 = 0xE3;
const MODE_SEQUENCER: [u8; 5] = [0x03, 0x01, 0x0F, 0x00, 0x06];
const MODE_CRTC: [u8; 25] = [
    0x5F, 0x4F, 0x50, 0x82, 0x54, 0x80, 0x0B, 0x3E, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0xEA, 0x0C, 0xDF, 0x28, 0x00, 0xE7, 0x04, 0xE3, 0xFF,
];
const MODE_GRAPHICS: [u8; 9] = [0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x05, 0x0F, 0xFF];
/// Colour `n` of a pixel is entry `n` of the palette below; overscan black; all four planes.
const MODE_ATTRIBUTE: [u8; 21] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 0x01, 0x00, 0x0F, 0x00, 0x00,
];
/// Red, green and blue of the sixteen colours (`rt::proto::colour`), 0..=255 each.
const PALETTE: [[u8; 3]; 16] = [
    [0x10, 0x12, 0x18],
    [0x1E, 0x38, 0x5C],
    [0x2E, 0x7D, 0x4F],
    [0x2F, 0x8C, 0x8C],
    [0xA8, 0x3A, 0x3A],
    [0x7A, 0x4E, 0x9E],
    [0xD0, 0x8C, 0x28],
    [0xC8, 0xCC, 0xD0],
    [0x58, 0x60, 0x6C],
    [0x3C, 0x82, 0xDC],
    [0x5C, 0xD0, 0x70],
    [0x5C, 0xD4, 0xD8],
    [0xF0, 0x60, 0x5C],
    [0xC8, 0x80, 0xE0],
    [0xF4, 0xD4, 0x5C],
    [0xFF, 0xFF, 0xFF],
];

/// The mouse pointer: `X` black, `.` white, anything else leaves the picture as it is.
const POINTER: [&[u8; 12]; 17] = [
    b"X           ",
    b"XX          ",
    b"X.X         ",
    b"X..X        ",
    b"X...X       ",
    b"X....X      ",
    b"X.....X     ",
    b"X......X    ",
    b"X.......X   ",
    b"X........X  ",
    b"X.....XXXXX ",
    b"X..X..X     ",
    b"X.X X..X    ",
    b"XX  X..X    ",
    b"X    X..X   ",
    b"     X..X   ",
    b"      XX    ",
];

fn port_out(port: u32, value: u8) {
    let _ = sys::io_out(PORTS, port, 1, value as u32);
}

fn port_in(port: u32) -> u8 {
    sys::io_in(PORTS, port, 1).unwrap_or(0) as u8
}

/// Write one byte of the plane that the sequencer has selected.
fn card_write(offset: usize, value: u8) {
    if offset < STRIDE * HEIGHT {
        // SAFETY: `main` mapped the card's 64 KiB writable at VGA_BASE before the first
        // call and never unmaps them; the offset is inside the 38400 bytes of a plane; no
        // Rust reference covers device memory, so a volatile store cannot break aliasing.
        unsafe { ((VGA_BASE as usize + offset) as *mut u8).write_volatile(value) }
    }
}

/// Set the bits `x0..x1` of a row to one value.
fn span(row: &mut [u8], x0: usize, x1: usize, on: bool) {
    let (first, last) = (x0 / 8, (x1 - 1) / 8);
    let first_mask = 0xFFu8 >> (x0 & 7);
    let last_mask = 0xFFu8 << (7 - ((x1 - 1) & 7));
    let mut apply = |index: usize, mask: u8| {
        if on {
            row[index] |= mask;
        } else {
            row[index] &= !mask;
        }
    };
    if first == last {
        apply(first, first_mask & last_mask);
    } else {
        apply(first, first_mask);
        apply(last, last_mask);
        row[first + 1..last].fill(if on { 0xFF } else { 0 });
    }
}

/// Put the low `count` bits of `pattern` (at most 8) into a row, the first at bit `x`.
fn put_bits(row: &mut [u8], x: usize, count: usize, pattern: u16) {
    let index = x / 8;
    let shift = 16 - count - (x & 7);
    let mask = ((1u16 << count) - 1) << shift;
    let low = if index + 1 < row.len() { row[index + 1] } else { 0 };
    let word = (((row[index] as u16) << 8 | low as u16) & !mask) | (pattern << shift);
    row[index] = (word >> 8) as u8;
    if index + 1 < row.len() {
        row[index + 1] = word as u8;
    }
}

/// A picture in the card's own layout: four planes of one bit per pixel, one after the
/// other, each `stride` bytes wide and `height` rows high.
struct Bitmap {
    base: u32,
    stride: usize,
    height: usize,
}

impl Bitmap {
    const NONE: Bitmap = Bitmap { base: 0, stride: 0, height: 0 };

    fn bytes(stride: usize, height: usize) -> usize {
        PLANES * stride * height
    }

    /// Map fresh memory for a picture of this size at `base`.
    fn create(base: u32, stride: usize, height: usize) -> Option<Bitmap> {
        let pages = Bitmap::bytes(stride, height).div_ceil(PAGE as usize) as u32;
        sys::map(base, pages, true).ok()?;
        Some(Bitmap { base, stride, height })
    }

    fn destroy(&mut self) {
        let pages = Bitmap::bytes(self.stride, self.height).div_ceil(PAGE as usize) as u32;
        if pages > 0 {
            // SAFETY: the pages were mapped by `create` for this picture alone, and the
            // slices handed out by `plane` and `plane_mut` borrow `self`, so none is left.
            let _ = unsafe { sys::unmap(self.base, pages) };
        }
        *self = Bitmap::NONE;
    }

    fn plane(&self, plane: usize) -> &[u8] {
        let size = self.stride * self.height;
        // SAFETY: `create` mapped at least `PLANES * size` bytes at `base`, they stay
        // mapped until `destroy`, and the borrow of `self` keeps `destroy` away.
        unsafe { core::slice::from_raw_parts((self.base as usize + plane * size) as *const u8, size) }
    }

    fn plane_mut(&mut self, plane: usize) -> &mut [u8] {
        let size = self.stride * self.height;
        // SAFETY: as in `plane`; the exclusive borrow of `self` makes this the only slice.
        unsafe { core::slice::from_raw_parts_mut((self.base as usize + plane * size) as *mut u8, size) }
    }

    fn fill(&mut self, x: usize, y: usize, width: usize, height: usize, colour: u8) {
        let x1 = (x + width).min(self.stride * 8);
        let y1 = (y + height).min(self.height);
        if x >= x1 || y >= y1 {
            return;
        }
        let stride = self.stride;
        for plane in 0..PLANES {
            let on = colour >> plane & 1 != 0;
            let rows = self.plane_mut(plane);
            for row in y..y1 {
                span(&mut rows[row * stride..(row + 1) * stride], x, x1, on);
            }
        }
    }

    /// Draw one character with its top left corner at `x`, `y`, if all of it fits.
    fn glyph(&mut self, x: usize, y: usize, byte: u8, text: u8, background: u8) {
        if x + font::WIDTH > self.stride * 8 || y + font::HEIGHT > self.height {
            return;
        }
        let shape = font::glyph(byte);
        let stride = self.stride;
        for plane in 0..PLANES {
            let rows = self.plane_mut(plane);
            for (line, bits) in shape.iter().enumerate() {
                let bits = (bits >> 2) as u16;
                let pattern = match (text >> plane & 1, background >> plane & 1) {
                    (1, 1) => 0x3F,
                    (1, _) => bits,
                    (_, 1) => !bits & 0x3F,
                    _ => 0,
                };
                let row = (y + line) * stride;
                put_bits(&mut rows[row..row + stride], x, font::WIDTH, pattern);
            }
        }
    }

    fn text(&mut self, x: usize, y: usize, bytes: &[u8], text: u8, background: u8) {
        for (index, &byte) in bytes.iter().enumerate() {
            self.glyph(x + index * font::WIDTH, y, byte, text, background);
        }
    }
}

struct Window {
    used: bool,
    /// Top left corner on the screen; `x` is a multiple of 8, so that a row of the window
    /// is copied to the screen as whole bytes.
    x: i32,
    y: i32,
    bitmap: Bitmap,
    title: [u8; TITLE_MAX],
    title_len: usize,
    /// The display's own copy of the window capability, of which the program's is a copy.
    cap: u32,
}

impl Window {
    const NONE: Window =
        Window { used: false, x: 0, y: 0, bitmap: Bitmap::NONE, title: [0; TITLE_MAX], title_len: 0, cap: NO_CAP };

    fn width(&self) -> usize {
        self.bitmap.stride * 8
    }

    fn height(&self) -> usize {
        self.bitmap.height
    }

    fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.width() as i32 && y < self.y + self.height() as i32
    }

    /// Draw the frame and the title. The terminal has the keyboard, so its title stands out.
    fn draw_frame(&mut self, has_keyboard: bool) {
        let (width, height) = (self.width(), self.height());
        let (bar, ink) = if has_keyboard { (BLUE, WHITE) } else { (GREY, SILVER) };
        self.bitmap.fill(0, 0, width, TOP, bar);
        self.bitmap.fill(0, TOP, BORDER, height - TOP, GREY);
        self.bitmap.fill(width - BORDER, TOP, BORDER, height - TOP, GREY);
        self.bitmap.fill(0, height - BORDER, width, BORDER, GREY);
        self.bitmap.fill(0, 0, width, 1, BLACK);
        self.bitmap.fill(0, height - 1, width, 1, BLACK);
        self.bitmap.fill(0, 0, 1, height, BLACK);
        self.bitmap.fill(width - 1, 0, 1, height, BLACK);
        self.bitmap.fill(BORDER - 1, TOP - 1, width - 2 * BORDER + 2, 1, BLACK);
        let room = (width - 2 * BORDER - 8) / font::WIDTH;
        let shown = self.title_len.min(room);
        let title = self.title;
        self.bitmap.text(BORDER + 4, 3, &title[..shown], ink, bar);
    }
}

/// The part of the screen that has to be drawn again: rows `y0..y1`, bytes `c0..c1`.
#[derive(Clone, Copy)]
struct Dirty {
    y0: usize,
    y1: usize,
    c0: usize,
    c1: usize,
}

impl Dirty {
    const NOTHING: Dirty = Dirty { y0: HEIGHT, y1: 0, c0: STRIDE, c1: 0 };

    fn is_empty(&self) -> bool {
        self.y0 >= self.y1 || self.c0 >= self.c1
    }

    /// Add the pixels `x0..x1`, `y0..y1`, as far as they are on the screen.
    fn add(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        let (x0, x1) = (x0.clamp(0, WIDTH as i32) as usize, x1.clamp(0, WIDTH as i32) as usize);
        let (y0, y1) = (y0.clamp(0, HEIGHT as i32) as usize, y1.clamp(0, HEIGHT as i32) as usize);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        self.y0 = self.y0.min(y0);
        self.y1 = self.y1.max(y1);
        self.c0 = self.c0.min(x0 / 8);
        self.c1 = self.c1.max(x1.div_ceil(8));
    }

    fn size(&self) -> usize {
        if self.is_empty() {
            0
        } else {
            (self.y1 - self.y0) * (self.c1 - self.c0)
        }
    }
}

struct Display {
    restarts: u32,
    me: u32,
    wanted: Bitmap,
    shown: Bitmap,
    windows: [Window; MAX_WINDOWS],
    /// The windows from the lowest to the topmost.
    order: [u8; MAX_WINDOWS],
    count: usize,
    dirty: Dirty,
    last_frame: u64,
    frames: u32,
    next_second: u64,
    next_sweep: u64,
    /// Nothing is drawn before this time: the console has announced more text.
    hold_until: u64,
    // The terminal window.
    cells: [[u8; COLUMNS]; LINES],
    column: usize,
    line: usize,
    // The mouse.
    mouse_seen: bool,
    mouse_x: i32,
    mouse_y: i32,
    buttons: u32,
    /// The window being moved with the mouse, and where in it the pointer holds it.
    held: Option<(usize, i32, i32)>,
}

/// Decimal digits of `value` at the end of `field`; returns the index of the first one.
fn digits(mut value: u32, field: &mut [u8; 10]) -> usize {
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

impl Display {
    // ---------------------------------------------------------------- the card

    fn set_mode() {
        port_out(MISC_WRITE, MODE_MISC);
        port_out(SEQUENCER, 0);
        port_out(SEQUENCER + 1, 0x01); // hold the sequencer while its registers change
        for (index, &value) in MODE_SEQUENCER.iter().enumerate().skip(1) {
            port_out(SEQUENCER, index as u8);
            port_out(SEQUENCER + 1, value);
        }
        port_out(SEQUENCER, 0);
        port_out(SEQUENCER + 1, MODE_SEQUENCER[0]);
        // Registers 0 to 7 of the CRT controller are write-protected by bit 7 of register 17.
        port_out(CRTC, 0x11);
        port_out(CRTC + 1, MODE_CRTC[0x11] & 0x7F);
        for (index, &value) in MODE_CRTC.iter().enumerate() {
            port_out(CRTC, index as u8);
            port_out(CRTC + 1, value);
        }
        for (index, &value) in MODE_GRAPHICS.iter().enumerate() {
            port_out(GRAPHICS, index as u8);
            port_out(GRAPHICS + 1, value);
        }
        // The attribute controller takes index and value through one port, in turns;
        // reading the status port makes the next write an index.
        port_in(STATUS);
        for (index, &value) in MODE_ATTRIBUTE.iter().enumerate() {
            port_out(ATTRIBUTE, index as u8);
            port_out(ATTRIBUTE, value);
        }
        port_out(DAC_INDEX, 0);
        for colour in PALETTE {
            for part in colour {
                port_out(DAC_DATA, part >> 2); // the card takes six bits of each
            }
        }
        port_in(STATUS);
        port_out(ATTRIBUTE, 0x20); // show the picture
    }

    fn select_planes(mask: u8) {
        port_out(SEQUENCER, 2);
        port_out(SEQUENCER + 1, mask);
    }

    // ---------------------------------------------------------------- drawing the screen

    /// Build the dirty part of the wanted picture from the windows, then write to the card
    /// what differs from what it shows.
    fn frame(&mut self) {
        let dirty = core::mem::replace(&mut self.dirty, Dirty::NOTHING);
        if dirty.is_empty() {
            return;
        }
        let Dirty { y0, y1, c0, c1 } = dirty;
        for plane in 0..PLANES {
            let value = if DESKTOP >> plane & 1 != 0 { 0xFF } else { 0 };
            let rows = self.wanted.plane_mut(plane);
            for y in y0..y1 {
                rows[y * STRIDE + c0..y * STRIDE + c1].fill(value);
            }
        }
        for &slot in &self.order[..self.count] {
            let window = &self.windows[slot as usize];
            let stride = window.bitmap.stride as i32;
            let left = window.x >> 3;
            let (from, to) = (left.max(c0 as i32), (left + stride).min(c1 as i32));
            let (top, bottom) = (window.y.max(y0 as i32), (window.y + window.height() as i32).min(y1 as i32));
            if from >= to || top >= bottom {
                continue;
            }
            let bytes = (to - from) as usize;
            for plane in 0..PLANES {
                let source = window.bitmap.plane(plane);
                let target = self.wanted.plane_mut(plane);
                for y in top..bottom {
                    let at = ((y - window.y) * stride + from - left) as usize;
                    let screen = y as usize * STRIDE + from as usize;
                    target[screen..screen + bytes].copy_from_slice(&source[at..at + bytes]);
                }
            }
        }
        if self.mouse_seen {
            for (row, line) in POINTER.iter().enumerate() {
                for (column, &mark) in line.iter().enumerate() {
                    let (x, y) = (self.mouse_x + column as i32, self.mouse_y + row as i32);
                    let colour = match mark {
                        b'X' => BLACK,
                        b'.' => WHITE,
                        _ => continue,
                    };
                    if x >= 0 && y >= 0 {
                        self.wanted.fill(x as usize, y as usize, 1, 1, colour);
                    }
                }
            }
        }
        for plane in 0..PLANES {
            Display::select_planes(1 << plane);
            let wanted = self.wanted.plane(plane);
            let shown = self.shown.plane_mut(plane);
            for y in y0..y1 {
                for offset in y * STRIDE + c0..y * STRIDE + c1 {
                    if shown[offset] != wanted[offset] {
                        shown[offset] = wanted[offset];
                        card_write(offset, wanted[offset]);
                    }
                }
            }
        }
        self.frames = self.frames.wrapping_add(1);
        self.last_frame = sys::time();
    }

    fn dirty_window(&mut self, slot: usize) {
        let window = &self.windows[slot];
        let (x, y) = (window.x, window.y);
        self.dirty.add(x, y, x + window.width() as i32, y + window.height() as i32);
    }

    fn dirty_pointer(&mut self) {
        let (x, y) = (self.mouse_x, self.mouse_y);
        self.dirty.add(x, y, x + POINTER[0].len() as i32, y + POINTER.len() as i32);
    }

    // ---------------------------------------------------------------- the bar

    fn draw_bar(&mut self) {
        let bar = &mut self.windows[BAR].bitmap;
        bar.fill(0, 0, WIDTH, BAR_HEIGHT, GREY);
        bar.fill(0, BAR_HEIGHT - 1, WIDTH, 1, BLACK);
        bar.text(8, 2, concat!("Inti ", env!("CARGO_PKG_VERSION")).as_bytes(), WHITE, GREY);
        let mut field = [0; 10];
        if self.restarts > 0 {
            let label = b"display restarted";
            bar.text(200, 2, label, SILVER, GREY);
            let at = digits(self.restarts, &mut field);
            bar.text(200 + (label.len() + 1) * font::WIDTH, 2, &field[at..], SILVER, GREY);
        }
        let seconds = (sys::time() / 1_000_000) as u32;
        let at = digits(seconds, &mut field);
        let text = &field[at..];
        let x = WIDTH - 8 - (text.len() + 2) * font::WIDTH;
        bar.text(x - 3 * font::WIDTH, 2, b"up ", SILVER, GREY);
        bar.text(x, 2, text, WHITE, GREY);
        bar.text(x + text.len() * font::WIDTH, 2, b" s", SILVER, GREY);
        self.dirty.add(0, 0, WIDTH as i32, BAR_HEIGHT as i32);
    }

    // ---------------------------------------------------------------- the terminal

    fn draw_cell(&mut self, column: usize, line: usize, cursor: bool) {
        if column >= COLUMNS || line >= LINES {
            return;
        }
        let (text, background) = if cursor { (BLACK, SILVER) } else { (SILVER, BLACK) };
        let (x, y) = (BORDER + column * font::WIDTH, TOP + line * font::HEIGHT);
        self.windows[TERMINAL].bitmap.glyph(x, y, self.cells[line][column], text, background);
        let (left, top) = (TERMINAL_X + x as i32, TERMINAL_Y + y as i32);
        // Windows are moved, so the corner is read each time.
        let (dx, dy) = (self.windows[TERMINAL].x - TERMINAL_X, self.windows[TERMINAL].y - TERMINAL_Y);
        self.dirty.add(left + dx, top + dy, left + dx + font::WIDTH as i32, top + dy + font::HEIGHT as i32);
    }

    fn scroll(&mut self) {
        let bitmap = &mut self.windows[TERMINAL].bitmap;
        let stride = bitmap.stride;
        let (first, last) = (TOP * stride, (TOP + LINES * font::HEIGHT) * stride);
        for plane in 0..PLANES {
            bitmap.plane_mut(plane).copy_within(first + font::HEIGHT * stride..last, first);
        }
        let width = COLUMNS * font::WIDTH;
        bitmap.fill(BORDER, TOP + (LINES - 1) * font::HEIGHT, width, font::HEIGHT, BLACK);
        self.cells.copy_within(1.., 0);
        self.cells[LINES - 1] = [b' '; COLUMNS];
        self.line -= 1;
        self.dirty_window(TERMINAL);
    }

    fn clear_terminal(&mut self) {
        self.cells = [[b' '; COLUMNS]; LINES];
        self.column = 0;
        self.line = 0;
        let (width, height) = (COLUMNS * font::WIDTH, LINES * font::HEIGHT);
        self.windows[TERMINAL].bitmap.fill(BORDER, TOP, width, height, BLACK);
        self.dirty_window(TERMINAL);
    }

    fn terminal_write(&mut self, bytes: &[u8]) {
        self.draw_cell(self.column, self.line, false);
        for &byte in bytes {
            match byte {
                b'\n' => {
                    self.column = 0;
                    self.line += 1;
                }
                b'\r' => self.column = 0,
                8 => self.column = self.column.saturating_sub(1),
                b'\t' => self.column = ((self.column / 8 + 1) * 8).min(COLUMNS),
                32..=126 => {
                    if self.column >= COLUMNS {
                        self.column = 0;
                        self.line += 1;
                        if self.line >= LINES {
                            self.scroll();
                        }
                    }
                    self.cells[self.line][self.column] = byte;
                    self.draw_cell(self.column, self.line, false);
                    self.column += 1;
                }
                _ => {}
            }
            if self.line >= LINES {
                self.scroll();
            }
        }
        self.draw_cell(self.column, self.line, true);
    }

    // ---------------------------------------------------------------- windows

    /// Make window `slot` with a picture of `width` by `height` pixels, frame included.
    fn create(&mut self, slot: usize, x: i32, y: i32, width: usize, height: usize, title: &[u8]) -> bool {
        let base = WINDOW_BASE + slot as u32 * WINDOW_REGION;
        if Bitmap::bytes(width / 8, height) > WINDOW_REGION as usize {
            return false;
        }
        let Some(bitmap) = Bitmap::create(base, width / 8, height) else { return false };
        let window = &mut self.windows[slot];
        *window = Window { used: true, x, y, bitmap, ..Window::NONE };
        window.title_len = title.len().min(TITLE_MAX);
        window.title[..window.title_len].copy_from_slice(&title[..window.title_len]);
        self.order[self.count] = slot as u8;
        self.count += 1;
        self.dirty_window(slot);
        true
    }

    fn close(&mut self, slot: usize) {
        self.dirty_window(slot);
        let window = &mut self.windows[slot];
        if window.cap != NO_CAP {
            // Take the capability back from whoever holds a copy, then give it up.
            let _ = sys::cap_revoke(window.cap);
            let _ = sys::cap_drop(window.cap);
        }
        window.bitmap.destroy();
        *window = Window::NONE;
        if let Some(at) = self.order[..self.count].iter().position(|&other| other as usize == slot) {
            self.order.copy_within(at + 1..self.count, at);
            self.count -= 1;
        }
        if matches!(self.held, Some((held, _, _)) if held == slot) {
            self.held = None;
        }
    }

    fn raise(&mut self, slot: usize) {
        if let Some(at) = self.order[..self.count].iter().position(|&other| other as usize == slot) {
            self.order.copy_within(at + 1..self.count, at);
            self.order[self.count - 1] = slot as u8;
            self.dirty_window(slot);
        }
    }

    fn overlaps(&self, x: i32, y: i32, width: i32, height: i32) -> bool {
        self.order[..self.count].iter().any(|&slot| {
            let other = &self.windows[slot as usize];
            x < other.x + other.width() as i32
                && other.x < x + width
                && y < other.y + other.height() as i32
                && other.y < y + height
        })
    }

    /// A place for a new window: the first free one, from the top right downwards; if the
    /// screen is full, on top of the others, each a little further down and right.
    fn place(&self, width: usize, height: usize) -> (i32, i32) {
        let (width, height) = (width as i32, height as i32);
        let mut y = BAR_HEIGHT as i32 + 8;
        while y + height <= HEIGHT as i32 - 8 {
            let mut x = WIDTH as i32 - 8 - width;
            while x >= 8 {
                if !self.overlaps(x - 8, y - 8, width + 16, height + 16) {
                    return (x, y);
                }
                x -= 8;
            }
            y += 8;
        }
        let step = 16 * (self.count as i32 - 2);
        let x = (40 + step).min(WIDTH as i32 - width).max(0) & !7;
        let y = (56 + step).min(HEIGHT as i32 - height).max(BAR_HEIGHT as i32);
        (x, y)
    }

    fn open(&mut self, msg: &Msg, reply: &mut Msg) {
        let Some(slot) = (FIRST_CLIENT..MAX_WINDOWS).find(|&slot| !self.windows[slot].used) else {
            return;
        };
        let inside_width = (msg.words[0] as usize).clamp(16, WIDTH - 2 * BORDER - 8).next_multiple_of(8);
        let inside_height = (msg.words[1] as usize).clamp(font::HEIGHT, HEIGHT - BAR_HEIGHT - TOP - BORDER);
        let (width, height) = (inside_width + 2 * BORDER, inside_height + TOP + BORDER);
        let (x, y) = self.place(width, height);
        let title = &msg.data[..(msg.len as usize).min(MSG_DATA)];
        if !self.create(slot, x, y, width, height, title) {
            return;
        }
        let badge = WINDOW_BADGE | (self.restarts & 0xFFFF) << 8 | slot as u32;
        match sys::cap_derive(MINT, R_SEND, badge, 0) {
            Ok(cap) => self.windows[slot].cap = cap,
            Err(_) => return self.close(slot),
        }
        let window = &mut self.windows[slot];
        window.bitmap.fill(BORDER, TOP, inside_width, inside_height, BLACK);
        window.draw_frame(false);
        reply.label = OK;
        reply.cap = window.cap;
        reply.words = [inside_width as u32, inside_height as u32, 0, 0];
    }

    /// A request that came through the capability of window `slot`.
    fn draw(&mut self, slot: usize, msg: &Msg, reply: &mut Msg) {
        let window = &mut self.windows[slot];
        let inside_width = window.width() - 2 * BORDER;
        let inside_height = window.height() - TOP - BORDER;
        let (x, y) = (msg.words[0] as usize, msg.words[1] as usize);
        let (corner_x, corner_y) = (window.x + BORDER as i32, window.y + TOP as i32);
        // The part of the window that changes, in pixels from the corner of its inside.
        let mut changed = (0, 0, 0, 0);
        match msg.label {
            WIN_TEXT => {
                let (text, background) = (msg.words[2] as u8 & 15, (msg.words[2] >> 4) as u8 & 15);
                let bytes = &msg.data[..(msg.len as usize).min(MSG_DATA)];
                if y < inside_height && y + font::HEIGHT <= inside_height {
                    for (index, &byte) in bytes.iter().enumerate() {
                        let Some(at) = x.checked_add(index * font::WIDTH) else { break };
                        if at + font::WIDTH > inside_width {
                            break;
                        }
                        window.bitmap.glyph(BORDER + at, TOP + y, byte, text, background);
                    }
                    let end = x.saturating_add(bytes.len() * font::WIDTH).min(inside_width);
                    changed = (x.min(end), y, end, y + font::HEIGHT);
                }
            }
            WIN_FILL => {
                let (width, height) = ((msg.words[2] & 0xFFFF) as usize, (msg.words[2] >> 16) as usize);
                if x < inside_width && y < inside_height {
                    let (width, height) = (width.min(inside_width - x), height.min(inside_height - y));
                    window.bitmap.fill(BORDER + x, TOP + y, width, height, msg.words[3] as u8 & 15);
                    changed = (x, y, x + width, y + height);
                }
            }
            WIN_CLOSE => {
                reply.label = OK;
                return self.close(slot);
            }
            _ => return,
        }
        reply.label = OK;
        let (x0, y0, x1, y1) = changed;
        self.dirty.add(corner_x + x0 as i32, corner_y + y0 as i32, corner_x + x1 as i32, corner_y + y1 as i32);
    }

    /// Close every window whose capability no task holds a copy of any more.
    fn sweep(&mut self) {
        if !(FIRST_CLIENT..MAX_WINDOWS).any(|slot| self.windows[slot].used) {
            return;
        }
        let mut held = [false; CAP_SLOTS];
        for task in 0..MAX_TASKS as u32 {
            if !sys::task_info(INFO, task).is_ok_and(|info| info.state != ST_FREE) {
                continue;
            }
            for cap in 0..CAP_SLOTS as u32 {
                if let Ok(info) = sys::cap_info(INFO, task, cap) {
                    if info.from != NO_CAP && info.from >> 8 == self.me {
                        held[(info.from & 0xFF) as usize % CAP_SLOTS] = true;
                    }
                }
            }
        }
        for slot in FIRST_CLIENT..MAX_WINDOWS {
            let window = &self.windows[slot];
            if window.used && window.cap != NO_CAP && !held[window.cap as usize % CAP_SLOTS] {
                self.close(slot);
            }
        }
    }

    // ---------------------------------------------------------------- the mouse

    fn mouse(&mut self, dx: i32, dy: i32, buttons: u32) {
        self.dirty_pointer();
        self.mouse_seen = true;
        self.mouse_x = (self.mouse_x + dx.clamp(-200, 200)).clamp(0, WIDTH as i32 - 1);
        self.mouse_y = (self.mouse_y - dy.clamp(-200, 200)).clamp(0, HEIGHT as i32 - 1);
        self.dirty_pointer();
        let pressed = buttons & 1 != 0 && self.buttons & 1 == 0;
        let released = buttons & 1 == 0;
        self.buttons = buttons;
        let (x, y) = (self.mouse_x, self.mouse_y);
        if pressed {
            // The topmost window under the pointer comes to the front; held by its title,
            // it follows the pointer until the button is let go.
            let hit = self.order[1..self.count].iter().rev().find(|&&slot| self.windows[slot as usize].contains(x, y));
            if let Some(&slot) = hit {
                let slot = slot as usize;
                self.raise(slot);
                let window = &self.windows[slot];
                if y < window.y + TOP as i32 {
                    self.held = Some((slot, x - window.x, y - window.y));
                }
            }
        } else if released {
            self.held = None;
        }
        if let Some((slot, grip_x, grip_y)) = self.held {
            let width = self.windows[slot].width() as i32;
            let new_x = (x - grip_x).clamp(32 - width, WIDTH as i32 - 32) & !7;
            let new_y = (y - grip_y).clamp(BAR_HEIGHT as i32, HEIGHT as i32 - TOP as i32);
            if (new_x, new_y) != (self.windows[slot].x, self.windows[slot].y) {
                self.dirty_window(slot);
                self.windows[slot].x = new_x;
                self.windows[slot].y = new_y;
                self.dirty_window(slot);
            }
        }
    }

    // ---------------------------------------------------------------- requests

    fn info(&self, reply: &mut Msg) {
        reply.label = OK;
        let clients = (FIRST_CLIENT..MAX_WINDOWS).filter(|&slot| self.windows[slot].used).count();
        let pointer = (self.mouse_x as u32 & 0xFFFF) | (self.mouse_y as u32) << 16;
        reply.words = [clients as u32, self.frames, self.restarts, pointer];
        let mut len = 0;
        for &slot in &self.order[1..self.count] {
            let window = &self.windows[slot as usize];
            let record = &mut reply.data[len..len + WINDOW_RECORD];
            let shown = window.title_len.min(12);
            record[..shown].copy_from_slice(&window.title[..shown]);
            record[12..14].copy_from_slice(&(window.x as i16).to_le_bytes());
            record[14..16].copy_from_slice(&(window.y as i16).to_le_bytes());
            record[16..18].copy_from_slice(&(window.width() as u16).to_le_bytes());
            record[18..20].copy_from_slice(&(window.height() as u16).to_le_bytes());
            record[20] = slot;
            len += WINDOW_RECORD;
        }
        reply.len = len as u32;
    }

    fn request(&mut self, msg: &Msg) {
        // Nobody has a reason to send this server a capability; one that arrives is given up.
        if msg.cap != NO_CAP {
            let _ = sys::cap_drop(msg.cap);
        }
        let mut reply = Msg::new(DENIED);
        let slot = (msg.badge & 0xFF) as usize;
        let mine = WINDOW_BADGE | (self.restarts & 0xFFFF) << 8;
        match msg.label {
            _ if msg.badge & 0xFF00_0000 == WINDOW_BADGE => {
                // A capability from before a restart names a window that no longer exists.
                reply.label = ERR;
                let known = msg.badge & !0xFF == mine && (FIRST_CLIENT..MAX_WINDOWS).contains(&slot);
                if known && self.windows[slot].used {
                    self.draw(slot, msg, &mut reply);
                }
            }
            PING => reply.label = OK,
            DISP_TEXT if msg.badge == B_CONSOLE => {
                if msg.words[0] == 1 {
                    self.clear_terminal();
                }
                self.terminal_write(&msg.data[..(msg.len as usize).min(MSG_DATA)]);
                // Text that comes in several requests is drawn once, after the last of them.
                self.hold_until = if msg.words[1] == 1 { sys::time() + HOLD_US } else { 0 };
                reply.label = OK;
                reply.words[0] = self.restarts;
            }
            DISP_MOUSE if msg.badge == B_CONSOLE => {
                self.mouse(msg.words[0] as i32, msg.words[1] as i32, msg.words[2]);
                reply.label = OK;
            }
            WIN_OPEN if msg.badge == B_SHELL || msg.badge == B_APP => {
                reply.label = ERR;
                self.open(msg, &mut reply);
                if sys::reply(msg.token, &reply).is_err() && reply.cap != NO_CAP {
                    // The capability could not be handed over; then there is no window.
                    self.close(slot_of(reply.cap, &self.windows));
                    let _ = sys::reply(msg.token, &Msg::new(ERR));
                }
                return;
            }
            DISP_INFO => self.info(&mut reply),
            // Fault injection for the recovery tests, honoured only for the shell.
            DBG_CRASH if msg.badge == B_SHELL => rt::crash(),
            DBG_HANG if msg.badge == B_SHELL => loop {
                core::hint::spin_loop();
            },
            _ => {}
        }
        let _ = sys::reply(msg.token, &reply);
    }

    fn run(&mut self) -> ! {
        loop {
            let now = sys::time();
            let wake = if self.dirty.is_empty() {
                self.next_second
            } else {
                self.next_second.min((self.last_frame + FRAME_US).max(self.hold_until))
            };
            let wait_ms = (wake.saturating_sub(now) / 1000).clamp(1, 1000) as u32;
            let mut msg = Msg::new(0);
            match sys::recv(REQUESTS, &mut msg, wait_ms) {
                Ok(()) if msg.label & L_KERNEL == 0 => self.request(&msg),
                Ok(()) | Err(E_TIMEOUT) => {}
                Err(_) => sys::exit(2),
            }
            let now = sys::time();
            if now >= self.next_second {
                self.next_second = now + 1_000_000;
                self.draw_bar();
            }
            if now >= self.next_sweep {
                self.next_sweep = now + SWEEP_US;
                self.sweep();
            }
            let due = self.dirty.size() <= SMALL_CHANGE || now >= self.last_frame + FRAME_US;
            if due && now >= self.hold_until {
                self.frame();
            }
        }
    }
}

/// The window whose capability sits in slot `cap` of this task.
fn slot_of(cap: u32, windows: &[Window; MAX_WINDOWS]) -> usize {
    windows.iter().position(|window| window.used && window.cap == cap).unwrap_or(FIRST_CLIENT)
}

#[no_mangle]
pub fn main() {
    let [restarts, _, _, _] = rt::args();
    let me = client::task_named(INFO, b"display").unwrap_or(MAX_TASKS as u32);
    let mapped = sys::map_mem(SCREEN, VGA_BASE, 0, VGA_PAGES, true).is_ok();
    let (Some(wanted), Some(shown)) = (
        Bitmap::create(WANTED_BASE, STRIDE, HEIGHT),
        Bitmap::create(SHOWN_BASE, STRIDE, HEIGHT),
    ) else {
        sys::exit(1)
    };
    if !mapped {
        sys::exit(1);
    }
    let mut display = Display {
        restarts,
        me,
        wanted,
        shown,
        windows: [Window::NONE; MAX_WINDOWS],
        order: [0; MAX_WINDOWS],
        count: 0,
        dirty: Dirty::NOTHING,
        last_frame: 0,
        frames: 0,
        next_second: 0,
        next_sweep: 0,
        hold_until: 0,
        cells: [[b' '; COLUMNS]; LINES],
        column: 0,
        line: 0,
        mouse_seen: false,
        mouse_x: WIDTH as i32 / 2,
        mouse_y: HEIGHT as i32 / 2,
        buttons: 0,
        held: None,
    };
    Display::set_mode();
    // The card starts with whatever was in its memory; make it and the copy of it black.
    Display::select_planes(0x0F);
    for offset in 0..STRIDE * HEIGHT {
        card_write(offset, 0);
    }
    let terminal_width = COLUMNS * font::WIDTH + 2 * BORDER;
    let terminal_height = LINES * font::HEIGHT + TOP + BORDER;
    let made = display.create(BAR, 0, 0, WIDTH, BAR_HEIGHT, b"")
        && display.create(TERMINAL, TERMINAL_X, TERMINAL_Y, terminal_width, terminal_height, b"Terminal");
    if !made {
        sys::exit(1);
    }
    display.windows[TERMINAL].draw_frame(true);
    display.clear_terminal();
    display.terminal_write(b"");
    display.dirty.add(0, 0, WIDTH as i32, HEIGHT as i32);
    display.run()
}
