use crate::alloc::string::ToString;
use alloc::vec::Vec;

const UART0_BASE: usize = 0x0900_0000;
const UART0_DR: *mut u8 = UART0_BASE as *mut u8; // Data register (offset 0x00)
const UART0_FR: *const u32 = (UART0_BASE + 0x18) as *const u32; // Flag register (offset 0x18)
const RXFE: u32 = 1 << 4; // Receive FIFO empty flag
const TXFF: u32 = 1 << 5; // Transmit FIFO full flag

unsafe fn putchar(c: u8) {
    // Write directly to UART data register
    unsafe {
        UART0_DR.write_volatile(c);
    }
}

// blocking print
pub fn print(s: &str) {
    for c in s.bytes() {
        unsafe {
            putchar(c);
        }
    }
}

pub fn has_char() -> bool {
    unsafe {
        let flags = UART0_FR.read_volatile();
        (flags & RXFE) == 0 // If RXFE is 0, data is available
    }
}

// blocking read
fn _getchar_blocking() -> u8 {
    unsafe {
        // Wait until data is available
        while !has_char() {}
        // Read the character
        UART0_DR.read_volatile()
    }
}

// non-blocking read (only call if has_char() is true!)
pub fn getchar() -> u8 {
    unsafe {
        UART0_DR.read_volatile()
    }
}

const BUFFER_SIZE: usize = 100;

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

pub fn print_as_akuma(s: &str) {
    print("≽ܫ≼ ... ");
    print(s);
    print("\n")
}
