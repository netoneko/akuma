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
      python3 scripts/utils/hpbox.py ramdisk [GiB]  # target/ on tmpfs (rotational root)
      python3 scripts/utils/hpbox.py ramdisk-sync   # copy tmpfs target/ back to disk
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
# Akuma: the config alias already carries port, user, key and no host checking.
AK = ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=20", "akuma"]


def ubuntu(cmd, timeout=300):
    """Run `cmd` on the Ubuntu side. Returns (rc, stdout, stderr).

    `errors="replace"`, not strict UTF-8. What comes back here is often a guest
    **console log**, and a console at `SMP=4` interleaves: measured 2026-09-11,
    Firecracker's own `[anonymous-instance:main]` line landed between the two
    halves of the banner's em dash and `text=True` raised
    `UnicodeDecodeError('utf-8', b'\xe2\x80…')`. `amd64_trials.py` reported that
    as `firecracker smp=4: ERROR` on a boot that had in fact passed 619/0 — a
    green run read as a failure, which is the expensive direction.
    """
    r = subprocess.run(UB + [cmd], capture_output=True, text=True,
                       errors="replace", timeout=timeout)
    return r.returncode, r.stdout, r.stderr


def akuma(cmd, timeout=60):
    """Run `cmd` on Akuma. Returns (rc, stdout, stderr).

    `reboot -f` never returns cleanly — call it and catch TimeoutExpired, or use
    `reboot_to('ubuntu')`.
    """
    r = subprocess.run(AK + [cmd], capture_output=True, text=True,
                       errors="replace", timeout=timeout)
    return r.returncode, r.stdout, r.stderr


