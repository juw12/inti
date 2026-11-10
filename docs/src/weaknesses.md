# The weaknesses Inti targets, with sources

Inti does not try to beat mainstream operating systems at what they are good at. It picks three weaknesses that their own makers and the research literature document, and shows a design in which each one is absent. For each weakness this page gives the evidence, what Inti does, what was measured, and what Inti does **not** solve.

The measurements are from the gate run of {{gates.date}} in {{test.emulator}}. `python build.py gates` repeats them. They are emulator figures.

## Drivers that can take the whole system down

**The evidence.**

- Device drivers caused 85% of recently reported Windows XP failures ([Swift, Bershad and Levy, "Improving the Reliability of Commodity Operating Systems", SOSP 2003](https://dl.acm.org/doi/10.1145/945445.945466)).
- In Linux, drivers had error rates three to seven times higher than the rest of the kernel ([Chou and others, "An Empirical Study of Operating Systems Errors", SOSP 2001](https://dl.acm.org/doi/10.1145/502034.502042)). Ten years later drivers still contained the most faults, although their fault rate had fallen below that of other directories such as the architecture and file-system code ([Palix and others, "Faults in Linux: Ten Years Later", ASPLOS 2011](https://dl.acm.org/doi/10.1145/1961296.1950401)).
- Microsoft's documentation says of Windows that if a kernel-mode driver crashes, the entire operating system crashes ([Microsoft Learn, "User mode and kernel mode"](https://learn.microsoft.com/en-us/windows-hardware/drivers/gettingstarted/user-mode-and-kernel-mode)).
- On 19 July 2024 a faulty content update for CrowdStrike's kernel driver crashed Windows machines around the world. Microsoft estimated 8.5 million affected devices, less than one percent of all Windows machines ([Microsoft, 20 July 2024](https://blogs.microsoft.com/blog/2024/07/20/helping-our-customers-through-the-crowdstrike-outage/)). CrowdStrike's analysis: a new template type defined 21 input fields, the driver's code supplied only 20 values, and reading the 21st was an out-of-bounds memory read that crashed the system ([root cause analysis, PDF](https://www.crowdstrike.com/wp-content/uploads/2024/08/Channel-File-291-Incident-Root-Cause-Analysis-08.06.2024.pdf)).

**What Inti does.** The console driver (screen, keyboard and serial port) and the file server are ordinary programs in ring 3. Each holds only the capabilities it was started with. When one of them faults, the processor stops that program and nothing else; the kernel tells the supervisor, `init`, which starts a fresh copy with the same capabilities. A driver that stops answering is found by a ping, ended and replaced in the same way. Clients keep working because the message endpoints belong to `init` and outlive the server.

**What was measured.** Each fault was injected {{test.fault_runs}} times. "Until the service answers again" runs from just before the fault to the first answer from the new copy, on the system's own clock.

{{table.recovery}}

A hang takes longer than a crash because it has to be noticed: the supervisor pings every {{supervisor.ping_every_ms}} ms and waits up to {{supervisor.ping_timeout_ms}} ms for the answer.

```log
{{log.crash_fs}}
```

```log
{{log.hang_console}}
```

The same two faults in the browser demo, where the emulator is slower:

```weblog
{{weblog.faults}}
```

**What Inti does not solve.**

- There is no IOMMU support. A driver that holds I/O ports for a device capable of direct memory access could use that device to overwrite any memory. Neither of Inti's two servers drives such a device, so this is untested rather than solved.
- A hang is only found by the ping. A driver that answers pings but does its real work wrongly is not detected.
- The restart is not transparent in every case. A half-typed line and anything typed ahead are lost when the console driver restarts, and a client whose request was in flight sends it again, so a console write can appear twice.
- Inti has two small servers. Real systems have thousands of drivers, and MINIX 3 demonstrated this design with real drivers long ago: each driver is a separate user-mode process, and a reincarnation server restarts one that crashes ([MINIX 3 documentation, "Reliability"](https://wiki.minix3.org/doku.php?id=www:documentation:reliability); [Tanenbaum, Herder and Bos, IEEE Computer, 2006](https://doi.org/10.1109/MC.2006.156)).

## A kernel written in a memory-unsafe language

**The evidence.**

- About 70% of the vulnerabilities Microsoft assigns a CVE each year are memory-safety issues ([Microsoft Security Response Center, 16 July 2019](https://www.microsoft.com/en-us/msrc/blog/2019/07/a-proactive-approach-to-more-secure-code)).
- Around 70% of Chromium's high-severity security bugs are memory-unsafety problems ([The Chromium Projects, "Memory safety"](https://www.chromium.org/Home/chromium-security/memory-safety)).
- The Linux kernel has started to accept Rust code for this reason ([kernel.org, "Rust"](https://docs.kernel.org/rust/index.html)).

**What Inti does.** The kernel is written in Rust and does not link the `alloc` crate: it has no heap, and all of its state is one value made of fixed-size tables. Hardware access needs `unsafe` (port I/O, page tables, the switch between tasks); there are {{count.unsafe_kernel}} such blocks in the kernel and its interface crate, each with a comment that says why it is sound, and the gate script fails if one is missing. Arithmetic overflow checks stay on in the release build. The kernel never dereferences an address a program gave it. On top of that, a checker inside the kernel recounts everything the kernel owns against {{count.invariant_rules}} rules whenever a program with the right capability asks.

**What was measured.** The fuzzer keeps {{fuzz.kids}} throw-away programs running, each making random and almost-right system calls, and asks the kernel to check its rules every {{fuzz.check_every_ms}} ms.

```log
{{log.fuzz}}
```

**What Inti does not solve.**

- Rust's guarantees end where `unsafe` begins. The {{count.unsafe_kernel}} blocks were written and reviewed by one author; they are not verified.
- Memory safety does not rule out logic errors. The checker and the fuzzer found none in this run, which is evidence and not proof. seL4 has machine-checked proofs of functional correctness ([seL4 FAQ](https://sel4.systems/About/FAQ.html)); Inti has nothing of that kind.
- The fuzzer is random, not guided by coverage, and its programs hold no capability for the kernel log, so that one call is fuzzed only up to its capability check.
- Code pages are read-only, but this paging mode has no no-execute bit, so a program's data pages are executable.

## Programs that run with all the rights of their user

**The evidence.**

- A program that acts with the full power of its user has access to all of that user's resources. Watson and others call such implicit access ambient authority; their example is the web browser, which has to parse data from many untrusted sources. They built Capsicum to add capabilities to UNIX ([Watson, Anderson, Laurie and Kennaway, "Capsicum: practical capabilities for UNIX", USENIX Security 2010](https://www.usenix.org/legacy/event/sec10/tech/full_papers/Watson.pdf)).
- The classic statement of the problem is Hardy's confused deputy ([Hardy, "The Confused Deputy", ACM SIGOPS Operating Systems Review, 1988](https://doi.org/10.1145/54289.871709)), and the classic statement of the remedy is least privilege: every program should operate with the least set of privileges necessary to complete the job ([Saltzer and Schroeder, "The Protection of Information in Computer Systems", 1975](https://doi.org/10.1109/PROC.1975.9939)).

**What Inti does.** There is nothing a program can reach by naming it. Every system call starts from a slot in the caller's own capability table, a new program starts with exactly the capabilities its parent copied into it, and a capability can be copied with fewer rights or a smaller range but never more. A capability can also arrive in a message, but only when the sending and the receiving end of that conversation both carry the grant right, and whoever handed a capability out can take back every copy of it while the holders keep running. Memory, tasks and endpoints are lent the same way, as quotas that a parent gives its child. A program started from the shell gets one capability: the right to send text to the console.

**What was measured.** `rogue all` starts a hostile program with that one capability. It makes {{rogue.all_total}} attempts: {{rogue.software}} through the system call interface and {{rogue.by_cpu}} directly on the hardware. The kernel refuses {{rogue.by_kernel}} with an error code, the console driver refuses {{rogue.by_driver}}, the processor ignores {{rogue.ignored}} and stops the program on the other {{rogue.by_cpu}}. Among the attempts: creating an endpoint without endpoint quota, giving a child more task quota than it has, handing a capability to the console driver, and revoking capabilities it does not hold. Afterwards the kernel's rules still hold.

```log
{{log.rogue}}
```

In the browser demo one of the hardware attempts is skipped, because that attempt stops the v86 emulator with an internal error instead of raising the fault. The demo says so on the screen:

```weblog
{{weblog.rogue}}
```

**What Inti does not solve.**

- Delegation and revocation are simple. A capability travels in a message only between ends that both hold the grant right, and `cap_revoke` takes back all copies of a capability at once, not a chosen one. A call that a server has already taken is carried through even if the caller's capability is revoked meanwhile.
- The supervisor is trusted: `init` starts with every capability. The shell is more powerful than an ordinary program too: it holds the capability to read kernel information, the test capabilities it passes to the test programs, and a capability to ask `init` for a shutdown or an injected fault.
- Memory, tasks and endpoints are charged to quotas, so a hostile program can use up its own share and nothing else. Processor time is not charged: a program can take its full share of the processor.
- The list of attempts is long but it is a list. It shows that these attempts fail, not that no attempt can succeed.

## The sources, checked

`python tools/check_sources.py` fetched every source on {{sources.date}} and looked for the quoted words in its text: the visible text of a web page, or the text of a PDF as far as a small reader in the script can extract it. Some publishers refuse automated requests, and one site could not be reached from the machine that ran the check. For those the script looked in a second copy, which the table names: the paper's abstract in its OpenAlex record, or the same text on a university or project site. {{sources.confirmed}} of {{sources.total}} sources were confirmed this way, {{sources.confirmed_cited}} at the cited address itself and {{sources.confirmed_copy}} on a second copy. A source that cannot be confirmed is listed as such rather than dropped. Finding the words shows that the source says them; it is not a substitute for reading the source.

{{table.sources}}
