# Akuma as a drop-in Gentoo kernel — a stage3 in a box

**Date:** 2026-10-14. **Where:** ryzen metal, entry 14 (`smp=8`, wifi).
**Question:** can Gentoo's own userspace (OpenRC, portage, openssh) run
unmodified on the Akuma kernel, the way it runs on Linux — i.e. is Akuma a
drop-in kernel for a Gentoo root?

**Status at the break:** the box **boots, auto-starts with the metal, runs
OpenRC to `openrc done`, and executes Gentoo binaries** — python 3.14.7,
bash, TCP, DNS and a fresh `/dev` all work inside it. The single blocker to
`emerge openssh` is a deterministic native crash in python C extensions
(`_blake2` SIGSEGV; emerge SIGILL) — full investigation, ruled-out causes
and resumption recipe in
[`AKUMA_GENTOO_PORTAGE_CRASH.md`](AKUMA_GENTOO_PORTAGE_CRASH.md).

The shape of the test is a **herd box**, not a bare-metal root swap: the stage3
lives at `/opt/gentoo/root` on p3, herd boots it (`boxed = true`) with `/proc`
and `/tmp` mounted in from box 0, OpenRC runs its runlevels inside, and the
box's openssh is to answer on **4444** (host sshd keeps 2222; Pop owns 22 when
the laptop boots back into it). Whatever breaks is the finding.

## Why a box and not `root=/dev/nvme0n1p3`

Akuma's init is herd; a Gentoo root wants OpenRC as init. The box gives the
same kernel surface a jailed container gets — namespace, `/proc`, the shared
smoltcp stack — while p3's real Akuma root stays bootable. If the userspace
compat is proven in the box, a bare-metal Gentoo root is a follow-up.

## The download

`stage3-amd64-musl-hardened-openrc-20261011T170049Z.tar.xz` — the same file
the 2026-10-13/14 network test used
([`AKUMA_AMD64_LAPIC_ONESHOT_STALE.md`](AKUMA_AMD64_LAPIC_ONESHOT_STALE.md)).

- **The first one was truncated.** 218 692 940 B on disk, the `.aria2` control
  file still beside it, and `aria4.log` ended `INPR` at 11 % — the "240 s
  capped sample" run of the LAPIC test was killed mid-transfer and left
  behind. Its sha256 (`230333d3…`) matched nothing. The control file is the
  tell: aria2 removes it on completion; a `.aria2` beside a stage file means
  *not done*, whatever `ls` says (aria2 preallocates, so size alone lies).
- **Redownloaded fresh** (not resumed) at ~258 KiB/s after moving the laptop
  closer to the router — 2× the 111–120 KiB/s ceiling measured at the old
  spot. Full file in ~16 min, `OK`, control file gone.
- **Verified against Gentoo's PGP-signed `.DIGESTS`**: sha512
  `f116e69f6f038…450ca69c`, size 244 518 672 B, both exact. `sha512sum` on the
  metal (busybox) took ~1 min.

Gentoo publishes SHA512 and BLAKE2B, not SHA256 — the LAPIC doc's `2004735a…`
was a locally computed sha256 from the earlier whole-file run. Verify against
`.DIGESTS` from the mirror, not against a note.

## What the kernel source already said (before running anything)

Found while planning; each one shapes what the box can and cannot do. The
first three were **fixed this session** (see below).

