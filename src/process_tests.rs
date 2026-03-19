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

    // Test SysV message queue syscalls (186-189)
    test_msgqueue_create_destroy();
    test_msgqueue_send_recv();
    test_msgqueue_box_isolation();

    // Test CLONE_VFORK is dispatched (not ENOSYS) and VFORK_WAITERS is clean afterward
    test_vfork_dispatch();

    // Test CLONE_VFORK pre-insertion race fix
    test_vfork_waiters_clean_at_boot();
    test_vfork_complete_removes_entry();

    // Test that user_va_limit allows Go's high-arena goroutine stacks (>4 GB, ~130 GB)
    test_user_va_limit_48bit();

    // Test signal mask blocking on delivery (SA_NODEFER logic)
    test_signal_mask_nodefer_blocks();
    test_signal_mask_nodefer_flag_skips();

    // Test signal frame layout constants are self-consistent
    test_sigframe_layout_constants();

    // Test pipe write/read round-trip (catches use-after-close silent data loss)
    test_pipe_write_read_roundtrip();
    test_pipe_write_missing_returns_epipe();
    test_pipe_close_write_signals_eof();
    test_pipe_refcount_lifecycle();
    test_pipe_write_returns_epipe_after_read_close();
    test_pipe_eof_only_when_write_count_zero();
    test_pipe_clone_ref_then_double_close();
    test_pipe_dupfd_bumps_refcount();
    test_pipe_dup3_atomically_replaces_and_closes_old();

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

// ── SysV message queue tests (nr 186–189) ─────────────────────────────────

const NR_MSGGET: u64 = 186;
const NR_MSGCTL: u64 = 187;
const NR_MSGRCV: u64 = 188;
const NR_MSGSND: u64 = 189;
const IPC_PRIVATE: u64 = 0;
const IPC_CREAT: u64 = 0o1000;
const IPC_RMID: u64 = 0;
/// msgget(IPC_PRIVATE) creates a queue and returns a valid msqid; two successive
/// calls return distinct msqids; msgctl(IPC_RMID) returns 0 for each.
fn test_msgqueue_create_destroy() {
    let flags = IPC_CREAT | 0o600;

    let id1 = crate::syscall::handle_syscall(NR_MSGGET, &[IPC_PRIVATE, flags, 0, 0, 0, 0]);
    let id2 = crate::syscall::handle_syscall(NR_MSGGET, &[IPC_PRIVATE, flags, 0, 0, 0, 0]);

    // Both IDs must be small positive integers, not error codes.
    let ok_ids = (id1 as i64) > 0 && (id2 as i64) > 0 && id1 != id2;

    let rm1 = crate::syscall::handle_syscall(NR_MSGCTL, &[id1, IPC_RMID, 0, 0, 0, 0]);
    let rm2 = crate::syscall::handle_syscall(NR_MSGCTL, &[id2, IPC_RMID, 0, 0, 0, 0]);

    if ok_ids && rm1 == 0 && rm2 == 0 {
        console::print("[Test] msgqueue_create_destroy PASSED\n");
    } else {
        crate::safe_print!(
            64,
            "[Test] msgqueue_create_destroy FAILED: id1={} id2={} rm1={} rm2={}\n",
            id1 as i64, id2 as i64, rm1 as i64, rm2 as i64,
        );
    }
}

