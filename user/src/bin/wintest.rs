//! `wintest`: checks from user space that a window belongs to the program that opened it.
//!
//! Run it as `wintest +window &`. It holds the console, the display and the windows it
//! opens, like any program with a window, and tries what such a program must not be able
//! to do: draw without a window, turn its window capability into one for another window,
//! speak for the console, break the display, draw outside its window, draw in a window it
//! has closed, and open more windows than there is room for.
//!
//! Every check prints one line, `  ok   <name>` or `  FAIL <name>`; the last line is
//! `WINTEST: PASS n/n` or `WINTEST: FAIL p/n`. Then it keeps one window, filled with one
//! colour by a request for far more than the window, so that the test on the host can look
//! at the screen and see that nothing outside the window changed. When the display server
//! is restarted it says whether the old capability was refused, and opens a new window.
#![no_std]
#![no_main]

use rt::abi::{Msg, R_SEND};
use rt::app::{CONSOLE, DISPLAY};
use rt::io::{set_output, Sink};
use rt::proto::colour::{BLACK, CORAL, WHITE};
use rt::proto::{DBG_CRASH, DBG_HANG, DISP_MOUSE, DISP_TEXT, OK, PING, WINDOW_BADGE};
use rt::proto::{WIN_CLOSE, WIN_FILL, WIN_TEXT};
use rt::{client, outln, sys};

const WIDTH: u32 = 96;
const HEIGHT: u32 = 40;
/// The display has room for six windows of programs.
const ROOM: usize = 6;

struct Test {
    passed: u32,
    total: u32,
}

impl Test {
    fn check(&mut self, name: &str, ok: bool) {
        self.total += 1;
        if ok {
            self.passed += 1;
            outln!("  ok   ", name);
        } else {
            outln!("  FAIL ", name);
        }
    }
}

/// Send `label` through `cap`; true if the display did what was asked.
fn accepted(cap: u32, label: u32, words: [u32; 4]) -> bool {
    let mut msg = Msg::new(label);
    msg.words = words;
    client::request(cap, &mut msg).is_ok() && msg.label == OK
}

fn answers() -> bool {
    accepted(DISPLAY, PING, [0; 4])
}

/// Fill the whole window by asking for far more than the window.
fn flood(window: u32) -> bool {
    client::win_fill(window, 0, 0, 0xFFFF, 0xFFFF, CORAL)
}

fn checks(test: &mut Test) -> Option<u32> {
    let all = [0, 0, 100 | 100 << 16, WHITE as u32];
    test.check("without a window capability, fill, text and close are refused",
        !accepted(DISPLAY, WIN_FILL, all) && !accepted(DISPLAY, WIN_TEXT, all) && !accepted(DISPLAY, WIN_CLOSE, all));
    test.check("the display capability cannot be made into a window capability",
        (0..8).all(|slot| sys::cap_derive(DISPLAY, R_SEND, WINDOW_BADGE | slot, 0).is_err()));

    let first = client::win_open(DISPLAY, WIDTH, HEIGHT, b"Window test");
    test.check("a program with the display capability can open a window", first.is_some());
    let first = first?;
    test.check("it can draw in its own window",
        client::win_fill(first, 0, 0, WIDTH, HEIGHT, BLACK) && client::win_text(first, 4, 4, WHITE, BLACK, b"wintest"));
    // A copy that names the same window is allowed: of the badges tried, at most its own.
    let mut copies = 0;
    for restarts in (0..=0xFFFF).step_by(257) {
        for slot in 0..8 {
            if let Ok(copy) = sys::cap_derive(first, R_SEND, WINDOW_BADGE | restarts << 8 | slot, 0) {
                copies += 1;
                let _ = sys::cap_drop(copy);
            }
        }
    }
    test.check("its window capability cannot be made into one for another window", copies <= 1);
    test.check("a program cannot write in the terminal or move the pointer",
        [DISPLAY, first].iter().all(|&cap| !accepted(cap, DISP_TEXT, [0; 4]) && !accepted(cap, DISP_MOUSE, [5, 5, 0, 0])));
    test.check("a program cannot make the display crash or hang",
        [DISPLAY, first].iter().all(|&cap| !accepted(cap, DBG_CRASH, [0; 4]) && !accepted(cap, DBG_HANG, [0; 4])) && answers());
    test.check("requests that reach outside the window are cut off or ignored",
        flood(first)
            && accepted(first, WIN_FILL, [0xFFFF_FFF0, 0xFFFF_FFF0, 0xFFFF_FFFF, 1])
            && accepted(first, WIN_TEXT, [0xFFFF_FFFF, 0, 0xFF, 0])
            && accepted(first, WIN_TEXT, [0, 0xFFFF_FFFF, 0xFF, 0])
            && answers());

    let second = client::win_open(DISPLAY, WIDTH, HEIGHT, b"Second");
    test.check("it can open a second window", second.is_some());
    let second = second?;
    test.check("after it closed a window, the capability for it draws nothing",
        accepted(second, WIN_CLOSE, [0; 4]) && !flood(second) && !client::win_text(second, 0, 0, WHITE, BLACK, b"x"));
    let _ = sys::cap_drop(second);

    // The other windows on the screen count too, so this opens what there is room for.
    let mut more = [None; ROOM + 1];
    for window in more.iter_mut() {
        *window = client::win_open(DISPLAY, 16, 12, b"More");
    }
    let opened = more.iter().flatten().count();
    test.check("the display refuses a window when there is no room for one",
        (1..ROOM).contains(&opened) && more[ROOM].is_none() && answers());
    for window in more.into_iter().flatten() {
        client::win_close(window);
    }
    test.check("the first window is still there after all this", flood(first));
    Some(first)
}

#[no_mangle]
pub fn main() {
    set_output(Sink::Console(CONSOLE));
    if !answers() {
        return outln!("wintest: this program was not given the display; run: wintest +window &");
    }
    let mut test = Test { passed: 0, total: 0 };
    let window = checks(&mut test);
    if test.passed == test.total {
        outln!("WINTEST: PASS ", test.passed, "/", test.total);
    } else {
        outln!("WINTEST: FAIL ", test.passed, "/", test.total);
    }
    let Some(mut window) = window else { return };
    loop {
        sys::sleep(100);
        if client::win_fill(window, 0, 0, 1, 1, CORAL) {
            continue;
        }
        // The display was restarted and took the window with it.
        let _ = sys::cap_drop(window);
        loop {
            if let Some(new) = client::win_open(DISPLAY, WIDTH, HEIGHT, b"Window test") {
                window = new;
                break;
            }
            sys::sleep(100);
        }
        if flood(window) {
            outln!("WINTEST: the old window was refused after a display restart; a new one is open");
        }
    }
}
