# Minimal Dev Busybox Applet Set

Scope: the smallest set of busybox applets that a developer logging in to
develop **Akuma itself and Rust programs** expects to find operational, *even
if the environment is mounted read-only*. This is a gap inventory, not a fix
log — it lists what works today, what is missing, and clusters the missing
syscalls/procfs/sysfs entries by the applet group they block.

The devbox today (`overlays/devbox/run.sh`) ships a full busybox applet
symlink set + a writable ext2 root, so most of Tier 1 already works. The gaps
below bite when (a) running on a non-rump / native-smoltcp build, (b) trying
to mount a read-only root, or (c) using the diagnostic/networking applets that
read `/proc` and `/sys`.

> **Related:** [`BUSYBOX_MISSING_SYSCALLS.md`](BUSYBOX_MISSING_SYSCALLS.md)
> (`wait4` rusage, `times`), [`PROCFS.md`](PROCFS.md) (current procfs shape),
> [`UNAME.md`](UNAME.md), [`DEVBOX_ISSUES.md`](DEVBOX_ISSUES.md).

---

## Goal

A stable, reproducible in-VM environment where a developer can:

1. `ssh` in to a shell,
2. edit / inspect source (`cat`, `ls`, `grep`, `find`, `stat`),
3. run `cargo` / `rustc` / a C toolchain,
4. bring the network up and verify it (`ifconfig` at minimum),
5. do all of the above against a **read-only** rootfs (state goes to tmpfs
   or a separate writable layer).

The kernel's syscall ABI is mature for binaries (see
[`../reference/subsystems/syscalls.md`](../reference/subsystems/syscalls.md) —
Grade A for dispatch). The blockers for the *tooling environment* are
concentrated in **procfs/sysfs virtual files** and the **mount syscall**,
not in classic syscall numbers.

---

## Tier 1 — Must work: login, navigation, file ops (mostly read-only)

**Verified 2026-08-12** on a `release-smp-shared` (native/smoltcp) build via
the harness in this commit. Status reflects what was observed, not predicted.

| Applet | Status | Notes |
|---|---|---|
| `sh` (ash) | **PASS** | Login shell; multi-call dispatch via `argv[0]`. |
| `pwd`, `cd` | **PASS** | `cd` is an ash builtin; `sys_getcwd` implemented. |
| `ls` | **PASS** | `getdents64`. |
| `cat`, `echo`, `printf` | **PASS** | |
| `uname` | **PASS** | Reports `Akuma <pkg-ver> <git-profile> aarch64`. |
| `id` | **PARTIAL** | `id -u`/`id -g` work (0/0); full `id` fails "can't get groups" — **`getgroups` (nr 158) undispatched** (Verification Finding V2). Name display also needs `/etc/passwd` (Finding V3). |
| `whoami` | **FAIL** | `unknown uid 0` — **no `/etc/passwd` on the devbox overlay** (Finding V3). Kernel side is fine. |
| `mkdir`, `rmdir`, `ln` (sym + hard), `rm`, `cp`, `mv` | **PASS** | `mkdirat`/`unlinkat`/`symlinkat`/`linkat`/`renameat2`. |
| `touch` | **FAIL** | `touch <newfile>` silently does nothing — **`utimensat` (nr 88) hardcoded `=> 0`** returns success for nonexistent paths (Finding V1). `touch <existing>` works. |
| `chmod` | **PASS** | `fchmod`/`fchmodat` + `statx` round-trips the mode bits. |
| `chown` | **FAIL (by-name)** | `chown root:root f` → `unknown user root` — no `/etc/passwd` (Finding V3). `chown 0:0 f` (numeric) is the workaround. |
| `tr`, `cut`, `sort`, `uniq`, `tee`, `wc` | **PASS** | Pure userspace, no new syscalls. |
| `env`, `printenv`, `export`, `unset` | **PASS** | env via auxv + `execve`. |
| `sleep`, `kill`, `exit`, `true`, `false`, `test`, `[` | **PASS** | `clock_nanosleep` fixed (`THREAD_SLEEP_MISSING_CLOCK_NANOSLEEP.md`). `kill` verified 3/3 isolated (an earlier harness flake was background-job/SSH teardown, not the applet). |
| `time` | **PASS** | After `wait4` rusage fix (`BUSYBOX_MISSING_SYSCALLS.md`). |
| `nproc` | **PASS** | After `sched_getaffinity` byte-count fix (`runbooks/selfhost-kernel-build.md` §5.6). |