def patch(paths=None, repo=".", base="HEAD", dry_run_first=True, touch=True):
    """Send local changes to the Ubuntu side as a patch and apply them there.

    **Superseded by :func:`deploy`.** Kept because it takes a `paths` filter and
    a `base` other than `HEAD`, which is occasionally what you want; for the
    ordinary "make the box match my tree" case `deploy` is strictly better —
    it pins the box to a *commit* first, so what lands is a known state plus a
    named patch rather than an unknown state plus a patch.

    This shells out to `patch -p1` because when it was written the box had no
    `.git`. It has one now, so `deploy` uses `git apply --3way`, which
    understands renames and deletions and can merge a hunk whose context moved.

    `paths` restricts the diff (repo-relative, as `git diff` takes them);
    `None` sends every tracked change against `base`. Returns ``(rc, message)``.
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

    The blunt instrument, for when a patch will not apply and you know exactly
    which files you want overwritten. :func:`deploy` is the default.

    Content is piped over ssh and written with `cat`, so each file lands with a
    **fresh mtime**. That is not incidental: cargo's freshness check reads
    mtimes, so any transport that preserves the laptop's — and a file whose
    laptop mtime predates the box's build artifacts — leaves cargo linking the
    stale rlib and reporting a missing symbol you can plainly `grep` for in the
    source.

    Cannot express a **deletion**: a file removed locally stays on the box and
    keeps compiling. :func:`deploy` can, and should be preferred.

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


def sync_from_git(rev=None, branch=None, timeout=300):
    """Make the box's tree exactly `rev` by fetching and hard-resetting.

    Returns ``(rc, message)``.

    # Why this and not a file sweep

    The box's tree is a **snapshot that drifts**, and every content-based answer
    to "what does it not have?" is a guess with a silent failure mode: send too
    few files and the build succeeds against stale source, so the bug you just
    fixed is still there and the evidence says it is not. `git` already knows
    the answer exactly.

    Hard reset is right here and would be wrong anywhere else: `/root/akuma` on
    the box is a **deployment checkout**, not anyone's working tree. Nothing is
    ever authored on it — the loop is laptop → push → box — so there is nothing
    a reset can destroy. (The rule against `git reset` in this project's
    CLAUDE.md is about the *developer's* repository, where a reset rewrites work
    the user has not reviewed.)

    The catch, and it is the whole usage note: **this only carries committed
    work.** Mid-iteration, with the fix still in the working tree, use
    :func:`send_files` with an explicit list instead. Push, then sync.
    """
    if rev is None:
        rev = f"origin/{branch}" if branch else "origin/HEAD"
    # `safe.directory` is not optional and its absence is not obvious: git
    # refuses a repository whose owner differs from the caller with
    # "detected dubious ownership", exits 128, and prints nothing to stdout —
    # so a caller that only reads stdout sees an empty success. Set every time;
    # `--add` on an existing value is a no-op.
    cmd = (f"git config --global --add safe.directory {BOX_REPO}; "
           f"cd {BOX_REPO} && git fetch --all --prune 2>&1 | tail -3 && "
           f"git reset --hard {rev} 2>&1 | tail -2 && git log --oneline -1")
    rc, out, err = ubuntu(cmd, timeout=timeout)
    return rc, (out + err).strip()


def deploy(repo=".", branch=None, timeout=600,
           paths=("amd64", "crates", "src", "scripts", "Cargo.toml", "Cargo.lock")):
    """Make the box's checkout match this worktree exactly — commits *and* dirt.

    Returns ``(rc, message)``. This is the one to call; everything below it is a
    piece of it.

    # What it does

    1. Fetch, and hard-reset the box to the newest local commit it can reach.
       Usually that is local `HEAD`. If `HEAD` has not been pushed, it falls
       back to the newest ancestor that *has* been, and says which.
    2. `git apply --3way` the remaining diff — unpushed commits and the working
       tree together, as one patch against the commit the box just landed on.

    So an unpushed, uncommitted fix reaches the box in one call, and the box
    ends up at a known commit plus a named patch rather than at "some files were
    overwritten".

    # Why this replaces both older helpers

    :func:`patch` shells out to `patch -p1` because when it was written the box
    had no `.git`. It has one now, and `git apply --3way` is strictly better:
    it understands renames and deletions, it can merge a hunk whose context
    moved, and when it cannot it reports a conflict instead of a rejected hunk.
    :func:`send_files` cannot express a deletion at all — a file removed locally
    stays on the box and keeps compiling.

    The failure both older paths shared is the one that costs the most: sync too
    little and the build succeeds against stale source, so the bug you just
    fixed is still there and the evidence says it is not. Resetting to a commit
    and applying a patch cannot do that quietly — either the reset lands or it
    errors, and either the patch applies or it conflicts.
    """
    import subprocess as sp

    def git(*args, check=False):
        r = sp.run(["git", "-C", repo, *args], capture_output=True, text=True)
        if check and r.returncode != 0:
            raise RuntimeError(f"git {' '.join(args)}: {r.stderr.strip()}")
        return r

    head = git("rev-parse", "HEAD").stdout.strip()
    if not head:
        return 1, "not a git repository"

    # Which local commits does the remote already have? `git branch -r
    # --contains` is the honest question; asking the box would be a round trip
    # for the same answer.
    fetch = f"git config --global --add safe.directory {BOX_REPO}; cd {BOX_REPO} && git fetch --all --prune 2>&1 | tail -2"
    rc, out, err = ubuntu(fetch, timeout=timeout)
    if rc not in (0, None):
        return rc, f"fetch failed: {(out + err).strip()}"

    # The newest local commit that exists on the remote. Walk back from HEAD
    # rather than trusting @{upstream}, which may not be set.
    base = None
    for line in git("rev-list", "--max-count=200", "HEAD").stdout.split():
        contains = git("branch", "-r", "--contains", line).stdout.strip()
        if contains:
            base = line
            break
    if base is None:
        return 1, ("no commit in the last 200 is on any remote — push something first, "
                   "or the box has nothing to reset to")

    rc, out, err = ubuntu(
        f"cd {BOX_REPO} && git reset --hard {base} 2>&1 | tail -2 && git log --oneline -1",
        timeout=timeout)
    if rc not in (0, None):
        return rc, f"reset failed: {(out + err).strip()}"
    landed = (out + err).strip().splitlines()[-1] if (out + err).strip() else base[:7]

    # Everything else: unpushed commits + the working tree, as one patch.
    diff = git("diff", base).stdout
    note = f"box at {landed}"
    if base != head:
        note += f" (local HEAD {head[:7]} is not pushed — sent as patch)"

    # **Untracked files are not in `git diff`**, and a refactor's first act is to
    # create one. This is not a corner case; it is the single most likely way to
    # deploy stale source, and it is silent in the worst way: the box builds, the
    # build *succeeds* against the old code, and the numbers you then read look
    # like your change did nothing.
    #
    # Measured 2026-09-07: `amd64/src/boot.rs` was new, the patch carried the
    # edits to `main.rs`/`multiboot2.rs` that referenced it and not the file
    # itself, and the box shipped a kernel byte-identical to the previous one.
    # The md5 in `stage_akuma.sh`'s output was the only evidence.
    #
    # `--exclude-standard` so `.gitignore` still applies — `target/` must never
    # come along.
    untracked = [
        f for f in git("ls-files", "--others", "--exclude-standard", "--", *paths).stdout.split()
    ]
    if untracked:
        rc, msg = send_files(untracked, repo=repo, touch=False)
        if rc not in (0, None):
            return rc, note + f"; sending untracked files failed: {msg}"
        note += f"; {len(untracked)} untracked file(s) sent"

    if not diff.strip():
        return 0, note + "; nothing further to apply"

    r = subprocess.run(
        UB + [f"cd {BOX_REPO} && git apply --3way --whitespace=nowarn -"],
        input=diff, capture_output=True, text=True, timeout=300,
    )
    if r.returncode != 0:
        return r.returncode, note + "; git apply failed:\n" + (r.stdout + r.stderr).strip()

    lines = len(diff.splitlines())
    return 0, f"{note}; applied a {lines}-line patch"


def files_missing_on_box(repo=".", paths=("amd64", "crates", "Cargo.toml", "Cargo.lock"),
                        timeout=120):
    """Exactly the files this worktree has that the box's checkout does not.

    Returns ``(rc, paths)`` for :func:`send_files`.

    Now that `/root/akuma` is a real checkout (:func:`sync_from_git`), the box
    can be *asked* what it is on, and the answer makes this exact rather than a
    guess: everything committed since that point, plus everything still dirty in
    the working tree. No checksum sweep, no commit-range magic number, and no
    dependence on the box having been synced recently.

    This is the mid-iteration path — the one for a fix that is not pushed yet.
    Once it is pushed, :func:`sync_from_git` is simpler and carries deletions
    too, which this cannot.
    """
    import subprocess as sp

    rc, head = box_head(timeout=timeout)
    if rc not in (0, None) or not head:
        return 1, f"could not read the box's HEAD: {head}"
    sha = head.split()[0]

    def git(*args):
        r = sp.run(["git", "-C", repo, *args], capture_output=True, text=True)
        return r.stdout.splitlines() if r.returncode == 0 else []

    # Committed locally but not on the box.
    committed = git("diff", "--name-only", f"{sha}..HEAD", "--", *paths)
    # Still only in the working tree.
    dirty = [l[3:] for l in git("status", "--porcelain", "--", *paths) if l[3:]]

    import os
    both = sorted({f for f in committed + dirty if os.path.isfile(os.path.join(repo, f))})
    return 0, both


def box_head(timeout=60):
    """The commit the box's tree is on — one line, `<short sha> <subject>`.

    Worth printing before every remote trial. "Which system is running" is the
    first question this loop teaches you to ask; "which commit is it building"
    is the second, and it has cost just as much time.
    """
    rc, out, err = ubuntu(f"cd {BOX_REPO} && git log --oneline -1", timeout=timeout)
    return rc, (out + err).strip()


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
    # `set -o pipefail` is not decoration. Without it the pipeline's status is
    # `tail`'s, which is 0 whatever rustc did — so a failed build returned
    # `rc == 0` and the caller went on to `stage()`, which happily installed
    # the *previous* kernel and armed GRUB for it. Measured 2026-09-11: a
    # compile error (E0063) reported `build rc 0` and `stage rc 0`, and the
    # only evidence anything was wrong was an `error:` line inside the tail.
    # Same silent-success family as `BOX_CARGO` itself, one line up.
    cmd = (f"set -o pipefail; {BOX_CARGO}; cd {BOX_REPO} && "
           f"cargo build -p {pkg} --target {target} --release 2>&1 | tail -25")
    rc, out, err = ubuntu(cmd, timeout=timeout)
    out = (out + err).strip()
    # Belt as well as braces: `pipefail` is a bash-ism and this command travels
    # through whatever `ssh` gives it a shell of. A cargo `error:` line at the
    # start of a line is unambiguous.
    if rc == 0 and any(l.startswith("error") for l in out.splitlines()):
        return 1, out
    return rc, out


# Where the on-disk copy of `target/` lives while a tmpfs stands in its place.
BOX_TARGET = f"{BOX_REPO}/target"
BOX_TARGET_DISK = f"{BOX_REPO}/target.disk"


def ramdisk(size_g=6, prime=True, timeout=600):
    """Put the box's `target/` on tmpfs, primed from the on-disk copy.

    Returns ``(rc, message)``. Idempotent: already-mounted is a no-op success.

    The box's root is a **rotational** disk (`/sys/block/sda/queue/rotational`
    is 1) and a kernel build is thousands of small object writes, so rustc
    spends real time waiting on it. RAM is not the constraint — 15 GiB total,
    and the whole `target/` tree is ~300 MiB.

    **It does not survive a reboot, and this loop reboots constantly** (Ubuntu →
    Akuma → Ubuntu, several times a session). That is the reason for the
    two-directory shape rather than a plain `mount -t tmpfs`:

    * `target.disk/` is the persistent copy. It is what a boot with no tmpfs
      mounted builds against, so *forgetting* to re-mount costs speed and
      nothing else — never a cold rebuild, and never a stale artifact.
    * `prime` rsyncs it into the fresh tmpfs, so the first build after a reboot
      is incremental like every other one.
    * :func:`ramdisk_sync` copies back. Call it before a reboot if the build
      you just made is one you want to keep; skipping it only costs the next
      boot's first build.

    Every path `hpbox` and `/root/stage_akuma.sh` know stays exactly where it
    was — `BOX_KERNEL`, `BOX_DISK`, GRUB's `/boot/akuma` install — because this
    mounts *over* `target/` rather than moving it. A `CARGO_TARGET_DIR` export
    would have been one line and would have desynchronised all of them.
    """
    script = (
        'set -e\n'
        f'mkdir -p {BOX_TARGET} {BOX_TARGET_DISK}\n'
        f'if mountpoint -q {BOX_TARGET}; then echo "ALREADY on tmpfs"; '
        f'df -h {BOX_TARGET} | tail -1; exit 0; fi\n'
        # Seed the persistent copy from what is on disk — but ONLY the first
        # time, when there is nothing there yet. After a reboot the tmpfs is
        # gone and `target/` is once more the *underlying* directory, frozen at
        # whatever it held before the first mount; copying that over
        # `target.disk` would throw away every build since. `target.disk` is
        # the authority once it exists, and :func:`ramdisk_sync` is the only
        # thing that writes it.
        f'if [ -z "$(ls -A {BOX_TARGET_DISK})" ]; then '
        f'rsync -a --delete {BOX_TARGET}/ {BOX_TARGET_DISK}/; fi\n'
        f'mount -t tmpfs -o size={size_g}G tmpfs {BOX_TARGET}\n'
        + (f'rsync -a {BOX_TARGET_DISK}/ {BOX_TARGET}/\n' if prime else '')
        + f'df -h {BOX_TARGET} | tail -1\n'
        'echo RAMDISK OK\n'
    )
    rc, out, err = ubuntu(script, timeout=timeout)
    return rc, (out + err).strip()


def ramdisk_sync(timeout=600):
    """Copy the tmpfs `target/` back to the persistent `target.disk/`.

    A no-op success when `target/` is not on tmpfs. Worth doing before a reboot
    into Akuma; the cost of skipping it is one slower build, not a wrong one.
    """
    script = (
        'set -e\n'
        f'if ! mountpoint -q {BOX_TARGET}; then echo "not on tmpfs; nothing to sync"; exit 0; fi\n'
        f'rsync -a --delete {BOX_TARGET}/ {BOX_TARGET_DISK}/\n'
        'echo SYNCED\n'
    )
    rc, out, err = ubuntu(script, timeout=timeout)
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
    out = (out + err).strip()
    # The script builds before it installs, and a build failure inside it does
    # not stop it: it installs whatever is already in `target/` and arms GRUB
    # for that. So a `rc == 0` here means "the script ran", not "the kernel you
    # just wrote is the one that will boot" — check for the failure the same
    # way :func:`build` does, or the next boot silently measures the *previous*
    # change. This is what made a staged run report a fold that had not
    # compiled (2026-09-11).
    if rc == 0 and any(l.startswith("error") for l in out.splitlines()):
        return 1, out
    return rc, out


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
    if cmd == "ramdisk":
        rc, msg = ramdisk(size_g=int(rest[0]) if rest else 6)
        print(msg)
        return rc
    if cmd == "ramdisk-sync":
        rc, msg = ramdisk_sync()
        print(msg)
        return rc
    print(f"unknown command: {cmd}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(_main(sys.argv[1:]))
