use crate::alloc::string::ToString;
use alloc::vec::Vec;
use core::fmt::Write;
#[cfg(kernel_console_lock)]
use core::sync::atomic::{AtomicU8, Ordering};
#[cfg(kernel_console_lock)]
use spinning_top::Spinlock;

// ============================================================================
// UART Driver - Encapsulates all MMIO access
// ============================================================================

/// PL011 UART register offsets
const DR_OFFSET: usize = 0x00; // Data register
const FR_OFFSET: usize = 0x18; // Flag register

/// Flag register bits
const RXFE: u32 = 1 << 4; // Receive FIFO empty flag
#[allow(dead_code)]
const TXFF: u32 = 1 << 5; // Transmit FIFO full flag

/// UART driver that encapsulates all MMIO access
struct Uart {
    base: usize,
}

impl Uart {
    /// Create a new UART driver at the given base address
    const fn new(base: usize) -> Self {
        Self { base }
    }

    /// Write a byte to the UART data register
    #[inline]
    fn write(&self, byte: u8) {
        // SAFETY: Writing to UART data register at known address
        unsafe {
            ((self.base + DR_OFFSET) as *mut u8).write_volatile(byte);
        }
    }

    /// Read a byte from the UART data register
    #[inline]
    fn read(&self) -> u8 {
        // SAFETY: Reading from UART data register at known address
        unsafe { ((self.base + DR_OFFSET) as *mut u8).read_volatile() }
    }

    /// Read the UART flag register
    #[inline]
    fn flags(&self) -> u32 {
        // SAFETY: Reading from UART flag register at known address
        unsafe { ((self.base + FR_OFFSET) as *const u32).read_volatile() }
    }

    /// Check if there is data available to read
    #[inline]
    fn has_data(&self) -> bool {
        (self.flags() & RXFE) == 0
    }
}

/// Global UART instance at remapped VA (physical 0x0900_0000 via L0[1])
static UART: Uart = Uart::new(akuma_exec::mmu::DEV_UART_VA);

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
        {
            let me = akuma_exec::bkl::current_core_id() as u8 + 1;
            if CONSOLE_OWNER.load(Ordering::Relaxed) == me {
                // Reentrant fast path: panic / sync-exception inside an
                // `emit()` this core already owns. Write directly, do not
                // re-acquire (Spinlock is not reentrant).
                for &b in bytes {
                    UART.write(b);
                }
                return;
            }
            let _g = CONSOLE_LOCK.lock();
            CONSOLE_OWNER.store(me, Ordering::Relaxed);
            for &b in bytes {
                UART.write(b);
            }
            CONSOLE_OWNER.store(0, Ordering::Relaxed);
            drop(_g);
        }
        #[cfg(not(kernel_console_lock))]
        for &b in bytes {
            UART.write(b);
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

/// Print a number in decimal (no heap allocation)
pub fn print_dec(n: usize) {
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
/// Use with `core::fmt::Write`.
pub struct StackWriter<const N: usize> {
    buf: [u8; N],
    pos: usize,
}

impl<const N: usize> StackWriter<N> {
    /// Create a new stack writer with the given buffer size
    pub const fn new() -> Self {
        Self {
            buf: [0; N],
            pos: 0,
        }
    }

    /// Get the formatted string (returns empty on invalid UTF-8)
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.pos]).unwrap_or("")
    }

    /// Print the buffer contents to console and clear
    pub fn flush(&mut self) {
        print(self.as_str());
        self.pos = 0;
    }
}

impl<const N: usize> Write for StackWriter<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let remaining = self.buf.len() - self.pos;
        let to_copy = bytes.len().min(remaining);
        self.buf[self.pos..self.pos + to_copy].copy_from_slice(&bytes[..to_copy]);
        self.pos += to_copy;
        // Don't return error on truncation - just truncate silently for safety
        Ok(())
    }
}

/// Safe formatting macro that writes to a stack buffer and prints.
/// Unlike `format!`, this cannot panic from allocation failures.
///
/// Usage:
/// ```
/// safe_print!(64, "[Thread0] loop={} | zombies={}\n", counter, zombies);
/// ```
#[macro_export]
macro_rules! safe_print {
    ($size:expr, $($arg:tt)*) => {{
        use core::fmt::Write;
        let mut writer = $crate::console::StackWriter::<$size>::new();
        let _ = write!(writer, $($arg)*);
        writer.flush();
    }};
}

/// Like safe_print but prepends a `[T<secs>.<cs>]` uptime timestamp.
#[macro_export]
macro_rules! tprint {
    ($size:expr, $($arg:tt)*) => {{
        use core::fmt::Write;
        let __us = $crate::timer::uptime_us();
        let __s = __us / 1_000_000;
        let __cs = (__us % 1_000_000) / 10_000;
        let mut writer = $crate::console::StackWriter::<$size>::new();
        let _ = write!(writer, "[T{}.{:02}] ", __s, __cs);
        let _ = write!(writer, $($arg)*);
        writer.flush();
    }};
}

/// Check if a character is available for reading
pub fn has_char() -> bool {
    UART.has_data()
}

/// Read a character (non-blocking, only call if has_char() is true)
pub fn getchar() -> u8 {
    UART.read()
}

/// Read a character (blocking)
#[allow(dead_code)]
fn getchar_blocking() -> u8 {
    while !has_char() {}
    UART.read()
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
