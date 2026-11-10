/* The boot sector copies the kernel to 1 MiB and jumps to its first byte. */
ENTRY(_start)
SECTIONS {
    . = 0x100000;
    __kernel_start = .;
    .text : { KEEP(*(.head)) *(.text .text.*) }
    .rodata : { *(.rodata .rodata.*) }
    . = ALIGN(4096);
    __rodata_end = .;                 /* text + rodata are mapped read-only */
    .data : { *(.data .data.*) *(.got .got.*) }
    .bss (NOLOAD) : ALIGN(16) {
        __bss_start = .;
        *(.bss .bss.*) *(COMMON)
        . = ALIGN(4096);
        __bss_end = .;                /* the boot archive is moved here */
    }
    /DISCARD/ : { *(.eh_frame*) *(.note*) *(.comment*) }
}
