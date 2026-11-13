#!/usr/bin/env python3 -I
"""tcg_rig.py - Taoxichuan under QEMU microvm TCG on the Mac (no ryzen, no KVM).

  tcg_rig.py build                 cargo build the kernel (no-tests,trace-err) into TX_TARGET
  tcg_rig.py boot [fresh]          boot TX_DISK through a qcow2 overlay; waits for an ssh round trip
  tcg_rig.py run 'CMD' [TIMEOUT]   run CMD in the guest over ssh (prints rc, stdout, stderr tail)
  tcg_rig.py stop                  stop this rig's QEMU (by its pidfile only - never by pattern)
  tcg_rig.py battery [goose]       the regression battery on the running guest; `goose` adds the GLM prompt
                                   (needs goose + its config on the image); exits 1 if any line differs from BATTERY
  tcg_rig.py battery toolchain     on the Rust toolchain image: rustc and a cargo build with a build script, with
                                   no LD_LIBRARY_PATH (the loader must find librustc_driver through $ORIGIN)

Environment: TX_DISK (the base ext2 image, required for boot; the guest never writes it - writes land in
TX_RUN/overlay.qcow2, and `boot fresh` discards them), TX_KEY (ssh private key for the image's
authorized_keys), TX_RUN (default $TMPDIR/tx-tcg), TX_PORT (default 2244), TX_MEM (3072), TX_SMP (1; above 1 also puts `smp=N` on the command line),
TX_TARGET (cargo target dir, default target/), TX_KERNEL (default TX_TARGET/x86_64-unknown-none/release/taoxichuan; any kernel built on akuma-tokio-kernel boots the same way),
TX_APPEND (extra cmdline), TX_INIT (default `init=/bin/sshd initargs=--port,22`; with another init, `boot` cannot
wait for ssh - read the console).

The cmdline carries `epoch=<host unix seconds>`: microvm has no RTC, and without the anchor the guest's clock
reads 1970 and every TLS client refuses certificates as "not valid yet" (rustls says so; busybox ssl_client
only says "No error information"). Stage files onto TX_DISK with `debugfs -w` only while the rig is stopped,
then `boot fresh`. Console: TX_RUN/console.log (read it with `grep -a`).
"""
import os
import subprocess
import sys
import time

REPO = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".."))
RUN = os.environ.get("TX_RUN", os.path.join(os.environ.get("TMPDIR", "/tmp"), "tx-tcg"))
PORT = os.environ.get("TX_PORT", "2244")
TARGET = os.environ.get("TX_TARGET", os.path.join(REPO, "target"))
KERNEL = os.environ.get("TX_KERNEL", os.path.join(TARGET, "x86_64-unknown-none", "release", "taoxichuan"))
OVERLAY = os.path.join(RUN, "overlay.qcow2")
PIDFILE = os.path.join(RUN, "qemu.pid")


def ssh(cmd, timeout=120):
    key = os.environ.get("TX_KEY")
    args = ["ssh", "-p", PORT, "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
            "-o", "BatchMode=yes", "-o", "ConnectTimeout=20", "-o", "LogLevel=ERROR"]
    if key:
        args += ["-i", key]
    try:
        r = subprocess.run(args + ["root@127.0.0.1", cmd], capture_output=True, text=True, timeout=timeout)
        return r.returncode, r.stdout, r.stderr
    except subprocess.TimeoutExpired:
        return 124, "", "timeout"


def stop():
    if os.path.exists(PIDFILE):
        try:
            os.kill(int(open(PIDFILE).read().strip()), 15)
        except (ValueError, ProcessLookupError):
            pass
        os.remove(PIDFILE)
        time.sleep(1)


def build():
    env = dict(os.environ, CARGO_TARGET_DIR=TARGET)
    subprocess.run(["cargo", "build", "-p", "taoxichuan", "--target", "x86_64-unknown-none", "--release",
                    "--features", "no-tests,trace-err"], cwd=REPO, env=env, check=True)
    print(KERNEL)


def boot(fresh):
    disk = os.environ.get("TX_DISK")
    if not disk or not os.path.exists(disk):
        sys.exit("TX_DISK must name the base ext2 image")
    if not os.path.exists(KERNEL):
        sys.exit(f"no kernel at {KERNEL}: run '{sys.argv[0]} build'")
    os.makedirs(RUN, exist_ok=True)
    stop()
    if fresh or not os.path.exists(OVERLAY):
        if os.path.exists(OVERLAY):
            os.remove(OVERLAY)
        subprocess.run(["qemu-img", "create", "-q", "-f", "qcow2", "-b", os.path.abspath(disk), "-F", "raw", OVERLAY],
                       check=True)
    init = os.environ.get("TX_INIT", "init=/bin/sshd initargs=--port,22")
    append = os.environ.get("TX_APPEND", "")
    smp = os.environ.get("TX_SMP", "1")
    # QEMU's `-smp` only gives the guest the vCPUs; the kernel starts its application
    # processors from `smp=N` on the command line. Without the token a TX_SMP=4 guest ran on
    # one core while reporting nothing amiss.
    if smp != "1" and "smp=" not in append:
        append = f"smp={smp} {append}"
    cl = f"{init} epoch={int(time.time())} {append}".strip()
    for i in range(4):
        cl += f" virtio_mmio.device=512@{0xfeb00000 + i * 512:#x}:{5 + i}"
    open(os.path.join(RUN, "console.log"), "w").close()
    subprocess.run(["qemu-system-x86_64", "-M", "microvm,accel=tcg", "-cpu", "max",
                    "-smp", os.environ.get("TX_SMP", "1"), "-m", os.environ.get("TX_MEM", "3072"),
                    "-kernel", KERNEL, "-append", cl, "-nodefaults", "-no-reboot", "-display", "none",
                    "-serial", f"file:{RUN}/console.log",
                    "-drive", f"file={OVERLAY},format=qcow2,if=none,id=d0",
                    # QEMU's virtio-mmio defaults to the legacy (v1) transport; the kernel's
                    # in-tree drivers speak only v2, as Firecracker and kvm0 do.
                    "-global", "virtio-mmio.force-legacy=false",
                    "-device", "virtio-blk-device,drive=d0,bus=virtio-mmio-bus.0",
                    "-netdev", f"user,id=n0,hostfwd=tcp:127.0.0.1:{PORT}-:22",
                    "-device", "virtio-net-device,netdev=n0,bus=virtio-mmio-bus.1",
                    "-pidfile", PIDFILE, "-daemonize"], check=True)
    if "TX_INIT" in os.environ:
        print(f"BOOTED (no ssh wait with TX_INIT) console={RUN}/console.log")
        return
    t0 = time.time()
    while time.time() - t0 < 180:
        rc, out, _ = ssh("echo ready", 40)
        if rc == 0 and "ready" in out:
            print(f"BOOT OK ready_in={time.time() - t0:.0f}s port={PORT} console={RUN}/console.log")
            return
        time.sleep(3)
    sys.exit(f"BOOT FAIL no ssh round trip in 180 s (console: {RUN}/console.log)")