## Tier 2 — Text filters

**Verified 2026-08-12** (same build/harness). Originally deferred in this
doc; the pass below promotes it out of deferred status. `vi` is excluded
(known-good by user report) and a few pagers are absent-as-applet rather than
broken — noted inline.

| Applet | Status | Notes |
|---|---|---|
| `head`, `tail` | **PASS** | `head -2`, `tail -2` produce exact bytes. |
| `sed` | **PASS** | `sed 's/a/A/'` substitution. |
| `awk` | **PASS** | Single-pass `'{s+=$1} END{print s}'` (gawk/mawk-compatible). |
| `grep` | **PASS** | Fixed-string match. |
| `od` | **PASS** | `od -c`. |
| `hexdump` | **PASS** | `hexdump -C`; busybox applet present. |
| `dd` | **PASS** | `bs=1 count=5` byte-exact copy. |
| `fold` | **PASS** | `fold -w2`. |
| `nl` | **PASS** | Line numbering (formatting tolerance for leading ws). |
| `expand` | **PASS** | Tab→space. |
| `paste` | **PASS** | Two-file column join. |
| `more` | **PASS** | Non-interactive (`</dev/null`) read path. |
| `fmt` | **PASS** | busybox applet present; reflows. |
| `less` | **ABSENT** | Not a busybox applet (external). Harness treats absence as PASS (no regression). |
| `vi` | **EXCLUDED** | Known-good by user report; not exercised here. |

Tier 2 has **no kernel-side gaps**: every implemented applet passes.

## Tier 3 — Inspection / system state (PARTIAL — see clusters)

| Applet | Status | Blocker |
|---|---|---|
| `ps` | OK | Reads `/proc/<pid>/stat` (implemented). |
| `top` | Partial | Reads `/proc/<pid>/stat`; CPU% inaccurate (no per-proc `utime`/`stime`). |
| `free` | **Broken** | Reads `/proc/meminfo` — **file absent** (Cluster A). |
| `uptime` | **Broken** | Reads `/proc/uptime` — **absent** (Cluster A). |
| `loadavg` | **Broken** | Reads `/proc/loadavg` — **absent** (Cluster A). |
| `df` | **Broken** | Calls `statfs(path)` (nr 43) — **undispatched**, returns `ENOSYS` (Cluster G). |
| `du` | OK | Walks the tree with `statx`. |
| `stat`, `file` | OK | `statx`/`newfstatat`. |
| `mount` (listing) | OK | `sys_mount` exists; `/proc/self/mounts` absent (Cluster A) but the `mount` applet reads via `sys_mount` table. |
| `lsof` | n/a | No applet; `/proc/<pid>/fd` exists if needed manually. |

## Tier 4 — Network (the explicit "ifconfig at least")

| Applet | rump devbox | native / smoltcp / extreme-size | Blocker (native) |
|---|---|---|---|
| `ifconfig` | **OK** | **Broken** | No `/proc/net/dev`, no `SIOCGIF*` ioctls (Clusters D + F). rump answers these via the socket-layer proxy. |
| `route` | Partial | Broken | `/proc/net/route` absent (Cluster D). |
| `netstat` | Partial | Broken | `/proc/net/{dev,route,arp,tcp,udp}` mostly absent (Cluster D). |
| `ip` (iproute2, not busybox) | Partial | Broken | Reads `/sys/class/net/*` (Cluster E). |
| `ping` | Likely broken | Broken | Needs raw sockets (`SOCK_RAW`); unimplemented. |
| `wget`, `curl` (external) | OK | OK over smoltcp | |
| `nslookup`, `host` | OK | OK | DNS via `RESOLVE_HOST` syscall. |
| `hostname` | Partial | Partial | `gethostname` is userspace (via `uname`); `sethostname` (nr 165) undispatched (Cluster H). |

**The floor:** `ifconfig` (listing) on at least one build. On rump devbox
this is already met; on native/smoltcp it is blocked by Cluster D + Cluster F.

