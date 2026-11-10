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
- **W^X: no page is both writable and executable.** The kernel uses PAE paging, whose page-table entries have a no-execute bit. A page of a program is either code, which is read-only and executable, or data that the processor refuses to run: the stack, the program's data, memory it maps later and its message buffers. The loader refuses an image with a segment that is both writable and executable, and an image with two segments on one page. No system call changes the protection of a page once it is mapped, and device memory is never executable. The kernel's own code is read-only and nothing else in its part of memory can be executed. All of this needs a processor with the no-execute bit; without it the kernel still boots and says in its boot log that W^X is not enforced (see [Limitations](#limitations)).
- The kernel has no heap. All of its state is one value made of fixed-size tables: {{limits.tasks}} tasks, {{limits.endpoints}} endpoints, {{limits.irq_lines}} interrupt lines. Nothing is allocated at run time, so nothing can leak; a full table is the error `E_NOSLOT`.

## Tasks

A task is an address space, one thread of execution, a table of capabilities and three quotas: memory, tasks and endpoints. Scheduling is round-robin, and a timer interrupt every {{timer.period_ms}} ms pre-empts a task that does not yield. Tasks form a tree: a task that ends takes its children with it, and its parent receives a message from the kernel that says how it ended (its exit code, or the processor exception and the address). An exception in ring 3 only ever ends the task that caused it.

Every quota is lent, never created. A parent gives a child part of its own memory, task and endpoint quota when it starts it, and gets all of it back when the child has ended and the parent has taken the notice. Starting a task costs one unit of task quota and creating an endpoint one unit of endpoint quota; a task that has none left is refused with `E_NOSLOT`. An endpoint is paid for by the task that created it until its last capability is gone, and by that task's parent if the creator ends first. The quotas of all living tasks add up to the sizes of the two kernel tables ({{limits.tasks}} tasks, {{limits.endpoints}} endpoints), so a program can use up its own share and nothing else. `init` lends the shell part of its quota and keeps the rest, which is what it restarts the servers with.

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
- `cap_derive` copies a capability with the same rights and range, or less: fewer rights, a smaller range of ports, fewer interrupt lines, fewer pages, read-only instead of writable. Nothing widens a capability. Every copy remembers which capability it was made from.
- A holder of the grant right can stamp a badge on a copy, once. The kernel puts the badge of the capability that was used into every message, so a server knows which client is calling without trusting what the client says about itself.
- `cap_revoke` takes back every copy that was made of a capability, and every copy of those copies, in every task, while the holders keep running. What a holder had only through such a copy goes with it: a task waiting on the endpoint is woken with `E_NOCAP`, device memory is unmapped, an interrupt line is unbound. The capability that was revoked from stays. `cap_drop` is different: it gives up one capability and leaves its copies alone, linked one step further up, so they can still be revoked from there.
- A capability can travel in a message, under the grant rule described under Messages. The copy that arrives is linked to the sender's, so delegation never puts a capability out of reach of revocation.
- No system call takes a global name. There are no process ids, no paths and no physical addresses in the interface: what a task was not given, it cannot even mention.
- The ports of the interrupt controllers, the timer and the speaker gate stay with the kernel. It refuses them even to a task whose capability covers them.

## Messages

One mechanism carries everything. A client calls an endpoint and blocks until the server replies; both sides can set a time limit. A message has a label, a few machine words and up to {{limits.msg_data}} bytes of data.

- A message is copied through a kernel buffer. Tasks never share memory through a message, so a sender cannot change it after the kernel has checked it.
- The server answers through a reply token that works once. A task cannot answer a call it did not receive.
- Interrupts, and the ends of supervised tasks, arrive on endpoints as messages from the kernel. Their labels are reserved: a program that tries to send one is refused.
- A caller whose server dies while handling its request is told so (`E_DEAD`), and the client library sends the request again to the new server. A request that was still waiting in the queue just waits for the new server.
- A call or a reply can carry one capability. The kernel copies it into a free slot of the receiver only if the capability used for sending and the capability used for receiving both carry the grant right; otherwise the sender gets `E_PERM` and nothing is delivered. The copy has the same rights and is linked to the sender's capability, so the sender can take it back with `cap_revoke`. If the receiver's table is full, the sender gets `E_NOSLOT`. In the shipped system `init` gives clients send-only capabilities and servers receive-only ones, so nobody can push a capability into the console driver, the file server or `init`.

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
- **Hangs.** Every {{supervisor.ping_every_ms}} ms `init` pings each server. A server that has not answered within {{supervisor.ping_timeout_ms}} ms is ended and replaced. `init` waits for the answer in slices of {{supervisor.ping_slice_ms}} ms and deals with whatever else happened between two slices, so another service that fails during the wait is restarted then and not after the timeout. In the worst case a single hang is noticed after {{supervisor.hang_worst_ms}} ms. Servers are pinged one after the other, so two that hang at the same moment are found one after the other. A new copy gets {{supervisor.grace_ms}} ms before its first ping. The shell is not pinged.
- **Crash loops.** An end within {{supervisor.quick_end_s}} s of the start counts as a quick end. From quick end number {{supervisor.backoff_from}} in a row, each restart is delayed by {{supervisor.backoff_step_ms}} ms more than the one before, up to {{supervisor.backoff_cap_ms}} ms. A service is never given up on.
- **The supervisor itself.** If `init` ends, the kernel ends every task and starts `init` again from the boot archive, at most once every {{kernel.root_restart_gap_s}} s.

{{table.recovery}}

"Until the service answers again" is measured by the shell on the system's clock, from just before it injects the fault to the first answer from the new copy. "Host stopwatch" is the same interval timed by the test script from outside, including the serial line. "Supervisor restart" is the time `init` reports between noticing the end and having started the new copy.

A hang, as the serial port recorded it:

```log
{{log.hang_fs}}
```

Two failures at once. `double` makes the console driver hang and then, while `init` is waiting for that driver to answer a ping, makes the file server crash. `init` gets the notice of the crash between two slices of its wait, says so, restarts the file server and goes on waiting for the console driver until its time is up. The test script requires these lines in this order in each of {{double.runs}} runs, and that the shell and a file listing answer afterwards. The file server answered again after {{double.fs_ms}} ms and the console driver after {{double.console_ms}} ms (medians):

```log
{{log.double}}
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
14. A task that has ended holds no quota of any kind and no capabilities.
15. There is one root task, and quota is neither created nor lost.
16. An interrupt line is bound to a living task and an endpoint that can be received from.
17. The kernel has never met a state it considered impossible.
18. Outside the range for programs, a page directory holds only the shared kernel mapping, and device memory is never ordinary memory.
19. Authority only narrows: every capability is an original, or a copy of a capability that still exists and is at least as strong.
20. Nothing outlives the capability it came through: no device page is mapped without its Memory capability, a task waits on an endpoint only while one of its capabilities lets it, a waiting sender may hand over a capability only while its own still carries the grant right, and an interrupt binding needs the capabilities it was made with.
21. The task and endpoint quotas balance: an ended task that waits to be noticed is charged to a living one, every endpoint in use is paid for by a living task, and slots in use plus unused quota equal the size of each table.
22. A mapped user page is user-accessible and has no unknown bit; it is never both writable and executable; device memory is never executable; and where the processor has the no-execute bit, every page that is not program code carries it.
23. The kernel's own map is what was built at boot, so kernel code is still read-only and nothing else in the kernel can be executed.
24. A task that waits for a message still owns the writable buffer it named, so the kernel can deliver into it later.

After the whole test run, with every injected fault and the fuzz run behind it, the checker still reported that all rules hold:

```log
{{log.uptime}}
```

## How it is tested

Everything runs in QEMU from one script, `tools/test.py`, which talks to the shell over the serial port and keeps the raw log (`evidence/test_serial.log`). QEMU is started with the processor model `{{test.cpu:raw}}`, because its default 32-bit model has no no-execute bit.

{{table.checks}}

- **`selftest`** is an ordinary program that checks {{selftest.checks}} promises of the kernel from user space: arguments, capabilities, messages, revocation, delegation, memory, tasks and faults, quotas, time and files. It runs after boot and again after all the injected faults. The complete list is its output, below.
- **`rogue`** is the hostile program described in [weaknesses and sources](weaknesses.md). Its {{rogue.all_total}} attempts are {{rogue.software}} through the system call interface, each tried with every one of its capability slots where a slot is needed, and {{rogue.by_cpu}} directly on the hardware, one fresh task per attempt.
- **`fuzz`** keeps {{fuzz.kids}} child tasks running, each making random and almost-right system calls for up to {{fuzz.kid_life_ms}} ms before it is replaced. Every {{fuzz.check_every_ms}} ms the fuzzer asks the kernel to check its rules, and at the end it checks that no task, endpoint, frame or quota went missing. `cap_revoke` is among the random calls, some of the messages name a capability to hand over, and each child works within a quota of tasks and endpoints lent by the fuzzer, part of which it lends to children of its own. The count of calls is a lower bound: it leaves out the calls a child makes to set itself up and to report, and all calls of the children's own children.
- **Fault injection.** The shell can ask a server to crash or hang, and the supervisor to crash. The test script does each one {{test.fault_runs}} times, checks that the shell still answers and that a file can still be read, and records the times. `double`, the two failures at once shown above, is run {{double.runs}} times as well.
- **Without the no-execute bit.** The script boots the same image once more with the processor model `{{test.cpu_without_nx:raw}}`. The kernel then says in its boot log that W^X is not enforced. The self-test passes its other {{nonx.selftest_passed}} checks and reports the {{nonx.selftest_skipped}} execute checks as skipped, `rogue all` reports {{nonx.rogue_denied}} of {{nonx.rogue_total}} attempts denied and names the {{nonx.rogue_not_denied}} that were not, the kernel's rules still hold and nothing panics.
- **The guided tour.** `tour` runs {{tour.steps}} steps in one go: the hostile program, a driver crash, a server hang, the two failures at once and the kernel's counters. The script requires every step and the closing line. The tour took {{tour.seconds}} s in QEMU and {{web.tour_seconds}} s in the browser check, which fails if the tour takes a minute or longer.
- **The browser.** `tools/web_check.py` boots the same disk image in {{web.emulator}} under {{web.browser}}, types {{web.commands}} commands ({{web.command_list:raw}}) and checks each result. All {{web.commands_ok}} passed; the prompt appeared after {{web.boot_seconds}} s. The self-test passes there with nothing skipped, and the hostile program's attempts to run code from its stack, its data and memory it mapped are denied there too.
- **The gates.** `python build.py gates` does all of the above from a clean tree and then runs `tools/check_claims.py` over these documents.

```log
{{log.selftest}}
```

What the browser run shows about W^X: the kernel's boot line, then the execute checks of the self-test and the execute attempts of the hostile program:

```weblog
{{weblog.wx}}
```

The boot without the no-execute bit, shortened to the telling lines:

```nonxlog
{{nonxlog.summary}}
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
- W^X depends on the processor's no-execute bit and has only been shown in two emulators: QEMU, which must be started with `{{test.cpu:raw}}` because its default 32-bit model lacks the bit, and v86. It has never been tried on a real processor. Without the bit the kernel still runs and prints that W^X is not enforced, and a program can then run code from its stack, its data and memory it mapped; the test run boots that way once, and the hostile program's {{nonx.rogue_not_denied}} execute attempts are reported as not denied. W^X stops injected code, not the reuse of code that is already in a program, and there is no address-space randomisation: programs are loaded at fixed addresses.
- No IOMMU support: a task holding I/O ports for a device that can access memory directly could subvert the kernel. Only `init` holds the full port range.
- Revocation is coarse. `cap_revoke` takes back all copies of a capability at once, not a chosen one, and dropping a capability does not revoke its copies.
- Revocation does not reach back in time. A call that the server has already taken is carried through after the caller's capability is revoked, and its reply can still carry a capability. A task that waits through a revoked capability keeps waiting if another of its capabilities still reaches the same endpoint.
- A server that holds the grant right has to drop the capabilities it does not want, or a client with the grant right could fill its table. In the shipped system no client of the servers holds the grant right.
- The kernel writes a message into the buffer of a blocked task long after it checked that buffer. That late write is safe only if the buffer is still the task's own writable memory. The checker has a rule for exactly this, and the argument behind it has four parts: a blocked task is not running, so it cannot unmap its own pages; an ended task gets no delivery; no system call changes a page's protection; and device memory, the only memory a revocation can take from a waiting task, is refused as a message buffer, also in part. The self-test checks the last part directly, with a device page taken from a task while it waits, and checks that a reply still reaches a caller whose capability was revoked in between. This is a tested argument, not a proof.
- `init` is trusted with everything. The shell holds the Info capability, the test capabilities it passes on, and a capability to ask `init` for a shutdown or an injected fault.
- Page tables are not freed when memory is unmapped, only when the task ends. A parent cannot change a child's quotas after starting it, and processor time is not charged to anyone.
- The kernel's own "table full" paths for tasks and endpoints are never executed by a test. User space cannot reach them while the rule that quotas balance holds, and that rule is checked throughout every fuzz and self-test run. If one were ever reached, the kernel would count it as an anomaly, which breaks the rule that the kernel has never met a state it considered impossible. A full capability table, by contrast, is reached and tested.

Robustness:

- A hang is detected only by the ping. The shell itself is not monitored for hangs.
- While `init` waits for a ping to be answered it handles other failures only between slices of {{supervisor.ping_slice_ms}} ms. Servers are pinged one after the other, so two that hang at the same moment are noticed one after the other. Only one combination of two failures is tested: the console driver hanging while the file server crashes.
- A client whose request was in flight when the server died sends it again, so a console write can be duplicated. A half-typed line and anything typed ahead are lost when the console driver restarts.
- The kernel is not pre-emptible, and the scheduler has no processor quotas: a task cannot starve the kernel, but it can take its full share of the processor. Revoking a capability and checking the invariants are scans of fixed-size tables that run to their end inside the kernel.
- The kernel log and the console driver share one serial port, so in the raw log a line from the kernel can land in the middle of a line from the shell.

Evidence:

- Every measurement was taken under emulation without hardware virtualisation. The timings say that recovery is fast relative to a reboot and nothing about real hardware.
- The fuzzer is random and not guided by coverage. Its child tasks hold no Log capability, so that call is fuzzed only up to its capability check.
- One hardware attempt of the hostile program, a forged return to ring 0, stops the v86 browser emulator with an internal error instead of raising the fault. Plain `rogue` and the browser demo skip that one attempt and say so; `rogue all` runs it under QEMU, where it is denied.
- The cited sources were located by web search during the build. [Weaknesses and sources](weaknesses.md) lists which of them a script could confirm on the page.
