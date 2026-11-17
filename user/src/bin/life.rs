//! `life`: Conway's Game of Life in a window.
//!
//! Start it with `life +window &`; a number after the name chooses the first pattern.
//! Like `clock` it draws through the capability for its own window and opens a new window
//! when the display server has been restarted. It runs until it is stopped: `stop life`.
#![no_std]
#![no_main]

use rt::abi::Msg;
use rt::app::{CONSOLE, DISPLAY};
use rt::io::{set_output, Sink};
use rt::proto::colour::{BLACK, LIME};
use rt::proto::PING;
use rt::{client, outln, sys};

const COLUMNS: usize = 30;
const ROWS: usize = 30;
/// A cell is a square of this many pixels, the last of them left dark.
const CELL: u32 = 4;
const WIDTH: u32 = COLUMNS as u32 * CELL;
const HEIGHT: u32 = ROWS as u32 * CELL;
const STEP_MS: u32 = 200;
/// After this many generations the board starts again from a new pattern.
const GENERATIONS: u32 = 600;

type Board = [[bool; COLUMNS]; ROWS];

/// A sequence of numbers that looks random (xorshift).
struct Random(u32);

impl Random {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0
    }
}

fn seed(random: &mut Random) -> Board {
    let mut board = [[false; COLUMNS]; ROWS];
    for row in board.iter_mut() {
        for cell in row.iter_mut() {
            *cell = random.next() % 4 == 0;
        }
    }
    board
}

/// The next generation. The board wraps around at its edges.
fn step(board: &Board) -> Board {
    let mut next = [[false; COLUMNS]; ROWS];
    for (y, row) in next.iter_mut().enumerate() {
        for (x, cell) in row.iter_mut().enumerate() {
            let mut neighbours = 0;
            for dy in [ROWS - 1, 0, 1] {
                for dx in [COLUMNS - 1, 0, 1] {
                    let other = board[(y + dy) % ROWS][(x + dx) % COLUMNS];
                    neighbours += (other && (dy, dx) != (0, 0)) as u32;
                }
            }
            *cell = neighbours == 3 || (neighbours == 2 && board[y][x]);
        }
    }
    next
}

/// Draw the cells that differ from what the window shows; all of them if `shown` is
/// `None`. False if the window is gone.
fn draw(window: u32, board: &Board, shown: Option<&Board>) -> bool {
    if shown.is_none() && !client::win_fill(window, 0, 0, WIDTH, HEIGHT, BLACK) {
        return false;
    }
    for y in 0..ROWS {
        for x in 0..COLUMNS {
            let alive = board[y][x];
            if shown.map_or(alive, |old| old[y][x] != alive) {
                let colour = if alive { LIME } else { BLACK };
                let (left, top) = (x as u32 * CELL, y as u32 * CELL);
                if !client::win_fill(window, left, top, CELL - 1, CELL - 1, colour) {
                    return false;
                }
            }
        }
    }
    true
}

#[no_mangle]
pub fn main() {
    set_output(Sink::Console(CONSOLE));
    if client::request(DISPLAY, &mut Msg::new(PING)).is_err() {
        return outln!("life: this program was not given the display; run: life +window &");
    }
    let chosen = rt::args()[0];
    let mut random = Random(if chosen != 0 { chosen } else { sys::time() as u32 | 1 });
    let mut board = seed(&mut random);
    let mut generation = 0;
    let mut window = None;
    let mut shown: Option<Board> = None;
    loop {
        let Some(slot) = window else {
            // No window yet, or the display was restarted and took the old one with it.
            window = client::win_open(DISPLAY, WIDTH, HEIGHT, b"Life");
            shown = None;
            if window.is_none() {
                sys::sleep(200);
            }
            continue;
        };
        if !draw(slot, &board, shown.as_ref()) {
            let _ = sys::cap_drop(slot);
            window = None;
            continue;
        }
        shown = Some(board);
        sys::sleep(STEP_MS);
        generation += 1;
        board = if generation % GENERATIONS == 0 { seed(&mut random) } else { step(&board) };
    }
}
