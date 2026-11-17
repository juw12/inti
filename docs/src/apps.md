# Write a program for Inti

A program for Inti is a small Rust file. Put it in `user/src/bin/`, build, and its name is a command at the `inti>` prompt. This page walks through the smallest program that keeps something between two runs, and then lists what a program can and cannot do.

## The example: a counter that survives a restart

`user/src/bin/counter.rs`, {{app.counter_lines}} lines, shown here exactly as it is built:

```rust
{{app.counter_source:raw}}
```

Build and run it:

```
python build.py run
```

At the prompt:

| You type | What happens |
|---|---|
| `counter` | the program holds only the console; it says it was not given the disk and how to run it |
| `counter +disk` | it reads `counter.txt`, adds one, saves the file and prints the run number |
| `counter +disk` again | the number is one higher |
| `shutdown`, then `python build.py run` again, then `counter +disk` | the number is one higher still: the file is on the disk |
| `cat counter.txt` | the file, as the shell sees it |

## Add your own

1. Copy `user/src/bin/counter.rs` to `user/src/bin/NAME.rs`. The name must be plain ASCII and fit the name field of the boot archive; the build stops with "archive name too long" when it does not. Do not take the name of a shell command: the shell would run its own command and never reach the program.
1. Change what `main` does.
1. Run `python build.py run`. Every file in `user/src/bin/` is compiled and put in the boot archive; there is no list to edit.
1. Type `NAME` or `NAME +disk` at the prompt. Numbers typed after the name reach the program; see "Arguments and input" below.

A program is `#![no_std]` and `#![no_main]`: there is no standard library and no heap. It defines `main` with `#[no_mangle]`, and it links the runtime library `rt` from `user/src/`, which starts the program, calls `main` and ends the task when `main` returns.

## What a program is given

A program can only use what it holds. The shell starts an ordinary program with:

| Slot | Name in `rt::app` | What it is | Given when |
|---|---|---|---|
| first | `CONSOLE` | a capability to send text to the console driver | always |
| second | `DISK` | a capability to send requests to the disk server | only when started as `NAME +disk` |
| third | `DISPLAY` | a capability to ask the display server for windows | only when started as `NAME +window` |

It also gets a memory quota of {{app.pages}} pages ({{app.memory_kib}} KiB) and a stack of {{app.stack_pages}} pages ({{app.stack_kib}} KiB). It gets no I/O ports, no interrupt lines, no capability for another program, and no way to ask for more. `counter` started without `+disk` shows what that means: its request for the disk is refused by the kernel, because the slot is empty, and the program is told so.

The test programs (`selftest`, `rogue`, `fuzz`, `bench`) are started differently, by name, because they test the kernel: `selftest`, `fuzz` and `bench` get extra capabilities and a larger quota, `rogue` gets room for one task only. That holds only for the file in the boot archive. A program you add is not on the list, and a copy of `selftest` on the disk is started like any other program: it fails its first checks, because it holds nothing extra. The test run checks this. The shell also refuses to write a disk file under the name of a file in the archive.

## Calls a program can make

Output, from `rt::io` and the macros of `rt`:

| Call | What it does |
|---|---|
| `set_output(Sink::Console(CONSOLE))` | send what the program prints to the console driver |
| `outln!(...)`, `out!(...)` | print text and numbers, with or without a line end |

Files, from `rt::client`. Each takes the capability slot first; with `DISK` it talks to the disk server.

| Call | What it does |
|---|---|
| `stat(disk, name)` | the size of a file |
| `read(disk, name, offset, buffer)` | read part of a file into a buffer, at most one message of `MSG_DATA` bytes per call; returns how many bytes |
| `list(disk, number)` | the name and size of the file with that number, to walk the directory |
| `save(disk, name, bytes)` | write a whole file in one step: after a power cut it is the old version or the new one |
| `append(disk, name, bytes)` | add to the end of a file, creating it if needed; each piece is added in one step |
| `remove(disk, name)` | remove a file |
| `create`, `write`, `commit` | the three steps `save` is made of, for a file that does not fit in one buffer |
| `disk_info(disk)` | data blocks, free blocks, number of files, and how long the server took to start |
| `disk_error(reason)` | the reason a call failed, as text |

