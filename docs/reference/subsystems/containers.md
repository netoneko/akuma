# Containers / boxes / herd

Current-state architecture for the box isolation model, the `herd` supervisor,
and OCI support.

> **Stability: B (watch).** Low volume but active through June. The box model
> is implemented; the `stack=rump` herd box path is **partly** implemented
> (Phase 5 / open). herd's config schema is the live surface — verify fields
> against `userspace/herd/src/main.rs` before relying on one. Box permissions
> were only enforced from 2026-08-08 — see "Box permissions" below before
> trusting any older statement about what a box can reach.

For the rump stack itself, see [`rump-stack.md`](rump-stack.md). For network
box routing, see [`networking.md`](networking.md).

## The box model

Akuma isolates processes into **boxes**, each with its own:
- **network stack** (native smoltcp **or** rump), keyed on `box_id`
- **VFS namespace** (optional `SubdirFs` fresh root)

- **Box 0** is the root box every process starts in. Normally smoltcp; `rump-default` (devbox) flips it to rump.
- `box_id` is per-process. The dispatch hook `intercept_box_syscall` enforces stack routing as a hard guarantee.
- A process spawns into or `join_box`s into another box for isolation.

Box syscalls live in `src/syscall/container.rs` (gated `sc-containers`). Rump
box bookkeeping: `mark_box_rump(box_id)` / `box_is_rump(box_id)` /
`RUMP_BOXES` (`src/rump_proxy.rs:67-84`).

## Box permissions

Every syscall that crosses a box boundary gates on
`akuma_exec::process::box_access` (`crates/akuma-exec/src/box_mod/access.rs`),
which is pure logic over a `registry_snapshot()`. The caller's identity comes
from one place, `container::caller_box_and_pid()`; a kernel thread with no
`Process` (built-in shell, boot path) counts as box 0.

`can_access_box(source, target)` allows: box 0 → anything; a box → itself;
a box → its **descendants** (via `parent_box_id` ancestry); and a fallback for
the box's recorded `creator_pid`.

| Syscall | Rule | Denied with |
|---|---|---|
| `REGISTER_BOX` | `can_register_box`: re-registering a live box needs `can_access_box`; a **new** box becomes a child of the caller's box and its `root_dir` must lie inside the caller's own root (`validate_nested_root`, component-boundary match). `root_dir` is canonicalized first. Box 0 cannot be created | `EPERM` |
| `KILL_BOX` | `can_kill_box`. The namespace is dropped only after the kill succeeds | `EPERM` |
| `SPAWN_EXT` (`box_id != 0`) | `can_access_box` — the child inherits that box's `box_id` **and** its mount namespace. `box_id == 0` means "inherit the caller's box" and is unchecked | `EPERM` |
| `SET_BOX_STACK` | `can_access_box` | `EPERM` |
| `MOUNT_IN_NS` | caller must be box 0 | `EPERM` |
| `UMOUNT2` | box 0 may not use it at all; a boxed process may not unmount **`/`** — that is its `SubdirFs` jail root, and an empty namespace falls back to the global mount table | `EPERM` |

`SubdirFs` resolves `.`/`..` and clamps at the virtual root before prefixing, so
a `..` cannot ascend out of `box_root` even if a caller reaches the filesystem
without going through `with_fs`. Mount targets are canonicalized because
`MountNamespace` compares mount points literally.

`reattach` has always enforced the same hierarchy rule, inside
`reattach_process_ext` (`crates/akuma-exec/src/process/exec.rs`).

None of the above was enforced before 2026-08-08 — the pure helpers existed and
were unit-tested but had no callers, and `parent_box_id` was hardcoded `None`,
which left every ancestry check blind. Full write-up:
[`../../archive/BOX_ISOLATION_SECURITY_FIXES.md`](../../archive/BOX_ISOLATION_SECURITY_FIXES.md).
Regression: `test_box_isolation_syscall_guards` in the boot suite.

### Two ways a box gets rump