---

## Verification findings (2026-08-12)

The Tier 1 + Tier 2 verification pass surfaced three real issues not in the
cluster table below — two kernel bugs and one rootfs gap. They are small and
independent of the clusters; listed here so they aren't lost.

### V1 — `utimensat` (nr 88) hardcoded to `0`  →  `touch` can't create files

`src/syscall/mod.rs:908`:

```rust
nr::UTIMENSAT => 0,
```

`utimensat` returns success unconditionally, including for a **nonexistent**
path. busybox `touch <newfile>` first calls `utimensat` to set timestamps;
seeing "success", it believes the file exists and never creates it. Observed:

```
$ rm -f /tmp/nope; busybox touch /tmp/nope; echo rc=$?
rc=0
$ test -e /tmp/nope && echo EXISTS || echo notcreated
notcreated
```

`touch <existing-file>` works (the no-op timestamp set is harmless).
**Fix shape:** implement `sys_utimensat` — resolve the path, return `ENOENT`
if missing (and `AT_FDCWD`/`AT_SYMLINK_NOFOLLOW` flags), and write the
requested timestamps into the inode (or zero the atime/mtime if `times ==
NULL`). Minimum viable: return `ENOENT` for missing paths, which alone makes
`touch` fall through to its `open(O_CREAT)` branch and start creating files.

### V2 — `getgroups` (nr 158) undispatched  →  `id` can't list groups

Not in the dispatch `match` at all (grep for `158 =>` / `GETGROUPS` in
`src/syscall/` returns nothing). Falls through to the generic `ENOSYS` arm.
busybox `id` prints `uid=0 gid=0` then aborts with `can't get groups`;
`id -G` fails likewise. `id -u`/`id -g` work (separate `getuid`/`getgid`
syscalls).

**Fix shape:** add `nr::GETGROUPS => proc::sys_getgroups(...)` returning `0`
supplementary groups for root (write nothing, return `0` as the count).
`setgroups` (159) is the write-side companion and can stay stubbed for now
(root has no supplementary groups to set).

### V3 — `/etc/passwd` + `/etc/group` absent from the devbox overlay  →  `whoami`, `chown <name>` fail

`overlays/devbox/rootfs/etc/` ships only `apk/ herd/ meow/ resolv.conf sshd/`
— no `passwd`, `group`, or `shadow`. Any `getpwnam("root")` / `getpwuid(0)`
caller fails: `whoami` (`unknown uid 0`), `chown root:root f` (`unknown user
root`), and the name display in `id`. Verified by dropping a minimal

```
root:x:0:0:root:/root:/bin/sh
```

into `/etc/passwd` in-guest: `whoami` → `root`, `chown root:root` → rc=0,
`id` → `uid=0(root) gid=0(root)` (groups still broken until V2). This is a
rootfs/config gap, not a kernel bug.

**Fix shape:** add `etc/passwd`, `etc/group` (and a locked `etc/shadow`) to
`overlays/devbox/rootfs/`. Three lines of static content; also unblocks
`login`, `su`, and any future `herd`/`sshd` UID accounting.

### Flake note — `kill`

The harness saw `kill` return `rc=255` once; isolated re-runs (3/3) pass.
The original failure was SSH-session teardown racing the backgrounded `sleep
30 &` job, not the applet or `sys_kill`. Not a finding.

---

## V4 (2026-08-16) — `applet not found` that is **not** a missing applet: `exec_shebang` puts the symlink-resolved interpreter in `argv[0]`

**Read this before chasing any `applet not found` on the devbox.** The applet
symlink set is complete (re-verified below); this message can instead mean the
kernel started busybox under the wrong `argv[0]`, and **every `#!/bin/sh` script
on the image fails this way.**

### Symptom

```
$ printf '#!/bin/sh\necho OK\n' > /tmp/a.sh; chmod +x /tmp/a.sh; /tmp/a.sh
a.sh: applet not found            # rc=127
```

It surfaces most visibly through `apk`, which is where it was found — a package
script that never starts, misread for years as a busybox trigger problem
([`DEVBOX_ISSUES.md`](DEVBOX_ISSUES.md) Issue 8):

