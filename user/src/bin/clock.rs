//! `clock`: a small window that shows how long the system has been up.
//!
//! The shell gives a program the display only when asked to: `clock +window &`. The
//! program then holds two capabilities, the console and the display, and after it has
//! opened its window a third: the window. That one is all it can draw with, and it reaches
//! this window and no other.
//!
//! When the display server is restarted the window is gone, and drawing in it fails; the
//! program then opens a new one. It runs until it is stopped: `stop clock`.
#![no_std]
#![no_main]

use rt::abi::Msg;
use rt::app::{CONSOLE, DISPLAY};
use rt::io::{set_output, Sink};
use rt::proto::colour::{AMBER, BLACK, GREY, WHITE};
use rt::proto::PING;
use rt::{client, font, outln, sys};

/// "up 000:00:00": hours, minutes and seconds.
const LETTERS: u32 = 12;
const MARGIN: u32 = 8;
const WIDTH: u32 = LETTERS * font::WIDTH as u32 + 2 * MARGIN;
const BAR_Y: u32 = MARGIN + font::HEIGHT as u32 + 6;
const BAR_HEIGHT: u32 = 4;
const HEIGHT: u32 = BAR_Y + BAR_HEIGHT + MARGIN;

fn two(value: u32, field: &mut [u8]) {
    field[0] = b'0' + (value / 10 % 10) as u8;
    field[1] = b'0' + (value % 10) as u8;
}

/// Draw the time and, below it, a bar that fills up once a minute. False if the window
/// is gone.
fn draw(window: u32, seconds: u32, all: bool) -> bool {
    let mut text = *b"up 000:00:00";
    let hours = seconds / 3600 % 1000;
    text[3] = b'0' + (hours / 100) as u8;
    two(hours, &mut text[4..6]);
    two(seconds / 60 % 60, &mut text[7..9]);
    two(seconds % 60, &mut text[10..12]);
    let bar = WIDTH - 2 * MARGIN;
    let filled = bar * (seconds % 60 + 1) / 60;
    if all || seconds % 60 == 0 {
        if !client::win_fill(window, 0, 0, WIDTH, HEIGHT, BLACK)
            || !client::win_fill(window, MARGIN, BAR_Y, bar, BAR_HEIGHT, GREY)
        {
            return false;
        }
    }
    client::win_text(window, MARGIN, MARGIN, WHITE, BLACK, &text)
        && client::win_fill(window, MARGIN, BAR_Y, filled, BAR_HEIGHT, AMBER)
}

#[no_mangle]
pub fn main() {
    set_output(Sink::Console(CONSOLE));
    if client::request(DISPLAY, &mut Msg::new(PING)).is_err() {
        return outln!("clock: this program was not given the display; run: clock +window &");
    }
    let mut window = None;
    let mut shown = u32::MAX;
    loop {
        let seconds = (sys::time() / 1_000_000) as u32;
        let Some(slot) = window else {
            // No window yet, or the display was restarted and took the old one with it.
            window = client::win_open(DISPLAY, WIDTH, HEIGHT, b"Clock");
            match window {
                Some(slot) if draw(slot, seconds, true) => shown = seconds,
                Some(_) => {}
                None => sys::sleep(200),
            }
            continue;
        };
        if seconds != shown {
            if draw(slot, seconds, false) {
                shown = seconds;
            } else {
                let _ = sys::cap_drop(slot);
                window = None;
                continue;
            }
        }
        sys::sleep(100);
    }
}
