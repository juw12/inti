/* Every user program is linked at 1 GiB in three parts that never share a page, so that the
   kernel maps each with one kind of access: constants (read-only), code (read-only and
   executable), data (writable). */
ENTRY(_start)
PHDRS {
    rodata PT_LOAD FILEHDR PHDRS FLAGS(4);
    text PT_LOAD FLAGS(5);
    data PT_LOAD FLAGS(6);
}
SECTIONS {
    . = 0x40000000 + SIZEOF_HEADERS;
    .rodata : { *(.rodata .rodata.*) } :rodata
    . = ALIGN(4096);
    .text : { *(.text .text.*) } :text
    . = ALIGN(4096);
    .data : { *(.data .data.*) *(.got .got.*) } :data
    .bss : { *(.bss .bss.*) *(COMMON) } :data
    /DISCARD/ : { *(.eh_frame*) *(.note*) *(.comment*) }
}
