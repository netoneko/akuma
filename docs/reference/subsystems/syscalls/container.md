# container syscalls

`register_box` (316) / `kill_box` (317) / `reattach` (318) / `mount` /
`umount2` / `mount_in_ns` (325). Source: `src/syscall/container.rs`. Gated
`sc-containers` (Tier 1 — see [`../syscalls.md`](../syscalls.md) "Feature
gates & ExecRuntime stubs"). For the box isolation model, herd, and OCI
bundles — none of which are re-derived here — see
[`../containers.md`](../containers.md).

> **Stability: B (watch).** Changed 2026-08-11: `mount`/`umount2` are now
> host-only, and `mount_in_ns` gained a `data` argument and the `overlay`
> fstype. The open item lives one
> layer down: `sys_reattach`'s channel delegation is correct at this boundary,
> but the target thread's wake-up can fail to take effect (see Background) —
> a scheduler/threading bug, not a validation bug in this file.

## register_box / kill_box

`sys_register_box` (`container.rs:4`) and `sys_kill_box` (`container.rs:36`)
are thin syscall-boundary wrappers: copy the `name`/`root` strings from user
memory (`EFAULT` on a bad pointer or failed copy), then hand off to
`akuma_exec::process::register_box` / `kill_box` and
`crate::vfs::create_box_namespace` / `remove_box_namespace`. No uniqueness or
ownership check on `id` happens in this file — that's the caller's (herd's)
responsibility. `kill_box` maps "unknown box id" to `ESRCH`; any other
failure mode is not distinguished.

## reattach

`sys_reattach(pid)` (`container.rs:43`) delegates entirely to
`akuma_exec::process::reattach_process`, which re-points the target process's
output channel at the caller's and checks box-hierarchy permission (same box,
host/box-0 caller, or caller created the target's box). At the syscall
boundary the only outcome this file adds is the errno mapping: unknown `pid`
→ `ESRCH`. Permission-denied and other `Err` cases from
`reattach_process` also collapse to `ESRCH` here — the syscall does not
distinguish "no such process" from "not allowed to reattach to it".

## mount / umount2

`sys_mount` and `sys_umount2` are **host-only, and umount2 always fails.**

`sys_mount` copies `target`/`fstype` as NUL-terminated strings
(`copy_from_user_str`, capped at 256/64 bytes) and hard-codes the mountable
filesystem set to two types: `"proc"` → `ProcFilesystem`, `"tmpfs"` →
`MemoryFilesystem`; anything else is `ENODEV`. It operates on the global VFS
mount table, because only box 0 can reach it.

A caller whose `box_id != 0` gets `EPERM` from both. Until 2026-08-11 a boxed
process could mount into its own namespace and unmount anything except `/`; now
a box's namespace is composed entirely from outside, by box 0, before the box
runs — see [`../containers.md`](../containers.md) -> "Mount policy: composed
from outside, once" for why, and for the consequence that a container cannot
build a container. `sys_umount2` validates its pointer argument and then fails
for everyone: box 0 never used it either.

`_source_ptr`/`_flags`/`_data_ptr` on `sys_mount` are accepted but unused —
loopback devices, bind mounts, and mount flags are not implemented.

## mount_in_ns

`sys_mount_in_ns(box_id, target, target_len, fstype, fstype_len, data)` is the
**host-only** counterpart to `mount`: it lets a box-0 (host/herd) caller mount
into a *different*, already-running box's namespace — `EPERM` immediately if the
caller's own `box_id != 0`. This is how herd wires up a container's `/proc` or
`/tmp` before or after spawning into a box, without needing the box's own
process to call `mount` on itself. Routes through
`crate::vfs::mount_in_namespace(box_id, target, fs)`.

Fstypes: `proc`, `tmpfs`, and — added 2026-08-11 — **`overlay`**, the only one
that reads the `data` argument (a NUL-terminated option string, capped at 4096
bytes; `EINVAL` if absent or unparseable).

```
lowerdir=/var/lib/box/layers/sha256-b:/var/lib/box/layers/sha256-a,upperdir=/var/lib/box/containers/c1/upper
```

Linux's option syntax minus `workdir` (accepted and ignored; nothing here needs
a staging directory). `lowerdir` is **topmost-first**, as on Linux. Parsing is
`akuma_isolation::overlay_fs::parse_options`, a pure function with host tests;
paths must be absolute, and there is no escaping, so `:` and `,` cannot appear
in a directory name. Each directory is canonicalized and must exist and be a
directory (`ENOENT` otherwise) — a typo that silently produced an empty layer
would surface much later as a missing file inside the container. Each becomes a
`SubdirFs` over the **global** root filesystem, not the target box's: the layer
store lives outside any container, and the box is handed only the union.

Mounting an overlay at `/` goes through `MountNamespace::replace_pristine_root`
rather than `mount`, because a box's namespace already has its `SubdirFs` jail
there and `mount` rejects the duplicate. It is a one-shot, not a general
replace: it fails unless `/` still holds the untouched jail, and the syscall
additionally refuses a box that already has processes (`EPERM`), since
re-rooting a live box would move the filesystem under processes holding paths
resolved against the old one. Together with `umount2` — which never lets a box
drop its `/` — a box's root can be set once and never removed or redirected.

## Background

- [`../containers.md`](../containers.md) — the box model, herd, OCI bundles;
  read this first for anything beyond the syscall boundary.
- `archive/NAMESPACES.md` — introduced `mount`/`umount2`/`mount_in_ns` and the
  per-box `MountNamespace` table these syscalls front.
- `archive/BOX_CONTAINERS.md` §7.1 "Native Reattachment" — the design intent
  for `sys_reattach` (kernel-mediated I/O delegation, replacing `box`'s old
  manual byte-proxy).
- `archive/KNOWN_ISSUES.md` #4 "`reattach` fails to wake target process" —
  open: the target thread stays `WAITING` despite an observed wake call,
  even with the "Sticky Wake" logic in `threading.rs`/`process.rs`.
- `archive/BOX_DOCKER_COMPAT.md` — the overlay fstype, the root-swap one-shot,
  and the mount lockdown.
- `archive/SYSCALL_ERRNO_COMPLIANCE_CHANGES.md` — `sys_reattach`/`sys_kill_box`
  errno mapping (`ESRCH` on unknown id, the May 2026 cleanup referenced above).
