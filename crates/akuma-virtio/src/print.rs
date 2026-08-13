//! Console output for the virtio drivers, under the kernel's no-alloc rule.
//!
//! The drivers in this crate used to live in the kernel bin crate and print via
//! its `safe_print!` macro. A library crate cannot reach that macro, and the
//! obvious substitute — `log::info!`, which `akuma-net` uses — is not one: every
//! crate in this tree pins `log` with `max_level_off`, and the kernel registers
//! no logger at all, so `log::` calls compile to nothing. Routing driver output
//! through `log` would have silently deleted every `[Block] Capacity: …` and
//! `[RNG] Found virtio-rng at slot …` line from the boot log.
//!
//! So this reproduces `safe_print!`'s contract against `akuma-exec`'s registered
//! `print_str` hook: format into a caller-sized **stack** buffer, never the heap.
//! CLAUDE.md § "Kernel conventions" is explicit that no path ending at the
//! console may allocate — the console is what survives when the allocator is
//! what broke.

/// Format into a stack buffer of `$n` bytes and write it to the kernel console.
///
/// Truncates rather than allocating if the output exceeds `$n`. Silently does
/// nothing when no runtime is registered, so host unit tests and early-boot
/// callers stay quiet instead of panicking — the same guard
/// `akuma_exec::sync`'s BKL diagnostics use.
#[macro_export]
macro_rules! vprint {
    ($n:expr, $($arg:tt)*) => {{
        let mut buf = [0u8; $n];
        let mut pos = 0usize;
        let _ = ::core::fmt::write(
            &mut $crate::FmtBuf { buf: &mut buf, pos: &mut pos },
            ::core::format_args!($($arg)*),
        );
        if pos > 0 {
            if let Ok(s) = ::core::str::from_utf8(&buf[..pos]) {
                $crate::print::print_str(s);
            }
        }
    }};
}

/// Write an already-formed `&str` to the kernel console.
///
/// No-op when no runtime is registered (host tests).
pub fn print_str(s: &str) {
    if akuma_exec::runtime::is_registered() {
        (akuma_exec::runtime::runtime().print_str)(s);
    }
}
