# How Inti works

This document describes the system as it is built. Every number in it comes from the source code or from the gate run of {{gates.date}}, and the list of limitations at the end is meant to be complete.

## The idea

A mainstream kernel contains its drivers, its file systems and its network stack, all running with full control of the machine. Inti's kernel contains none of them. It does five things: it hands out memory, switches between tasks, passes messages, checks capabilities and forwards interrupts. Everything else is an ordinary program.

```
ring 3   shell, hello, rogue, selftest, fuzz, bench     programs
         console driver, file server                    servers: restarted when they fail
         init                                           supervisor: starts with every capability
------------------------------ system calls ------------------------------
ring 0   kernel: memory, tasks, messages, capabilities, interrupts
---------------------------------------------------------------------------
         boot sector
```

{{table.loc}}

The gate limit for the kernel is {{loc.limit}} lines, counted without blank lines and comments. The boot sector is another {{loc.boot}} lines and user space {{loc.user}}.

## Boot

The boot sector is {{size.boot_sector_bytes}} bytes of Rust (`boot/src/main.rs`); there is no GRUB and no multiboot header in between. It reads the kernel and the boot archive from the disk with BIOS calls, asks the BIOS for the memory map, enables the A20 line, switches to 32-bit protected mode, copies what it read to address `0x100000` and jumps there.

The kernel (`kernel/src/main.rs`) sets up its descriptor tables, the interrupt controller and the timer, builds a frame allocator from the memory map, turns paging on and starts exactly one task, `init`, from the boot archive. The archive is a flat, read-only list of named files that follows the kernel on the disk. It holds {{size.files}} files: {{size.programs}} programs and a text file.

```log
{{log.boot}}
```

The prompt appeared {{boot.guest_ms}} ms after the kernel started, by the system's own clock, and {{boot.host_ms}} ms after the test script started the emulator.

{{table.sizes}}

## Memory

- Pages are {{limits.page_bytes}} bytes. Programs live between `0x4000_0000` and `0xC000_0000`. The kernel maps the first {{limits.max_ram_mib}} MiB of physical memory for itself, out of reach of ring 3.
- The kernel never dereferences an address a program gave it. It walks the task's page tables in software and touches the frame through its own mapping, so a bad pointer is an error code (`E_FAULT`) and never a page fault inside the kernel.
- Every frame has exactly one owner and every task has a memory quota. A child's quota is taken out of its parent's when it is started and returns when the child is gone, so a program cannot use more memory than it was given, however many children it starts.
- Program code is mapped read-only, so a program cannot overwrite its own code.
- The kernel has no heap. All of its state is one value made of fixed-size tables: {{limits.tasks}} tasks, {{limits.endpoints}} endpoints, {{limits.irq_lines}} interrupt lines. Nothing is allocated at run time, so nothing can leak; a full table is the error `E_NOSLOT`.

## Tasks

A task is an address space, one thread of execution, a table of capabilities and a memory quota. Scheduling is round-robin, and a timer interrupt every {{timer.period_ms}} ms pre-empts a task that does not yield. Tasks form a tree: a task that ends takes its children with it, and its parent receives a message from the kernel that says how it ended (its exit code, or the processor exception and the address). An exception in ring 3 only ever ends the task that caused it.

The clock is channel 2 of the 8254 timer, running free. It has a resolution of {{clock.resolution_ns}} ns and wraps every {{clock.wrap_ms}} ms; the kernel extends it to a count of microseconds since boot. Checked against the host's clock during the test run, it ran at {{clock.idle_ratio}} of real time while idle and {{clock.fuzz_ratio}} during the fuzz run.

## Capabilities

A capability is an entry in a task's own table of {{limits.cap_slots}} slots. System calls name slots, never objects. There are {{count.cap_types}} kinds:

| Capability | What it allows |
|---|---|
| Endpoint | sending to, or receiving from, one message queue; it carries rights (send, receive, grant) and a badge |
| IoPort | reading and writing one range of I/O ports |
| Irq | having a set of interrupt lines delivered to an endpoint |
| Memory | mapping one range of physical memory (device memory, the boot archive), read-only or writable |
| Task | ending one task |
| Log | writing to the kernel log |
| Info | reading kernel counters and tables, and running the invariant checker |

The rules:

- A task starts with the capabilities its parent chose to copy into it, at most {{limits.spawn_caps}}, and nothing else. `init` starts with all of them.
- `cap_derive` copies a capability with the same rights and range, or less: fewer rights, a smaller range of ports, fewer interrupt lines, fewer pages, read-only instead of writable. Nothing widens a capability.
- A holder of the grant right can stamp a badge on a copy, once. The kernel puts the badge of the capability that was used into every message, so a server knows which client is calling without trusting what the client says about itself.
- No system call takes a global name. There are no process ids, no paths and no physical addresses in the interface: what a task was not given, it cannot even mention.
- The ports of the interrupt controllers, the timer and the speaker gate stay with the kernel. It refuses them even to a task whose capability covers them.

## Messages

