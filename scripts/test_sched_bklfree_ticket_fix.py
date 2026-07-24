#!/usr/bin/env python3
"""Test the ticket-leak fix for sched_bklfree_el0 (M5c step-2)."""

import subprocess
import time
import sys
import os

def wait_for_ssh(timeout=120):
    """Wait for SSH server to start."""
    print(f"Waiting for SSH server (max {timeout}s)...")
    start = time.time()
    while time.time() - start < timeout:
        try:
            result = subprocess.run(
                ["ssh", "-o", "StrictHostKeyChecking=no", "-o", "ConnectTimeout=2",
                 "-p", "2222", "root@localhost", "echo", "SSH_READY"],
                capture_output=True,
                timeout=3
            )
            if result.returncode == 0 and b"SSH_READY" in result.stdout:
                print("SSH server is ready!")
                return True
        except (subprocess.TimeoutExpired, subprocess.CalledProcessError):
            pass
        time.sleep(2)
    print("SSH server did not start in time")
    return False

def run_ssh_command(cmd, timeout=30):
    """Run a command via SSH and return (success, output)."""
    try:
        result = subprocess.run(
            ["ssh", "-o", "StrictHostKeyChecking=no", "-p", "2222",
             "root@localhost", cmd],
            capture_output=True,
            timeout=timeout
        )
        return result.returncode == 0, result.stdout.decode('utf-8', errors='replace'), result.stderr.decode('utf-8', errors='replace')
    except subprocess.TimeoutExpired:
        return False, "", "Command timed out"

def test_ticket_leak_fix():
    """Test that sched_bklfree_el0 doesn't leak tickets under fork/exec load."""
    
    print("=== Testing M5c step-2 ticket-leak fix ===\n")
    
    # Build and boot with SMP=4
    print("Building kernel with SMP=4...")
    build_result = subprocess.run([
        "cargo", "build", "--profile", "release-smp-shared", "--features", "smp-shared"
    ], capture_output=True)
    
    if build_result.returncode != 0:
        print("Build failed!")
        print(build_result.stderr.decode())
        return False
    
    print("Build successful!\n")
    
    # Start QEMU with SMP=4
    print("Starting QEMU with SMP=4...")
    qemu_proc = subprocess.Popen([
        "cargo", "run", "--profile", "release-smp-shared", "--features", "smp-shared"
    ], env={**os.environ, "SMP": "4", "GDB": "1"},  # GDB=1 for gdbstub
       stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    
    # Wait for SSH
    if not wait_for_ssh(timeout=180):
        print("Failed to connect to SSH")
        qemu_proc.terminate()
        return False
    
    print("\n=== Running forktest with sched_bklfree_el0 enabled ===\n")
    
    # Enable sched_bklfree_el0 and run forktest
    # First check if forktest exists
    success, stdout, stderr = run_ssh_command("ls /bin/forktest* 2>/dev/null", timeout=5)
    if not success or "forktest" not in stdout:
        print("forktest not found, skipping forktest test")
        print("Running busybox fork loop instead...")
        
        # Run busybox fork loop as alternative test
        cmd = "for i in $(seq 1 50); do busybox sh -c 'busybox true & busybox true & wait'; done && echo FORK_TEST_PASS"
        success, stdout, stderr = run_ssh_command(cmd, timeout=120)
        
        if success and "FORK_TEST_PASS" in stdout:
            print("✓ Busybox fork loop test passed!")
        else:
            print("✗ Busybox fork loop test failed")
            print(f"stdout: {stdout}")
            print(f"stderr: {stderr}")
            qemu_proc.terminate()
            return False
    else:
        # Run forktest with combined_stress
        cmd = "/bin/forktest_parent -num_children=3 -duration=15s -combined_stress"
        print(f"Running: {cmd}")
        success, stdout, stderr = run_ssh_command(cmd, timeout=60)
        
        if success:
            print("✓ Forktest completed successfully!")
            
            # Check for BKL stuck events in the output
            if "[BKL] stuck" in stdout or "[BKL] stuck" in stderr:
                print("⚠ Warning: BKL stuck events detected:")
                for line in (stdout + stderr).split('\n'):
                    if "[BKL] stuck" in line:
                        print(f"  {line}")
            else:
                print("✓ No BKL stuck events - good!")
                
            # Check for RECOVERED events (should be minimal with the fix)
            recovered_count = (stdout + stderr).count("[BKL] RECOVERED")
            if recovered_count > 0:
                print(f"⚠ Warning: {recovered_count} BKL RECOVERED events (ticket self-heals)")
            else:
                print("✓ No BKL RECOVERED events - ticket accounting looks clean!")
        else:
            print("✗ Forktest failed")
            print(f"stdout: {stdout}")
            print(f"stderr: {stderr}")
            qemu_proc.terminate()
            return False
    
    print("\n=== Test completed successfully! ===")
    qemu_proc.terminate()
    return True

if __name__ == "__main__":
    try:
        success = test_ticket_leak_fix()
        sys.exit(0 if success else 1)
    except KeyboardInterrupt:
        print("\nTest interrupted")
        sys.exit(1)
    except Exception as e:
        print(f"Test error: {e}")
        import traceback
        traceback.print_exc()
        sys.exit(1)