# Unix permissions: credentials, ownership, DAC

**Date:** 2026-10-14
**Status:** Implemented 2026-10-14 — steps 1–7 of §9, enforcement default **off**. See §11 for what landed, where it departs from this sketch, and what is left. Current-state reference: [`../reference/subsystems/permissions.md`](../reference/subsystems/permissions.md).
**Parent:** [`USER_MANAGEMENT_AND_BOXES.md`](../../proposals/USER_MANAGEMENT_AND_BOXES.md) (2026-08-20).
**Track:** this → [`HARD_LINKS.md`](HARD_LINKS.md) → [`BOX_BUILD.md`](BOX_BUILD.md).
That doc settled the *identity* model (box = boundary, uid = label, one-way
`setuid`, no setuid-on-exec). This one is the next layer down: make the label
real, put ownership on inodes, and enforce owner/group/other mode bits — enough
that OCI images which drop privilege actually run unprivileged.

---

## 1. Why now: OCI images check, not just call

Today every process is uid 0, `setuid` is an accepted no-op, `chown` is an
accepted no-op, and `st_uid` is always 0. That is enough for images that run as
root. It is not enough for images that *verify* they dropped:

| Image | What it does | What breaks today |
|---|---|---|
| `postgres:alpine` | entrypoint `chown`s `$PGDATA`, then `su-exec postgres` | `postgres` refuses to start as root; then checks `$PGDATA` is owned by `geteuid()` and is `0700`/`0750` — needs real `st_uid` **and** a real `chown` |
| `redis:alpine` | `setpriv --reuid=redis` then re-execs if `id -u` is still 0 | Issue 15 loop: `setpriv` "succeeds", uid stays 0 |
| any image with `"User": "1000"` in its config | runtime starts the entrypoint as 1000 | `box` ignores `User` entirely; process runs as root |
| `nginx`, `bitnami/*` | `chown`s cache dirs, workers drop to `nginx`/1001 | ownership not recorded; tar layers lose their owners at extraction |

The recurring shape: **a reported identity and an enforced one must agree**, or
the image's own sanity checks fire. So "report only" (the parent doc's §11.6
first step) gets postgres past `geteuid() != 0` and then straight into "data
directory has wrong ownership". Ownership has to be real at the same time.

Enforcement (`EACCES`) is a separate, later step — and, importantly, a **no-op
for every existing workload** (§6.1).

---

## 2. Current state (verified 2026-10-14)

Syscalls now live in `crates/akuma-syscalls-glue`; amd64 folds into the same
handlers (`amd64/src/usermode.rs:2305`), so **one implementation serves both
kernels**.

| Fact | Location |
|---|---|
| `getuid`/`getgid`/`getegid` literal `0`; `geteuid` a fn returning `0` | `akuma-syscalls-glue/src/lib.rs:875-878`, `proc.rs:1414` |
| `getresuid`/`getresgid` write 0,0,0; `getgroups` reports 0 groups | `proc.rs:1506`, `proc.rs:1525` |
| `setuid`/`setgid`/`setresuid`/`setresgid`/`setgroups`/`capset` → `0` | `lib.rs:1044-1049` |
| `fchown`/`fchownat` → `0`, store nothing | `lib.rs:897-898` |
| `umask(2)` returns the const `0o022` and **cannot be set** | `lib.rs:889`, `fs.rs:3163` |
| `faccessat`/`faccessat2` = existence test, `mode` ignored | `fs.rs:3029` |
| `Metadata` has `mode`, no `uid`/`gid` | `akuma-vfs/src/types.rs:68` |
| `Filesystem::chmod` exists (default `NotSupported`); no `chown` | `types.rs:238` |
| ext2 inode has 16-bit `uid`/`gid`, read and written; every create writes `0`; `uid_high`/`gid_high` (osd2) not decoded | `akuma-ext2/src/ext2.rs:857,863,3050,3100…` |
| Overlay copy-up preserves `mode`, not owner | `akuma-isolation/src/overlay_fs.rs:332` |
| Tar extraction applies `mode & 0o7777`, discards uid/gid | `userspace/tar/src/format.rs:70`, `extract.rs:363` |
| `box` does not parse the image config's `User` | `userspace/box/src/spec.rs` |
| `Process` has `box_id`, `fds: Arc<SharedFdTable>`, `signal_actions: Arc<…>`; no creds | `akuma-exec/src/process/mod.rs:798-831` |
| `sys_kill` checks neither box nor uid (parent doc §7, **still open**) | `proc.rs:1855` |
| Create paths already do create-then-`chmod(mode & !UMASK)` | `fs.rs:3189-3198` (mkdirat) |

