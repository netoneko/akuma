//! Kernel console — formatting, cross-core serialisation, and line input.
//!
//! # This module forbids `unsafe`
//!
//! It held three `unsafe` blocks, one per PL011 register access, until
//! 2026-08-31. [`akuma_primitives::mmio::MmioReg`] collapsed them to one, and
//! that one moved to `crates/akuma-uart` — the device is a crate now, and what
//! is left here is *policy*: IRQ masking, the opt-in cross-core `Spinlock` and
//! its reentrancy guard, the `MULTICORE` runtime gate, and every formatting
//! helper. See `docs/archive/AKUMA_UART_EXTRACTION.md`.
//!
//! The ban is `akuma-kernel-core`'s crate-level one since this module moved
//! there (2026-09-01) — it used to be a module-local `#![forbid(unsafe_code)]`,
//! back when the surrounding crate could not carry one. That local attribute is
//! why `scripts/cloc_akuma.py` reported `akuma-kernel-glue` as forbidding while
//! it still held boot assembly: the script marks a crate when ANY file in it
//! carries the attribute.
//!
//! The ban does not mean the console is proven sound — it means the one genuinely
//! unsafe operation, vouching that `DEV_UART_VA` is a mapped PL011 window, is
//! stated once in the crate that owns the device instead of at each access.

use crate::alloc::string::ToString;
use alloc::vec::Vec;
#[cfg(kernel_console_lock)]
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
#[cfg(kernel_console_lock)]
use spinning_top::Spinlock;

// ============================================================================
// Cross-core serialization (opt-in via `CONSOLE_LOCK=1`)
// ============================================================================
//
// `with_irqs_disabled` alone only masks IRQs on the *calling* core, so under
// `smp-shared` (real multi-core, the default) two cores can both be inside
// `emit()`'s byte loop at once and byte-interleave each other's lines at the
// shared PL011 data register. When `kernel_console_lock` is set (build.rs:
// `CONSOLE_LOCK=1`), a `Spinlock` + owner-core-ID reentrancy guard serializes
// the loop across cores. The reentrancy guard is what keeps the panic handler
// (`src/main.rs:127`) and sync-exception paths in `src/exceptions.rs` safe:
// if a panic/fault lands while *this* core already holds the lock, the owner
// check short-circuits the acquire instead of self-deadlocking. Background:
// docs/archive/UART_SMP_INTERLEAVE_FIX.md.
#[cfg(kernel_console_lock)]
static CONSOLE_LOCK: Spinlock<()> = Spinlock::new(());
/// `current_core_id() + 1` of the lock holder, or 0 if free. Per-core
/// reentrancy guard for the panic / sync-exception path.
#[cfg(kernel_console_lock)]
static CONSOLE_OWNER: AtomicU8 = AtomicU8::new(0);

/// Is there more than one core that could be inside `emit()`?
///
/// The lock is compiled in, but acquiring it is gated on this. Both halves of the
/// gate are correctness, not optimization:
///
/// * **Acquiring while single-core can deadlock.** The lock is taken inside
///   `with_irqs_disabled`, so a print issued from a section that already runs with
///   preemption disabled spins on a `Spinlock` whose holder may be a thread that
///   cannot be scheduled. With another core running, that core drains it; with one
///   core, nothing can, and the kernel wedges with no output. This is why
///   `platform-firecracker` used to compile the lock out altogether — it was a
///   single-vCPU target (docs/reference/firecracker/README.md §3.8).
/// * **Not acquiring while multi-core interleaves bytes.** `with_irqs_disabled`
///   masks IRQs on the calling core only, so two cores byte-interleave at the
///   shared PL011 data register (docs/archive/UART_SMP_INTERLEAVE_FIX.md).
///
/// Neither condition is known at build time — `vcpu_count`/`SMP=N` is a runtime
/// choice — so the decision is made here instead. Set once, by the first secondary
/// to come online; never cleared, because a core that has run could have output
/// pending anywhere.
#[cfg(kernel_console_lock)]
static MULTICORE: AtomicBool = AtomicBool::new(false);

/// Start serializing console output across cores. Called by each secondary as it
/// comes online, before it can print.
///
/// A single line can still interleave at the instant this flips: the BSP may
/// already be inside `emit()`'s byte loop without the lock. That is one line at
/// bringup, against the alternative of never locking at all.
///
/// `allow(dead_code)`: the only caller is `smp_shared::secondary_entry_shared`,
/// which is compiled out on single-core targets (the extreme profile).
#[cfg(kernel_console_lock)]
#[allow(dead_code)]
pub fn set_multicore() {
    MULTICORE.store(true, Ordering::Release);
}

/// No-op when the lock is compiled out (size/extreme profiles). See above for
/// why this can be uncalled.
#[cfg(not(kernel_console_lock))]
#[allow(dead_code)]
pub fn set_multicore() {}

// ============================================================================
// Public API - Safe wrappers around UART operations
// ============================================================================

