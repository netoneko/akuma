# Containers / boxes / herd

Current-state architecture for the box isolation model, the `herd` supervisor,
and OCI support.

> **Stability: B (watch).** Active again 2026-08-11 (OCI overlay roots, `box
> run`, mount lockdown — see "Mount policy" and "OCI images and the overlay
> root"). The box model
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
| `MOUNT_IN_NS` | caller must be box 0. With fstype `overlay` at `/`, the target box must additionally have **no processes** and a still-pristine root | `EPERM` |
| `MOUNT` | caller must be box 0 (since 2026-08-11 — a boxed process could previously mount into its own namespace) | `EPERM` |
| `UMOUNT2` | caller must be box 0 (since 2026-08-11 — a box could previously unmount anything but `/`); real since 2026-08-24, always `EBUSY` on `/` | `EPERM` |

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
| `env` | [] | **repeatable**: one `env = KEY=VALUE` per line. See below |
| `core` | 0 | core pin via the `core_init` syscall (mutually exclusive with `boxed`); the kernel side is now a permanent `ENOSYS` stub — the one-kernel-per-core multikernel it activated was removed, see `docs/archive/TRIM_FAT_MULTIKERNEL.md` — so herd treats it as unavailable on every current build |

**Lifecycle:** service starts → runs → on exit: `oneshot` → `Completed`; else
`restart` → respawn after `restart_delay_ms`, up to `max_retries`. `herd status`
lists services + states.

> `restart_delay` / `restart_delay_ms` and `start_delay` / `start_delay_ms` are
> both accepted. Until 2026-08-20 the parser matched only the un-suffixed
> spelling while this table and the shipped devbox `sshd.conf` both wrote `_ms`,
> so those lines fell through the config parser's `_ => {}` and did nothing —
> the devbox sshd's documented 10-second wait for box 0's rump DHCP handshake
> had never actually happened.

### `env` — service environment

Repeatable rather than space-split like `args`, so a value may contain spaces
(`env = JAVA_OPTS=-Xmx256m -Xms64m`). Only the **first** `=` separates name from
value, so a value may contain more of them (`env = DSN=host=db port=5432`).

Entries override **by name**, and what they override depends on the service:

| service kind | base environment |
|---|---|
| `bundle` | the bundle's own OCI `process.env` |
| everything else | `PATH`, `HOME`, `TERM` (`boxlib::spec::DEFAULT_ENV`) |

**A service with no `env` line passes no environment at all**, and the kernel
applies its own `DEFAULT_ENV`. That is deliberate rather than incidental: the
kernel treats a supplied environment as the *whole* environment, so composing
unconditionally would have changed every existing service. Adding even one `env`
line makes the service's environment entirely its own, which is why the defaults
are re-applied as a base rather than dropped — otherwise a service that gained
one variable would silently lose `HOME` and `TERM`.

Worked example, including the traps:
[`../../../overlays/devbox/rootfs/etc/herd/available/redis-boxed.conf.example`](../../../overlays/devbox/rootfs/etc/herd/available/redis-boxed.conf.example).

### devbox sshd.conf (reference)

```
command = /bin/sshd
args = --port 22 --shell /bin/sh
start_delay_ms = 10000     # wait for box 0's ~5s rump DHCP (honoured since 2026-08-20)
restart = true
# UNBOXED — box 0 itself is rump under rump-default, no join_box needed
```

## Mount policy: composed from outside, once

A box's mount namespace is built **entirely by box 0**, through `MOUNT_IN_NS`,
before anything runs in the box. A boxed process cannot mount, cannot unmount,
and cannot re-root itself.

The reasoning is that a mount table is the box's whole view of the filesystem.
Anything a box can mount, it can mount *over* — its own `/proc`, a directory its
supervisor is watching, the path another process resolves against — and a box
that can shadow paths inside itself is a box whose isolation is described by
whatever it did last rather than by what its creator set up.

A box's root gets the same treatment in time as well as space:
`MountNamespace::replace_pristine_root` fails unless `/` still holds the
untouched `SubdirFs` jail, and `MOUNT_IN_NS` refuses to re-root a box that
already has processes. So a root can be set once, at creation, and never
redirected — swapping a live one would move the filesystem under processes
holding paths and cwds resolved against the old one. The complementary rule is
`umount2`, which never lets a box lose its `/` (an empty namespace makes
`with_fs` fall back to the **global** mount table — the whole host filesystem).

**Consequence: no nested OCI images.** Composing a container root requires an
overlay mount, and no box can mount. Nested **boxes** are unaffected — they are
process and network-stack grouping, and a box may still register a child box
rooted inside its own subtree. Docker-in-docker can be revisited later; nothing
needs it now.

Regression: `test_box_isolation_syscall_guards` cases 8b–8d.

## OCI images and the overlay root

`box run <image>` starts a container whose root is the image's layers, read-only
and shared, with a private writable directory on top. Implemented 2026-08-11;
task-level procedure in
[`../../runbooks/run-docker-image.md`](../../runbooks/run-docker-image.md).

### Layer store

`box pull` extracts each layer into its own content-addressed directory. The
image directory holds metadata only:

```
/var/lib/box/layers/sha256-<hex>/          shared by every image that uses it
/var/lib/box/images/<name>/oci-config.json
/var/lib/box/images/<name>/layers          digest per line, base-first
/var/lib/box/containers/<id>/upper         one container's writable layer
```

A layer already present is not re-downloaded. Extraction stages in
`<digest>.tmp` and renames into place, so an interrupted pull cannot leave a
directory that looks complete. Whiteout entries are left **as files** — they
are part of the layer per the OCI spec and the overlay interprets them at
lookup time, so an extracted layer needs no rewriting.

### `OverlayFs`

`crates/akuma-isolation/src/overlay_fs.rs`. One writable upper over N read-only
lowers, all `SubdirFs` over the same ext2. Layers are indexed topmost-first
(0 = upper); a name is served by the lowest index that has it.

- **Lookup walks one component at a time**, so a whiteout or opaque marker on an
  *ancestor* hides the subtree beneath it. A plain file at an intermediate
  component ends resolution rather than letting the path resume in a layer that
  file is hiding.
- **Whiteouts** use the names the registry already ships: `.wh.<name>` hides
  `<name>` below, `.wh..wh..opq` hides a directory's entire lower content while
  leaving the directory visible.
- **Writes copy up**: parent chain materialized in upper, file copied from the
  winning lower, then delegated. **Deletes** that cannot touch a read-only lower
  write a whiteout instead. Re-creating a deleted name clears its whiteout.
- **Directory rename returns `NotSupported`** — the subtree copy-up plus
  per-name whiteouts it would need is not implemented.

**Every layer must sit on the same underlying filesystem.** `read_at_by_inode`
is forwarded on the assumption that an inode number identifies a file across all
of them, and the file page cache is keyed on inode alone
(`src/file_page_cache.rs`). A `MemoryFilesystem` upper would break this —
memfs synthesizes inodes by hashing the path — and would hand the page-fault
path an unrelated file's contents. This is why a tmpfs upper for `--rm` is
deferred rather than "obviously fine".

Host-tested in the crate: whiteouts at every level, opaque directories, merge
and dedupe, copy-up, whiteout clearing, layer capping (`MAX_LOWER_LAYERS = 32`).

### How a container root is assembled

1. `box run` registers the box with `root_dir` = the container directory, which
   is what the kernel validates and jails to.
2. `create_box_namespace` mounts a `SubdirFs` at `/` as usual.
3. `MOUNT_IN_NS` with fstype `overlay` and
   `lowerdir=<top>:…:<base>,upperdir=<container>/upper` **replaces** that `/`
   via `MountNamespace::replace_pristine_root` — `mount` itself rejects the
   duplicate. See "Mount policy" above for why this is a one-shot.
4. `box run` injects `/etc/resolv.conf` and `/etc/hosts` into the upper layer,
   which no OCI image ships and every networked program expects.

5. Image `Env` is the container's environment, and `-e KEY=VALUE` (repeatable)
   overrides it — see below.

### Environment

`SpawnOptions` carries `env_ptr`/`env_len`, and `box run` composes what goes in
them the way `docker run` does:

```
image config `Env`   →   -e overrides, by name   →   PATH guaranteed
```

Three rules, each of which is a way to get it wrong:

- **Overrides replace in place.** `-e LANG=x` against an image that sets `LANG`
  replaces that entry where it sits rather than appending a second one — two
  entries for one name leaves the winner up to the libc. Later `-e` beats
  earlier, so `-e A=1 -e A=2` is `A=2`.
- **Only the first `=` splits.** `-e DSN=host=db port=5432` is one variable
  whose value contains both a space and an `=`.
- **`PATH` is added if the composition lacks it.** The kernel treats a supplied
  environment as the *whole* environment, so a list without `PATH` would leave
  the container unable to resolve a bare program name. Images essentially always
  set it; `boxlib::spec::DEFAULT_PATH` covers the case where one does not.

`-e KEY` with no `=` is Docker's passthrough: the value comes from `box`'s own
environment, and a name that is not set there is dropped rather than passed as
empty.

The ABI is **size-negotiated**, not versioned by a flag: `SPAWN_EXT`'s third
argument is the caller's `sizeof(SpawnOptions)`, and `env_ptr`/`env_len` were
appended after `box_id` so every earlier offset is unchanged. A `/bin/box` or
`/bin/herd` older than the kernel — they are separate artifacts on one image, so
this happens — passes 72, the kernel reads only those bytes, and the container
gets the default environment instead of the kernel reading whatever followed the
caller's struct on the stack and using it as an `envp`.

Before 2026-08-20 image `Env` was dropped entirely and a container got the
kernel's `DEFAULT_ENV`. The visible consequence was `PATH`: it was `/usr/bin:/bin`
until 2026-08-16, so a container's own shell could not find the binary the image
installs under `/usr/local/bin` and every official Entrypoint died on
`exec: <prog>: not found`. `box run` still resolves a bare Entrypoint against the
standard `PATH` directories itself, since the kernel's spawn takes a path.

### Not implemented
- **No per-process credentials**, and this is the live blocker for running an
  official image under its own Entrypoint. `setresuid`/`setuid`/`setgid`/
  `setgroups`/`capset` are accepting no-ops and `getuid`/`geteuid` hardcode 0,
  so the near-universal entrypoint shape
  `exec setpriv --reuid=<user> -- "$0" "$@"` guarded by `[ "$(id -u)" = '0' ]`
  **re-execs itself forever**: the child sees uid 0 again and takes the same
  branch. Not a crash — the container simply never starts its program and
  prints nothing. Workaround is `--entrypoint <the real binary>`; the fix is
  uid/gid fields on `Process`, reported rather than enforced.
  [`../../archive/DEVBOX_ISSUES.md`](../../archive/DEVBOX_ISSUES.md) Issue 15.
- No layer GC, no `box rmi`, no digest pinning.
- Full OCI runtime spec (cgroups, capabilities, seccomp, devices) — Akuma's
  isolation is namespace + network-stack, not a full container runtime.

## OCI bundle support (herd)

herd's `bundle` field points at an OCI bundle directory (`config.json` +
rootfs). If set, overrides `command`/`box_root`. Basic bundle loading works;
this predates and is separate from the `box run` path above. See
`archive/CONTAINERS_STAGE_2_PLAN.md`.

