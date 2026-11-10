Inti - a small capability microkernel operating system, written from scratch.

Everything outside the kernel is an ordinary program: the console driver,
the file server and this shell. Each one holds only the capabilities it was
given when it started, and a supervisor restarts the drivers when they stop.

Things to try:

  ps              the running programs
  caps 1          what the console driver is allowed to touch
  crash console   break the console driver and watch it come back
  hang fs         make the file server stop answering
  rogue           a program that tries to break out, one attempt at a time
  rogue all       the same plus one attempt the browser emulator cannot run
  selftest        the kernel checks itself
  fuzz 20000      random system calls against the kernel
  crash init      break the root program; the kernel restarts user space
  help            every command

This file is served by the file server from a read-only archive that was
loaded together with the kernel.
