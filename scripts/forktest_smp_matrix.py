#!/usr/bin/env python3
"""
Run forktest in various parameter combinations on SMP=2 and SMP=4.
"""

import subprocess
import time
import os
import sys
import signal

# Test configurations: (name, args, duration_seconds)
TEST_CONFIGS = [
    ("basic", [], 30),
    ("mmap_test", ["-mmap_test", "-mmap_alloc_mb=4"], 30),
    ("file_io", ["-file_io"], 30),
    ("signal", ["-send_signal"], 30),
    ("goroutine_stress", ["-goroutine_stress"], 30),
    ("combined_light", ["-combined_stress", "-num_children=3", "-mmap_alloc_mb=4", "-duration=10s"], 20),
    ("combined_heavy", ["-combined_stress", "-num_children=10", "-mmap_alloc_mb=10", "-duration=30s"], 45),
]

SMP_LEVELS = [2, 4]

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
        """Thread to log VM output."""
        for line in iter(vm_process.stdout.readline, ''):
            log_file.write(line)
            log_file.flush()
            if "Started sshd" in line:
                return True
            if "[PANIC]" in line or "WILD-DA" in line or "[SGI-S POISON]" in line:
                print(f"[CRASH DETECTED] {line.strip()}")
        return False

    import threading
    log_thread = threading.Thread(target=log_output)
    log_thread.daemon = True
    log_thread.start()

    # Wait for sshd to start
    print("Waiting for sshd to start...")
    log_thread.join(timeout=60)

    if not log_thread.is_alive() and vm_process.poll() is None:
        # sshd started, run forktest
        print("sshd started, running forktest...")

        try:
            result = subprocess.run([
                "ssh", "-o", "StrictHostKeyChecking=no",
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
            log_file.close()
            vm_process.terminate()
            try:
                vm_process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                vm_process.kill()
                vm_process.wait()
    else:
        print("[FAIL] sshd did not start in time")
        log_file.close()
        vm_process.terminate()
        try:
            vm_process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            vm_process.kill()
            vm_process.wait()
        return False

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