| fact | source | consequence |
|---|---|---|
| ~~No `mknod(2)` anywhere~~ | grep over the whole tree | device nodes cannot be created at runtime — motived the box `/dev` instance |
| ext2 has no char-device dispatch | grep in `akuma-ext2` | static nodes baked into an image would not dispatch anyway |
| ~~**Boxes get no synthetic `/dev`**~~ | `crates/akuma-syscalls-glue/src/fs.rs:2037` (`dev_node` arm is host-only, box_id 0) | ~~`/dev/null` inside any box does not exist~~ **fixed:** a box now gets a fresh instance of the standard set |
| `/dev` is not a filesystem | `crates/akuma-vfs-glue/src/lib.rs:1172` — a resolve-time virtual dir, not a mountable fstype | the box `/dev` is served by widening the same resolve-time layer, not by mounting |
| herd's `mount =` knows `proc` and `tmpfs` only | `userspace/herd/src/main.rs:933` (`proc`→`/proc`, `tmpfs`→`/tmp`) | no third fstype to lean on |
| The terminal is a **channel property**, not a device | `archive/BOX_PTY_INTERACTIVE_SHELL.md` — `SPAWN_FLAG_PTY`, the kernel line discipline runs on the spawn channel | openssh's `openpty()` opens `/dev/ptmx` — served in-box now; whether a boxed sshd gets working ptys is **untested** (openpty returns fds, not channels) |
| Boxed services share box 0's smoltcp stack on NIC0 | `docs/reference/subsystems/networking.md` § The two stacks; verified live: boxed TCP to `1.1.1.1:53/80/443` all connect | a boxed sshd on 4444 is reachable from the LAN — no port mapping needed |
| herd has native box services | `containers.md` § herd; `redis-boxed.conf.example` | `boxed`/`box_root`/`mount`/`restart` **is** the "comes up automatically" mechanism; the box registers under the service name, so `box use gentoo …` works |

An interim regular-file `/dev` shim (empty `null`/`zero`/`random`/`urandom`
plus `/proc/self/fd` symlinks) carried the first bring-up before the kernel
change landed; the re-extract from Pop dropped the regular-file half and kept
only the `/proc` symlinks, which the fresh instance now complements.
Interactive ssh into the box is still open: openssh's `openpty()` opens
`/dev/ptmx` + `/dev/pts/N`, which the fresh instance serves — but akuma's
terminal is normally a spawn-channel property, so whether a *boxed* openssh
gets working ptys is untested. `ssh -p 4444 root@… '<command>'` (no pty
requested) is the path expected to work regardless.

## Where the box stands (verified live, in-box)

| works | evidence |
|---|---|
| auto-start with the metal | herd `gentoo: running pid=…` on every boot; box in `/proc/boxes` with a live primary |
| OpenRC | `sysinit`/`boot`/`default` run to `openrc done`; only `save-keymaps` fails (needs a real console) |
| usr-merged paths | `/bin/sh`, `/usr/bin/python3.14`, all through `sbin`/`bin` links |
| python 3.14.7 | `--version`, imports, `os.listdir`, `mmap`, site-packages |
| `/dev` | fresh instance: proper char metadata, `null` write, `urandom` entropy |
| networking | TCP to `1.1.1.1:53/80/443` from bash `/dev/tcp`; DNS resolves `distfiles.gentoo.org` through its CDN CNAME |
| portage's module | `import portage` → `PORTAGE-OK 3.0.82.2` |

| blocked | by |
|---|---|
| `emerge openssh` (and thus the sshd-on-4444 payoff) | **the C-extension crash** — `_blake2` SIGSEGV, emerge SIGILL — see [`AKUMA_GENTOO_PORTAGE_CRASH.md`](AKUMA_GENTOO_PORTAGE_CRASH.md) for the discriminators, ruled-out causes, and the resume recipe |
| interactive ssh into the box | unknown — openssh not installed yet; `openpty` via the in-box `ptmx` is plausible but untested |

Resumption recipe: `box use gentoo /bin/sh -c 'FEATURES=-sandbox
MAKEOPTS=-j6 emerge --oneshot net-misc/openssh > /emerge.log 2>&1'`, poll
`/opt/gentoo/root/emerge.log` from the host (the box root is plain p3),
then `rc-update add sshd default`, set `Port 4444` +
`PermitRootLogin prohibit-password` in `sshd_config`, restart the box, and
from the Mac: `ssh -p 4444 root@192.168.1.159 uname -a` — an Akuma `uname`
from a Gentoo userspace.

