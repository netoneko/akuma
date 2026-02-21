#![no_std]
#![no_main]

extern crate alloc;

use libakuma::{
    fd, print, println,
    set_cursor_position, set_terminal_attributes, get_terminal_attributes,
    clear_screen, hide_cursor, show_cursor, poll_input_event,
};
use alloc::format;

// Mode flags for terminal attributes (mirroring kernel's terminal/mod.rs)
pub mod mode_flags {
    /// Enable raw mode (disable canonical, echo, ISIG)
    pub const RAW_MODE_ENABLE: u64 = 0x01;
    /// Disable raw mode (restore canonical, echo, ISIG)
    pub const RAW_MODE_DISABLE: u64 = 0x02;
}

#[no_mangle]
pub extern "C" fn main() {
    println("Terminal Test Program Started");

    // --- 1. Get and store initial terminal attributes ---
    let mut initial_mode_flags: u64 = 0;
    let res = get_terminal_attributes(
        fd::STDIN,
        &mut initial_mode_flags as *mut u64 as u64,
    );
    if res < 0 {
        println(&format!("Error getting initial terminal attributes: {}", res));
        libakuma::exit(1);
    }
    println(&format!(
        "Initial terminal mode flags: {:#x}",
        initial_mode_flags
    ));

    // --- 2. Set raw mode ---
    let res = set_terminal_attributes(fd::STDIN, 0, mode_flags::RAW_MODE_ENABLE);
    if res < 0 {
        println(&format!("Error setting raw mode: {}", res));
        libakuma::exit(1);
    }
    println("Raw mode enabled.");

    // --- 3. Clear screen ---
    let res = clear_screen();
    if res < 0 {
        println(&format!("Error clearing screen: {}", res));
        libakuma::exit(1);
    }
    // Note: "Screen cleared." might be cleared itself if it was printed before clear_screen

    // --- 4. Hide cursor ---
    let res = hide_cursor();
    if res < 0 {
        println(&format!("Error hiding cursor: {}", res));
        libakuma::exit(1);
    }

    // --- 5. Set cursor position and print text ---
    set_cursor_position(0, 0); // Top-left
    println("Hello from Akuma Terminal Test!");

    set_cursor_position(0, 2); // Row 3
    println("Try typing something. Input will be echoed below:");

    // --- 6. Poll for input (non-blocking) ---
    let mut input_buf = [0u8; 64];
    set_cursor_position(0, 4); // Row 5
    println("(Non-blocking poll, type something if you want)");

    libakuma::sleep_ms(100); // Give user a moment to react

    let bytes_read = poll_input_event(0, &mut input_buf); // timeout_ms = 0 for non-blocking
    if bytes_read < 0 {
        println(&format!("Non-blocking poll error: {}", bytes_read));
    } else if bytes_read > 0 {
        print("Non-blocking read: ");
        libakuma::write(fd::STDOUT, &input_buf[..bytes_read as usize]);
        println("");
    } else {
        println("Non-blocking poll: No input received.");
    }
    
    // --- 7. Poll for input (blocking) ---
    set_cursor_position(0, 6); // Row 7
    println("Blocking poll: Waiting for input (type a few characters and press enter or Ctrl+D)...");

    let bytes_read_blocking = poll_input_event(core::u64::MAX, &mut input_buf); // u64::MAX for blocking
    if bytes_read_blocking < 0 {
        println(&format!("Blocking poll error: {}", bytes_read_blocking));
    } else if bytes_read_blocking > 0 {
        print("Blocking read: ");
        libakuma::write(fd::STDOUT, &input_buf[..bytes_read_blocking as usize]);
        println("");
    } else {
        println("Blocking poll: No input received.");
    }

    // --- 8. Show cursor ---
    libakuma::sleep_ms(1000); // Wait a bit
    let res = show_cursor();
    if res < 0 {
        println(&format!("Error showing cursor: {}", res));
        libakuma::exit(1);
    }
    println("Cursor shown.");

    // --- 9. Restore original terminal attributes ---
    let res = set_terminal_attributes(fd::STDIN, 0, initial_mode_flags);
    if res < 0 {
        println(&format!(
            "Error restoring initial terminal attributes: {}",
            res
        ));
        libakuma::exit(1);
    }
    println("Terminal attributes restored.");

    println("Terminal Test Program Finished");
    libakuma::exit(0);
}