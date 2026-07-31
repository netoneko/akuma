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
# INSTANCE=1 → SSH port 2322 (2222 + 100*1), avoids clashing with the default VM.
INSTANCE = os.environ.get("INSTANCE", "1")
# `cargo_runner.sh` forwards host SSH_PORT (2222 + 100*INSTANCE) → guest :22, and
# guest :22 is what answers on a devbox-smoltcp boot.
#
# This used to be `2323 + 100*INSTANCE` (the *tel* forward, guest :23) on the
# assumption that the devbox userspace sshd binds guest :23. It doesn't:
# `overlays/devbox/rootfs/etc/herd/enabled/sshd.conf` runs it as
# `/bin/sshd --port 22`, and on a smoltcp boot the in-kernel SSH server has
# already bound :22 by the time herd's 10 s start_delay elapses — so :22 is the
# live listener and :23 was never open. Verified by banner: :22 answers
# `SSH-2.0-Akuma_0.1` (in-kernel, crates/akuma-ssh) vs the userspace server's
# `SSH-2.0-Akuma_0.1_User`.
SSH_PORT = 2222 + 100 * int(INSTANCE)
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

# Literal template lines the kernel prints as *documentation* at boot (the bun-install
# memory-requirements banner in `src/tests.rs`), which contain the same words as a real
# fault report. Without this, every boot "crashes" on line ~634 before the hammer even
# starts — which is exactly what `fork_hammer_result.txt` recorded.
FAULT_FALSE_POSITIVES = [
    "[Fault] Process N (name) SIGSEGV after Xs",
    "[DA-DP] ... anon alloc failed, 0 free pages",
    "[signal] sig 11 frame page ... not mappable",
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

# Every forked child prints this, and the parent shell echoes it back. "Didn't crash"
# is NOT the bar: a fork whose child address space is subtly wrong can still exit 0
# (`busybox true` never reads its own memory), so each child must produce *correct
# output* — which means its CoW-shared text/data/stack pages actually resolved.
CHILD_MARK = "fork_ok_"


def fork_hammer_round(round_num):
    """Run 8 concurrent SSH connections each doing 8 busybox forks.

    Each fork is `busybox echo fork_ok_<i>` so the child has to execute correctly and
    write the expected bytes back up the pipe; `verify_round` checks all 8 markers came
    back per connection. `busybox true` (exit-status only) is kept alongside it so the
    cheap fork path is still hammered at the same rate.
    """
    threads = []
    N = 8
    results = [None] * N

    cmd = ("for i in 1 2 3 4 5 6 7 8; do busybox true; busybox echo "
           + CHILD_MARK + "$i; done; echo OK")

    def worker(idx):
        rc, out, err = ssh_cmd(cmd, timeout=30)
        results[idx] = (rc, out, err)

    for i in range(N):
        t = threading.Thread(target=worker, args=(i,))
        threads.append(t)
        t.start()

    for t in threads:
        t.join(timeout=60)

    return results


def verify_round(results):
    """Data-integrity check on the forked children's output.

    Returns `(corrupt, torn_down)`, two lists of human-readable descriptions.

    The distinction matters, and it is drawn from the observed failure shape rather
    than guessed. Under 8 concurrent connections this sshd tears sessions down
    (documented caveat in `docs/runbooks/debug-smp-fork-corruption.md`: "the userspace
    sshd exhausts after ~3 rounds of 8 concurrent connections — a pre-existing sshd
    robustness issue, not fork corruption"). When that happens the connection returns
    the trailing `OK` from the shell builtin but **none** of the 8 `busybox echo`
    markers, and ssh reports "Connection closed by remote host" — measured to be
    strictly all-or-nothing (0/8 or 8/8, never partial) and to occur at the same rate
    with fork BKL-held as BKL-free.

    Fork/CoW corruption looks different: a child whose address space is subtly wrong
    produces *partial* or *garbled* output, because some of its pages resolved and some
    did not. So:

      - **partial** markers (1..7 of 8) → `corrupt`; that is the real signal.
      - **zero** markers with `OK` present → `torn_down`; reported, but not a fault.

    A connection that never ran at all (no `OK`) is skipped entirely — the caller
    already counts those as exhaustion.
    """
    corrupt, torn_down = [], []
    for idx, r in enumerate(results):
        if not r:
            continue
        rc, out, _err = r
        out = out or ""
        if "OK" not in out:
            continue  # connection never ran; exhaustion, handled by the caller
        found = [i for i in range(1, 9) if f"{CHILD_MARK}{i}" in out]
        if len(found) == 8:
            continue
        if not found:
            torn_down.append(f"conn {idx}: session torn down before any child ran (rc={rc})")
        else:
            corrupt.append(
                f"conn {idx}: PARTIAL child output, got {found} of 1..8 (rc={rc})"
            )
    return corrupt, torn_down

def check_log_for_faults():
    """Check the boot log for crash signatures."""
    try:
        with open(LOG_FILE, "rb") as f:
            content = f.read().decode("utf-8", errors="replace")
    except FileNotFoundError:
        return []

    faults = []
    for line in content.split("\n"):
        stripped = line.strip()
        if any(fp in stripped for fp in FAULT_FALSE_POSITIVES):
            continue
        for pattern in FAULT_PATTERNS:
            if pattern in line:
                faults.append(stripped)
                break
    return faults

def main():
    boots = int(os.environ.get("BOOTS", "3"))
    rounds = int(os.environ.get("ROUNDS", "10"))
    smp = os.environ.get("SMP", "4")
    # Overridable so the same harness validates a BKL carve-out build, e.g.
    #   FEATURES=devbox-smoltcp,no-tests,no-bkl-process scripts/validate_fork_smp.py
    features = os.environ.get("FEATURES", "devbox-smoltcp,no-tests")
    disk = os.environ.get("DISK", "devbox.img")
    memory = os.environ.get("MEMORY", "4096")

    print(f"Fork-hammer validation: {boots} boots × {rounds} rounds, SMP={smp}, "
          f"features={features}, disk={disk}, mem={memory}")
    open(LOG_FILE, "w").close()  # truncate

    total_faults = 0
    total_torn = 0
    for boot in range(1, boots + 1):
        print(f"\n=== Boot {boot}/{boots} ===")
        # Boot QEMU
        env = os.environ.copy()
        env["SMP"] = smp
        env["INSTANCE"] = INSTANCE
        # `scripts/cargo_runner.sh` reads DISK and MEMORY. This used to set
        # DEVBOX_DISK/DEVBOX_MEMORY, which the runner ignores — so every "devbox at
        # 4 GB" run in this harness's history actually booted disk.img at the 256M
        # default. Fixed; both stay overridable from the environment.
        env["DISK"] = disk
        env["MEMORY"] = memory

        proc = subprocess.Popen(
            ["bash", "-c", f"cd {REPO} && INSTANCE={INSTANCE} SMP={smp} "
             f"DISK={disk} MEMORY={memory} "
             f"cargo run --profile release-smp-shared --features {features} "
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
        boot_torn = 0
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
            # Data integrity, not just "didn't crash": every child that ran must have
            # produced its expected output. Only *partial* output counts as a fault —
            # see verify_round for why a fully-empty session does not.
            bad_output, torn = verify_round(results)
            boot_torn += len(torn)
            if bad_output:
                boot_faults += len(bad_output)
                print(f"  Round {rnd}: {len(bad_output)} PARTIAL-OUTPUT CHILDREN (corruption)!")
                for b in bad_output[:5]:
                    print(f"    {b}")
                break
            # Report round status
            errors = sum(1 for r in results if r and r[0] != 0 and 'OK' not in (r[1] if r[1] else ''))
            if errors > 6:
                print(f"  Round {rnd}: {errors}/8 SSH connections failed (likely exhaustion)")
            elif rnd % 5 == 0:
                print(f"  Round {rnd}: OK ({8-errors}/8 succeeded)")

        total_faults += boot_faults
        total_torn += boot_torn
        if boot_faults == 0:
            print(f"  Boot {boot}: CLEAN ({rounds} rounds, 0 faults, "
                  f"{boot_torn} sshd session teardowns)")
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

    print(f"\n=== RESULT: {total_faults} total fault lines across {boots} boots "
          f"({total_torn} sshd session teardowns, not counted as faults) ===")
    if total_faults == 0:
        print("PASS: fork-corruption bug appears FIXED")
        with open(RESULT_FILE, "w") as rf:
            rf.write(f"PASS: 0 faults across {boots} boots × {rounds} rounds at SMP={smp}"
                     f" (features={features}; {total_torn} sshd session teardowns)\n")
    else:
        print("FAIL: corruption still present")
        sys.exit(1)

if __name__ == "__main__":
    main()
