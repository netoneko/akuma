//! Process Execution Tests
//!
//! Tests for user process execution during boot.

use crate::config;
use crate::console;
use crate::fs;
use crate::process;
use alloc::string::String;
use alloc::string::ToString;

/// Run all process tests
pub fn run_all_tests() {
    console::print("\n--- Process Execution Tests ---\n");

    // Re-enabled to investigate EC=0x0 crash
    test_echo2();

    // Minimal ELF loading verification (run before stdcheck)
    test_elftest();

    // Test stdcheck with mmap allocator
    test_stdcheck();

    // Test procfs stdin/stdout access
    test_procfs_stdio();

    // Test Linux compatibility bridging (vfork/execve)
    test_linux_process_abi();

    console::print("--- Process Execution Tests Done ---\n\n");
}

/// Test Linux process compatibility ABI (bridging vfork/execve/wait4)
///
/// This test exercises the kernel's bridging syscalls by simulating 
/// the pattern used by GNU Make and other Linux binaries.
fn test_linux_process_abi() {
    let mut test_path = "/bin/hello_musl.bin";
    
    // Check if binary exists
    if crate::fs::read_file(test_path).is_err() {
        test_path = "/bin/hello";
        if crate::fs::read_file(test_path).is_err() {
            crate::safe_print!(96, "[Test] No test binary found for Linux ABI test\n");
            return;
        }
    }

    crate::safe_print!(128, "[Test] Testing Linux Process ABI bridging using {} (vfork -> execve -> wait4)...\n", test_path);

    // Enable validation bypass so we can pass kernel-originated pointers to syscall handlers
    crate::syscall::BYPASS_VALIDATION.store(true, core::sync::atomic::Ordering::Release);

    // Allocate a physical page to simulate user memory
    let test_frame = crate::pmm::alloc_page_zeroed().expect("Failed to alloc test frame");
    let test_user_addr = 0x2000_0000usize; // A safe userspace address
    
    crate::safe_print!(128, "[Test] test_frame PA={:#x}, mapping to VA={:#x}\n", test_frame.addr, test_user_addr);

    unsafe {
        // Map it temporarily in current address space (kernel)
        crate::mmu::map_user_page(test_user_addr, test_frame.addr, crate::mmu::user_flags::RW_NO_EXEC);
        
        // Copy strings to the "user" page
        let p_virt = crate::mmu::phys_to_virt(test_frame.addr) as *mut u8;
        let path_bytes = test_path.as_bytes();
        core::ptr::copy_nonoverlapping(path_bytes.as_ptr(), p_virt, path_bytes.len());
        // Null terminator
        *p_virt.add(path_bytes.len()) = 0;
        
        crate::safe_print!(128, "[Test] Written path to PA {:#x}: {}\n", test_frame.addr, test_path);
        
        let arg1_str = "1"; // 1 output
        let arg1_offset = 64;
        let arg1_bytes = arg1_str.as_bytes();
        core::ptr::copy_nonoverlapping(arg1_bytes.as_ptr(), p_virt.add(arg1_offset), arg1_bytes.len());
        *p_virt.add(arg1_offset + arg1_bytes.len()) = 0;

        // Construct argv array at offset 128
        let argv_offset = 128;
        let argv_ptr = p_virt.add(argv_offset) as *mut u64;
        *argv_ptr = test_user_addr as u64; // arg0 = path
        *argv_ptr.add(1) = (test_user_addr + arg1_offset) as u64; // arg1
        *argv_ptr.add(2) = 0; // null terminator

        // Construct empty envp array at offset 160
        let envp_offset = 160;
        let envp_ptr = p_virt.add(envp_offset) as *mut u64;
        *envp_ptr = 0; // null terminator

        // Ensure writes are visible
        core::arch::asm!("dsb ish", "isb");
    }

    // VERIFY: Can we read it back from the virtual address?
    unsafe {
        let ptr = test_user_addr as *const u8;
        if *ptr == 0 {
            crate::safe_print!(128, "[Test] ERROR: Virtual address {:#x} reads as ZERO even after mapping!\n", test_user_addr);
            // Try to force it?
        } else {
            crate::safe_print!(64, "[Test] Virtual address {:#x} verification OK\n", test_user_addr);
        }
    }

    // 1. Simulate vfork via sys_clone (syscall 220)
    let clone_args = [0x4111, 0, 0, 0, 0, 0];
    let child_pid = crate::syscall::handle_syscall(crate::syscall::nr::CLONE, &clone_args);
    
    if child_pid != 0x7FFFFFFF {
        crate::safe_print!(128, "[Test] Linux ABI FAILED: CLONE (vfork) expected 0x7FFFFFFF, got {:#x}\n", child_pid);
        crate::syscall::BYPASS_VALIDATION.store(false, core::sync::atomic::Ordering::Release);
        return;
    }
    crate::safe_print!(64, "[Test] vfork bridging: SUCCESS\n");

    // 2. Simulate execve via sys_execve (syscall 221)
    let path_ptr = test_user_addr as u64;
    let argv_ptr = (test_user_addr + 128) as u64;
    let envp_ptr = (test_user_addr + 160) as u64;

    let exec_args = [path_ptr, argv_ptr, envp_ptr, 0, 0, 0];
    let exec_res = crate::syscall::handle_syscall(crate::syscall::nr::EXECVE, &exec_args);
    
    if (exec_res as i64) < 0 {
        crate::safe_print!(128, "[Test] Linux ABI FAILED: EXECVE returned error {}\n", exec_res as i64);
        crate::syscall::BYPASS_VALIDATION.store(false, core::sync::atomic::Ordering::Release);
        return;
    }
    crate::safe_print!(64, "[Test] execve bridging: SUCCESS\n");

    // Find the real PID of the spawned process
    let mut real_pid = 0;
    let name_pattern = if test_path.contains("musl") { "musl" } else { "hello" };
    for _ in 0..10 {
        for p in crate::process::list_processes() {
            if p.name.contains(name_pattern) && p.pid > 1 {
                real_pid = p.pid;
                break;
            }
        }
        if real_pid != 0 { break; }
        crate::threading::yield_now();
    }

    if real_pid == 0 {
        crate::safe_print!(64, "[Test] Linux ABI FAILED: Could not find spawned process\n");
        crate::syscall::BYPASS_VALIDATION.store(false, core::sync::atomic::Ordering::Release);
        return;
    }
    crate::safe_print!(96, "[Test] Found real PID: {}\n", real_pid);

    // 3. Simulate wait4 (syscall 260) - waiting for the REAL pid
    let wait_args = [real_pid as u64, 0, 0, 0, 0, 0];
    let wait_res = crate::syscall::handle_syscall(crate::syscall::nr::WAIT4, &wait_args);
    
    crate::safe_print!(96, "[Test] wait4 bridging returned PID: {:#x}\n", wait_res);

    // Disable bypass
    crate::syscall::BYPASS_VALIDATION.store(false, core::sync::atomic::Ordering::Release);

    // Verify stdout
    if let Some(ch) = crate::process::get_child_channel(real_pid) {
        let mut buf = [0u8; 256];
        let n = ch.read(&mut buf);
        if n > 0 {
            if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                crate::safe_print!(128, "[Test] Actual stdout: {}\n", s);
                if s.contains("hello") || s.contains("Hello") {
                    crate::safe_print!(64, "[Test] stdout verification: PASSED\n");
                } else {
                    crate::safe_print!(64, "[Test] stdout verification: FAILED (missing 'hello')\n");
                }
            }
        } else {
            crate::safe_print!(64, "[Test] stdout verification: FAILED (no output)\n");
        }
    }
    
    console::print("[Test] Linux Process ABI test: COMPLETED\n");
}

