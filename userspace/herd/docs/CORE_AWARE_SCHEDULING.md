# herd core-aware scheduling

**Status:** plan / not yet implemented.
**Related:** `docs/MULTIKERNEL.md` (the multikernel; esp. §10 the spawn-on-core-1 scenario,
§10.1/§10.3 capability dispatch). This doc is the **userspace/herd** half: how the service
supervisor places services onto specific cores once the kernel can host a process there.

## Goal

Let a herd service declare *which core* it runs on, so we can schedule processes onto the
multikernel's per-core kernels (the whole point of one-kernel-per-core: dedicate a core to a
workload). A service config gains a `core = N` field; herd asks the kernel to spawn that
service pinned to core N.

## Scope (deliberately small for the first cut)

In:
- A single explicit target core per service: `core = N`. "Core N is enough for now."
- A pre-spawn **availability check**: is core N up and able to host a process?
- If core N is **not** available: **log an error and do not start the service.** Do **NOT**
  fall back to / reschedule on another core. The service simply fails to come up (normal
  herd restart backoff still applies, but it always retries the *same* core N — never a
  different one).
- **Box + non-BSP core is a misconfiguration** → log an error, don't start the service (see
  *Boxes and core-pinning are mutually exclusive* below).

Out (explicitly not in this cut — see *Future*):
- Affinity *sets* / ranges / lists (`core = 1,2` or `core = 2-3`).
- Load balancing, auto-placement, migration, or rebalancing across cores.
- Per-service capability assignment (run on core N with caps `{Net}`) — that rides on the
  caps-as-data work in `docs/MULTIKERNEL.md` §16; cross-referenced under *Future*.
- NUMA / locality heuristics.

## Config surface

`parse_service_config` (`userspace/herd/src/main.rs`) gains one key:

```
# /etc/herd/<svc>.conf
command = /bin/hello
core = 1          # pin to core 1; omit or `core = 0` => BSP (current behavior)
```

- Add `core: Option<u32>` (or `i32` with `-1` = unpinned) to `ServiceConfig` (default: unpinned).
- Parse `"core"` → `config.core = parse_u32(value)`.
- Unset / `core = 0` preserves today's behavior exactly (everything on the BSP).

## ABI change: `SpawnOptions.pin_core`

Core pinning rides the existing `SYSCALL_SPAWN_EXT` (315) path. Add a field to the shared
`SpawnOptions` struct — **and it MUST be changed in lockstep in all three consumers**, or the
struct layout mismatches and the kernel reads garbage (there is already a hard-won comment in
`herd/src/main.rs::spawn_in_box` about exactly this class of ABI-mismatch bug → EFAULT):

1. `src/syscall/proc.rs:1113` — the kernel's `SpawnOptions` (source of truth) + `sys_spawn_ext`.
2. `userspace/box/src/main.rs:33` — `box`'s copy.
3. `userspace/herd/src/main.rs` — herd's copy (in `spawn_in_box`).

Proposed field (append at the end to keep offsets of existing fields stable):

```rust
pub struct SpawnOptions {
    // … existing fields …
    pub box_id: u64,
    /// Target core for execution. 0 = BSP / unpinned (default). Non-zero = pin to that
    /// secondary core's kernel. The kernel validates availability (see below).
    pub pin_core: u64,
}
```

herd sets `pin_core = config.core.unwrap_or(0)` in `spawn_in_box`.

## Availability check + failure semantics

"Available for execution" = core N exists, is `Online`, and can host an EL0 process (i.e. its
per-core runtime is up — see kernel dependency). Two layers, both cheap:

1. **Kernel validates `pin_core` in `sys_spawn_ext`.** If `pin_core` names a core that is not
   online / cannot host a process, return a distinct errno (suggest `-ENXIO` / `-ENODEV`)
   rather than silently spawning on the BSP. This is the authoritative check (herd can't race
   it).
2. **herd logs and fails — no fallback.** On that error herd logs e.g.
   `[herd] service <name>: core <N> unavailable (err <e>) — not started` and leaves the
   service in its failed/stopped state. herd's existing restart backoff may retry, but the
   retry MUST keep `pin_core = N` — it must never try a different core. (Equivalent to: the
   service is pinned-or-nothing.)

