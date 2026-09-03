//! 16550 UART on the legacy COM1 port block, via port I/O.
//!
//! This is the amd64 counterpart of `akuma-uart`, and it is a separate
//! implementation rather than a `cfg` arm of that crate on purpose: the two
//! share a register *layout* (the 16550 programming model) but nothing else.
//! AArch64 reaches its UART through an MMIO window the MMU has to have mapped;
//! x86 reaches this one through `in`/`out` on an I/O port, which needs no
//! mapping and works before paging is on. Merging them would mean an
//! abstraction over "how a byte reaches a register", which is the
//! `trait Arch` shape `REDUCING_PLATFORM_DEPENDENCY.md` §7 argues against.
//!
//! No heap, no formatting machinery, no `core::fmt` — same rule as the kernel's
//! `safe_print!`: the console is what survives when everything else is broken,
//! so it may not depend on anything that can fail.

/// COM1. Fixed by the PC architecture since the IBM PC; not discovered.
const COM1: u16 = 0x3F8;

const DATA: u16 = 0;
const INT_ENABLE: u16 = 1;
const FIFO_CTRL: u16 = 2;
const LINE_CTRL: u16 = 3;
const MODEM_CTRL: u16 = 4;
const LINE_STATUS: u16 = 5;

/// Line status bit 5: transmit holding register empty.
const LSR_THR_EMPTY: u8 = 1 << 5;

/// Write a byte to an I/O port.
///
/// # Safety
/// The caller must know what device answers at `port`. Every call in this
/// module targets the COM1 register block, which is architecturally fixed.
#[inline]
unsafe fn outb(port: u16, val: u8) {
    // SAFETY: caller's obligation, discharged above.
    unsafe {
        core::arch::asm!("out dx, al", in("dx") port, in("al") val,
                         options(nomem, nostack, preserves_flags));
    }
}

/// Read a byte from an I/O port.
///
/// # Safety
/// As [`outb`].
#[inline]
unsafe fn inb(port: u16) -> u8 {
    let val: u8;
    // SAFETY: caller's obligation, discharged at each call site.
    unsafe {
        core::arch::asm!("in al, dx", out("al") val, in("dx") port,
                         options(nomem, nostack, preserves_flags));
    }
    val
}

/// Configure COM1 for 115200 8N1 with FIFOs on.
///
/// Divisor 1 against the 115200 Hz base clock. QEMU does not care about the
/// baud rate, but a real machine does and a wrong divisor is invisible under
/// emulation — which is exactly the class of bug that only shows up on
/// hardware, so it is set correctly here rather than left at the default.
pub fn init() {
    // SAFETY: COM1's register block is fixed by the PC architecture.
    unsafe {
        outb(COM1 + INT_ENABLE, 0x00); // no interrupts; this driver polls
        outb(COM1 + LINE_CTRL, 0x80); // DLAB on: the next two ports are the divisor
        outb(COM1 + DATA, 0x01); // divisor low  = 1  -> 115200 baud
        outb(COM1 + INT_ENABLE, 0x00); // divisor high = 0
        outb(COM1 + LINE_CTRL, 0x03); // DLAB off, 8 bits, no parity, 1 stop
        outb(COM1 + FIFO_CTRL, 0xC7); // FIFO on, cleared, 14-byte trigger
        outb(COM1 + MODEM_CTRL, 0x0B); // DTR + RTS + OUT2
    }
}

/// Emit one byte, spinning until the transmit holding register drains.
///
/// Unbounded spin. On real hardware with no cable this blocks forever, which is
/// the wrong trade for a production console and the right one for a bring-up
/// console: a dropped byte during boot is a bug you cannot see, and a stall is
/// a bug you can.
pub fn putb(byte: u8) {
    // SAFETY: COM1's register block is fixed by the PC architecture.
    unsafe {
        while inb(COM1 + LINE_STATUS) & LSR_THR_EMPTY == 0 {
            core::hint::spin_loop();
        }
        outb(COM1 + DATA, byte);
    }
}

/// Emit a string, translating `\n` to CRLF.
pub fn puts(s: &str) {
    for &b in s.as_bytes() {
        if b == b'\n' {
            putb(b'\r');
        }
        putb(b);
    }
}

/// Emit a `u64` as zero-padded 16-digit hex.
///
/// Fixed width rather than trimmed: leading zeros make addresses line up in a
/// boot log, and a variable-width printer needs a branch this does not.
pub fn put_hex(mut val: u64) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 16];
    for slot in out.iter_mut().rev() {
        *slot = DIGITS[(val & 0xF) as usize];
        val >>= 4;
    }
    for &b in &out {
        putb(b);
    }
}

/// Emit a `u64` in decimal.
pub fn put_dec(val: u64) {
    if val == 0 {
        putb(b'0');
        return;
    }
    // 20 digits is the width of u64::MAX; the buffer can never overflow.
    let mut buf = [0u8; 20];
    let mut n = val;
    let mut i = buf.len();
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    for &b in &buf[i..] {
        putb(b);
    }
}