```
* bash-5.3.9-r1.post-upgrade: applet not found
ERROR: lib/apk/exec/bash-5.3.9-r1.post-upgrade: exited with error 127
1 error; 697.4 MiB in 58 packages
```

### Diagnosis — five invocations, measured in-guest

The last two rows are the decisive control: same binary, same script, differing
**only** in `argv[0]`.

| # | invocation | result |
|---|---|---|
| A | `#!/bin/sh` script | `rc=127` `a.sh: applet not found` |
| B | `#!/bin/busybox sh` script | `rc=0` `OK` |
| C | `#!/bin/bash` script | `rc=0` `OK` |
| D | `busybox /tmp/a.sh` — *the argv the kernel actually builds* | `rc=127` `a.sh: applet not found` |
| E | `busybox sh /tmp/a.sh` | `rc=0` `OK` |

`/bin/sh` is `lrwxrwxrwx /bin/sh -> /bin/busybox`. B works because the shebang
names the applet explicitly; C works because `/bin/bash` is a real 866 KB ELF
that ignores `argv[0]`. D reproduces the kernel's failure from userspace, which
is what makes this a proof rather than a story.

### Root cause

`exec_shebang` in `src/syscall/proc.rs` shadows the as-written interpreter with
its symlink target and then uses that for **both** jobs:

```rust
let interpreter = crate::vfs::resolve_symlinks(interpreter);  // "/bin/sh" -> "/bin/busybox"
...
new_args.push(interpreter.clone());   // argv[0] = "/bin/busybox"   <-- wrong
do_execve(interpreter, new_args, env) //   load  = "/bin/busybox"   <-- right
```

Linux passes the interpreter **exactly as written in the shebang** as `argv[0]`
and resolves the path only to load the image. Busybox is a multi-call binary
whose entire dispatch is `argv[0]`: invoked as `busybox`, it treats its first
argument as an applet name, so `argv[1]` (the script path) is looked up as an
applet, basename and all. Any interpreter reached through a symlink loses its
identity this way — busybox is simply the one that notices loudest.

### Fix

Keep the two values apart; only `argv[0]` changes:

```rust
let interp_argv0 = String::from(interpreter);                  // "/bin/sh"       -> argv[0]
let interp_path  = crate::vfs::resolve_symlinks(interpreter);  // "/bin/busybox"  -> load
```

