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

## Update 2026-10-15: openssh built on Pop, box turned off, bare-metal root planned

**Portage tree synced** onto the Gentoo root from Pop (the box root is plain
p3): `rsync://rsync.gentoo.org/gentoo-portage/` into
`/opt/gentoo/root/var/db/repos/gentoo` — 133 511 files, 249 570 318 B on the
wire (the tree is ~720 MB on disk; the `gentoo-latest` snapshot is 51 MB
compressed), ~16 min at ~260 KB/s over wifi. `etc/portage/repos.conf/gentoo.conf`
was copied from `usr/share/portage/config/repos.conf`.

**The workaround for the python crash: build on Pop, run on Akuma.** Portage's
`emerge` is python, so every python front end (eix only indexes; `pmerge`
is python too) hits the same `_blake2`/SIGILL kernel bug. Instead, chroot from
Pop's Linux kernel (`mount -t proc`, `--rbind /dev`, `sysfs`, a copied
`resolv.conf`) and `emerge --oneshot --getbinpkg net-misc/openssh`. It
installed `openssh-10.5_p1` from the binhost in under 20 s, exit 0. The result
is ordinary musl ELF that Akuma runs. Paludis (`cave`, C++) is the
non-python alternative but needs a working emerge to bootstrap. The native
`emerge` crash remains an open kernel bug; this only routes around it.

**sshd configured in the root, not yet run on Akuma:** `Port 4444`,
`PermitRootLogin prohibit-password`, `PasswordAuthentication no`, host keys
generated with `ssh-keygen -A`, the Mac's `id_ed25519.pub` in
`/root/.ssh/authorized_keys`, `rc-update add sshd default`, `sshd -t` clean.
`/etc/resolv.conf` in the root now holds Pop's resolver at the time.

**The herd box is off:** `/etc/herd/enabled/gentoo.conf` (a symlink) was
removed from p3; `available/gentoo.conf` is untouched. Re-enable with
`ln -s /etc/herd/available/gentoo.conf /etc/herd/enabled/gentoo.conf`.

**Next: a separate partition, booted as box 0.** The amd64 kernel already
parses `root=/dev/nvme0n1pN` and `init=` (`amd64/src/multiboot2.rs:992`), so
the plan is a new p7 carved from the unused tail of p3 (the partition is 279 GB
but its ext2 covers only 64 GB), the stage3 + portage + openssh copied onto it
with Pop's tools, and a second GRUB entry
`root=/dev/nvme0n1p7 init=/sbin/openrc-init` beside the existing Akuma one.
The kernel stays on the ESP, so no Akuma binaries land in the Gentoo root.
Wifi tooling goes in the same way. Not started: it needs a partition-table
change. Expected first-boot findings: OpenRC wanting `/sys` and devtmpfs,
and whether box 0's synthetic `/dev` suffices.

**Open: p3 is `not clean`.** `e2fsck -fn` from Pop reports HTREE damage in
`/opt/gentoo/root/etc/ssl/certs` and `/src/symlink-wt/docs/archive`, a missing
LARGE_DIR flag and stale free counts — the Akuma ext2 writer problem already
recorded in `AKUMA_GENTOO_PORTAGE_CRASH.md`, not caused by this session. No
repair has been run. Anything Akuma writes to a Linux-checked root should be
`e2fsck`-ed from Pop after each session.

## Update 2026-10-15 (later): Gentoo booted as box 0 — three blockers, three fixes

p7 (179 GiB, `GENTOO`, ext2) was carved from the unused tail of p3 (p3 shrunk
279 -> 100 GiB; its ext2 spans 64 GiB so no data moved; GPT backup at
`/root/nvme0n1-gpt-backup-2026-10-15.bin` on Pop). The stage3 + portage +
openssh were copied from p3's `/opt/gentoo/root` with `rsync -aHAX`, plus the
`wifi` tool (`/usr/local/bin/wifi`), `/etc/wifi` (copied, never printed) and the
rtw89 firmware. Menu **entry 17** (`overlays/ryzen/grub.cfg`):
`init=/usr/bin/bash initargs=/akuma-init.sh root=/dev/nvme0n1p7 rtw89wifi wdt smp=8`.
`/akuma-init.sh` starts `wifi auto` in a restart loop, then `openrc sysinit`,
`boot`, `default`, then sshd directly if OpenRC did not.

