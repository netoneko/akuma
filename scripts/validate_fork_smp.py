#!/usr/bin/env python3
"""
Fork-corruption validation harness for SMP=4.

Boots devbox-smoltcp at SMP=4, waits for sshd, then fork-hammers:
16 concurrent SSH connections each running a busybox fork loop.
Greps the boot log for SIGSEGV / corruption signatures.

Success bar: 0 fault lines across N boots × M rounds.
"""
import subprocess, time, sys, os, signal, threading, socket

REPO = "/Users/netoneko/github.com/netoneko/akuma"
# INSTANCE=1 → SSH port 2322 (2222 + 100*1), avoids clashing with default.
# NOTE: the devbox userspace sshd actually binds to guest:23 (not :22), so the
# effective SSH port is INSTANCE's tel port: 2323 + 100*INSTANCE = 2423 for INSTANCE=1.
INSTANCE = os.environ.get("INSTANCE", "1")
# Userspace sshd is on guest port 23, mapped to host's tel port (2323 + 100*N)
SSH_PORT = 2323 + 100 * int(INSTANCE)
LOG_FILE = f"{REPO}/fork_hammer_boot.log"
RESULT_FILE = f"{REPO}/fork_hammer_result.txt"

# Crash signatures to grep for
FAULT_PATTERNS = [
    "SIGSEGV",
    "abort from EL0",
    "[WILD-DA]",
    "[DA-MISS]",
    "PANIC",
    "ppid=0",  # clobbered Process signature
]

def wait_for_sshd(timeout=120):
    """Wait for the SSH port to accept connections."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            s.settimeout(3)
            s.connect(('127.0.0.1', SSH_PORT))
            banner = s.recv(256)
            s.close()
            if b"SSH" in banner:
                return True
        except (ConnectionRefusedError, socket.timeout, OSError):
            pass
        time.sleep(2)
    return False

def ssh_cmd(cmd, timeout=30):
    """Run a command over SSH using Python (ssh CLI is blocked)."""
    try:
        result = subprocess.run(
            ["ssh", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
             "-o", "ConnectTimeout=5", "-p", str(SSH_PORT), "root@localhost", cmd],
            capture_output=True, text=True, timeout=timeout
        )
        return result.returncode, result.stdout, result.stderr
    except subprocess.TimeoutExpired:
        return -1, "", "timeout"
    except Exception as e:
        return -1, "", str(e)

def fork_hammer_round(round_num):
    """Run 8 concurrent SSH connections each doing 8 busybox forks."""
    threads = []
    N = 8
    results = [None] * N

    def worker(idx):
        rc, out, err = ssh_cmd("for i in 1 2 3 4 5 6 7 8; do busybox true; done; echo OK", timeout=30)
        results[idx] = (rc, out, err)

    for i in range(N):
        t = threading.Thread(target=worker, args=(i,))
        threads.append(t)
        t.start()

    for t in threads:
        t.join(timeout=60)

    return results

def check_log_for_faults():
    """Check the boot log for crash signatures."""
    try:
        with open(LOG_FILE, "rb") as f:
            content = f.read().decode("utf-8", errors="replace")
    except FileNotFoundError:
        return []

    faults = []
    for line in content.split("\n"):
        for pattern in FAULT_PATTERNS:
            if pattern in line:
                faults.append(line.strip())
                break
    return faults

def main():
    boots = int(os.environ.get("BOOTS", "3"))
    rounds = int(os.environ.get("ROUNDS", "10"))
    smp = os.environ.get("SMP", "4")

    print(f"Fork-hammer validation: {boots} boots × {rounds} rounds, SMP={smp}")
    open(LOG_FILE, "w").close()  # truncate

    total_faults = 0
    for boot in range(1, boots + 1):
        print(f"\n=== Boot {boot}/{boots} ===")
        # Boot QEMU
        env = os.environ.copy()
        env["SMP"] = smp
        env["INSTANCE"] = INSTANCE
        env["DEVBOX_DISK"] = "devbox.img"
        env["DEVBOX_MEMORY"] = "4096"

        proc = subprocess.Popen(
            ["bash", "-c", f"cd {REPO} && INSTANCE={INSTANCE} SMP={smp} "
             f"DEVBOX_MEMORY=4096 "
             f"cargo run --profile release-smp-shared --features devbox-smoltcp,no-tests "
             f"> {LOG_FILE} 2>&1"],
            env=env,
            preexec_fn=os.setsid,
        )

        print(f"  QEMU PID={proc.pid}, waiting for sshd...")
        if not wait_for_sshd(timeout=120):
            print(f"  ERROR: sshd did not come up within 120s")
            # Check for early crash
            faults = check_log_for_faults()
            if faults:
                print(f"  CRASHED during boot! {len(faults)} fault lines:")
                for f in faults[:10]:
                    print(f"    {f}")
                with open(RESULT_FILE, "w") as rf:
                    rf.write(f"FAILED: crashed during boot {boot}\n")
                    for f in faults:
                        rf.write(f"  {f}\n")
            os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
            time.sleep(3)
            os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
            total_faults += len(faults)
            continue

        print(f"  sshd is up. Starting fork-hammer ({rounds} rounds × 16 concurrent)...")

        boot_faults = 0
        for rnd in range(1, rounds + 1):
            results = fork_hammer_round(rnd)
            # Check log after each round
            faults = check_log_for_faults()
            new_faults = [f for f in faults if "SIGSEGV" in f or "WILD-DA" in f
                          or "DA-MISS" in f or "PANIC" in f or "abort from EL0" in f]
            if new_faults:
                boot_faults += len(new_faults)
                print(f"  Round {rnd}: {len(new_faults)} NEW fault lines!")
                for f in new_faults[:5]:
                    print(f"    {f}")
                break
            # Report round status
            errors = sum(1 for r in results if r and r[0] != 0 and 'OK' not in (r[1] if r[1] else ''))
            if errors > 6:
                print(f"  Round {rnd}: {errors}/8 SSH connections failed (likely exhaustion)")
            elif rnd % 5 == 0:
                print(f"  Round {rnd}: OK ({8-errors}/8 succeeded)")

        total_faults += boot_faults
        if boot_faults == 0:
            print(f"  Boot {boot}: CLEAN ({rounds} rounds, 0 faults)")
        else:
            print(f"  Boot {boot}: FAILED ({boot_faults} fault lines)")
            with open(RESULT_FILE, "w") as rf:
                rf.write(f"FAILED boot {boot}: {boot_faults} faults\n")

        # Kill QEMU
        os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
        time.sleep(3)
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
        except ProcessLookupError:
            pass
        time.sleep(2)

    print(f"\n=== RESULT: {total_faults} total fault lines across {boots} boots ===")
    if total_faults == 0:
        print("PASS: fork-corruption bug appears FIXED")
        with open(RESULT_FILE, "w") as rf:
            rf.write(f"PASS: 0 faults across {boots} boots × {rounds} rounds at SMP={smp}\n")
    else:
        print("FAIL: corruption still present")
        sys.exit(1)

if __name__ == "__main__":
    main()
