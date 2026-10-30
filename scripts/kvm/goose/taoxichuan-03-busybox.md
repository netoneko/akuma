# Taoxichuan stage 3: busybox runs - `echo` and `ls`, non-interactive

You are goose-glm, the metal track of the experiment in
`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Stages 1 and 2 are done and
signed off (your `taoxichuan-s2`, head `0b03d938` or later: a static musl hello runs in ring 3 and its output is
byte-identical to the host's). The aarch64 host track is now supervised by Claude Sonnet; Kimi is gone.
This prompt is stage 3 only; sshd is stage 4, a later prompt. Do not build ahead.

Every shell command starts with
`export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Checkout:
`/src/github.com/netoneko/akuma` ($AKUMA_SRC).

## 1. Take the supervisor's state first

`git fetch origin rust-org-il-experiment`, branch `taoxichuan-s3` off `taoxichuan-s2`, and
`git merge --no-edit origin/rust-org-il-experiment` into it (expect no conflicts; it carries agent-log
sections and supervisor tooling). **Fetch `origin` and merge again right before your final gate run**, in
case the supervisor pushed a portability fix meanwhile. Same hygiene as before: new `asm!`/`global_asm!`/
`naked_asm!`/port I/O behind `#[cfg(target_arch = "x86_64")]` (the supervisor compiles every new crate on an
aarch64 Mac, and your `akuma-ring3-amd64` naked asm is currently NOT gated: gate it in this stage, a
two-line change), and `cargo clippy -p <crate> --target x86_64-unknown-none -- -D warnings` green for every
crate you touch.

## 2. Stage 3 deliverable (the user narrowed the pinned gate)

The experiment doc pins "`ash` runs a script". **The user dropped that for now: no interactive shell, no
pty, no terminal handling.** The gate is:

> The guest runs `busybox echo <a fixed string>` and `busybox ls <a fixed directory on the ext2 root>` as
> kernel-spawned programs, and the console output of each matches the reference **byte for byte** (after the
> console's CR-before-LF normalisation, as in stage 2), exit code 0, zero `[Fault]`/`PANIC`/`[sig!]` lines.

- busybox is already `/bin/busybox` on the guest ext2 root (static, the one stage 1 booted). Use it as a
  multi-call binary (`argv[0]` or first argument selects the applet); applet symlinks and `sh` are not needed.
- References must come from an independent source, not from the guest: run the same applets on the box (its
  Akuma userland has busybox) against the same file set, or list the image with `ext2ls.py`. Pick a directory
  with a handful of known entries (create one with `ext2addfile.py` if `/bin` is too noisy) so `ls` output is
  deterministic. Save references and the guest outputs next to the log; hash-compare as in stage 2.
- Two programs means two tasks: that is fine as kernel-reaped **sequential** tasks (spawn, run to `exit`, spawn
  the next). There is still no `fork`, no scheduler and no herd in the image; if you find you need `fork` for
  `echo`/`ls`, stop and say why in RESULT.md instead of building it.
- Keep the stage 1 and 2 gates: banner, spawn line, and `/bin/hello` must still pass.

Where the work is (your stage-2 notes, quoted): the `-ENOSYS` arm prints the syscall number, so boot
busybox once and the console lists what to add; expect `rt_sigaction`, `rt_sigprocmask`, `openat`,
`newfstatat`, `getdents64`, `close`, `clock_gettime`, maybe `ioctl` (answer `ENOTTY` for a non-tty stdout),
`uname`, `set_tid_address`, `readv`. Static musl `malloc` already lives on your `brk`/`mmap` slabs. `ls`
walking a directory on ext2 is the first thing that needs the VFS from a ring-3 file descriptor: keep the fd
table to the smallest thing that works (a few fixed slots), and think about whether that logic belongs in a
crate before you grow `taoxichuan/src/main.rs` further (it is 1,106 lines now).

The experiment's composition rule stands: pull as few crates as possible; extract into a crate first where
the logic still lives in `amd64/src/`. Record the crate count (it was 14 at the end of stage 2) and the image
size (198,072 B) at the end of this stage in RESULT.md.

## 3. Ops rules (unchanged)

- `kbuild -j 4 -p taoxichuan --features no-tests`; install to `/boot/akuma-guest-dev` only, never
  `/boot/akuma-guest`. Never `cargo clean`.
- One `kvm0` VM at a time: `herd stop vm-serve-mc` before `herd enable vm-dev; herd start vm-dev` (re-run
  `herd enable` after editing the conf: the enabled copy is a snapshot); when done `herd stop vm-dev; herd
  disable vm-dev; herd start vm-serve-mc`. **`vm-serve-mc` must be running at the end; put the conf back on
  the stage-1 busybox line.**
- Never `pkill` by name; kill by pid. Never pipe a long-running command into `grep -q`; output to a file, poll
  the file.
- The harness's `[KVM] akuma-vm guest: FAIL (banner=yes, init=no, ...)` line is herd's generic marker check;
  the gate is the kernel's own evidence lines plus the zero-fault grep.
- Boot the main amd64 kernel once as a guest at the end (`kbuild -j 4 --features no-tests`;
  `AKUMA-GUEST-UP` -> `AKUMA-GUEST-DONE`) to prove nothing broke.
- `git push litter taoxichuan-s3` after every commit. **Never push to `origin`.** Do not touch the dirty
  submodules (`userspace/rumpkernel/src-netbsd`, `userspace/tcc/tinycc`) or untracked `root/`.

## 4. Paperwork (commit early, commit often)

1. Gate console log -> `/root/dev-cycle/taoxichuan-s3/vm-dev-pass.log`, the main-kernel boot log beside it,
   the references and the guest outputs with their hashes.
2. Append a dated `### Stage 3 - metal track (goose-glm)` section to
   `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`: what you composed, crates pulled or extracted,
   the gate evidence, the byte-compare result, anything surprising. Do not rewrite other sections.
3. `/root/dev-cycle/taoxichuan-s3/RESULT.md`: PASS/FAIL, branch + commit, how it was booted, crate count and
   image size, and what you would tell the stage-4 prompt (sshd, exec-only: needs a network path and sockets).

You are on `glm-5.3-flash` at low effort. Explore as much as you need; the 5-hour quota is the budget and the
user wants to see how far it goes. Work autonomously until the gate passes or you are genuinely blocked; then
write the result file, commit, push, and end the turn.
