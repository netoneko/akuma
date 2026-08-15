//! Heap-free console output: one stack writer, one `safe_print!`, one hook.
//!
//! CLAUDE.md § "Kernel conventions" forbids heap allocation on any path that
//! ends at the console — the console is what survives when the allocator is what
//! broke. Every crate therefore formats into a stack buffer, and before this
//! module there were **three** copies of that macro and **five** of the writer
//! under it:
//!
//! | copy | sink |
//! |---|---|
//! | `src/console.rs` `safe_print!` + `StackWriter<N>` | `console::print` |
//! | `akuma-exec` `threading::safe_print!` + `threading::StackWriter<N>` | `runtime().print_str` |
//! | `akuma-virtio` `print::vprint!` | `runtime().print_str`, guarded |
//! | `akuma-exec` `process::FmtBuf` | caller's buffer |
//! | `akuma-exec` `process::children::LazyDebugWriter<N>` | `runtime().print_str` |
//! | `akuma-exec` `mmu::as_trace::Buf` (function-local) | `runtime().print_str` |
//!
//! All three macros were the same six lines. `akuma-virtio/src/print.rs` said so
//! in its own header — "a library crate cannot reach that macro" — which is the
//! missing-crate diagnosis of
//! `docs/archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §5.55 stated by the
//! duplicate itself.
//!
//! # The sink is a hook, and it degrades
//!
//! A leaf crate cannot reach the kernel's UART, so the sink is a boot-registered
//! `fn(&str)`. `akuma_exec::runtime::register` installs it from the same
//! `ExecRuntime::print_str` that `runtime()` hands out, so output lights up at
//! exactly the moment it did before.
//!
//! Unregistered, [`print_str`] is a **no-op** rather than a panic. That is what
//! keeps host unit tests and pre-`register()` callers quiet, and it is a change
//! from `akuma-exec`'s writers, which called `(runtime().print_str)(…)` and so
//! panicked if they ever ran early. `akuma-virtio` already guarded with
//! `is_registered()`; this makes that guard the rule instead of one crate's
//! local caution.

use crate::OnceCopy;

/// The kernel's console sink, installed once at boot.
static PRINT_HOOK: OnceCopy<fn(&str)> = OnceCopy::new();

/// Install the console sink. Called once from `akuma_exec::runtime::register`.
///
/// Idempotent by `OnceCopy`'s contract: a second call is ignored.
pub fn set_print_hook(f: fn(&str)) {
    PRINT_HOOK.set(f);
}

/// Whether a console sink has been registered. Non-panicking probe for callers
/// that want to skip formatting work entirely when output would be discarded.
#[must_use]
pub fn is_print_registered() -> bool {
    PRINT_HOOK.is_set()
}

/// Write an already-formed `&str` to the kernel console.
///
/// No-op before [`set_print_hook`] (host tests, early boot) — never panics, so
/// it is safe on any path including IRQ context and panic handlers.
pub fn print_str(s: &str) {
    if let Some(f) = PRINT_HOOK.get() {
        f(s);
    }
}

/// Fixed-size stack buffer implementing [`core::fmt::Write`], for formatting
/// without touching the heap.
///
/// Truncates silently rather than erroring once `N` bytes are used: a
/// diagnostic that loses its tail is better than one that returns `Err` on a
/// path where nobody checks.
pub struct StackWriter<const N: usize> {
    buf: [u8; N],
    pos: usize,
}

impl<const N: usize> StackWriter<N> {
    #[must_use]
    pub const fn new() -> Self {
        Self { buf: [0; N], pos: 0 }
    }

    /// The bytes written so far, or `""` if they are not valid UTF-8 (which
    /// truncation mid-codepoint can produce).
    #[must_use]
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.pos]).unwrap_or("")
    }

    /// Bytes written so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pos
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pos == 0
    }

    /// Print the contents to the console and reset the buffer.
    pub fn flush(&mut self) {
        print_str(self.as_str());
        self.pos = 0;
    }
}

impl<const N: usize> Default for StackWriter<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> core::fmt::Write for StackWriter<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let n = bytes.len().min(N - self.pos);
        self.buf[self.pos..self.pos + n].copy_from_slice(&bytes[..n]);
        self.pos += n;
        // Truncation is deliberately not an error — see the type doc.
        Ok(())
    }
}

