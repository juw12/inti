# /hone run log: Inti OS

Request (2026-11-10): "build me from scratch operating system the best in the world, find other
weakness and be the number one, /hone"

Goal beyond the goal: an operating system anyone can boot in a browser tab, in which a crashing
driver or a hostile program is demonstrably a non-event.

## Run queue

- [x] Q1 Freeze rubric (`rubric.md`), start cairn session, ping council
- [x] Q2 Toolchain: QEMU (portable, checksum verified), Rust i586 smoke test
- [x] Q3 Research: weaknesses of mainstream operating systems, each with a primary source (`docs/weaknesses.md`)
- [x] Q4 Boot sector + microkernel (memory, traps, tasks, IPC, capabilities, syscalls, ELF loader)
- [x] Q5 User space: init supervisor, console driver, file server, shell, hello, rogue, selftest, fuzz, bench
- [x] Q6 Test harness in QEMU: gates G1-G7, raw logs, `evidence/claims.json`, real screenshots
- [x] Q7 Browser demo (v86) + headless boot gate G8
- [x] Q8 README, design doc, comparison scorecard with limitations, claims check G9
- [ ] Q9 Dossier, council baseline v0, improvement loop (max 5 rounds, keep-best, pairwise)
- [ ] Q10 Sync: GitHub repo + Pages demo, profile README, portfolio card, landing page, Obsidian notes, `P --log`, cairn end

## Assumptions

- Assumed: "best in the world / number one" cannot be shown for a whole operating system in one run, so it is never claimed. The run delivers a real bootable OS that targets documented weaknesses of mainstream systems, with a scorecard that shows the measured axes where it leads and the axes where it loses.
- Assumed: name "Inti" (Indonesian for core / kernel), repository `juw12/inti`, MIT licence.
- Assumed: target is 32-bit x86 (i586) so that one disk image runs both in QEMU and in the browser (v86 has no 64-bit mode); language Rust (already installed), no assembler or C toolchain needed.
- Assumed: design is a capability microkernel with user-mode drivers and a restart supervisor, because the best-documented weaknesses of mainstream kernels are driver faults, memory-unsafe kernel code and ambient authority.
- Assumed: QEMU is fetched as a portable copy into the working folder (no system install, no admin rights); v86 (BSD-2-Clause) is vendored for the browser demo.
- Assumed: the project lives in the working folder `inti/`; nothing is written to Desktop, Documents or Downloads except the Obsidian notes the sync step requires.
- Assumed: the kernel clock is PIT channel 2 (free-running, read by latch), not the TSC and not tick counting. The test harness starts QEMU at above-normal priority, because a starved emulator makes that clock fall behind (measured on this host: 5.8 s guest against 6.6 s host at normal priority, 2.8 s against 2.8 s at above-normal priority).
- Assumed: timings use QEMU's default real-time clock (no `-icount`) and are reported next to a host-stopwatch cross-check; every timing is labelled as measured under emulation.
- Assumed: QEMU interrupt tracing (`-d int`) is off for every measurement; it adds roughly 100 us per trap.
- Assumed: port 0x61 (speaker gate) is kernel-reserved along with the two interrupt controllers and the timer.
- Assumed: a restarted console driver continues below the existing screen text instead of trusting the previous driver's hardware cursor.
- Assumed: supervisor policy is fixed constants: ping every 200 ms, 500 ms ping timeout, 500 ms grace after a restart, back-off from the third quick end; if init itself dies the kernel restarts user space at most once per second.
- Assumed: capabilities pass only at spawn (no capability transfer over IPC in this version); service start arguments are uniform; the shell's memory quota is fixed at boot; test programs get their extra capabilities by name; the fuzzer's children hold no Log capability.
- Assumed: the fuzz count is the kernel's per-task call count minus setup and report calls, so it is a lower bound; test programs find their own task entry by name.
- Assumed: the echo child inverts reply data so a reply can be told from an untouched buffer.
- Assumed: fuzzer child lifetime 400 ms, look interval 20 ms, audit interval 25 ms (was 60 / 10 / 5 ms; child spawns and audits dominated the run time before).
- Assumed: the fuzz banner reads "Fuzzing the kernel with N ..." because the harness strips `kernel: ` log lines and the old wording ("the kernel: N ...") was eaten by that filter; it was a harness artefact, not lost console output.
- Assumed: the browser demo uses v86 0.5.473, vendored under `docs/v86/` with its licence, speaker disabled; `build.py` copies every built image to `docs/inti.img` so the demo always serves the tested image.
- Assumed: the browser check sets `PLAYWRIGHT_SKIP_VALIDATE_HOST_REQUIREMENTS=1` (Playwright's dependency checker is a separate download behind the proxy) and falls back to an installed Edge or Chrome.
- Assumed: `rogue` has two forms. Plain `rogue` skips one hardware attempt (a forged `iret` frame to ring 0) because that attempt stops the v86 browser emulator with an internal error; it prints `skipped` and says so. `rogue all` runs every attempt and is what gate G5 uses under QEMU.
- Assumed: `tools/test.py` strips carriage returns from the saved serial log so that log excerpts in the documents can be compared line by line.
- Assumed: gate G7's `SAFETY:` rule is applied to the boot sector and the user programs as well as the kernel, and its line limit counts `kernel/src` plus `abi/src` (the kernel links the `abi` crate).
- Assumed: the documents are rendered from templates in `docs/src/`; a number may appear in a template only as a placeholder filled from `evidence/claims.json`, inside code, or in a paragraph or table row that carries a source URL.
- Assumed: a number written in code formatting in a document must occur as a literal in the Rust sources, the linker scripts, `build.py` or `tools/*.py` (rule 4 of `tools/check_claims.py`), so a constant quoted in the prose cannot drift from the code.
- Assumed: the cited sources are checked by `tools/check_sources.py`, which needs the network and is therefore run outside the gates. A source counts as confirmed when the quoted words are found at the cited address or on a named second copy (an OpenAlex abstract, or a university or project copy). A source that cannot be confirmed is listed as such, not dropped.
- Assumed: Herder et al. 2006 is no longer cited, because no copy a script could read supported the sentence it was cited for. The MINIX 3 documentation page and Tanenbaum, Herder and Bos 2006 carry that sentence instead.
- Assumed: the sentences about CrowdStrike, Palix et al. and Capsicum were reworded to follow the sources' own text, after the first wording said more than the sources do.
- Assumed: the third weakness (ambient authority) is cited to Capsicum, Hardy 1988, and Saltzer and Schroeder 1975.
- Assumed: two comments were corrected to match the code (one in `kernel/src/arch.rs`, one on `E_TIMEOUT`); no behaviour changed.
- Assumed: the two BIOS images in `docs/v86/` are under the GNU LGPL version 3. Checked against the v86 repository: its `bios/` folder carries that licence text and a script that builds both images from the SeaBIOS sources. Both licence texts were added next to the images.
- Assumed: the scratch probe files in `build/` were moved to `..\scratch_probes\` before the clean build of the full gates run. Nothing was deleted.

## Rubric

See `rubric.md` (id `inti-os-v1`, frozen before generating).

## Rounds

| round | change | gates | judge score | pairwise win | kept? | note |
|---|---|---|---|---|---|---|
| 2026-11-10 17:45 | council score | 7.95 | split: - | Pega-Vision=7.8, Pega-Thinking=7.95, GLM-5.3-Flash=8.35 |