Mode bits are already honest end to end (tar → ext2 → `stat` → `chmod`). Owners
are the missing column.

---

## 3. Principles carried over, and one restated

From the parent doc, unchanged:

- **`box_id` is the boundary; uid is inside it.** DAC never replaces a box check;
  every rule here is `box check && dac check`.
- **One operation lowers a uid (`setuid` family), only from 0, never back.**
  r/e/s move together; no independently settable saved uid; idempotent re-set
  succeeds (musl's `__synccall` broadcast).
- **No setuid/setgid-on-exec, ever.** `S_ISUID` is stored and reported, never
  honoured. Images ship setuid `ping`/`su`/`passwd`; as non-root they fail
  gracefully, which is what Docker with `no-new-privileges` does too.
- **Box creation/entry is credential-neutral.**

Restated, because DAC looks like it contradicts "uid never grants":
**uid 0 is not a grant, it is the starting point.** Root of a box bypasses DAC
*within that box's jail*; every other uid gets a subset of what that root has.
Owner-match lets uid 1000 into its own files, which its box's root could already
enter. Nothing a non-zero uid can do is beyond its box's 0, and nothing returns a
lineage to 0 — so the lattice is still monotone and the ladder is still unbuilt.

**Uid numerals are box-scoped, with identity mapping.** There is no user
namespace and no uid shifting: uid 999 in the postgres box and uid 999 in another
box are different principals that never meet, because their jails are disjoint.
The two places numerals from different boxes touch are both benign:
read-only shared OCI lower layers (both boxes see the image's own owners) and
box 0 (root, bypasses). Recorded as a known limit in §8.

---

## 4. Credentials

### 4.1 Shape

```rust
// akuma-exec/src/process/creds.rs
pub const NGROUPS: usize = 32;          // setgroups(n > 32) -> EINVAL

#[derive(Clone, Copy)]
pub struct Creds {
    pub uid: u32,                        // r = e = s, always
    pub gid: u32,                        // r = e = s, always
    groups: [u32; NGROUPS],
    ngroups: u8,
    pub umask: u16,
}

pub struct SharedCreds(Spinlock<Creds>);   // Process.creds: Arc<SharedCreds>
```

- **Fixed-size, `Copy`, no heap.** A permission check snapshots `Creds` once per
  syscall (part of `Subject::current()`, parent doc §14) and never allocates.
  Linux's `NGROUPS_MAX` is 65536; 32 covers every image's `/etc/group`
  membership by a wide margin, and `EINVAL` past it is honest.
- **Thread-group-shared `Arc`**, exactly as parent §11.2: fork = private copy,
  `clone(CLONE_THREAD)` = shared, `execve` = preserved.
- **`umask` lives here** rather than a separate `fs_struct`: Linux shares it per
  `CLONE_FS`, which every thread-creating path sets, so thread-group scope is the
  same answer. `umask(2)` becomes real; `fs::UMASK` the const is deleted and the
  create paths read the caller's.

### 4.2 The transition rule

```
setgroups(list)      : uid == 0 ? set : (list == current ? Ok : EPERM)
setgid / setresgid   : uid == 0 ? gid = n : (n == gid ? Ok : EPERM)
setuid / setresuid   : uid == 0 ? uid = n : (n == uid ? Ok : EPERM)
                       setresuid(r,e,s) from 0 requires r==e==s (-1 = "same")
                       else EINVAL — the only way to express a split is refused
setfsuid / setfsgid  : return current, change nothing (fs ids == e ids here)
```

`gosu`/`su-exec`/`setpriv`/runc all call groups → gid → uid in that order while
still root, so they work unchanged. `su`/`sudo` cannot work and are not meant to
(parent §11.6).

### 4.3 The consistency set — one commit

Every reader switches to `Creds` together, or the "two sources of truth" trap the
`capget` comment documents reopens:

- `get{,e}{u,g}id`, `getres{u,g}id`, `getgroups`
- `/proc/<pid>/status` `Uid:`/`Gid:`/`Groups:` (four columns, all equal)
- **`capget` and `/proc/<pid>/status` `Cap*`**: full set at uid 0, **empty** at
  any other uid. Linux clears the effective/permitted sets on a 0 → non-zero
  `setuid`; libcap-ng and `setpriv` read this to decide whether the drop
  happened.
- `stat`'s `st_uid`/`st_gid` (§5)
- `getpwuid`-facing synthetic `/etc/passwd`, if/when parent §10 lands

---

## 5. Ownership in the VFS

### 5.1 Trait

```rust
pub struct Metadata { /* … */ pub uid: u32, pub gid: u32 }

trait Filesystem {
    /// `None` leaves that id unchanged (chown's -1). Defaults to NotSupported,
    /// for the same reason `set_times` does: a silent Ok suppresses fallbacks.
    fn chown(&self, path: &str, uid: Option<u32>, gid: Option<u32>)
        -> Result<(), FsError> { Err(FsError::NotSupported) }
}
```

Per filesystem:

| fs | Work |
|---|---|
| ext2 | Decode/encode `l_i_uid_high`/`l_i_gid_high` (osd2 bytes 4..8, inode offsets 120/122) so ids ≥ 65536 round-trip; set them at create; implement `chown` like `chmod` |
| memfs / tmpfs | two `u32`s per node |
| overlay | copy-up carries owner as well as mode (`overlay_fs.rs:332`); `chown` = copy-up + upper |
| `SubdirFs` | passthrough |
| procfs | report the target process's uid/gid on `/proc/<pid>/*` |
| devfs | `0:0` (tty slaves: owner of the session, later) |

### 5.2 New files get the creator's ids

Today's create paths are "create, then `chmod(mode & !UMASK)`". Extend the same
helper to "create, then `chown(creds.uid, creds.gid)` and `chmod(mode &
!creds.umask)`" — **one** function, `apply_create_attrs`, called from
`openat(O_CREAT)`, `mkdirat`, `mknodat`, `symlinkat`, AF_UNIX `bind`.

The window between create and chown shows a root-owned file for a few hundred
ns. That leaks nothing (it is *less* accessible to the creator, not more) and
matches the existing chmod window. Threading a `CreateAttrs` through every
`Filesystem::create_*` is the clean fix and can follow; it is trait churn across
six filesystems, not a correctness requirement.

`S_ISGID` on a parent directory → inherit the directory's gid (BSD semantics);
cheap and some images rely on it for shared dirs. Otherwise not needed.

### 5.3 chown/chmod rules

- `chown` uid change: root only. gid change: root, or owner changing to a group
  it is in.
- `chmod`: root or owner. Non-root `chmod` clears `S_ISGID` if the caller is not
  in the file's group (Linux does; matters little since we never honour it).
- Any non-root write to a file clears `S_ISUID`/`S_ISGID`? **Skip** — exists to
  defend setuid-on-exec, which we do not have.

---

## 6. Enforcement (DAC)

### 6.1 The check is one pure function

```rust
// akuma-isolation/src/dac.rs   (crate is already #![forbid(unsafe_code)])
pub enum Want { R = 4, W = 2, X = 1 }   // bitmask

pub fn may(c: &Creds, mode: u32, uid: u32, gid: u32, want: u8) -> bool {
    if c.uid == 0 {
        // root bypasses everything except "exec a file with no x bit at all"
        return want & X == 0 || is_dir(mode) || mode & 0o111 != 0;
    }
    let bits = if c.uid == uid { mode >> 6 }
               else if c.in_group(gid) { mode >> 3 }
               else { mode };
    (bits & 7) & want == want             // owner class is NOT unioned with other
}

pub fn may_delete(c: &Creds, dir: &Attr, victim: &Attr) -> bool;  // + sticky
```

Host-tested as a truth table — including the two Linux surprises worth pinning:
an owner with `0o077` is **denied** even though "other" has rwx (classes are
exclusive), and root cannot exec `0o644`.

**Why it is a no-op today:** every process is uid 0, and the root arm returns
true for everything except exec-without-x. So turning enforcement on changes
behaviour for exactly (a) processes that dropped, which do not exist until §4
ships, and (b) root trying to exec a non-executable file — which already fails in
the ELF loader or should. That makes the risky step cheap to land.

### 6.2 Where it is called

Checks happen at **name resolution and open**, never on `read`/`write` of an fd
— Unix semantics, and the reason this costs almost nothing on hot paths.

| Operation | Check |
|---|---|
| `openat` existing | R / W per access mode; `O_TRUNC` ⇒ W |
| `openat(O_CREAT)` new | W+X on parent |
| `execve` | X on file (and R on a `#!` script, since the interpreter opens it) |
| `mkdirat`, `mknodat`, `symlinkat`, `linkat`, AF_UNIX `bind` | W+X on parent |
| `unlinkat`, `rmdir`, `renameat` | W+X on parent(s) + **sticky** (`/tmp` is 1777) |
| `truncate` | W |
| `utimensat` | owner, or W for `UTIME_NOW`/NULL |
| `chmod`/`chown` | §5.3 |
| `faccessat{,2}` | `may()` with the real ids (== effective here) — finally answers `mode` |
| `getdents` | R on the directory (at open) |
| AF_UNIX `connect` | W on the socket node |
| `mmap(MAP_SHARED, PROT_WRITE)` | fd opened for write (verify this is already enforced) |

### 6.3 The expensive one: search permission

POSIX wants X on **every directory component** of a path. Akuma's resolution is
string-based (`resolve_path_at` → `resolve_symlinks` → `MountSet::resolve` →
ext2's internal walk), so there is no per-component hook today, and bolting on a
`metadata()` per component would multiply `openat` cost by path depth.

Recommendation: **phase it.**

1. Enforce on the final object and its parent only (the table above). This is
   what every OCI-relevant check needs: postgres's `0700` `$PGDATA` is the
   *final* directory for its own files and a *parent* for other users' creates.
2. Later, push a `&Creds` into ext2's directory walk, which already reads every
   component inode, so the check is a mode compare on data it has in hand.
   Measure with an `openat` A/B before and after
   (`scripts/benchmarks/`, A/B/A as usual).

Until step 2, the divergence is pinned and stated: *a non-root process can open
a file it has permission on inside a directory it lacks X on.* Linux denies it.
That under-denies, never over-denies, so nothing that works on Linux breaks.

### 6.4 Signals

`kill`/`tkill`/`tgkill`/`pidfd_send_signal`: `can_reach(box) && (sender.uid == 0
|| sender.uid == target.uid)`. Lands together with the parent doc's §7 box
check, which is still open (`proc.rs:1855`) — one predicate, `can_signal`, not
two layers bolted on separately.

### 6.5 Kill switch

`akuma_config::DAC_ENFORCE` as a relaxed atomic with a setter, same pattern as
the BKL toggles, read once per syscall into the `Subject`. Off = §4/§5 report
and record everything, `may()` is never consulted. That is both the A/B lever and
the escape hatch if an image trips an unforeseen check.

---

## 7. OCI integration (`box`)

1. **Tar keeps owners.** `akuma-tar` parses uid/gid (header bytes 108..116,
   116..124; prefer `uname`/`gname`? **No** — runc uses numeric ids, names would
   resolve against the *host* table) and `lchown`s after create. Already-pulled
   images in the layer store are root-owned and must be re-pulled; say so in
   `box` when it detects an old store (a version stamp in the store dir).
2. **Parse `config.User`**: `uid`, `uid:gid`, `name`, `name:group`, `uid:group`.
   Names resolve against the **image's** `/etc/passwd` and `/etc/group` inside
   the assembled rootfs (runc's rule), plus supplementary gids from
   `/etc/group` membership. Pure logic → `boxlib::spec`, host-tested.
3. **Spawn with credentials.** `SpawnOptions` (`akuma-syscalls-glue/src/proc.rs:1573`)
   gains `uid`/`gid`/`groups`. Kernel rule: honoured only if the *caller* is uid
   0 — it is a drop performed at spawn, the same one-way move as `setuid`. ABI
   change, so kernel/herd/`box` move in lockstep (`userspace/herd/README.md`).
4. **`box run --user`** overrides, like `docker run -u`.
5. **Privileged ports: do not enforce.** Docker has set
   `net.ipv4.ip_unprivileged_port_start=0` inside containers since 20.10, so
   images that run as non-root and bind `:80` exist and work there.
6. `herd` services get the same `user =` key for free from (3).

---

## 8. Known limits, stated up front

- **Box-root bypasses DAC on anything shared into the box** (bind-style volumes,
  if added). Same as Docker without userns. A user namespace / uid shift is the
  fix and is out of scope.
- **No ACLs, no xattrs, no capabilities subdivision.** `CAP_*` stay
  all-or-nothing by uid 0 until parent §9.2's box capability set.
- **No setuid-on-exec** (by design, §3).
- **Search permission under-enforced** until §6.3 step 2.
- **No `fsuid`, no `SECBIT_KEEP_CAPS`, no `PR_SET_KEEPCAPS`.** Each is a way to
  hold privilege across a drop; none exists.
- `/etc/passwd` content is not touched by this sketch; OCI images bring their own
  and that is what `config.User` resolves against.

---

## 9. Order

Each row is its own commit; rows marked ⚠ change what the system permits.

1. ⚠ `can_signal`: box + uid check on all four signal paths (parent §7 + §6.4).
2. `akuma-isolation::dac` — pure `may`/`may_delete`/`in_group`, truth-table host tests.
3. `Creds` on `Process` + all of §4.3 in **one** commit. `setuid` family becomes
   the §4.2 rule; `umask` real. Unblocks Issue 15 (redis).
4. `Metadata.{uid,gid}` + `Filesystem::chown` + ext2 `uid_high` + memfs + overlay
   copy-up + `st_uid`/`st_gid` + real `fchown{,at}` + `apply_create_attrs`.
   Host tests: ext2 round-trip of uid 70000. amd64: add the missing x86_64
   `chown` (92) and `lchown` (94) shims, and fix `fchown` (93), which today
   passes the fd as a dirfd with a NULL path — harmless only while `fchownat`
   returns 0.
5. `akuma-tar` preserves owners; layer-store version stamp.
6. `box`: `config.User` resolution + `SpawnOptions` creds (ABI, lockstep).
7. ⚠ DAC enforcement behind `DAC_ENFORCE`, default **off**; flip on after (8).
8. Gates (below) green with it on; then default on.
9. Later: search permission in ext2's walk (§6.3), `CreateAttrs` through the
   trait (§5.2), procfs `hidepid`-style restriction on `/proc/<pid>/{fd,environ}`.

Everything from step 3 lands in `akuma-syscalls-glue`, so amd64 gets it for free;
check `amd64/src/usermode.rs` routes `setgroups`/`setresgid`/`setfsuid` (x86_64
numbers differ) before calling it done.

---

## 10. Verification sketch

- **Host:** `dac` truth table (owner/group/other exclusivity, root exec, sticky
  delete); `Creds` transition table incl. idempotent re-set and `setresuid`
  split refusal; ext2 uid_high round-trip; `boxlib` `User` parsing against
  fixture `/etc/passwd`/`/etc/group`.
- **Linux A/B probe:** `userspace/permprobe/`, a static musl binary that, as
  root, creates a fixture tree, drops to 1000:1000 with groups `{1000, 50}`, and
  prints one errno per operation in §6.2 (incl. sticky `/tmp`, `0o077` owner
  denial, `faccessat` with each mode). Same binary on Akuma and Docker Linux;
  diff the outputs. Under-denials from §6.3 are expected and listed in the probe.
- **Guest gates:** `postgres:alpine` starts, accepts a connection and survives a
  restart (exercises drop, `chown`, ownership + mode check); `redis:alpine`
  starts without re-exec looping; an image with `"User": "1000"` reports
  `id` = 1000 and cannot write `/etc`.
- **No regression:** acceptance 05/10/11/13 and the self-host build with
  `DAC_ENFORCE` on — expected identical, since everything there is uid 0 (§6.1).
  A difference means the root arm is wrong.

---

## Open questions

1. **`NGROUPS = 32`** — fine for OCI; any herd/host use expecting more?
2. **Re-pull vs. migrate** old layer stores — a re-pull is simplest; a one-shot
   "chown from the cached tarballs" is possible if pulls are expensive on metal.
3. **Enforce in box 0 at all?** Box 0 is all-root today, so it is moot until a
   service there drops via `herd user =`. Recommendation: yes, same rule
   everywhere — a box-0 special case is a second policy to audit.

## 11. Implementation record (2026-10-14)

### What landed

| §9 step | Landed as |
|---|---|
| 1. `can_signal` | `akuma_exec::process::check_signal` — box reach (always on, `ESRCH`, no allocation: `box_registry::box_reaches` walks the parent chain under the lock with a 64-deep cap and **without** the creator-pid fallback), then uid (`EPERM`, only while enforcing). Wired into AArch64 `kill`/`tkill`/`tgkill`/`pidfd_send_signal` and amd64 `kill`/`tkill`, including amd64's `kill(-1)`/group form, which signalled every process on the machine from inside a box |
| 2. pure checks | `akuma_isolation::dac` + `akuma_isolation::creds`, 30 host tests |
| 3. creds + consistency set | `Process::creds: Arc<SharedCreds>`; glue `cred.rs`; `/proc/<pid>/status` `Uid/Gid/Groups/Cap*`; `capget` empty for non-root; `umask(2)` real. x86_64 gained rows for `setreuid`/`setregid`/`setgroups`/`set/getres[ug]id`/`setfs[ug]id` — they were `ENOSYS` on amd64 |
| 4. ownership | `Metadata.{uid,gid}`, `Filesystem::chown`; ext2 incl. `l_i_uid_high`/`l_i_gid_high`; memfs per-node mode+owner; overlay copy-up carries mode+owner; `stat`/`fstat`/`statx` report owners; real `fchown`/`fchownat` (x86_64 `fchown` became a row, `chown`/`lchown` got shims); new files owned by the creator; `/proc/<pid>/*` owned by the process |
| 5. tar owners | `akuma-tar` parses numeric uid/gid (octal and GNU base-256) and `lchown`s as root; libakuma gained `lchown` |
| 6. `config.User` | `boxlib::spec::resolve_user` (runc rules) + `box run -u`; `SpawnOptions` grew to 112 bytes (`uid`, `gid`, `groups_ptr`, `ngroups`, `cred_flags`), installed before the child is published, only ever lowering (`spawn_creds`) |
| 7. enforcement | `akuma_syscalls_glue::perm`, behind `/proc/dac` |

### Where it departs from the sketch

- **The switch is `/proc/dac`, not an `akuma_config` constant.** AArch64 has no
  kernel command line to set one from, and a runtime file is what an A/B needs.
  Host root only.
- **`getuid` & co. left `FastPath::Leaf`.** They read the caller's `Process`
  now, which the tier's criterion 2 forbids (`akuma-syscalls` § "Admission
  criteria").
- **The sticky rule is `EPERM`, the directory rule `EACCES`**, as on Linux —
  the first draft returned `EACCES` for both; the Linux reference run caught it.
  `dac::sticky_allows` is split out so callers can tell them apart.
- **memfs's root is `1777`**, as a Linux tmpfs is. It reported a fixed
  `0755 root` for every directory, which would shut every non-root writer out
  of any tmpfs once enforcing.
- **Three latent mode bugs fixed on the way**, all invisible while nothing
  read modes: overlay copy-up of a *directory* (and of a file's parents) kept
  the upper's default `0755` — a container's `/tmp` lost `1777` the moment
  anything wrote under it; `akuma-tar` never applied directory modes at all;
  `open(O_CREAT)` never applied the umask.
- **amd64 needed its own `execve` check** (it has its own exec path) and a
  `truncate` row (x86_64 76 was `ENOSYS`). Both found by `permprobe` on the
  metal, not by reading.

### Verification

| Where | Result |
|---|---|
| Host tests | all green; new: creds 8, dac 11, box reach 2, ext2 owner 4, memfs 3, overlay 2, procfs 2, abi 1, box `resolve_user` 7, tar 1 |
| Linux (Docker, arm64) | `permprobe` 50/50 — defines the expected column |
| AArch64 QEMU/HVF (`no-tests`) | `permprobe` 50/50; cross-box `kill -0`/`kill -9` → `ESRCH`, host process survives |
| AArch64 Firecracker/KVM boot suite | 299/1 — **identical to unmodified HEAD**, which fails the same `thread_slot_reclaim_on_spawn` |
| amd64 QEMU boot suite | 807/0 |
| amd64 metal (trashcan) | `permprobe` 50/50; cross-box `kill` → `ESRCH` |
| `box run` end to end | alpine layer keeps `/etc/shadow 0640 0:42`, `/tmp 1777`; `-u 1000:1000`, `-u nobody` run as those ids; unknown user refused; as `nobody` with `/proc/dac=1`, `/etc/shadow`, `/etc`, `/root` refused, `/tmp` writable |

### Left

- **Default on.** Blocked on modes of existing disks: the trashcan has 90
  `0644` files in `/bin`, `/bin/rm` among them, which root cannot exec under
  enforcement (rule 4). A one-off `chmod +x` of ELF files, then flip
  `dac::ENFORCE`'s initial value.
- Search permission on intermediate directories (§6.3 step 2).
- `CreateAttrs` through the trait instead of create-then-chown (§5.2).
- herd `user =` (the `SpawnOptions` fields are there; herd does not set them).
- Re-pull images pulled before today: their layers are root-owned.

## Background

- [`USER_MANAGEMENT_AND_BOXES.md`](../../proposals/USER_MANAGEMENT_AND_BOXES.md) — identity model this builds on
- `docs/archive/DEVBOX_ISSUES.md` Issue 15 — the `setpriv`/redis loop
- `docs/archive/UTIMENSAT_STUB_TOUCH.md` — why trait defaults are `NotSupported`, not `Ok`
- `docs/reference/subsystems/containers.md` — box mount policy
- `userspace/box/docs/BOX_RUN.md` — overlay copy-up, layer store