| # | blocker (as observed) | cause | fix |
|---|---|---|---|
| 1 | `[init] not on the disk: /bin/sh`, back to Pop | `run_init` used `read_file(path)`, which follows no symlink; Gentoo is usr-merged (`/bin -> usr/bin`, `sh -> bash`) | `run_init` resolves symlinks first, like `execve` (`amd64/src/usermode.rs`); the entry also names the real file (`/usr/bin/bash`) so it works on older kernels |
| 2 | `wifi: /dev/wifi0: open failed (-2)` in a loop; no network | the rtw89 loader looked up `/lib/firmware/rtw89/...` with `metadata`/`read_at`, neither follows links, and `/lib -> usr/lib`; "no firmware", so no `wifi0` registered | the loader resolves the path once and reads the resolved name (`amd64/src/rtw89.rs`, one `String` at boot) |
| 3 | `cannot start netmount as hostname would not start` | **`sethostname`/`setdomainname` were in neither syscall table** — no number, no variant, no handler — so OpenRC's `hostname` service got `ENOSYS` | rows in `akuma-syscalls-linux::nr` + `akuma-syscalls-abi` (82 rows), handler in `akuma-syscalls-glue::proc`, `uname` overlays the two fields; pure rule in `akuma_syscalls_linux::uts` with host tests; decode pinned in the abi tests and the amd64 boot suite |

Also learned: the boot log was torn because a background job shares the log fd
and spawned children do not share the inherited file offset (known,
`AKUMA_GENTOO_PORTAGE_CRASH.md`) — the init script logs with `>>` now. And a
mount made on Pop **propagates into every daemon's mount namespace**: after
`umount /mnt/p3` the filesystem stayed mounted in ~10 of them (`e2fsck`: "in
use", state "not clean") until reboot. Do all p3/p7 work inside
`unshare -m --propagation private`.

**Result of the first full boot with the fixed kernel:** wifi joined
(`192.168.1.159`), OpenRC reached `done`, and Gentoo's sshd accepts a TCP
connection on **4444** — then closes it before sending a banner
(`kex_exchange_identification: Connection closed by remote host`). Not yet
diagnosed; the init script now also runs a debug sshd (`-ddd -e`, port 4445,
log `/var/log/sshd-debug.log`) and, while `/autoreboot` exists, reboots itself
to Pop after 6 minutes so the logs can be read without a power button.

## Update 2026-10-15 (night): why OpenSSH on 4444 closed the connection — four layers deep

With a second way in (Akuma's own `sshd`, copied into the Gentoo root as
`/usr/local/bin/akuma-sshd` on **2222**, same host key and `authorized_keys` as
p3) the failure could be debugged from the inside: `sshd -ddd -e -D -p 4447`
by hand, one connection from the Mac, read `/tmp/sd.log`. Each fix exposed the
next, and none was visible from outside — the client only ever said
`Connection closed by remote host`.

| # | log line | cause | fix |
|---|---|---|---|
| 1 | `Connection from … with IP opts: 00 00 00 00`, exit 255 | `getsockopt(IPPROTO_IP, IP_OPTIONS)` fell into the common path and answered a 4-byte zero; OpenSSH's `check_ip_options` reads any non-zero length as source routing and drops the connection | `sys_getsockopt` reports length 0 for `IP_OPTIONS` (`akuma-syscalls-glue/src/net.rs`). `python3 -c 'socket.socket().getsockopt(0,4,64)'` is now `b''` |
| 2 | `chroot("/var/empty"): Function not implemented` | **no `chroot(2)`** in either kernel | real `chroot`: `Process::root`, inherited at fork, applied once in `resolve_mount` after the path is canonicalised against the chroot's own `/` (`akuma_vfs::under_root`, host-tested incl. `..` and nested chroots). Table rows x86_64 161 / asm-generic 51. Verified live: inside the jail `/..` stays inside and `/etc/passwd` is gone |
| 3 | `prctl(PR_SET_SECCOMP): Invalid argument` | Gentoo's OpenSSH is built with the seccomp-filter sandbox; Akuma has no seccomp | rebuilt on Pop (`--with-sandbox=no`; the `rlimit` sandbox's configure test fails on musl) via `/etc/portage/env/openssh-rlimit.conf` + `package.env`. A no-op `PR_SET_SECCOMP` was rejected: it would be a sandbox that sandboxes nothing |
| 4 | `wc` / Python segfault in the shell once logged in | the 512 KiB user stack (below) | see the next section |

