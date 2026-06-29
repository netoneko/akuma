# herd — Akuma's userspace process supervisor

`herd` is Akuma's init-style service supervisor: a single long-running userspace
process that starts the system's background services, captures their output, and
restarts them when they die. It is the moral equivalent of `runit`/`s6`/`systemd`
for Akuma — deliberately small, config-file driven, and `no_std` (musl libc via
`libakuma`).

> Named "herd" because herding cats is an apt metaphor for managing processes.

- **Source:** `userspace/herd/src/main.rs` (single file).
- **Built by:** `userspace/build.sh` (or `userspace/build.sh --herd-only`).
- **Runs as:** an EL0 process spawned at boot (gated by the kernel's
  `AUTO_START_HERD`; see [autostart cost notes] in the kernel memory) or launched
  manually over SSH.
- **Related docs:** [`docs/CORE_AWARE_SCHEDULING.md`](docs/CORE_AWARE_SCHEDULING.md)
  (pinning services to multikernel cores) and the kernel's
  `docs/MULTIKERNEL.md`.

---

## What it does

Run with no arguments (or `herd daemon`), it becomes a foreground supervisor:

1. **Ensures its directories exist** — `/etc/herd/enabled`, `/etc/herd/available`,
   `/var/log/herd`.
2. **Loads config** — reads every `*.conf` in `/etc/herd/enabled` and builds an
   in-memory service table.
3. **Starts enabled services** — spawns each, honoring any per-service start delay.
4. **Supervises forever** in a ~100 ms poll loop:
   - drains each running service's stdout into `/var/log/herd/<svc>.log`
     (with rotation at 32 KB),
   - reaps exited children (`waitpid`) and applies restart policy,
   - fires due restarts,
   - reloads config every 20 s (picking up newly enabled / disabled services).

