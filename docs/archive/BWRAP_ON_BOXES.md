# Running real bubblewrap on Akuma: namespaces as boxes

**Status: working, verified on the amd64 kernel under KVM (2026-11-18).** Real Alpine `bubblewrap` 0.12 runs, and so does
codex 0.169.0's own runtime sandbox command (`bwrap ... --argv0 codex-linux-sandbox ... --apply-seccomp-then-exec`):
read-only root, no path to the old root, network refused. The rule that comes out of it is in
`docs/reference/subsystems/containers.md` § Private mount namespaces; this file keeps the reasoning and the dead ends.
Regression suite: `scripts/bwrap_suite.py` (12 cases). What else a sandbox's uid 0 can still reach:
`docs/archive/BOX0_ONLY_AUDIT.md`. Feature flag: **`sc-private-ns`** (in `linux-extra-syscalls`).

## Why

codex sandboxes every command with `bwrap` (`docs/runbooks/debug-codex-on-akuma.md` turned the sandbox off because it
could not run). The way to learn what bubblewrap needs was to run it, in a KVM guest of the amd64 kernel with the
kernel's `strace_err`, and fix one blocker at a time. In order:

| # | blocker | answer |
|---|---|---|
| 1 | `Can't read /proc/sys/kernel/overflowuid` | `/proc/sys/kernel/overflow{uid,gid}` = 65534 |
| 2 | `open /proc/<pid>/ns` | `/proc/<pid>/ns/{cgroup,ipc,mnt,net,pid,user,uts}`, links `name:[inode]` from the process's **box** |
| 3 | `signalfd4` -> `ENOSYS` (the parent watches its child through one) | `FileDescriptor::SignalFd`, read/poll over the thread's pending set |
| 4 | `clone(CLONE_NEWNS)` ignored: the child's `mount()` would hit the **global** table | the private namespace below |
| 5 | `mount(MS_SLAVE/BIND/REMOUNT)`, `pivot_root`, `umount2(MNT_DETACH)` | below |
| 6 | bind sources and targets are `/proc/self/fd/N` | chase the link (`resolve_symlinks`) for both |
| 7 | mounts stack on Linux (bind a dir onto itself, then bind over that) | a mount over a mount point replaces it |
| 8 | `/proc/self/mountinfo` is parsed after every bind | `/proc/<pid>/mountinfo` with real mount ids |
| 9 | `--dev` binds `oldroot/dev/null` etc. | a device-node stand-in filesystem at `oldroot/dev`; a bound node is a one-node mount |
| 10 | `mkdir` of an existing directory on a read-only mount said `EROFS` | `EEXIST` first, as Linux (the lookup precedes the write check) |
| 11 | `--unshare-net`: `if_nametoindex("lo")` and a `NETLINK_ROUTE` conversation | `SIOCGIFINDEX`; a netlink socket that acknowledges and changes nothing |
| 12 | `--unshare-user`: writes to `/proc/<pid>/{uid_map,gid_map,setgroups}` | those files, identity contents, writes swallowed |
| 13 | codex compares `fdinfo` `mnt_id` and `st_dev` with `mountinfo` | `/proc/<pid>/fdinfo/<n>`; `st_dev` = `makedev(0, mount id)` per mount |
| 14 | codex checks `capget` is empty after `--cap-drop ALL` | `capset` that empties effective+permitted is remembered (`Creds::caps_dropped`) |

## The model: a namespace is a box

Akuma has no Linux namespaces. It has **boxes**: a process's `box_id` selects a `MountNamespace` (a `MountSet`, 48
entries since this work, 16 before), a path resolves against the box's mounts first, then the box's jail, and the global
table only as the fall-through. Nested namespacing is deliberately not supported (a boxed process cannot mount, register
a box outside its subtree, ...: `docs/reference/subsystems/containers.md` § Box permissions).

