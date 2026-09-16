# amd64: `sc-containers` — real `mount(2)`/`umount2(2)`, box lifecycle left unwired

**Date:** 2026-09-17
**Status:** landed in the working tree, **uncommitted** — the user drives commits on this repo.
**Scope:** `sc-containers` was the one syscall family
`docs/archive/AKUMA_AMD64_EPOLL_EVENTFD_FOLD.md` left gated off entirely. This
session wires the two real Linux calls in it (`mount`/`umount2`) and leaves
the three Akuma-private box-lifecycle calls (`register_box`/`kill_box`/
`reattach`) and `mount_in_ns` unreachable on purpose — amd64 has no "box"
concept for them to mean anything against yet, which is a design question for
the user, not a wiring task.

## What was wired

| syscall | x86_64 | asm-generic | dispatch |
|---|---:|---:|---|
| `mount` | 165 | 40 | `to_glue`, `syscall_table!` row |
| `umount2` | 166 | 39 | `to_glue`, `syscall_table!` row |

### `crates/akuma-syscalls-abi`

One new `── mount ──` section, two rows (`Mount`, `Umount2`), placed after the
`*at` path family since mount is the other filesystem-shaping syscall pair.
Plain arguments — `mount(source, target, fstype, flags, data)` /
`umount2(target, flags)` — no wire struct, so unlike `epoll_event` this needed
no x86_64 packing check. Confirmed against real musl headers rather than
trusted from memory (same discipline as every prior pass):
`/opt/homebrew/Cellar/musl-cross/0.9.11/libexec/{x86_64,aarch64}-linux-musl/include/bits/syscall.h`
— `SYS_mount`/`SYS_umount2` are 165/166 on x86_64 and 40/39 on aarch64, matching
`akuma-syscalls-linux::nr::MOUNT`/`UMOUNT2`, which already existed (the
AArch64 kernel has had `sc-containers` all along). 18/18 host tests still
pass; `tables_disagree_where_linux_does` and `every_number_is_claimed_once`
cover the two new rows without needing new test bodies, same as the epoll
pass.

### `amd64/src/usermode.rs`

Two `to_glue` arms next to `Renameat` (the other real-fs family), and three
new `dispatch_smoke_test` checks that need no filesystem (`hop()` for both
rows, plus `umount2("/")` == `EBUSY` — the global-root guard fires before any
mount-table lookup, so it runs even with `have_fs == false`), and a full live
tmpfs mount/write/read/unmount round trip gated behind `have_fs`, observed
through the same native `crate::fs::*` helpers the redirect suite above it
already uses (`create_dir`/`write_file`/`read_file`/`remove_dir`) rather than
a hand-rolled `openat`/`read`/`write` sequence. 10 new checks total.

**No box-lifecycle arms were added.** `register_box`(316)/`kill_box`(317)/
`reattach`(318)/`mount_in_ns`(325) are Akuma-private numbers reached through
`usermode.rs`'s `AKUMA_PRIVATE_BASE` match (`nr - 0x1000`), which lists
specific arms (300/301/302/303/313/319/322/326) and falls through to `ENOSYS`
for everything else. Turning `sc-containers` on in `akuma-syscalls-glue` does
not change this — that feature only gates whether `container.rs` **compiles**
and is reachable via the neutral `mount`/`umount2` numbers, not whether the
private-syscall match has arms for the other four. See "What's still gated
off" for why they should stay that way.

### `amd64/Cargo.toml`

New `sc-containers` feature, added to `default`:

```
sc-containers = ["akuma-syscalls-glue/sc-containers", "akuma-vfs-glue/sc-containers"]
```

Both forwards are required, and finding that out was the actual compile-risk
check this session did first (see next section) — `akuma-vfs-glue/sc-containers`
is not optional here.

## The compile risk that was checked and did not materialize

