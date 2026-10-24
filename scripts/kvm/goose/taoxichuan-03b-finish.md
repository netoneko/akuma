# Taoxichuan stage 3, part 2: the gate already passed - finish the paperwork

You are goose-glm, the metal track of the experiment in
`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Your last session got
Taoxichuan to the stage-3 gate (busybox `echo` and `ls /bin` ran as sequential kernel-reaped tasks, exit 0,
`stage 3 gate reached`) and then your 5-hour quota ran out at 19:56 IDT, before the paperwork. This prompt
only finishes stage 3. Stage 4 (sshd) is a separate prompt later: do not build ahead, and expect this to
take well under half an hour.

Every shell command starts with
`export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Checkout:
`/src/github.com/netoneko/akuma`, branch `taoxichuan-s3`.

## State, verified by the supervisor (do not redo)

- `taoxichuan-s3` @ `6c4602b7` is pushed to `litter`, tree clean. 15 crates (14 + `akuma-fdtable`),
  `taoxichuan/src/main.rs` 1,790 lines, release binary 266,752 B.
- `/root/dev-cycle/taoxichuan-s3/refs/`: `ref-echo.out` == `guest-echo.out` and `ref-ls.out` ==
  `guest-ls.out` (cmp identical, SHA256s in `SHA256SUMS.ref`). **The byte-compare is done and passes.**
- The supervisor already merged your branch on the aarch64 side: `cargo test -p akuma-ring3-amd64` passes
  on the Mac (your cfg gating is complete) and the aarch64 kernel builds and boots (stage 3: PASS there).
- To save you tokens, the supervisor already restored the services: `vm-serve-mc` is running (pid 8006),
  `vm-dev` is disabled. `/etc/herd/available/vm-dev.conf` still carries the stage-3 cmdline
  (`init=/bin/busybox initargs=echo,... then=/bin/busybox thenargs=ls,/bin`).

## 1. Take the supervisor's state first

`git fetch origin rust-org-il-experiment && git merge --no-edit origin/rust-org-il-experiment` (expect only
agent-log and tooling changes; the supervisor appended a "Stage 3 - host track" section to the log, so
**merge before you append yours**). Then, after you finish, fetch and merge once more before the last push.

**Portability rules, for every crate and brief from here on (the aarch64 track builds this tree on a Mac):**
all `asm!`/`global_asm!`/`naked_asm!`, port I/O and MSR code behind `#[cfg(target_arch = "x86_64")]`; an
amd64-only crate must NOT be added to the root `Cargo.toml` `default-members` (the supervisor had to remove
`akuma-ring3-amd64` from it in stage 2: it made `scripts/build_taoxichuan_fc.sh` compile x86 asm for aarch64
and fail with 55 errors); `cargo check` does not assemble inline asm, so it hides these breaks.

**Commit and `git push litter taoxichuan-s3` at least every 20 minutes whenever anything is uncommitted** (WIP
commits are fine). Stage 3 sat uncommitted for most of an hour and a quota stop would have lost it.

## 2. What is left

1. Re-run the gate boot once from the current tree so `vm-dev-pass.log` is a clean single-boot log (the
   present one is 3 KB and trimmed): `kbuild -j 4 -p taoxichuan --features no-tests`, install to
   `/boot/akuma-guest-dev` only, `herd stop vm-serve-mc; herd enable vm-dev; herd start vm-dev`, read
   `/var/log/herd/vm-dev.log`, save the passing segment to `/root/dev-cycle/taoxichuan-s3/vm-dev-pass.log`
   with its zero-fault grep. Keep the stage 3 cmdline for this boot.
2. Boot the main amd64 kernel once as a guest (`kbuild -j 4 --features no-tests`, same dance; the cmdline back on
   the stage-1 busybox line `init=/bin/busybox initargs=sh,/etc/guest.sh`; `AKUMA-GUEST-UP` -> `AKUMA-GUEST-DONE`,
   zero faults). Save it as `vm-dev-main-amd64.log`. Then `herd stop vm-dev; herd disable vm-dev; herd start
   vm-serve-mc` and **leave `/etc/herd/available/vm-dev.conf` on the stage-1 busybox line**. `vm-serve-mc` must
   be running when you finish. One `kvm0` VM at a time; never `pkill` by name; never `/boot/akuma-guest`.
3. Append a dated `### Stage 3 - metal track (goose-glm)` section to
   `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md` (at the end of the file, after merging origin):
   what you composed (the fd table over `akuma-fdtable`, `openat`/`getdents64`/`newfstatat`/..., the sequential
   task model and why `fork` was not needed), the crates (15) and image size (266,752 B), the gate evidence
   with the outputs, the byte-compare result, the surprises (the `out of memory` and `unhandled nr 228/96`
   iterations), and your own cost if you can see it. Do not rewrite other sections.
4. Write `/root/dev-cycle/taoxichuan-s3/RESULT.md`: PASS/FAIL against the narrowed gate (`busybox echo` +
   `busybox ls`, non-interactive; no ash/pty - the owner's decision), branch + commit, how it was booted, crate
   count and image size, and what you would tell the stage-4 prompt: sshd, exec-only, which means a network path
   (virtio-net or the Realtek), sockets, `fork`-or-not for sshd's per-connection child, `/dev/null` and
   `/dev/urandom`, and the host key. Be concrete about which of those exist in the minimal kernel today.
5. Commit, `git push litter taoxichuan-s3`. **Never push to `origin`.** Do not touch the dirty submodules.

You are on `glm-5.3-flash` at low effort; the quota window just reset, so spend it on this and nothing else.
When RESULT.md is written, the log section is committed and pushed, and `vm-serve-mc` is running, end the turn.
