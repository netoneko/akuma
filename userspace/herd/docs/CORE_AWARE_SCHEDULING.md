# herd core-aware scheduling

**Status:** implemented (2026-06-30) for the single-pinned-program case — `/bin/hello` runs on a
secondary core via `core = 1` in its service config (acceptance/12 Milestone 2). This doc describes
the **userspace/herd** half of the multikernel: how herd places a service onto a specific per-core
kernel.

**Related:** `docs/MULTIKERNEL.md` — the multikernel (esp. **§6.1** the `core_init` activation
handshake + init-program slot, **§10** running `hello` on core 1, **§10.3** capability dispatch).

## The model (what was actually built)

Process creation is **never cross-core** — there is no `SpawnProcess` message and no
`SpawnOptions.pin_core` field (an earlier draft of this doc proposed both; neither was built). A
process runs on a kernel only because *that kernel's own kernel* created it. So herd places a
service on core N by **naming the program in the kernel's `core_init` activation message**, and core
N's own kernel spawns it locally:

```
herd (BSP, EL0)                          kernel                         core N
───────────────                          ──────                         ──────
reads <svc>.conf: core = N
core_init(N, "/bin/<svc>")  ── syscall ─▶ write init_program[N] slot
  (libakuma::syscall 327)                 send MSG_CORE_INIT ──────────▶ wake, scheduler up,
                                          (§6.1; kernel fills the         read slot, fetch ELF via
                                          slot, NOT herd directly)        forwarded open/read, spawn
                                                                          /bin/<svc> LOCALLY
```

The program named can be the **workload directly** (`/bin/hello`) or a **per-core supervisor**
(`/bin/herd`) that then spawns several services on that core with ordinary local syscalls — same
mechanism; the choice is just which path you put in `core = N`'s config.

## Config surface

`parse_service_config` (`userspace/herd/src/main.rs`) has one key for this:

```conf
# /etc/herd/enabled/hello.conf
command = /bin/hello
oneshot = true
core    = 1          # run on secondary core 1; omit or `core = 0` => BSP (current behavior)
```

- `ServiceConfig` has `core: u32` (default `0` = unpinned / BSP).
- `core = 0` (or unset) preserves today's behavior exactly: herd spawns the service locally on the
  BSP via its normal spawn path.
- `core = N` (N > 0): herd does **not** spawn locally. In `start_service` it calls
  `core_init(N, config.command)` (the `core_init` helper → `libakuma::syscall(327, N, path_ptr, …)`).

## Lifecycle of a pinned service (deliberately minimal for the first cut)

A pinned service has **no local pid** — the process lives on core N, its output drains to the
console via core N's ring (§8.2), and its exit is reaped by core N's kernel. herd cannot `waitpid`
a cross-core process, so it treats `core_init` returning 0 as "launched" and:

- **oneshot** (`oneshot = true`): moves the service to the terminal **`Completed`** state (it ran
  once on its core).
- **non-oneshot**: marks it **`Running`** best-effort (not locally supervised). herd does **not**
  restart pinned services in this cut — a restart would need cross-core liveness/reaping it doesn't
  have yet.

On `core_init` failure (core can't be activated / host) the service goes **`Failed`** — no fallback
to another core, no local-BSP fallback. Pinned-or-nothing.

## Boxes and core-pinning are mutually exclusive

A service **cannot** be both boxed and pinned to a non-BSP core. Box/namespace state is
per-kernel-private (`src/syscall/container.rs`, replicated `.bss`) — a BSP box is meaningless to a
secondary's kernel. herd enforces this: if `is_boxed(config) && config.core != 0` it logs an error
and does **not** start the service (same no-fallback rule). "Boxed" = any of `boxed = true`,
`bundle = …`, `join_box = …`, or `box_root != "/"`.

The supported way to get boxes on a subkernel: **name a per-core herd as the init program**
(`core = N`, `command = /bin/herd`). That herd creates boxes via *its own* kernel and supervises its
own services — boxes are always scoped to the kernel that owns them.

## Kernel dependency

The herd side rides the kernel's `core_init(idx, path)` syscall (`nr::CORE_INIT = 327`) and the
`MachineConfig::init_program[idx]` descriptor slot (`docs/MULTIKERNEL.md` §6.1). Both are
implemented; the secondary's local spawn of the named program over forwarded VFS is **R4b
(done)** in `docs/MULTIKERNEL.md` §15. The default boot is herd-managed
(`AUTO_START_HERD && MULTIKERNEL_INIT_HERD`); with those off the kernel auto-activates secondaries
itself for its boot self-tests and herd's `core_init` calls are unnecessary.

## Limitations (current cut) & future

- **One init program per core.** The first pinned service to call `core_init(N, …)` becomes core
  N's init program; a second service pinned to the same N finds it already online and is ignored.
  To run multiple services on one core, point `core = N` at a per-core `/bin/herd`.
- **No supervision/restart** of pinned services (no cross-core reaping yet).
- **Future:** capabilities per service (once the caps map is descriptor data, `docs/MULTIKERNEL.md`
  §16: `core = 1, caps = net`); pinning across nodes/VMs (`core` → `node` under the cluster vision);
  affinity sets / placement policy. Also the per-core-chroot direction (`docs/MULTIKERNEL.md` §16:
  `/srv/cores/<N>` + shared RO `/bin`,`/usr`) would give each pinned program an isolated rootfs
  without a full box.
