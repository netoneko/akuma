//! The 16550 COM-port console, minimal: configure, probe, emit.
//!
//! This is the machine half of `amd64/src/serial.rs`, extracted (2026-10-24,
//! the Taoxichuan stage-1 composition) so a composed kernel can pull a console
//! as a crate rather than inheriting the full module. What was kept is exactly
//! what works before anything else exists:
//!
//! * [`init`] — the scratch-register probe and the 115200 8N1 configuration;
//! * [`puts`]/[`putb`]/[`put_hex`]/[`put_dec`] — emission, with the same
//!   best-effort bounded lock and the same CRLF translation;
//! * [`present`]/[`getb`]/[`has_byte`] — the polled receive side.
//!
//! What was deliberately **left behind** in `amd64/src/serial.rs` is every layer
//! that assumes more machine than "a COM port": the `akuma-dmesg` ring behind
//! `dmesg`/`sys_syslog`, the framebuffer mirror (`multiboot2::mirror_byte`), the
//! splash notetaker, the fatal-mode gate (`begin_fatal`) and the quiet-boot
//! plumbing. A kernel that wants those keeps its own `serial.rs` beside this
//! crate's logic or grows this crate later; a kernel that only needs a banner on
//! a serial line (a KVM guest's console log) pulls this and nothing more.
//!
//! No heap, no formatting machinery, no `core::fmt` — same rule as the kernel's
//! `safe_print!`: the console is what survives when everything else is broken,
//! so it may not depend on anything that can fail.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use akuma_port::{COM1, UartReg};
use core::sync::atomic::{AtomicBool, Ordering};

/// Line status bit 5: transmit holding register empty.
const LSR_THR_EMPTY: u8 = 1 << 5;
/// Data Ready — a byte is waiting in the receive buffer.
const LSR_DATA_READY: u8 = 1 << 0;

/// One writer at a time, per call — the same policy and the same reasons as
/// `amd64/src/serial.rs`'s `LOCK`: serialised per call so a line assembled from
/// `puts`/`put_hex`/`put_dec` stays readable, and **best-effort** on purpose,
/// because a core that cannot get the lock within the budget must still be able
/// to report. The budget is spins, not time: no clock is up yet.
static LOCK: AtomicBool = AtomicBool::new(false);
const LOCK_BUDGET: u32 = 1 << 22;

/// Did [`init`] find a UART? `false` means every read reports no data — the
/// reference bare-metal box has no UART at all, which is why the probe exists.
static PRESENT: AtomicBool = AtomicBool::new(false);

struct Guard;

fn lock() -> Guard {
    let mut spins = 0u32;
    while LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        spins += 1;
        if spins >= LOCK_BUDGET {
            break;
        }
        core::hint::spin_loop();
    }
    Guard
}

impl Drop for Guard {
    fn drop(&mut self) {
        LOCK.store(false, Ordering::Release);
    }
}

/// Configure COM1 for 115200 8N1 with FIFOs on.
///
/// Probe first (write two patterns to the scratch register, read them back):
/// a real chip returns what it was given, an empty bus returns `0xFF` for both.
/// Only a chip that answers gets configured or read from. QEMU/Firecracker/KVM
/// all emulate the scratch register faithfully; the HP bare-metal box has no
/// port at all, and without the probe every read here would hang on a
/// transmit-holding register that never drains.
pub fn init() {
    let present = {
        COM1.write(UartReg::Scratch, 0x5A);
        let a = COM1.read(UartReg::Scratch);
        COM1.write(UartReg::Scratch, 0xA5);
        let b = COM1.read(UartReg::Scratch);
        a == 0x5A && b == 0xA5
    };
    PRESENT.store(present, Ordering::Release);
    if !present {
        return;
    }
    COM1.write(UartReg::IntEnable, 0x00); // no interrupts; this console polls
    COM1.write(UartReg::LineCtrl, 0x80); // DLAB on: the next two ports are the divisor
    COM1.write(UartReg::Data, 0x01); // divisor low  = 1  -> 115200 baud
    COM1.write(UartReg::IntEnable, 0x00); // divisor high = 0
    COM1.write(UartReg::LineCtrl, 0x03); // DLAB off, 8 bits, no parity, 1 stop
    COM1.write(UartReg::FifoCtrl, 0xC7); // FIFO on, cleared, 14-byte trigger
    COM1.write(UartReg::ModemCtrl, 0x0B); // DTR + RTS + OUT2
}

/// Did [`init`] find a UART?
#[must_use]
pub fn present() -> bool {
    PRESENT.load(Ordering::Relaxed)
}

/// The raw emitter: no lock, no present-check short-circuit, just the
/// transmit drain and the write. Every public function holds [`LOCK`] once and
/// calls this per byte — the same shape as `amd64/src/serial.rs`'s
/// `putb_raw`, and for the same reason: the lock is best-effort and re-taking
/// it from inside itself would spin the whole [`LOCK_BUDGET`] per byte.
fn putb_raw(byte: u8) {
    if !PRESENT.load(Ordering::Relaxed) {
        return;
    }
    while COM1.read(UartReg::LineStatus) & LSR_THR_EMPTY == 0 {
        core::hint::spin_loop();
    }
    COM1.write(UartReg::Data, byte);
}

/// Emit one byte, spinning until the transmit holding register drains.
///
/// Unbounded spin, deliberately: a dropped byte during boot is a bug you cannot
/// see, a stall is a bug you can.
pub fn putb(byte: u8) {
    let _g = lock();
    putb_raw(byte);
}

/// Emit a string, translating `\n` to CRLF.
pub fn puts(s: &str) {
    let _g = lock();
    for &b in s.as_bytes() {
        if b == b'\n' {
            putb_raw(b'\r');
        }
        putb_raw(b);
    }
}

/// Emit a `u64` as zero-padded 16-digit hex.
///
/// Fixed width rather than trimmed: leading zeros make addresses line up in a
/// boot log, and a variable-width printer needs a branch this does not.
pub fn put_hex(val: u64) {
    let _g = lock();
    let mut buf = [0u8; 16];
    let mut v = val;
    for slot in buf.iter_mut().rev() {
        *slot = b"0123456789abcdef"[(v & 0xF) as usize];
        v >>= 4;
    }
    for b in buf {
        putb_raw(b);
    }
}

/// Emit a `u64` in decimal.
pub fn put_dec(val: u64) {
    let _g = lock();
    let mut buf = [0u8; 20];
    let mut v = val;
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    for &b in &buf[i..] {
        putb_raw(b);
    }
}

/// Take a byte from the receive buffer, or `None` if none is waiting.
///
/// Polled, like the transmit side: no device interrupts are taken by anything
/// that pulls this crate. A caller that wants to block spins on this.
#[must_use]
pub fn getb() -> Option<u8> {
    if !PRESENT.load(Ordering::Relaxed) {
        return None; // no chip: no data, not an endless 0xFF
    }
    if COM1.read(UartReg::LineStatus) & LSR_DATA_READY == 0 {
        return None;
    }
    Some(COM1.read(UartReg::Data))
}

/// Is a byte waiting on the receive side? Non-destructive.
#[must_use]
pub fn has_byte() -> bool {
    if !PRESENT.load(Ordering::Relaxed) {
        return false;
    }
    COM1.read(UartReg::LineStatus) & LSR_DATA_READY != 0
}