/// Test minimal ELF loading with elftest binary
///
/// This is the simplest possible test - if the binary runs and exits with
/// code 42, ELF loading is working correctly.
fn test_elftest() {
    const ELFTEST_PATH: &str = "/bin/elftest";

    // Check if file exists first
    if fs::read_file(ELFTEST_PATH).is_err() {
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
            return;
        }
    }

    crate::safe_print!(96, "[Test] Executing {}...\n", ELFTEST_PATH);
    
    match process::exec_with_io(ELFTEST_PATH, None, None) {
        Ok((exit_code, _stdout)) => {
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
            crate::safe_print!(64, "[Test] Failed to execute elftest: {}\n", e);
        }
    }
}

/// Test the stdcheck binary if it exists (tests mmap allocator)
fn test_stdcheck() {
    const STDCHECK_PATH: &str = "/bin/stdcheck";

    // Check if file exists first
    if fs::read_file(STDCHECK_PATH).is_err() {
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
            return;
        }
    }

    crate::safe_print!(128, "[Test] Executing {} with mmap allocator...\n", STDCHECK_PATH);

    match process::exec_with_io(STDCHECK_PATH, None, None) {
        Ok((exit_code, _stdout)) => {
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
            crate::safe_print!(64, "[Test] Failed to execute stdcheck: {}\n", e);
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
            match process::Process::from_elf("echo2", &alloc::vec!["echo2".to_string()], &[], &data) {
                Ok(proc) => {
                    crate::safe_print!(96, 
                        "[Test] Process created: PID={}, entry={:#x}\n",
                        proc.pid, proc.context.pc
                    );
                    console::print("[Test] echo2 test PASSED (process creation succeeded)\n");

                    // Note: Actually executing the process would require
                    // the full scheduler integration. For now, we just verify
                    // that the ELF can be loaded.
                    drop(proc);
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

/// Check if a binary exists, respecting FAIL_TESTS_IF_TEST_BINARY_MISSING
fn check_binary_exists(path: &str) -> bool {
    if fs::read_file(path).is_err() {
        if config::FAIL_TESTS_IF_TEST_BINARY_MISSING {
            crate::safe_print!(64, "[Test] {} not found - FAIL\n", path);
            panic!("Required test binary not found");
        } else {
            crate::safe_print!(96, "[Test] {} not found, skipping procfs test\n", path);
            return false;
        }
    }
    true
}

/// Test procfs stdin/stdout access
///
/// This test verifies:
/// 1. /proc/<pid>/fd/0 (stdin) is readable via procfs
/// 2. /proc/<pid>/fd/1 (stdout) is readable via procfs
/// 3. Proper content is returned from process buffers
fn test_procfs_stdio() {
    const HELLO_PATH: &str = "/bin/hello";
    const ECHO2_PATH: &str = "/bin/echo2";

    // Check binaries exist (respect FAIL_TESTS_IF_TEST_BINARY_MISSING)
    if !check_binary_exists(HELLO_PATH) || !check_binary_exists(ECHO2_PATH) {
        return;
    }

    crate::safe_print!(64, "[Test] Testing procfs stdin/stdout access...\n");

    // 1. Spawn hello with "10 50" args (10 outputs, 50ms delay = ~500ms runtime)
    let hello_args = &["10", "50"];
    let (_hello_thread_id, _hello_channel, hello_pid) = match process::spawn_process_with_channel(
        HELLO_PATH,
        Some(hello_args),
        None,
    ) {
        Ok(result) => result,
        Err(e) => {
            crate::safe_print!(96, "[Test] Failed to spawn hello: {}\n", e);
            return;
        }
    };

    // 2. Spawn echo2 with stdin data
    let stdin_data = b"test input for echo2\n";
    let (_echo2_thread_id, _echo2_channel, echo2_pid) = match process::spawn_process_with_channel(
        ECHO2_PATH,
        None,
        Some(stdin_data),
    ) {
        Ok(result) => result,
        Err(e) => {
            crate::safe_print!(96, "[Test] Failed to spawn echo2: {}\n", e);
            return;
        }
    };

    crate::safe_print!(
        96,
        "[Test] Spawned hello (PID {}) and echo2 (PID {})\n",
        hello_pid,
        echo2_pid
    );

    // 3. Wait ~500ms for processes to run (hello takes ~450ms)
    // Use polling with yield since there's no sleep_ms in kernel
    let wait_start = crate::timer::uptime_us();
    let wait_duration_us = 500_000; // 500ms
    while crate::timer::uptime_us() - wait_start < wait_duration_us {
        crate::threading::yield_now();
    }

    // 4. Read echo2's stdin via procfs: /proc/<echo2_pid>/fd/0
    let stdin_path = alloc::format!("/proc/{}/fd/0", echo2_pid);
    match fs::read_file(&stdin_path) {
        Ok(data) => {
            if data == stdin_data {
                crate::safe_print!(64, "[Test] procfs stdin read: PASSED\n");
            } else {
                crate::safe_print!(
                    128,
                    "[Test] procfs stdin MISMATCH: expected {} bytes, got {}\n",
                    stdin_data.len(),
                    data.len()
                );
            }
        }
        Err(e) => {
            crate::safe_print!(96, "[Test] Failed to read {}: {:?}\n", stdin_path, e);
        }
    }

    // 5. Read hello's stdout via procfs: /proc/<hello_pid>/fd/1
    let stdout_path = alloc::format!("/proc/{}/fd/1", hello_pid);
    match fs::read_file(&stdout_path) {
        Ok(data) => {
            // Verify stdout contains expected content
            if let Ok(s) = core::str::from_utf8(&data) {
                if s.contains("hello (10/10)") && s.contains("hello: done") {
                    crate::safe_print!(64, "[Test] procfs stdout read: PASSED\n");
                } else {
                    crate::safe_print!(
                        128,
                        "[Test] procfs stdout missing expected content (got {} bytes)\n",
                        data.len()
                    );
                }
            } else {
                crate::safe_print!(64, "[Test] procfs stdout: invalid UTF-8\n");
            }
        }
        Err(e) => {
            crate::safe_print!(96, "[Test] Failed to read {}: {:?}\n", stdout_path, e);
        }
    }

    // Cleanup: wait for processes to exit
    // Note: we don't have waitpid in this context, but processes should have exited by now
    crate::threading::cleanup_terminated();

    crate::safe_print!(64, "[Test] procfs stdio test complete\n");
}
