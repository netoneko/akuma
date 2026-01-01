use crate::alloc::string::ToString;
use alloc::vec::Vec;

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

/// Global UART instance for QEMU virt machine
static UART: Uart = Uart::new(0x0900_0000);

// ============================================================================
// Public API - Safe wrappers around UART operations
// ============================================================================

/// Print a string to the console
pub fn print(s: &str) {
    for c in s.bytes() {
        UART.write(c);
    }
}

/// Print a single character
pub fn print_char(c: char) {
    UART.write(c as u8);
}

/// Print a number in hexadecimal (no heap allocation)
pub fn print_hex(n: u64) {
    const HEX_CHARS: &[u8] = b"0123456789abcdef";
    let mut buf = [0u8; 16];
    let mut i = 16;
    let mut val = n;
    
    if val == 0 {
        UART.write(b'0');
        return;
    }
    
    while val > 0 && i > 0 {
        i -= 1;
        buf[i] = HEX_CHARS[(val & 0xf) as usize];
        val >>= 4;
    }
    
    for c in &buf[i..] {
        UART.write(*c);
    }
}

/// Print a number in decimal (no heap allocation)
pub fn print_dec(n: usize) {
    let mut buf = [0u8; 20];
    let mut i = 20;
    let mut val = n;
    
    if val == 0 {
        UART.write(b'0');
        return;
    }
    
    while val > 0 && i > 0 {
        i -= 1;
        buf[i] = b'0' + (val % 10) as u8;
        val /= 10;
    }
    
    for c in &buf[i..] {
        UART.write(*c);
    }
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
    print("\n")
}
