# Taoxichuan stage 4: sshd works - an ssh round trip executes a command (exec-only)

You are goose-glm, the metal track of the experiment in
`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Stages 1-3 are done (stage 3, once you
finish its paperwork: `taoxichuan-s3`, a static musl `hello` and non-interactive `busybox echo`/`ls` run as
sequential kernel-reaped tasks, byte-identical to the host). This prompt is stage 4 only: **sshd**. Stage 5 ("no
herd") is a constraint you already satisfy (init is kernel-spawned), stage 6 is `apk add curl`, a later prompt. Do
not build ahead.

Every shell command starts with
`export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Checkout:
`/src/github.com/netoneko/akuma`.

## 1. Take the supervisor's state first, and the standing rules

`git fetch origin rust-org-il-experiment`, branch `taoxichuan-s4` off `taoxichuan-s3`, and
`git merge --no-edit origin/rust-org-il-experiment` into it. Fetch and merge `origin` again right before your
final gate run.

- **Portability (the aarch64 track builds this tree on a Mac; it broke twice already):** all
  `asm!`/`global_asm!`/`naked_asm!`, port I/O and MSR code behind `#[cfg(target_arch = "x86_64")]`; an
  amd64-only crate is NOT added to the root `Cargo.toml` `default-members`; `cargo check` does not assemble
  inline asm, so it hides these breaks; `cargo clippy -p <crate> --target x86_64-unknown-none -- -D warnings`
  green for every crate you touch.
- **Commit and `git push litter taoxichuan-s4` at least every 20 minutes whenever anything is uncommitted** (WIP
  commits are fine). Stage 3 sat uncommitted for most of an hour; your 5-hour quota window can end mid-stage, and
  an uncommitted tree is the thing that gets lost.
- Never push to `origin`. Do not touch the dirty submodules.

## 2. Stage 4 deliverable

The pinned gate (experiment doc): **an ssh round trip from the host executes a command and the output is
byte-equal; exec-only, no pty.** Concretely:

- The Taoxichuan guest boots from kernel-spawned init (no herd), brings up its NIC, runs the userspace
  `/bin/sshd`, and accepts a connection on guest port 22, forwarded by the host (`vm_forward = <host>:22` in the
  `vm-dev` conf, e.g. `2224:22`; the NIC is polled and the host gateway opens the connection to the guest, see
  `docs/reference/subsystems/kvm.md` "The relay" and the `vm_forward` row).
- From the host: `ssh -T -p <fwd> root@localhost '<command>'` runs the command in the guest and prints its
  output. Use a command with fixed output (`busybox echo hello-stage-4`, and `busybox ls /bin`), and save the
  outputs byte-exact. The check is a completed ssh command, never a log marker (`scripts/vm_ready.py` is the
  repo's version of that idea).
- **The reference comes from the full kernel's guest, which is already running as `vm-serve-mc`** (forwarded
  `2223:22`, the same userspace sshd, the same kind of root image `/boot/akuma-serve-root.img`). Run the same two
  commands against it FIRST and save the outputs; then stop it for your boot. Only one `kvm0` VM at a time.
  Compare with `cmp` and SHA256 as before.
- Zero `[Fault]`/`PANIC`/`[sig!]` lines in the guest console for the whole session, and stage 1-3 gates still pass.
- **Never write to `/boot/akuma-serve-root.img`** (a live VM uses it). Make your own root image: copy it to a new
  file (e.g. `/boot/akuma-dev-root-ssh.img`), point `vm_disk` at the copy in `vm-dev.conf`, and keep the stage 3
  16 MiB image for the stage 1-3 regression. Keys (host key, `authorized_keys`) and sshd's config come from the
  copy; do not generate or print any private key material in the logs.

Where the work is (this is your survey; I am not listing files): the minimal kernel today has a block device, an
ext2 root, ring 3 with a handful of syscalls and an fd table with a few fixed slots, and no NIC, no sockets, no
pipes, no time beyond what `clock_gettime` needs, and one task at a time. sshd (`userspace/sshd`, one process,
sessions are polled futures in one loop; see `docs/reference/subsystems/ssh.md`) needs, at least: a network
path (the main kernel's guest uses virtio-net through `akuma-net`/`akuma-net-nic`, check what compiles for
`x86_64-unknown-none`), the socket calls to listen/accept/read/write/poll on it, randomness for the key exchange
(`getrandom`; the main amd64 kernel has a source, check where it comes from), the clock, reading its host key and
`authorized_keys` from ext2, and a way to run the client's command and read its output back. The last one is the
design decision of the stage: with no scheduler and no `fork`, the simplest honest model may be that spawning a
child runs it to completion inside the spawn and its stdout lands in a kernel buffer the parent then reads (a
pipe that is just a buffer); decide, say why in RESULT.md, and pull as few crates as you can. Check whether
`sshd`'s `fork-sessions` feature or its default async build is the one that fits. Extract before you grow
`taoxichuan/src/main.rs` further: it is 1,790 lines and should not be where this stage lives.

Per the experiment's composition rule: pull as few crates as possible; extract into a crate first where the logic
still lives in `amd64/src/`. Record the crate count (15 at the end of stage 3) and the image size (266,752 B) in
RESULT.md.

## 3. Ops rules (unchanged)

- `kbuild -j 4 -p taoxichuan --features no-tests`; install to `/boot/akuma-guest-dev` only, never
  `/boot/akuma-guest`. Never `cargo clean`.
- One `kvm0` VM at a time: `herd stop vm-serve-mc` before `herd enable vm-dev; herd start vm-dev` (re-run
  `herd enable` after editing the conf: the enabled copy is a snapshot); when done `herd stop vm-dev; herd disable
  vm-dev; herd start vm-serve-mc`. **`vm-serve-mc` must be running when you finish, and `vm-dev.conf` back on the
  stage-1 busybox line.** Never `pkill` by name; kill by pid; never pipe a long-running command into `grep -q`.
- Never run `goose`, `goose-kimi` or `crush` wrappers on the box and do not touch `/root/.config/goose/` (it holds the
  key your own server reads).
- Boot the main amd64 kernel once as a guest at the end (`kbuild -j 4 --features no-tests`; `AKUMA-GUEST-UP` ->
  `AKUMA-GUEST-DONE`) to prove nothing broke.

## 4. Paperwork (commit early, commit often)

1. Gate evidence in `/root/dev-cycle/taoxichuan-s4/`: the clean single-boot console log with its zero-fault grep,
   the reference outputs from `vm-serve-mc`, the guest outputs, `SHA256SUMS`, the main-kernel boot log.
2. A dated `### Stage 4 - metal track (goose-glm)` section at the end of
   `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md` (after merging `origin`): what you composed, crates
   pulled or extracted, the design decision above, the gate evidence, surprises, your own cost if visible.
3. `/root/dev-cycle/taoxichuan-s4/RESULT.md`: PASS/FAIL, branch + commit, how it was booted, crate count and image
   size, and what you would tell the stage-6 prompt (`apk add curl`: the guest needs outbound networking through
   `vm_outbound = true`, DNS, TLS, an apk index, and `fork`/`wait` in earnest for `apk` and `tar`).

You are on `glm-5.3-flash` at low effort. Explore as much as you need; the quota is the budget and the owner wants
to see how far it goes. Work autonomously until the gate passes or you are genuinely blocked; then write the result
file, commit, push, and end the turn.