The task brief for this session flagged a real concern: `container.rs`'s
`sys_mount` has an `fstype == "proc"` arm that constructs
`akuma_vfs_glue::proc::ProcFilesystem::new()`, and — per older docs — that
type was believed not to compile for `x86_64-unknown-none` at all, which
would make `sc-containers` fail to build for amd64 independent of the "does
amd64 have boxes" question.

**That belief was already stale before this session touched anything.**
`crates/akuma-vfs-glue/Cargo.toml`'s own dependency comment on `akuma-procfs`
carries a correction dated 2026-09-07:

> **Corrected 2026-09-07:** this comment used to claim the crate "cannot
> itself be compiled for `x86_64-unknown-none`". That stopped being true when
> B3 gave `akuma-mmu` its x86 `UserAddressSpace`; `cargo check -p
> akuma-vfs-glue --target x86_64-unknown-none` passes, and the amd64 kernel
> mounts its root into this crate's table as of C1 step 4a. `proc::ProcFilesystem`
> is still not mounted there, but for a different reason — it reports on
> `akuma-exec` processes, which that target does not yet register.

Verified directly rather than trusted either way, per the task's own
instruction to investigate before picking a path:

```
cargo check -p akuma-syscalls-glue --no-default-features \
  --features smoltcp,sc-containers,akuma-vfs-glue/sc-containers \
  --target x86_64-unknown-none
```

**Clean, zero errors**, `container.rs`'s `proc` arm included. The one thing
the task's stale premise got right: the *forwarding* (root `Cargo.toml` does
`akuma-vfs-glue/sc-containers` for the AArch64 binary; amd64's own manifest
had simply never defined an equivalent feature) is real and required — the
standalone check without it fails "item configured out", because
`mount_in_namespace`/`unmount_in_namespace`/`create_box_namespace`/
`remove_box_namespace`/`replace_box_root` inside `akuma-vfs-glue` are
themselves behind that crate's own `sc-containers` gate. `container.rs`'s
`sys_mount`/`sys_umount2` do not call any of those five — they use
`mount_with`/`unmount`/`remount`/`metadata`/`get_root_fs`, which are
unconditional in `akuma-vfs-glue` — so the forward is needed for the crate to
compile as a whole (`mod container` is one `#[cfg(feature = "sc-containers")]`
block covering the box-lifecycle functions too), not because `mount`/`umount2`
themselves reach the gated functions.

A second, smaller stale-comment correction fell out of the same finding:
`amd64/Cargo.toml`'s `sc-sysv-ipc` comment repeated the old "does not build
for `x86_64-unknown-none` at all" claim about `ProcFilesystem` to explain why
`akuma-vfs-glue/sc-sysv-ipc` is not forwarded. Corrected in place (dated) —
the real reason `/proc/sysvipc/msg` has nothing to reach on this target is
that `ProcFilesystem` is not *mounted* here (it renders from `akuma-exec`
process state through a `/proc` this target does not wire up), not that it
fails to build.

Also checked and unnecessary as a result: no `#[cfg(target_arch)]` carve-out
of the `"proc"` match arm inside `container.rs` — the type builds fine, so
there was nothing to gate. If amd64 ever mounts `/proc` for real, the
existing `sys_mount` code needs no change to serve it (it would report an
empty tree until amd64 registers processes into `akuma-exec`'s table the way
the `akuma-procfs`/`akuma-vfs-glue` comments already describe — a separate,
already-known gap, not a new one this session found).

## `caller_may_mount`: why this target's single-box world makes mount/umount2 unconditionally usable

`sys_mount`/`sys_umount2` both gate on `caller_may_mount()`:

```rust
fn caller_may_mount() -> bool {
    akuma_exec::process::current_process_shared().is_none_or(|p| p.box_id == 0)
}
```

