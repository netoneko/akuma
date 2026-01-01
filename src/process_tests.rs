//! Process Execution Tests
//!
//! Tests for user process execution during boot.

use alloc::format;

use crate::console;
use crate::fs;
use crate::process;

/// Run all process tests
pub fn run_all_tests() {
    console::print("\n--- Process Execution Tests ---\n");

    test_echo2();

    console::print("--- Process Execution Tests Done ---\n\n");
}

/// Test the echo2 binary if it exists
fn test_echo2() {
    const ECHO2_PATH: &str = "/bin/echo2";

    // Check if the binary exists
    match fs::read_file(ECHO2_PATH) {
        Ok(data) => {
            console::print(&format!(
                "[Test] Found {} ({} bytes), attempting to execute...\n",
                ECHO2_PATH,
                data.len()
            ));

            // Try to create a process from the ELF
            match process::Process::from_elf("echo2", &data) {
                Ok(proc) => {
                    console::print(&format!(
                        "[Test] Process created: PID={}, entry={:#x}\n",
                        proc.pid, proc.context.pc
                    ));
                    console::print("[Test] echo2 test PASSED (process creation succeeded)\n");

                    // Note: Actually executing the process would require
                    // the full scheduler integration. For now, we just verify
                    // that the ELF can be loaded.
                }
                Err(e) => {
                    console::print(&format!("[Test] Failed to load echo2: {}\n", e));
                    console::print("[Test] echo2 test FAILED\n");
                }
            }
        }
        Err(_) => {
            console::print(&format!(
                "[Test] {} not found, skipping test\n",
                ECHO2_PATH
            ));
            console::print("[Test] To run this test, copy the echo2 binary to /bin/echo2\n");
        }
    }
}
