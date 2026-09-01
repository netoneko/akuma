//! `log` crate sink that routes into the kernel console.
//!
//! # Why this exists
//!
//! `akuma-net` (and smoltcp inside it) reports progress through `log::info!` and
//! friends. Until 2026-08-21 nothing in this tree ever called `log::set_logger`,
//! so every one of those statements went nowhere — which meant
//! `smoltcp_net::init` was **completely silent**, and a hang inside it was
//! invisible between `main.rs`'s two `safe_print!` bookends. That is exactly the
//! interval that hangs under Firecracker
//! (`docs/archive/AKUMA_FIRECRACKER_KVM.md` §5.1).
//!
//! # Heap-free, per the console rule
//!
//! `CLAUDE.md` forbids heap allocation on any path ending at the console: the
//! console is what survives when the allocator is what broke. `log::Record`
//! hands us a `core::fmt::Arguments`, which would normally want a `String`, so
//! this sink formats into a fixed-size [`StackWriter`] instead and truncates
//! rather than growing. 256 bytes covers every message in the tree; a longer one
//! is silently cut, which is the same contract `safe_print!` has.
//!
//! # Filtering
//!
//! The runtime level is set to `Info`, so `debug!`/`trace!` are dropped at the
//! sink. Note that `log`'s *compile-time* `max_level_*` features win over this
//! entirely: a crate built with `max_level_off` has its macros elided and cannot
//! be re-enabled from here. `akuma-net` carried that feature, which is why
//! registering a logger alone was not enough.

use akuma_primitives::console::StackWriter;
use core::fmt::Write as _;
use log::{Level, LevelFilter, Metadata, Record};

/// Bytes available to format one log record. Truncates beyond this.
const LINE_CAP: usize = 256;

struct KernelLogger;

impl log::Log for KernelLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= Level::Info
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // Fixed stack buffer: no allocation on a console path.
        let mut w = StackWriter::<LINE_CAP>::new();
        // `write!` on StackWriter truncates instead of failing, so the result is
        // deliberately ignored.
        let _ = writeln!(w, "[{}] {}", short_level(record.level()), record.args());
        w.flush();
    }

    fn flush(&self) {}
}

/// One-character level tag, to keep lines short on a serial console.
const fn short_level(level: Level) -> &'static str {
    match level {
        Level::Error => "E",
        Level::Warn => "W",
        Level::Info => "I",
        Level::Debug => "D",
        Level::Trace => "T",
    }
}

static LOGGER: KernelLogger = KernelLogger;

/// Route the `log` facade into the kernel console.
///
/// Call once, as early as the console works. Idempotent: a second call is a
/// no-op, because `log::set_logger` refuses to replace an installed logger and
/// the error is deliberately discarded.
pub fn init() {
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(LevelFilter::Info);
    }
}