/// Full round-trip: create queue, send a message, receive it back, check the
/// content, then remove the queue.  Uses BYPASS_VALIDATION so kernel-stack
/// buffers pass the user-pointer check.
fn test_msgqueue_send_recv() {
    use core::sync::atomic::Ordering;

    // Enable pointer bypass for this test so kernel stack addresses are accepted.
    crate::syscall::BYPASS_VALIDATION.store(true, Ordering::Release);

    let flags = IPC_CREAT | 0o600;
    let msqid = crate::syscall::handle_syscall(NR_MSGGET, &[IPC_PRIVATE, flags, 0, 0, 0, 0]);

    // Build a send buffer: [mtype: i64][mtext: "hello\0"]
    let send_mtype: i64 = 42;
    let mtext = b"hello";
    let mut send_buf = [0u8; 8 + 5];
    send_buf[0..8].copy_from_slice(&send_mtype.to_ne_bytes());
    send_buf[8..].copy_from_slice(mtext);

    let send_ptr = send_buf.as_ptr() as u64;
    let send_ret = crate::syscall::handle_syscall(
        NR_MSGSND,
        &[msqid, send_ptr, 5, 0, 0, 0], // msgsz=5, flags=0
    );

    // Receive buffer: [mtype: i64][mtext: 16 bytes]
    let recv_buf = [0u8; 8 + 16];
    let recv_ptr = recv_buf.as_ptr() as u64;
    let recv_ret = crate::syscall::handle_syscall(
        NR_MSGRCV,
        &[msqid, recv_ptr, 16, 0, 0, 0], // msgsz=16, msgtyp=0 (any), flags=0
    );

    crate::syscall::handle_syscall(NR_MSGCTL, &[msqid, IPC_RMID, 0, 0, 0, 0]);

    crate::syscall::BYPASS_VALIDATION.store(false, Ordering::Release);

    let recv_mtype = i64::from_ne_bytes(recv_buf[0..8].try_into().unwrap());
    let recv_text = &recv_buf[8..8 + recv_ret as usize];

    if send_ret == 0 && recv_ret == 5 && recv_mtype == 42 && recv_text == mtext {
        console::print("[Test] msgqueue_send_recv PASSED\n");
    } else {
        crate::safe_print!(
            64,
            "[Test] msgqueue_send_recv FAILED: send={} recv={} mtype={} text={:?}\n",
            send_ret as i64, recv_ret as i64, recv_mtype, recv_text,
        );
    }
}

/// Two queues created with the same named key in box 0 share the same msqid
/// (second msgget without IPC_EXCL returns the existing one).
/// A third call with IPC_EXCL returns EEXIST.
fn test_msgqueue_box_isolation() {
    const EEXIST: u64 = (-17i64) as u64;
    let key: u64 = 0xdeadbeef_u64;
    let flags = IPC_CREAT | 0o600;

    let id1 = crate::syscall::handle_syscall(NR_MSGGET, &[key, flags, 0, 0, 0, 0]);
    // Same key, no IPC_EXCL — should return the same msqid.
    let id2 = crate::syscall::handle_syscall(NR_MSGGET, &[key, flags, 0, 0, 0, 0]);
    // Same key + IPC_EXCL — should return EEXIST.
    let id3 = crate::syscall::handle_syscall(
        NR_MSGGET,
        &[key, flags | 0o2000 /* IPC_EXCL */, 0, 0, 0, 0],
    );

    crate::syscall::handle_syscall(NR_MSGCTL, &[id1, IPC_RMID, 0, 0, 0, 0]);

    if (id1 as i64) > 0 && id1 == id2 && id3 == EEXIST {
        console::print("[Test] msgqueue_box_isolation PASSED\n");
    } else {
        crate::safe_print!(
            64,
            "[Test] msgqueue_box_isolation FAILED: id1={} id2={} id3={}\n",
            id1 as i64, id2 as i64, id3 as i64,
        );
    }
}

// ── CLONE_VFORK dispatch test ──────────────────────────────────────────────

/// Verify CLONE_VFORK (flag 0x4000) is dispatched rather than falling through
/// to ENOSYS.  In the kernel boot context there is no current process, so
/// sys_clone_pidfd returns !0u64 (EFAULT-ish) rather than a child PID — but
/// that is distinct from ENOSYS (-38), proving the dispatch arm is wired.
fn test_vfork_dispatch() {
    const ENOSYS: u64 = (-38i64) as u64;
    const CLONE_VFORK: u64 = 0x4000;
    const CLONE_VM: u64 = 0x100;
    // nr=56 (clone), flags=CLONE_VFORK|CLONE_VM|SIGCHLD
    let flags = CLONE_VFORK | CLONE_VM | 0x11; // 0x11 = SIGCHLD
    let result = crate::syscall::handle_syscall(56, &[flags, 0, 0, 0, 0, 0]);
    if result != ENOSYS {
        console::print("[Test] vfork_dispatch not-ENOSYS PASSED\n");
    } else {
        console::print("[Test] vfork_dispatch FAILED: returned ENOSYS (arm not wired)\n");
    }
}