Every process amd64 creates is hardcoded `box_id: 0` (`net.rs`, `usermode.rs`
— "no containers" by explicit design, per the task brief's own finding), and
`is_none_or` also passes when there is no registered process at all (the boot
task). So the permission check can never refuse a real call on this target —
which is the correct behavior for a kernel with exactly one box, and is also
why the boot-suite live round trip below needed no `userspace/` probe to
exercise the *positive* path (unlike `io_setup`'s stricter
`lookup_process_shared`, which the boot task does not answer to — see the
epoll doc's Follow-up 2). A ring-3 probe was still written and run, to prove
the real syscall entry path and not just the internal `syscall_dispatch`
function call the boot suite uses directly — see "Live verification" below.

## Live verification

Same rig as the epoll/eventfd session (`amd64/run.sh`, PVH under QEMU/TCG,
`SSH_PORT=2322 HTTP_PORT=8180 INIT=/bin/sshd`), started and torn down cleanly
in this session (`kill` on the exact pid holding `hostfwd=tcp::2322-:2222`,
never a blanket `pkill`).

| check | result |
|---|---:|
| `cargo test -p akuma-syscalls-abi` (host) | 18/18 pass |
| `cargo test -p akuma-syscalls-linux -p akuma-syscalls-glue` (host) | 43/43 pass, including `akuma_private_syscalls_stay_in_their_block` |
| `cargo check -p akuma-syscalls-glue --features …,sc-containers,akuma-vfs-glue/sc-containers --target x86_64-unknown-none` | clean |
| `cargo clippy`, `akuma-syscalls-abi`, both `x86_64-unknown-none` and `aarch64-unknown-none` | clean |
| `cargo build --release --target x86_64-unknown-none` in `amd64/` | clean, binary links |
| `cargo clippy --release --target x86_64-unknown-none` in `amd64/` | clean (one `manual_c_str_literal` warning surfaced and was fixed by binding the literal to a `let`, matching this file's existing style for every other path literal) |
| `cargo build --release` at repo root (AArch64 kernel, unaffected) | clean |
| **Boot suite, QEMU/TCG, `SSH_PORT=2322`** | **745 passed, 0 failed** (735 + the 10 new checks below) |
| `hop()` checks: `mount 165 -> 40`, `umount2 166 -> 39` | both `[OK]` |
| `umount2("/")` is `EBUSY`, with no disk/`have_fs` dependency | `[OK]` |
| **Live tmpfs round trip in the boot suite**: mkdir a probe dir, `mount(tmpfs)`, write a file, read it back, `umount2`, confirm the file is gone, remove the probe dir | all 7 sub-checks `[OK]` |
| **Live ring-3 probe**, cross-built `x86_64-linux-musl-gcc -static`, pushed over ssh with `cat` (not base64 — the guest busybox has no `base64` applet) | `mkdir`, `mount(tmpfs)`, write, readback, bad-fstype is `ENODEV`, `umount2`, file gone (`ENOENT`), `umount2("/")` is `EBUSY` — all `OK`, process exits 0 |

The ring-3 probe is the stronger proof: it goes through the real `int`/`syscall`
entry path with musl's actual `mount(2)`/`umount2(2)` libc wrappers, not the
boot task calling `syscall_dispatch` as a plain function call. It was written
for this session and run from the scratch directory, not added to
`userspace/` — this family doesn't have an existing probe tree the way
`userspace/epollprobe/` does, and adding one is out of scope unless asked.

## Firecracker: run, on real KVM, on the bare-metal reference box's Ubuntu side

The first attempt at this repeated the prior session's mistake: `~/.ssh/config`
only lists the two unreachable private AWS addresses plus an `akuma` alias for
`192.168.1.123`, and that box was assumed to be an Akuma **target** only. It
is also the Firecracker **host** — the same physical HP 500-502nj
("the trashcan") boots two personalities on one IP, and its *Ubuntu* side
(port 22, hostname `vaporwave`) is a real x86_64 Linux machine with `/dev/kvm`
and `/usr/local/bin/firecracker` already installed
(`docs/runbooks/amd64-bare-metal-loop.md`, `scripts/utils/hpbox.py`) — exactly
what `amd64/run-firecracker.sh`'s own header asks for. The user pointed this
out directly rather than letting the report stand.

**The ssh-config trap named in `hpbox.py`'s own docstring is real and bit this
attempt on the first try.** `~/.ssh/config` has:

```
Host akuma 192.168.1.123 192.168.1.220
    Port 2222
    IdentityFile ~/github.com/netoneko/akuma/target/x86_64-unknown-none/release/amd64-ssh-test-key
    StrictHostKeyChecking no
```

That `Host` line matches the bare IP as one of its patterns, so any `ssh`/`scp`
to `192.168.1.123` that does not explicitly pass `-p` silently inherits `Port
2222` from this block and lands on the **Akuma** personality (whatever kernel
is currently booted there) instead of Ubuntu — the config fills in unspecified
options, and `run-firecracker.sh`'s own `$SSH`/`scp` invocations never pass
`-p`/`-P`. `amd64-bare-metal-loop.md` documents this exact trap for interactive
use (`-F /dev/null` for the Ubuntu side); it applies just as much to a script
that hardcodes its own ssh flags. Worked around **without editing the tracked
script**: a scratch copy of `run-firecracker.sh` with `-F /dev/null -p 22`
(`-P 22` for `scp`) added to its `SSH`/`scp` invocations, and its relative
`HERE=$(dirname "$0")`/`mkdisk.sh` paths fixed up for running from outside
`amd64/`. Confirmed first, over ssh with the same explicit `-p 22 -F
/dev/null`: `/dev/kvm` present, `firecracker` at `/usr/local/bin/firecracker`,
`/root/akuma` a live git checkout (the bare-metal loop's deployment target,
not needed for this path — `run-firecracker.sh` builds locally and `scp`s the
already-built ELF + disk image, it does not build on the box).

```
FC_HOST=root@192.168.1.123 FC_KEY=~/.ssh/id_ed25519 TIMEOUT=60 INIT=/bin/paws \
    sh <patched-copy-of-run-firecracker.sh>
```

Booted clean on the box's real KVM under Firecracker (`vcpu_count: 1`, no
network — `FC_NET` unset):

| check | result |
|---|---:|
| Boot suite | **723 passed, 0 failed**. Lower than QEMU's 745 for reasons that are environment, not regression: no `FC_NET` here (this run's `machine-config` has no `network-interfaces`), so `net: netpoll skipped (no stack)` and every net-dependent check the QEMU run's slirp NIC enables are absent — a difference already documented for this exact family of comparison in `AKUMA_AMD64_EPOLL_EVENTFD_FOLD.md`'s Firecracker-vs-QEMU table. |
| `[syscall] no row for x86_64 nr=40 — returning ENOSYS` | Present on **both** rigs (checked against this session's own QEMU boot log) — x86_64 40 is `sendfile`, an unrelated, pre-existing gap, not something this change touched or introduced. |
| **All 10 of this session's new checks** (`dispatch: mount 165 -> 40`, `dispatch: umount2 166 -> 39`, `umount2("/")` `EBUSY`, and all 7 steps of the live tmpfs round trip) | **all `[OK]`**, grepped directly out of `~/akuma/boot.log` on the box after the run |

The `mount`/`umount2` family is not itself timing-sensitive (unlike the BKL/
scheduler work `amd64-bare-metal-loop.md`'s "fast lane" section warns can
diverge between TCG and KVM), so the ring-3 musl probe was **not** re-run
against this boot — repeating it would need `INIT=/bin/sshd` plus `FC_NET=1`
and `amd64/net-setup.sh`'s tap/DHCP setup on the box, and the boot suite's own
live round trip already exercises the identical code path (`syscall_dispatch`
by the real x86_64 number, through `container::sys_mount`/`sys_umount2`, into
`akuma_vfs_glue`) that QEMU's ring-3 probe validated over ssh. The disk image
attached is `mkdisk.sh`'s default 128 MiB image built fresh by the (patched)
script, not the box's persistent USB disk from the bare-metal GRUB loop — no
persistent-root state was touched.

No firecracker process from this run was left behind: `run-firecracker.sh`'s
own `TIMEOUT=60`/`timeout --foreground` ended it before the script returned,
confirmed by `pgrep -a -f 'akuma/akuma-vm.json'` matching only the `pgrep`
command's own argv (the exact self-match trap
`amd64-bare-metal-loop.md` warns `pkill -f` falls into) once the run had
finished. A **different**, pre-existing `firecracker --config-file
/root/akuma-fc.json` process was found running on the box, left over from
unrelated prior activity — not touched, per the rule against acting on
unfamiliar state without knowing whose it is.

## What's still gated off, and why it should stay that way without a design decision

`register_box`(316) / `kill_box`(317) / `reattach`(318) / `mount_in_ns`(325)
now **compile** on amd64 (they're in the same `sc-containers`-gated module as
`mount`/`umount2`) but are **not dispatched** — no arm exists for their
numbers in `usermode.rs`'s `AKUMA_PRIVATE_BASE` match, so a userspace call to
any of them is `ENOSYS`, unchanged from before this session.

This is deliberate, not an oversight: every process this target creates has
`box_id: 0` and there is no second box for `register_box` to create alongside
it, no isolated namespace for `mount_in_ns` to compose, and nothing for
`reattach`/`kill_box` to target or detach from. Wiring their numbers would
compile-check fine and then either no-op or do something contradicting the
"single box" invariant the rest of this target's syscall surface assumes
(`msgget`'s `current_box_id()`, `caller_may_mount`'s box-0 check, `net.rs`'s
`current_box_id: || 0`). Whether amd64 should grow a real second-box concept
— and what it would even isolate, given there is no per-box network namespace,
process tree scoping, or `/proc` on this target yet — is a design question
substantially larger than "wire a syscall," and per this session's brief is
left for the user to decide rather than invented here.

## What's left

- **The box-lifecycle design question** above — needs a decision from the
  user on scope (now / later / never), not more wiring. Firecracker
  verification is no longer a blocker — see above; `FC_HOST=root@192.168.1.123`
  (Ubuntu side, port 22, needs the ssh-config workaround described above) is
  a real, reachable Firecracker host on this network.
- Every syscall `akuma-syscalls-glue` implements is now reachable from amd64
  in some form: `sc-containers`'s box-lifecycle quarter is a documented,
  deliberate exception, not a gap that looks like one.
- `brk`(12) is still the one syscall-table row with no amd64 arm at all
  (`AKUMA_AMD64_EPOLL_EVENTFD_FOLD.md`'s finding, unrelated to this session).
- A durable fix for the ssh-config trap this session hit is out of scope here
  but worth naming: either `run-firecracker.sh` gains an `FC_SSH_OPTS`/
  explicit `-p` passthrough, or the trashcan box gets a distinct DNS name/IP
  for its Ubuntu side so `Host akuma 192.168.1.123 …`'s pattern stops also
  matching the host meant for Firecracker. Left as a finding, not fixed,
  since it touches a shared script and a shared `~/.ssh/config` beyond this
  task's scope.

## Background

- `docs/archive/AKUMA_AMD64_EPOLL_EVENTFD_FOLD.md` — the session this one
  continues; its "Whole families still gated off" section is what identified
  `sc-containers` as the remaining family and the `akuma-vfs-glue` forwarding
  gap this session confirmed and fixed.
- `docs/archive/AKUMA_AMD64_C1_DISPATCH_VOCABULARY.md` — the `Syscall`
  table/`to_glue` shape both sessions extend.
- `docs/archive/MOUNT_MISSING_SYSCALLS.md` — the original `mount`/`umount2`
  implementation on the AArch64 side, including the `mount_errno` mapping
  reused unmodified here.