* **`clone(CLONE_NEWNS)` from the host makes a fresh box** (child of nothing, root `/`, an empty mount set so the host's
  view falls through) and the child runs in it. Both kernels: glue's `sys_clone`, and the amd64 kernel's own `sys_fork`
  path (it does not go through glue's `clone`; the first attempt missed that and the child stayed in box 0).
  Only box 0 may do it, with uid 0 or `CLONE_NEWUSER`. The other `CLONE_NEW*` bits are accepted and change nothing.
* **The builder may mount into its own namespace, and only that one.** `mount_permit()` is still "host root"; the new
  `NsCreator` token (`akuma-primitives`) is minted in one place, `akuma_exec::process::ns_creator()`, when the caller's
  box is a private namespace that is not locked, and it lets the *private* mount effects (`private_ns_mount`, `_unmount`,
  `_remount`, `_pivot`, `_detach`) act on that namespace. Nothing that takes a `HostRoot` accepts it.
* **bubblewrap pivots twice**, which is what changed the first design. It pivots into a scratch tmpfs first and then binds
  *every source from `oldroot/...`*, so the old root has to stay reachable for that window; it then `umount2`s `oldroot`
  and does a second, "Russian doll" `pivot_root(".", ".")`. So:
  * `pivot_root(new, put_old)` with `put_old` **under** `new`: re-root the namespace at `new` (the mounts that were
    under it, re-based, `MountSet::rebased`) and **attach the pre-pivot view at `put_old`**: the namespace's own
    mounts, then the global table's. The namespace stays buildable.
  * `umount2(put_old, MNT_DETACH)` removes that attached tree (`unmount_tree`).
  * `pivot_root(x, x)` (put_old == new_root): there is no old root to keep. It is **mounted nowhere** and the namespace
    is **locked**: no more mounts, pivots or unshares. The `umount2(".", MNT_DETACH)` that follows is a no-op on `/`.
* So **during the setup window bubblewrap's own code can see the host's real root**. That is bubblewrap, trusted, and
  the only thing running in the box then. The untrusted command starts after the lock: it has no path to the host
  root. This is the "fudge" of `pivot_root` the design asked for: the old root exists only as long as the classic
  contract requires it, and is never reachable by the sandboxed command.

## What is built where

| piece | crate |
|---|---|
| `NsCreator` token | `akuma-primitives::privilege` |
| `register_private_ns`, `private_ns_building`, `lock_private_ns`, old-root path, sweep list | `akuma-isolation::box_registry` (host-only stubs when `containers` is off) |
| `MountSet::{rebased, unmount_tree, entries}` | `akuma-vfs` |
| `ns_creator()`, `fork_process_into` | `akuma-exec` |
| `private_ns_{mount,unmount,remount,pivot,detach}`, `resolve_for_bind`, device stand-in, `render_mountinfo`, `fdinfo` | `akuma-vfs-glue` (`sc-private-ns`) |
| `clone_namespace`, `mount`, `umount`, `pivot_root`, `signalfd`, `netlink` | `akuma-syscalls-glue` (`sc-private-ns` / `sc-eventfd`) |
| `/proc/sys/kernel/*`, `ns/`, `fdinfo/`, `mountinfo`, `uid_map` ... | `akuma-procfs` tree |
| `pivot_root` 41 / 155, `signalfd4` 74 / 289 | `akuma-syscalls-{linux,abi}` |
| `bind`, `bind-ro` for `MOUNT_IN_NS` (earlier, `c2837ba0`) | `akuma-isolation::subdir_fs`, glue `container.rs` |

Sweep: a private namespace whose processes are gone is torn down by the next `clone(CLONE_NEWNS)` (not younger than
5 s: its first process may not exist yet), so a bubblewrap run does not leak a box.

## Verified (2026-11-18, amd64 kernel, KVM, `scripts/bwrap_suite.py`)

`bwrap --bind / / echo`; `--ro-bind / /` refuses writes (`EROFS`); `--tmpfs /` shows only what was bound in; a writable
`--bind` reaches the host while `/etc` stays read-only; `/oldroot` does not exist; device nodes work; codex's flag set
(`--unshare-user --unshare-pid --unshare-ipc --unshare-net --cap-drop ALL`) runs; the host's mount table is the same
afterwards. And codex's inner stage (`codex ... --apply-seccomp-then-exec -- cmd`): it verifies capabilities, installs its
seccomp filter (`docs/archive/SECCOMP.md`), execs the command; the root is read-only and `socket(AF_INET)` is `EPERM`
inside while the same `nc` connects outside.

**Not run:** the AArch64 kernel. It shares glue's `sys_clone`, `mount`, `pivot_root` and the procfs/netlink pieces and
`cargo check -p akuma` is clean with the feature on, but no AArch64 guest was booted. The aarch64 `pivot_root` number
(41) is the asm-generic one; the amd64 dispatch arm (155) is what was exercised.

`codex sandbox linux -- CMD` fails with `execvp linux` on **real Linux too** (0.169.0 passes its own subcommand name to
the inner stage); the test uses the command line codex's runtime generates instead.

## What this does not enforce (said out loud)

* **Network.** `--unshare-net` is accepted, a `NETLINK_ROUTE` socket acknowledges its `lo` setup, and the box still shares
  the host stack. What stops a codex sandbox from connecting is codex's own seccomp filter (restricted mode). A sandbox
  without it can reach the host's loopback services.
* **Seccomp** is separate work (`docs/archive/SECCOMP.md`); this feature does not depend on it, codex's stage does.
* **User namespaces / uid maps.** One set of credentials. `uid_map`/`gid_map`/`setgroups` accept writes and change
  nothing; a dropped-capability uid 0 reports no capabilities but keeps uid 0's file overrides.
* **PID namespace.** `--unshare-pid` is accepted; the box sees the pids it always saw (the `/proc` view is box-filtered).
* **`--perms`, `--size`, `--remount-ro` on a tmpfs** are accepted; `codex-daemon-UID` therefore shows as an empty,
  writable tmpfs rather than mode 000. `MS_REC` is ignored: a bind is of one mount's view.
* `st_dev` changed for every file from a constant `1` to `makedev(0, mount id)`. `statx` follows; synthetic nodes keep 1.

## Dead ends and surprises

* First plan was a `bwrap` shim over `box`; dropped when the user pointed out boxes already give the isolation and that
  the real binary should run.
* Reading the code predicted the old root would be unneeded; the first pivot proved otherwise (the denied mount was
  `oldroot/proc -> proc`).
* The amd64 kernel's `clone` is its own (`sys_fork`), not glue's: wiring only glue left the child in box 0.
* `strace_err` shows one syscall in five with a path; `strace_nr=...` adds the rest. Both are kernel command-line flags.
* The image needs `/dev /proc /sys /run /tmp` as real directories: a `--ro-bind / /` makes them impossible to create.
