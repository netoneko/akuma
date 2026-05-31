#!/usr/bin/env python3
"""Boot Akuma at several RAM sizes and compile a program at each, to characterize
the kernel/user VA-split / identity-map cap (docs/COW_OPTIMIZATIONS.md memory work).

Small sizes use tcc (cheap) to compile hello.c; larger sizes use rustc on hello.rs.
For each size: boot, wait for SSH, compile, run the binary, then record whether it
crashed and any fault FAR + free-RAM. Writes a summary to logs/split_summary.txt.
"""
import subprocess, time, os, signal, re, sys

ROOT = "/Users/netoneko/github.com/netoneko/akuma"
os.chdir(ROOT)

# (MEMORY, compiler, compile_cmd, run_cmd)
TC = "tcc /akuma-playground/hello.c -o /tmp/hc"
TR = "rustc -C linker=clang -o /tmp/hr /akuma-playground/hello.rs"
MATRIX = [
    ("256M",  "tcc",   TC, "exec /tmp/hc"),
    ("512M",  "tcc",   TC, "exec /tmp/hc"),
    ("1024M", "tcc",   TC, "exec /tmp/hc"),
    ("2048M", "rustc", TR, "exec /tmp/hr"),
    ("3584M", "rustc", TR, "exec /tmp/hr"),
    ("4096M", "rustc", TR, "exec /tmp/hr"),
    ("6144M", "rustc", TR, "exec /tmp/hr"),
]

def kill_qemu():
    subprocess.run(["pkill", "-f", "qemu-system-aarch64"], capture_output=True)
    time.sleep(2)

def ssh(cmd, timeout):
    try:
        r = subprocess.run(["ssh","-o","StrictHostKeyChecking=no","-o","ConnectTimeout=10",
                            "-p","2222","root@localhost",cmd],
                           capture_output=True, text=True, timeout=timeout)
        return r.returncode, r.stdout, r.stderr
    except subprocess.TimeoutExpired:
        return -1, "TIMEOUT", ""

def qemu_alive():
    return subprocess.run(["pgrep","-f","qemu-system-aarch64"],capture_output=True).returncode == 0

def wait_boot(log, timeout=240):
    t0 = time.time()
    seen_alive = False
    while time.time() - t0 < timeout:
        try:
            with open(log) as f:
                if "SSH Server] Listening" in f.read():
                    return True
        except FileNotFoundError:
            pass
        alive = qemu_alive()
        if alive:
            seen_alive = True
        elif seen_alive:
            # qemu was up and is now gone => it died/exited before SSH
            return False
        time.sleep(2)
    return False

def scan_log_for_fault(log):
    """Return (sigsegv_count, first_far, ram_summary)."""
    far = None
    segv = 0
    ram = None
    try:
        with open(log) as f:
            txt = f.read()
    except FileNotFoundError:
        return 0, None, None
    # real SIGSEGV (exclude help/template lines with 'N'/'Xs' placeholders)
    for m in re.finditer(r"Process \d+ \([^)]+\) SIGSEGV after [\d.]+s", txt):
        segv += 1
    fm = re.search(r"FAR=0x([0-9a-fA-F]+).*SIGSEGV", txt, re.S)
    m2 = re.search(r"Data abort from EL0 at FAR=0x([0-9a-fA-F]+)", txt)
    if m2: far = m2.group(1)
    # last RAM line
    rams = re.findall(r"RAM: (\d+)/(\d+)MB free", txt)
    if rams: ram = rams[-1]
    # kernel panic / EL1 fault
    el1 = len(re.findall(r"EL1|kernel panic|KERNEL PANIC|Synchronous.*EL1", txt))
    return segv, far, (ram, el1)

results = []
for mem, comp, ccmd, rcmd in MATRIX:
    kill_qemu()
    log = f"logs/split_{mem}.log"
    if os.path.exists(log): os.remove(log)
    env = dict(os.environ, MEMORY=mem)
    qp = subprocess.Popen(["cargo","run","--release"], stdout=open(log,"w"),
                          stderr=subprocess.STDOUT, env=env, preexec_fn=os.setsid)
    # Let cargo start and exec qemu before wait_boot's liveness check kicks in.
    for _ in range(15):
        if qemu_alive(): break
        time.sleep(1)
    booted = wait_boot(log, timeout=240)
    if not booted:
        results.append((mem, comp, "BOOT-FAIL", "", "", ""))
        kill_qemu()
        continue
    time.sleep(3)
    ct0 = time.time()
    crc, cout, cerr = ssh(ccmd, timeout=240)
    cdt = time.time() - ct0
    compiled = "[exit code: -11]" not in cout and "TIMEOUT" not in cout
    # Run the produced binary. The Akuma SSH session returns after ~60s, but a
    # large rustc compile detaches and keeps linking (~110s+), so the binary may
    # not exist yet. Poll exec until it runs or we give up.
    ran_ok = False
    rout = ""
    for _ in range(18):  # up to ~180s
        rrc, rout, rerr = ssh(rcmd, timeout=60)
        if "Hello" in rout:
            ran_ok = True
            break
        # Not-ready states: binary missing yet, or a partial ELF still being
        # written by the linker ("Invalid ELF magic" as the file grows).
        if ("fs error" in rout or "Failed to stat" in rout or "Not found" in rout
                or "Invalid ELF magic" in rout or "Failed to load ELF" in rout):
            time.sleep(10)
            continue
        # produced some other output (e.g. a real crash) — stop polling
        break
    time.sleep(2)
    segv, far, raminfo = scan_log_for_fault(log)
    status = "OK" if (compiled and ran_ok and segv == 0) else ("SIGSEGV" if segv else "FAIL")
    results.append((mem, comp, status, f"{cdt:.0f}s", far or "-",
                    f"segv={segv} run={'Hello' if ran_ok else 'no'} ram={raminfo}"))
    kill_qemu()

kill_qemu()
# Summary
lines = ["MEM      COMP   STATUS    CTIME  FAULT_FAR    DETAIL"]
for r in results:
    lines.append(f"{r[0]:<8} {r[1]:<6} {r[2]:<9} {r[3]:<6} {r[4]:<12} {r[5]}")
summary = "\n".join(lines)
print(summary)
with open("logs/split_summary.txt","w") as f:
    f.write(summary + "\n")