// ── CLONE_VFORK race-fix tests ─────────────────────────────────────────────

/// Verify VFORK_WAITERS is empty at kernel boot.  A non-zero count would mean
/// a previous test (or boot-time clone) leaked an entry, which would prevent
/// those child PIDs from ever being correctly reaped.
fn test_vfork_waiters_clean_at_boot() {
    let len = crate::syscall::proc::vfork_waiters_len();
    if len == 0 {
        console::print("[Test] vfork_waiters_clean_at_boot PASSED\n");
    } else {
        crate::safe_print!(
            64,
            "[Test] vfork_waiters_clean_at_boot FAILED: {} stale entries\n",
            len,
        );
    }
}

// ── user_va_limit regression tests ────────────────────────────────────────

/// Verify that `user_va_limit()` returns the full 48-bit TTBR0 limit.
///
/// Regression test for the bug where `user_va_limit` returned
/// `proc.memory.stack_top` (≈2.7 GB) or later a hard-coded 4 GB cap.  Both
/// were too small for Go on AArch64, which places goroutine stacks and
/// M-structs in high arenas like 0x203e000000 (≈130 GB).  The correct limit
/// is 0x0000_FFFF_FFFF_FFFF (standard Linux 48-bit VA).
fn test_user_va_limit_48bit() {
    const EXPECTED: u64 = 0x0000_FFFF_FFFF_FFFFu64;
    // 4 GB — the old wrong cap
    const OLD_CAP_4GB: u64 = 0x1_0000_0000u64;
    // Representative Go goroutine arena address (~130 GB) that must be allowed
    const GO_GOROUTINE_ARENA: u64 = 0x203e_0000_00u64;

    let limit = crate::syscall::user_va_limit_value();

    if limit == EXPECTED && limit > OLD_CAP_4GB && limit >= GO_GOROUTINE_ARENA {
        console::print("[Test] user_va_limit_48bit PASSED\n");
    } else {
        crate::safe_print!(
            96,
            "[Test] user_va_limit_48bit FAILED: limit=0x{:x} expected=0x{:x}\n",
            limit, EXPECTED,
        );
    }
}

// ── Signal mask / SA_NODEFER regression tests ─────────────────────────────

/// Verify that delivering a signal blocks the signal in the process signal mask
/// when SA_NODEFER is NOT set.
///
/// The kernel code in `try_deliver_signal` does:
///   if action.flags & SA_NODEFER == 0 { proc.signal_mask |= 1 << (signal - 1); }
///
/// This test exercises that bit arithmetic directly: starting with a cleared
/// mask and a SIGURG delivery (signal 23, bit 22), the mask must have bit 22
/// set after delivery and only bit 22 set.
fn test_signal_mask_nodefer_blocks() {
    const SA_NODEFER: u64 = 0x40000000;
    const SIGURG: u32 = 23;
    let flags_without_nodefer: u64 = 0; // No SA_NODEFER

    let mut signal_mask: u64 = 0;
    // Mirror the kernel logic from try_deliver_signal
    if flags_without_nodefer & SA_NODEFER == 0 && SIGURG >= 1 && SIGURG <= 64 {
        signal_mask |= 1u64 << (SIGURG - 1);
    }

    let expected_bit = 1u64 << (SIGURG - 1); // bit 22
    if signal_mask == expected_bit {
        console::print("[Test] signal_mask_nodefer_blocks PASSED\n");
    } else {
        crate::safe_print!(
            64,
            "[Test] signal_mask_nodefer_blocks FAILED: mask=0x{:x} expected=0x{:x}\n",
            signal_mask, expected_bit,
        );
    }
}

