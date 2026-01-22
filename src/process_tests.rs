//! Process Execution Tests
//!
//! Tests for user process execution during boot.

use crate::config;
use crate::console;
use crate::fs;
use crate::process;

/// Run all process tests
pub fn run_all_tests() {
    console::print("\n--- Process Execution Tests ---\n");

    // Re-enabled to investigate EC=0x0 crash
    test_echo2();

    // Minimal ELF loading verification (run before stdcheck)
    test_elftest();

    // Test stdcheck with mmap allocator
    test_stdcheck();

    console::print("--- Process Execution Tests Done ---\n\n");
}

/// Test minimal ELF loading with elftest binary
///
/// This is the simplest possible test - if the binary runs and exits with
/// code 42, ELF loading is working correctly.
fn test_elftest() {
    const ELFTEST_PATH: &str = "/bin/elftest";

    match fs::read_file(ELFTEST_PATH) {
        Ok(data) => {
            crate::safe_print!(96, 
                "[Test] Found {} ({} bytes), verifying ELF loading...\n",
                ELFTEST_PATH,
                data.len()
            );

            match process::Process::from_elf("elftest", &data) {
                Ok(mut proc) => {
                    // Execute the process
                    let exit_code = proc.execute();

                    // elftest exits with code 42 on success
                    if exit_code == 42 {
                        console::print("[Test] elftest PASSED (ELF loading verified)\n");
                    } else {
                        crate::safe_print!(96, 
                            "[Test] elftest FAILED: expected exit code 42, got {}\n",
                            exit_code
                        );
                    }
                }
                Err(e) => {
                    crate::safe_print!(64, "[Test] Failed to load elftest: {}\n", e);
                }
            }
        }
        Err(_) => {
            if config::FAIL_TESTS_IF_TEST_BINARY_MISSING {
                crate::safe_print!(64, 
                    "[Test] {} not found - FAIL\n",
                    ELFTEST_PATH
                );
                panic!("Required test binary not found");
            } else {
                crate::safe_print!(96, 
                    "[Test] {} not found, skipping ELF loading test\n",
                    ELFTEST_PATH
                );
            }
        }
    }
}

/// Test the stdcheck binary if it exists (tests mmap allocator)
fn test_stdcheck() {
    const STDCHECK_PATH: &str = "/bin/stdcheck";

    match fs::read_file(STDCHECK_PATH) {
        Ok(data) => {
            crate::safe_print!(128, 
                "[Test] Found {} ({} bytes), executing with mmap allocator...\n",
                STDCHECK_PATH,
                data.len()
            );

            match process::Process::from_elf("stdcheck", &data) {
                Ok(mut proc) => {
                    crate::safe_print!(96, 
                        "[Test] Process created: PID={}, entry={:#x}\n",
                        proc.pid, proc.context.pc
                    );

                    // Actually execute the process
                    let exit_code = proc.execute();

                    if exit_code == 0 {
                        console::print("[Test] stdcheck PASSED\n");
                    } else {
                        crate::safe_print!(64, 
                            "[Test] stdcheck FAILED with exit code {}\n",
                            exit_code
                        );
                    }
                }
                Err(e) => {
                    crate::safe_print!(64, "[Test] Failed to load stdcheck: {}\n", e);
                }
            }
        }
        Err(_) => {
            if config::FAIL_TESTS_IF_TEST_BINARY_MISSING {
                crate::safe_print!(64, 
                    "[Test] {} not found - FAIL\n",
                    STDCHECK_PATH
                );
                panic!("Required test binary not found");
            } else {
                crate::safe_print!(96, 
                    "[Test] {} not found, skipping mmap allocator test\n",
                    STDCHECK_PATH
                );
            }
        }
    }
}

#[allow(dead_code)]
/// Test the echo2 binary if it exists
fn test_echo2() {
    const ECHO2_PATH: &str = "/bin/echo2";

    // Check if the binary exists
    match fs::read_file(ECHO2_PATH) {
        Ok(data) => {
            crate::safe_print!(96, 
                "[Test] Found {} ({} bytes), attempting to execute...\n",
                ECHO2_PATH,
                data.len()
            );

            // Try to create a process from the ELF
            match process::Process::from_elf("echo2", &data) {
                Ok(proc) => {
                    crate::safe_print!(96, 
                        "[Test] Process created: PID={}, entry={:#x}\n",
                        proc.pid, proc.context.pc
                    );
                    console::print("[Test] echo2 test PASSED (process creation succeeded)\n");

                    // Note: Actually executing the process would require
                    // the full scheduler integration. For now, we just verify
                    // that the ELF can be loaded.
                }
                Err(e) => {
                    crate::safe_print!(64, "[Test] Failed to load echo2: {}\n", e);
                    console::print("[Test] echo2 test FAILED\n");
                }
            }
        }
        Err(_) => {
            if config::FAIL_TESTS_IF_TEST_BINARY_MISSING {
                crate::safe_print!(64, "[Test] {} not found - FAIL\n", ECHO2_PATH);
                panic!("Required test binary not found");
            } else {
                crate::safe_print!(64, "[Test] {} not found, skipping test\n", ECHO2_PATH);
            }
        }
    }
}
