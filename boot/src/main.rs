//! Inti boot sector: 512 bytes that take a PC from the BIOS to the kernel.
//!
//! 1. read the kernel + boot archive from the disk (LBA 1 onwards) to 0x10000,
//! 2. ask the BIOS for the memory map (E820) and store it at 0x8000,
//! 3. enable the A20 line, enter 32-bit protected mode,
//! 4. copy the payload to 1 MiB and jump to it with EAX = "INTI", EBX = 0x8000.
//!
//! `build.py` patches `chunk_count` (offset 0x1B0) with the payload size in 32 KiB chunks.
#![no_std]
#![no_main]

use core::arch::global_asm;

global_asm!(
    r#"
    .section .boot, "ax", @progbits
    .code16
    .global _start
_start:
    cli
    xorw %ax, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw $0x7c00, %sp
    ljmp $0, $cs_is_zero
cs_is_zero:
    sti
    cld
    movb %dl, boot_drive

    /* 1. load the payload, 64 sectors (32 KiB) per BIOS call */
    movw chunk_count, %cx
load_chunk:
    pushw %cx
    movb $0x42, %ah
    movb boot_drive, %dl
    movw $dap, %si
    int $0x13
    jc disk_error
    popw %cx
    addw $0x0800, dap_segment
    addl $64, dap_lba
    loop load_chunk

    /* 2. E820 memory map: u16 count at 0x8000, 24-byte entries from 0x8004 */
    movw $0, 0x8000
    xorl %ebx, %ebx
    movw $0x8004, %di
e820_next:
    movl $0xE820, %eax
    movl $0x534D4150, %edx
    movl $24, %ecx
    movl $1, 20(%di)
    int $0x15
    jc e820_done
    cmpl $0x534D4150, %eax
    jne e820_done
    incw 0x8000
    addw $24, %di
    cmpw $40, 0x8000
    jae e820_done
    testl %ebx, %ebx
    jnz e820_next
e820_done:

    /* 3. A20 gate, then protected mode */
    movw $0x2401, %ax
    int $0x15
    inb $0x92, %al
    orb $2, %al
    andb $0xFE, %al
    outb %al, $0x92

    cli
    lgdtl gdt_desc
    movl %cr0, %eax
    orl $1, %eax
    movl %eax, %cr0
    ljmp $0x08, $protected

disk_error:
    movw $disk_msg, %si
print_next:
    lodsb
    testb %al, %al
    jz halt
    movb $0x0E, %ah
    xorw %bx, %bx
    int $0x10
    jmp print_next
halt:
    hlt
    jmp halt

    .code32
protected:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw %ax, %fs
    movw %ax, %gs
    movl $0x7c00, %esp

    /* 4. move the payload above 1 MiB and enter the kernel */
    movl $0x10000, %esi
    movl $0x100000, %edi
    movzwl chunk_count, %ecx
    shll $13, %ecx
    rep movsl
    movl $0x49544E49, %eax
    movl $0x8000, %ebx
    movl $0x100000, %edx
    jmp *%edx

    .balign 4
gdt:
    .quad 0
    .quad 0x00CF9A000000FFFF
    .quad 0x00CF92000000FFFF
gdt_desc:
    .word 23
    .long gdt
dap:
    .byte 0x10, 0
    .word 64
    .word 0
dap_segment:
    .word 0x1000
dap_lba:
    .long 1
    .long 0
boot_drive:
    .byte 0
disk_msg:
    .asciz "Inti: disk read error"

    .org 0x1B0
chunk_count:
    .word 0
    .org 0x1FE
    .word 0xAA55
"#,
    options(att_syntax)
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
