# Inti

**A small operating system, written from scratch, that you can break on purpose.**

Inti (Indonesian for "core") is a boot sector, a capability microkernel of {{loc.kernel_total}} lines of Rust, and device drivers that run as ordinary programs. It was built to take three well-documented weaknesses of mainstream operating systems, do the opposite, and measure what happens.

**[Try it in your browser](https://juw12.github.io/inti/)**. There is nothing to install: the page boots the same disk image that the tests boot in QEMU, and its first button is a guided tour of {{tour.steps}} announced steps that takes under a minute ({{web.tour_seconds}} s in the last browser check): a hostile program, a crashed driver, a hung file server, two failures at once and the kernel's counters. Other buttons do each of these on its own and fuzz the kernel.

Inti is not the best operating system in the world, and nothing here says so. It is a research toy: no network, no disk file system, no applications, one processor, and it has only ever run in emulators. [The scorecard](docs/scorecard.md) shows where Linux, Windows, seL4, MINIX 3 and Redox are ahead, which is nearly everywhere outside the three points below.

| A hostile program is denied | A driver fails and is replaced |
|---|---|
| ![The hostile program's attempts, each one denied](docs/img/qemu-rogue.png) | ![A driver failing and being restarted](docs/img/qemu-recovery.png) |

| The kernel is fuzzed | The same disk image in a browser tab |
|---|---|
| ![The report of the fuzz run](docs/img/qemu-fuzz.png) | ![The browser demo](docs/img/web-demo.png) |

These are captures of the emulated screen, taken by the test scripts during the run that produced the numbers below.

## Three weaknesses, and what Inti does instead

| Weakness of mainstream systems | What Inti does | Measured on this build |
|---|---|---|
| **A driver bug takes the whole system down.** Drivers run inside the kernel. Device drivers caused 85% of recently reported Windows XP failures ([Swift, Bershad and Levy, SOSP 2003](https://dl.acm.org/doi/10.1145/945445.945466)), and on 19 July 2024 one faulty update for a kernel driver crashed an estimated 8.5 million Windows devices ([Microsoft](https://blogs.microsoft.com/blog/2024/07/20/helping-our-customers-through-the-crowdstrike-outage/)). | Drivers are ordinary programs with no kernel privileges. A supervisor restarts a driver that crashes or stops answering, and programs that were waiting get their answer from the new one. | A crashed console driver answers again after {{recovery.console_crash.median_ms}} ms (median of {{recovery.console_crash.runs}} runs), a hung one after {{recovery.console_hang.median_ms}} ms. With the console driver hung and the file server crashed at the same moment, both answer again (after {{double.console_ms}} ms and {{double.fs_ms}} ms). When the supervisor itself is crashed, the kernel restarts user space. |
| **Kernels are written in memory-unsafe languages.** About 70% of the vulnerabilities Microsoft assigns a CVE each year are memory-safety issues ([MSRC, 2019](https://www.microsoft.com/en-us/msrc/blog/2019/07/a-proactive-approach-to-more-secure-code)); Chromium reports the same share of its high-severity bugs ([Chromium](https://www.chromium.org/Home/chromium-security/memory-safety)). | The kernel is Rust with no heap: every table has a fixed size. Each of its {{count.unsafe_kernel}} `unsafe` blocks carries a written justification, and a checker inside the kernel recounts everything the kernel owns against {{count.invariant_rules}} rules. | {{fuzz.calls}} random system calls from {{fuzz.tasks}} throw-away programs: {{fuzz.violations}} rule violations in {{fuzz.checks}} checks, {{fuzz.anomalies}} kernel anomalies, {{fuzz.leaks}} leaked resources. |
| **Programs run with all the rights of their user.** A program that acts with the full power of its user can reach everything that user can; Watson and others call such implicit access ambient authority ([Capsicum, USENIX Security 2010](https://www.usenix.org/legacy/event/sec10/tech/full_papers/Watson.pdf)). | There is no ambient authority. Every system call starts from a capability the program was handed, and a capability can be narrowed but never widened. | A hostile program that holds one capability makes {{rogue.all_total}} attempts on the kernel, the hardware and other programs. {{rogue.all_denied}} are denied and the system stays up. |

None of these ideas is new: MINIX 3 restarts user-mode drivers, seL4 is a capability kernel with proofs, Redox is a microkernel system in Rust. Inti is a small system that combines them and shows its evidence. [Weaknesses and sources](docs/weaknesses.md) has the details and what Inti does not solve.

## What was measured

`python build.py gates` builds from a clean tree, runs every test in QEMU, boots the same image in a headless browser and checks these documents. Result of {{gates.date}} ({{test.emulator}} on {{test.host}}): **{{gates.passed}} of {{gates.total}} gates pass.**

{{table.gates}}

Two more checks run on the build machine, outside QEMU, and are not gates. `python build.py model` runs the kernel's own source against a simulated machine: {{model.tests_passed}} of {{model.tests}} directed tests pass, the kernel's checker reports {{model.seeds_found}} of {{model.seeds}} corruptions planted in its state, and a bounded exploration of {{model.states}} states and {{model.transitions}} steps meets {{model.violations}} violations. `python build.py mutants` then breaks the kernel on purpose, one small change at a time, in {{mutants.sample}} places taken at even spacing. The places are redrawn whenever the kernel's source changes. It last changed a round ago, when {{mutants.new_places}} of the {{mutants.sample}} were new. The first run on them caught {{mutants.before.caught}} of {{mutants.before.compiled}} broken kernels. After directed tests were written for what had got away, the same places give {{mutants.caught}} of {{mutants.compiled}}, which says how well those tests were aimed. A second sample of {{mutants.held_out.sample}} places gave {{mutants.held_out.before.caught}} of {{mutants.held_out.before.compiled}} while no test was aimed at it. This round tests were written for six of its misses, none of which turned out to be a fault in the kernel, and it gives {{mutants.held_out.caught}} of {{mutants.held_out.compiled}}. A third sample of {{mutants.fresh.sample}} places, drawn after those tests and with no test written against it, gives {{mutants.fresh.caught}} of {{mutants.fresh.compiled}}, and that is the figure to read. [How Inti works](docs/design.md#the-model) lists every broken kernel that got through, with the reason. The first run of the directed tests found one defect in the kernel and four gaps in its checker. They are fixed, and the failing run is kept in `evidence/model-before-fixes.log`. All of this is testing of small configurations to a small depth. It is not a proof.

All timings are emulator timings. They show that a restart is quick compared with a reboot; they say nothing about real hardware, which Inti has never run on.

A driver crash, exactly as the serial port recorded it:

```log
{{log.crash_console}}
```

The hostile program (a sample; `...` marks lines left out, and the full list is in [weaknesses and sources](docs/weaknesses.md)):

```log
{{log.rogue_short}}
```

## Try it

In the browser: open the [live demo](https://juw12.github.io/inti/) and press "Take the guided tour" or any other button, or click the screen and type.

On your own machine, with QEMU installed, `python build.py run` builds the image and boots it in a window (`run.bat` does the same on Windows). Things to type at the `inti>` prompt:

| Command | What happens |
|---|---|
| `help` | lists every command |
| `tour` | the guided tour: a hostile program, a driver crash, a server hang, two failures at once and the kernel's counters, each step announced before it runs |
| `crash console` | the screen and keyboard driver dies of a page fault; the supervisor replaces it and the shell prints how long that took |
| `hang fs` | the file server stops answering; the supervisor notices, ends it and starts a new one |
| `double` | the console driver hangs and, while the supervisor is waiting for it, the file server crashes; both are replaced |
| `rogue` | a hostile program tries to read kernel memory, touch hardware, run code from its own data, forge or revoke capabilities and exceed its quotas |
| `fuzz {{fuzz.default_calls:raw}}` | random system calls from throw-away programs while the kernel checks its own rules |
| `selftest` | {{selftest.checks}} promises of the kernel, checked one by one from an ordinary program |
| `crash init` | the supervisor itself dies; the kernel starts every program again |
| `svc`, `ps`, `caps <task>`, `mem`, `uptime` | what is running, who holds which capabilities and what each was copied from, quotas, memory, counters |

## Build and test

You need Rust with the `i586-unknown-linux-gnu` target, Python {{build.python_min}} or newer, and QEMU for everything that runs the image. There is no assembler, no C compiler and no make: the boot sector is written in Rust too.

```
rustup target add i586-unknown-linux-gnu
python build.py          # compile everything and write build/inti.img
python build.py run      # boot the image in a QEMU window
python build.py test     # run every test in QEMU
python build.py gates    # every check, from a clean build
python build.py model    # the kernel's source on this machine: directed tests, bounded exploration
python build.py mutants  # break the kernel on purpose and see which checks notice
python build.py mutants --held-out  # the same in a second sample of places
python build.py mutants --fresh     # and in a third, drawn after tests were aimed at the second
```

The model and the mutation check need a Rust toolchain for the build machine itself; on Windows `build.py` uses the GNU one. They are not part of `gates`. The browser check also needs Playwright with Chromium (`pip install playwright`, then `playwright install chromium`). The last gate run used {{gates.rustc}}, Python {{gates.python}} and {{test.emulator}}; its test run took {{test.duration_s}} s.

## How it works

- **Boot.** A boot sector of {{size.boot_sector_bytes}} bytes, written in Rust, loads the kernel through the BIOS. No GRUB, no multiboot.
- **Kernel.** {{loc.kernel_total}} lines of Rust, {{size.kernel_kib}} KiB when built. It hands out memory, switches between tasks, passes messages, checks capabilities and forwards interrupts, through {{count.syscalls}} system calls. It has no heap, no drivers and no file system. It uses PAE paging and, where the processor has the no-execute bit, enforces W^X: no page is both writable and executable, for programs and for the kernel itself.
- **Capabilities.** A program can only name what it holds: {{count.cap_types}} kinds of capability (endpoints, I/O ports, interrupt lines, memory, tasks, the kernel log, kernel information). No system call takes a global name such as a process id or a path. A capability can be copied with fewer rights but never more, passed on in a message when both sides hold the grant right, and revoked: whoever handed it out takes back every copy while the holders keep running.
- **Quotas.** Memory, tasks and endpoints are lent by a parent to its children and returned when they end, so a program can exhaust its own share and nothing else. Processor time is shared out as well: a task gets its parent's percentage or a smaller one, everything it starts counts against it, and a task that has used its percentage of a second runs only when no task within its share wants the processor. In QEMU a program beside three spinning tasks kept {{shares.boot.kept_held}}% of its speed when they were held to {{shares.boot.limit}}% and {{shares.boot.kept_free}}% when they were not.
- **Drivers.** The console driver (screen, keyboard, serial port) and the file server are ordinary programs, each started with a handful of capabilities.
- **Supervisor.** `init` holds every capability at boot, hands each server the few it needs, drops its own capabilities for all ports, all interrupt lines and the screen, and restarts a server that crashes or stops answering. If `init` itself dies, the kernel starts user space again.
- **Self-checking.** `selftest` checks the kernel's promises from user space, the invariant checker inside the kernel recounts every frame, capability and queue, and `fuzz` throws random system calls at the kernel while that checker runs.

[How Inti works](docs/design.md) has the details: the system calls, the supervisor's policy, the invariant rules, what has to be trusted, and the full list of limitations.

## Where it loses

Inti leads only on the narrow things it was built to show, against systems that were built to do far more. It supports no hardware beyond an emulated PC, and it has no network, no persistent storage, no applications and one processor. Its kernel is tested, self-checking and explored on a host to a small depth, but not proven: seL4 is formally verified and Inti is not. MINIX 3 showed restartable user-mode drivers at real scale years ago, and Redox is a Rust microkernel system that runs real programs. The [scorecard](docs/scorecard.md) goes through this point by point.

## About the numbers

No number in these documents is typed by hand unless a source link stands next to it. `tools/claims.py` computes each one from the raw serial log, the test results or the source code and writes it to `evidence/claims.json` together with the file it came from. The documents are rendered from templates in `docs/src/`. `tools/check_claims.py` then recomputes everything and rejects a document that contains a number which is neither computed, nor cited, nor present in the source code; it also compares every line of every log excerpt with the raw log.

## Repository layout

| Path | What is there |
|---|---|
| `boot/` | the boot sector ({{loc.boot}} lines) |
| `kernel/` | the kernel ({{loc.kernel}} lines) |
| `abi/` | the interface that the kernel and the programs share ({{loc.abi}} lines) |
| `user/` | the runtime library, the supervisor, the drivers, the shell and the test programs ({{loc.user}} lines) |
| `model/` | the kernel's source compiled for the build machine, with directed tests and a bounded explorer ({{loc.model}} lines) |
| `build.py`, `tools/` | the build, test, gate and document scripts ({{loc.tools}} lines of Python) |
| `evidence/` | the raw logs and every measured number of the last gate run, the last model run and the last mutation run |
| `docs/` | the browser demo, these documents and their templates |
| `rubric.md`, `refine_log.md` | the criteria this project was built against, and the log of the run that built it |

## Licence

MIT, see `LICENSE`. The browser demo uses the [v86](https://github.com/copy/v86) emulator (BSD 2-Clause licence) and the BIOS images that ship with it; see `docs/v86/NOTICE.txt`.