# (name, command, timeout, expected stdout). The expected values are the 2026-11-01 baseline; `apk add git`'s
# "9 errors" are pre-existing trigger-script failures (exit 127), recorded in the 2026-10-30 battery too.
BATTERY = [
    ("quick", "echo q", 40, "q"),
    ("F1", 'sh -c "echo x > /tmp/f; cat /tmp/f; wc -c /tmp/f"', 60, "x\n2 /tmp/f"),
    ("PK1", "yes | head -n 20000 | wc -l", 90, "20000"),
    ("fs", "rm -rf /tmp/l /tmp/d; ln -s /tmp/f /tmp/l; readlink /tmp/l; mkdir /tmp/d; rmdir /tmp/d && echo rmdir-ok",
     60, "/tmp/f\nrmdir-ok"),
    ("append", "printf a > /tmp/ap; printf b >> /tmp/ap; printf c >> /tmp/ap; cat /tmp/ap; echo; tail -c 2 /tmp/ap",
     60, "abc\nbc"),
    # A session shell's dup2 over its inherited copy of sshd's socket must not close sshd's connection.
    ("dup2-sock", "exec 4</etc/hostname; echo after; cat <&4 >/dev/null; echo ok", 60, "after\nok"),
    ("pipeline", "seq 1 500 | sort -rn | head -n 1", 60, "500"),
    ("apk", "apk add git 2>&1 | tail -1", 300, "9 errors; 15 MiB in 19 packages"),
    ("git", "git --version", 60, "git version 2.54.0"),
]
# The toolchain image's PATH, and deliberately no LD_LIBRARY_PATH: rustc's RUNPATH is `$ORIGIN/../lib`, which musl's
# loader expands through readlink("/proc/self/exe") - a kernel that cannot answer that fails here and nowhere else.
RUSTENV = "export HOME=/root PATH=/usr/local/rust/bin:/usr/bin:/bin; "
TOOLCHAIN = [
    ("rustc", RUSTENV + "rustc --version | cut -d' ' -f1", 120, "rustc"),
    ("cargo-build", RUSTENV + "rm -rf /tmp/demo && cd /tmp && cargo new --vcs none --bin demo >/dev/null 2>&1 && cd demo"
     ' && printf "fn main() { println!(\\"built {}\\", env!(\\"BS\\")); }\\n" > src/main.rs'
     ' && printf "fn main() { println!(\\"cargo:rustc-env=BS=ok\\"); }\\n" > build.rs'
     " && cargo build -j2 --offline >/dev/null 2>&1 && ./target/debug/demo", 1500, "built ok"),
]
GOOSE = ("goose", ". /etc/goose.env; cd /root; timeout 400 goose run --no-session -t "
         "'Reply with exactly the word PONG and nothing else. Do not use tools.' 2>&1 | tail -1", 460, "PONG")


def battery(extra):
    bad = 0
    lines = TOOLCHAIN if extra == "toolchain" else BATTERY + ([GOOSE] if extra == "goose" else [])
    for name, cmd, timeout, want in lines:
        rc, out, _ = ssh(cmd, timeout)
        got = out.strip()
        ok = rc == 0 and got == want
        bad += not ok
        print(f"{'PASS' if ok else 'FAIL'} {name}" + ("" if ok else f" rc={rc} got={got[-200:]!r}"))
    faults = sum(1 for line in open(os.path.join(RUN, "console.log"), "rb")
                 if b"[Fault]" in line or b"PANIC" in line)
    print(f"{'PASS' if faults == 0 else 'FAIL'} console faults={faults}")
    sys.exit(1 if bad or faults else 0)


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    cmd = sys.argv[1]
    if cmd == "build":
        build()
    elif cmd == "boot":
        boot(len(sys.argv) > 2 and sys.argv[2] == "fresh")
    elif cmd == "stop":
        stop()
    elif cmd == "battery":
        battery(sys.argv[2] if len(sys.argv) > 2 else "")
    elif cmd == "run":
        rc, out, err = ssh(sys.argv[2], int(sys.argv[3]) if len(sys.argv) > 3 else 120)
        print(f"rc={rc}")
        print(out[-6000:], end="")
        if err.strip():
            print("ERR:", err[-800:])
        sys.exit(rc)
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
