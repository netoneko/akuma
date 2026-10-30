# Taoxichuan stage 4, part 2: the guest goes silent after the root mounts - iron out networking

You are goose-glm, the metal track of the experiment in
`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Your stage-4 session (sshd, exec-only ssh
round trip) was interrupted by the supervisor at 23:30 IDT to save quota. This prompt continues it. The spec for
the gate is still `scripts/kvm/goose/taoxichuan-04-sshd.md` in your checkout; read only what you need from it.

**Budget: roughly 25-30 minutes of work.** The window has about 28% left and stage 4 burned ~60% of a window per
hour. Spend it on the one failure below, not on reading or on paperwork. **Commit and `git push litter
taoxichuan-s4` every 10 minutes whenever anything is uncommitted** (WIP commits are fine).

Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Checkout: `/src/github.com/netoneko/akuma`, branch `taoxichuan-s4`. Box log timestamps
are UTC; the supervisor's are IDT (UTC+3).

## State, verified by the supervisor (do not redo)

- `taoxichuan-s4` @ `e418fd61` on the box, tree clean. That is your `0f1c9926` (`akuma-net` + `NetRuntime` in
  `net.rs`, `child.rs`, `sock.rs`, socket syscalls) plus a supervisor checkpoint commit of your later edits to
  `child.rs`, `main.rs` and `Cargo.lock`. **It is not pushed: `git push litter taoxichuan-s4` first.**
- The supervisor does not know whether this tree builds cleanly or which of your last edits were reverted.
- `/etc/herd/available/vm-dev.conf` is your stage-4 config (`init=/bin/sshd initargs=--port,22`, disk
  `/boot/akuma-dev-root-ssh.img`, forward `2224:22`). Leave it. `vm-dev` is **disabled**, `vm-serve-mc` is
  **running** (the supervisor restored both).
- **The lead.** Your last boot (about 23:20 IDT, kernel `/boot/akuma-guest-dev` 604,696 B) logs, in
  `/var/log/herd/vm-dev.log`: banner line `Taoxichuan/amd64 -- long mode reached`, the machine description,
  `heap`, `pmm`, `blk: 2 virtio slot(s) ... irq 5`, `[Block] Registered vda: 16 MB`, `fs: ext2 root mounted on
  /dev/vda` - and then **nothing**: no `Taoxichuan` banner, no init spawn, no sshd. `vm-dev` ended with
  `last_exit=1`. Whatever runs between "ext2 root mounted" and the banner in `kmain` (your stage-4 bring-up
  order: the TSC/PIT calibration, the virtio-net probe, `NetRuntime` registration, `akuma_net` init) either
  hangs, faults silently or exits the VM. The host side prints `exc=` counters and the guest's last state when
  the VM stops: read the lines just above the end of the log first.

## What to do

1. `git push litter taoxichuan-s4` (see above). Then read the tail of `/var/log/herd/vm-dev.log` and `kmain`'s
   bring-up order in `taoxichuan/src/main.rs`; add `safe_print!` markers around each step between the ext2 mount
   and the banner if the log does not already say which one stops. One boot per hypothesis.
2. Fix it, then get the NIC up: the guest has 2 virtio-mmio slots (blk at slot 0, net should be slot 1). The NIC
   is polled; kvm0 never raises its interrupt line, and `netpoll` is driven from the `nanosleep` arms and
   `wait_until`. A static address is fine if DHCP is the obstacle (`vm_forward = 2224:22` and the host gateway
   decide what the guest needs; check how the full-kernel guest `vm-serve-mc` is configured, as the brief says).
3. Gate: from the box, `ssh -T -p 2224 root@localhost '<command>'` returns the command's output byte-equal to the
   reference, with zero `[Fault]`/`PANIC`/`[sig!]` lines in the guest console. A completed ssh command counts,
   never a log marker.
4. Boot dance, every time: `herd stop vm-serve-mc; herd enable vm-dev; herd start vm-dev`, read the log, then
   `herd stop vm-dev; herd disable vm-dev; herd start vm-serve-mc`. One `kvm0` VM at a time. Never `pkill` by
   name, never touch `/boot/akuma-guest`, never touch `/root/.config/goose`.

## Portability rules (unchanged)

All `asm!`/`global_asm!`/`naked_asm!`, port I/O and MSR code behind `#[cfg(target_arch = "x86_64")]`. An
amd64-only crate must NOT be in the root `Cargo.toml` `default-members`. `cargo check` does not assemble inline
asm, so it hides these breaks. Never push to `origin`; do not touch the dirty submodules.

## When to stop

- **Gate passes:** write `/root/dev-cycle/taoxichuan-s4/RESULT.md` (PASS, branch + commit, how it was booted, crate
  count and image size, and what stage 6 needs: outbound networking for `apk add curl`), commit, push litter,
  restore `vm-serve-mc`, end the turn. The supervisor writes the log section.
- **Not passed when you are about 20 minutes in, or the quota error appears:** write RESULT.md as **PARTIAL** -
  exactly which step now fails and the evidence, what works (boot, root mount, NIC probe, DHCP, socket, ...), and the
  next experiment you would run. Commit, push litter, restore `vm-serve-mc`, end the turn. A precise PARTIAL is a
  good outcome; a wedged VM or an unpushed tree is not.