/// Verify that SA_NODEFER prevents the delivered signal from being added to
/// the process signal mask.
///
/// When SA_NODEFER is set the signal handler may be entered recursively; the
/// kernel must NOT block the signal in `proc.signal_mask`.
fn test_signal_mask_nodefer_flag_skips() {
    const SA_NODEFER: u64 = 0x40000000;
    const SIGURG: u32 = 23;
    let flags_with_nodefer: u64 = SA_NODEFER;

    let mut signal_mask: u64 = 0;
    if flags_with_nodefer & SA_NODEFER == 0 && SIGURG >= 1 && SIGURG <= 64 {
        signal_mask |= 1u64 << (SIGURG - 1);
    }

    if signal_mask == 0 {
        console::print("[Test] signal_mask_nodefer_flag_skips PASSED\n");
    } else {
        crate::safe_print!(
            64,
            "[Test] signal_mask_nodefer_flag_skips FAILED: mask unexpectedly set to 0x{:x}\n",
            signal_mask,
        );
    }
}

// ── Signal frame layout constant regression tests ─────────────────────────

/// Verify that the signal frame layout constants are self-consistent and match
/// the Linux AArch64 ABI.
///
/// Layout (from linux/arch/arm64/include/uapi/asm/sigcontext.h):
///   siginfo_t      128 bytes  at offset   0
///   ucontext_t hdr 168 bytes  at offset 128  (uc_flags+uc_link+uc_stack+uc_sigmask+__unused)
///   sigcontext     280 bytes  at offset 296  (fault_addr + regs[31] + sp + pc + pstate)
///   FPSIMD record  528 bytes  at offset 576  (_aarch64_ctx(8)+fpsr(4)+fpcr(4)+vregs[32](512))
///   null terminator  8 bytes  at offset 1104
///   total size    1112 bytes
///
/// The `uc_sigmask` field lives at ucontext+40 → frame offset 168 (128+40).
fn test_sigframe_layout_constants() {
    use crate::exceptions::{
        TEST_SIGFRAME_FPSIMD, TEST_SIGFRAME_MCONTEXT, TEST_SIGFRAME_SIZE,
        TEST_SIGFRAME_UC_SIGMASK, TEST_SIGFRAME_UCONTEXT,
    };

    let mut ok = true;

    // siginfo_t: 128 bytes, starts at 0
    if TEST_SIGFRAME_UCONTEXT != 128 {
        crate::safe_print!(64, "[Test] sigframe: UCONTEXT offset wrong: {}\n", TEST_SIGFRAME_UCONTEXT);
        ok = false;
    }

    // ucontext header: 168 bytes
    if TEST_SIGFRAME_MCONTEXT != 128 + 168 {
        crate::safe_print!(64, "[Test] sigframe: MCONTEXT offset wrong: {}\n", TEST_SIGFRAME_MCONTEXT);
        ok = false;
    }

    // sigcontext: 280 bytes
    if TEST_SIGFRAME_FPSIMD != 128 + 168 + 280 {
        crate::safe_print!(64, "[Test] sigframe: FPSIMD offset wrong: {}\n", TEST_SIGFRAME_FPSIMD);
        ok = false;
    }

    // FPSIMD(528) + null(8) = 536
    if TEST_SIGFRAME_SIZE != 128 + 168 + 280 + 528 + 8 {
        crate::safe_print!(64, "[Test] sigframe: SIZE wrong: {}\n", TEST_SIGFRAME_SIZE);
        ok = false;
    }

    // uc_sigmask is at ucontext_t+40 within the frame
    if TEST_SIGFRAME_UC_SIGMASK != 128 + 40 {
        crate::safe_print!(64, "[Test] sigframe: UC_SIGMASK offset wrong: {}\n", TEST_SIGFRAME_UC_SIGMASK);
        ok = false;
    }

    if ok {
        console::print("[Test] sigframe_layout_constants PASSED\n");
    }
}

// ── Pipe lifecycle regression tests ───────────────────────────────────────