/// Single console output chokepoint. IRQs are disabled across the UART path so a
/// timer preemption can't interleave two threads' output mid-message. Under
/// `kernel_console_lock` (opt-in), a cross-core spinlock additionally
/// serializes the whole byte sequence so two cores can't interleave at the
/// shared PL011 register.
#[inline]
fn emit(bytes: &[u8]) {
    crate::irq::with_irqs_disabled(|| {
        #[cfg(kernel_console_lock)]
        if MULTICORE.load(Ordering::Acquire) {
            let me = akuma_exec::bkl::current_core_id() as u8 + 1;
            if CONSOLE_OWNER.load(Ordering::Relaxed) == me {
                // Reentrant fast path: panic / sync-exception inside an
                // `emit()` this core already owns. Write directly, do not
                // re-acquire (Spinlock is not reentrant).
                for &b in bytes {
                    akuma_uart::write_byte(b);
                }
                return;
            }
            let _g = CONSOLE_LOCK.lock();
            CONSOLE_OWNER.store(me, Ordering::Relaxed);
            for &b in bytes {
                akuma_uart::write_byte(b);
            }
            CONSOLE_OWNER.store(0, Ordering::Relaxed);
            drop(_g);
            return;
        }
        for &b in bytes {
            akuma_uart::write_byte(b);
        }
    });
}

/// Print a string to the console.
/// Disables IRQs to prevent timer preemption from interleaving output
/// of two threads mid-message.
pub fn print(s: &str) {
    emit(s.as_bytes());
}

/// Print a single character
pub fn print_char(c: char) {
    emit(&[c as u8]);
}

/// Print a number in hexadecimal (no heap allocation)
pub fn print_hex(n: u64) {
    const HEX_CHARS: &[u8] = b"0123456789abcdef";
    let mut buf = [0u8; 16];
    let mut i = 16;
    let mut val = n;

    if val == 0 {
        emit(b"0");
        return;
    }

    while val > 0 && i > 0 {
        i -= 1;
        buf[i] = HEX_CHARS[(val & 0xf) as usize];
        val >>= 4;
    }

    emit(&buf[i..]);
}

/// Print a number in decimal (no heap allocation).
///
/// `usize` is 64-bit on every target this kernel builds for, so this is
/// [`print_u64`] with a widening cast rather than a second digit loop.
#[inline]
pub fn print_dec(n: usize) {
    print_u64(n as u64);
}

/// Print a u64 in decimal (no heap allocation)
pub fn print_u64(n: u64) {
    let mut buf = [0u8; 20];
    let mut i = 20;
    let mut val = n;

    if val == 0 {
        emit(b"0");
        return;
    }

    while val > 0 && i > 0 {
        i -= 1;
        buf[i] = b'0' + (val % 10) as u8;
        val /= 10;
    }

    emit(&buf[i..]);
}

// ============================================================================
// Stack-based formatting (no heap allocation, panic-safe)
// ============================================================================

/// A stack-allocated buffer for formatting without heap allocation.
///
/// Re-exported from `akuma_primitives::console`, which now owns the tree's one
/// copy — this crate's was one of five, and its `safe_print!` one of three (see
/// that module's header). Kept as a re-export so `console::StackWriter::<N>` and
/// `tprint!` below resolve unchanged.
///
/// One behavioural note: this version's `flush` called [`print`] directly, while
/// the shared one goes through a registered hook. `rust_start` installs
/// [`print`] as that hook before its own first output, so the two are the same
/// function from the first instruction onward — and an unregistered hook
/// discards rather than panicking, which is strictly safer than what the
/// `akuma-exec` copies did.
pub use akuma_primitives::console::StackWriter;

// `tprint!` moved to `akuma_primitives::console` on 2026-09-01, alongside
// `safe_print!` and the `StackWriter` both use. It stayed here for one reason —
// "the timestamp comes from `crate::timer::uptime_us()`, and a leaf crate has no
// clock". That reason was already stale: `akuma_primitives::clock` has held the
// uptime hook since it was split out, registered by `akuma_exec::runtime` from
// `ExecRuntime::uptime_us` — which `src/main.rs` sets to `timer::uptime_us`, the
// same function. The binary re-exports the macro at its crate root, so
// `crate::tprint!` resolves unchanged.

/// Check if a character is available for reading
#[must_use]
pub fn has_char() -> bool {
    akuma_uart::has_data()
}

/// Read a character (non-blocking, only call if has_char() is true)
#[must_use]
pub fn getchar() -> u8 {
    akuma_uart::read_byte()
}

/// Read a character (blocking)
#[allow(dead_code)]
fn getchar_blocking() -> u8 {
    while !has_char() {}
    akuma_uart::read_byte()
}

#[allow(dead_code)]
const BUFFER_SIZE: usize = 100;

#[allow(dead_code)]
pub fn read_line(buffer: &mut Vec<u8>, with_echo: bool) -> usize {
    loop {
        if has_char() {
            let c: u8 = getchar();
            buffer.push(c);
            if with_echo {
                print(&(c as char).to_string());
            }
            if c == b'\n' || c == b'\r' {
                return buffer.len();
            }
        }
    }
}

#[allow(dead_code)]
pub fn print_as_akuma(s: &str) {
    print("≽ܫ≼ ... ");
    print(s);
    print("\n");
}
