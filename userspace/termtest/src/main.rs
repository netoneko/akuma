#![no_std]
#![no_main]

extern crate alloc;

use libakuma::{
    fd, print, println,
    set_cursor_position, set_terminal_attributes, get_terminal_attributes,
    clear_screen, hide_cursor, show_cursor, poll_input_event,
    args, fork, getpid, waitpid_status, set_terminal_size, ForkResult,
};
use alloc::format;
use alloc::vec::Vec;

// Mode flags for terminal attributes (mirroring kernel's terminal/mod.rs)
pub mod mode_flags {
    /// Enable raw mode (disable canonical, echo, ISIG)
    pub const RAW_MODE_ENABLE: u64 = 0x01;
    /// Disable raw mode (restore canonical, echo, ISIG)
    pub const RAW_MODE_DISABLE: u64 = 0x02;
}

const DEFAULT_STRESS_CHILDREN: u32 = 6;
const STRESS_ITERS: u32 = 300;
const STRESS_HEARTBEAT_EVERY: u32 = 50;
const STRESS_JOIN_TIMEOUT_MS: u64 = 30_000;

#[no_mangle]
pub extern "C" fn main() {
    let mut args_iter = args();
    let _prog = args_iter.next();

    if matches!(args_iter.next(), Some("--stress")) {
        let child_count: u32 = args_iter
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_STRESS_CHILDREN);
        run_stress(child_count);
        return;
    }

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

/// Reproduces the contention pattern behind
/// `docs/archive/TERM_POLL_INPUT_PREEMPTION_FIX.md`: `fork()`s `child_count`
/// children off this process. `fork()` clones `terminal_state`, `channel`, and
/// `stdin` as shared `Arc`s into the child (unlike `spawn_pty`, which mints a
/// fresh `TerminalState` per session) — so every forked child here races the
/// SAME `Arc<Spinlock<TerminalState>>`/`input_waker` the wedge doc analyzes,
/// without needing a second SSH session or a human typing.
///
/// Each child alternates blocking `poll_input_event` calls (the exact loop
/// that wedged) with a terminal ioctl (`TIOCSWINSZ`/`TCGETS`, which also takes
/// `term_state_lock`), printing a heartbeat every `STRESS_HEARTBEAT_EVERY`
/// iterations. Run this under `SMP>=2` for the cross-core shape the doc
/// describes. A hung run shows as one or more pids that stop printing
/// heartbeats — the parent's own join loop reports exactly which pids never
/// finished within `STRESS_JOIN_TIMEOUT_MS`.
fn run_stress(child_count: u32) {
    println(&format!(
        "termtest --stress: forking {} children sharing this process's TerminalState",
        child_count
    ));

    let mut children: Vec<u32> = Vec::new();
    for _ in 0..child_count {
        match fork() {
            Ok(ForkResult::Child) => {
                stress_child_loop();
                libakuma::exit(0);
            }
            Ok(ForkResult::Parent(pid)) => {
                children.push(pid);
            }
            Err(e) => {
                println(&format!("termtest --stress: fork failed: {}", e));
                libakuma::exit(1);
            }
        }
    }

    println(&format!("termtest --stress: {} children forked, waiting...", children.len()));

    let mut done: Vec<u32> = Vec::new();
    let mut waited_ms: u64 = 0;
    while done.len() < children.len() && waited_ms < STRESS_JOIN_TIMEOUT_MS {
        for &pid in &children {
            if done.contains(&pid) {
                continue;
            }
            if let Some(status) = waitpid_status(pid) {
                println(&format!(
                    "termtest --stress: pid {} exited (code={}, signaled={})",
                    pid, status.exit_code(), status.signaled()
                ));
                done.push(pid);
            }
        }
        if done.len() < children.len() {
            libakuma::sleep_ms(200);
            waited_ms += 200;
        }
    }

    if done.len() == children.len() {
        println("termtest --stress: PASS -- all children finished, no wedge");
        libakuma::exit(0);
    } else {
        let stuck: Vec<u32> = children.iter().copied().filter(|p| !done.contains(p)).collect();
        println(&format!(
            "termtest --stress: FAIL -- {} children never finished within {}ms: {:?}",
            stuck.len(),
            STRESS_JOIN_TIMEOUT_MS,
            stuck
        ));
        libakuma::exit(1);
    }
}

/// One stress child's work loop — see [`run_stress`] for the contention shape.
fn stress_child_loop() {
    let pid = getpid();
    let mut buf = [0u8; 64];
    for i in 0..STRESS_ITERS {
        // A short timeout keeps each iteration bounded even when no input ever
        // arrives, so the loop free-runs to completion when the fix holds —
        // exercises the exact blocking wait loop the wedge doc names.
        let _ = poll_input_event(20, &mut buf);

        // Alternate a second, independently-contended path on the same
        // `term_state_lock`: TIOCSWINSZ (write) vs TCGETS-equivalent (read).
        if i % 3 == 0 {
            let _ = set_terminal_size(fd::STDIN as i32, 80, 24);
        } else {
            let mut flags: u64 = 0;
            let _ = get_terminal_attributes(fd::STDIN, &mut flags as *mut u64 as u64);
        }

        if i % STRESS_HEARTBEAT_EVERY == 0 {
            println(&format!("termtest --stress: pid {} iter {}/{}", pid, i, STRESS_ITERS));
        }
    }
    println(&format!("termtest --stress: pid {} done ({} iters)", pid, STRESS_ITERS));
}