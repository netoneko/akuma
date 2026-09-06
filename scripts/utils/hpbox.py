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
      python3 scripts/utils/hpbox.py patch [path…]  # send local diff, apply there
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


def patch(paths=None, repo=".", base="HEAD", dry_run_first=True, touch=True):
    """Send local changes to the Ubuntu side as a patch and apply them there.

    Preferred over :func:`push` for iterating on kernel source, for three
    reasons the rsync path learned the hard way:

    * **It carries only what changed.** `push` needs you to name every file, and
      naming too few is how a build fails on a symbol whose *source* is plainly
      present on the box — you synced the file and not the crate.
    * **It fails loudly on drift.** The box's tree is a snapshot, not a
      checkout, so it drifts. `patch --dry-run` refuses a hunk that does not
      apply; rsync would overwrite whatever was there and say nothing.
    * **It sidesteps the mtime trap.** `rsync -a` preserves mtimes and cargo's
      freshness check reads them, so syncing a file *older* than the box's
      existing artifacts leaves cargo convinced nothing changed and linking the
      stale rlib. A patched file is written now, so its mtime is now.

    `paths` restricts the diff (repo-relative, as `git diff` takes them);
    `None` sends every tracked change against `base`. Returns
    ``(rc, message)`` — rc 0 on success, and `message` is whatever `patch`
    said when it is not.

    The box has no `.git`, so this is `patch -p1`, not `git apply`.
    """
    cmd = ["git", "diff", base]
    if paths:
        cmd += ["--"] + list(paths)
    diff = subprocess.run(cmd, cwd=repo, capture_output=True, text=True)
    if diff.returncode != 0:
        return diff.returncode, f"git diff failed: {diff.stderr.strip()}"
    if not diff.stdout.strip():
        return 0, "no local changes to send"

    def _apply(extra):
        return subprocess.run(
            UB + [f"cd /root/akuma && patch -p1 {extra}"],
            input=diff.stdout, capture_output=True, text=True, timeout=120,
        )

    if dry_run_first:
        chk = _apply("--dry-run --forward")
        if chk.returncode != 0:
            return chk.returncode, "dry run refused:\n" + (chk.stdout + chk.stderr).strip()
    r = _apply("--forward")
    if r.returncode != 0:
        return r.returncode, (r.stdout + r.stderr).strip()

    if touch:
        # Belt and braces for the mtime trap: `patch` already writes fresh
        # mtimes, but a crate the patch did not touch can still hold a stale
        # rlib from a *previous* partial sync.
        subprocess.run(
            UB + ["cd /root/akuma && find crates amd64 userspace -type f "
                  "\\( -name '*.rs' -o -name '*.toml' \\) -newermt '-2 days' -exec touch {} + 2>/dev/null; true"],
            capture_output=True, text=True, timeout=180,
        )
    return 0, (r.stdout or "applied").strip()


def send_files(paths, repo=".", touch=True):
    """Make the box's copy of `paths` byte-identical to this worktree's.

    The companion to :func:`patch`, and the one to reach for when the two trees
    are not on the same base. `patch` sends a *diff against `HEAD`*, so it
    applies only while the box's snapshot is on that same commit; the moment it
    is a commit behind, every hunk in a rewritten file is refused. That refusal
    is the helper working — but the fix is to send the files themselves.

    Content is piped over ssh and written with `cat`, so each file lands with a
    **fresh mtime**. That is not incidental: `rsync -a` preserves mtimes and
    cargo's freshness check reads them, so a file whose laptop mtime predates
    the box's build artifacts leaves cargo linking the stale rlib and reporting
    a missing symbol you can plainly `grep` for in the source.

    Returns ``(rc, message)``.
    """
    import os
    sent = []
    for rel in paths:
        local = os.path.join(repo, rel)
        try:
            with open(local, "rb") as fh:
                body = fh.read()
        except OSError as exc:
            return 1, f"{rel}: {exc}"
        remote = f"/root/akuma/{rel}"
        # `cat > file` via a heredoc-free stdin pipe: no quoting of the content
        # at all, so a Rust file full of quotes and backslashes is safe.
        r = subprocess.run(
            UB + [f"mkdir -p $(dirname {remote}) && cat > {remote}"],
            input=body, capture_output=True, timeout=180,
        )
        if r.returncode != 0:
            return r.returncode, f"{rel}: {r.stderr.decode(errors='replace').strip()}"
        sent.append(rel)

    if touch:
        subprocess.run(
            UB + ["cd /root/akuma && find crates amd64 userspace -type f "
                  "\\( -name '*.rs' -o -name '*.toml' \\) -exec touch {} + 2>/dev/null; true"],
            capture_output=True, text=True, timeout=300,
        )
    return 0, f"sent {len(sent)} file(s): " + ", ".join(sent)


