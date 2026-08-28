#!/usr/bin/env python3
"""Reproduce F8 (the intermittent SMP=1 exercise-suite wedge) under a gdbstub, and
snapshot the spinning core the moment it happens.

The wedge is ~40-50% per suite, so this loops attempts until it catches one. On a
catch it leaves QEMU ALIVE and dumps, via lldb against the symbolized ELF:
  * pc / sp / the low registers of the (single) core
  * a backtrace
  * several samples of pc, so a spin shows up as a small set of addresses

Detection is "the boot log stopped growing while QEMU is still running" — the tell
established for F8, since the wedge can show zero watchdog time-jump lines.
"""
import os, re, subprocess, sys, time

import os as _os, sys as _sys
_sys.path.insert(0, _os.path.join(_os.path.dirname(_os.path.abspath(__file__))))
import vm_ready

REPO = subprocess.run(["git", "rev-parse", "--show-toplevel"],
                      capture_output=True, text=True, check=True).stdout.strip()
ELF = os.path.join(REPO, "target/aarch64-unknown-none/release/akuma")
PORT = 1234
EXERCISES = ["cowstale", "bssfork", "forkprobe", "elftest"]
STALL_SECS = 40          # no console output for this long, with qemu alive => wedged
OUT = "/tmp/f8"


def ssh(cmd, timeout=90):
    try:
        r = subprocess.run(
            ["ssh", "-q", "-o", "StrictHostKeyChecking=no",
             "-o", "UserKnownHostsFile=/dev/null", "-o", "ConnectTimeout=10",
             "-p", "2222", "root@localhost", cmd],
            capture_output=True, timeout=timeout)
        return r.returncode, r.stdout.decode("utf-8", "replace")
    except subprocess.TimeoutExpired:
        return -1, "<ssh timeout>"


def size(p):
    try:
        return os.path.getsize(p)
    except OSError:
        return -1


def lldb_snapshot(tag, log_path):
    """Attach, sample the pc a few times, symbolize. QEMU stays up afterwards."""
    # ELR_EL1/ESR_EL1/FAR_EL1 are the point of this: PC pinned at the vector says
    # "a fault loop", but only ELR_EL1 names the FAULTING instruction and ESR/FAR say
    # what it did. `register read --all` because QEMU's aarch64 gdbstub puts the system
    # registers in a separate set, and they are absent from the default `pc sp x*` view.
    cmds = ["-o", f"gdb-remote localhost:{PORT}",
            "-o", "register read pc sp x0 x1 x29 x30",
            "-o", "register read --all",
            "-o", "bt"]
    for _ in range(6):
        cmds += ["-o", "register read pc"]
    cmds += ["-o", "image lookup -a $pc", "-o", "detach", "-o", "quit"]
    r = subprocess.run(["lldb", "-b", ELF] + cmds,
                       capture_output=True, timeout=300)
    out = r.stdout.decode("utf-8", "replace") + r.stderr.decode("utf-8", "replace")
    with open(f"{OUT}_lldb_{tag}.txt", "w") as f:
        f.write(out)
    return out


def attempt(n):
    subprocess.run(["pkill", "-f", "qemu-system-aarch64"], capture_output=True)
    time.sleep(2)
    log_path = f"{OUT}_boot_{n}.log"
    log = open(log_path, "w")
    env = dict(os.environ, SMP="1", MEMORY="2048", GDB="1", GDB_PORT=str(PORT))
    vm = subprocess.Popen(["cargo", "run", "--release"], cwd=REPO, env=env,
                          stdout=log, stderr=subprocess.STDOUT)

    booted = False
    deadline = time.time() + 480
    while time.time() < deadline:
        time.sleep(3)
        try:
            with open(log_path, "rb") as f:
                if vm_ready.ssh_probe(2222) or re.search(rb"Started sshd|sshd started", f.read()):
                    booted = True
                    break
        except FileNotFoundError:
            pass
    if not booted:
        print(f"attempt {n}: NO BOOT", flush=True)
        vm.terminate()
        log.close()
        return None

    # Drive the exercises exactly like the gate: detached, sentinel-terminated.
    for ex in EXERCISES:
        p = f"/tmp/f8_{ex}.log"
        ssh(f"nohup sh -c '{{ {ex}; echo __EX_DONE__; }} > {p} 2>&1' > /dev/null 2>&1 &")
        deadline = time.time() + 420
        last, last_change = size(log_path), time.time()
        done = False
        while time.time() < deadline:
            time.sleep(5)
            cur = size(log_path)
            if cur != last:
                last, last_change = cur, time.time()
            elif time.time() - last_change > STALL_SECS and vm.poll() is None:
                print(f"attempt {n}: WEDGED during {ex} "
                      f"(log stalled {int(time.time()-last_change)}s, qemu alive)",
                      flush=True)
                out = lldb_snapshot(n, log_path)
                pcs = re.findall(r"pc\s*=\s*(0x[0-9a-f]+)", out)
                print("  pc samples:", " ".join(pcs[:8]), flush=True)
                for line in out.splitlines():
                    if ("akuma`" in line or "Summary:" in line
                            or line.strip().startswith("frame #")):
                        print("  " + line.strip()[:150], flush=True)
                vm.terminate()
                log.close()
                return n
            rc, o = ssh(f"cat {p} 2>/dev/null", timeout=30)
            if "__EX_DONE__" in o:
                done = True
                break
        if not done:
            print(f"attempt {n}: {ex} did not finish, no stall detected", flush=True)

    print(f"attempt {n}: suite completed, no wedge", flush=True)
    vm.terminate()
    try:
        vm.wait(timeout=10)
    except subprocess.TimeoutExpired:
        vm.kill()
    log.close()
    return None


if __name__ == "__main__":
    tries = int(sys.argv[1]) if len(sys.argv) > 1 else 4
    for i in range(1, tries + 1):
        if attempt(i) is not None:
            print(f"CAUGHT on attempt {i}; lldb output in {OUT}_lldb_{i}.txt")
            break
    subprocess.run(["pkill", "-f", "qemu-system-aarch64"], capture_output=True)
