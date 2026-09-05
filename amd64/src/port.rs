//! x86 port I/O.
//!
//! Extracted from `serial.rs` when a second consumer appeared (masking the
//! legacy 8259 PICs in `lapic.rs`). Two instructions and no policy — the
//! decision about *which* port answers stays with the driver that owns the
//! device.

/// Write a byte to an I/O port.
///
/// # Safety
/// The caller must know what device answers at `port`; a write to the wrong one
/// can reprogram hardware the kernel depends on.
#[inline]
pub unsafe fn outb(port: u16, val: u8) {
    // SAFETY: caller's obligation, discharged at each call site.
    unsafe {
        core::arch::asm!("out dx, al", in("dx") port, in("al") val,
                         options(nomem, nostack, preserves_flags));
    }
}

/// Read a byte from an I/O port.
///
/// # Safety
/// As [`outb`]. Some ports have read side effects.
#[inline]
pub unsafe fn inb(port: u16) -> u8 {
    let val: u8;
    // SAFETY: caller's obligation, discharged at each call site.
    unsafe {
        core::arch::asm!("in al, dx", out("al") val, in("dx") port,
                         options(nomem, nostack, preserves_flags));
    }
    val
}

/// Write a dword to an I/O port. Used for the PCI `0xCF8` CONFIG_ADDRESS /
/// `0xCFC` CONFIG_DATA pair (`pci.rs`).
///
/// # Safety
/// As [`outb`].
#[inline]
pub unsafe fn outl(port: u16, val: u32) {
    // SAFETY: caller's obligation, discharged at each call site.
    unsafe {
        core::arch::asm!("out dx, eax", in("dx") port, in("eax") val,
                         options(nomem, nostack, preserves_flags));
    }
}

/// Read a dword from an I/O port.
///
/// # Safety
/// As [`outb`]. Some ports have read side effects.
#[inline]
pub unsafe fn inl(port: u16) -> u32 {
    let val: u32;
    // SAFETY: caller's obligation, discharged at each call site.
    unsafe {
        core::arch::asm!("in eax, dx", out("eax") val, in("dx") port,
                         options(nomem, nostack, preserves_flags));
    }
    val
}