# Where the box keeps the things this module drives.
BOX_REPO = "/root/akuma"
BOX_CARGO = "export PATH=/root/.cargo/bin:$PATH"
BOX_KERNEL = f"{BOX_REPO}/target/x86_64-unknown-none/release/akuma-amd64"
BOX_DISK = f"{BOX_REPO}/target/x86_64-unknown-none/release/amd64-root.img"


def build(pkg="akuma-amd64", target="x86_64-unknown-none", timeout=900):
    """Build `pkg` on the Ubuntu side. Returns ``(rc, tail_of_output)``.

    `BOX_CARGO` is not optional: a non-interactive ssh does not read the
    profile that puts `~/.cargo/bin` on `PATH`, so a bare `cargo build` over
    this transport fails with `bash: cargo: command not found` — a line that
    matches no `^error` grep and so reads as a *successful* build with no
    output. The binary's mtime is the only thing that gives it away.
    """
    cmd = (f"{BOX_CARGO}; cd {BOX_REPO} && "
           f"cargo build -p {pkg} --target {target} --release 2>&1 | tail -25")
    rc, out, err = ubuntu(cmd, timeout=timeout)
    return rc, (out + err).strip()


def firecracker(vcpus=1, init="/bin/busybox", initargs="uname,-a", memory=2048,
                timeout_s=90, disk=True):
    """Boot the box's freshly-built kernel under Firecracker. Returns the log.

    The config is written here rather than kept on the box, so the two cannot
    drift — the same reason `amd64/run-firecracker.sh` stages its own. That
    script is the laptop-driven path and needs `FC_HOST` plus ssh options that
    dodge the `akuma` alias; this one runs entirely on the Ubuntu side, against
    the kernel `build()` just produced.

    `timeout` without `--foreground` is correct **here specifically**: the
    kernel halts with `cli; hlt` and never exits, so a bound is required, and
    over ssh with no `-t` there is no controlling TTY for the process-group
    problem that `--foreground` exists to avoid.
    """
    boot_args = f"init={init}"
    if initargs:
        boot_args += f" initargs={initargs}"
    drives = ("[]" if not disk else
              '[{"drive_id":"rootfs","path_on_host":"%s",'
              '"is_root_device":false,"is_read_only":false}]' % BOX_DISK)
    cfg = (
        '{\n'
        f'  "boot-source": {{ "kernel_image_path": "{BOX_KERNEL}", "boot_args": "{boot_args}" }},\n'
        f'  "drives": {drives},\n'
        '  "network-interfaces": [],\n'
        f'  "machine-config": {{ "vcpu_count": {vcpus}, "mem_size_mib": {memory} }}\n'
        '}\n'
    )
    # Written through stdin, not a quoted heredoc: the JSON carries braces and
    # quotes that a shell-side heredoc in an ssh argument would have to escape.
    r = subprocess.run(UB + ["cat > /tmp/hpbox-fc.json"], input=cfg,
                       capture_output=True, text=True, timeout=60)
    if r.returncode != 0:
        return f"config write failed: {r.stderr.strip()}"
    run = (f"for p in $(pgrep -x firecracker); do kill -9 $p; done; "
           f"rm -f /tmp/hpbox-fc.sock /tmp/hpbox-fc.log; "
           f"timeout {timeout_s} firecracker --no-api "
           f"--config-file /tmp/hpbox-fc.json --api-sock /tmp/hpbox-fc.sock "
           f"> /tmp/hpbox-fc.log 2>&1; cat /tmp/hpbox-fc.log")
    _rc, out, err = ubuntu(run, timeout=timeout_s + 90)
    return out + err


