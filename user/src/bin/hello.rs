//! The smallest Inti program. It is started with exactly one capability: the right to
//! send text to the console.
#![no_std]
#![no_main]

use rt::io::{set_output, Sink};
use rt::outln;

/// Capability slot 0: send right on the console endpoint.
const CONSOLE: u32 = 0;

#[no_mangle]
pub fn main() {
    set_output(Sink::Console(CONSOLE));
    outln!("Hello from user space.");
    outln!("This program holds one capability (console output) and can do nothing else.");
}
