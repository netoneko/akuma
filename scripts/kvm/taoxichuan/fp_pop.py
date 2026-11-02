#!/usr/bin/env python3 -I
"""fp_pop.py K0 SHA - Taoxichuan's self-hosting fixed point on the ryzen under Pop's Linux KVM.

The same procedure as `fp_metal.py`, on the other host hypervisor: K0 is a Taoxichuan kernel built on the Mac from SHA
(`--features no-tests,trace-err`), and SHA must be pushed (the guest fetches it from GitHub by its full name). For each
generation n = 1, 2: boot K(n-1) with `/root/tx-pop.sh` on a fresh overlay of the Akuma root partition (which carries
the toolchain), build SHA inside it, stop the guest, and pull K(n) out of the overlay with `qemu-nbd --read-only` and
`debugfs dump` (the guest sshd's large output is lossy). Passes when K1 == K2 byte for byte
(docs/archive/TAOXICHUAN_FIXED_POINT.md).

The build runs detached in the guest and is polled with short ssh sessions. The guest is stopped by its pidfile only
(`tx-pop.sh stop`), never by pattern.

Environment: FP_RUN (/root/tx-fp, the guest's run dir on the ryzen), FP_SMP (4), FP_RAM (3072), FP_PORT (2226),
FP_INIT (/root/tx/sshd-fork-linuxabi, on the Akuma root), FP_NBD (/dev/nbd7), FP_GEN_TIMEOUT (seconds per build, 3600).
"""
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "utils"))
import hpbox  # noqa: E402

RUN = os.environ.get("FP_RUN", "/root/tx-fp")
SMP = os.environ.get("FP_SMP", "4")
RAM = os.environ.get("FP_RAM", "3072")
PORT = os.environ.get("FP_PORT", "2226")
INIT = os.environ.get("FP_INIT", "/root/tx/sshd-fork-linuxabi")
NBD = os.environ.get("FP_NBD", "/dev/nbd7")
GEN_TIMEOUT = int(os.environ.get("FP_GEN_TIMEOUT", "3600"))
TARGET = f"{RUN}/target"
KERNEL = f"{TARGET}/x86_64-unknown-none/release/taoxichuan"
GUEST_K = "/tmp/tgt/x86_64-unknown-none/release/taoxichuan"
POP = hpbox.RZ_ROOT[-1].split("@")[1]
SSH = ["ssh", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null", "-o", "BatchMode=yes",
       "-o", "ConnectTimeout=15", "-o", "LogLevel=ERROR", "-p", PORT, f"root@{POP}"]
ENV = f"TX_RUN={RUN} TX_TARGET={TARGET} TX_PORT={PORT} TX_SMP={SMP} TX_MEM={RAM} TX_APPEND=smp={SMP} TX_INIT={INIT}"


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, flush=True)


def host(cmd, timeout=300, data=None):
    r = subprocess.run(hpbox.RZ_ROOT + [cmd], input=data, capture_output=True, timeout=timeout)
    return r.returncode, r.stdout.decode(errors="replace") + r.stderr.decode(errors="replace")[-400:]


def guest(cmd, timeout=90):
    try:
        r = subprocess.run(SSH + [cmd], capture_output=True, text=True, timeout=timeout)
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
git sparse-checkout set --no-cone /Cargo.toml /Cargo.lock /build.rs /linker.ld /.cargo /rust-toolchain.toml /src /amd64 /taoxichuan /osaka /crates
git checkout -q FETCH_HEAD || {{ echo FAIL checkout >> /tmp/fp.status; exit 1; }}
echo CHECKED-OUT $(date +%s) >> /tmp/fp.status
cargo build --release --target x86_64-unknown-none -p taoxichuan --features no-tests,trace-err -j4 --target-dir /tmp/tgt > /tmp/fp.log 2>&1
echo BUILD rc=$? $(date +%s) >> /tmp/fp.status
md5sum {GUEST_K} >> /tmp/fp.status
sync
echo DONE >> /tmp/fp.status
"""


def stop_vm():
    host(f"{ENV} bash /root/tx-pop.sh stop; sleep 2; true")


def generation(n, kernel_local, sha):
    log(f"gen {n}: shipping {kernel_local}")
    data = open(kernel_local, "rb").read()
    rc, out = host(f"mkdir -p $(dirname {KERNEL}) && cat > {KERNEL}.new && chmod +x {KERNEL}.new && "
                   f"mv {KERNEL}.new {KERNEL} && md5sum {KERNEL}", data=data)
    assert rc == 0, out
    log(f"gen {n}: K{n - 1} on the box: {out.split()[0]}")
    stop_vm()
    rc, out = host(f"{ENV} bash /root/tx-pop.sh boot fresh")
    assert "guest up" in out, out
    log(f"gen {n}: {out.strip()}")
    t0 = time.time()
    while time.time() - t0 < 300:
        rc, out = guest("uname -a", timeout=30)
        if rc == 0 and out.strip():  # a completed command is readiness
            break
        time.sleep(5)
    else:
        rc, tail = host(f"tail -c 1500 {RUN}/console.log")
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
    out_k = f"{RUN}/k{n}"
    rc, out = host(f"rm -f {out_k}; modprobe nbd max_part=0; qemu-nbd --read-only -c {NBD} {RUN}/overlay.qcow2 && sleep 1 && "
                   f"debugfs -R 'dump {GUEST_K} {out_k}' {NBD} >/dev/null 2>&1; qemu-nbd -d {NBD} >/dev/null; md5sum {out_k}; "
                   f"grep -acE 'Fault|PANIC|BADFRAME|stuck|never ack' {RUN}/console.log", timeout=300)
    host_md5 = out.split()[0]
    log(f"gen {n}: K{n} md5 in guest {guest_md5}, pulled {host_md5}; fault-ish console lines: {out.split()[-1]}")
    assert host_md5 == guest_md5, f"debugfs pulled a different file than the guest built:\n{out}"
    local = os.path.join(os.path.dirname(os.path.abspath(kernel_local)), f"fp-pop-k{n}")
    with open(local, "wb") as f:
        f.write(subprocess.run(hpbox.RZ_ROOT + [f"cat {out_k}"], capture_output=True, check=True).stdout)
    return local, host_md5


def main(argv):
    if len(argv) != 2:
        print(__doc__)
        return 2
    k0, sha = argv
    try:
        sha = subprocess.run(["git", "rev-parse", "--verify", "-q", f"{sha}^{{commit}}"], capture_output=True,
                             text=True, check=True).stdout.strip()
    except subprocess.CalledProcessError:
        print(f"fp_pop: {sha} is not a commit here", file=sys.stderr)
        return 2
    log(f"fixed point of {sha} under Pop KVM, K0 {k0}, smp={SMP}, {RAM} MiB")
    k1, m1 = generation(1, k0, sha)
    k2, m2 = generation(2, k1, sha)
    stop_vm()
    log(f"K1 {m1}\nK2 {m2}\n" + ("FIXED POINT: K1 == K2" if m1 == m2 else "NOT A FIXED POINT: K1 != K2"))
    return 0 if m1 == m2 else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