`stat`, `read` and `list` return an `Option`: nothing when the file is not there or the server did not answer. The calls that change something, and `disk_info`, return a `Result` with the reason. The reasons a disk call can fail, as `disk_error` words them: "no ATA disk found", "no Inti volume on the disk", "the Inti volume is damaged", "no such file", "the disk is full", "the directory is full", "bad file name", "another writer started first", "the file changed in the meantime", "the disk reported an error", "no capability for the disk", "the disk server does not answer".

Windows, from `rt::client`. A program started as `NAME +window &` runs in the background and can open windows; `user/src/bin/clock.rs` is the smallest example.

| Call | What it does |
|---|---|
| `win_open(DISPLAY, width, height, title)` | open a window whose inside has that many pixels; returns the slot of the capability for it |
| `win_fill(window, x, y, width, height, colour)` | fill a rectangle inside the window with one of the {{display.colours}} colours of `rt::proto::colour` |
| `win_text(window, x, y, text_colour, background, bytes)` | draw text in the system font, {{font.width}} by {{font.height}} pixels a letter |
| `win_close(window)` | close the window and give its capability up |

`win_fill` and `win_text` return false when the window no longer exists, which is what happens after the display server was restarted; the program then opens a new window and draws again. A window gets no keyboard input.

The kernel's own system calls are in `rt::sys`; [How Inti works](design.md) lists them. An ordinary program holds nothing most of them could act on.

## Arguments and input

- **Numbers after the name.** The shell passes up to three numbers typed after the name, as in `fuzz 100000`. A program reads them with `rt::args()`, which returns four numbers; the fourth is the memory quota the program was started with. A word that is not a number arrives as zero. There are no text arguments.
- **A line from the keyboard.** A program that holds `CONSOLE` can call `client::readline(CONSOLE, prompt, buffer)`. The console driver refuses that only while the shell itself waits for a line, and the shell does not wait while a program runs. No program in the archive reads a line this way and no test covers it, so treat it as untried.

## Run a program from the disk

When a name is not a shell command, the shell looks for a file of that name in the boot archive and then on the disk. A disk file that is a program runs by its name, with the same two slots as above:

| You type | What happens |
|---|---|
| `cp hello hi` | the program file is copied to the disk |
| `hi` | the copy runs from the disk |
| `cp counter count` and `count +disk` | the copy counts in the same `counter.txt` |

The test run does the first two steps on every run. What is missing is a way to bring a new program onto a system that is already running: there is no network, no removable medium, and no tool on the build machine that adds a file to an existing volume. A file placed in `disk/` reaches only a newly made volume; `python build.py run --fresh` makes one, and discards the files of the old one.

## What a program does not have to do

- **Survive a driver restart.** When the console driver or the disk server is replaced while the program waits for an answer, the library sends the request again. The program sees a slower answer.
- **Clean up.** When a program ends or faults, the kernel takes back its memory and its capabilities.

## Limits

- **No per-program files.** A program started with `+disk` can read, replace and remove every file on the disk. The only protection is that a program started without it cannot touch the disk at all.
- **Numbers only as arguments, and keyboard input is untried.** See "Arguments and input" above. A file is the tested way to give a program its input.
- **No heap, no threads, no network.**
- **Windows are for output.** A program can draw in its windows; keys go to the terminal only, and a program is not told about the mouse.
- **No way to install a program on a running system.** A program runs from the boot archive or from the disk, but it gets to either only when the image or the volume is built.
- **Small files.** The disk holds at most {{disk.max_files}} files and {{disk.max_volume_mib}} MiB; see [Files that last](storage.md).
- **The resend after a server death has no test of its own.** [Files that last](storage.md) says where that matters.
