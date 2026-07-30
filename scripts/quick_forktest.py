#!/usr/bin/env python3
"""
Quick forktest sanity check on SMP=2 and SMP=4.
"""

import subprocess
import time
import os
import sys

SMP_LEVELS = [2, 4]
TEST_DURATION = 30  # seconds

def run_forktest_quick(smp):
    """Run forktest quickly on given SMP level."""
    print(f"\n{'='*60}")
    print(f"Testing SMP={smp} with basic forktest")
    print(f"{'='*60}")

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

    log_filename = f"quick_forktest_smp{smp}.log"
    log_file = open(log_filename, "w")

    # Wait for sshd (simple polling)
    print("Waiting for sshd to start...")
    start_time = time.time()
    sshd_ready = False

    while time.time() - start_time < 60:
        line = vm_process.stdout.readline()
        if not line:
            break
        log_file.write(line)
        log_file.flush()
        if "Started sshd" in line:
            sshd_ready = True
            print("sshd ready!")
            break

    if not sshd_ready:
        print("[FAIL] sshd did not start")
        vm_process.terminate()
        vm_process.wait(timeout=5)
        log_file.close()
        return False

    # Run forktest
    print("Running forktest...")
    try:
        result = subprocess.run([
            "ssh", "-o", "StrictHostKeyChecking=no",
            "-o", "ConnectTimeout=10", "-p", "2222",
            "root@localhost", "/bin/forktest_parent", "-duration=15s"
        ], capture_output=True, text=True, timeout=30)

        print(f"Exit code: {result.returncode}")

        # Check for crash indicators
        crashed = ("SIGSEGV" in result.stderr or "WILD-DA" in result.stderr or
                   "panic" in result.stderr.lower())

        # Check kernel log
        log_file.flush()
        with open(log_filename, "r") as f:
            log_content = f.read()
            bkl_recovered = log_content.count("[BKL] RECOVERED")
            watchdog = log_content.count("[WATCHDOG]")
            wild_da = log_content.count("WILD-DA")

        print(f"[BKL] RECOVERED: {bkl_recovered}")
        print(f"[WATCHDOG]: {watchdog}")
        print(f"WILD-DA: {wild_da}")

        passed = (not crashed and result.returncode == 0 and
                 bkl_recovered < 5 and watchdog == 0 and wild_da == 0)

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

def main():
    results = {}

    for smp in SMP_LEVELS:
        passed = run_forktest_quick(smp)
        results[smp] = passed
        time.sleep(5)

    print(f"\n\n{'#'*60}")
    print("# Results Summary")
    print(f"{'#'*60}")
    for smp, passed in results.items():
        status = "PASS" if passed else "FAIL"
        print(f"SMP={smp}: {status}")

    all_passed = all(results.values())
    print(f"\nOverall: {'PASS' if all_passed else 'FAIL'}")

    return 0 if all_passed else 1

if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        print("\nInterrupted")
        sys.exit(2)