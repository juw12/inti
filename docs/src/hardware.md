# Inti on a real computer

**Inti has never been started on a physical computer.** Every statement on this page is marked as *read from the source* or *measured in an emulator*; nothing here was seen on real hardware. If you try it, what your screen says is new information, and the last section says what to write down.

## What the image needs

Read from the source (`boot/src/main.rs`, `kernel/src/arch.rs`, `kernel/src/mem.rs`, `user/src/bin/console.rs`, `user/src/bin/disk.rs`):

| Part | What Inti uses | What to expect without it |
|---|---|---|
| Firmware | A legacy BIOS, or UEFI firmware with its compatibility module (CSM) switched on. The boot sector asks the BIOS to read the disk (the extended read call) and for the memory map (E820). There is no UEFI loader. | The firmware does not offer the stick as a boot device, or the screen stays empty. |
| Processor | A 32-bit x86 processor with PAE paging; a 64-bit one runs Inti in 32-bit mode. The no-execute bit is used when the processor has it. | Without PAE the kernel stops with a message. Without the no-execute bit Inti runs and its first lines say that writable memory can be executed. |
| Number of processors | One. Others are never started. | - |
| Interrupts and clock | The two legacy interrupt controllers (8259) and the legacy timer (8254). | No clock tick: the system hangs early. Some recent machines have neither. |
| Screen | The VGA text screen. | Nothing to see; the serial port still carries every line. |
| Keyboard | The keyboard controller at port `0x60` and its interrupt. A USB keyboard works only if the firmware imitates that controller (often called "USB legacy support"). | The system starts and nothing can be typed, except over the serial port. |
| Serial port | COM1 at `0x3F8`, optional. Everything on the screen is also written there, and input is read from it. | - |
| Disk for files | The first drive on the primary ATA channel (eight ports from `0x1F0`, and `0x3F6`), by polling, without DMA. | The system starts, the disk server says `no ATA disk found`, and no file lasts. This is the case on SATA drives in AHCI mode, on NVMe drives and on USB sticks. |

## What was measured, in an emulator

On {{machines.date}} the built image was started on {{machines.count}} emulated machines ({{machines.emulator}} on {{machines.host}}, {{machines.duration}}; `python build.py machines` repeats it). {{machines.started}} of them started, {{machines.prompt}} reached the shell, {{machines.selftest_pass}} passed `selftest`, and {{machines.kept}} wrote a file to the disk and read it back. {{machines.no_storage}} reached the shell and kept no file.

{{table.machines}}

Each machine starts from the same image file, in a mode in which the emulator throws its writes away afterwards. "Keeps a file" means that `write machine.txt kept` was followed by a `cat machine.txt` that printed the line. A machine that did not start, or did not reach the shell, is listed with what the emulator or the serial port said.

`selftest` has one check that depends on how evenly the machine runs: three programs that spin in a group held to a `20`% share must leave the rest of the processor to a program outside the group. On these emulated machines, on a build machine that is shared with other work, that check does not pass every time. {{machines.selftest_sentence}} In the table of version `0.2.0` it failed on the machine with two processors. In an earlier run of the table it failed on the machine with the most memory, and in a still earlier run that machine failed one line whose name the tool did not keep then.

Two different things were seen behind those failures. In the table of version `0.2.0` the emulated processor fell to a tenth of its speed or less for seconds at a time; the check set all {{timing.tries}} of its measurements aside and failed for want of a steady one. The lines it printed are in `evidence/machines.json` as it stands at the tag `v0.2.0`; the file in this version holds the newer table. In a loop of sixty starts with `2048` MiB of memory and sixty with `64` MiB, taken in turn, one start failed, with `2048` MiB: the guest counted a hundred and fifty-nine clock ticks in two seconds where two hundred are due, and the check read thirty-eight percent where it asks for forty-five. The check then judged steadiness by the work done per tick. A machine that stalls loses ticks and work together, so the measurement was accepted as steady. That was a fault in the check and not in the scheduler: with the lost ticks counted, the same figures give about forty-eight percent. The loop set forty-five measurements aside with `2048` MiB and twenty-two with `64` MiB, and why they differ is not known. The loop is not in the repository and its output is not kept there.