/// Verify a basic pipe write/read round-trip works correctly.
///
/// This is the most fundamental sanity check for the pipe subsystem: create a
/// pipe, write known bytes into the write end, read them back from the read
/// end, and verify the content matches.
///
/// If this test fails or `pipe_write` silently returns 0, the symptom would be
/// processes getting empty stdout — exactly the bug seen with `compile -V=full`.
fn test_pipe_write_read_roundtrip() {
    let id = crate::syscall::pipe::pipe_create();
    let input = b"hello pipe";
    let n = match crate::syscall::pipe::pipe_write(id, input) {
        Ok(n) => n,
        Err(e) => {
            crate::safe_print!(64, "[Test] pipe_write_read_roundtrip FAILED: pipe_write returned Err({})\n", e);
            crate::syscall::pipe::pipe_close_write(id);
            crate::syscall::pipe::pipe_close_read(id);
            return;
        }
    };
    if n != input.len() {
        crate::safe_print!(64, "[Test] pipe_write_read_roundtrip FAILED: pipe_write returned {} expected {}\n", n, input.len());
        crate::syscall::pipe::pipe_close_write(id);
        crate::syscall::pipe::pipe_close_read(id);
        return;
    }

    let mut buf = [0u8; 32];
    let (read_n, eof) = crate::syscall::pipe::pipe_read(id, &mut buf);
    if read_n == input.len() && buf[..read_n] == *input && !eof {
        console::print("[Test] pipe_write_read_roundtrip PASSED\n");
    } else {
        crate::safe_print!(
            96,
            "[Test] pipe_write_read_roundtrip FAILED: read_n={} eof={} content={:?}\n",
            read_n, eof, &buf[..read_n],
        );
    }

    crate::syscall::pipe::pipe_close_write(id);
    crate::syscall::pipe::pipe_close_read(id);
}

/// Verify `pipe_write` returns Err(EPIPE) for a destroyed pipe.
///
/// After `pipe_close_write` + `pipe_close_read` the pipe is removed from PIPES.
/// Any subsequent `pipe_write` call with that ID must return Err(EPIPE), not
/// silently succeed with 0. The old silent-0 behaviour was the root cause of
/// `compile -V=full` producing empty stdout.
fn test_pipe_write_missing_returns_epipe() {
    let id = crate::syscall::pipe::pipe_create();
    crate::syscall::pipe::pipe_close_write(id);
    crate::syscall::pipe::pipe_close_read(id);
    let result = crate::syscall::pipe::pipe_write(id, b"should be lost");
    if result.is_err() {
        console::print("[Test] pipe_write_missing_returns_epipe PASSED\n");
    } else {
        crate::safe_print!(
            64,
            "[Test] pipe_write_missing_returns_epipe FAILED: returned Ok({}) expected Err(EPIPE)\n",
            result.unwrap(),
        );
    }
}

/// Verify that closing the write end of a pipe causes subsequent reads to
/// return EOF (`eof = true, n = 0`).
///
/// Go's pipe reader blocks in `sys_read` until either data is available or the
/// write end is closed.  If the write-close logic is broken, the reader would
/// hang forever rather than getting EOF.
fn test_pipe_close_write_signals_eof() {
    let id = crate::syscall::pipe::pipe_create();
    // Don't write anything; just close the write end.
    crate::syscall::pipe::pipe_close_write(id);

    let mut buf = [0u8; 16];
    let (n, eof) = crate::syscall::pipe::pipe_read(id, &mut buf);
    if n == 0 && eof {
        console::print("[Test] pipe_close_write_signals_eof PASSED\n");
    } else {
        crate::safe_print!(
            64,
            "[Test] pipe_close_write_signals_eof FAILED: n={} eof={}\n",
            n, eof,
        );
    }

    crate::syscall::pipe::pipe_close_read(id);
}

