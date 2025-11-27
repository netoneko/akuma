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
pub fn getchar() -> u8 {
    unsafe {
        // Wait until data is available
        while !has_char() {}
        // Read the character
        UART0_DR.read_volatile()
    }
}

const BUFFER_SIZE: usize = 100;

pub fn read_line(buffer: &mut [u8; BUFFER_SIZE]) -> usize {
    let mut i: usize = 0;
    while i < BUFFER_SIZE {
        buffer[i] = getchar(); // Blocks until input is available
        if buffer[i] == b'\n' || buffer[i] == b'\r' {
            break;
        }
        i += 1;
    }
    i
}

// pub fn read_line() -> Box<str> {
//     let buffer = &mut [0u8; BUFFER_SIZE];
//     let mut i: usize = 0;
//     let user_input = String::new();
//     while has_char() && i < BUFFER_SIZE {
//         let c: u8 = getchar();
//         if c == b'\n' {
//             break;
//         }
//         buffer[i] = c;
//         i += 1;
//         user_input.push(c as char);
//     }
//     user_input.into_boxed_str().leak()
// }