def stage(cmdline_extra="", timeout=900):
    """Build, rebuild the root image, install into `/boot/akuma`, arm GRUB.

    Wraps the box's own `/root/stage_akuma.sh`, which is the authority on the
    sequence (it also backs up what it replaces and checks the multiboot2
    header survived the link). Returns ``(rc, output)``.

    **`cmdline_extra` is the whole boot configuration.** The script rewrites the
    GRUB entry to `init=/bin/sshd <extra>`, so anything omitted is *dropped* —
    passing nothing boots the RAM image in `root.img` and does not touch the USB
    controller at all. `root=/dev/sda1` is what selects the persistent root;
    `usb` brings the controller up without mounting it; `skiptests` skips the
    suite; `nosmp` is single core.
    """
    rc, out, err = ubuntu(f'CMDLINE_EXTRA="{cmdline_extra}" bash /root/stage_akuma.sh 2>&1',
                          timeout=timeout)
    return rc, (out + err).strip()


def restage_disk(keep_keys=True, timeout=600):
    """Copy the freshly built `root.img` onto the persistent `sda1` root.

    The box's own `/AKUMA_DISK.txt` marker records this as the procedure and
    says "RE-STAGE after a fresh userspace build" — without it the persistent
    root keeps whatever userspace it was last given while `/boot/akuma/root.img`
    moves on, and the two disagree about which binaries the machine has. That
    is not academic: with `root=/dev/sda1` the RAM image is not consulted at
    all, so a freshly built `/bin/sshd` that exists only there is not the one
    that runs.

    `rsync --delete` is what makes the two identical, and it takes
    `etc/sshd/authorized_keys` with it — the image carries only the generated
    test key, so any key added by hand is lost. `keep_keys` saves that file
    first and merges back any line the image does not have.

    Ubuntu sees the partition as **`sdb1`** (its own disk is `sda`); Akuma sees
    the same partition as `sda1`, because it enumerates only the USB one. Every
    confusing moment in this loop has started with mixing those two up.
    """
    merge = ""
    if keep_keys:
        merge = (
            'if [ -f /tmp/ak_keys.save ]; then\n'
            '  while IFS= read -r k; do\n'
            '    [ -n "$k" ] || continue\n'
            '    grep -qF "$k" /mnt/akvol/etc/sshd/authorized_keys 2>/dev/null '
            '|| echo "$k" >> /mnt/akvol/etc/sshd/authorized_keys\n'
            '  done < /tmp/ak_keys.save\n'
            'fi\n'
        )
    script = (
        'set -e\n'
        'mkdir -p /mnt/rimg /mnt/akvol\n'
        'mountpoint -q /mnt/rimg  && umount /mnt/rimg  || true\n'
        'mountpoint -q /mnt/akvol && umount /mnt/akvol || true\n'
        'mount -o loop,ro /boot/akuma/root.img /mnt/rimg\n'
        'mount /dev/sdb1 /mnt/akvol\n'
        'cp -f /mnt/akvol/etc/sshd/authorized_keys /tmp/ak_keys.save 2>/dev/null || true\n'
        'rsync -aH --delete /mnt/rimg/ /mnt/akvol/\n'
        + merge +
        'printf "staged from /boot/akuma/root.img by rsync -aH --delete on %s\\n'
        'RE-STAGE after a fresh userspace build.\\n" "$(date -Is)" '
        '> /mnt/akvol/AKUMA_DISK.txt\n'
        'ls -la /mnt/akvol/bin/ssh /mnt/akvol/bin/sshd\n'
        'cat /mnt/akvol/etc/sshd/authorized_keys\n'
        'sync\n'
        'umount /mnt/akvol\n'
        'umount /mnt/rimg\n'
        'echo RESTAGED\n'
    )
    rc, out, err = ubuntu(script, timeout=timeout)
    return rc, (out + err).strip()


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
    if cmd == "fc":
        print(firecracker(vcpus=int(rest[0]) if rest else 1))
        return 0
    if cmd == "build":
        rc, out = build()
        print(out)
        return rc
    if cmd == "patch":
        rc, msg = patch(rest or None)
        print(msg)
        return rc
    if cmd == "reboot-to":
        return 0 if reboot_to(rest[0]) else 1
    print(f"unknown command: {cmd}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(_main(sys.argv[1:]))