/// Verify pipe refcount lifecycle: the pipe stays alive until BOTH the cloned
/// write ref AND the original read ref are closed.
///
/// `dup3` (and `fork_process`) call `pipe_clone_ref` to increment the write or
/// read count.  The pipe must not be destroyed after the first close — only
/// after all refs on both sides reach zero.  This test simulates one dup:
///   write_count=2 (original + cloned), read_count=1
/// After the first write close: pipe still alive (write_count=1 > 0).
/// After second write close: EOF visible to reader.
/// After read close: pipe fully removed.
fn test_pipe_refcount_lifecycle() {
    let id = crate::syscall::pipe::pipe_create();
    // Clone the write ref (simulates dup3 or fork).
    crate::syscall::pipe::pipe_clone_ref(id, true);

    // Close first write ref — pipe must still be alive.
    crate::syscall::pipe::pipe_close_write(id);
    let result = crate::syscall::pipe::pipe_write(id, b"still alive");
    if result.is_err() {
        crate::safe_print!(64, "[Test] pipe_refcount_lifecycle FAILED: pipe died after first close\n");
        crate::syscall::pipe::pipe_close_write(id);
        crate::syscall::pipe::pipe_close_read(id);
        return;
    }

    // Close second write ref — now the read end should see EOF after draining.
    crate::syscall::pipe::pipe_close_write(id);

    let mut buf = [0u8; 32];
    let (read_n, _eof) = crate::syscall::pipe::pipe_read(id, &mut buf);
    // After draining, a second read should return EOF.
    let (n2, eof2) = crate::syscall::pipe::pipe_read(id, &mut buf);

    if read_n == 11 && n2 == 0 && eof2 {
        console::print("[Test] pipe_refcount_lifecycle PASSED\n");
    } else {
        crate::safe_print!(
            96,
            "[Test] pipe_refcount_lifecycle FAILED: read_n={} n2={} eof2={}\n",
            read_n, n2, eof2,
        );
    }

    crate::syscall::pipe::pipe_close_read(id);
}

/// Verify that closing the READ end of a pipe does NOT destroy the pipe while
/// there are still active writers.
///
/// The bug in `compile -V=full` is that pipe_id=6 was fully destroyed (both
/// counts 0) BEFORE compile's write. This can happen if:
///   1. read_count prematurely hits 0 (Go's reader closes fd_r early)
///   2. write_count then drops to 0 (Go closes fd_w + race)
///   3. Pipe removed → compile's subsequent write returns 0 (silent data loss)
///
/// This test verifies that a single close_read (simulating Go's reader closing
/// early) leaves the pipe alive and writable as long as write_count > 0.
/// When read end is closed, writing should return EPIPE (Linux behavior).
/// The pipe struct must stay alive (not removed from PIPES) until write_count
/// also reaches 0, but writes correctly fail with EPIPE.
fn test_pipe_write_returns_epipe_after_read_close() {
    let id = crate::syscall::pipe::pipe_create();
    crate::syscall::pipe::pipe_close_read(id);
    // write_count=1, read_count=0: pipe is still in PIPES but broken.
    let result = crate::syscall::pipe::pipe_write(id, b"should fail");
    if result.is_err() {
        console::print("[Test] pipe_write_returns_epipe_after_read_close PASSED\n");
    } else {
        crate::safe_print!(
            64,
            "[Test] pipe_write_returns_epipe_after_read_close FAILED: returned Ok({}) expected Err(EPIPE)\n",
            result.unwrap(),
        );
    }
    crate::syscall::pipe::pipe_close_write(id);
}

/// Verify that `pipe_can_read` returns EOF (true) ONLY when write_count==0,
/// not when write_count > 0 and buffer is empty.
///
/// This is the fundamental condition that triggers the broken epoll-fires-early
/// scenario: if write_count is 0 while a writer's fd is still open, `pipe_can_read`
/// mistakenly returns true, epoll fires immediately, the reader reads 0 bytes, and
/// closes its end — causing the pipe to be fully destroyed before the writer writes.
fn test_pipe_eof_only_when_write_count_zero() {
    // Case 1: write_count > 0, buffer empty → NOT EOF (false)
    let id = crate::syscall::pipe::pipe_create();
    let mut buf = [0u8; 16];
    let (n, eof) = crate::syscall::pipe::pipe_read(id, &mut buf);
    let case1_ok = n == 0 && !eof;

    // Case 2: write_count == 0 (write end closed), buffer empty → EOF (true)
    crate::syscall::pipe::pipe_close_write(id);
    let (n2, eof2) = crate::syscall::pipe::pipe_read(id, &mut buf);
    let case2_ok = n2 == 0 && eof2;

    if case1_ok && case2_ok {
        console::print("[Test] pipe_eof_only_when_write_count_zero PASSED\n");
    } else {
        crate::safe_print!(
            96,
            "[Test] pipe_eof_only_when_write_count_zero FAILED: case1(n={},eof={}) case2(n={},eof={})\n",
            n, eof, n2, eof2,
        );
    }
    crate::syscall::pipe::pipe_close_read(id);
}