**What `chroot` is and is not** (also in the doc comment on `sys_chroot`): a real
per-process root for path resolution; not a security boundary. Akuma-native
`spawn` children start at `/` again, `/dev` and `/proc` are virtual and resolved
ahead of the mount table, `cwd` is not re-based (callers `chdir("/")`, as
OpenSSH does), and there are no capabilities — only uid 0.

**Boot reliability:** on some boots the radio scans and never sees the network
(`wifi: no known network in range` forever; `ifconfig` shows only `169.254…`).
Earlier boots joined within seconds. Not understood. The init script now waits
120 s for an address and, if `/autoreboot` exists, reboots to Pop so a driver
(`/tmp/cycle_gentoo.py`: arm entry 17, reboot, poll 2222/4444, re-arm on a
bounce) can retry unattended.

## Result 2026-10-15: `ssh -p 4444 root@192.168.1.159` into Gentoo works

```
$ ssh -p 4444 root@192.168.1.159 'uname -a; hostname; id; head -2 /etc/os-release'
Akuma akuma-gentoo 0.0.8 7f0707d5-release-smp-shared x86_64 AMD Ryzen 7 8845HS w/ Radeon 780M Graphics AuthenticAMD GNU/Linux
akuma-gentoo
uid=0(root) gid=0(root) groups=0(root),1(bin),2(daemon),3(sys),4(adm),6(disk),10(wheel),11(floppy),26(tape),27(video)
NAME='Gentoo'
ID='gentoo'
```

An Akuma `uname` from a Gentoo userspace, over OpenSSH, as the doc's resumption
recipe asked for. Getting there found four more kernel defects beyond the table
above, all fixed in the tree:

