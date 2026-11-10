// Boots the Inti disk image in the v86 emulator and wires the page to it.
"use strict";

(function () {
    const PROMPT = "inti> ";
    const screen = document.getElementById("screen");
    const serialPanel = document.getElementById("serial");
    const status = document.getElementById("status");
    const buttons = Array.from(document.querySelectorAll("button[data-command]"));

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
            status.textContent = "Running. Click the screen to type, or use the buttons below.";
            status.className = "status ready";
            buttons.forEach(function (button) {
                button.disabled = false;
            });
        }
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
            screen.focus();
        });
    });

    start();
})();