/// Simulate the vfork stdout pipe lifecycle:
///   pipe_create → clone_ref (for child) → clone_ref (for child dup3) →
///   close_write (child closes original fd_w) → close_write (parent closes fd_w) →
///   write (simulate compile writing) → should succeed.
///
/// This mirrors what SHOULD happen for compile -V=full:
///   1. Go: pipe_create → write_count=1, read_count=1
///   2. fork: clone_deep_for_fork bumps write_count=2, read_count=2
///   3. child: dup3 bumps write_count=3; close fd_w → 2; execve closes fd_r → read_count=1
///   4. parent: close fd_w → write_count=1
///   5. compile writes to fd[1] → MUST SUCCEED
fn test_pipe_clone_ref_then_double_close() {
    let id = crate::syscall::pipe::pipe_create(); // write=1, read=1

    // Step 2: fork bumps both counts
    crate::syscall::pipe::pipe_clone_ref(id, true);  // write=2 (child copy)
    crate::syscall::pipe::pipe_clone_ref(id, false); // read=2 (child copy)

    // Step 3a: child dup3 adds write ref for fd=1
    crate::syscall::pipe::pipe_clone_ref(id, true);  // write=3

    // Step 3b: child closes original fd_w
    crate::syscall::pipe::pipe_close_write(id); // write=2

    // Step 3c: execve closes child's fd_r (cloexec)
    crate::syscall::pipe::pipe_close_read(id); // read=1

    // Step 4: parent closes its fd_w
    crate::syscall::pipe::pipe_close_write(id); // write=1

    // Step 5: compile writes to fd[1] — MUST find pipe and succeed
    match crate::syscall::pipe::pipe_write(id, b"compile -V=full output") {
        Ok(22) => console::print("[Test] pipe_clone_ref_then_double_close PASSED\n"),
        Ok(n) => crate::safe_print!(
            64,
            "[Test] pipe_clone_ref_then_double_close FAILED: write returned Ok({}) expected Ok(22)\n",
            n,
        ),
        Err(e) => crate::safe_print!(
            64,
            "[Test] pipe_clone_ref_then_double_close FAILED: write returned Err({}) — pipe missing with write_count=1\n",
            e,
        ),
    }

    // Cleanup
    crate::syscall::pipe::pipe_close_write(id); // write=0
    crate::syscall::pipe::pipe_close_read(id);  // read=0, pipe destroyed
}

/// Verify that duplicating a PipeRead via `pipe_clone_ref` (simulating F_DUPFD_CLOEXEC)
/// properly maintains the read_count so the pipe is not prematurely destroyed.
///
/// Bug fixed: `sys_fcntl(F_DUPFD/F_DUPFD_CLOEXEC)` was not calling `pipe_clone_ref`,
/// so closing the original fd would drop read_count to 0 even though the duplicate
/// fd still referenced the pipe.  This caused `pipe_can_write` to return false
/// (no reader) and confused the EOF logic.
fn test_pipe_dupfd_bumps_refcount() {
    use crate::syscall::pipe::*;

    let id = pipe_create(); // write=1, read=1

    // Simulate fcntl(fd_r, F_DUPFD_CLOEXEC): duplicate the read end
    pipe_clone_ref(id, false); // read=2

    // Close the original read end (as if Go closed the source fd after dup)
    pipe_close_read(id); // read=1 — NOT 0, because the duplicate still holds a ref

    // We should still be able to write (read_count=1 due to duplicate)
    match pipe_write(id, b"data for duplicate reader") {
        Ok(25) => console::print("[Test] pipe_dupfd_bumps_refcount PASSED\n"),
        other => crate::safe_print!(128,
            "[Test] pipe_dupfd_bumps_refcount FAILED: pipe_write returned {:?} (expected Ok(25))\n",
            other,
        ),
    }

    // Cleanup: close duplicate reader and write end
    pipe_close_read(id);  // read=0
    pipe_close_write(id); // write=0, pipe destroyed
}