| defect | symptom | fix |
|---|---|---|
| **wifi scan table crowded out by hidden networks** | `wifi: no known network in range` forever on some boots (the scan table is `MAX_BSS = 24`; a dozen hidden-SSID APs and channel 1's neighbours filled it before the sweep reached channel 11, where the home AP lives) | `rtw89_sta::sweep` skips empty-SSID beacons (nothing can be joined by an empty name). The boot went from failing 4 times in a row to joining first try |
| **user stack limit 512 KiB** | `wc -l <file>` SIGSEGV at `cr2 == rsp`: coreutils keeps a 256 KiB buffer on the stack. The stack was `ELF_STACK_PAGES = 128` pages mapped at exec with no growth | the top 128 pages stay eager; below them one anonymous demand-paged region up to **8 MiB** (`ELF_STACK_MAX_PAGES`, `mm::lazy_stack_region`, registered at first exec and again after `execve`'s `install_image`), `RLIMIT_STACK` reports 8 MiB |
| **`/proc` lists every process that ever ran** | `ps`/`ls /proc` never return on a long-running box | [`AKUMA_PROC_ENUMERATION_HANG.md`](AKUMA_PROC_ENUMERATION_HANG.md) |
| (kernel) `getsockopt(IP_OPTIONS)`, `chroot`, `sethostname` | see the table above | |

**Interactive login (follow-up, same day).** `ssh -p 4444 root@…` with no command
closed the connection while `ssh … 'cmd'` worked: only the pty path failed.
OpenSSH's monitor, allocating the pty, called `chown("/dev/pts/0", 0, 5)` and
`fatal()`ed on `ENOENT`. `/dev/pts/N` exists only in the kernel's pty table,
`stat` knows about it, but `sys_fchownat` probed `exists()` on the real
filesystem first (and `fchmodat` fell through to the ext2 path). Found with the
per-pid syscall log (`/proc/<pid>/syscalls`, which records **asm-generic**
numbers — `fchownat` is 54 there, 260 is `wait4`) and a four-line Python
reproduction (`os.openpty()`, `os.chown(os.ttyname(s), 0, 5)`). Fix:
`akuma_vfs_glue::is_pty_slave`; `chown` and `chmod` on a live slave accept and
ignore, like device nodes. The login shell now runs on `/dev/pts/0`.

**`ps` ("Error, do this: mount -t proc proc /proc").** Not a missing mount.
procps-ng 4.0.6 starts with `open("/proc/self", O_PATH | O_DIRECTORY)`; `/proc/self`
was reported as a symlink (`is_symlink`) but `read_symlink("self")` had no arm,
so nothing could follow it and the open was `ENOENT`. Found with the kernel's
`strace_err` flag, which logs one `[sc!]` line per *failing* syscall with its
decoded path to `dmesg`: the boot log said `nr=2 … -> -2 "/proc/self"`. That
flag was only parsed on the PVH boot path; the GRUB (multiboot2) path now reads
it too, and menu **entry 18** is entry 17 plus `strace_err`. Fix: `readlink
/proc/self` answers with the caller's tgid (`akuma-vfs-glue/src/proc.rs`). `ps
ax` now lists the box. (Other `/proc` files that standard tools probe and that
do not exist yet: `version`, `vmstat`, `diskstats`, `partitions`, `cmdline`,
`swaps`, `sys/kernel/{osrelease,ostype,hostname,pid_max}` — none has blocked a
tool so far.)

**The local console — works (2026-10-15).** The screen showed an empty,
unblinking cursor because PID 1 is a script that sends all of its output to log
files and never ran a shell on the console the kernel hands it (stdin = the
xHCI keyboard, stdout = the framebuffer, `docs/runbooks/amd64-console-shell.md`).
`akuma-init.sh` saves the screen on fds 3/4 before redirecting 1/2 and runs
`bash -il <&5 >&3 2>&4` in a restart loop, before the network wait, so a shell
is on the TV even when the radio never joins.

The first version did not work and the kernel was fine: **a background job
(`( ... ) &`) gets `/dev/null` as its stdin when job control is off (POSIX)**,
so the `<&0` inside the loop read nothing. `/proc/self/fd/0 -> /dev/null` in a
logged test said so at once. The keyboard is now saved on **fd 5** at the top of
the script (`exec 3>&1 4>&2 5<&0`) before any background job starts. A forked
child of init reading the console channel needs nothing else — the channel and
line discipline are shared by every console-attached process (`console.rs`).
Verified on the metal: the banner is on the TV, a physical Enter reached a
logged `read`, and `bash -il` stays up.

**Process note.** `overlays/ryzen/gentoo/akuma-init.sh` was committed **empty** in
`6b4dbe29` (the fetch from Pop returned nothing while the box was running Gentoo
and I did not check) — fixed here; `deploy.py [entry] [--init]` now installs
the kernel and, with `--init`, the script from the repo, so the repo is the
source.

**The python crash is very probably the same stack bug.** After the stack fix,
on the same Gentoo root that crashed all of 2026-10-14: `wc -l` works,
`hashlib.blake2b` returns a digest (it SIGSEGV'd), `emerge --version` runs (it
SIGILL'd), and `emerge --oneshot --getbinpkg app-text/tree` gets through
dependency resolution in ~1 s. See [`AKUMA_GENTOO_PORTAGE_CRASH.md`](AKUMA_GENTOO_PORTAGE_CRASH.md).

**Still open:** the native `emerge` then stops at "Emerging (1 of 1)": it forks
its ebuild runner (a Python child, with a `multiprocessing` resource tracker and
an asyncio thread) and nothing further happens — load 0, `build.log` empty, no
`ebuild.sh` process. Not investigated. Until it is, packages are built on Pop in
a chroot and run here (that is how this OpenSSH got here).

How to run it: `overlays/ryzen/gentoo/` (`akuma-init.sh`, `cycle_gentoo.py`),
menu entry 17. Akuma's own sshd is also in the box on 2222. The box no longer
reboots itself (`/autoreboot` is removed); touch it again to get the
no-network-after-120 s bounce back to Pop.

## Background

- [`AKUMA_AMD64_LAPIC_ONESHOT_STALE.md`](AKUMA_AMD64_LAPIC_ONESHOT_STALE.md) —
  where this stage file came from, and the box fixes this test inherits.
- [`AKUMA_AMD64_BOXES_AUDIT.md`](AKUMA_AMD64_BOXES_AUDIT.md) — boxes on amd64.
- [`../reference/subsystems/containers.md`](../reference/subsystems/containers.md)
  — the box model, herd service schema, mount policy.
- [`DEVFS_MISSING.md`](DEVFS_MISSING.md) — why `/dev` is virtual and host-only.
- [`BOX_PTY_INTERACTIVE_SHELL.md`](BOX_PTY_INTERACTIVE_SHELL.md) — the channel
  terminal, and why openssh's openpty cannot be reached from a box.