Optionally, herd can pre-query online cores to log a clearer message before even attempting
the spawn — but the kernel-side validation is what makes it correct, so the query is just UX.
If a query is wanted, expose online cores via a tiny read-only source (a `/proc`-style file or
a small syscall returning an online-core bitmap); not required for the first cut.

## Boxes and core-pinning are mutually exclusive

A process **cannot** be both in a box *and* pinned to a non-BSP core. Boxes/namespaces are
governed by the kernel instance that *hosts* the process — the container tables are per-kernel
replicated state (`src/syscall/container.rs`, replicated `.bss`). A box created by the BSP
(its `box_id`, its mount/namespace tables) is meaningless to a *secondary* kernel, which has
its own, separate container machinery. So "run this in BSP box B **and** on core 1" is a
contradiction: core 1's kernel can't see BSP box B.

Rule: if a service config sets **both** a box and a non-BSP core, that's a **misconfiguration**
— **log an error and do not bring the service up** (same as an unavailable core: no fallback,
no "just drop the box" or "just drop the pin" guessing). "Boxed" means any of `boxed = true`,
`bundle = …`, `join_box = …`, or `box_root != "/"`; "non-BSP core" means `core` is set to a
value other than 0. Concretely, reject when `is_boxed(config) && config.core.is_some_and(|c| c != 0)`.

Escape hatch (the supported way to get boxes on a subkernel): **run a herd instance inside the
target subkernel.** That herd creates boxes via *its own* kernel (core N's container machinery)
and pins to its own core — so boxes are always scoped to the kernel that owns them. A user who
wants boxes on their multikernel setup runs herd-per-subkernel rather than asking the BSP's herd
to place a boxed service onto another core.

## Kernel-side work + dependency

The herd-side (config key + ABI field + log-and-fail) is buildable **now**, but it only does
something real once the kernel can actually run a process on a secondary:

- **Hard dependency: R4b.3b** in `docs/MULTIKERNEL.md` — *run an EL0 process on a secondary
  core*. The page-table foundation (R4b.3a, `build_secondary_user_kernel_view`) and the
  generic forwarder (R4b.4, `service_forwarded_syscall`) are done; R4b.3b is the EL0
  entry/exit-teardown piece that's still open.
- Until R4b.3b lands, **no secondary can host a process**, so `pin_core != 0` correctly fails
  the availability check and logs the error — which is exactly the intended degenerate
  behavior. So herd's core-awareness can ship and be inert-but-correct ahead of the kernel.
- When R4b.3b is in: `sys_spawn_ext` with `pin_core = N` sends a `SpawnProcess{argv, env, cwd,
  box}` message to core N's kernel over the ring (§10 Part A), which creates the process in
  core N's partition and exec's it — the loader's `open`/`read` forward back to the owner via
  the generic forwarder (§10.3). Console/sockets follow the same dispatch.

## Suggested implementation order

1. **herd + ABI (now):** add `core` config key, `SpawnOptions.pin_core` (3 consumers in
   lockstep), set it in `spawn_in_box`. No behavior change for unpinned services.
2. **Kernel validation (now):** `sys_spawn_ext` rejects `pin_core` it can't honor with a
   distinct errno; herd logs + fails (no fallback). Verifiable immediately: pinning to a
   secondary fails cleanly, pinning to 0 / unpinned works as today.
3. **Real cross-core spawn (after R4b.3b):** `sys_spawn_ext` routes `pin_core = N` to core N
   via a `SpawnProcess` ring message; the §10 Part A path runs the process there.

## Future (out of this cut, noted so the seam is intentional)

- **Capabilities per service.** Once the caps map is data in the descriptor (`docs/MULTIKERNEL.md`
  §16), a service could declare both a core *and* a capability set (`core = 1`, `caps = net`),
  and the placement + caps go together. Pairs with the consensus/cluster-config direction.
- **Pinning across boxes/VMs.** The forwarder is transport-agnostic; "core N" generalizes to
  "node N" (a peer VM) under the cluster vision. `pin_core` may become `pin_node`.
- **Affinity sets / placement policy.** Only worth it once there are enough cores + workloads
  to balance; intentionally excluded now.