One mechanism carries everything. A client calls an endpoint and blocks until the server replies; both sides can set a time limit. A message has a label, a few machine words and up to {{limits.msg_data}} bytes of data.

- A message is copied through a kernel buffer. Tasks never share memory through a message, so a sender cannot change it after the kernel has checked it.
- The server answers through a reply token that works once. A task cannot answer a call it did not receive.
- Interrupts, and the ends of supervised tasks, arrive on endpoints as messages from the kernel. Their labels are reserved: a program that tries to send one is refused.
- A caller whose server dies while handling its request is told so (`E_DEAD`), and the client library sends the request again to the new server. A request that was still waiting in the queue just waits for the new server.

## System calls

This is the complete kernel interface: {{count.syscalls}} calls through interrupt vector `0x80`.

{{table.syscalls}}

{{table.errors}}

## Drivers and servers

- **`console`** drives the screen, the keyboard and the serial port. It holds its request endpoint, the serial ports `0x3F8` to `0x3FF`, the keyboard data port `0x60`, the cursor ports `0x3D4` and `0x3D5`, two interrupt lines and one page of screen memory. It does not hold the keyboard controller's command port `0x64`, which can reset the machine.
- **`fs`** serves the files of the boot archive. It holds its request endpoint and the archive as read-only memory. It keeps no state between requests, so a restart loses nothing.
- **`shell`** is the command line. A program it starts gets one capability, the right to send text to the console, and a small memory quota. The test programs (`selftest`, `fuzz`, `bench`) get a few more, listed at the top of their source files.

Each server gets a memory quota of {{supervisor.server_pages}} pages.

## Supervision

`init` is the root task and the supervisor of {{supervisor.services}} services: the two servers and the shell. Its whole policy is a handful of constants at the top of `user/src/bin/init.rs`:

- **Crashes.** When a service ends, for whatever reason, the kernel sends `init` a message and `init` starts a fresh copy with the same capabilities.
- **Hangs.** Every {{supervisor.ping_every_ms}} ms `init` pings each server. A server that has not answered within {{supervisor.ping_timeout_ms}} ms is ended and replaced. In the worst case a hang is therefore noticed after {{supervisor.hang_worst_ms}} ms. A new copy gets {{supervisor.grace_ms}} ms before its first ping. The shell is not pinged.
- **Crash loops.** An end within {{supervisor.quick_end_s}} s of the start counts as a quick end. From quick end number {{supervisor.backoff_from}} in a row, each restart is delayed by {{supervisor.backoff_step_ms}} ms more than the one before, up to {{supervisor.backoff_cap_ms}} ms. A service is never given up on.
- **The supervisor itself.** If `init` ends, the kernel ends every task and starts `init` again from the boot archive, at most once every {{kernel.root_restart_gap_s}} s.

{{table.recovery}}

"Until the service answers again" is measured by the shell on the system's clock, from just before it injects the fault to the first answer from the new copy. "Host stopwatch" is the same interval timed by the test script from outside, including the serial line. "Supervisor restart" is the time `init` reports between noticing the end and having started the new copy.

A hang, as the serial port recorded it:

```log
{{log.hang_fs}}
```

A crash loop. The third and fourth quick ends in a row were delayed by {{backoff.third_ms}} ms and {{backoff.fourth_ms}} ms:

```log
{{log.backoff}}
```

The supervisor crashing. The kernel had user space running again {{init_crash.host_ms}} ms later on the host's stopwatch:

```log
{{log.crash_init}}
```

What the shell's `svc` command showed at the end of the test run:

```log
{{log.svc}}
```

## The invariant checker

`kernel/src/check.rs` is a second, independent account of everything the kernel owns. On request it walks every table and every page table and returns the number of the first rule that does not hold, or zero. It only reads kernel state and it never panics. The self-test and the fuzzer call it between their rounds, so a kernel bug that corrupts state is caught at once instead of crashing something later. The rules:

1. A frame that has an owner is a frame the allocator manages, and a task that has ended owns no memory.
2. No frame has two owners.
3. A frame that has an owner is not marked free.
4. The memory charged to a task equals what its page tables show.
5. The memory charged to a task is within its quota.
6. Owned frames and free frames add up to all frames.
7. The allocator's count of free frames matches its bitmap.
8. Every endpoint's reference count matches a recount of the capabilities and registrations that name it.
9. Every endpoint's count of receive rights matches a recount.
10. A task blocked in a receive is the registered waiter of its endpoint.
11. Nothing waits on an endpoint that nobody can receive from, and an ended task waits only on its own parent's endpoint.
12. A caller waits for a reply only from a task that can still give it.
13. No task outlives its parent.
14. A task that has ended holds no quota and no capabilities.
15. There is one root task, and quota is neither created nor lost.
16. An interrupt line is bound to a living task and an endpoint that can be received from.
17. The kernel has never met a state it considered impossible.
18. Outside the range for programs, a page directory holds only the shared kernel mapping, and device memory is never ordinary memory.

After the whole test run, with every injected fault and the fuzz run behind it, the checker still reported that all rules hold:

```log
{{log.uptime}}
```

## How it is tested

Everything runs in QEMU from one script, `tools/test.py`, which talks to the shell over the serial port and keeps the raw log (`evidence/test_serial.log`).

{{table.checks}}

- **`selftest`** is an ordinary program that checks {{selftest.checks}} promises of the kernel from user space: argument validation, capabilities, messages, memory, tasks and faults, what happens when each table is full, time and files. It runs after boot and again after all the injected faults. The complete list is its output, below.
- **`rogue`** is the hostile program described in [weaknesses and sources](weaknesses.md). Its {{rogue.all_total}} attempts are {{rogue.software}} through the system call interface, each tried with every one of its capability slots where a slot is needed, and {{rogue.by_cpu}} directly on the hardware, one fresh task per attempt.
- **`fuzz`** keeps {{fuzz.kids}} child tasks running, each making random and almost-right system calls for up to {{fuzz.kid_life_ms}} ms before it is replaced. Every {{fuzz.check_every_ms}} ms the fuzzer asks the kernel to check its rules, and at the end it checks that no task, endpoint or frame went missing. The count of calls is a lower bound: it leaves out the calls a child makes to set itself up and to report, and all calls of the children's own children.
- **Fault injection.** The shell can ask a server to crash or hang, and the supervisor to crash. The test script does each one {{test.fault_runs}} times, checks that the shell still answers and that a file can still be read, and records the times.
- **The browser.** `tools/web_check.py` boots the same disk image in {{web.emulator}} under {{web.browser}}, types {{web.commands}} commands ({{web.command_list:raw}}) and checks each result. All {{web.commands_ok}} passed; the prompt appeared after {{web.boot_seconds}} s.
- **The gates.** `python build.py gates` does all of the above from a clean tree and then runs `tools/check_claims.py` over these documents.

```log
{{log.selftest}}
```

What the kernel's basic operations cost in this emulator. These figures describe QEMU as much as Inti and are here for completeness, not as a claim about speed:

{{table.bench}}

## What has to be trusted

- **The kernel**: {{loc.kernel_total}} lines with {{count.unsafe_kernel}} `unsafe` blocks, each with a written justification that the gate script insists on. Across the whole repository there are {{count.unsafe_total}}.
- **The boot sector**, which runs before any protection exists.
- **`init`**, which starts with every capability and decides who gets what.
- **The console driver**, but only for what it carries: it sees every keystroke and all output. It cannot touch another task's memory.
- **The Rust compiler and its core library**, and the machine underneath, which here has always been an emulator.

Not trusted: the file server, the shell's child programs and anything else started with a narrowed set of capabilities.

## Limitations

Hardware and scope:

- 32-bit x86 only, one processor, at most {{limits.max_ram_mib}} MiB of memory, and only ever run in QEMU and v86. It has never run on a real machine.
- No network, no persistent file system, no user accounts, no graphical display, no applications beyond the test and demo programs.
- The clock needs the legacy 8254 timer. If the machine stalls for longer than one wrap of the counter, the clock falls behind. A sleeping task wakes up to one timer interrupt late.

Security:

- No formal verification. The kernel is tested, fuzzed and self-checking; that is evidence, not proof.
- No no-execute protection: code pages are read-only, but every readable page is executable.
- No IOMMU support: a task holding I/O ports for a device that can access memory directly could subvert the kernel. Only `init` holds the full port range.
- Capabilities are handed over only when a task is started. They cannot be sent in messages, and there is no way to revoke one short of ending its holder.
- `init` is trusted with everything. The shell holds the Info capability, the test capabilities it passes on, and a capability to ask `init` for a shutdown or an injected fault.
- The task table and the endpoint table are shared and not charged to quotas, so a hostile program can fill them until it is ended. Page tables are not freed when memory is unmapped, only when the task ends.

Robustness:

- A hang is detected only by the ping. The shell itself is not monitored for hangs.
- While `init` waits for a ping to be answered it does not handle other events, so a second failure in that moment waits for the ping to time out.
- A client whose request was in flight when the server died sends it again, so a console write can be duplicated. A half-typed line and anything typed ahead are lost when the console driver restarts.
- The kernel is not pre-emptible, and the scheduler has no processor quotas: a task cannot starve the kernel, but it can take its full share of the processor.
- The kernel log and the console driver share one serial port, so in the raw log a line from the kernel can land in the middle of a line from the shell.

Evidence:

- Every measurement was taken under emulation without hardware virtualisation. The timings say that recovery is fast relative to a reboot and nothing about real hardware.
- The fuzzer is random and not guided by coverage. Its child tasks hold no Log capability, so that call is fuzzed only up to its capability check.
- One hardware attempt of the hostile program, a forged return to ring 0, stops the v86 browser emulator with an internal error instead of raising the fault. Plain `rogue` and the browser demo skip that one attempt and say so; `rogue all` runs it under QEMU, where it is denied.
- The cited sources were located by web search during the build. [Weaknesses and sources](weaknesses.md) lists which of them a script could confirm on the page.