**`spawn.rs::resolve_shebang_chain` already does exactly this** — it pushes the
unresolved interpreter into the argv prefix and resolves separately into
`elf_path`, and states the rule in its doc comment ("a shell must see the name it
was asked to run, not the symlink target"). So the two shebang implementations
disagree, and only the `execve` one is wrong; the durable fix is one shared
implementation rather than two that drift.

### Verify

```sh
printf '#!/bin/sh\necho OK\n' > /tmp/a.sh && chmod +x /tmp/a.sh && /tmp/a.sh   # expect: OK
/bin/busybox /tmp/a.sh                                                          # expect: OK
apk fix 2>&1 | tail -2                                                          # expect: no "1 error"
```

### Applet inventory re-check (2026-08-16), for contrast

All present on `devbox.img`, so a missing symlink is **not** the explanation for
V4: `wc`, `head`, `ps`, `tail`, `sort`, `uniq`, `grep`, `sed`, `awk`, `du`, `df`
— 11/11. `/bin/bash` (GNU bash 5.3.9) is installed as a real binary, which also
retires [`DEVBOX_ISSUES.md`](DEVBOX_ISSUES.md) Issue 7's premise. Still broken
independently of V4: `df` fails with `/proc/mounts: No such file or directory`
(Cluster C below), and `cat /proc/cores` returns `read error: No such file or
directory` while `cores` is listed in `/proc` (Issue 4).

---

## Clustered gap analysis

The missing surface clusters into eight groups. Order is roughly "cheapest
fix / highest value first." Findings V1/V2 above sit alongside Cluster G
(undispatched syscalls); V3 sits with the rootfs, not the kernel.

### Cluster A — procfs system-identity + summary files

None of these exist in `src/vfs/proc.rs`. All are small static / near-static
text files; each is a 20–60-line addition to `ProcFilesystem::read_file`.

| Path | Consumers | Source of truth in kernel |
|---|---|---|
| `/proc/meminfo` | `free`, `top`, glibc `sysconf(_SC_PHYS_PAGES)`, many rustc crates' build scripts | `pmm::total_count` / `free_count` (already used by `sys_sysinfo`). |
| `/proc/cpuinfo` | `lscpu`, `nproc` fallback, OpenSSL/armcaps probes, `num_cpus` crate fallback | DTB + `probed_core_count()`; MIDR_EL1 per core. |
| `/proc/uptime` | `uptime`, load-average derivation | `timer::uptime_us()`. |
| `/proc/loadavg` | `loadavg`, `top` header | Run-queue depth (scheduler); can stub `0.00 0.00 0.00 1/N`. |
| `/proc/stat` | `top`, `mpstat`-like | Per-CPU `uptime_us` deltas (already tracked for PSTATS). |
| `/proc/version` | `busybox version`-style probes, humans | `env!("CARGO_PKG_VERSION")` + git profile (same fields as `uname`, see `sys_uname`). |

### Cluster B — procfs `/proc/sys/*` (sysctl tree)

Not implemented at all. The `sys/` subtree is its own virtual directory and
needs a path-walking pseudo-fs (or a flat matcher). Highest-value leaves:

| Path | Consumers |
|---|---|
| `/proc/sys/kernel/{ostype,osrelease,version}` | `uname -a` fallback paths, libc probes |
| `/proc/sys/kernel/hostname` | `hostname`, libc `gethostname` |
| `/proc/sys/kernel/domainname` | `domainname`, NIS probes |
| `/proc/sys/net/ipv4/ip_forward` | routing daemons, container netns setup |
| `/proc/sys/net/core/{rmem_default,wmem_default}` | `busybox netstat -s`, socket tuning |
| `/proc/sys/fs/file-max` | `ulimit -n` calibration |

`sys_uname` already returns these for the uname(2) syscall; the procfs leaves
should return the same strings to keep the two paths from diverging.

### Cluster C — procfs `/proc/mounts` + `/proc/filesystems`

| Path | Consumers | Source |
|---|---|---|
| `/proc/mounts`, `/proc/self/mounts` | `mount`, `findmnt`, `df`, libc `setmntent` | `vfs::list_mounts()` already exists and powers the `mount` shell builtin. |
| `/proc/filesystems` | `mount -t auto` probing, `df` | Static list: `ext2`, `proc`, `tmpfs`, (+ `overlay` if `sc-containers`). |
| `/proc/self/mountinfo` | systemd-analogs, advanced `findmnt` | Derivable from `list_mounts`. |

### Cluster D — procfs `/proc/net/*` (interface-centric)

Implemented today: `/proc/net/{tcp,udp}` (socket lists).
**All interface-listing files are absent** — these are what `ifconfig`,
`route`, `netstat` fall back to when netlink (`AF_NETLINK`) is unavailable
(netlink is **not** implemented; everything route/interface goes through
`/proc/net` on Akuma).

| Path | Consumers | Source of truth |
|---|---|---|
| `/proc/net/dev` | `ifconfig`, `netstat -i`, `cat /proc/net/dev` | smoltcp iface stats / rump interface list. |
| `/proc/net/if_inet6` | `ifconfig` (IPv6 addrs) | Stub empty (no IPv6 yet). |
| `/proc/net/route` | `route`, `netstat -r` | smoltcp routing table / rump. |
| `/proc/net/arp` | `arp`, `busybox arp` | Neighbour cache (stub empty acceptable). |

**This is the cluster that makes `ifconfig` work on a non-rump build.** On
the rump devbox these are answered implicitly by the rump socket proxy at
the syscall layer; on native/smoltcp the kernel must synthesise them itself.

### Cluster E — sysfs is entirely absent

There is no `sysfs`. `src/vfs/` contains only `ext2.rs`, `proc.rs`, and the
shared `mod.rs`; `sys_mount`'s fstype allow-list is `{"proc", "tmpfs"}`
(`src/syscall/container.rs:118`) so `mount -t sysfs none /sys` returns
`ENODEV`. The init rootfs does not pre-create `/sys`.

Highest-impact leaves when added (any of these makes a modern tool happier):

| Path | Consumers |
|---|---|
| `/sys/class/net/` | `ip link`, `ifconfig` (modern), udev analogs |
| `/sys/devices/system/cpu/cpu*/topology` | `lscpu`, `num_cpus` |
| `/sys/block/<dev>/size` | `lsblk`, `fdisk -l` |
| `/sys/kernel/notes` | kmod probes (not relevant for Akuma) |

A minimal `/sys` can be a second pseudo-fs alongside procfs, or flat files
under the existing tmpfs mount. **Recommendation:** do not implement a full
sysfs; implement only `/sys/class/net/` if modern `iproute2` is ever needed,
otherwise rely on Cluster D's `/proc/net/dev`.

### Cluster F — `mount` syscall drops flags (blocks read-only root)

`sys_mount` (`src/syscall/container.rs:101`) signature takes `flags` but the
parameter is named `_flags` and never read. Concretely:

- **`MS_RDONLY` (0x1) is silently ignored** — every mount is created
  read-write capable. There is no way to ask for a read-only root, which is
  the headline feature of this effort.
- `MS_NOSUID`, `MS_NODEV`, `MS_NOEXEC`, `MS_RELATIME`, `MS_STRICTATIME` —
  all dropped.
- `source` (`_source_ptr`) and `data` (`_data_ptr`) ignored — no bind mounts,
  no mount options parsed.
- `fstype` allow-list is `{proc, tmpfs}` — `sysfs`, `ext2`, `bind` rejected
  with `ENODEV` (`MOUNT_IN_NS` handles `overlay` separately for boxes).
- `umount2` always returns `EPERM` (`container.rs:147`) — even host box 0
  cannot unmount through the syscall; only the kernel boot path tears mounts
  down.

**For "read-only mount, ifconfig at least":** the minimum cut is to honour
`MS_RDONLY` in the `MountTable`/`Filesystem` layer (refuse `O_WRITE` opens
on files under a read-only mount) and plumb it through `statfs`'s `f_flags`
(`ST_RDONLY` = bit 0).

### Cluster G — old/un-dispatched syscall numbers

These fall through the big `match` in `syscall/mod.rs:1076` to `ENOSYS` +
the `[ENOSYS] nr=NNN` log line.

| nr | Name | Who calls it | Fix shape |
|---|---|---|---|
| 43 | `statfs` | busybox `df` (via musl `statfs(path)`), anything checking FS capacity by path | Thin wrapper: resolve path → `sys_fstatfs(dirfd_of_path)`. `fstatfs` is already implemented (`fs.rs:1158`). |
| 158 | `getgroups` | busybox `id` / `id -G` | **Verification Finding V2.** Return `0` supplementary groups (count 0, write nothing). Companion `setgroups` (159) can stay stubbed. |
| —  | `fstatfs` `f_flags` field | `df`, `findmnt -O` | Currently hardcoded `0`; should carry `ST_RDONLY` once Cluster F lands. |
| `SYS_stat`/`SYS_lstat`/`SYS_readlink`/`SYS_getdents` (old) | — | Not present on aarch64 asm-generic table; musl already routes these to the `*at`/`64` variants. **Not a gap.** | — |

Also in this neighbourhood but a different shape (returns `0` instead of
`ENOSYS`, so it does *not* trip the generic arm):

| nr | Name | Who calls it | Fix shape |
|---|---|---|---|
| 88 | `utimensat` | busybox `touch <newfile>` | **Verification Finding V1.** Hardcoded `=> 0` at `syscall/mod.rs:908` lies about success on nonexistent paths. Return `ENOENT` for missing paths (minimum viable); full impl writes atime/mtime into the inode. |

### Cluster H — network ioctls (`SIOCGIF*`)

`sys_ioctl` (`src/syscall/term.rs:6`) handles only terminal (`TC*`/`TIO*`),
`FIONBIO`/`FIONREAD`/`FIOCLEX`, OSS audio, and (under `rump`) `TUNSETIFF`.
Everything else returns `ENOTTY` (-25) — *including the whole `0x89xx`
`SIOC*` family* that `ifconfig`/`route`/`netstat` use on a socket fd:

| ioctl | Code | Purpose |
|---|---|---|
| `SIOCGIFCONF` | 0x8912 | List all interfaces (the classic ifconfig probe). |
| `SIOCGIFADDR` | 0x8915 | Interface IPv4 address. |
| `SIOCGIFNETMASK` | 0x891b | Netmask. |
| `SIOCGIFHWADDR` | 0x8927 | Hardware (MAC) address. |
| `SIOCGIFMTU` | 0x8921 | MTU. |
| `SIOCGIFFLAGS` | 0x8913 | Up/Broadcast/... flags. |
| `SIOCGIFINDEX` | 0x8933 | Numeric index. |

On the **rump devbox** these never reach the kernel's `ENOTTY` path: the
rump proxy intercepts socket-family syscalls and the rump stack answers
them. On **native/smoltcp** they must be answered in `sys_ioctl` by reading
the smoltcp interface state. (Equivalent to Cluster D's `/proc/net/dev` —
pick one surface and let the other redirect.)

---

## Recommended landing order

1. **Verification fixes V1–V3** (Tier 1 completion) — `utimensat` ENOENT,
   `getgroups` dispatch, and `/etc/passwd`+`/etc/group` in the devbox overlay.
   Three tiny, independent patches that take Tier 1 from "mostly works" to
   "green". Do these first.
2. **Cluster A** (procfs summary files) — biggest perceived-environment win
   per line of code; touches only `src/vfs/proc.rs`.
3. **Cluster F** (`MS_RDONLY` + `f_flags`) — unblocks the read-only rootfs
   goal directly.
4. **Cluster D** (`/proc/net/dev` at least) — makes `ifconfig` work on
   native/smoltcp builds; aligns native with what rump already provides.
5. **Cluster G** (`statfs` nr 43) — one-line wrapper, fixes `df`.
6. **Cluster B/C** (sysctl leaves, `/proc/mounts`) — convenience +
   consistency with `uname`/`mount` syscall paths.
7. **Cluster H** — only if a tool refuses to read `/proc/net/dev` and
   insists on the ioctl surface.
8. **Cluster E** (sysfs) — defer indefinitely; add a single leaf only if a
   concrete tool demands it.

Each cluster is independently shippable; none require the others first.

---

## Verify

When a cluster lands, exercise it from an SSH session on **both** a rump
devbox and a native/smoltcp (or `extreme-size`) build:

```sh
# Cluster A
free; uptime; cat /proc/cpuinfo | head; cat /proc/version

# Cluster C
mount; cat /proc/mounts; cat /proc/filesystems

# Cluster D / F (after also wiring MS_RDONLY)
ifconfig; cat /proc/net/dev
mount -o ro -t tmpfs none /mnt/ro   # then try to touch /mnt/ro/x → EROFS

# Cluster G
df -h /
```

Cross-check that the syscall-debug log stays clean of `[ENOSYS] nr=43`,
`[ENOSYS] nr=NNN (mount)`, etc.:

```sh
dmesg | grep ENOSYS
```

Per the CLAUDE.md "Verify" convention for runbook-style docs: this is an
archive doc, not a runbook, so the above is the smoke check, not a gate.

---

## Background

- Current procfs shape: [`PROCFS.md`](PROCFS.md) and `src/vfs/proc.rs`.
- Syscall dispatch table: [`../reference/subsystems/syscalls.md`](../reference/subsystems/syscalls.md)
  + per-family docs under `../reference/subsystems/syscalls/`.
- Existing busybox porting notes (syscall-level, not applet-level):
  [`BUSYBOX_MISSING_SYSCALLS.md`](BUSYBOX_MISSING_SYSCALLS.md).
- Devbox image contents (full busybox applet symlink set):
  [`../runbooks/build-devbox.md`](../runbooks/build-devbox.md).
- Why `ifconfig` already works on rump (proxy interception, not kernel
  support): [`../reference/subsystems/rump-stack.md`](../reference/subsystems/rump-stack.md).
- The shell-applet audit this doc explicitly defers (head/tail/sed/...):
  tracked as a separate doc, `MINIMAL_DEV_BUSYBOX_SHELL_APPLETS.md` (to be
  written).
