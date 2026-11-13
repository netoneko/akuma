//! The console the kernel prints on: COM1 once [`crate::Kernel::console`] has wired it (feature
//! `console`), and nothing otherwise. Every boot line, `safe_print!` from the shared crates, and the
//! stdio of a process with no channel of its own (`console_stdio_write`) go through here. The panic
//! path does not: it writes COM1 directly (`raw_uart`), so a kernel that dies says why even without it.

#[cfg(feature = "console")]
use core::sync::atomic::{AtomicBool, Ordering::Relaxed};

#[cfg(feature = "console")]
static ON: AtomicBool = AtomicBool::new(false);

/// Bring up COM1 and route the shared crates' `safe_print!` to it.
#[cfg(feature = "console")]
pub fn init() {
    akuma_comconsole::init();
    ON.store(true, Relaxed);
    akuma_primitives::console::set_print_hook(puts);
}

/// Whether the console is wired.
#[cfg(feature = "console")]
fn on() -> bool {
    ON.load(Relaxed)
}

/// Whether a UART answered at COM1 (a wired console with no UART drops its output).
pub fn present() -> bool {
    #[cfg(feature = "console")]
    if on() {
        return akuma_comconsole::present();
    }
    false
}

pub fn putb(b: u8) {
    #[cfg(feature = "console")]
    if on() {
        akuma_comconsole::putb(b);
    }
    #[cfg(not(feature = "console"))]
    let _ = b;
}

pub fn puts(s: &str) {
    #[cfg(feature = "console")]
    if on() {
        akuma_comconsole::puts(s);
    }
    #[cfg(not(feature = "console"))]
    let _ = s;
}

pub fn put_hex(v: u64) {
    #[cfg(feature = "console")]
    if on() {
        akuma_comconsole::put_hex(v);
    }
    #[cfg(not(feature = "console"))]
    let _ = v;
}

pub fn put_dec(v: u64) {
    #[cfg(feature = "console")]
    if on() {
        akuma_comconsole::put_dec(v);
    }
    #[cfg(not(feature = "console"))]
    let _ = v;
}
