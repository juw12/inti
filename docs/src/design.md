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
- Every frame has exactly one owner and every task has a memory quota. A child's quota is taken out of its parent's when it is started and returns when the child is gone, so a program cannot use more memory than it was given, however many children it starts. A page table that no longer maps anything is given back at once, so a mapping that fails or is undone costs nothing.
- **W^X: no page is both writable and executable.** The kernel uses PAE paging, whose page-table entries have a no-execute bit. A page of a program is either code, which is read-only and executable, or data that the processor refuses to run: the stack, the program's data, memory it maps later and its message buffers. The loader refuses an image with a segment that is both writable and executable, and an image with two segments on one page. No system call changes the protection of a page once it is mapped, and device memory is never executable. The kernel's own code is read-only and nothing else in its part of memory can be executed. All of this needs a processor with the no-execute bit; without it the kernel still boots and says in its boot log that W^X is not enforced (see [Limitations](#limitations)).
- The kernel has no heap. All of its state is one value made of fixed-size tables: {{limits.tasks}} tasks, {{limits.endpoints}} endpoints, {{limits.irq_lines}} interrupt lines. Nothing is allocated at run time, so nothing can leak; a full table is the error `E_NOSLOT`.

## Tasks

A task is an address space, one thread of execution, a table of capabilities, three quotas (memory, tasks and endpoints) and a share of the processor. Scheduling is round-robin among the ready tasks that are within their share, and a timer interrupt every {{timer.period_ms}} ms pre-empts a task that does not yield. Tasks form a tree: a task that ends takes its children with it, and its parent receives a message from the kernel that says how it ended (its exit code, or the processor exception and the address). An exception in ring 3 only ever ends the task that caused it.

Every quota is lent, never created. A parent gives a child part of its own memory, task and endpoint quota when it starts it, and gets all of it back when the child has ended and the parent has taken the notice. Starting a task costs one unit of task quota and creating an endpoint one unit of endpoint quota; a task that has none left is refused with `E_NOSLOT`. An endpoint is paid for by the task that created it until its last capability is gone, and by that task's parent if the creator ends first. The quotas of all living tasks add up to the sizes of the two kernel tables ({{limits.tasks}} tasks, {{limits.endpoints}} endpoints), so a program can use up its own share and nothing else. `init` lends the shell part of its quota and keeps the rest, which is what it restarts the servers with.

A share of the processor is a percentage from one to {{shares.full}}. A task gets its parent's share or a smaller one when it is started (word {{shares.spawn_word}} of the `spawn` request; zero means the parent's), and nothing raises it afterwards. At every switch the kernel reads the processor's cycle counter and charges the cycles to the task that ran and to every task above it in the tree, so a task's count covers everything it started: starting children, or starting them again and again, does not escape the count. Time is cut into windows of {{shares.window_ticks}} timer interrupts, one second. A task is over its share when it, or a task above it, has used its percentage of the previous window's cycles in the current window, and the scheduler picks such a task only when no ready task is within its share. A share is therefore a promise to everybody else and not a ceiling: it never leaves the processor idle, and a group that is over its share still runs when nobody else wants to. `init` gives the shell and everything started from it {{shares.shell}}%, so the servers and `init` itself come first once the shell's programs have had that much of a second.

In QEMU the self-test starts three tasks that never yield, once with a share of {{shares.boot.limit}}% and once with no share of their own, and counts its own work beside them. It kept {{shares.boot.kept_held}}% of its speed beside the held group and {{shares.boot.kept_free}}% beside the free one; the kernel charged the groups {{shares.boot.charged_held}}% and {{shares.boot.charged_free}}% of a second. The run after all the injected faults gave {{shares.faults.kept_held}}% and {{shares.faults.kept_free}}%, with {{shares.faults.charged_held}}% and {{shares.faults.charged_free}}% charged. The held group is charged more than its {{shares.boot.limit}}% by design, not by error: the self-test runs under the shell, and once the shell's tree has used its {{shares.shell}}% of a window every task in it is over a share, none is preferred, and the four tasks split what is left of the second evenly again.

```log
{{log.shares}}
```

What a share does not do: a server's work for a client is charged to the server, not to the client (no donation across a call); no share is enforced in the first second after boot, because there is no previous window to measure against; time the kernel spends in a system call is charged to the caller; a task can overrun by up to one timer period ({{timer.period_ms}} ms) per window before the kernel looks again; and the usage that `sysinfo` reports is that of the last full window, in thousandths, not of the current one.

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
- `cap_revoke` takes back every copy that was made of a capability, and every copy of those copies, in every task, while the holders keep running. What a holder had only through such a copy goes with it: a task waiting on the endpoint is woken with `E_NOCAP`, device memory is unmapped, an interrupt line is unbound. The capability that was revoked from stays. That is enough for a server to take back one client's access and leave the others': it derives one capability per client from its endpoint (`cap_derive`, each with its own badge), hands each client a copy of its own, and later revokes from the derived capability of the client it wants to cut off. The self-test does exactly this with two clients and then lets the first one in again. `cap_drop` is different: it gives up one capability and leaves its copies alone, linked one step further up, so they can still be revoked from there.
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

- **`console`** drives the screen, the keyboard and the serial port. It holds its request endpoint, the serial ports `0x3F8` to `0x3FF`, the keyboard data port `0x60`, the cursor ports `0x3D4` and `0x3D5`, two interrupt lines and one page of screen memory. It does not hold the keyboard controller's command port `0x64`, which can reset the machine; after start-up no task does, `init` included, and the self-test walks every task's capabilities to check it.
- **`fs`** serves the files of the boot archive. It holds its request endpoint and the archive as read-only memory. It keeps no state between requests, so a restart loses nothing.
- **`shell`** is the command line. A program it starts gets one capability, the right to send text to the console, and a small memory quota. The test programs (`selftest`, `fuzz`, `bench`) get a few more, listed at the top of their source files.

The serial line has one writer at a time. While a console driver is running and answering, `init` does not write its reports through the kernel log; it hands each line to the console as a request (`CON_REPORT`, which goes to the serial port only and is accepted only from the supervisor's badge), with a limit of {{console.report_ms}} ms and no retry. When there is no console, or it does not answer, `init` writes through the kernel log as before. The kernel itself writes to the serial port at boot, on a panic and when it starts user space again, and for a task that holds the log capability and asks (the self-test writes one line that way to test the call).

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
19. Authority only narrows: every capability is an original, or a copy of a capability that still exists and is at least as strong, and every chain of copies ends at an original.
20. Nothing outlives the capability it came through: no device page is mapped without its Memory capability, a task waits on an endpoint only while one of its capabilities lets it, a waiting sender may hand over a capability only while its own still carries the grant right, and an interrupt binding needs the capabilities it was made with.
21. The task and endpoint quotas balance: an ended task that waits to be noticed is charged to a living one, every endpoint in use is paid for by a living task, and slots in use plus unused quota equal the size of each table.
22. A mapped user page is user-accessible and has no unknown bit; it is never both writable and executable; device memory is never executable; and where the processor has the no-execute bit, every page that is not program code carries it.
23. The kernel's own map is what was built at boot, so kernel code is still read-only and nothing else in the kernel can be executed.
24. A task that waits for a message still owns the writable buffer it named, so the kernel can deliver into it later.
25. A page table maps at least one page: one that maps nothing has been given back.
26. A capability for memory, an interrupt line or I/O ports lies within what the kernel handed out at boot.
27. A task's share of the processor is between one percent and {{shares.full}}% and never above the share of the task that started it.

After the whole test run, with every injected fault and the fuzz run behind it, the checker still reported that all rules hold:

```log
{{log.uptime}}
```

## How it is tested

Everything runs in QEMU from one script, `tools/test.py`, which talks to the shell over the serial port and keeps the raw log (`evidence/test_serial.log`). QEMU is started with the processor model `{{test.cpu:raw}}`, because its default 32-bit model has no no-execute bit.

{{table.checks}}

- **`selftest`** is an ordinary program that checks {{selftest.checks}} promises of the kernel from user space: arguments, capabilities, messages, revocation, delegation, memory, tasks and faults, quotas, shares of the processor, who holds which hardware, time and files. It runs after boot and again after all the injected faults. The complete list is its output, below.
- **`rogue`** is the hostile program described in [weaknesses and sources](weaknesses.md). Its {{rogue.all_total}} attempts are {{rogue.software}} through the system call interface, each tried with every one of its capability slots where a slot is needed, and {{rogue.by_cpu}} directly on the hardware, one fresh task per attempt.
- **`fuzz`** keeps {{fuzz.kids}} child tasks running, each making random and almost-right system calls for up to {{fuzz.kid_life_ms}} ms before it is replaced. Every {{fuzz.check_every_ms}} ms the fuzzer asks the kernel to check its rules, and at the end it checks that no task, endpoint, frame or quota went missing. `cap_revoke` is among the random calls, some of the messages name a capability to hand over, and each child works within a quota of tasks and endpoints lent by the fuzzer, part of which it lends to children of its own. The count of calls is a lower bound: it leaves out the calls a child makes to set itself up and to report, and all calls of the children's own children.
- **Fault injection.** The shell can ask a server to crash or hang, and the supervisor to crash. The test script does each one {{test.fault_runs}} times, checks that the shell still answers and that a file can still be read, and records the times. `double`, the two failures at once shown above, is run {{double.runs}} times as well.
- **Without the no-execute bit.** The script boots the same image once more with the processor model `{{test.cpu_without_nx:raw}}`. The kernel then says in its boot log that W^X is not enforced. The self-test passes its other {{nonx.selftest_passed}} checks and reports the {{nonx.selftest_skipped}} execute checks as skipped, `rogue all` reports {{nonx.rogue_denied}} of {{nonx.rogue_total}} attempts denied and names the {{nonx.rogue_not_denied}} that were not, the kernel's rules still hold and nothing panics.
- **The guided tour.** `tour` runs {{tour.steps}} steps in one go: the hostile program, a driver crash, a server hang, the two failures at once and the kernel's counters. The script requires every step and the closing line. The tour took {{tour.seconds}} s in QEMU and {{web.tour_seconds}} s in the browser check, which fails if the tour takes a minute or longer.
- **The browser.** `tools/web_check.py` boots the same disk image in {{web.emulator}} under {{web.browser}}, types {{web.commands}} commands ({{web.command_list:raw}}) and checks each result. All {{web.commands_ok}} passed; the prompt appeared after {{web.boot_seconds}} s. The self-test passes there with nothing skipped, and the hostile program's attempts to run code from its stack, its data and memory it mapped are denied there too.
- **Two checks changed in this version.** The check "only `init` holds a capability for the reset port `0x64`" became "no task, `init` included, holds a capability for the reset port `0x64`", because `init` now drops it; the new check also requires that its walk finds at least six port capabilities, so it cannot pass by finding none. The share measurement used to compare against one half-second baseline. In one run the host stalled during that baseline, it came out about eight times too small, and the check failed with speeds several times higher than running alone while the kernel's own charge figures were normal. The self-test now takes three baselines (before, between and after the two measurements), uses the largest and prints all the raw counts. The thresholds are the same as before. A larger reference makes "kept enough speed beside the held group" harder to pass and "lost speed beside the free group" easier, and it adds about a second to each run.
- **The gates.** `python build.py gates` does all of the above from a clean tree and then runs `tools/check_claims.py` over these documents.
- **The model.** `python build.py model` runs the kernel's own source on the build machine against a simulated processor, with directed tests and a bounded exploration of its states, and `python build.py mutants` measures whether those checks notice a kernel that was broken on purpose. Both are described under [the model](#the-model). Neither is a gate.

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

## The model

QEMU runs the whole system, but only along the paths that the test programs take, and a failure in the middle of a system call is hard to arrange from user space. So the kernel is also run a second way. `model/` compiles {{model.kernel_files}} of the kernel's {{count.kernel_files}} source files, unchanged, as a program for the build machine: {{model.kernel_file_list:raw}}, which hold {{model.kernel_lines}} of the kernel's {{loc.kernel}} lines. The other {{model.left_out_files}}, {{model.left_out_list:raw}}, talk to the processor. The model replaces them with a simulated machine that keeps physical memory in an array, walks page tables by the rules of PAE paging and records what the kernel asks of the processor. A system call is a call of the kernel's own entry point with a trap frame. `python build.py model` builds and runs it and writes `evidence/model.json`; the last run, of {{model.date}}, took {{model.duration_s}} s.

This is a bounded check of small configurations and not a proof. It is not one of the gates either: the rubric and its gates were frozen before the model existed. Its numbers reach these documents the way all the others do, through `evidence/claims.json`. Every result of the model and of the mutation check below also records a fingerprint of the source files of the kernel, the interface crate and the model. `tools/check_claims.py`, which is one of the gates, refuses a result whose fingerprint is not that of the files as they are, so after a change to any of them the figures have to be measured again before a document may quote them.

**Directed tests.** {{model.tests}} tests put the kernel into situations that a program in QEMU cannot reach or cannot observe: the task table and the endpoint table full, memory running out part of the way through a system call, a capability revoked while it is in use, the root task dying while others wait. The "table full" branches are reached by forging a quota, because the rule that quotas balance keeps honest programs away from them. {{model.tests_passed}} of the {{model.tests}} pass. Among them, {{model.seeds}} corruptions are planted in the kernel's state one at a time and the checker has to report each one under the number of the rule that describes it, so that a corruption which some other rule happens to notice does not count; it reports {{model.seeds_found}}. {{model.blind}} more are forged capabilities that the checker has no means to tell from honest ones: a capability for every I/O port, one for the kernel log and a handle on a task that is not a child. The test requires that they go unnoticed, and {{model.blind_unseen}} of {{model.blind}} do, so that the checker's blind spots are written down and cannot grow without a test failing.

**What the first run found.** The first run failed: {{model.before.tests_passed}} of {{model.before.tests}} tests passed, and the checker reported {{model.before.seeds_found}} of {{model.before.seeds}} planted corruptions. Its raw log is kept as `evidence/model-before-fixes.log`. These are its failures:

```modellog
{{modellog.before}}
```

One was a defect in the kernel and four were gaps in its checker:

- A `MAP` of two pages that ran out of memory at the second page, in a part of the address space that needed a new page table, gave back the first page but kept the empty page table, charged to the task. The call had failed and still cost its caller a page until the task ended. `MAP` now undoes everything when it fails, and a page table is given back as soon as it maps nothing, also after `UNMAP` and after a device mapping is revoked.
- The checker had no rule against an empty page table, and none that tied a capability for memory or for an interrupt line to what the kernel handed out at boot. These are the two newest rules in the list above. Its rule that authority only narrows looked one step back and so missed two copies that named each other as their source; it now follows every chain to an original.

One suspicion did not hold up: that device memory could be revoked from under the message buffer of a waiting task. The kernel refuses a message buffer in device memory when the wait begins, so that state cannot arise, and nothing was changed for it.

The same tests today:

```modellog
{{modellog.result}}
```

**Bounded exploration.** `model explore` starts from a freshly booted machine and tries every step of a fixed menu in every state it reaches, breadth first, down to a fixed depth. A step is a system call by one of the tasks, an interrupt, a timeout that expires or a fault. States are compared by a summary of the kernel's tables, so that two orders of steps with the same outcome are explored once. After every step the explorer requires that

- the kernel's own checker finds every rule intact,
- the kernel's idea of the machine and the simulated machine say the same,
- no wake-up is lost: no task waits to receive on an endpoint that has something for it,
- a message that arrives is the message that was sent, and a reply is the reply that was given,
- the step did what it promises: a derived capability is no wider than its source, a revocation empties exactly the copies, memory that is handed out is zero and a killed task is gone,
- a system call that returned an error changed nothing, and
- after the root task has died, the kernel's tables are those of a fresh boot.

There is one exemption from "an error changes nothing", because the kernel is meant to behave so: a receive that only polls and returns `E_TIMEOUT` may on its way have failed a queued caller whose capability could not be handed over.

{{table.model}}

Together that is {{model.states}} states and {{model.transitions}} steps with {{model.violations}} violations. {{model.deliveries}} deliveries and {{model.replies}} replies were compared with what had been sent, and {{model.restarts}} restarts with a fresh boot. {{model.refused}} of the steps were system calls that returned an error, {{model.no_memory}} of them because memory had run out and {{model.no_slot}} because a table or a quota was full. The exploration found no defect in the kernel: everything this round found came from the directed tests.

**Failures in pairs.** The explorer notes which kinds of failure it has met on the way to each state: a task that exits, faults or is killed, a capability dropped or revoked under a task that was using it, a timeout, and the death of the root task. Of the {{model.pairs_possible}} ordered pairs of these kinds, {{model.pairs}} occur on the paths it took. The other {{model.pairs_not_met}} do not: {{model.pairs_not_met_list}}. Part of the reason is the search itself, which is breadth first, keeps only the first path to each state and starts its record again after the root task dies. Those pairs are not shown to be handled. They are not shown at all.

**The mutation check.** A check that passes says little unless it would have failed on a broken kernel. `python build.py mutants` finds {{mutants.sites}} places in the kernel files that the model compiles where a small change is possible: a comparison turned into its opposite or moved by one, `&&` and `||` exchanged, `+` and `-` exchanged, a call or an assignment removed, a condition inverted. It puts them in a fixed order, cuts the list into {{mutants.sample}} equal stretches and takes the middle place of each, so that nobody chooses. It makes each change alone and runs the directed tests and then the exploration against the changed kernel, which counts as caught when a check fails, when the program panics or when a check runs past its time limit.

The places are positions in that list, so both samples are drawn anew whenever the list changes. A round ago the list had {{mutants.previous.sites}} places. Code added since has made it {{mutants.sites}}, and every stretch moved. Of the {{mutants.sample}} places in today's first sample, {{mutants.from_previous_first}} were in the first sample of the round before and {{mutants.from_previous_held_out}} in its held-out sample; the other {{mutants.new_places}} are new. A place is matched here by file, kind of change and the text of the line, so two identical lines can match each other and the counts of places met before are upper bounds. The scores of the round before, {{mutants.previous.first.caught}} of {{mutants.previous.first.compiled}} in the first sample and {{mutants.previous.held_out.caught}} of {{mutants.previous.held_out.compiled}} in the held-out one, therefore do not carry over, and they are not comparable with the figures below place for place. The earlier places are kept in `evidence/mutants-previous.json` so that the overlap can be counted again.

The first run of this round caught {{mutants.before.caught}} of {{mutants.before.compiled}} changed kernels and missed {{mutants.before.not_caught}}; its log is kept as `evidence/mutants-before-tests.log`. Since most of its places were new, that run measured tests that had for the most part not been aimed at them. Every mutant it missed was read. A round ago the misses had pointed at questions no test had asked: what happens at the first and at the last value of a range, in what order queued messages arrive and whether a buffer that lies across two pages is copied whole, whether a program of more than one page is loaded whole, what the kernel reports about a task and about its end, and whose turn it is. Five groups of directed tests were written for those. This round's misses pointed at more: which badge a derived endpoint capability carries, which interrupt lines a derived capability covers, whether a program is refused for a single wrong field in its header, what happens at the edges of user space, what the kernel counts of system calls and of faults, and what a task is charged when a window of processor time ends. Three more groups now ask these. They were added; no directed test that existed was changed or removed, and the kernel was not changed for them: what this run found were holes in the tests.

The same {{mutants.sample}} places today: {{mutants.not_compiled}} of the changed kernels did not compile. Of the {{mutants.compiled}} that did, {{mutants.caught}} were caught ({{mutants.caught_percent}} %), {{mutants.caught_by_tables}} by a directed test and {{mutants.caught_by_exploration}} by the exploration: {{mutants.caught_by_failed_check}} through a check that failed, {{mutants.caught_by_panic}} through a panic and {{mutants.caught_by_time_limit}} by running past the time limit. {{mutants.not_caught}} were not caught:

{{table.mutants_not_caught}}

That figure is no estimate for the rest of the kernel. The tests were written with these very mutants in view, so it measures how well they were aimed. For how far they reach there is a second sample, `python build.py mutants --held-out`, which takes the first place of each stretch instead of the middle one. It shares no place with the first sample, and no test was written against it this round. That does not make it untouched. Because the samples are drawn anew when the list changes, {{mutants.held_out.from_previous_first}} of its places were in the first sample of the round before, where tests were aimed at them, and {{mutants.held_out.from_previous_held_out}} were in the held-out sample then. It is reported as it came out: {{mutants.held_out.not_compiled}} of its {{mutants.held_out.sample}} changed kernels did not compile, and of the {{mutants.held_out.compiled}} that did, {{mutants.held_out.caught}} were caught ({{mutants.held_out.caught_percent}} %), {{mutants.held_out.caught_by_tables}} by a directed test and {{mutants.held_out.caught_by_exploration}} by the exploration. {{mutants.held_out.not_caught}} were not caught. No test has been written for them, so that the sample stays held out:

{{table.mutants_held_out_not_caught}}

A mutant that survives is a change that makes no difference, or one that the model cannot see, or a hole in the checks. The last column of both tables says which, as judged by reading the code; the tool cannot tell them apart, and a reading can be wrong. Of the {{mutants.held_out.not_caught}} in the held-out sample, {{mutants.held_out.judged_no_difference}} were judged to make no difference and {{mutants.held_out.judged_outside_model}} to lie outside what the model can see, because the simulated serial port counts the bytes written to it and keeps none of them. The other {{mutants.held_out.judged_hole}} are holes: the changed kernel behaves differently and no check on the build machine asks. Some of them are small: a counter, a reported figure, a wait that ends one tick late when its deadline falls exactly on a tick. Some are not: a task that waits to send and has lost the grant right still hands its capability over, a program segment that begins on an odd-numbered page gets a page too many, and a capability for device memory covers one frame more than its range. The last of these got away a round ago as well. They are open work.

The holes listed a round ago were a different list, and most of the places they were found at are in neither sample now, because the samples moved. Those holes were not closed on purpose, and whether the tests added this round happen to catch them was not measured. A sample that moves cannot show that a hole named earlier is closed; only a run of that very mutant can. Closing them takes new tests and then a new sample, because a test written against a mutant ends its being held out. Neither sample was run in QEMU, so a mutant listed here may well be caught by the tests there; that was not tried.

The held-out figure has a weakness of its own: its places lie a few places before those of the first sample in the same order, often in the same function, so a test written for one may catch the other.

The tool had a defect of its own. It works in a copy of the sources, and the copy kept the file times of the originals, so the build could mistake the program left from the last mutant of an earlier run for the unchanged kernel. The tool's first step, in which the unchanged kernel must compile and pass every check, failed and stopped the run, which is how the defect was found. The copies now get the time of the copying, and the tool stops if the program it is about to judge is not newer than the file it changed. Both samples reported here were run with the corrected tool. A changed kernel was compiled anew in any case, because the tool writes the changed file just before it builds.

**What the model does not show.**

- It is bounded. Each scenario has a few tasks and endpoints, a small number of capabilities and a fixed depth, and "explored to the end" means only that no step of the menu led to a new state within those bounds. Only {{model.complete_list:raw}} got that far.
- The machine is simulated. {{model.left_out_list:raw}}, {{model.left_out_lines}} lines with the code that talks to the processor and the devices and the kernel's start-up code, are not part of the model, and neither is the boot sector. They are tested only in the emulators.
- The model is a 64-bit program and the kernel a 32-bit one. Code that depends on the width of `usize` can behave differently, and the memory routines that the interface crate provides for the 32-bit build are the build machine's own in the model.
- Time is abstract. A timeout expires because the explorer says so and not because a clock ran, and any task that is ready may run next, which is more than the real scheduler allows. The order of scheduling and the clock are outside the exploration, and the delay before a root task that died soon after its start is started again is covered by a directed test only.
- States are merged by a hashed summary of tasks, capabilities, endpoints, waits, mappings, quotas and share limits. What a task has used of its share and the order among ready tasks are not part of the summary, and in the exploration every share is {{shares.full}}%, so the exploration says nothing about scheduling under a share; the directed tests of the model do that. The contents of user pages, message tokens and counters are left out, so two states that differ only there count as one, and a hash collision would hide a state.
- The root task never drops the capabilities it was booted with, and in three of the scenarios its steps are restricted to keep the number of states small.
- The oracles lean on the kernel's own checker, which is itself under test. The planted corruptions and the mutants that fall in the checker's file are the evidence that it notices things, and its known blind spots are named above.
- The mutation check is two samples of {{mutants.sample}} out of {{mutants.sites}} places and uses simple changes, one at a time. The share caught in the first sample was raised by tests aimed at it. The held-out share is the one to read, with the caution that some of its places were aimed at a round earlier, and no share would make the kernel correct. Both samples move whenever the list of places changes, so a share from one round cannot be set beside a share from another.
- Not done: a proof, symbolic execution, a run of the `unsafe` code under Miri, and an exploration of the real 32-bit build.

## What has to be trusted

- **The kernel**: {{loc.kernel_total}} lines with {{count.unsafe_kernel}} `unsafe` blocks, each with a written justification that the gate script insists on. Across the whole repository there are {{count.unsafe_total}}.
- **The boot sector**, which runs before any protection exists.
- **`init`**, which starts with every capability and decides who gets what. After start-up it keeps what it hands to the servers when it restarts them and two port ranges that power the machine off (`0x604` to `0x605` and `0xB004` to `0xB005`); it drops its capabilities for all ports, all interrupt lines and the screen memory.
- **The console driver**, but only for what it carries: it sees every keystroke and all output. It cannot touch another task's memory.
- **The Rust compiler and its core library**, and the machine underneath, which here has always been an emulator.
- **The model**, for everything it shows: its simulated machine and its oracles are {{loc.model}} lines by the same author as the kernel, and a mistake there can hide a mistake in the kernel. The mutation check measures how much they notice, not whether they are right.

Not trusted: the file server, the shell's child programs and anything else started with a narrowed set of capabilities.

## Limitations

Hardware and scope:

- 32-bit x86 only, one processor, at most {{limits.max_ram_mib}} MiB of memory, and only ever run in QEMU and v86. It has never run on a real machine.
- No network, no persistent file system, no user accounts, no graphical display, no applications beyond the test and demo programs.
- The clock needs the legacy 8254 timer. If the machine stalls for longer than one wrap of the counter, the clock falls behind. A sleeping task wakes up to one timer interrupt late.

Security:

- No formal verification. The kernel is tested, fuzzed, self-checking and explored on a host to a small depth; that is evidence, not proof. [The model](#the-model) lists what the exploration leaves out.
- W^X depends on the processor's no-execute bit and has only been shown in two emulators: QEMU, which must be started with `{{test.cpu:raw}}` because its default 32-bit model lacks the bit, and v86. It has never been tried on a real processor. Without the bit the kernel still runs and prints that W^X is not enforced, and a program can then run code from its stack, its data and memory it mapped; the test run boots that way once, and the hostile program's {{nonx.rogue_not_denied}} execute attempts are reported as not denied. W^X stops injected code, not the reuse of code that is already in a program, and there is no address-space randomisation: programs are loaded at fixed addresses.
- No IOMMU support: a task holding I/O ports for a device that can access memory directly could subvert the kernel. After start-up no task holds the full port range, `init` included, but nothing but `init`'s own code makes it give that range up.
- Revocation follows the copies, not the holders. `cap_revoke` takes back everything that was copied from one capability, so a server that wants to cut off one client must have derived a capability for that client beforehand; clients that were handed copies of the same capability go together. Dropping a capability does not revoke its copies. (Earlier versions of this page said a chosen copy could not be taken back. That was wrong: the kernel has not changed, the per-client pattern above always worked, and there is now a self-test for it.)
- Revocation does not reach back in time. A call that the server has already taken is carried through after the caller's capability is revoked, and its reply can still carry a capability. A task that waits through a revoked capability keeps waiting if another of its capabilities still reaches the same endpoint.
- A server that holds the grant right has to drop the capabilities it does not want, or a client with the grant right could fill its table. In the shipped system no client of the servers holds the grant right.
- The kernel writes a message into the buffer of a blocked task long after it checked that buffer. That late write is safe only if the buffer is still the task's own writable memory. The checker has a rule for exactly this, and the argument behind it has four parts: a blocked task is not running, so it cannot unmap its own pages; an ended task gets no delivery; no system call changes a page's protection; and device memory, the only memory a revocation can take from a waiting task, is refused as a message buffer, also in part. The self-test checks the last part directly, with a device page taken from a task while it waits, and checks that a reply still reaches a caller whose capability was revoked in between. This is a tested argument, not a proof.
- `init` is trusted with everything. The shell holds the Info capability, the test capabilities it passes on, and a capability to ask `init` for a shutdown or an injected fault.
- A parent cannot change a child's quotas or its share of the processor after starting it.
- The kernel's own "table full" paths for tasks and endpoints are executed only on the host model, where a test forges a quota to reach them, and never in QEMU. User space cannot reach them while the rule that quotas balance holds, and that rule is checked throughout every fuzz and self-test run. If one were ever reached, the kernel would count it as an anomaly, which breaks the rule that the kernel has never met a state it considered impossible. A full capability table, by contrast, is reached and tested.

Robustness:

- A hang is detected only by the ping. The shell itself is not monitored for hangs.
- While `init` waits for a ping to be answered it handles other failures only between slices of {{supervisor.ping_slice_ms}} ms. Servers are pinged one after the other, so two that hang at the same moment are noticed one after the other. Only one combination of two failures is tested: the console driver hanging while the file server crashes.
- A client whose request was in flight when the server died sends it again, so a console write can be duplicated. A half-typed line and anything typed ahead are lost when the console driver restarts.
- The kernel is not pre-emptible. Shares of the processor are coarse: one-second windows, no enforcement in the first second, an overrun of up to one timer period per window, no donation of a client's share to the server that works for it, and no ceiling, so a program held to a share still takes every cycle that nobody else asks for. They have been measured only in QEMU, under load from the host, on one processor. Revoking a capability and checking the invariants are scans of fixed-size tables that run to their end inside the kernel.
- The kernel log and the console driver share one serial port. `init` now reports through the console while it answers, which removed the cut lines seen in earlier logs, but two cases remain: if a report takes longer than {{console.report_ms}} ms because the console is slow and not dead, `init` falls back to the kernel log and the line can appear twice or land inside another; and a kernel panic or a restart of user space writes at once, wherever the console's line happens to be.

Evidence:

- Every measurement was taken under emulation without hardware virtualisation. The timings say that recovery is fast relative to a reboot and nothing about real hardware.
- The model is a second program built from the kernel's source, not the kernel that boots, and its exploration is bounded. [The model](#the-model) says what that leaves out.
- The fuzzer is random and not guided by coverage. Its child tasks hold no Log capability, so that call is fuzzed only up to its capability check.
- One hardware attempt of the hostile program, a forged return to ring 0, stops the v86 browser emulator with an internal error instead of raising the fault. Plain `rogue` and the browser demo skip that one attempt and say so; `rogue all` runs it under QEMU, where it is denied.
- The cited sources were located by web search during the build. [Weaknesses and sources](weaknesses.md) lists which of them a script could confirm on the page.
