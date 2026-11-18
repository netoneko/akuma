# Running real bubblewrap on Akuma: namespaces as boxes

**Status: design + running log (started 2026-11-18).** The reference rule that comes out of this goes in
`docs/reference/subsystems/containers.md` once the feature is verified; this file keeps the reasoning and the dead ends.

## Why

codex sandboxes every command with `bwrap` (`docs/runbooks/debug-codex-on-akuma.md` § 3 turns the sandbox off because it
cannot run). Real bubblewrap 0.12 (Alpine `bubblewrap`, musl) was run in a KVM guest of the amd64 kernel with the kernel's
`strace_err` to find, one blocker at a time, what it needs. The loop and its findings:

| # | blocker | answer |
|---|---|---|
| 1 | `bwrap: Can't read /proc/sys/kernel/overflowuid` | `/proc/sys/kernel/overflow{uid,gid}` (65534), `587aede4` |
| 2 | `open /proc/<pid>/ns/…` | `/proc/<pid>/ns/{cgroup,ipc,mnt,net,pid,user,uts}` symlinks reading `name:[inode]` from the process's **box** |
| 3 | `signalfd` / `signalfd4` → `ENOSYS` | to do |
| 4 | `clone(CLONE_NEWNS)` is ignored; the child's `mount()` would hit the **global** mount table | below |
| 5 | `mount(MS_BIND/MS_REC/MS_SLAVE/MS_REMOUNT)`, `pivot_root`, `umount2(MNT_DETACH)` | below |
| 6 | `/proc/self/{setgroups,uid_map,gid_map}` writes (only with `--unshare-user`) | below |

bubblewrap **always** `clone`s a child with `CLONE_NEWNS`, builds the new root under a tmpfs, and `pivot_root`s into it, even
with no `--unshare-*` flag, so `pivot_root` cannot be avoided by choosing flags.

## The model: a namespace is a box, `pivot_root` is a re-root with the old root dropped

Akuma has no Linux namespaces. It has **boxes**: a process's `box_id` selects a `MountNamespace` (a `MountSet<16>`), whose
`/` is a `SubdirFs` jail over a host directory; a path is resolved against the box's mounts first, then the jail, and the
global table only as the fall-through. Nested namespacing is deliberately not supported (a boxed process cannot mount,
`umount`, register a box outside its own subtree, or create a box that is not inside its own root:
`docs/reference/subsystems/containers.md` § Box permissions).

So the mapping is:

* **`clone(CLONE_NEWNS)` / `unshare(CLONE_NEWNS)` from box 0 allocates a fresh box** (a child of the caller's, root `/`,
  an empty mount set) and the child runs in it. The mounts it makes land in *its* namespace, never in the global table. This
  is what removes the hazard of #4: real `bwrap` run on a kernel that ignores `CLONE_NEWNS` would `mount` over the
  host's `/tmp` and `pivot_root` the host. The other namespace flags (`NEWPID`, `NEWIPC`, `NEWUTS`, `NEWNET`, `NEWUSER`)
  are accepted and change nothing; what that leaves unenforced is listed below.
* **The creator may mount into its own namespace until it pivots.** Today `mount_permit` is "box 0 and uid 0" and every
  other caller gets `EPERM`. The new rule is one function beside it: a process whose box is a namespace *it created* and
  has not pivoted may `mount` (into that namespace's set, via `mount_in_namespace`), not into the global table. Nobody else
  gains anything.
* **`pivot_root(new_root, put_old)` re-roots the box's namespace at `new_root` and drops the old root.** The new
  namespace is built from the building one: the root becomes a `SubdirFs` over whatever `new_root` resolves to (typically
  a directory of the tmpfs `bwrap` mounted), and every mount whose target lies under `new_root` is kept, re-based. Mounts
  outside it are dropped. `put_old` is left as an empty directory and the old root is mounted **nowhere**, so there is no
  path to the real root: a classic `pivot_root` leaves it reachable until the caller `umount2`s it, and that window is
  exactly what the fudge removes. The namespace is then **locked**.
* **A locked namespace accepts no further `mount`, `pivot_root`, `unshare` or `clone(CLONE_NEW*)`.** `umount2` of a path that
  is not a mount point succeeds (bubblewrap calls `umount2("/oldroot", MNT_DETACH)` after the pivot, where Linux has the
  old root mounted and Akuma has nothing); `umount2` of a real mount in a locked namespace is `EPERM`.
* **Bind mounts** (`MS_BIND`) resolve the *source* through the caller's current view (`resolve_mount`) and mount a
  `SubdirFs` over it, read-only when a later `MS_REMOUNT|MS_RDONLY` asks (`SubdirFs::new_read_only`, `c2837ba0`). `MS_REC`,
  `MS_SLAVE`/`MS_PRIVATE` (propagation) are accepted and ignored: a namespace has no shared subtrees to leak.

## What this does not enforce (said out loud, so nobody believes more than is true)

* **Network.** `--unshare-net` is accepted and the box still shares the host stack. A no-network box needs a per-box socket
  gate; until then codex's "network restricted" is not enforced by the sandbox.
* **Seccomp.** `prctl(PR_SET_SECCOMP)` is `EINVAL`; codex's `--apply-seccomp-then-exec` stage is separate work.
* **User namespaces / uid maps.** `--unshare-user` is accepted; the `uid_map`/`gid_map`/`setgroups` writes are swallowed.
  There is one set of credentials; nothing is remapped.
* **PID namespace.** `--unshare-pid` is accepted; the box sees the pids it always saw.

## Audit to run when this lands

A separate pass, by design: list every syscall the kernel refuses outside box 0 and decide which should be added. See
`docs/archive/BOX0_ONLY_AUDIT.md` (written after the feature).
