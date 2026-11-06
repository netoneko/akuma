//! The interrupt entry stub for the TLB-shootdown and wake IPIs.
//!
//! A hardware interrupt cannot enter a Rust function: the CPU pushes an `iretq` frame, `swapgs` has to
//! happen on a ring-3 entry, and the caller-saved registers are live. So the vector points at this stub,
//! which does that bookkeeping and calls one `extern "C"` function, which calls the handler the kernel
//! registered. Moved here from `akuma-tokio-kernel-amd64` (2026-11-06) so that crate can
//! `forbid(unsafe_code)`: the `global_asm!`, the `#[unsafe(no_mangle)]` and the `unsafe extern` are all
//! link-level constructs.

use core::sync::atomic::{AtomicUsize, Ordering};

static HANDLER: AtomicUsize = AtomicUsize::new(0);

/// Register the function the stub runs (service a pending shootdown, send the EOI). Call before the vector is
/// installed in the IDT.
pub fn set_handler(f: fn()) {
    HANDLER.store(f as usize, Ordering::Release);
}

/// The vector body (called from `shootdown_entry`).
#[unsafe(no_mangle)]
extern "C" fn shootdown_dispatch(_frame: *const u8) {
    let h = HANDLER.load(Ordering::Acquire);
    if h != 0 {
        // SAFETY: `h` was stored from a `fn()` by `set_handler` (the only writer) and is never any other value.
        let f: fn() = unsafe { core::mem::transmute::<usize, fn()>(h) };
        f();
    }
}

core::arch::global_asm!(
    r#"
    .section .text
.global shootdown_entry
shootdown_entry:
    test qword ptr [rsp + 8], 3
    jz 1f
    swapgs
1:
    push rbp
    push rax
    push rcx
    push rdx
    push rsi
    push rdi
    push r8
    push r9
    push r10
    push r11
    sub rsp, 8
    cld
    lea rdi, [rsp + 88]
    call shootdown_dispatch
    add rsp, 8
    pop r11
    pop r10
    pop r9
    pop r8
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    pop rax
    pop rbp
    test qword ptr [rsp + 8], 3
    jz 2f
    swapgs
2:
    iretq
"#
);

unsafe extern "C" {
    fn shootdown_entry();
}

/// The entry stub's address, for `IDT.set(vector, ..)`.
#[must_use]
pub fn entry_addr() -> usize {
    shootdown_entry as *const () as usize
}