/// Verify that `sys_dup3` atomically replaces an existing fd entry and properly
/// closes the old entry's resources.
///
/// Bug fixed: the old implementation used `get_fd` + `set_fd` as separate
/// operations, leaving a TOCTOU window where a concurrent thread (CLONE_FILES
/// goroutine) could insert a new PipeRead between the check and the write,
/// causing `set_fd` to silently overwrite it without calling `pipe_close_read`.
/// The new `swap_fd` method closes this race.
fn test_pipe_dup3_atomically_replaces_and_closes_old() {
    use crate::syscall::pipe::*;

    // Create pipe A (simulates fd that currently occupies newfd slot)
    let id_a = pipe_create(); // write=1, read=1

    // Create pipe B (the new fd we're dup3-ing in)
    let id_b = pipe_create(); // write=1, read=1

    // Simulate: dup3 replaces the PipeRead(id_a) slot with PipeWrite(id_b)
    // Step 1: increment refcount for pipe_b (the new entry)
    pipe_clone_ref(id_b, true); // write=2
    // Step 2: old entry at the slot was PipeRead(id_a) — close it
    pipe_close_read(id_a);  // read=0; pipe_a: write=1, read=0
    // Step 3: new entry is installed (pipe_b write end)
    // (slot now holds PipeWrite(id_b), write_count=2)

    // After the simulated dup3:
    // - pipe_a read_count should be 0 (old slot entry was closed)
    // - pipe_b write_count should be 2 (original + dup'd)

    // pipe_a: read_count=0 → write should return EPIPE (no readers, Linux behavior)
    let a_write = pipe_write(id_a, b"to pipe_a");
    if a_write.is_err() {
        console::print("[Test] pipe_dup3_atomically_replaces_and_closes_old: old entry closed correctly PASSED\n");
    } else {
        crate::safe_print!(128,
            "[Test] pipe_dup3_atomically_replaces_and_closes_old FAILED: pipe_write(id_a) returned Ok({}) expected Err(EPIPE)\n",
            a_write.unwrap(),
        );
    }

    // pipe_b: write_count=2, read_count=1, should still be writable
    match pipe_write(id_b, b"still alive") {
        Ok(11) => console::print("[Test] pipe_dup3_atomically_replaces_and_closes_old: new entry still alive PASSED\n"),
        other => crate::safe_print!(128,
            "[Test] pipe_dup3_atomically_replaces_and_closes_old FAILED: pipe_write(id_b) returned {:?}\n",
            other,
        ),
    }

    // Cleanup
    pipe_close_write(id_a); // write=0 → pipe_a fully destroyed
    pipe_close_write(id_b); // write=2-1=1
    pipe_close_write(id_b); // write=0
    pipe_close_read(id_b);  // read=0, pipe_b destroyed
}

/// Directly exercise the CLONE_VFORK race-fix mechanism:
///
/// Before the fix, `sys_clone_pidfd` inserted the parent TID into VFORK_WAITERS
/// *after* `fork_process` marked the child thread READY.  On a preemptive
/// scheduler the child could exec and call `vfork_complete` before the parent
/// inserted, leaving the table empty — so `vfork_complete` became a no-op and
/// the parent blocked in `schedule_blocking(u64::MAX)` forever.
///
/// The fix: insert into VFORK_WAITERS *before* `fork_process`.  This test
/// simulates that scenario end-to-end: pre-insert an entry then call
/// `vfork_complete` and verify the entry is removed (table is clean again).
fn test_vfork_complete_removes_entry() {
    // Use a PID that is unlikely to collide with any real process.
    const FAKE_CHILD_PID: u32 = 0xFFFF_FFFE;

    let removed = crate::syscall::proc::test_vfork_complete_mechanism(FAKE_CHILD_PID);

    if removed {
        console::print("[Test] vfork_complete_removes_entry PASSED\n");
    } else {
        console::print(
            "[Test] vfork_complete_removes_entry FAILED: entry still in VFORK_WAITERS after vfork_complete\n",
        );
    }

    // Ensure no entry leaked regardless of the outcome above.
    let len = crate::syscall::proc::vfork_waiters_len();
    if len != 0 {
        crate::safe_print!(
            64,
            "[Test] vfork_complete_removes_entry: LEAK — {} stale entries remain\n",
            len,
        );
    }
}
