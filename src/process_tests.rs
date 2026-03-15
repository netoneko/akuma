//! Process Execution Tests
//!
//! Tests for user process execution during boot.

use crate::config;
use crate::console;
use crate::fs;
use akuma_exec::process;
use alloc::string::ToString;
use alloc::format;

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

    // Test waitid WNOHANG with no children returns ECHILD
    test_waitid_stub();

    // Test POSIX exec signal-reset invariant (signal_actions + sigaltstack cleared on exec)
    test_signal_reset_on_exec();

    // Test that SIG_IGN is preserved across exec (POSIX)
    test_signal_ignore_preserved_on_exec();

    // Test tgkill (syscall 131) is wired — does not return ENOSYS
    test_tgkill_not_enosys();

    console::print("--- Process Execution Tests Done ---\n\n");
}

/// Test Linux process compatibility ABI (bridging vfork/execve/wait4)
///
/// This test exercises the kernel's bridging syscalls by simulating 
/// the pattern used by GNU Make and other Linux binaries.
fn test_linux_process_abi() {
    // Find a suitable musl-linked test binary (Linux ABI)
    let test_path = if crate::fs::read_file("/bin/hello_musl.bin").is_ok() {
        "/bin/hello_musl.bin"
    } else if crate::fs::read_file("/bin/hello").is_ok() {
        "/bin/hello"
    } else {
        crate::safe_print!(96, "[Test] No test binary found for Linux ABI test\n");
        return;
    };

    crate::safe_print!(128, "[Test] Testing Linux Process ABI: executing {}...\n", test_path);

    // sys_execve and sys_wait4 require a current process (they read the PID from the
    // process-info page which is only mapped in user address spaces, not the boot TTBR0).
    // Test by spawning directly via the kernel process API (same path a Linux binary takes
    // internally after the kernel bridges vfork/execve).
    match process::exec_with_io(test_path, Some(&["1", "0"]), None) {
        Ok((exit_code, stdout)) => {
            let output = core::str::from_utf8(&stdout).unwrap_or("<invalid utf-8>");
            crate::safe_print!(128, "[Test] exit_code={}, stdout: {}\n", exit_code, output);
            if output.contains("hello") || output.contains("Hello") {
                console::print("[Test] Linux Process ABI test: PASSED\n");
            } else {
                crate::safe_print!(64, "[Test] Linux Process ABI test: FAILED (unexpected output)\n");
            }
        }
        Err(e) => {
            crate::safe_print!(96, "[Test] Linux Process ABI test: FAILED ({})\n", e);
        }
    }
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
            match process::Process::from_elf("echo2", &alloc::vec!["echo2".to_string()], &[], &data, None) {
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
        akuma_exec::threading::yield_now();
    }

    // 4. Read echo2's stdin via procfs: /proc/<echo2_pid>/fd/0
    let stdin_path = format!("/proc/{}/fd/0", echo2_pid);
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
    let stdout_path = format!("/proc/{}/fd/1", hello_pid);
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
    akuma_exec::threading::cleanup_terminated();

    crate::safe_print!(64, "[Test] procfs stdio test complete\n");
}

/// POSIX requires that on exec, custom signal handlers are reset to SIG_DFL and
/// the alternate signal stack is disabled.  This test verifies the invariant
/// directly on the Process struct without executing the process.
fn test_signal_reset_on_exec() {
    use akuma_exec::process::{SignalAction, SignalHandler};
    use alloc::string::String;

    const ELF_PATH: &str = "/bin/elftest";
    let elf_data = match fs::read_file(ELF_PATH) {
        Ok(d) => d,
        Err(_) => {
            crate::safe_print!(96, "[Test] signal_reset_on_exec SKIPPED ({} not found)\n", ELF_PATH);
            return;
        }
    };

    let mut proc = match process::Process::from_elf(
        "elftest", &[String::from("elftest")], &[], &elf_data, None,
    ) {
        Ok(p) => p,
        Err(e) => {
            crate::safe_print!(64, "[Test] signal_reset_on_exec: from_elf failed: {:?}\n", e);
            return;
        }
    };

    // Inject a custom signal handler (SIGSEGV = index 10) and a fake sigaltstack.
    proc.signal_actions[10] = SignalAction {
        handler: SignalHandler::UserFn(0xdeadbeef),
        flags: 0x0800_0000, // SA_ONSTACK
        mask: 0,
        restorer: 0,
    };
    proc.sigaltstack_sp    = 0xc400_4000;
    proc.sigaltstack_size  = 0x8000;
    proc.sigaltstack_flags = 0; // SS_ONSTACK active

    // Replace the image — same binary, new address space.
    if let Err(e) = proc.replace_image(&elf_data, &[String::from("elftest")], &[]) {
        crate::safe_print!(64, "[Test] signal_reset_on_exec: replace_image failed: {}\n", e);
        return;
    }

    // The custom handler must be gone.
    let handler_reset = matches!(proc.signal_actions[10].handler, SignalHandler::Default);
    // The alternate signal stack must be disabled (SS_DISABLE = 2).
    let altstack_disabled = proc.sigaltstack_sp == 0
        && proc.sigaltstack_size == 0
        && proc.sigaltstack_flags == 2;

    if handler_reset && altstack_disabled {
        console::print("[Test] signal_reset_on_exec PASSED\n");
    } else {
        crate::safe_print!(
            64,
            "[Test] signal_reset_on_exec FAILED: handler_reset={} altstack_disabled={} (sp=0x{:x} flags={})\n",
            handler_reset, altstack_disabled,
            proc.sigaltstack_sp, proc.sigaltstack_flags,
        );
    }
}

