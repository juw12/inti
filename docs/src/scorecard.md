# Scorecard: Inti next to real operating systems

Is Inti the best operating system in the world? **No.** It is not close, and one build session could not make it so. This page puts it next to five real systems, first on the things it was built to show and then on the things that make an operating system useful, and says plainly who is ahead.

How to read it: figures and failure behaviour of other systems carry a link in the same row. "Not compared" means that no claim is made because no source was checked. Inti's own figures come from the gate run of {{gates.date}} ({{test.emulator}}); they are emulator figures and `python build.py gates` reproduces them.

## What Inti was built to show

| | Inti | Linux | Windows | seL4 | MINIX 3 | Redox |
|---|---|---|---|---|---|---|
| Where drivers run | user mode, all of them | in the kernel, where the fault studies found them to be the most error-prone part ([Chou and others](https://dl.acm.org/doi/10.1145/502034.502042), [Palix and others](https://dl.acm.org/doi/10.1145/1961296.1950401)) | kernel-mode drivers share the kernel's address space ([Microsoft Learn](https://learn.microsoft.com/en-us/windows-hardware/drivers/gettingstarted/user-mode-and-kernel-mode)); a user-mode driver framework exists as well | user mode ([seL4 FAQ](https://sel4.systems/About/FAQ.html)) | user mode ([MINIX 3 documentation](https://wiki.minix3.org/doku.php?id=www:documentation:reliability)) | user space ([Redox book](https://doc.redox-os.org/book/microkernels.html)) |
| A failed driver is replaced without a reboot | yes: after {{recovery.console_crash.median_ms}} ms for a crash and {{recovery.console_hang.median_ms}} ms for a hang (median, emulated) | not compared | not for kernel-mode drivers: a crash in one crashes the whole system ([Microsoft Learn](https://learn.microsoft.com/en-us/windows-hardware/drivers/gettingstarted/user-mode-and-kernel-mode)) | left to the system built on top of the kernel; not compared | yes, by its reincarnation server ([MINIX 3 documentation](https://wiki.minix3.org/doku.php?id=www:documentation:reliability)) | not compared |
| Kernel language | Rust, with {{count.unsafe_kernel}} `unsafe` blocks | C, with Rust support being added ([kernel.org](https://docs.kernel.org/rust/index.html)) | C and C++ ([MSRC](https://www.microsoft.com/en-us/msrc/blog/2019/07/a-proactive-approach-to-more-secure-code)) | C and assembler ([Klein and others, SOSP 2009](https://www.sel4.systems/Research/pdfs/sel4-formal-verification-os-kernel.pdf)) | C | Rust ([Redox book](https://doc.redox-os.org/book/microkernels.html)) |
| Kernel size | {{loc.kernel_total}} lines | "over 8 million lines" by its own, old, documentation ([kernel.org](https://docs.kernel.org/process/1.Intro.html)) | not published | 8,700 lines of C and 600 of assembler when it was verified ([Klein and others, SOSP 2009](https://www.sel4.systems/Research/pdfs/sel4-formal-verification-os-kernel.pdf)) | about 4,000 lines of executable kernel code ([MINIX 3 documentation](https://wiki.minix3.org/doku.php?id=www:documentation:reliability)) | under 40,000 lines of Rust ([Redox book](https://doc.redox-os.org/book/microkernels.html)) |
| How a program gets access | only through capabilities it was handed, at its start or in a message; whoever handed one out can revoke every copy | users and permissions; a program runs with the ambient authority of its user ([Watson and others](https://www.usenix.org/legacy/event/sec10/tech/full_papers/Watson.pdf)) | users and access control lists | capabilities ([seL4 FAQ](https://sel4.systems/About/FAQ.html)) | not compared | not compared |
| Evidence that the kernel is correct | {{selftest.checks}} self-tests, {{fuzz.calls}} fuzzed system calls, a run-time checker with {{count.invariant_rules}} rules, {{model.states}} states explored on a host model, {{mutants.held_out.caught}} of {{mutants.held_out.compiled}} planted defects caught in a sample that no test was aimed at; **no proof** | not compared | not compared | machine-checked proofs of functional correctness, integrity and confidentiality ([seL4 FAQ](https://sel4.systems/About/FAQ.html)) | not compared | not compared |

Reading this table fairly:

- Against Linux and Windows, Inti shows the property it was built for: a failing driver is contained and replaced while everything else keeps running. It shows it with one driver and one file server, in an emulator, under no real workload.
- Against seL4, MINIX 3 and Redox, Inti shows nothing they do not already do better. MINIX 3 has restartable user-mode drivers at real scale, seL4 has capabilities with proofs, and Redox is a Rust microkernel system with real applications.
- Inti's kernel is small because it does little, not because it does the same with less. The line counts in the table are not a like-for-like comparison.

## What makes an operating system useful

| | Inti | Real systems |
|---|---|---|
| Hardware | one emulated PC, in QEMU and in v86; it has never run on a real machine | Linux and Windows run on almost everything; seL4 has proofs for configurations on Arm, x86 and RISC-V ([seL4 FAQ](https://sel4.systems/About/FAQ.html)) |
| Processors | one, in 32-bit mode | many, in 64-bit mode |
| Network | none | yes |
| Storage | a read-only archive loaded at boot; nothing is saved | real file systems |
| Programs | {{size.programs}} small ones, and a shell without pipes or scripts | everything people use computers for |
| Drivers | one, for the screen, the keyboard and the serial port | thousands |
| Assurance | tests, a fuzzer, a run-time checker and a bounded exploration on a host, by one author | decades of use; for seL4, proofs |
| Age | built in one session on {{gates.date}} | years to decades |

## Where Inti is ahead, and how narrow that is

- **Fault containment against monolithic kernels.** In Inti a driver crash is an event the system recovers from in milliseconds, and the supervisor crashing is survivable as well. On Windows a crash in a kernel-mode driver crashes the system ([Microsoft Learn](https://learn.microsoft.com/en-us/windows-hardware/drivers/gettingstarted/user-mode-and-kernel-mode)). This is a real difference in design, and it is the oldest argument for microkernels, not Inti's invention.
- **Reproducible evidence.** One command rebuilds the system, repeats every measurement and checks every number in these documents against the raw log. That is a property of how this project presents itself, not of the operating system.
- **Trying it takes no installation.** The fault injection runs in a browser tab. Other small systems run in browser emulators too; what Inti adds is that breaking it is the point of the demo.

## Where Inti loses

- **Hardware and software.** No real hardware, no network, no persistent storage, no applications, one processor.
- **Assurance.** No formal verification, one author, one day. The exploration of the kernel on a host covers small configurations to a small depth and proves nothing beyond them. seL4's proofs are in a different class.
- **Security mechanisms.** W^X is enforced only on processors with the no-execute bit and has only been shown in emulators; there is no address-space randomisation and no IOMMU. Revocation takes back all copies of a capability or none, and neither revocation nor delegation is proven correct. The supervisor is trusted with everything.
- **Robustness.** A hang is found only by a ping; the kernel is not pre-emptible; the scheduler has no processor quotas, so a program can take its full share of the processor.
- **Performance.** Never measured outside an emulator, so nothing is claimed.

The full list is in [How Inti works](design.md), under "Limitations".

## Verdict

Inti is not number one, and "the best operating system in the world" is not a claim that this project makes or could defend. What it is: a complete, bootable system small enough to read in an afternoon, in which three documented weaknesses of mainstream systems are absent by construction, with every measurement reproducible and every limit stated.