Two adjacent kernel findings this session surfaced, both recorded in
[`AKUMA_GENTOO_PORTAGE_CRASH.md`](AKUMA_GENTOO_PORTAGE_CRASH.md): spawned
children do not share the inherited fd's file offset (POSIX says they
must; script log files tear), and akuma's ext2 directory writes are not
Linux-clean (htree/filetype corruption Pop's fsck had to repair; the tree
was re-extracted from Pop and verified complete against the manifest).
Probe scripts and outputs from this session remain in the box root
(`/opt/gentoo/root/p*.sh`, `s*.txt`, `h-*.txt`, `e*.txt`).

## The blocker the first boot found: intermediate symlinks

`herd start gentoo` registered the box (`/proc/boxes`:
`3045470212,gentoo,/opt/gentoo/root,1,0`) and then failed to spawn `/bin/sh`.
The extraction was **not** the problem — checking the stage3's own
`.CONTENTS.gz` against the tree showed every symlink correct, including the
two that looked wrong: `sbin -> usr/bin` and `usr/sbin -> bin` are Gentoo's
2026 sbin-merge, verbatim from the tarball.

The real defect is in the kernel's path resolution, and a controlled test on
the metal pins it exactly (`/tmp/symtest`, symlink `real`):

| path | result |
|---|---|
| `/tmp/symtest/real/marker` (no link) | OK |
| `/tmp/symtest/rel` (link as **final** component) | OK |
| `/tmp/symtest/rel/marker` (link **interior**) | ENOENT |
| `open` / `write` through it | ENOENT / ENOTDIR |
| absolute target, `..`, CWD-relative | same failures |

`resolve_symlinks` (`crates/akuma-vfs-glue/src/lib.rs:1093`) canonicalized the
whole path once and only asked "is the *final* path a symlink?" — following
the link and **dropping every component that came after it**. `/bin/sh`
through `bin -> usr/bin` resolved to `/usr/bin`; the `sh` vanished. The akuma
rootfs has no symlinks at all (`ls -la /` shows none), which is why eight
months of boxes, `box pull`, nginx and redis never tripped it; the amd64 boot
suite's symlink test only ever exercised a final-component link, so it passed
too. A usr-merged Gentoo tree crosses such a link on **every** binary path.

## The fix (`fix/intermediate-symlinks`, commit `0d58f739`)

The walk moves into host-testable `akuma-vfs` as
**`resolve_symlink_chain(path, max_hops, read_link)`** — the walker owns the
splicing (component-by-component, target joined against the link's directory,
absolute targets restarting at `/`, `max_hops` bounding loops at the link
itself), the glue (`akuma-vfs-glue::resolve_symlinks`) supplies the
`read_link` answers and keeps the devbox `/bin/sh` → `/bin/dash` redirect.
Both kernels get it through the one choke point: aarch64 wires it in
`akuma-kernel-glue`, amd64 in `amd64/src/exec_runtime.rs`.

Tests:

- **12 host tests** in `akuma-vfs` (`symlink_chain_tests`): the lost suffix,
  relative and absolute targets, chains, the sbin-merge shape from the stage3,
  ELOOP bounded, `max_hops` honored, textual `..` pinned as known divergence.
- **amd64 boot suite** (`amd64/src/fs.rs`): a link interior to a path —
  suffix carried through, bytes read through the open that used it. This runs
  in the QEMU rehearsal and on every plain-kernel boot.

Gates: per-crate clippy `-D warnings` clean; full host suite green (187
binaries); both amd64 kernels build.

## Background

- [`AKUMA_AMD64_LAPIC_ONESHOT_STALE.md`](AKUMA_AMD64_LAPIC_ONESHOT_STALE.md) —
  where this stage file came from, and the box fixes this test inherits.
- [`AKUMA_AMD64_BOXES_AUDIT.md`](AKUMA_AMD64_BOXES_AUDIT.md) — boxes on amd64.
- [`../reference/subsystems/containers.md`](../reference/subsystems/containers.md)
  — the box model, herd service schema, mount policy.
- [`DEVFS_MISSING.md`](DEVFS_MISSING.md) — why `/dev` is virtual and host-only.
- [`BOX_PTY_INTERACTIVE_SHELL.md`](BOX_PTY_INTERACTIVE_SHELL.md) — the channel
  terminal, and why openssh's openpty cannot be reached from a box.
