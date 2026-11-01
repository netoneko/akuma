#!/usr/bin/env python3 -I
"""fp_metal.py K0 SHA - Taoxichuan's self-hosting fixed point on the ryzen under Akuma's own hypervisor.

K0 is a Taoxichuan kernel built on the Mac from SHA (`--features no-tests,trace-err`); SHA must be pushed, because
the guest fetches it from GitHub. For each generation n = 1, 2: boot K(n-1) as a `herd vm-run` guest on a fresh copy
of the toolchain image, build SHA inside it, pull K(n) out of the guest's disk with `debugfs dump` (the guest sshd's
large output is lossy), and finally require K1 == K2 byte for byte (docs/archive/TAOXICHUAN_FIXED_POINT.md).

The build is started detached inside the guest and polled with short ssh sessions: one long session to this guest
dies after a few minutes (the procedure doc's pitfall). The box runs one guest at a time (a second spec is refused
with EBUSY), so stop herd's `vm-dev` first (`herd stop vm-dev`) and start it again after. The guest is killed by
its recorded pid only; its forward is FP_PORT (default 2225).

Environment: FP_HOST (192.168.1.159), FP_KEY (ssh key for the box's root and the guest image, default
~/.ssh/id_ed25519), FP_IMAGE (/root/tx/rs.goose.orig, the toolchain image), FP_VCPUS (4), FP_RAM (3072),
FP_GEN_TIMEOUT (seconds per build, 3600).
"""
import os
import subprocess
import sys
import time

HOST = os.environ.get("FP_HOST", "192.168.1.159")
KEY = os.environ.get("FP_KEY", os.path.expanduser("~/.ssh/id_ed25519"))
IMAGE = os.environ.get("FP_IMAGE", "/root/tx/rs.goose.orig")
PORT = os.environ.get("FP_PORT", "2225")
VCPUS = os.environ.get("FP_VCPUS", "4")
RAM = os.environ.get("FP_RAM", "3072")
GEN_TIMEOUT = int(os.environ.get("FP_GEN_TIMEOUT", "3600"))
WORK = "/root/tx/fp"
GUEST_K = "/tmp/tgt/x86_64-unknown-none/release/taoxichuan"
OPTS = ["-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null", "-o", "BatchMode=yes",
        "-o", "ConnectTimeout=15", "-o", "LogLevel=ERROR", "-i", KEY]


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, flush=True)


def host(cmd, timeout=300, data=None):
    r = subprocess.run(["ssh", *OPTS, "-p", "2222", f"root@{HOST}", cmd], input=data,
                       capture_output=True, timeout=timeout)
    return r.returncode, r.stdout.decode(errors="replace") + r.stderr.decode(errors="replace")[-400:]


def guest(cmd, timeout=90):
    try:
        r = subprocess.run(["ssh", *OPTS, "-p", PORT, f"root@{HOST}", cmd], capture_output=True, text=True,
                           timeout=timeout)
        return r.returncode, r.stdout
    except subprocess.TimeoutExpired:
        return 255, ""


def fp_script(sha):
    # Sent on the command line (ssh stdin into the guest is lossy); no single quotes inside.
    return f"""export HOME=/root PATH=/usr/local/rust/bin:/usr/bin:/bin GIT_PAGER=cat TERM=dumb
rm -rf /tmp/gh /tmp/tgt; mkdir -p /tmp/gh; cd /tmp/gh
echo START $(date +%s) > /tmp/fp.status
git init -q . && git remote add origin https://github.com/netoneko/akuma.git
git fetch -q --depth 1 --filter=blob:none origin {sha} || {{ echo FAIL fetch >> /tmp/fp.status; exit 1; }}
git sparse-checkout set --no-cone /Cargo.toml /Cargo.lock /build.rs /linker.ld /.cargo /rust-toolchain.toml /src /amd64 /taoxichuan /crates
git checkout -q FETCH_HEAD || {{ echo FAIL checkout >> /tmp/fp.status; exit 1; }}
echo CHECKED-OUT $(date +%s) >> /tmp/fp.status
cargo build --release --target x86_64-unknown-none -p taoxichuan --features no-tests,trace-err -j4 --target-dir /tmp/tgt > /tmp/fp.log 2>&1
echo BUILD rc=$? $(date +%s) >> /tmp/fp.status
md5sum {GUEST_K} >> /tmp/fp.status
sync
echo DONE >> /tmp/fp.status
"""


def stop_vm():
    host(f"[ -f {WORK}/vm.pid ] && kill $(cat {WORK}/vm.pid) 2>/dev/null; rm -f {WORK}/vm.pid; sleep 3; true")


