#!/usr/bin/env python3
"""Real bubblewrap on Akuma: the private-namespace regression suite.

Runs the cases that took `bwrap` from "can't read /proc/sys/kernel/overflowuid" to running codex's own sandbox
command line (`docs/archive/BWRAP_ON_BOXES.md`) against a booted amd64 guest, over ssh, and checks what each prints.

Prerequisites (a guest that has them; `docs/runbooks/debug-codex-on-akuma.md` § "Run the sandbox" has the recipe):
  * the amd64 kernel built with `sc-private-ns` (in `linux-extra-syscalls`, so in `default` and `headless`);
  * Alpine's `bubblewrap`, `libcap2` and `musl` (`/usr/bin/bwrap`, `/lib/libcap.so.2`, `/lib/ld-musl-x86_64.so.1`) and
    busybox at `/bin/busybox` (applet links `/usr/bin/true`, ...), with `/dev /proc /sys /run /tmp /work` as directories
    on the root filesystem (bwrap creates mount points under a read-only bind of `/`, and cannot create these there);
  * sshd running on the forwarded port with the key below authorized.

Optional: with codex at `/usr/local/bin/codex` and `/root/inner.sh` (the runbook's script), the codex inner stage cases
run too, otherwise they are skipped.

    scripts/bwrap_suite.py [--port 2222] [--key target/x86_64-unknown-none/release/amd64-ssh-test-key]

Exit status 0 when every case that ran passed. Never run it against a machine you care about: it creates `/work/f`.
"""
import argparse
import subprocess
import sys

BW = "/usr/bin/bwrap"
CODEX_SHAPE = (
    f"{BW} --ro-bind / / --dev /dev --proc /proc --unshare-user --unshare-pid --unshare-ipc --unshare-net "
    "--cap-drop ALL --"
)


def make_runner(port, key):
    def run(cmd, timeout=120):
        r = subprocess.run(
            ["ssh", "-i", key, "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
             "-o", "LogLevel=ERROR", "-o", "ConnectTimeout=10", "-p", str(port), "root@localhost", cmd],
            capture_output=True, text=True, timeout=timeout)
        return r.stdout + r.stderr
    return run


# (name, command, substrings that must appear, substrings that must not)
CASES = [
    ("a plain bind runs a command",
     f"{BW} --bind / / /bin/busybox echo hi", ["hi"], ["bwrap:"]),
    ("a read-only root refuses writes (EROFS)",
     f"{BW} --ro-bind / / --dev /dev --proc /proc /bin/busybox sh -c 'echo x > /tmp/f; echo rc=$?'",
     ["Read-only file system", "rc=1"], ["rc=0"]),
    ("--tmpfs / shows only what was bound in",
     f"{BW} --tmpfs / --ro-bind /bin /bin --ro-bind /lib /lib --proc /proc /bin/busybox ls /",
     ["bin", "lib", "proc"], ["etc", "usr"]),
    ("a writable bind reaches the host; the rest stays read-only",
     f"{BW} --ro-bind / / --bind /work /work --dev /dev --proc /proc /bin/busybox sh -c "
     "'echo ok > /work/f; cat /work/f; echo > /etc/x; echo rc=$?'",
     ["ok", "Read-only file system", "rc=1"], []),
    ("the old root is not reachable",
     f"{BW} --ro-bind / / --dev /dev --proc /proc /bin/busybox ls /oldroot",
     ["No such file or directory"], []),
    ("device nodes work by path",
     f"{BW} --ro-bind / / --dev /dev --proc /proc /bin/busybox sh -c "
     "'echo hi > /dev/null; echo null-ok; head -c 4 /dev/zero | wc -c'",
     ["null-ok", "4"], []),
    ("codex's flag set (user/pid/ipc/net namespaces, cap-drop ALL) runs",
     f"{CODEX_SHAPE} /bin/busybox sh -c 'echo codex-flags-ok'", ["codex-flags-ok"], ["bwrap:"]),
    ("the host's mounts are untouched afterwards",
     "cat /proc/self/mountinfo", [" / rw", "/proc"], ["/newroot", "/oldroot", "/dev/null", " ro,"]),
    ("the host sees what the sandbox wrote",
     "cat /work/f; rm -f /work/f", ["ok"], []),
]

# Needs the codex binary: its inner stage verifies capabilities and installs seccomp before exec'ing the command.
CODEX_CASES = [
    ("codex inner stage runs the command",
     "sh /root/inner.sh /bin/busybox echo inner-ok", ["inner-ok", "exit=0"], ["panicked"]),
    ("codex inner stage: the root is read-only",
     "sh /root/inner.sh /bin/busybox sh -c 'echo x > /tmp/g; echo rc=$?'",
     ["Read-only file system", "rc=1"], ["rc=0"]),
    ("codex inner stage: network is refused (seccomp)",
     "sh /root/inner.sh /bin/busybox sh -c '/bin/busybox nc -w 2 10.0.2.2 2222 </dev/null 2>&1 | head -c 80'",
     ["Operation not permitted"], ["SSH-2.0"]),
]


def check(run, name, cmd, want, unwanted):
    out = run(cmd)
    bad = [w for w in want if w not in out] + [f"unexpected {u!r}" for u in unwanted if u in out]
    print(("PASS " if not bad else "FAIL ") + name)
    if bad:
        print("     missing/unexpected:", bad)
        print("     output:", out.strip().replace("\n", "\n            ")[:600])
    return not bad


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=2222)
    ap.add_argument("--key", default="target/x86_64-unknown-none/release/amd64-ssh-test-key")
    args = ap.parse_args()
    run = make_runner(args.port, args.key)
    ok = all([check(run, *c) for c in CASES])
    if "No such file" not in run("ls /usr/local/bin/codex /root/inner.sh 2>&1"):
        ok = all([check(run, *c) for c in CODEX_CASES]) and ok
    else:
        print("SKIP codex inner stage cases (no /usr/local/bin/codex or /root/inner.sh)")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