/// [`core::fmt::Write`] over a **caller-owned** buffer.
///
/// For the cases where the formatted text is inspected or reused rather than
/// printed straight out — the `[PSTATS]` top-N line builds two of these side by
/// side.
///
/// Same semantics as [`StackWriter`] — silent truncation, no allocation — but
/// the buffer and cursor live in the caller so several writers can share one
/// stack frame's worth of space. Fields are public because every call site
/// constructs it as a struct literal.
pub struct FmtBuf<'a> {
    pub buf: &'a mut [u8],
    pub pos: &'a mut usize,
}

impl core::fmt::Write for FmtBuf<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let n = bytes.len().min(self.buf.len() - *self.pos);
        self.buf[*self.pos..*self.pos + n].copy_from_slice(&bytes[..n]);
        *self.pos += n;
        Ok(())
    }
}

/// Format pre-built [`core::fmt::Arguments`] into an `N`-byte stack buffer and
/// print it.
///
/// The function-shaped counterpart to [`safe_print!`], for helpers that take
/// `Arguments` instead of being macros themselves (`akuma-exec`'s `as_trace`).
pub fn print_args<const N: usize>(args: core::fmt::Arguments) {
    let mut w = StackWriter::<N>::new();
    let _ = core::fmt::write(&mut w, args);
    w.flush();
}

/// Format into a `$size`-byte stack buffer and print it. Cannot fail from
/// allocation, because it never allocates.
///
/// ```ignore
/// safe_print!(64, "[Thread0] loop={} | zombies={}\n", counter, zombies);
/// ```
///
/// Truncates at `$size` rather than allocating. Silently discards output before
/// the console hook is registered.
#[macro_export]
macro_rules! safe_print {
    ($size:expr, $($arg:tt)*) => {{
        use ::core::fmt::Write as _;
        let mut writer = $crate::console::StackWriter::<$size>::new();
        let _ = write!(writer, $($arg)*);
        writer.flush();
    }};
}

#[cfg(test)]
mod tests {
    use super::{FmtBuf, StackWriter};
    use core::fmt::Write;

    #[test]
    fn writes_and_reads_back() {
        let mut w = StackWriter::<32>::new();
        write!(w, "pid={} va={:#x}", 7, 0x1000).unwrap();
        assert_eq!(w.as_str(), "pid=7 va=0x1000");
        assert_eq!(w.len(), 15);
    }

    #[test]
    fn truncates_instead_of_erroring() {
        let mut w = StackWriter::<8>::new();
        // Deliberately overlong: the contract is silent truncation, because
        // callers use `let _ = write!(...)` and would never see an Err.
        write!(w, "0123456789abcdef").unwrap();
        assert_eq!(w.as_str(), "01234567");
        assert_eq!(w.len(), 8);
    }

    #[test]
    fn as_str_is_empty_on_truncated_utf8() {
        let mut w = StackWriter::<2>::new();
        // 'é' is two bytes; one byte of it lands and the buffer fills, leaving
        // an incomplete codepoint. `as_str` must not panic.
        write!(w, "a\u{e9}").unwrap();
        assert_eq!(w.len(), 2);
        assert_eq!(w.as_str(), "");
    }

    #[test]
    fn flush_resets_the_cursor() {
        let mut w = StackWriter::<16>::new();
        write!(w, "hello").unwrap();
        w.flush(); // no hook registered in host tests: discards, must not panic
        assert!(w.is_empty());
        assert_eq!(w.as_str(), "");
    }

    #[test]
    fn print_str_without_a_hook_is_a_noop() {
        // The property that lets a leaf crate print: unregistered is quiet, not
        // fatal. `akuma-exec`'s writers called `(runtime().print_str)(…)` and
        // would have panicked here.
        super::print_str("discarded\n");
        assert!(!super::is_print_registered());
    }

    #[test]
    fn fmt_buf_shares_a_caller_buffer() {
        let mut buf = [0u8; 16];
        let mut pos = 0usize;
        write!(FmtBuf { buf: &mut buf, pos: &mut pos }, "ab").unwrap();
        write!(FmtBuf { buf: &mut buf, pos: &mut pos }, "cd").unwrap();
        assert_eq!(pos, 4);
        assert_eq!(core::str::from_utf8(&buf[..pos]).unwrap(), "abcd");
    }

    #[test]
    fn fmt_buf_truncates_at_the_caller_buffer_length() {
        let mut buf = [0u8; 4];
        let mut pos = 0usize;
        write!(FmtBuf { buf: &mut buf, pos: &mut pos }, "abcdefgh").unwrap();
        assert_eq!(pos, 4);
        assert_eq!(core::str::from_utf8(&buf[..pos]).unwrap(), "abcd");
    }
}
