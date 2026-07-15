# container syscalls

`register_box` (316) / `kill_box` (317) / `reattach` (318) / `mount` /
`umount2` / `mount_in_ns` (325). Source: `src/syscall/container.rs`. Gated
`sc-containers` (Tier 1 — see [`../syscalls.md`](../syscalls.md) "Feature
gates & ExecRuntime stubs"). For the box isolation model, herd, and OCI
bundles — none of which are re-derived here — see
[`../containers.md`](../containers.md).

> **Stability: B (watch).** The syscalls themselves are thin and quiet (last
> substantive change: May 2026 errno-code cleanup). The open item lives one
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

`sys_mount` (`container.rs:50`) and `sys_umount2` (`container.rs:88`) copy
`target`/`fstype` as NUL-terminated strings (`copy_from_user_str`, capped at
256/64 bytes) and hard-code the mountable filesystem set to exactly two
types: `"proc"` → `ProcFilesystem`, `"tmpfs"` → `MemoryFilesystem`; anything
else is `ENODEV`. Routing then depends on the **caller's own** `box_id`:

- `box_id == 0` (host): `mount` operates on the global VFS mount table;
  `umount2` unconditionally returns `EPERM` — the host cannot unmount through
  this syscall.
- `box_id != 0`: both operate on `proc.namespace.mount`, the box's own
  per-box mount namespace.

`_source_ptr`/`_flags`/`_data_ptr` on `sys_mount` are accepted but unused —
loopback devices, bind mounts, and mount flags are not implemented.

## mount_in_ns

`sys_mount_in_ns(box_id, ...)` (`container.rs:108`) is the **host-only**
counterpart to `mount`: it lets a box-0 (host/herd) caller mount into a
*different*, already-running box's namespace — `EPERM` immediately if the
caller's own `box_id != 0`. This is how herd wires up a container's `/proc`
or `/tmp` before or after spawning into a box, without needing the box's own
process to call `mount` on itself. Same two-entry fstype allow-list as
`mount`; routes through `crate::vfs::mount_in_namespace(box_id, target, fs)`.

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
- `archive/SYSCALL_ERRNO_COMPLIANCE_CHANGES.md` — `sys_reattach`/`sys_kill_box`
  errno mapping (`ESRCH` on unknown id, the May 2026 cleanup referenced above).
