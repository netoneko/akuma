# container syscalls

`register_box` (316) / `kill_box` (317) / `reattach` (318) / `mount` /
`umount2` / `mount_in_ns` (325). Source: `src/syscall/container.rs`. Gated
`sc-containers` (Tier 1 — see [`../syscalls.md`](../syscalls.md) "Feature
gates & ExecRuntime stubs"). For the box isolation model, herd, and OCI
bundles — none of which are re-derived here — see
[`../containers.md`](../containers.md).

> **Stability: B (watch).** Changed 2026-08-11: `mount`/`umount2` are now
> host-only, and `mount_in_ns` gained a `data` argument and the `overlay`
> fstype. **Fixed 2026-08-23:** the reattach input stall (see Background) was
> never a wake failure — the wake fires correctly. `sys_read`'s stdin loop and
> `sys_poll_input_event` each captured their `Arc<ProcessChannel>` once before
> blocking and reused it across the whole wait; `reattach` repoints
> `Process::channel` to a new `Arc`, which an already-parked read never saw, so
> it kept draining the abandoned channel forever while new input landed
> elsewhere. Fixed in `src/syscall/fs.rs`/`src/syscall/term.rs` by
> re-resolving the channel every loop iteration instead of once outside it.
> **Also 2026-08-23:** `sys_reattach` gained a `force` argument (`screen
> -d`-style detach-and-take-over, see "reattach" below) and `box grab` no
> longer hangs forever after its target exits — it was polling `waitpid()`
> on a pid that is essentially never its own child, which can never report
> that pid's exit. **Also 2026-08-23:** every successful reattach now
> propagates the caller's terminal size to the target and sends it
> `SIGWINCH`, and a `force`-displaced holder gets a terminal-reset escape
> sequence and a real, disposition-aware `SIGTERM` rather than a raw
> force-stop — see "reattach" below for both.
> **Changed 2026-08-24 ("mount now works",** `docs/archive/MOUNT_MISSING_SYSCALLS.md`
> **§7 build list):** `sys_umount2` actually unmounts now (still `EBUSY` on
> `/`); `sys_mount` honors `MS_REMOUNT` (flips the stored `MS_RDONLY` bit via
> `crate::vfs::remount`) and gained a real `"ext2"` fstype for mounting a
> second virtio-blk disk by device name (`source`, up to 4 devices —
> `crate::block::device_index_by_name`). `statfs`/`fstatfs` now report the
> resolved mount's real stats instead of hardcoded numbers — see
> [`fs.md`](fs.md) "statfs / fstatfs", which is also where that code now
> lives (moved out of `container.rs` so it still builds without
> `sc-containers`, since both syscalls dispatch unconditionally).

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

`sys_reattach(pid, force)` (`container.rs:74`) delegates entirely to
`akuma_exec::process::reattach_process`, which re-points the target process's
output channel at the caller's and checks box-hierarchy permission (same box,
host/box-0 caller, or caller created the target's box). At the syscall
boundary this file adds two errno mappings: unknown `pid` (or a permission
denial) → `ESRCH`, and — added 2026-08-23 — "already attached" → `EBUSY`.
`ESRCH` still does not distinguish "no such process" from "not allowed to
reattach to it".

**`force` (added 2026-08-23), `screen -d`-style.** Each process tracks who
currently holds its I/O in `Process::grabbed_by` (`Option<Pid>`, set by a
successful reattach, trusted only while that pid is still alive — a grabber
that already exited leaves a harmless stale value that self-corrects on the
next check rather than needing an explicit clear on every exit path). If the
target already has a different, live holder:

- `force == 0`: refused, `EBUSY`. Nobody's channel gets stolen out from under
  them by accident.
- `force != 0`: the previous holder is detached (see below) before the
  reattach proceeds — otherwise its own wait loop has no way to learn it's
  been superseded and would spin forever against a channel nobody reads from
  anymore (see `box grab`'s exit detection, next paragraph, for the same "no
  way to learn" shape on the read side).

`userspace/box`'s `box grab` exposes this as `-d`/`--detach`; `box run`/`box
open -i`/`box use -i`/`paws`'s reattach-into-shell path all pass `force =
false` — freshly spawned children can't already have a holder.

`akuma_exec::process::reattach_process_ext` only *decides* whether someone
needs detaching — it returns `Ok(Some(previous_holder))` rather than acting,
because acting needs the disposition-aware signal path this crate can't reach
(wrong dependency direction: that logic lives in `src/syscall/proc.rs`).
`sys_reattach` (`container.rs`) does the actual detach, in order:

1. Write a soft terminal-reset escape sequence
   (`TERM_RESET_SEQUENCE` — exit alt-screen, show cursor, clear attributes,
   DECSTR soft reset, newline) into the previous holder's own channel, so
   whatever the grabbed app left that human's terminal in (raw mode,
   alt-screen, hidden cursor) doesn't survive the connection dropping — that
   state lives in the *client's* terminal emulator, unreachable once the
   connection is gone.
2. `super::proc::sys_kill(previous_holder, SIGTERM)` — the same
   disposition-aware path a plain `kill(2)` uses (not
   `kill_process_with_signal`'s crate-level hard-stop, which is for genuine
   force-kills elsewhere and skips normal cleanup), so the previous holder's
   own process exits through its ordinary `exit_group` teardown.

**Terminal size and repaint, every successful reattach (not just `force`).**
After the reattach itself succeeds, `sys_reattach` copies the caller's
`term_width`/`term_height` onto the target's `TerminalState` and sends the
target `SIGWINCH` (28) via the same `sys_kill` path — mirroring what
`screen`/`tmux` do on attach so a full-screen app (anything ncurses-based)
actually redraws against the new session's size instead of looking frozen or
misdrawn until something else prompts it. Safe unconditionally: `SIGWINCH`'s
POSIX default action is Ignore (confirmed absent from
`crate::syscall::signal::signal_is_fatal_default`), so a target with no
handler installed — a plain `cat`, say — just drops it.

**`box grab`'s exit-on-target-exit** is a userspace fix, not a kernel one, but
worth recording next to this: the grabbed process is essentially never `box
grab`'s own child (`reattach` doesn't reparent), so `waitpid()` on it can
never report an exit — Linux `wait4` semantics require an actual parent-child
relationship, and this kernel is no different. `box grab`'s loop now falls
back to a plain `kill(pid, 0)` existence probe to notice the target is gone
and exit on its own instead of spinning forever after it. See
`docs/archive/REATTACH_STALE_CHANNEL_HANG.md`.

## mount / umount2

`sys_mount` and `sys_umount2` are **host-only** (`caller_may_mount`: `box_id ==
0`). Until 2026-08-24 `umount2` failed unconditionally and `mount` ignored
`source`/`flags`; both now do real work — see the Stability note above.

`sys_mount` copies `target`/`fstype` as NUL-terminated strings
(`copy_from_user_str`, capped at 256/64 bytes) and operates on the global VFS
mount table, because only box 0 can reach it. `flags & MS_REMOUNT` takes a
short-circuit path: `crate::vfs::remount` just flips the stored `MS_RDONLY`
bit on an existing mount (`fs`/`source` args ignored, matching Linux) and
returns before the fstype switch runs — Linux requires the target to already
be a mount point, which `remount` enforces via `NotFound`.

For a fresh mount, `target` must resolve to an existing directory (`ENOTDIR`/
the resolve error otherwise — this never gates `/` or `/proc` at boot, which
bypass this arm). The fstype set is now three, not two:

| `fstype` | Filesystem | `source` |
|---|---|---|
| `"proc"` | `ProcFilesystem` | ignored |
| `"tmpfs"` | `MemoryFilesystem` | ignored |
| `"ext2"` | `crate::vfs::ext2::mount_device` on the device `source` names | required — `crate::block::device_index_by_name(source)`, `ENODEV` if no such device or it has no ext2 magic |
| anything else | — | `ENODEV` |

Any device already registered by `crate::block::init` (`vda`..`vdd`, up
to `MAX_BLOCK_DEVICES = 4`) can be mounted this way, including `vda` itself
(already mounted at `/`, so the duplicate-path check is what stops a second
root). A data-disk instance gets a small fixed 16 MiB cache cap rather than the
root's global budget, which never shrinks. `mount_errno` maps the VFS error to
its Linux errno (`NoSpace`→`ENOMEM`, `AlreadyExists`→`EBUSY`, `NotFound`→
`ENOENT`, `NotADirectory`→`ENOTDIR`, `ReadOnly`→`EROFS`, else `EINVAL`) instead
of folding everything into `EINVAL` as before.

A caller whose `box_id != 0` gets `EPERM` from both `mount` and `umount2`.
Until 2026-08-11 a boxed process could mount into its own namespace and
unmount anything except `/`; now a box's namespace is composed entirely from
outside, by box 0, before the box runs — see
[`../containers.md`](../containers.md) -> "Mount policy: composed from
outside, once" for why, and for the consequence that a container cannot build
a container.

`sys_umount2` canonicalizes `target`, refuses `/` unconditionally (`EBUSY`,
like Linux — the boot root stays for the boot's lifetime), and otherwise calls
`crate::vfs::unmount` on the global table. It only ever touches the global
table: a box's namespace mounts are composed from outside via `MOUNT_IN_NS`
and torn down with the box, so this arm cannot empty a box's namespace and
trigger the `with_fs` global-table fallback hazard documented in
[`../vfs.md`](../vfs.md).

`_data_ptr` on `sys_mount` is still accepted but unused — bind mounts and
loopback devices are not implemented.

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
  the symptom (target stays unresponsive after reattach) was real but the
  original diagnosis was wrong: the thread does wake. **Fixed 2026-08-23** —
  see the Stability note above for the actual cause (a stale channel `Arc`
  cached across the blocking wait).
- `archive/BOX_DOCKER_COMPAT.md` — the overlay fstype, the root-swap one-shot,
  and the mount lockdown.
- `archive/SYSCALL_ERRNO_COMPLIANCE_CHANGES.md` — `sys_reattach`/`sys_kill_box`
  errno mapping (`ESRCH` on unknown id, the May 2026 cleanup referenced above).
