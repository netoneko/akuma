#!/usr/bin/env python3
"""
Run forktest in various parameter combinations on SMP=2 and SMP=4.
"""

import subprocess
import time
import os
import socket
import sys
import signal

# Test configurations: (name, args, duration_seconds)
# NOTE: every config MUST pass an explicit `-duration`. forktest_parent's
# `-duration` flag defaults to 0, which means "run until all children finish" —
# unbounded. The first five configs used to omit it, so they could never
# complete inside the `duration + 30` s subprocess timeout and reported
# "[FAIL] Forktest timed out" on a perfectly healthy kernel, at every SMP level.
TEST_CONFIGS = [
    ("basic", ["-duration=20s"], 30),
    ("mmap_test", ["-mmap_test", "-mmap_alloc_mb=4", "-duration=20s"], 30),
    ("file_io", ["-file_io", "-duration=20s"], 30),
    ("signal", ["-send_signal", "-duration=20s"], 30),
    ("goroutine_stress", ["-goroutine_stress", "-duration=20s"], 30),
    ("combined_light", ["-combined_stress", "-num_children=3", "-mmap_alloc_mb=4", "-duration=10s"], 20),
    ("combined_heavy", ["-combined_stress", "-num_children=10", "-mmap_alloc_mb=10", "-duration=30s"], 45),
]

# Override to re-run a single level, e.g. SMP_LEVELS=4 for the SMP=4 half only.
SMP_LEVELS = [int(n) for n in os.environ.get("SMP_LEVELS", "2,4").split(",")]