## `box` userspace tool

`userspace/box/` — the CLI for `box run`, `box use`, `box open`, image pull.
`archive/BOX_CONTAINERS.md` is the proposal;
`userspace/box/docs/OCI_IMAGE_PULL.md` (pull pipeline),
`userspace/box/docs/BOX_RUN.md` (containers + overlay) and
`userspace/box/docs/TESTING.md` cover the implementation.

Note `box run` and `box open -i` differ: `run` builds an overlay root and is the
docker-shaped command; `open -i` still points the box root directly at an
image's legacy flat `rootfs/` directory, which `box pull` no longer creates.

## PTY-in-box (interactive SSH into a box)

`archive/BOX_PTY_INTERACTIVE_SHELL.md` — the interactive shell bridge into a
boxed service.

## Background

- `archive/BOX_CONTAINERS.md`, `archive/CONTAINERS_STAGE_1_PLAN.md`,
  `archive/CONTAINERS_STAGE_2_PLAN.md`, `archive/RUMP_PLUS_HERD.md`.
- `archive/BOX_PTY_INTERACTIVE_SHELL.md`, `archive/BOX_SUBDIR_FS_LIMITATIONS.md`.
- `archive/BOX_ISOLATION_SECURITY_FIXES.md` — the nine unenforced boundaries and
  how each is gated now.
- `archive/BOX_DOCKER_COMPAT.md` — `box run`, the layer store and `OverlayFs`,
  the mount lockdown, and the two bugs found underneath (busybox tar +
  `linkat`-copies-files; `spawn_ext` and shebangs).
- `archive/MOUNT_MISSING_SYSCALLS.md` — the audit behind `umount2`/`MS_REMOUNT`/
  multi-disk `mount(2)` becoming real 2026-08-24; see
  [`syscalls/container.md`](syscalls/container.md) "mount / umount2" for the
  current syscall-boundary behavior.
- `userspace/box/docs/OCI_IMAGE_PULL.md`, `userspace/herd/docs/CORE_AWARE_SCHEDULING.md`.
- `archive/REDIS_END_TO_END.md` — running the official `redis:alpine` image end
  to end: what `#!`-in-spawn, the `PATH`, the credential syscalls and
  `/proc/self` each cost, and which one is still open.
  `../../runbooks/run-redis.md` is the recipe.
