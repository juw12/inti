/* The BIOS loads the 512-byte boot sector at 0x7C00. */
ENTRY(_start)
SECTIONS {
    . = 0x7C00;
    .boot : { KEEP(*(.boot)) }
    /DISCARD/ : { *(.eh_frame*) *(.note*) *(.comment*) *(.text*) *(.rodata*) *(.data*) *(.bss*) }
}