def generation(n, kernel_local, sha):
    log(f"gen {n}: shipping {kernel_local}")
    data = open(kernel_local, "rb").read()
    k = f"{WORK}/k{n - 1}"
    rc, out = host(f"mkdir -p {WORK} && cat > {k}.new && chmod +x {k}.new && mv {k}.new {k} && md5sum {k}", data=data)
    assert rc == 0, out
    log(f"gen {n}: K{n - 1} on the box: {out.split()[0]}")
    stop_vm()
    rc, out = host(f"cp {IMAGE} {WORK}/disk.img.new && mv {WORK}/disk.img.new {WORK}/disk.img && echo ok", timeout=900)
    assert "ok" in out, out
    spec = (f"type=kvm\nkernel={k}\ncmdline=init=/bin/sshd initargs=--port,22 hostrelay={HOST} epoch=$(date +%s)\n"
            f"ram_mib={RAM}\nvcpus={VCPUS}\ndisk={WORK}/disk.img\ndisk_ram=0\noutbound=1\nforward={PORT}:22\n")
    rc, out = host(f"cat > {WORK}/vm.spec <<SPEC\n{spec}SPEC\n"
                   f"setsid /bin/herd vm-run {WORK}/vm.spec </dev/null > {WORK}/vm{n}.log 2>&1 & echo $! > {WORK}/vm.pid; "
                   f"sleep 2; cat {WORK}/vm.pid")
    log(f"gen {n}: guest started (host pid {out.strip()})")
    t0 = time.time()
    while time.time() - t0 < 300:
        rc, early = host(f"grep -a 'refused the spec' {WORK}/vm{n}.log", timeout=30)
        if rc == 0:
            raise SystemExit(f"gen {n}: {early.strip()} - is another guest (herd's vm-dev) holding the hypervisor?")
        rc, out = guest("uname -a", timeout=30)
        # Any completed command is readiness (`uname` is glue's: it says "Akuma", not "taoxichuan").
        if rc == 0 and out.strip():
            break
        time.sleep(5)
    else:
        rc, tail = host(f"tail -c 1500 {WORK}/vm{n}.log")
        raise SystemExit(f"gen {n}: guest never answered ssh\n{tail}")
    log(f"gen {n}: guest up: {out.strip()}")
    rc, out = guest(f"cat > /tmp/fp.sh <<'FPEOF'\n{fp_script(sha)}FPEOF\nsetsid sh /tmp/fp.sh </dev/null >/dev/null 2>&1 &\necho started")
    assert "started" in out, (rc, out)
    t0 = time.time()
    status = ""
    while time.time() - t0 < GEN_TIMEOUT:
        time.sleep(30)
        rc, status = guest("cat /tmp/fp.status", timeout=60)
        if "DONE" in status or "FAIL" in status:
            break
    log(f"gen {n}: status after {int(time.time() - t0)} s:\n{status.strip()}")
    if "BUILD rc=0" not in status:
        rc, tail = guest("tail -20 /tmp/fp.log", timeout=60)
        raise SystemExit(f"gen {n}: build failed\n{tail}")
    lines = status.split()
    guest_md5 = next(lines[i - 1] for i, w in enumerate(lines) if w == GUEST_K)
    stop_vm()
    out_k = f"{WORK}/k{n}"
    rc, out = host(f"rm -f {out_k}; debugfs -R 'dump {GUEST_K} {out_k}' {WORK}/disk.img >/dev/null 2>&1; md5sum {out_k}; "
                   f"grep -acE 'Fault|PANIC|BADFRAME|stuck|never ack' {WORK}/vm{n}.log")
    host_md5 = out.split()[0]
    log(f"gen {n}: K{n} md5 in guest {guest_md5}, pulled {host_md5}; fault-ish console lines: {out.split()[-1]}")
    assert host_md5 == guest_md5, "debugfs pulled a different file than the guest built"
    local = os.path.join(os.path.dirname(os.path.abspath(kernel_local)), f"fp-k{n}")
    with open(local, "wb") as f:
        f.write(subprocess.run(["ssh", *OPTS, "-p", "2222", f"root@{HOST}", f"cat {out_k}"],
                               capture_output=True, check=True).stdout)
    return local, host_md5


def main(argv):
    if len(argv) != 2:
        print(__doc__)
        return 2
    k0, sha = argv
    log(f"fixed point of {sha}, K0 {k0}, {VCPUS} vCPUs, {RAM} MiB")
    k1, m1 = generation(1, k0, sha)
    k2, m2 = generation(2, k1, sha)
    log(f"K1 {m1}\nK2 {m2}\n" + ("FIXED POINT: K1 == K2" if m1 == m2 else "NOT A FIXED POINT: K1 != K2"))
    return 0 if m1 == m2 else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
