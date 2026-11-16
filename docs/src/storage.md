# Files that last: the disk server

Inti keeps files on a disk. The driver for the disk and the file system on it are one ordinary program, `disk`, which runs in ring 3 with three capabilities and is restarted like any other server when it fails. This page says what it promises, how the volume is laid out, what was tested, and where it stops.

Everything measured here is from the storage test of {{storage.date}} in {{storage.emulator}}: **{{storage.checks_ok}} of {{storage.checks}} checks pass.** `python build.py storage` repeats it (about {{storage.minutes}} minutes). No physical disk was involved at any point; [Inti on a real computer](hardware.md) says what that leaves open.

## What it promises

- **A change is whole or absent.** After a power cut, a file that was being written, replaced, extended or removed is its old version or its new one, and every other file is untouched.
- **A disk it does not know is never written.** The server never creates a volume. On a disk without a valid Inti volume it answers every request with a reason and writes nothing.
- **It writes only inside its volume.** Every write goes through one function that refuses a block outside the mounted volume.
- **A crash of the server loses no file.** The supervisor starts a new copy, which reads the volume again; a request that was under way is sent again by the client.

The promises hold under the assumptions in [what the tests model](#what-the-tests-model), and they have only been checked in an emulator.

## Using it

| Command | What it does |
|---|---|
| `ls` | the files in the boot archive, then the files on the disk |
| `cat FILE` | print a file |
| `write FILE TEXT` | replace the file with one line |
| `append FILE TEXT` | add a line to the file |
| `edit FILE` | type the new content line by line; a line with only `.` saves it |
| `cp FROM TO` | copy a file, from the archive or the disk, to the disk |
| `rm FILE` | remove a file from the disk |
| `df` | how many files and free blocks the disk has |
| `crash disk`, `hang disk` | make the disk server fail, and watch it being replaced |
| `counter +disk` | run a program that is handed the disk (see [Write a program for Inti](apps.md)) |

`python build.py run` keeps the disk in `build/run.img` from one start to the next; `python build.py run --fresh` starts from the built image again. In the browser demo the files last until the page is reloaded.

The built image carries the content of the folder `disk/` on its volume (files: {{storage.seeded_files}}; {{storage.seeded_list}}), which says how to try this. After the first start the disk server was ready {{storage.ready_ms}} ms after it was started, and {{storage.free_blocks_at_start}} of {{disk.seed_data_blocks}} data blocks were free.

## The volume

The volume is a partition of type `{{disk.partition_type:raw}}` in an ordinary partition table, so other systems see a partition of a kind they do not know and leave it alone. The format is called `INTIFS01`.

| | |
|---|---|
| Block | {{disk.block_bytes}} bytes ({{disk.block_sectors}} sectors of {{disk.sector_bytes}} bytes) |
| Blocks in a volume | at most {{disk.max_blocks}}, so a volume holds at most {{disk.max_volume_mib}} MiB |
| Files | at most {{disk.max_files}}, in one directory |
| File names | one to {{disk.name_bytes}} characters from `A-Z a-z 0-9 . _ -` |
| The volume in the built image | {{disk.seed_blocks}} blocks ({{disk.seed_mib}} MiB), starting {{disk.seed_start_mib}} MiB into the disk |

The first two blocks of the volume are the two **meta slots**. A meta block holds everything except file content: a magic word, a sequence number, the size of the volume, the directory, a table with one entry per block that says which block follows it in its file, and a checksum (CRC-32) over the rest. All other blocks hold file content.

The disk server is {{disk.server_lines}} lines of Rust. A second implementation of the format, `tools/intifs.py` ({{disk.reader_lines}} lines of Python), runs on the build machine. It builds the volume in the image, and the tests use it to read back what the machine wrote. The two share no code, and `tools/claims.py` refuses to render this page if they disagree on a size or an offset.

## How a change is made

1. New content is written to blocks that no file of the current version uses.
1. The drive is told to flush.
1. A new meta block, with the sequence number one higher, is written to the slot that does **not** hold the current one.
1. The drive is told to flush again. Only now is the change answered as done.

When the server starts, it reads both slots. A slot counts only if its magic word and checksum are right, and of two that count the one with the higher sequence number is used. A cut before step three leaves the old meta block in charge and the new content in blocks that nothing refers to. A cut during step three leaves a torn block whose checksum is wrong, and the old one is used. A cut after it leaves the new one.

Replacing a file therefore needs free room for the new version while the old one still exists. An `append` leaves the full blocks of the file where they are; it writes the partly filled last block again, with the new bytes added, to a free block.

The server holds one unfinished new version at a time. When a second program starts to write a file before the first has finished, the first is told "another writer started first" and has to send its file again; the library call `save` does that by itself a few times. An `append` names the size it expects the file to have, and is told "the file changed in the meantime" if it has another.

## A slow drive, and one that stops

The server polls the drive. A step of a command (the drive becoming ready, taking a sector, finishing a flush) is usually over at once, but a drive that was asleep has to spin up first, and one that is emptying its cache can take seconds. Three rules cover that.

- **It waits up to {{disk.step_s}} s for a step**, and then answers the request with "the disk reported an error". A drive that has not identified itself yet, or that is being reset after a failed step, gets {{disk.find_s}} s.
- **While it waits, it goes on answering the supervisor.** The supervisor ends a server that leaves a ping unanswered for {{supervisor.ping_timeout_ms}} ms, which is far less than a slow drive needs. So between two looks at the drive the server looks at its request endpoint. A ping is answered at once. Any other request is kept, and served in its turn when the drive is done.
- **After a failed step the channel is reset before the next command.** The drive may still be in the middle of the command that was given up, and would take the bytes of the next one for the rest of it.

A change that is given up this way is answered as failed. Whether it happened depends on the step that failed: before the new meta block is whole on the disk, the volume keeps the version from before, and that is the case the test below produces. If the step that fails is the last flush, the new version may be on the disk although the request was answered with an error; the server then reads the volume again instead of guessing which one it has.

The first two rules replace an earlier, shorter patience of three tenths of a second for a step, with no answer to pings during it. That was enough for the emulator's drive nearly every time, and not every time: a check of this test failed in two of four hundred rounds of a loop that ran only that case. The error it reported was the server's own impatience and not the drive's: one step had taken longer than the server was willing to wait.

## What was tested

Every case boots a copy of the built image in QEMU, lets the machine change the disk, and then reads the copy on the build machine with `tools/intifs.py`.

{{table.storage_checks}}

### Power cuts at random moments

The emulator's process was killed {{storage.cuts}} times in a row during a change, on the same disk, and the machine was started again each time. {{storage.cuts_before}} cuts fell before the first write of the change, {{storage.cuts_during}} between the first write and the switch to the new meta block, and {{storage.cuts_after}} after it. The volume was broken after {{storage.cuts_broken}} of them. {{storage.cuts_texts}} file contents were compared with what they had to be.

{{table.storage_cuts}}

### Every cut point of a recorded run

Random cuts mostly miss the narrow moments. So in a second case QEMU records every sector the machine writes while it makes {{storage.recorded_changes}} changes ({{storage.recorded_list}}): {{storage.sector_writes}} sector writes and {{storage.flushes}} flushes. The build machine then constructs every state a cut could leave:

- a cut after any one sector write: {{storage.cut_states}} states;
- the writes since the last flush lost in any combination, for the {{storage.meta_periods}} periods in which a meta block was written ({{storage.lost_meta_states}} states) and the {{storage.data_periods}} in which data was written ({{storage.lost_data_states}} states).

Each of the {{storage.states}} states is checked with `tools/intifs.py`: the volume must be sound and hold exactly the files from before or from after the change under way. The machine was then started on {{storage.machine_starts}} of them.

### Disks it must not touch

{{storage.refusals}} disks without a usable volume were each given {{storage.refusal_attempts}} attempts to write, also after the disk server had been crashed and restarted. The image file was byte for byte the same afterwards for {{storage.refusals_unchanged}} of {{storage.refusals}}.

{{table.storage_refusals}}

### A drive that is slow, or stops

QEMU can hold back what a machine writes to its drive. Three sessions use that.

- **Slow.** One write a second is let through. `write c.txt slow` took {{storage.slow_write_s}} s and went through, and the supervisor's log has no line about the disk server.
- **Stopped, then free again.** One byte a second is let through, so the write does not finish. The error came after {{storage.stopped_error_s}} s. Then the limit is lifted, and the next read took {{storage.stopped_read_s}} s.
- **Still busy.** Sixteen bytes a second are let through, and the limit stays in place while the next read is sent. The error came after {{storage.busy_error_s}} s and the read took {{storage.busy_read_s}} s.

In both of the last two the read gave the right file, the unfinished file was absent, in the machine's listing and in the image, a write after it landed, and the disk server was not restarted.

Two things this does not show. First, the last session reaches the reset of a drive that is still busy, but QEMU lets the held-back write through before it carries the reset out, so a drive that ignores a reset was not met. Second, no request from a second program ever arrived while the drive was busy: in the shipped system the shell waits for the program it started, so the path that keeps such a request and serves it later has never run with a request in it.

### Two implementations against each other

{{storage.fuzz_cases}} meta blocks were changed at random, with the checksum made right again so that the change is not caught there. The disk server and `tools/intifs.py` had to agree on whether each volume is sound: {{storage.fuzz_sound}} were sound, {{storage.fuzz_damaged}} damaged, and they disagreed on {{storage.fuzz_disagreements}}.

### How long a change takes

Measured as {{storage.timing_what}}, {{storage.timing_runs}} runs each. These are emulator figures and include the time the serial port takes; they say nothing about a real disk.

{{table.storage_timing}}

## Faults planted in the disk server

A test that passes says little unless it would fail on a broken server. `python build.py storage-mutants` plants {{storage.mutants.planted}} faults in the disk server, one at a time, and runs a shortened storage test ({{storage.mutants.cuts}} random cuts, {{storage.mutants.fuzz}} random meta blocks, {{storage.mutants.checks}} checks) against each. A fault counts as caught only if the test fails twice. The unchanged server passed before and after the batch. Run of {{storage.mutants.date}}, about {{storage.mutants.minutes}} minutes: **{{storage.mutants.caught}} of {{storage.mutants.planted}} caught.**

{{table.storage_mutants}}

Not caught, or not tested: {{storage.mutants.survivors}}. Before the run, each fault was marked as expected to be caught or not. Expected to get through: {{storage.mutants.expected_not_caught}}, because it removes a second check behind one that already refuses the same input. Verdicts that differ from what was expected: {{storage.mutants.surprises}}.

Three earlier runs of this batch are not counted. In the first, one fault (`older-slot`) was planted as text that did not compile, and one (`shared-block`) got through, because the only damaged volume of that kind in the test was also refused for a second reason. The planted text was corrected and a damaged volume that only the removed check refuses was added to the test. The second run had those corrections, and it went stale when the disk server was changed to wait for a slow drive: its figures were for a server that no longer exists. The third run was against the server as it is now, with two faults added for that change (`no-ping-while-waiting` and `no-reset-after-failure`). Before it, each of the two was run against the slow-drive case alone, to see that it fails the checks written for it and not some other one. That run was thrown away at its very end: the unchanged server, tested once more after the last fault, failed, and the rule of the batch is that nothing is written then. The cause was in the test harness, not in the server, and [the next section](#a-fault-in-the-test-harness) describes it. The figures above are from the fourth run: the same server, the same faults, the corrected harness. Planted again on their own after the batch: {{storage.mutants.redone_list}}.

{{table.storage_mutants_by}}

The faults are a hand-written list, chosen by the author of the server, of the mistakes this design invites. That is weaker than the evenly spaced samples used for the kernel, and it says nothing about faults nobody thought of.

## A fault in the test harness

The third run of the batch ended with a machine that seemed to start and print nothing. The harness waited for the prompt, received no byte from the serial port, and reported the unchanged server as failed. The machine was fine. The fault was in `tools/vm.py`, which every test in QEMU uses.

The harness reaches the machine's serial port over a TCP connection, and QEMU waits for that connection before it starts the machine. QEMU needs a moment before it listens, so the harness tried to connect, gave up after one second, and tried again. On the build machine, which runs Windows, the system itself goes on trying for about two seconds before it reports that nobody listens. An attempt the harness had already given up could therefore still arrive a moment later and be accepted. QEMU took it for the serial connection, started the machine, and the machine printed the lines of its start into a connection nobody held. When the harness connected again, those lines were gone, and it waited for a prompt that had already been printed.

The two halves of this were reproduced apart from each other.

- `python tools/connect_race.py` plays both sides with plain sockets, without QEMU. Of one hundred and fifty attempts on the build machine, three were given up by the client and accepted by the server.
- With a first connection opened and dropped on purpose, the old harness received no byte in three seconds from a machine that QEMU reported as running, and that printed its prompt when a key was sent to it. The corrected harness received the whole start in the same situation.

The failure as it happened in the batch was not seen again in forty ordinary starts. That this is what happened then is a conclusion from the mechanism and from the matching symptom; it was not observed.

For the fourth run of the batch the harness was changed in three ways. It started QEMU with the machine stopped and let it run only when the connections were open. It waited five seconds for a connection, which is longer than the system takes to refuse one. And when it gave up waiting for output, the message said whether the machine was running and where it was. The rule of the batch was left as it was: the unchanged server has to pass before and after, and a failed run is not tried again.

A start lost in this way shows as a failed run, never as a passed one. A planted fault, though, is counted as caught when the test fails, so a lost start could in principle be taken for a catch; that is why a fault has to fail twice, and why the table above names the check that failed both times.

### A second fault, found after the batch

In the fourth run, five checks failed in one of a fault's two runs and passed in the other. One of them is a check that cuts the power at random moments (`slot-in-use`, first run); it catches that fault only when a cut happens to fall in the right place, so it is expected to differ between runs. The other four have nothing to do with the fault that was planted. Three are the same check, "the case reached its end" of the differential case (`older-slot` in its second run, `shared-block` and `partition-past-end` in their first). The fourth is "shutdown switches the machine off" (`reuse-live-block`, second run).

To find out, the differential case was run in a loop on the unchanged build. Of six hundred sessions, two failed, and both failed while the harness was starting QEMU, before the machine had run. In one, the harness had picked a free port by opening and closing it, and another program took the port before QEMU did. In the other, a wait during the start ran out after sixteen seconds. The likeliest step is the greeting on QEMU's control channel, but the message of that time did not name the step, so this is not proven.

The harness now works the other way round. It opens its two listening sockets itself and keeps them, and QEMU connects to them. No port is picked and then let go, and there is no attempt to connect that could be given up and still arrive, so the fault of the third run cannot happen in that form either. The machine is still started stopped and released only when both channels are open. Each step of the start has its own message, with the time every step took, the state of QEMU and whatever QEMU wrote as an error. A start that stalls while QEMU is alive is thrown away and made again, at most three starts in all. That happens only before the machine has run its first instruction, and every such start is printed and counted. A QEMU that ends by itself is not started again.

With that harness the same loop ran one thousand sessions with no failure and no start made a second time. If the old rate were exactly two in six hundred, a thousand clean sessions would turn up by chance about one time in thirty; this is good evidence and not a proof. Sessions that take many seconds still occur. Most take about one second and the slowest of the thousand took about twenty-one, and at which stage the time goes is not known. None of them failed.

What this means for the figures above. The batch was run with the harness as it was before this second change, and it was not run again. The three failures of "the case reached its end" are what a lost start looks like, and are taken to be that. The failure of "shutdown switches the machine off" is not explained. None of the four is part of a verdict: every fault counted as caught failed both of its runs on the checks written for it, and the table names them. `tools/vm.py` starts the machine and does not decide what passes, and it is not among the files whose change makes the batch stale.

## What the tests model

- **A sector is written whole or not at all.** A drive that leaves half a sector is not modelled.
- **What a flush has acknowledged stays.** A drive that reports a flush done and then loses the data is not modelled. Cheap drives that do this exist, and on such a drive the promises above do not hold.
- **A power cut is the emulator's process being killed**, or a state built from the recorded writes. No power was ever cut on a physical machine.
- **The disk is QEMU's emulated ATA disk.** A slow drive is that disk with its writes held back by QEMU. Error reporting, a drive that sleeps and wakes, and the behaviour of a real controller are not covered.

## Where it stops

- **One directory, no owners.** Whoever holds the capability for the disk can read, replace and remove every file. The shell decides which program gets that capability (`NAME +disk`); there are no per-file rights.
- **Small.** At most {{disk.max_files}} files and {{disk.max_volume_mib}} MiB. Replacing a file needs room for both versions.
- **One kind of disk.** Only the first drive of the primary ATA channel, by polling. A disk behind AHCI or USB is not found, and the system then runs without files that last; [Inti on a real computer](hardware.md) shows this on emulated machines.
- **No repair.** If the newest meta block has a right checksum but describes an impossible volume, the server reports the volume as damaged and does not fall back to the older slot. There is no tool that repairs a volume.
- **An untested path.** When the disk server dies while it holds a request, the client sends the request again, up to a fixed number of times. No test makes the server die at exactly that moment.
- **A dead drive is slow to report.** A request waits up to {{disk.step_s}} s for a drive that has stopped answering before it gets its error, and nothing can cancel it. The supervisor does not step in, because the server answers its pings all the while.
- **A drive is looked for once.** A drive that needs longer than {{disk.find_s}} s to identify itself when the server starts is reported as missing, and is not looked for again until the server is restarted.
- **Requests kept during a wait are untested**, as said above.
- **No cache and no speed.** Every change is flushed twice before it is answered.
- **Nothing here was run on a physical disk.**
