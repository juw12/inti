// Boots the Inti disk image in the v86 emulator and wires the page to it.
"use strict";

(function () {
    const PROMPT = "inti> ";
    const screen = document.getElementById("screen");
    const serialPanel = document.getElementById("serial");
    const status = document.getElementById("status");
    const machine = document.querySelector(".machine");
    const caption = document.getElementById("caption");
    const next = document.getElementById("next");
    const typed = document.getElementById("typed");
    const line = document.getElementById("line");
    const buttons = Array.from(document.querySelectorAll("button[data-command]"));
    const controls = buttons.concat([next, line, typed.querySelector("button")]);

    // How long the page waits for a step of the tour before it stops waiting.
    const STEP_LIMIT_MS = 30000;
    // The prompt can come back a moment before the last line of a step.
    const LATE_LINE_MS = 1500;

    function duration(microseconds) {
        const count = Number(microseconds);
        if (count < 1000) {
            return count + " \u00b5s";
        }
        const milliseconds = count / 1000;
        return (milliseconds < 10 ? milliseconds.toFixed(1) : Math.round(milliseconds)) + " ms";
    }

    // The guided tour. Each step is one shell command. `after` reads the numbers out of what
    // the machine printed, and returns null when the line it needs is not there.
    const TOUR = [
        {
            command: "rogue",
            label: "run a hostile program",
            before: "A program with no permissions tries to read kernel memory, touch hardware and forge permissions.",
            after: function (output) {
                const found = /ROGUE: (\d+)\/(\d+) attempts denied/.exec(output);
                return found && "The kernel refused " + found[1] + " of the " + found[2] +
                    " attempts it ran. Each one is listed in the log, with how it was stopped.";
            },
        },
        {
            command: "crash console",
            label: "crash the keyboard driver",
            before: "The keyboard driver crashes. It is an ordinary program, so the supervisor can replace it.",
            after: function (output) {
                const found = /RECOVERY: console crash -> answering again after (\d+) us/.exec(output);
                return found && "The keyboard driver crashed, and its replacement was answering after " +
                    duration(found[1]) + ".";
            },
        },
        {
            command: "crash display",
            label: "crash the display",
            before: "The program that draws this screen crashes.",
            after: function (output) {
                const found = /RECOVERY: display crash -> answering again after (\d+) us/.exec(output);
                return found && "The display crashed, a new one started, and the text was drawn again after " +
                    duration(found[1]) + ".";
            },
        },
        {
            command: "hang fs",
            label: "hang the file server",
            before: "The file server stops answering without crashing, which is harder to notice.",
            after: function (output) {
                const found = /RECOVERY: fs hang -> answering again after (\d+) us/.exec(output);
                const wait = /fs did not answer for (\d+) ms/.exec(output);
                return found && "The supervisor " + (wait ? "got no answer for " + wait[1] + " ms, " : "") +
                    "ended the file server and started a new one. It was answering again after " +
                    duration(found[1]) + ".";
            },
        },
        {
            command: "double",
            label: "two failures at once",
            before: "Two failures at once: the keyboard driver hangs while the file server crashes.",
            after: function (output) {
                const found = /DOUBLE FAILURE: fs answering again after (\d+) us, console after (\d+) us/.exec(output);
                return found && "Both were replaced. The file server answered again after " +
                    duration(found[1]) + ", the keyboard driver after " + duration(found[2]) + ".";
            },
        },
        {
            command: "uptime",
            label: "show the counters",
            before: "The kernel reports its counters and checks its own tables.",
            after: function (output) {
                const faults = /(\d+) faults contained/.exec(output);
                const anomalies = /(\d+) kernel anomalies/.exec(output);
                if (!faults || !anomalies) {
                    return null;
                }
                return faults[1] + " faults contained so far and " + anomalies[1] + " kernel anomalies" +
                    (output.includes("kernel invariants: all hold") ? ", and every kernel invariant holds" : "") +
                    ". Nothing was rebooted: this is the shell you started with.";
            },
        },
    ];

    // Everything the machine has written to its serial port, for the page and for tests.
    const state = {
        serial: "",
        ready: false,
        emulator: null,
        send: function (line) {
            state.emulator.serial0_send(line + "\r");
        },
    };
    window.inti = state;

    let pending = "";
    // The step of the tour that was run last (-1 before the first), and the one being waited for.
    let step = -1;
    let waiting = null;

    function say(lead, text) {
        const strong = document.createElement("b");
        strong.textContent = lead;
        caption.replaceChildren(strong, " " + text);
    }

    function showMachine() {
        const box = screen.getBoundingClientRect();
        if (box.top >= 0 && box.bottom <= window.innerHeight) {
            return;
        }
        const calm = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
        machine.scrollIntoView({ block: "start", behavior: calm ? "auto" : "smooth" });
    }

    function finishStep(text) {
        const lead = "Step " + (step + 1) + " of " + TOUR.length + ".";
        window.clearTimeout(waiting.limit);
        window.clearTimeout(waiting.late);
        waiting = null;
        say(lead, text || "This step ran, but the page did not find its result line. The log has what the machine reported.");
        next.disabled = false;
        next.textContent = step + 1 < TOUR.length ? "Next: " + TOUR[step + 1].label : "Run the tour again";
    }

    function checkStep() {
        if (!waiting || state.serial.length < waiting.mark + PROMPT.length || !state.serial.endsWith(PROMPT)) {
            return;
        }
        const text = TOUR[step].after(state.serial.slice(waiting.mark));
        if (text) {
            finishStep(text);
        } else if (!waiting.late) {
            waiting.late = window.setTimeout(function () {
                finishStep(TOUR[step].after(state.serial.slice(waiting.mark)));
            }, LATE_LINE_MS);
        }
    }

    function runStep() {
        step = step + 1 < TOUR.length ? step + 1 : 0;
        next.disabled = true;
        say("Step " + (step + 1) + " of " + TOUR.length + ".", TOUR[step].before);
        waiting = {
            mark: state.serial.length,
            late: 0,
            limit: window.setTimeout(function () {
                finishStep(TOUR[step].after(state.serial.slice(waiting.mark)));
            }, STEP_LIMIT_MS),
        };
        state.send(TOUR[step].command);
        showMachine();
    }

    function showLine(line) {
        const row = document.createElement("span");
        if (/^(kernel|init): /.test(line)) {
            row.className = "report";
        }
        row.textContent = line + "\n";
        serialPanel.appendChild(row);
    }

    function showSerial() {
        const stick = serialPanel.scrollTop + serialPanel.clientHeight >= serialPanel.scrollHeight - 30;
        const lines = pending.split("\n");
        pending = lines.pop();
        lines.forEach(showLine);
        if (stick) {
            serialPanel.scrollTop = serialPanel.scrollHeight;
        }
    }

    function onByte(byte) {
        if (byte === 13) {
            return;
        }
        const character = String.fromCharCode(byte);
        state.serial += character;
        pending += character;
        if (byte === 10) {
            showSerial();
        }
        if (!state.ready && state.serial.endsWith(PROMPT)) {
            state.ready = true;
            status.textContent = "Running. Start the tour, type a command, or press a button further down. " +
                "With a keyboard you can also click the screen and type.";
            status.className = "status ready";
            controls.forEach(function (control) {
                control.disabled = false;
            });
        }
        checkStep();
    }

    function start() {
        if (typeof V86 === "undefined" || typeof WebAssembly === "undefined") {
            status.textContent = "This browser cannot run the emulator (it needs WebAssembly).";
            return;
        }
        state.emulator = new V86({
            wasm_path: "v86/v86.wasm",
            memory_size: 64 * 1024 * 1024,
            vga_memory_size: 2 * 1024 * 1024,
            screen_container: screen,
            bios: { url: "v86/seabios.bin" },
            vga_bios: { url: "v86/vgabios.bin" },
            hda: { url: "inti.img", async: false },
            // Inti turns on the gate of timer channel 2 to use it as a clock. The emulator
            // would take that as "speaker on".
            disable_speaker: true,
            autostart: true,
        });
        state.emulator.add_listener("serial0-output-byte", onByte);
        state.emulator.add_listener("download-error", function () {
            status.textContent = "A file needed by the emulator could not be loaded.";
        });
        status.textContent = "Booting…";
    }

    buttons.forEach(function (button) {
        button.addEventListener("click", function () {
            state.send(button.dataset.command);
            screen.focus({ preventScroll: true });
            showMachine();
        });
    });

    next.addEventListener("click", runStep);

    // A phone shows no keyboard for the screen itself, so commands can be typed here too.
    typed.addEventListener("submit", function (event) {
        event.preventDefault();
        const command = line.value.trim();
        if (command && state.ready) {
            state.send(command);
            line.value = "";
        }
    });

    start();
})();