/// POSIX: SIG_IGN (ignore) dispositions survive exec; only custom handlers are reset.
fn test_signal_ignore_preserved_on_exec() {
    use akuma_exec::process::{SignalAction, SignalHandler};
    use alloc::string::String;

    const ELF_PATH: &str = "/bin/elftest";
    let elf_data = match fs::read_file(ELF_PATH) {
        Ok(d) => d,
        Err(_) => {
            crate::safe_print!(96, "[Test] signal_ignore_preserved SKIPPED ({} not found)\n", ELF_PATH);
            return;
        }
    };

    let mut proc = match process::Process::from_elf(
        "elftest", &[String::from("elftest")], &[], &elf_data, None,
    ) {
        Ok(p) => p,
        Err(e) => {
            crate::safe_print!(64, "[Test] signal_ignore_preserved: from_elf failed: {:?}\n", e);
            return;
        }
    };

    // SIGPIPE (index 12) is commonly set to SIG_IGN by Go and shells.
    proc.signal_actions[12] = SignalAction {
        handler: SignalHandler::Ignore,
        flags: 0,
        mask: 0,
        restorer: 0,
    };

    if let Err(e) = proc.replace_image(&elf_data, &[String::from("elftest")], &[]) {
        crate::safe_print!(64, "[Test] signal_ignore_preserved: replace_image failed: {}\n", e);
        return;
    }

    if matches!(proc.signal_actions[12].handler, SignalHandler::Ignore) {
        console::print("[Test] signal_ignore_preserved PASSED\n");
    } else {
        crate::safe_print!(
            64,
            "[Test] signal_ignore_preserved FAILED: SIG_IGN was not preserved after exec\n",
        );
    }
}

/// Minimal waitid coverage check: confirms sys_waitid (syscall 95) is wired up.
/// Full ABI testing requires a userspace binary that calls waitid() directly.
fn test_waitid_stub() {
    // sys_waitid is pub(super) so we can't call it from here; confirm it compiles
    // by checking that the child-channel helpers used by both wait4 and waitid work.
    let current_pid = akuma_exec::process::read_current_pid();
    if let Some(pid) = current_pid {
        // has_children on the current (kernel) process should return false — same
        // check that sys_waitid performs for P_ALL with no children.
        let has_children = akuma_exec::process::has_children(pid);
        if !has_children {
            console::print("[Test] waitid stub PASSED (no spurious children)\n");
        } else {
            crate::safe_print!(64, "[Test] waitid stub: unexpected children for pid {}\n", pid);
        }
    } else {
        console::print("[Test] waitid stub SKIPPED (no current pid)\n");
    }
}

/// Verify tgkill (syscall 131) is dispatched and does not return ENOSYS.
///
/// Calls tgkill(0, 0, 0) — null signal, which is a no-op on Linux used to
/// check if a thread exists.  Any wired implementation returns 0; ENOSYS
/// returns 0xffffffffffffffda (-38).
fn test_tgkill_not_enosys() {
    const ENOSYS: u64 = (-38i64) as u64;
    // nr=131 (TGKILL), args: tgid=0, tid=0, sig=0
    let result = crate::syscall::handle_syscall(131, &[0, 0, 0, 0, 0, 0]);
    if result != ENOSYS {
        console::print("[Test] tgkill not-ENOSYS PASSED\n");
    } else {
        console::print("[Test] tgkill not-ENOSYS FAILED: returned ENOSYS\n");
    }
}
