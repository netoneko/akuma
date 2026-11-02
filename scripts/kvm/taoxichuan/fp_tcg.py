#!/usr/bin/env python3 -I
"""fp_tcg.py K0 SHA - Taoxichuan's self-hosting fixed point under QEMU TCG on the Mac (no ryzen, no KVM).

The procedure of `fp_pop.py` on `tcg_rig.py`'s machine (microvm, `-cpu max`, x86_64 emulated on the Mac's arm64):
K0 is a Taoxichuan kernel built on the Mac from SHA (`--features no-tests,trace-err`). For each generation
n = 1, 2: boot K(n-1) on a fresh overlay of the staged image, build SHA inside it, stop the guest, and pull K(n)
out of the overlay (`qemu-img convert` to raw + `debugfs dump`; the guest sshd's large output is lossy). Passes
when K1 == K2 byte for byte (docs/archive/TAOXICHUAN_FIXED_POINT.md).

The toolchain image has neither git nor a resolver, so the inputs are staged instead of fetched: the Mac writes
`git archive SHA` of the same sparse paths the KVM runs check out, plus a CARGO_HOME filled by `cargo fetch
--locked`, as one tar onto an APFS clone of FP_DISK (grown to FP_DISK_MIB first; FP_DISK itself is never
written), and the guest extracts it to the same absolute paths (`/tmp/gh`, `/root/.cargo`) and builds `--offline`.
The build runs detached in the guest and is polled with short ssh sessions; the guest is stopped by the rig's
pidfile only.

Environment: FP_DISK (the Rust toolchain image, required), FP_KEY (ssh key for its authorized_keys, required),
FP_RUN (default $TMPDIR/tx-fp-tcg), FP_PORT (2246), FP_SMP (4), FP_RAM (3072), FP_DISK_MIB (4096),
FP_GEN_TIMEOUT (seconds per build, 21600), FP_DEBUGFS (default: debugfs on PATH, else Homebrew's e2fsprogs).
"""
import hashlib
import os
import shutil
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
RIG = os.path.join(HERE, "tcg_rig.py")
RUN = os.environ.get("FP_RUN", os.path.join(os.environ.get("TMPDIR", "/tmp"), "tx-fp-tcg"))
PORT = os.environ.get("FP_PORT", "2246")
SMP = os.environ.get("FP_SMP", "4")
RAM = os.environ.get("FP_RAM", "3072")
DISK_MIB = int(os.environ.get("FP_DISK_MIB", "4096"))
GEN_TIMEOUT = int(os.environ.get("FP_GEN_TIMEOUT", "21600"))
GUEST_K = "/tmp/tgt/x86_64-unknown-none/release/taoxichuan"
STAGE_TAR = "/root/fp-stage.tar"
SPARSE = ["Cargo.toml", "Cargo.lock", "build.rs", "linker.ld", ".cargo", "rust-toolchain.toml", "src", "amd64",
          "taoxichuan", "osaka", "crates"]
E2BIN = "/opt/homebrew/opt/e2fsprogs/sbin"


def e2(tool):
    return os.environ.get("FP_DEBUGFS") if tool == "debugfs" and os.environ.get("FP_DEBUGFS") else \
        shutil.which(tool) or os.path.join(E2BIN, tool)


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, flush=True)


def md5(path):
    return hashlib.md5(open(path, "rb").read()).hexdigest()


def rig(*args, kernel=None, timeout=600):
    env = dict(os.environ, TX_RUN=os.path.join(RUN, "vm"), TX_PORT=PORT, TX_SMP=SMP, TX_MEM=RAM,
               TX_DISK=os.path.join(RUN, "base.img"), TX_KEY=os.environ["FP_KEY"])
    env.pop("TX_INIT", None)
    if kernel:
        env["TX_KERNEL"] = kernel
    try:
        r = subprocess.run([sys.executable, "-I", RIG, *args], env=env, capture_output=True, text=True, timeout=timeout)
        return r.returncode, r.stdout + r.stderr
    except subprocess.TimeoutExpired:
        return 124, ""


def guest(cmd, timeout=90):
    rc, out = rig("run", cmd, str(timeout), timeout=timeout + 30)
    # tcg_rig prints "rc=N" first, then the command's stdout.
    return rc, out.split("\n", 1)[1] if out.startswith("rc=") and "\n" in out else out


def stage(sha):
    """The staged image: an APFS clone of FP_DISK, grown, carrying SHA's sources and their crates as one tar."""
    st = os.path.join(RUN, "stage")
    shutil.rmtree(st, ignore_errors=True)
    os.makedirs(os.path.join(st, "tmp", "gh"))
    arch = subprocess.run(["git", "-C", REPO, "archive", "--format=tar", sha, *SPARSE], capture_output=True, check=True)
    subprocess.run(["tar", "xf", "-", "-C", os.path.join(st, "tmp", "gh")], input=arch.stdout, check=True)
    cargo_home = os.path.join(st, "root", ".cargo")
    subprocess.run(["cargo", "fetch", "--locked", "--manifest-path", os.path.join(st, "tmp", "gh", "Cargo.toml")],
                   env=dict(os.environ, CARGO_HOME=cargo_home), check=True, capture_output=True)
    tar = os.path.join(RUN, "fp-stage.tar")
    subprocess.run(["tar", "--no-xattrs", "--no-mac-metadata", "-cf", tar, "-C", st, "tmp", "root"], check=True,
                   env=dict(os.environ, COPYFILE_DISABLE="1"))
    base = os.path.join(RUN, "base.img")
    if os.path.exists(base):
        os.remove(base)
    subprocess.run(["cp", "-c", os.environ["FP_DISK"], base], check=True)
    os.truncate(base, DISK_MIB << 20)
    subprocess.run([e2("e2fsck"), "-fy", base], capture_output=True)
    subprocess.run([e2("resize2fs"), base], check=True, capture_output=True)
    subprocess.run([e2("debugfs"), "-w", "-R", f"write {tar} {STAGE_TAR}", base], check=True, capture_output=True)
    log(f"staged {os.path.getsize(tar) >> 20} MiB of sources + crates onto {base} ({DISK_MIB} MiB)")


