#!/usr/bin/env python3
"""
Repro harness for SMP=4 fork-hammer WILD-DA FAR=0x0 crashes.
Reboots devbox-smoltcp at SMP=4, waits for sshd, then runs fork hammer.
"""

import subprocess
import time
import os
import sys
import signal

def run_cmd(cmd, timeout=30, check=False):
    """Run a command with timeout."""
    try:
        result = subprocess.run(
            cmd,
            shell=True,
            timeout=timeout,
            capture_output=True,
            text=True,
            check=check
        )
        return result
    except subprocess.TimeoutExpired:
        return None
    except subprocess.CalledProcessError as e:
        return e

def boot_and_test():
    """Boot the VM and run the fork hammer test."""
    boot_num = 1
    
    while boot_num <= 3:
        print(f"\n=== Boot {boot_num}/3 ===")
        
        # Start the VM
        vm_process = subprocess.Popen(
            "SMP=4 overlays/devbox/run-smoltcp.sh",
            shell=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True
        )
        
        log_file = open(f"01_fork_hunt_{boot_num}.log", "w")
        
        def log_output():
            """Thread to log VM output."""
            for line in iter(vm_process.stdout.readline, ''):
                log_file.write(line)
                log_file.flush()
                print(line, end='')
                if "Started sshd" in line:
                    return True
            return False
        
        import threading
        log_thread = threading.Thread(target=log_output)
        log_thread.daemon = True
        log_thread.start()
        
        # Wait for sshd to start
        print("Waiting for sshd to start...")
        log_thread.join(timeout=60)
        
        if not log_thread.is_alive() and vm_process.poll() is None:
            # sshd started
            print("sshd started, beginning fork hammer...")
            
            # Run fork hammer across 16 SSH connections
            import concurrent.futures
            
            def fork_hammer(worker_id):
                """Run fork hammer in one SSH connection."""
                try:
                    result = subprocess.run([
                        "ssh", "-o", "StrictHostKeyChecking=no",
                        "-o", "ConnectTimeout=5", "-p", "2222",
                        "root@localhost",
                        "for i in 1 2 3 4 5 6 7 8; do busybox true; done"
                    ], capture_output=True, text=True, timeout=30)
                    return (worker_id, result.returncode, result.stderr)
                except subprocess.TimeoutExpired:
                    return (worker_id, -1, "timeout")
                except Exception as e:
                    return (worker_id, -2, str(e))
            
            results = []
            with concurrent.futures.ThreadPoolExecutor(max_workers=16) as executor:
                futures = [executor.submit(fork_hammer, i) for i in range(16)]
                for future in concurrent.futures.as_completed(futures, timeout=120):
                    results.append(future.result())
            
            # Check for crashes
            crash_count = 0
            bkl_recovered_count = 0
            watchdog_count = 0
            
            for worker_id, retcode, stderr in results:
                if retcode != 0:
                    if "SIGSEGV" in stderr or "WILD-DA" in stderr or "abort from EL0" in stderr:
                        crash_count += 1
                        print(f"Worker {worker_id}: CRASH - {stderr[:100]}")
                    elif "timeout" in stderr:
                        print(f"Worker {worker_id}: timeout")
                    else:
                        print(f"Worker {worker_id}: error - {stderr[:100]}")
            
            # Check log for diagnostic lines
            log_file.flush()
            with open(f"01_fork_hunt_{boot_num}.log", "r") as read_log:
                log_content = read_log.read()
                bkl_recovered_count = log_content.count("[BKL] RECOVERED")
                watchdog_count = log_content.count("[WATCHDOG]")
            
            print(f"\n=== Boot {boot_num} results ===")
            print(f"Crashes: {crash_count}/16")
            print(f"[BKL] RECOVERED: {bkl_recovered_count}")
            print(f"[WATCHDOG]: {watchdog_count}")
            
            # Check for fatal crashes (WILD-DA FAR=0x0 at valid busybox PC)
            if "WILD-DA.*FAR=0x0" in log_content or "WILD-DA FAR=0x0" in log_content:
                print("FATAL: WILD-DA FAR=0x0 detected!")
            
            log_file.close()
            
            # Kill the VM
            vm_process.terminate()
            try:
                vm_process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                vm_process.kill()
                vm_process.wait()
            
            # Check if this boot passed
            if crash_count == 0 and bkl_recovered_count < 5 and watchdog_count == 0:
                print(f"Boot {boot_num}: PASSED")
                boot_num += 1
            else:
                print(f"Boot {boot_num}: FAILED")
                return False
        else:
            print("sshd did not start in time")
            log_file.close()
            vm_process.terminate()
            try:
                vm_process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                vm_process.kill()
                vm_process.wait()
            return False
    
    print("\n=== All boots PASSED ===")
    return True

if __name__ == "__main__":
    try:
        success = boot_and_test()
        sys.exit(0 if success else 1)
    except KeyboardInterrupt:
        print("\nInterrupted")
        sys.exit(2)
    except Exception as e:
        print(f"Error: {e}")
        import traceback
        traceback.print_exc()
        sys.exit(1)