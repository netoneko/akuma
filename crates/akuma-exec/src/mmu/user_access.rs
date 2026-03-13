//! Safe user memory access primitives with fault handling.
//!
//! Uses a thread-local recovery handler to catch EL1 Data Aborts (EC=0x25)
// crate/akuma-exec/src/mmu/user_access.rs

use core::arch::global_asm;
use crate::threading::set_user_copy_fault_handler;

unsafe extern "C" {
    fn __arch_copy_user_memory(dst: *mut u8, src: *const u8, len: usize) -> u64;
    fn __arch_copy_user_fault();
}

global_asm!(
    r#"
    .section .text
    .global __arch_copy_user_memory
    .global __arch_copy_user_fault

    // x0 = dst, x1 = src, x2 = len
    // Returns 0 on success, non-zero (EFAULT) on error
    __arch_copy_user_memory:
        // Check for 0 length
        cbz x2, 2f
    1:
        // Byte copy loop
        ldrb w3, [x1], #1
        strb w3, [x0], #1
        subs x2, x2, #1
        b.ne 1b
    2:
        mov x0, #0
        ret

    // Fault handler - jumped to by exception handler
    // Returns EFAULT (14)
    __arch_copy_user_fault:
        mov x0, #14
        ret
    "#
);

/// Copy from user memory to kernel memory safely.
/// Returns Ok(()) on success, Err(EFAULT) on failure.
pub unsafe fn copy_from_user_safe(dst: *mut u8, src: *const u8, len: usize) -> Result<(), u64> {
    set_user_copy_fault_handler(__arch_copy_user_fault as *const () as usize as u64);

    // Ensure compiler doesn't reorder these calls
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);

    let res = unsafe { __arch_copy_user_memory(dst, src, len) };

    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
    set_user_copy_fault_handler(0);

    if res == 0 {
        Ok(())
    } else {
        Err(res)
    }
}

/// Copy to user memory from kernel memory safely.
/// Returns Ok(()) on success, Err(EFAULT) on failure.
pub unsafe fn copy_to_user_safe(dst: *mut u8, src: *const u8, len: usize) -> Result<(), u64> {
    // Same implementation logic for now (byte copy handles both directions)
    unsafe { copy_from_user_safe(dst, src, len) }
}
