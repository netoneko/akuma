#!/usr/bin/env python3
"""Live regression suite for boxes on the amd64 kernel.

Needs a running amd64 guest with sshd (``INIT=/bin/sshd amd64/run.sh``) and no
network: it builds its own one-layer image from the guest's busybox. Usage::

    SSH_PORT=2290 scripts/box_amd64_suite.py            # default key/port below

Every check is a property that was once false on amd64
(``docs/archive/AKUMA_AMD64_BOXES_AUDIT.md``): a boxed process saw the host
filesystem; a second ``spawn_ext`` corrupted the heap; a boxed process that ran
in ring 3 for one tick triple-faulted; ``box use -i`` never saw its child exit;
``box grab`` took a transient child and wedged sshd; ``box close`` killed
nothing; ``box run`` lost its first lines.

ssh handshakes are slow under TCG, so the timeouts here are generous on purpose.
"""
import os
import subprocess
import sys

PORT = os.environ.get("SSH_PORT", "2290")
KEY = os.environ.get("SSH_KEY", "target/x86_64-unknown-none/release/amd64-ssh-test-key")

JAIL = "mkdir -p /jail/bin /jail/proc; cp /bin/busybox /jail/bin/; echo host > /outside.txt; echo in > /jail/inside.txt"
IMAGE = """
B=/bin/busybox; L=/var/lib/box/layers/sha256-aaaa
mkdir -p $L/bin $L/etc /var/lib/box/images/testimg /var/lib/box/containers
cp $B $L/bin/busybox
for a in sh ls cat echo sleep; do ln -sf busybox $L/bin/$a; done
echo from-image > $L/etc/motd
echo sha256:aaaa > /var/lib/box/images/testimg/layers
cat > /var/lib/box/images/testimg/oci-config.json <<'J'
{"config":{"Env":["PATH=/bin","FOO=bar"],"Cmd":["/bin/sh","-c","echo hello $FOO; cat /etc/motd; ls /"]}}
J
"""

failures = 0


def ssh(cmd, timeout=240):
    try:
        r = subprocess.run(
            ["ssh", "-i", KEY, "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
             "-o", "ConnectTimeout=120", "-p", PORT, "root@localhost", cmd],
            capture_output=True, text=True, timeout=timeout)
        return r.stdout
    except subprocess.TimeoutExpired as e:
        return (e.stdout or b"").decode(errors="replace") + "\n<TIMEOUT>"


def check(name, ok, detail=""):
    global failures
    print(("PASS  " if ok else "FAIL  ") + name + ("" if ok else f"\n      {detail!r}"))
    failures += 0 if ok else 1


def main():
    ssh(JAIL + "; " + IMAGE)

    # 1+2+3: the jail, exit codes, and three spawns in a row (heap / stack).
    for n in (1, 2, 3):
        ssh(f"/bin/box open s{n} --root /jail -d /bin/busybox sleep 300 >/dev/null 2>&1 </dev/null")
    out = ssh("/bin/box use s1 -i /bin/busybox ls /; echo rc=$?; "
              "/bin/box use s1 -i /bin/busybox cat /outside.txt; echo rc=$?; "
              "/bin/box use s1 -i /bin/busybox cat /inside.txt; echo rc=$?")
    check("box use sees the jail, not the host", "inside.txt" in out and "outside.txt" not in out.split("rc=")[0], out)
    check("the host's file is invisible inside the box", "can't open '/outside.txt'" in out, out)
    check("box use returns and propagates the exit code", "rc=1" in out and out.rstrip().endswith("rc=0"), out)
    ps = ssh("/bin/box ps")
    check("three boxes registered", all(f"s{n}" in ps for n in (1, 2, 3)), ps)

    # 4: grab takes the primary process, streams, and survives repetition.
    ssh("/bin/box open g1 --root /jail -d /bin/busybox sh -c "
        "'i=0; while [ $i -lt 600 ]; do echo tick$i; i=$((i+1)); /bin/busybox sleep 1; done' "
        ">/dev/null 2>&1 </dev/null")
    for i in range(4):
        out = ssh("timeout 6 /bin/box grab g1; echo done")
        check(f"box grab #{i + 1} streams the shell's output and sshd stays up",
              "tick" in out and "done" in out, out)

    # 5: close actually stops the processes.
    ssh("rm -f /jail/t2.txt; /bin/box open c1 --root /jail -d /bin/busybox sh -c "
        "'while :; do echo x >> /t2.txt; /bin/busybox sleep 1; done' >/dev/null 2>&1 </dev/null")
    out = ssh("/bin/busybox sleep 3; /bin/box close c1; /bin/busybox sleep 2; "
              "A=$(/bin/busybox wc -l < /jail/t2.txt); /bin/busybox sleep 4; "
              "B=$(/bin/busybox wc -l < /jail/t2.txt); echo $A $B; "
              "/bin/busybox cat /proc/boxes | /bin/busybox grep -c c1")
    lines = out.split()[-3:]   # "<before> <after> <grep -c c1 in /proc/boxes>"
    check("box close stops the container (file stops growing)",
          len(lines) == 3 and lines[0] == lines[1] and lines[2] == "0", out)

    # 6: box run — symlinked entrypoint, env, ordering, exit code, names.
    out = ssh("/bin/box run --rm testimg; echo rc=$?")
    check("box run: image Cmd through a symlinked /bin/sh, with every line", all(
        s in out for s in ("hello bar", "from-image", "etc", "rc=0")), out)
    out = ssh("/bin/box run --rm --entrypoint /bin/busybox testimg sh -c 'exit 7'; echo rc=$?")
    check("box run propagates the exit code", out.rstrip().endswith("rc=7"), out)
    out = ssh("/bin/box run --rm -e FOO=zzz -w /etc --entrypoint /bin/busybox testimg sh -c 'echo $FOO; pwd'")
    check("box run -e / -w", "zzz" in out and "/etc" in out, out)
    out = ssh("/bin/box run -d --name det1 --entrypoint /bin/busybox testimg sleep 100; "
              "/bin/box run -d --name det1 --entrypoint /bin/busybox testimg sleep 100; /bin/box ps")
    check("a live name is refused and the first container survives",
          "already in use" in out and "det1" in out.split("already in use")[-1], out)
    out = ssh("/bin/box close det1; /bin/box run -d --name det1 --entrypoint /bin/busybox testimg sleep 100")
    check("a closed name can be reused", "Started PID" in out, out)

    print("\n%s" % ("ALL PASSED" if failures == 0 else f"{failures} FAILED"))
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