From version `0.2.1` the check counts lost ticks: a measurement in which the guest kept fewer than {{timing.whole.percent}}% of the ticks that were due is set aside. A loop on that form then failed once for another reason, and the figure the check compares with was changed as well; [How Inti works](design.md) gives both changes and what each of them costs. On the image of this version the loop was run again and its output is kept in `evidence/timing/share-loop.jsonl`: {{timing.loop.runs}} runs of `selftest`, of which {{timing.loop.failed}} failed, {{timing.loop.failed_unmeasured}} of them because all {{timing.tries}} measurements were set aside and {{timing.loop.failed_measured}} on a figure. So the check is mended in one way and not in the other. It no longer reads a wrong share from a stalled machine, and it still fails when the build machine is busy through every measurement. A failed `selftest` in the table is therefore to be read with the lines the check printed, which the tool keeps.

An emulator is a tidy computer. The table shows which differences in processor, memory and disk controller Inti survives; it does not show that Inti runs on a machine you can touch.

## Try it from a USB stick

1. Get the image. `python build.py` writes `build/inti.img`; the repository carries the same bytes as `docs/inti.img`.
1. Write the image to a USB stick as it is, byte for byte. **This erases the stick.** On Windows, Rufus (choose "DD image" when it asks) and balenaEtcher do this. On Linux, find the device name of the stick with `lsblk` and be certain of it before you run:

   ```
   sudo dd if=inti.img of=/dev/sdX bs=1M conv=fsync
   ```

1. In the firmware setup of the computer, switch on legacy or CSM boot and switch off Secure Boot. Then choose the stick in the boot menu.
1. Within a few seconds the screen should show the kernel's first lines, a few lines from `init`, and the prompt `inti>`.

Started this way, Inti keeps no files: the image on the stick does hold a volume, and the disk server has no USB driver to reach it. It will say `no ATA disk found`, or it will look at an internal drive (see the next section).

## Before you start: what Inti can do to your disks

- **The boot sector and the kernel only read.** The boot sector reads the kernel from the device the BIOS started it from. The kernel has no disk driver. (Read from the source.)
- **The disk server looks at one drive: the first on the primary ATA channel.** On an older computer, or one whose SATA controller is set to IDE mode, that is an internal drive and not the stick.
- **On that drive it reads the partition table and writes nothing unless it finds a partition of type `{{disk.partition_type:raw}}` that holds a valid Inti volume.** It never creates or repairs a volume. In the emulator, {{storage.refusals_unchanged}} of {{storage.refusals}} disks without a valid Inti volume were left unchanged, byte for byte, after {{storage.refusal_attempts}} attempts to write each ([the storage page](storage.md) lists them). On a real drive this has never been tried.
- **So use a computer whose disks hold nothing you would miss, or unplug its internal drives first.**

## A ten-minute test

Type these at the prompt, in this order, and note what each one prints.

| Command | What should happen |
|---|---|
| `help` | the list of commands |
| `selftest` | {{selftest.checks}} lines that start with `ok`, then `SELFTEST: PASS` |
| `tour` | a hostile program is denied, a driver crashes and is replaced, a server hangs and is replaced; under a minute in the emulator |
| `df` | the files and free blocks of the volume, or the reason there is none |
| `uptime` | the time since the start and the kernel's counters; compare the time with a watch |
| `shutdown` | see below |

`shutdown` writes to the power-off ports of two emulators (QEMU and Bochs). A real computer has neither, so expect the line `init: this machine has no power-off port that init knows; still running`, and hold the power button. Nothing is lost by that: the disk server finishes every change before it answers.

## What may go wrong, and has never been tried

- **The A20 line.** The boot sector asks the BIOS to open it and then sets the bit at port `0x92`. A computer that needs the keyboard-controller method instead will fail before the kernel prints anything.
- **Reading from the stick.** The boot sector asks for `64` sectors in one call and does not try again after an error; it prints a short message and stops. Some BIOSes accept less in one call from a USB device.
- **The keyboard.** The firmware's imitation of a keyboard controller for USB keyboards differs from machine to machine.
- **The clock.** The time is taken from the legacy timer; how well it keeps time on a real machine is unknown.
- **A real ATA drive.** The disk server has only met the emulator's drive. It waits up to {{disk.step_s}} s for each step of a command, and that was tested by letting the emulator hold writes back ([the storage page](storage.md#a-slow-drive-and-one-that-stops)). A drive that sleeps and has to spin up, one that reports an error of its own, and one that needs longer than {{disk.find_s}} s to identify itself after power-on have never been met. In the last case the server says `no ATA disk found` and does not look again.

## If you try it

Open an issue in the repository with: the make and model of the computer and its year, how the firmware was set (legacy, CSM), a photo of the screen where it stopped, and the output of the commands above. A failure is as useful as a success.
