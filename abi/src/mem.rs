//! The four memory routines the compiler expects to exist.
//!
//! They are written in assembly so that the compiler cannot turn them into calls to
//! themselves. Both the kernel and every user program link this one copy.

use core::arch::naked_asm;
use core::ffi::c_void;

/// # Safety
/// `dst` and `src` must be valid for `n` bytes and must not overlap.
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    naked_asm!(
        "push esi",
        "push edi",
        "mov edi, [esp + 12]",
        "mov esi, [esp + 16]",
        "mov ecx, [esp + 20]",
        "mov eax, edi",
        "mov edx, ecx",
        "shr ecx, 2",
        "rep movsd",
        "mov ecx, edx",
        "and ecx, 3",
        "rep movsb",
        "pop edi",
        "pop esi",
        "ret",
    )
}

/// # Safety
/// `dst` and `src` must be valid for `n` bytes; they may overlap.
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn memmove(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    naked_asm!(
        "push esi",
        "push edi",
        "mov edi, [esp + 12]",
        "mov esi, [esp + 16]",
        "mov ecx, [esp + 20]",
        "mov eax, edi",
        "cmp edi, esi",
        "jbe 2f",
        "lea edx, [esi + ecx]",
        "cmp edi, edx",
        "jae 2f",
        // Overlapping with dst above src: copy backwards.
        "lea esi, [esi + ecx - 1]",
        "lea edi, [edi + ecx - 1]",
        "std",
        "rep movsb",
        "cld",
        "jmp 3f",
        "2:",
        "rep movsb",
        "3:",
        "pop edi",
        "pop esi",
        "ret",
    )
}

/// # Safety
/// `dst` must be valid for `n` bytes.
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn memset(dst: *mut c_void, value: i32, n: usize) -> *mut c_void {
    naked_asm!(
        "push edi",
        "mov edi, [esp + 8]",
        "movzx eax, byte ptr [esp + 12]",
        "imul eax, eax, 0x01010101",
        "mov ecx, [esp + 16]",
        "mov edx, ecx",
        "shr ecx, 2",
        "rep stosd",
        "mov ecx, edx",
        "and ecx, 3",
        "rep stosb",
        "mov eax, [esp + 8]",
        "pop edi",
        "ret",
    )
}

/// # Safety
/// `a` and `b` must be valid for `n` bytes.
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn memcmp(a: *const c_void, b: *const c_void, n: usize) -> i32 {
    naked_asm!(
        "push esi",
        "push edi",
        "mov esi, [esp + 12]",
        "mov edi, [esp + 16]",
        "mov ecx, [esp + 20]",
        "xor eax, eax",
        "test ecx, ecx",
        "repe cmpsb",
        "je 2f",
        "movzx eax, byte ptr [esi - 1]",
        "movzx ecx, byte ptr [edi - 1]",
        "sub eax, ecx",
        "2:",
        "pop edi",
        "pop esi",
        "ret",
    )
}

/// # Safety
/// `a` and `b` must be valid for `n` bytes.
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn bcmp(a: *const c_void, b: *const c_void, n: usize) -> i32 {
    naked_asm!("jmp {memcmp}", memcmp = sym memcmp)
}
