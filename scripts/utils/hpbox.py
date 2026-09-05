"""Drive the HP 500-502nj bring-up box ("the trashcan" / `vaporwave`) from a laptop.

One machine, two personalities on the same IP:

  * **Ubuntu** — builds, stages, arms GRUB — port 22
  * **Akuma** — the kernel under test — port 2222

`~/.ssh/config` carries an `akuma` alias (port 2222, the test key, no host
checking, because sshd regenerates its host key every boot). The trap that
creates: plain `ssh root@192.168.1.123` reaches **Akuma**, not Ubuntu. Anything
for the Ubuntu side MUST pass `-F /dev/null` — this module's `UB`/`ubuntu()` do.
A build once ran `cd /root/akuma` inside a kernel with no such directory because
of exactly this.

Operating manual: `docs/runbooks/amd64-bare-metal-loop.md`.

CLI:  python3 scripts/utils/hpbox.py which        # 'ubuntu' | 'akuma' | 'unknown'
      python3 scripts/utils/hpbox.py wait akuma   # block until that side answers
      python3 scripts/utils/hpbox.py ak  '<cmd>'  # run on Akuma
      python3 scripts/utils/hpbox.py ub  '<cmd>'  # run on Ubuntu
"""

import subprocess
import sys
import time

IP = "192.168.1.123"
SSH_KEY = "target/x86_64-unknown-none/release/amd64-ssh-test-key"

# Ubuntu: ignore ~/.ssh/config entirely, port 22.
UB = [
    "ssh", "-F", "/dev/null",
    "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
    "-o", "LogLevel=ERROR", "-o", "ConnectTimeout=10",
    "-p", "22", f"root@{IP}",
]
UB_RSH = ("ssh -F /dev/null -o StrictHostKeyChecking=no "
          "-o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -p 22")

# Akuma: the config alias already carries port, user, key and no host checking.
AK = ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=20", "akuma"]


def ubuntu(cmd, timeout=300):
    """Run `cmd` on the Ubuntu side. Returns (rc, stdout, stderr)."""
    r = subprocess.run(UB + [cmd], capture_output=True, text=True, timeout=timeout)
    return r.returncode, r.stdout, r.stderr


def akuma(cmd, timeout=60):
    """Run `cmd` on Akuma. Returns (rc, stdout, stderr).

    `reboot -f` never returns cleanly — call it and catch TimeoutExpired, or use
    `reboot_to('ubuntu')`.
    """
    r = subprocess.run(AK + [cmd], capture_output=True, text=True, timeout=timeout)
    return r.returncode, r.stdout, r.stderr


def push(files, repo="."):
    """rsync repo-relative paths to the Ubuntu side's snapshot at /root/akuma/.

    NEVER rsync the whole tree — vendored submodules make it ~37 GB. Name the
    files you changed; `--relative` recreates their directories.
    """
    r = subprocess.run(
        ["rsync", "-a", "--relative", "-e", UB_RSH] + list(files)
        + [f"root@{IP}:/root/akuma/"],
        cwd=repo, capture_output=True, text=True,
    )
    return r.returncode, r.stderr


def which_system(timeout=8):
    """'ubuntu', 'akuma', or 'unknown'. Asked, never assumed — port 22 is Ubuntu
    only, so a successful `uname` there is proof."""
    try:
        r = subprocess.run(UB + ["uname -a"], capture_output=True, text=True, timeout=timeout)
        if r.returncode == 0 and "Linux" in r.stdout:
            return "ubuntu"
    except subprocess.TimeoutExpired:
        pass
    try:
        r = subprocess.run(AK + ["uname -a"], capture_output=True, text=True, timeout=timeout)
        if r.returncode == 0 and "Akuma" in r.stdout:
            return "akuma"
    except subprocess.TimeoutExpired:
        pass
    return "unknown"


def wait_for(system, budget_s=300, poll_s=6):
    """Poll until the box is running `system`. Returns True if it got there."""
    deadline = time.time() + budget_s
    while time.time() < deadline:
        if which_system() == system:
            return True
        time.sleep(poll_s)
    return False


def reboot_to(system, budget_s=360):
    """Reboot the box into the other personality and wait for it.

    From Akuma: `reboot -f` (busybox `reboot` needs /proc; `-f` skips it). The
    GRUB one-shot has already been consumed, so Akuma resets into Ubuntu.
    From Ubuntu: plain `reboot`, which honours whatever `grub-reboot` armed.
    """
    here = which_system()
    if here == system:
        return True
    if here == "akuma":
        try:
            akuma("reboot -f", timeout=15)
        except subprocess.TimeoutExpired:
            pass  # expected: the connection dies with the machine
    elif here == "ubuntu":
        ubuntu('nohup sh -c "sleep 1; reboot" >/dev/null 2>&1 &', timeout=15)
    else:
        return False
    return wait_for(system, budget_s=budget_s)


def _main(argv):
    if not argv:
        print(__doc__)
        return 2
    cmd, rest = argv[0], argv[1:]
    if cmd == "which":
        print(which_system())
        return 0
    if cmd == "wait":
        return 0 if wait_for(rest[0], budget_s=int(rest[1]) if len(rest) > 1 else 300) else 1
    if cmd in ("ak", "akuma"):
        rc, o, e = akuma(" ".join(rest))
        sys.stdout.write(o)
        sys.stderr.write(e)
        return rc
    if cmd in ("ub", "ubuntu"):
        rc, o, e = ubuntu(" ".join(rest))
        sys.stdout.write(o)
        sys.stderr.write(e)
        return rc
    if cmd == "reboot-to":
        return 0 if reboot_to(rest[0]) else 1
    print(f"unknown command: {cmd}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(_main(sys.argv[1:]))