1. **`rump-default` (devbox):** the kernel marks box 0 rump at boot and brings
   up its `rump_server` itself. Every unboxed process routes to it. No herd
   box, no `join_box`. See [`rump-stack.md`](rump-stack.md).
2. **`stack=rump` herd box (Phase 5, partly open):** a herd-owned
   `rump_server` in a **fresh box** that processes must `join_box` into. This
   is the path for arbitrary additional rump boxes on a default-smoltcp build.
   Status: per-box proxy machinery is done; herd's full `stack` selector +
   bundle generation are open. See `archive/RUMP_PLUS_HERD.md`.

## herd — the supervisor

`userspace/herd/src/main.rs`. Reads `.conf` files from `/etc/herd/enabled/`,
spawns + supervises each service. Config schema (`ServiceConfig`, `main.rs:115-157`):

| Field | Default | Meaning |
|---|---|---|
| `command` | — | binary path |
| `args` | — | argv (single string, space-split) |
| `restart` | true | restart on exit |
| `restart_delay_ms` | (DEFAULT) | delay between restart attempts |
| `max_retries` | (DEFAULT) | cap before giving up |
| `oneshot` | false | run once → `Completed` (never restarted); a reboot runs it again |
| `start_delay_ms` | 0 | defer the INITIAL start (e.g. wait for a box's rump handshake) |
| `boxed` | false | spawn in a fresh box |
| `box_root` | "/" | box's root dir (non-"/" → `SubdirFs` fresh root) |
| `bundle` | "" | OCI bundle dir; overrides command/box_root if set |
| `stack` | "" / "smoltcp" | "rump" routes the box's net to a rump box |
| `join_box` | "" | join an existing box (e.g. sshd `join_box = rumpnet`) |
| `mount_fs` | [] | mount points to create ("proc"/"tmpfs"); a fresh-root box has no /proc unless mounted |
| `core` | 0 | multikernel core pin (mutually exclusive with `boxed`) |

**Lifecycle:** service starts → runs → on exit: `oneshot` → `Completed`; else
`restart` → respawn after `restart_delay_ms`, up to `max_retries`. `herd status`
lists services + states.

### devbox sshd.conf (reference)

```
command = /bin/sshd
args = --port 22 --shell /bin/sh
start_delay_ms = 10000     # wait for box 0's ~5s rump DHCP
restart = true
# UNBOXED — box 0 itself is rump under rump-default, no join_box needed
```

## OCI bundle support

`bundle` field points at an OCI bundle directory (`config.json` + rootfs). If
set, overrides `command`/`box_root`. Status: basic bundle loading works; full
OCI runtime spec (cgroups, capabilities, seccomp, devices) is **not**
implemented — Akuma's isolation is namespace + network-stack, not a full
container runtime. See `archive/CONTAINERS_STAGE_2_PLAN.md`.

## `box` userspace tool

`userspace/box/` — the CLI for `box use`, `box open --net`, image pull.
`archive/BOX_CONTAINERS.md` is the proposal; `userspace/box/docs/OCI_IMAGE_PULL.md`
+ `userspace/box/docs/TESTING.md` cover the implementation.

## PTY-in-box (interactive SSH into a box)

`archive/BOX_PTY_INTERACTIVE_SHELL.md` — the interactive shell bridge into a
boxed service.

## Background

- `archive/BOX_CONTAINERS.md`, `archive/CONTAINERS_STAGE_1_PLAN.md`,
  `archive/CONTAINERS_STAGE_2_PLAN.md`, `archive/RUMP_PLUS_HERD.md`.
- `archive/BOX_PTY_INTERACTIVE_SHELL.md`, `archive/BOX_SUBDIR_FS_LIMITATIONS.md`.
- `archive/BOX_ISOLATION_SECURITY_FIXES.md` — the nine unenforced boundaries and
  how each is gated now.
- `userspace/box/docs/OCI_IMAGE_PULL.md`, `userspace/herd/docs/CORE_AWARE_SCHEDULING.md`.