Run with a subcommand, it's a thin CLI for managing service config files instead
(see [CLI](#cli)).

---

## Directory layout

| Path | Purpose |
|---|---|
| `/etc/herd/available/` | Service definitions that *exist* but are not started. `herd add` writes here. |
| `/etc/herd/enabled/`   | Service definitions herd actually supervises. `herd enable` copies an available `.conf` here; `herd disable` removes it. |
| `/var/log/herd/`       | Per-service captured stdout: `<svc>.log` (current) + `<svc>.log.old` (rotated). |

A service is just a `<name>.conf` file. The *file name* (minus `.conf`) is the
service name. "Enabling" is literally "present in `enabled/`"; the supervisor
discovers services by listing that directory.

---

## Config format

Plain `key = value`, one per line. Blank lines and `#` comments are ignored.
Unknown keys are silently skipped (forward-compatible). The only required key is
`command` — a config with no `command` is rejected.

```conf
# /etc/herd/enabled/httpd.conf
command       = /bin/httpd
args          = -p 8080 /srv/www
restart_delay = 1000     # ms to wait before a restart
max_retries   = 0        # 0 = retry forever
restart       = true     # restart on non-zero exit (default true)
```

### All keys

| Key | Type | Default | Meaning |
|---|---|---|---|
| `command` | path | *(required)* | Executable to spawn. |
| `args` | space-separated | *(none)* | Arguments passed after `command`. |
| `restart_delay` | u64 ms | `1000` | Delay before a scheduled restart. |
| `max_retries` | u32 | `0` | Max restarts before giving up; `0` = infinite. |
| `restart` | bool | `true` | Whether to restart on non-zero exit. Set `false` for services whose restart needs special handling (e.g. a `rump_server` whose kernel sysproxy channel must be re-established). |
| `oneshot` | bool | `false` | Run exactly once: when the service exits (any code) it moves to the terminal `Completed` state and is never restarted. Overrides `restart`. A reboot runs it again. Use for boot-time one-off tasks. |
| `start_delay` | u64 ms | `0` | Defer the *initial* start by this long (e.g. a `join_box` service waiting for its target box's `rump_server` handshake). |
| `boxed` | bool | `false` | Run the service inside a container (box). |
| `box_root` | path | `/` | Root directory for the box's filesystem namespace. |
| `bundle` | dir | *(none)* | Path to an OCI bundle directory; reads `config.json` for root/args/env/mounts. Implies `boxed = true`. |
| `stack` | `""`/`smoltcp`/`rump` | `""` | Network stack for the box. `rump` routes the box's `AF_INET` through its `rump_server` via the kernel sysproxy client. |
| `join_box` | name | *(none)* | Spawn into an **existing** box (by name) instead of registering a new one. Implies `boxed = true`. The target box must already exist and be stack-marked by its owner service. |
| `mount` | space-separated | *(none)* | Filesystems to mount in the box's namespace before spawning. Only `proc` (→ `/proc`) and `tmpfs` (→ `/tmp`). A fresh-root box has no `/proc` otherwise — sshd's interactive bridge needs `/proc/<pid>/fd/0`. |
| `core` | u32 | `0` (BSP) | Run the service on a specific multikernel secondary core. `0`/unset = spawn locally on the BSP (default). Non-zero = herd calls `core_init(N, command)` so core N's kernel spawns it locally (no cross-core spawn, no local pid). Mutually exclusive with boxes. See [`docs/CORE_AWARE_SCHEDULING.md`](docs/CORE_AWARE_SCHEDULING.md). |

---

## Service lifecycle

Each supervised service moves through these states (`Completed` only applies to
`oneshot` services):

```
            start_service (spawn ok)
  Stopped ───────────────────────────▶ Running
     ▲                                    │
     │ clean exit (code 0, or restart=false)│ child exits
     │                                    ▼
     │                          ┌──── exit code != 0 && restart ────┐
     │                          │                                   │
     │            max_retries hit│                    within retries│
     └──────────◀── Failed ◀─────┘                                  ▼
                                                            PendingRestart
                                                                   │
                                              restart_at_ms elapsed │
                                                                   ▼
                                                              (re)start
```

- **Stopped** — known but not running (freshly loaded, cleanly exited, or disabled).
- **Running** — spawned; herd holds its pid + a stdout fd it polls.
- **PendingRestart** — exited non-zero, waiting out `restart_delay` before respawn.
  The retry **always uses the same config** (including, in future, the same
  `core` pin — never a different one).
- **Failed** — spawn failed, or restarts exhausted (`max_retries` reached), or a
  misconfiguration (e.g. an unreadable OCI bundle).

A **clean exit** (code 0, or any exit when `restart = false`) returns the service
to **Stopped** and resets the restart counter — and a Stopped service is brought
back up by the next supervisor pass. To run something **once**, set `oneshot = true`:
on exit it goes to **Completed** (terminal, never restarted) instead of Stopped.

---

## Containers (boxes)

herd integrates with Akuma's box/namespace machinery (`src/syscall/container.rs`)
via three kernel syscalls. There are three ways to run a service in a box:

1. **`boxed = true` + `box_root`** — herd generates a box id from the name,
   registers the box with that root, optionally marks its network stack, mounts
   the requested filesystems, then spawns the command into it.
2. **`bundle = <dir>`** — OCI mode. herd reads `<dir>/config.json`, extracting
   `root.path`, `process.args`/`env`/`cwd`, and `mounts`, and runs the bundle's
   `args[0]` in a box rooted at the bundle's rootfs. (A minimal JSON parser lives
   in `main.rs` — `json_get_str`/`json_get_str_array`/`json_get_object`/
   `json_get_mounts`.)
3. **`join_box = <name>`** — spawn into a box another service already owns (e.g.
   `sshd` joining the `rumpnet` box so its `AF_INET` is sysproxy-routed to that
   box's `rump_server`). herd does **not** register or stack-mark the box here —
   the owner does. `start_delay` + `restart` cover the race where the owner hasn't
   registered the box yet.

`stack = rump` must be set **before** the box's process is spawned so the kernel
wires a sysproxy channel onto fd 3. herd owns the `rump_server` lifecycle (one
server, no kernel-spawned second one); the kernel only attaches the channel and
drives the proxy.

---

## CLI

```
herd <command> [args]

  daemon | run | fg     Run the supervisor in the foreground (default with no args).
  status                List enabled services.
  add <svc>             Create /etc/herd/available/<svc>.conf from a template.
  config <svc>          Print a service's config (checks enabled/ then available/).
  enable <svc>          Copy available/<svc>.conf into enabled/.
  disable <svc>         Remove enabled/<svc>.conf.
  log <svc>             Print /var/log/herd/<svc>.log.
  help | --help | -h    Usage.
```

Note: `enable`/`disable` only edit the config files. A running daemon picks the
change up on its next 20 s config reload; a fresh boot picks it up at start.

---

## Logging & rotation

herd polls each running service's stdout fd every loop and appends what it reads
to `/var/log/herd/<svc>.log`. When a write would push the file past `MAX_LOG_SIZE`
(32 KB), the current log is moved to `<svc>.log.old` and a new log is started.
View logs with `herd log <svc>` or read the files directly.

---

## Kernel ABI

herd talks to the kernel through `libakuma::syscall` for the box-aware paths
(plain non-boxed spawns use `libakuma::spawn`). The relevant syscall numbers and
the `SpawnOptions` struct are defined in `main.rs`:

| Syscall | # | Used for |
|---|---|---|
| `SYSCALL_SPAWN_EXT`   | 315 | Spawn with full options (cwd, root, argv pointer array, box id). |
| `SYSCALL_REGISTER_BOX`| 316 | Create/update a box's name + root + primary pid. |
| `SYSCALL_SET_BOX_STACK`| 324 | Mark a box's network stack (`rump`). |
| `SYSCALL_MOUNT_IN_NS` | 325 | Mount `proc`/`tmpfs` into a box's namespace. |
| `SYSCALL_CORE_INIT`   | 327 | Activate a multikernel secondary core to run a named program (the `core = N` path). `core_init(idx, path_ptr)`. |

> **Core pinning does NOT use `SpawnOptions`.** A `core = N` service is *not* a cross-core
> spawn — herd calls `core_init(N, command)` (syscall 327), which hands the kernel the program
> path; core N's own kernel then spawns it locally (`docs/MULTIKERNEL.md` §6.1). There is no
> `pin_core` field. See [`docs/CORE_AWARE_SCHEDULING.md`](docs/CORE_AWARE_SCHEDULING.md).

> **ABI warning (hard-won).** `SpawnOptions` is a `#[repr(C)]` struct shared
> verbatim with the kernel (`src/syscall/proc.rs`) and `box` (`userspace/box/src/main.rs`).
> Its layout **must** match across all three consumers, and `argv` must be a
> NUL-terminated **pointer array** (`[path\0, arg\0, …, NULL]`) passed as arg2 —
> not a flat null-separated buffer. A past mismatch made the kernel read
> `command.len()` as the options pointer → `EFAULT`, and boxed services silently
> never started. When you add a field, append it at the end and change all three
> copies in lockstep.

---

## Building

```bash
userspace/build.sh --herd-only      # build just herd
userspace/build.sh                  # build all userspace binaries
scripts/populate_disk.sh            # stage binaries onto the ext2 disk image
```

herd depends only on `libakuma` (see `Cargo.toml`).

---

## Roadmap

- **Core-aware scheduling (done, first cut)** — a `core = N` config key runs a service on
  multikernel secondary core N: herd calls `core_init(N, command)` and that core's kernel spawns
  the program locally (ELF fetched via forwarded VFS). One init program per core, no local
  supervision/restart of pinned services. Fully described in
  [`docs/CORE_AWARE_SCHEDULING.md`](docs/CORE_AWARE_SCHEDULING.md).
- Future: multiple services per core (point `core = N` at a per-core `/bin/herd`),
  cross-core supervision/restart, capability-per-service, pinning across nodes/VMs (cluster
  vision), affinity sets. See the *Limitations & future* section of the core-aware doc.