def fp_script(sha):
    # Sent on the command line (ssh stdin into the guest is lossy); no single quotes inside. The staged tree has no
    # `.git`, so the build identity comes from AKUMA_SHA_OVERRIDE, spelled as the KVM runs' in-guest `git rev-parse
    # --short` spells it (7 digits).
    return f"""export HOME=/root PATH=/usr/local/rust/bin:/usr/bin:/bin TERM=dumb AKUMA_SHA_OVERRIDE={sha[:7]}
rm -rf /tmp/gh /tmp/tgt /root/.cargo
echo START $(date +%s) > /tmp/fp.status
tar xf {STAGE_TAR} -C / || {{ echo FAIL unpack >> /tmp/fp.status; exit 1; }}
echo CHECKED-OUT $(date +%s) >> /tmp/fp.status
cd /tmp/gh
cargo build --offline --release --target x86_64-unknown-none -p taoxichuan --features no-tests,trace-err -j4 --target-dir /tmp/tgt > /tmp/fp.log 2>&1
echo BUILD rc=$? $(date +%s) >> /tmp/fp.status
md5sum {GUEST_K} >> /tmp/fp.status
sync
echo DONE >> /tmp/fp.status
"""


def generation(n, kernel, sha):
    log(f"gen {n}: booting K{n - 1} ({md5(kernel)})")
    rc, out = rig("boot", "fresh", kernel=kernel, timeout=400)
    assert rc == 0, out
    rc, out = guest("uname -a", 60)
    log(f"gen {n}: guest up: {out.strip()}")
    rc, out = guest(f"cat > /tmp/fp.sh <<'FPEOF'\n{fp_script(sha)}FPEOF\nsetsid sh /tmp/fp.sh </dev/null >/dev/null 2>&1 &\necho started")
    assert "started" in out, (rc, out)
    t0 = time.time()
    status = ""
    while time.time() - t0 < GEN_TIMEOUT:
        time.sleep(60)
        rc, s = guest("cat /tmp/fp.status", 90)
        status = s if rc == 0 else status
        if "DONE" in status or "FAIL" in status:
            break
    log(f"gen {n}: status after {int(time.time() - t0)} s:\n{status.strip()}")
    if "BUILD rc=0" not in status:
        rc, tail = guest("tail -20 /tmp/fp.log", 90)
        raise SystemExit(f"gen {n}: build failed\n{tail}")
    words = status.split()
    guest_md5 = next(words[i - 1] for i, w in enumerate(words) if w == GUEST_K)
    rig("stop")
    raw = os.path.join(RUN, "pull.raw")
    out_k = os.path.join(RUN, f"fp-tcg-k{n}")
    for p in (raw, out_k):
        if os.path.exists(p):
            os.remove(p)
    subprocess.run(["qemu-img", "convert", "-O", "raw", os.path.join(RUN, "vm", "overlay.qcow2"), raw], check=True)
    subprocess.run([e2("debugfs"), "-R", f"dump {GUEST_K} {out_k}", raw], check=True, capture_output=True)
    os.remove(raw)
    pulled = md5(out_k)
    faults = sum(1 for line in open(os.path.join(RUN, "vm", "console.log"), "rb")
                 if any(m in line for m in (b"Fault", b"PANIC", b"BADFRAME", b"stuck", b"never ack")))
    log(f"gen {n}: K{n} md5 in guest {guest_md5}, pulled {pulled}, {os.path.getsize(out_k)} bytes; "
        f"fault-ish console lines: {faults}")
    assert pulled == guest_md5, "debugfs pulled a different file than the guest built"
    return out_k, pulled


def main(argv):
    if len(argv) != 2 or not os.environ.get("FP_DISK") or not os.environ.get("FP_KEY"):
        print(__doc__)
        return 2
    k0, sha = os.path.abspath(argv[0]), argv[1]
    try:
        sha = subprocess.run(["git", "-C", REPO, "rev-parse", "--verify", "-q", f"{sha}^{{commit}}"],
                             capture_output=True, text=True, check=True).stdout.strip()
    except subprocess.CalledProcessError:
        print(f"fp_tcg: {sha} is not a commit here", file=sys.stderr)
        return 2
    os.makedirs(RUN, exist_ok=True)
    log(f"fixed point of {sha} under TCG, K0 {k0}, smp={SMP}, {RAM} MiB")
    stage(sha)
    k1, m1 = generation(1, k0, sha)
    k2, m2 = generation(2, k1, sha)
    rig("stop")
    log(f"K1 {m1}\nK2 {m2}\n" + ("FIXED POINT: K1 == K2" if m1 == m2 else "NOT A FIXED POINT: K1 != K2"))
    return 0 if m1 == m2 else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