def run_forktest(smp, test_name, test_args, duration):
    """Run forktest with given parameters."""
    print(f"\n{'='*60}")
    print(f"SMP={smp} | Test: {test_name} | Args: {' '.join(test_args)}")
    print(f"{'='*60}")

    # Build forktest arguments
    forktest_args = [f"/bin/forktest_parent"] + test_args

    # Start the VM
    env = os.environ.copy()
    env["SMP"] = str(smp)
    env["MEMORY"] = "2048"

    vm_process = subprocess.Popen(
        ["overlays/devbox/run-smoltcp.sh"],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True
    )

    log_filename = f"forktest_smp{smp}_{test_name}.log"
    log_file = open(log_filename, "w")

    def log_output():
        """Thread to log VM output.

        Drains the pipe until EOF and never returns early. Two reasons:

        1. The console UART is deliberately NOT locked, so a marker line can be
           split mid-write by another core (`[herd] Started ` + `[syscall]
           bind(..)` + `sshd (pid= 2)` is a real SMP=4 capture). Any readiness
           test based on matching a whole line is unreliable by construction —
           readiness is probed on the SSH port instead, below.
        2. Returning at the marker left nobody draining the pipe, so the log
           stopped at boot and the crash grep below could only ever match boot
           output. Once the 64 KB pipe filled, QEMU blocked on write too.
        """
        for line in iter(vm_process.stdout.readline, ''):
            if log_file.closed:
                return
            log_file.write(line)
            log_file.flush()
            if "[PANIC]" in line or "WILD-DA" in line or "[SGI-S POISON]" in line:
                print(f"[CRASH DETECTED] {line.strip()}")

    import threading
    log_thread = threading.Thread(target=log_output)
    log_thread.daemon = True
    log_thread.start()

    # Probe for a real SSH banner rather than scrape the console. Two traps:
    #  - Console text is unreliable: a marker line can be split across herd's
    #    separate print() calls by another core's emit (CONSOLE_LOCK makes each
    #    emit atomic, but a logical line spans several).
    #  - A bare connect() is NOT readiness: QEMU's user-mode hostfwd accepts the
    #    TCP connection immediately, before the guest is listening. Only reading
    #    "SSH-" off the socket proves sshd is actually serving.
    print("Waiting for sshd to serve an SSH banner...")
    ready = False
    deadline = time.time() + 90
    while time.time() < deadline and vm_process.poll() is None:
        try:
            with socket.create_connection(("localhost", 2222), timeout=3) as s:
                s.settimeout(3)
                if s.recv(64).startswith(b"SSH-"):
                    ready = True
                    break
        except OSError:
            pass
        time.sleep(1)

    if ready:
        # sshd is accepting, run forktest
        print("sshd accepting, running forktest...")

        try:
            result = subprocess.run([
                "ssh", "-o", "StrictHostKeyChecking=no",
                # BatchMode: the server is pubkey-only (see scripts/ssh_harness.py).
                # Without this a failed pubkey drops to an interactive prompt with
                # no stdin and burns the whole timeout instead of returning.
                "-o", "BatchMode=yes",
                "-o", "ConnectTimeout=10", "-p", "2222",
                "root@localhost"
            ] + forktest_args, capture_output=True, text=True, timeout=duration + 30)

            print(f"Forktest exit code: {result.returncode}")
            if result.stdout:
                print("STDOUT:")
                print(result.stdout[:1000])  # First 1000 chars
            if result.stderr:
                print("STDERR:")
                print(result.stderr[:1000])

            # Check for crash indicators in stderr
            crashed = False
            if "SIGSEGV" in result.stderr or "WILD-DA" in result.stderr:
                crashed = True
                print("[FAIL] Crash detected in forktest output")

            # Check log for kernel crash indicators
            log_file.flush()
            with open(log_filename, "r") as read_log:
                log_content = read_log.read()
                bkl_recovered = log_content.count("[BKL] RECOVERED")
                watchdog = log_content.count("[WATCHDOG]")
                panic = log_content.count("[PANIC]")
                wild_da = log_content.count("WILD-DA")
                poison = log_content.count("[SGI-S POISON]")

            print(f"\n=== Kernel Diagnostics ===")
            print(f"[BKL] RECOVERED: {bkl_recovered}")
            print(f"[WATCHDOG]: {watchdog}")
            print(f"[PANIC]: {panic}")
            print(f"WILD-DA: {wild_da}")
            print(f"[SGI-S POISON]: {poison}")

            passed = (not crashed and
                     result.returncode == 0 and
                     bkl_recovered < 5 and
                     watchdog == 0 and
                     panic == 0 and
                     wild_da == 0 and
                     poison == 0)

            status = "PASS" if passed else "FAIL"
            print(f"\n=== {status} ===")
            return passed

        except subprocess.TimeoutExpired:
            print("[FAIL] Forktest timed out")
            return False
        except Exception as e:
            print(f"[FAIL] Error: {e}")
            return False
        finally:
            shutdown_vm(vm_process, log_thread, log_file)
    else:
        print("[FAIL] sshd never accepted a connection in time")
        shutdown_vm(vm_process, log_thread, log_file)
        return False


def shutdown_vm(vm_process, log_thread, log_file):
    """Stop the VM, then drain and close the log.

    Order matters: the reader thread is still writing until QEMU's stdout hits
    EOF, so closing the log first is what produced the
    `ValueError: I/O operation on closed file` tracebacks.
    """
    vm_process.terminate()
    try:
        vm_process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        vm_process.kill()
        vm_process.wait()
    log_thread.join(timeout=5)
    log_file.close()

def main():
    """Run all test configurations."""
    all_passed = True

    for smp in SMP_LEVELS:
        print(f"\n\n{'#'*60}")
        print(f"# SMP={smp} Testing")
        print(f"{'#'*60}")

        for test_name, test_args, duration in TEST_CONFIGS:
            passed = run_forktest(smp, test_name, test_args, duration)
            if not passed:
                all_passed = False
            # Brief pause between tests
            time.sleep(5)

    print(f"\n\n{'#'*60}")
    if all_passed:
        print("# ALL TESTS PASSED")
    else:
        print("# SOME TESTS FAILED")
    print(f"{'#'*60}")

    return 0 if all_passed else 1

if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        print("\nInterrupted")
        sys.exit(2)
    except Exception as e:
        print(f"Error: {e}")
        import traceback
        traceback.print_exc()
        sys.exit(1)