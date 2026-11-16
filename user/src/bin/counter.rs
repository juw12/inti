//! `counter`: the smallest program that keeps something between two runs.
//!
//! It counts how often it was run, in the file `counter.txt` on the disk. The shell gives
//! a program the disk only when asked to: `counter +disk`. Started as plain `counter`, the
//! program holds one capability, the console, and the kernel refuses its disk request.
#![no_std]
#![no_main]

use rt::app::{CONSOLE, DISK};
use rt::io::{set_output, Sink};
use rt::proto::D_DENIED;
use rt::{client, outln};

const FILE: &[u8] = b"counter.txt";

#[no_mangle]
pub fn main() {
    set_output(Sink::Console(CONSOLE));
    match client::disk_info(DISK) {
        Ok(_) => {}
        Err(D_DENIED) => {
            return outln!("counter: this program was not given the disk; run: counter +disk")
        }
        Err(reason) => return outln!("counter: ", client::disk_error(reason)),
    }
    let mut text = [0; 16];
    let len = client::read(DISK, FILE, 0, &mut text).unwrap_or(0);
    let before = text[..len]
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .fold(0u32, |sum, byte| sum.wrapping_mul(10).wrapping_add((byte - b'0') as u32));
    let runs = before.wrapping_add(1);

    // The number as text, written from the right, with a line end after it.
    let mut at = text.len() - 1;
    text[at] = b'\n';
    let mut rest = runs;
    loop {
        at -= 1;
        text[at] = b'0' + (rest % 10) as u8;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    // `save` replaces the file in one step: after a power cut it holds the old number or
    // the new one.
    match client::save(DISK, FILE, &text[at..]) {
        Ok(()) => outln!("counter: run number ", runs, ", saved in ", FILE),
        Err(reason) => outln!("counter: ", FILE, ": ", client::disk_error(reason)),
    }
}
