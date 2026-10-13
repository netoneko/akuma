# Akuma as a drop-in Gentoo kernel — a stage3 in a box

**Date:** 2026-10-14 (in progress). **Where:** ryzen metal, entry 14
(`smp=8`, wifi), kernel `44e0b93d-release-smp-shared` — the current `ugh` HEAD
with the box fixes. **Question:** can Gentoo's own userspace (OpenRC, portage,
openssh) run unmodified on the Akuma kernel, the way it runs on Linux — i.e.
is Akuma a drop-in kernel for a Gentoo root?

The shape of the test is a **herd box**, not a bare-metal root swap: the stage3
lives at `/opt/gentoo/root` on p3, herd boots it (`boxed = true`) with `/proc`
and `/tmp` mounted in from box 0, OpenRC runs its runlevels inside, and the
box's openssh answers on **4444** (host sshd keeps 2222; Pop owns 22 when the
laptop boots back into it). Whatever breaks is the finding.

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

Found while planning; each one shapes what the box can and cannot do.

| fact | source | consequence |
|---|---|---|
| No `mknod(2)` anywhere | grep over the whole tree | device nodes cannot be created at runtime, in a box or out |
| ext2 has no char-device dispatch | grep in `akuma-ext2` | static nodes baked into an image would not dispatch anyway |
| **Boxes get no synthetic `/dev`** | `crates/akuma-syscalls-glue/src/fs.rs:2037` (`dev_node` arm is host-only, box_id 0) | `/dev/null` inside any box does not exist |
| `/dev` is not a filesystem | `crates/akuma-vfs-glue/src/lib.rs:1172` — a resolve-time virtual dir, not a mountable fstype | `MOUNT_IN_NS(box, "/dev", …)` has nothing to mount; no devfs-in-box |
| herd's `mount =` knows `proc` and `tmpfs` only | `userspace/herd/src/main.rs:933` (`proc`→`/proc`, `tmpfs`→`/tmp`) | no third fstype to lean on |
| The terminal is a **channel property**, not a device | `archive/BOX_PTY_INTERACTIVE_SHELL.md` — `SPAWN_FLAG_PTY`, the kernel line discipline runs on the spawn channel | openssh's `openpty()` cannot work in a box (no `/dev/ptmx`, and none is coming); Akuma's own sshd never needed one |
| Boxed services share box 0's smoltcp stack on NIC0 | `docs/reference/subsystems/networking.md` § The two stacks; the boxed-redis end-to-end | a boxed sshd on 4444 is reachable from the LAN — no port mapping needed |
| herd has native box services | `containers.md` § herd; `overlays/devbox/rootfs/etc/herd/available/redis-boxed.conf.example` | `boxed`/`box_root`/`mount`/`restart` **is** the "comes up automatically" mechanism; the box registers under the service name, so `box use gentoo …` works |

**The `/dev` shim.** Since a box has no `/dev` and cannot be given one, the
root gets plain files and `/proc` symlinks (the box *does* have `/proc`):

```
/opt/gentoo/root/dev/null, zero, random, urandom   regular files (empty)
/opt/gentoo/root/dev/fd                            -> /proc/self/fd
/opt/gentoo/root/dev/std{in,out,err}               -> /proc/self/fd/{0,1,2}
```

A real `null` swallows nothing and grows; `< /dev/null` reads EOF; `2>/dev/null`
truncates. Wrong in the general case, good enough to let thousands of scripts
and `open("/dev/null")` calls succeed. `/dev/console` deliberately not
provided (this test boots OpenRC from a script; nothing reads it). A real fix
— a mountable devfs, or mknod + chrdev dispatch — is a kernel work item, not
a shim; this test exists partly to price that work.

Interactive ssh into the box is therefore expected to fail at `openpty` even
if sshd starts; `ssh -p 4444 root@… '<command>'` (no pty requested) is the
path that should work.

## Progress so far

1. **Verified the download** (above) — the truncated file is the kind of
   thing that would have surfaced hours later as `tar: unexpected EOF`.
2. **Extracted on-metal**: `unxz -c <tar.xz> | tar -x` into `/opt/gentoo/root`
   (busybox 1.35 userland, `unxz` present). Device nodes fail with
   `Function not implemented` — expected, that is the mknod gap; everything
   else extracts. (RC and file count to be recorded when extraction
   completes.)
3. **Still open — the plan being executed:**
   - `/etc/resolv.conf` (1.1.1.1, 8.8.8.8 — copied from the host's) and
     `/etc/hosts` into the root; the stage3 ships none that work in a box.
   - the `/dev` shim above,
   - `authorized_keys` for root (the Mac's ed25519 key, the same one the
     host sshd accepts on 2222),
   - `/box-init.sh` in the root: `openrc sysinit`, `openrc boot`,
     `openrc default`, then idle in a loop (the box's primary must stay
     alive), logging to `/box-init.log` on ext2 where the host can read it,
   - herd service `gentoo.conf`: `command = /bin/sh`, `args = /box-init.sh`,
     `boxed = true`, `box_root = /opt/gentoo/root`, `mount = proc tmpfs`,
     `restart = true` — enable + `herd start gentoo` (loopback 7117), no
     reboot needed; after a reboot it comes up on its own,
   - **`emerge openssh` inside the box** — the real drop-in test: portage,
     python, the sandbox, gcc, openssl, all on Akuma syscalls. Likely
     trouble, in order: sandbox (`FEATURES=-sandbox` first try if
     `libsandbox` misbehaves on musl), missing `/dev`, fetch (stage3 ships
     `wget`; if not, distfiles can be fetched host-side into
     `/var/cache/distfiles` — the box root is plain ext2 the host can write).
   - `rc-update add sshd default`; `sshd_config`: `Port 4444`,
     `PermitRootLogin prohibit-password`; then from the Mac
     `ssh -p 4444 root@192.168.1.159 uname -a` should print an Akuma
     `uname` from a Gentoo userspace.

## Background

- [`AKUMA_AMD64_LAPIC_ONESHOT_STALE.md`](AKUMA_AMD64_LAPIC_ONESHOT_STALE.md) —
  where this stage file came from, and the box fixes this test inherits.
- [`AKUMA_AMD64_BOXES_AUDIT.md`](AKUMA_AMD64_BOXES_AUDIT.md) — boxes on amd64.
- [`../reference/subsystems/containers.md`](../reference/subsystems/containers.md)
  — the box model, herd service schema, mount policy.
- [`DEVFS_MISSING.md`](DEVFS_MISSING.md) — why `/dev` is virtual and host-only.
- [`BOX_PTY_INTERACTIVE_SHELL.md`](BOX_PTY_INTERACTIVE_SHELL.md) — the channel
  terminal, and why openssh's openpty cannot be reached from a box.
