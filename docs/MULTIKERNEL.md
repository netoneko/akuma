# Akuma Multikernel — One Kernel Per Core

**Status:** In progress (2026-06-30), all behind `cfg(kernel_smp)`; the default build is
untouched. **Headline: a real EL0 process (`/bin/hello`) runs PINNED to a secondary core,
activated from userspace by herd — the §10 acceptance demo, Part A (acceptance/12).** Verified
under QEMU `SMP=4 @ 2 GB` (the pinned-process demo at `SMP=2 @ 2 GB`):
- **§11 build gating** — `smp` feature + `release-smp` profile.
- **M0** — secondaries wake via PSCI `CPU_ON` (hvc), report `Online`.
- **M1 isolation** — each secondary runs on its OWN restricted page table (shared RO code +
  descriptor + private stack/PerCpu; peers UNMAPPED). Hardware isolation is **proven**: a
  deliberate cross-core read **faults**. (This is a TTBR0 identity kernel, so isolation uses
  per-core *TTBR0* tables + replicated `.data`/`.bss` + a private PerCpu chunk, not the original
  "via TTBR1". Per-core `pmm`/heap over a real partition = **R2 ✅**; a per-core COOPERATIVE
  scheduler = **R3a ✅**; per-core PREEMPTIVE scheduler (timer-driven) = **R3b ✅**; the cross-core
  syscall-forwarding TRANSPORT = **R4a ✅**; a pinned EL0 process forwarding its exec = **R4b ✅**.)
- **Messaging (M2) + protocol** — per-core heartbeat liveness, a lock-free MPSC inbox ring, and a
  cross-core **SGI doorbell**; secondaries are event-driven (WFI-sleep, wake on per-core timer
  tick or doorbell). The debt-based memory-reclaim protocol (§9) runs the host-tested
  `akuma_smp::CoreStateMachine` over real rings (pressure → debtors repay their creditor →
  receiver zeroes + reclaims). Values are still faked (the demo doesn't yet move real pages) —
  logged only — but the protocol *logic* is the simulator-validated code.
- **Per-core runtime (Approach 2, §15) — R1+R2 DONE:** R1 — each secondary gets a PRIVATE copy of
  the kernel's `.data`/`.bss` at the same VA, so `static PMM`/allocator/`POOL` resolve to its
  own instance (verified: a secondary mutates a shared static into its private copy; the BSP's
  stays pristine). R2 — each secondary now stands up its OWN `pmm`/`allocator` over its RAM
  partition and allocates from it: the BSP carves the secondary's whole bringup image (page
  tables, replicated `.data`/`.bss`, stack, PerCpu) from that partition via a bump allocator
  and identity-maps the partition as 2 MiB blocks; the secondary seeds a private heap + PMM
  there and `alloc`s. The partitions are reserved from the BSP PMM at boot, so the pools are
  strictly disjoint (verified SMP=2 and SMP=4: each core allocs in-partition, BSP pool
  untouched). This unblocks per-core exec/scheduler (R3) and a real pinned process (R4).
- **Per-core console (§8.2) DONE** — a secondary's restricted table doesn't map the UART, so
  `console::print` routes (via one `emit()` chokepoint) to the core's `ConsoleRing` (a 4 KiB-page
  SPSC byte ring in the shared descriptor); a BSP drainer system thread forwards all rings to the
  UART. Verified SMP=2/4: each secondary prints `[core N] …` for itself. Unblocks all secondary
  output for R3.
- **Per-core cooperative scheduler (§15) — R3a DONE:** each secondary registers `akuma_exec`'s
  runtime in its OWN replicated `RUNTIME`/`CONFIG` cells (the BSP sets those *after* the `.data`
  snapshot, so a secondary's copy is pristine — it calls `akuma_exec::init` locally, with the
  scheduler SGI re-targeted at itself and `BOOT_TTBR0_OVERRIDE` = its restricted table so spawned
  threads inherit its address space, not the BSP's), stands up `threading::init`, installs the
  real `exception_vector_table`, and runs two kernel threads that `yield_now` to each other over
  its private scheduler + stacks. Verified SMP=2/4: every secondary reports `R3a: cooperative
  scheduler ✓ (2 threads, 16 yields)` then resumes the M2 heartbeat/doorbell loop intact.
- **Per-core preemptive scheduler (§15) — R3b DONE:** the BSP drives preemption by having its
  `timer_irq_handler` re-arm CNTV then `trigger_sgi(SGI_SCHEDULER)`; a secondary does the same but
  re-targets the SGI at itself. It registers a per-core timer handler (`register_handler_no_gic` —
  `register_handler` would poke core 0's redistributor) in its replicated dispatch table, runs on
  the real `exception_vector_table`, enables PPI 27 in its OWN redistributor, and arms CNTV. Proof:
  one PREEMPTIVE spinner thread that never yields runs (millions of iters) purely because the timer
  preempts the also-never-yielding boot thread. Verified SMP=2/4: `R3b: preemptive scheduler ✓
  (timer preempted)`, then the M2 heartbeat/doorbell loop resumes intact.
- **Cross-core forwarding transport (§8.1/§10) — R4a DONE:** the data-movement round-trip that
  every forwarded syscall rides — and the half §8.1 calls *hard*. A secondary `copyin`s a payload
  into its per-core `FwdBounce` slot (a `[AtomicU8;256]` in the shared descriptor — the sole shared
  byte buffer), pushes a `MSG_FWD_ECHO_REQ` to the BSP inbox, and spins for the reply; the BSP
  services it **from its bringup online-wait loop** (so a secondary blocking on the reply can't
  deadlock the BSP blocking on that secondary's `Online`), transforms the payload (stand-in for the
  real owner-side syscall), and replies. The secondary `copyout`s + verifies (nonce-matched, byte
  transform). Verified SMP=2/4: `R4a: cross-core forward round-trip ✓`. Neither core touched the
  other's partition — only the bounce region. The ring's `ready` Release/Acquire orders the bounce
  bytes. This is the keystone for R4b (loading a process's ELF = forwarding `open`/`read`).
- **Pinned EL0 process (§6.1/§10, M3 Part A) — R4b DONE (SMP=2 @ 2 GB):** `/bin/hello` runs as a real
  EL0 process on **core 1**, activated from userspace. herd reads `core = 1` from its service config and
  calls the `core_init(idx, path)` syscall; the BSP kernel writes the program path into the shared
  `init_program[idx]` slot and sends `MSG_CORE_INIT` (§6.1 — there is NO cross-core spawn). Core 1 wakes,
  stands up its scheduler/role, **fetches the whole ELF via forwarded `openat`/`read`/`close`** to core 0
  (it has no local VFS — "exec is recursive forwarding"), and spawns it **locally** via the normal loader
  (`Process::from_elf`); the per-core kernel-window overlay rides the `prepare_user_address_space` runtime
  hook so the SAME loader builds a correct user table on a secondary. The process runs on core 1's own
  scheduler/partition/pid space; its `write(1)` drains to core 0's UART through the §8.2 console ring; its
  exit is reaped by core 1's kernel. Verified: `[core 1] init: spawned /bin/hello (pid 1, tid 8)` →
  `hello (1/10)…(10/10) hello: done`.

Build with `scripts/build_smp.sh`; boot with `scripts/run_smp.sh` (or `SMP=N cargo run
--profile release-smp --features smp`). Host-test/simulate the protocol with no QEMU:
`cargo test -p akuma-smp --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)`. Remaining: the §10
**Part B** socket case (`curl` on core 1 = add socket arms to the owner-side forwarder, R4b.5), dynamic
memory (M4), and the §16 hardening (caps map as descriptor data, owner-side capability enforcement, BSP
on its own restricted table). Deferred cleanup/tech-debt is tracked in §16.
**Author:** design notes, 2026-06-28
**Scope:** AArch64 (QEMU virt). A shared-nothing, message-passing multikernel where each
physical core boots its own kernel instance ("a container per core"), instead of one
kernel driving N cores under shared locks.

---

## 1. The model: multikernel, not SMP

The goal is **not** traditional symmetric multiprocessing (one kernel, N cores sharing every
data structure). It is a **multikernel** in the Barrelfish sense: each core runs an
independent kernel instance with its **own** scheduler, physical-memory allocator, heap, and
process table. Cores are isolated by construction and coordinate by **passing messages**, not
by sharing mutable state.

Why this matters for cost:

| | Shared-kernel SMP | Multikernel (this design) |
|---|---|---|
| Kernel instances | 1 | N (one per core) |
| Global locks (`POOL`, `PMM`, `TALC`, `THREAD_PID_MAP`, …) | must all become fine-grained / lock-free | **untouched** — each instance keeps its single-core assumptions |
| `RUNNING` thread invariant | must allow N concurrent | stays "one per kernel instance" — trivially true |
| Coordination | shared memory + locks | explicit messages over a ring + IPI |
| Dominant cost | concurrency / race hunting (long tail) | secondary-core bringup + memory partitioning + per-core page tables |

The codebase today has **~35 global spinlocks** and a single global run queue with one
`round_robin_idx` (`crates/akuma-exec/src/threading/mod.rs:2179`). Making those safe under true
parallelism is a multi-week fine-grained-locking refactor with a long tail. The multikernel
model **sidesteps almost all of it**: if each core's `static PMM` resolves to *different
physical memory*, two cores never touch the same allocator, so the lock never contends across
cores. The isolation lives in the **page tables**, not in rewritten code.

### Payoffs the user is after
- **Effortless process pinning** — a process lives on the kernel instance that spawned it, and a
  kernel only ever spawns from its own userspace. So "pinning to core N" is simply *running the
  workload's init system (herd) on core N* — there's no affinity subsystem and no remote-spawn:
  the cross-core control is just `core_init` (activate the core + name its init program). Migration
  ("ship this process to core M") would be an explicit message, but it's far-future, not pinning.
- **Function offload / stripped kernels** — a core's `role` (read from boot config) selects
  which subsystems its image initializes. A compute core can omit the network stack and VFS
  entirely and **forward** those syscalls to the owning core (exactly the rump model,
  generalized to peer kernels).
- **Configurable at init** — roles and memory bounds come from a boot-time descriptor, not
  compile-time constants.

---

## 2. Current state of the codebase

> **Status (2026-06-29):** this section was the *pre-SMP baseline*. The multikernel is now built
> through **R4b.1** (§15) and verified SMP=2/4: secondary bringup, **hardware-enforced per-core
> isolation** (each secondary on its own TTBR0; peers unmapped; a cross-core read faults), per-core
> PMM/heap/scheduler running as steady state, cross-core SGI + lock-free ring messaging, and the
> §8.1 forwarding *transport*. Kept as the baseline that
> motivated the design. The default (non-`smp`) build remains byte-for-byte single-core (§11).

The right things already used per-CPU hardware state, which made the port tractable:

- ✅ **Secondary-core bringup** (now done) — PSCI `CPU_ON` (conduit from the DTB), `MPIDR_EL1`
  aff0 as the core index, a per-core PerCpu data area, secondary trampoline (`src/smp.rs`).
  Originally there was none (`src/boot.rs` set a single `STACK_TOP`).
- ✅ **Cross-core IPI targeting** (now done) — `trigger_sgi_core(aff0, sgi)` rings a specific peer;
  the original `trigger_sgi()` hardcoded the target list to PE0 only.
- ✅ Timer uses per-CPU regs (`CNTV_CVAL_EL0`/`CNTV_CTL_EL0`, `src/timer.rs:30`).
- ✅ GIC redistributor wake is written for "this PE" (`src/gic_v3.rs:152`).
- ✅ Current-thread tracking is per-CPU via `TPIDRRO_EL0`.
- ✅ `pmm::init(ram_base, ram_size, kernel_end)` already takes **explicit bounds**
  (`src/pmm.rs:499`) — per-core PMM is nearly free.
- ✅ A working **kernel-to-kernel message bus already exists**: the rump **sysproxy** RPC over
  a pluggable `Transport` trait (`crates/akuma-rump/src/sysproxy.rs`, driven from
  `src/rump_proxy.rs`). It already solves structured request/response marshaling and
  cross-address-space copyin/copyout. ~70% of the inter-core transport is prototyped.

---

## 3. Architecture

```
                         ┌──────────────────────────────────────────────┐
                         │  Physical RAM (QEMU virt, base 0x4000_0000)    │
                         └──────────────────────────────────────────────┘

   0x4000_0000  ┌─────────────────────────────────────────────────────────────┐
                │ SHARED, read-only-after-init                                  │
                │  • MachineConfig descriptor  (1 page in the pre-kernel gap)   │
                │  • kernel .text / .rodata    (one copy, mapped RO by all)     │
                │  • inter-core message rings  (one inbox ring per core)        │
                ├─────────────────────────────────────────────────────────────┤
   0x4010_0000  │ Core 0 (BSP) PRIVATE partition          role = Bsp           │
   .text base   │  • .data/.bss copy #0  • heap #0  • boot stack #0            │
                │  • PMM bitmap over [p0_base, p0_end)                          │
                │  • owns: virtio-blk, console, DTB                            │
                ├─────────────────────────────────────────────────────────────┤
                │ Core 1 PRIVATE partition                role = Network       │
                │  • .data/.bss copy #1  • heap #1  • boot stack #1            │
                │  • PMM bitmap over [p1_base, p1_end)                          │
                │  • owns: virtio-net, rump/smoltcp stack                      │
                ├─────────────────────────────────────────────────────────────┤
                │ Core 2 PRIVATE partition                role = Compute       │
                │  • .data/.bss copy #2  • heap #2  • boot stack #2            │
                │  • PMM bitmap over [p2_base, p2_end)                          │
                │  • no net / no VFS — forwards those syscalls over messages   │
                ├─────────────────────────────────────────────────────────────┤
                │ Core 3 PRIVATE partition                role = Compute       │
                │  • … (same shape)                                            │
   ram_end      └─────────────────────────────────────────────────────────────┘

   0x80_xx      Device MMIO window (GIC dist, UART, virtio, GICR) — SHARED,
                mapped via SHARED_DEV_L1_PHYS; role gates *which* a core uses.


   Per-core virtual view (why the SAME `static PMM` symbol is private):

        kernel VA space (identical layout on every core)
        ┌───────────────┬──────────────┬─────────────┬──────────────┐
        │ .text/.rodata │  .data/.bss  │    heap     │  boot stack  │
        └───────┬───────┴──────┬───────┴──────┬──────┴──────┬───────┘
                │              │              │             │
   Core 0 TTBR1 │  shared RO   │ → phys #0    │ → phys #0   │ → phys #0
   Core 1 TTBR1 │  shared RO   │ → phys #1    │ → phys #1   │ → phys #1
   Core 2 TTBR1 │  shared RO   │ → phys #2    │ → phys #2   │ → phys #2
                ▲              ▲
        one physical copy   each core's writable sections map to ITS OWN
        mapped by all       physical pages → `static PMM`/`POOL`/`TALC`
                            are private with zero code changes.


   Coordination plane (no shared mutable state — messages only):

        Core 0 ──msg──▶ ring[1] ──SGI doorbell──▶ Core 1 drains, replies
        Core 2 ──"open(/etc/x)"──▶ ring[0] ──▶ Core 0 (owns VFS) ──reply──▶ Core 2
        Core A ──"release pages [x,y)"──▶ ring[B] ──▶ Core B (memory renegotiation)
                  (reuses the rump sysproxy protocol + Transport trait)
```

---

## 4. Memory model

### 4.1 Partitioning (per-core PMM)

The BSP parses the DTB **once** (`detect_memory()`, `src/main.rs:193`) to learn total
`(ram_base, ram_size)`, then carves it into disjoint per-core partitions. Each core initializes
its own allocator over its own slice:

```rust
// Today (single global):
pmm::init(ram_base, ram_size, kernel_end);          // src/pmm.rs:499, called src/main.rs:564

// Multikernel (per core, same BitmapAllocator, different bounds):
pmm::init(cfg.ram_base, cfg.ram_len, cfg.kernel_end);
```

`BitmapAllocator` already manages an arbitrary contiguous range and marks pages below
`kernel_end` used / the rest free. Two independent bitmaps over disjoint ranges are exactly what
"shared-nothing memory" means — and they make a future page *transfer* well-defined (§4.4).

**Partitioning policy (first cut):** disjoint private partitions **plus** one small fixed
**shared region** for the descriptor + message rings + RO kernel text. Keep "shared mutable
memory" tiny and auditable — that is the whole point of going multikernel.

### 4.2 The load-bearing mechanism: replicated kernel writable state

Akuma's kernel globals (`PMM`, `POOL`, `TALC`, `THREAD_PID_MAP`, all ~35 spinlocks) are Rust
`static`s at **fixed virtual addresses** in `.data`/`.bss`. If two cores run the same image and
that VA maps to the same physical page, the cores share the allocator → instant shared-kernel
SMP. Passing a bounds pointer does **not** fix this.

The mechanism that does:

> **Each core gets its own page tables that map the kernel's *writable* sections
> (`.data`, `.bss`, heap, stack) to its own private physical pages — at the same virtual
> addresses.** Read-only `.text`/`.rodata` are physically shared (one copy, mapped RO by all).
> Device MMIO stays shared.

Result: the same `static PMM` symbol resolves to different physical memory per core, **with no
changes to any global-static code.** Isolation happens in the page tables.

BSP, when building core N's image:
1. Map shared `.text`/`.rodata` RO (one physical copy from `KERNEL_PHYS_BASE = 0x4010_0000`).
2. Allocate a fresh `.data`/`.bss` copy from core N's partition; copy the initial `.data`
   image in, zero `.bss`; map at the canonical kernel VA in core N's tables.
3. Allocate per-core heap + boot stack from core N's partition.
4. Build core N's restricted **TTBR0** table covering **only** its partition + shared regions.

> **NOTE (impl reality):** the doc originally said "TTBR1," but Akuma is a **TTBR0
> identity-mapped** kernel — kernel VA == PA, no TTBR1 split — so isolation is realized with
> per-core **TTBR0** tables (`build_isolated_table`), per-core replicated `.data`/`.bss`, and a
> private PerCpu chunk. Substitute TTBR0 for TTBR1 throughout this doc's design sketches.
>
> **Isolation status (achieved for secondaries):** each secondary now runs on its own restricted
> TTBR0 table that maps only shared RO code + the shared descriptor page + its own partition;
> every peer partition is **unmapped**, so a cross-partition access *faults* (proven — §enforcement
> self-test, R1/M1). This is "isolation by hardware," done. The earlier "isolation by convention"
> (all cores share the identity map; behaves only because each PMM hands out its own slice) was the
> M0 staging step and is **superseded**.
>
> ⚠️ **The one remaining asymmetry:** the **BSP** still runs on the original global boot tables
> (`src/boot.rs`, `extend_boot_ram_identity_map`), which identity-map *all* RAM — so the BSP is
> **all-seeing**: it can read/write any secondary's partition (and deliberately does, to read
> PerCpu markers during bringup verification). Isolation is therefore enforced **secondary→peer**,
> not yet **BSP→secondary**. Putting the BSP on its own restricted partition table is future work.

**What is shared vs. private (the precise model):**

| Memory | Who maps it | Access |
|---|---|---|
| RO kernel `.text`/`.rodata`, device MMIO | all cores | shared, read-only (MMIO RW) |
| The one **descriptor page** (`MACHINE_CONFIG`): ring inboxes, `FwdBounce` region, heartbeat counters, console rings, `CoreConfig[]`, `enforcement_results` | all cores | **shared RW** — the *only* mutable memory two cores both touch |
| A secondary's partition (its PMM pages / heap / stack) + its private **PerCpu** page | that secondary (RW); **also the BSP** via its all-seeing identity map | private to the secondary vs. its peers; reachable by the BSP |

So: **a secondary can reach no peer's memory except the shared descriptor page** (rings + bounce +
the small per-core status slots). The BSP is the exception until it too gets a restricted table.

**Invariant — kernels cannot manipulate each other's boxes/namespaces.** Box/container state
(`src/syscall/container.rs`) is per-kernel-private replicated `.bss`, exactly like `PMM`/`POOL`/the
process table — a box created by one kernel is invisible and unreachable to another, and there is no
cross-kernel API to touch a peer's boxes (by design, not convention). Consequences: a process is in
*at most one* kernel's box namespace — the namespace of the kernel that hosts it; a box and a non-BSP
core pin are mutually exclusive (the BSP's box can't follow a process onto a secondary's kernel); and
the way to get boxes on a subkernel is to run a box-creating supervisor (herd) *inside* that subkernel.
See `userspace/herd/docs/CORE_AWARE_SCHEDULING.md`.

---

## 5. The shared config descriptor

A read-only-after-init structure the BSP builds **before** bringing up any secondary. Lives at a
**fixed physical page in the 1 MB pre-kernel gap `0x4000_0000`–`0x4010_0000`** — confirmed unused
at boot (currently just reclaimed to PMM later) and already identity-mapped by the static boot
tables (`L1[1]` covers `0x4000_0000`–`0x7FFF_FFFF` as RAM), so every core can read it with no
extra mapping.

```rust
#[repr(C)]
struct MachineConfig {
    magic: u64,              // sanity-check the secondary read the right page
    version: u32,
    num_cores: u32,
    config_phys_addr: u64,   // self-pointer (re-find / re-map after MMU on)
    shared: SharedRegions,   // mapped by every core
    cores: [CoreConfig; MAX_CORES],
}

#[repr(C)]
struct SharedRegions {
    text_phys: u64,   text_len: u64,    // RO kernel code, one copy
    rodata_phys: u64, rodata_len: u64,
    rings_phys: u64,  rings_len: u64,   // message-ring pool
    dev_mmio_phys: u64,                  // 0x80_xx device window (role-gated use)
}

// Per-capability disposition: the kernel either owns (initializes) the subsystem,
// or proxies it to a peer core, or doesn't have it at all.
#[repr(C)]
enum CapDisposition {
    Own,                     // init a local instance of this subsystem
    Proxy(u32),              // forward these syscalls to the named core (§8.1 / §8.2)
    Absent,                  // unavailable; syscall returns ENOSYS
}

// One slot per capability: Vfs, Net, Console, Block, … (extend as subsystems are split out).
type CapabilityMap = [CapDisposition; CAP_COUNT];

#[repr(C)]
struct CoreConfig {
    mpidr: u64,              // which physical PE this entry describes
    role: CoreRole,          // convenience PRESET that expands into `caps` (Bsp|Network|Compute|…)
    caps: CapabilityMap,     // the authoritative per-capability Own/Proxy/Absent decision
    ram_base: u64,           // this core's PRIVATE physical partition
    ram_len: u64,            // read at RUNTIME, never a compile-time const (enables renegotiation)
    kernel_end: u64,         // pmm::init's "used below here" cut for this core
    data_bss_phys: u64,      // this core's private writable-section copy
    heap_base: u64,
    entry_sp: u64,           // this core's boot stack top
    ttbr1_phys: u64,         // BSP-built per-core kernel page tables
    msg_ring_phys: u64,      // this core's inbox ring (within rings_phys)
    state: AtomicU32,        // Offline -> Booting -> Online (BSP watches this)
}
```

`akuma_exec::ExecConfig` (`src/main.rs:653`) is the existing precedent for threading a config
struct through init — extend that pattern.

---

## 6. Boot & handoff sequence

The existing handoff is already a register pass: `rust_start(dtb_ptr)` receives the DTB in **x0**
from boot.rs assembly (`src/main.rs:146`). Mirror it for secondaries.

```
BSP (core 0)                                  Secondary (core N)
────────────                                  ──────────────────
1. boot.rs: x0 = DTB ptr (QEMU)
2. detect_memory(dtb) -> total RAM
3. carve partitions; build MachineConfig
   at 0x4000_0000; build per-core .data/.bss
   copies + TTBR1 sets + inbox rings
4. for each secondary mpidr:
     PSCI CPU_ON(mpidr,
                 secondary_entry_pa,
                 context_id = &MachineConfig) ─────▶ entry: x0 = context_id
                                                     5. read MPIDR_EL1
                                                     6. find own CoreConfig in MachineConfig
                                                     7. install TTBR1 (cfg.ttbr1_phys); MMU on
                                                     8. sp = cfg.entry_sp
                                                     9. secondary_main(cfg):
                                                          - pmm::init(ram_base, ram_len, kernel_end)
                                                          - allocator/heap init (private)
                                                          - per-core GIC redist + timer
                                                          - scheduler init (private POOL)
                                                          - role-gated subsystem init
                                                     10. cfg.state = Online
11. wait for all state == Online
12. start scheduling / role wiring
```

`context_id` is the standard PSCI mechanism for handing a value to the woken core; it arrives in
x0 just like QEMU hands the BSP its DTB. **Secondaries never parse the DTB** — they only read the
descriptor.

### 6.1 Userspace-driven activation + the init program (the `core_init` handshake)

A secondary doesn't run any workload just by being *online* — it **parks** after its soundness
self-tests and waits to be **activated** by userspace (acceptance/12). The init system (**herd**)
decides which cores to use and **what program each should run**, and tells the kernel via the
`core_init` syscall. The program name rides the activation message through one shared slot —
`MachineConfig::init_program[core]` (`InitProgram`, a fixed path buffer in the shared descriptor).

**Who fills the slot?** The **kernel** does, on the BSP side of the `core_init` syscall — *not*
herd directly. herd is an EL0 process; it cannot touch the descriptor (kernel memory). It only
passes the path *through the syscall*, and the kernel copies it into the slot **before** it sends
`MSG_CORE_INIT`. The ring push/pop (Release/Acquire) is what orders that write before the
secondary's read, so the parked core always sees the path once it pops the message. There is **no
cross-core spawn** — the kernel never injects a process into the secondary; it only names the
program, and the secondary's *own* kernel creates it (§7).

```
 BSP, EL0           BSP kernel (EL1)                         SHARED descriptor            Core 1 kernel (EL1, parked)
 ────────           ───────────────                          (MachineConfig)              ───────────────────────────
 herd
  │ reads hello.conf: core = 1
  │
  │ core_init(1, "/bin/hello")   ── SVC ──▶ sys_core_init(idx=1, path_ptr)
  │  (syscall nr 327)                         │ copy_from_user("/bin/hello")
  │                                           │ smp::core_init(1, b"/bin/hello"):
  │                                           │   ① write the slot ───────────▶ init_program[1].set("/bin/hello")
  │                                           │      (BEFORE the message)         ▲   (the kernel fills it,
  │                                           │                                   │    not herd)
  │                                           │   ② push activation ───────────▶ inboxes[1].push(MSG_CORE_INIT)
  │                                           │   ③ doorbell ─ trigger_sgi_core(1) ─ ─ ─ ─ ─ ─ ─ ─ ─▶ (wakes from WFI)
  │                                           │   log "[SMP] core_init(1): activating … init program: /bin/hello"
  │ ◀── returns 0 ──────────────────────────┘                                             │
  │ herd: "Starting service: hello on core 1"                                              │ pop MSG_CORE_INIT
  │ (no local pid — the process lives on core 1)                                           │ stand up scheduler/role
  │                                                                                        │ ── ONLINE ──
  │                                                                                        │ read the slot ◀──┘
  │                                                                          init_program[1].get() = "/bin/hello"
  │                                                                                        │
  │                                                                                        │ fetch ELF via FORWARDED
  │                            owner side: service_forwarded_syscall  ◀── openat/read/close ─┤ open/read/close to BSP
  │                            (real ext2 read on core 0)             ── ELF bytes ─────────▶│ (core 1 has no VFS)
  │                                                                                        │
  │                                                                                        │ spawn_process_from_image:
  │                                                                                        │   Process::from_elf(bytes)
  │                                                                                        │   → runs LOCALLY on core 1's
  │                                                                                        │     scheduler (pid 1, its own
  │                                                                                        │     per-kernel pid space)
  │                                                                                        │ write(1,"hello\n") → §8.2
  │                                                                                        │   console ring → BSP UART
  │                                                                                        │ exit → reaped by core 1
```

So the slot is the **one-way config channel** for activation: herd → (syscall) → kernel writes
slot → (message) → secondary reads slot → secondary spawns locally. It lives in the descriptor
precisely because that is the only memory both the BSP and the secondary map (§4.2); everything
else about the process — its address space, scheduler, fd table, pid — is private to core 1's
kernel. (`MULTIKERNEL_INIT_HERD`/`AUTO_START_HERD` off ⇒ the BSP fills no slot and auto-activates
cores itself for the boot self-tests; the herd-managed path above is the default.)

---

> **Implementation note (2026-06-29):** the pure, host-testable half of the SMP
> subsystem lives in `crates/akuma-smp` (`no_std`): the lock-free MPSC `Ring`, the
> `MachineConfig` descriptor, `partition()`, and a **sans-IO `CoreStateMachine`** —
> one core's decision logic for multi-core cooperation (today the debt-based reclaim
> protocol of §9; the place that will grow leadership/failover, §12). It is driven by
> `step(Event, &mut emit)`, where `emit` receives `Command`s, so the identical,
> **alloc-free** logic runs in an isolated secondary (no heap mapped) *and* in a host
> simulator. `cargo test -p akuma-smp` exercises the ring under real concurrent
> threads and simulates the protocol across N cores (memory conservation,
> repay-your-creditor-not-the-requester, receiver-zeroing) with zero QEMU. `src/smp.rs`
> is the kernel glue (asm, PSCI, page tables, and the pump that feeds the state machine
> real events and carries out its commands over the rings).

## 7. Message passing (reuse the rump sysproxy)

The inter-core coordination plane reuses the **already-host-tested** sysproxy stack rather than
inventing a protocol:

- `crates/akuma-rump/src/sysproxy.rs` — `Client<T: Transport>`; protocol decoupled from medium.
- Already provides: structured request/response marshaling, **copyin/copyout callbacks** (one
  kernel safely touches another address space), ABI translation, and a cooperative
  "take client → drive transaction → put back" lock (`src/rump_proxy.rs`).

New work = a `CoreTransport` implementing the `Transport` trait over a **shared-memory ring +
SGI doorbell**:
- Each core has an inbox ring in the shared `rings_phys` region.
- Sender enqueues (atomic producer index) and rings the doorbell: `trigger_sgi(target_core, MSG_SGI)`
  — **requires fixing `trigger_sgi()` to target arbitrary affinities** (today hardcoded to PE0).
- Receiver's SGI handler drains the ring and dispatches.

Message types (grow as needed): `CoreInit` (activate a parked core — §6/R4b), `ForwardSyscall`
(compute core → owning core for net/VFS), `SignalDeliver`, `ReleasePages`/`AcceptPages` (§4.4),
`Shutdown`.

> **There is deliberately no `SpawnProcess` message.** Process creation is never cross-core: a
> process exists on a kernel only because *that kernel's own userspace* issued the spawn syscall.
> The only cross-core "put work on core N" control is `CoreInit`, which activates a parked core and
> tells it which **init program** to bootstrap; that core's kernel then spawns it **locally** (its
> ELF fetched via forwarded VFS), and from there the init process (herd) spawns everything else on
> that core with ordinary local syscalls. So no kernel ever injects a process into another — which
> is what shared-nothing requires. (Likewise a process's `exit` is reaped by its parent on the same
> kernel — no cross-core `ChildExit`.) See §10.

**Reply routing / waiter table.** Every request that parks a thread needs its reply routed back
to *that* thread, so each core keeps a small table keyed by **correlation id** → `(thread id,
…)` for its outstanding requests; the owner echoes the correlation id in the reply, and the
SGI handler wakes the matching waiter. This table is **mandatory** — message passing does not
exist without it. Consequently the "async fd-readiness: push vs. poll" question is already
settled in favor of **push**: an async event (e.g. core 0's stack completing a blocking
`recvfrom`) is just another message carrying the waiter's correlation id into the **same**
table. Polling would be *additional* machinery; event-driven wake reuses what reply routing
already requires. The owner therefore tracks, per blocking fd op, which remote `(core, thread,
correlation id)` is waiting — which it must record anyway to send any reply at all.

---

## 8. Device ownership & roles

Devices are singletons; assign each to a core by `role`. Others reach it by forwarding a syscall
to the owner.

| Device | Owner (example) | Others |
|---|---|---|
| virtio-blk / ext2 VFS | BSP (Storage) | forward `open/read/write` |
| virtio-net + TCP/IP (smoltcp or rump) | Network core | forward socket syscalls (already the rump pattern) |
| console / UART | BSP | forward writes |
| GIC distributor | shared (global), per-core redistributor private | — |

Ownership is decided **per capability**, not per fixed role. Each core's `caps` map
(§5) tells it, for every subsystem independently, to `Own` it (init a local instance),
`Proxy(core)` it (install a forwarding stub → §8.1/§8.2), or treat it as `Absent` (return
`ENOSYS`). A `role` is just a named preset that expands into a `caps` map; the map is
authoritative. This is strictly more flexible than a role enum — e.g. a core can `Own` its VFS
but `Proxy` networking, or two cores can each own a separate NIC. A "stripped" kernel is simply
one whose `caps` are mostly `Proxy`/`Absent`: it skips initializing those subsystems and installs
forwarding stubs instead.

### 8.1 I/O forwarding — how the *bytes* cross cores

Routing a syscall to its owner is the easy half. The hard half (flagged as the open question)
is moving the **data**: a user buffer (e.g. curl's send buffer) is a physical page in the
caller's partition, mapped in the caller's address space. Shared-nothing means the owner core
must **never** touch it directly. The answer is the rump sysproxy's `copyin/copyout` split, with
the "callback" realized as a message round-trip:

- **Inbound** (`write`, `sendto`, …): the *forwarding core* does the `copyin` on its own side
  (user buffer → message payload) and ships the bytes in the message. The owner operates only on
  the payload.
- **Outbound** (`read`, `recvfrom`, …): the *owner* puts result bytes in the reply payload; the
  forwarding core does the `copyout` on its own side (payload → user buffer).
- **Each core only ever touches its own process memory.** The sole shared memory is the ring.
- **Bulk:** transfers larger than a ring slot are chunked in a loop, or carried through a shared
  **bounce region** in `rings_phys` (message passes an offset) to avoid copy amplification.
- **Blocking:** the calling thread parks (its core's scheduler runs others) until the reply
  doorbell SGI wakes it — identical to any blocking syscall, only the wake source is a message
  instead of a device IRQ.

This is exactly how `crates/akuma-rump` already moves socket data between the Akuma kernel and an
in-box `rump_server` today — the core-to-core case just swaps the transport.

### 8.2 Async output: per-core console append ring

Synchronous forwarding (§8.1) is correct when a syscall needs a **result** (`read` returns
bytes + count, `connect` returns a status). It is the **wrong** fit for serial/console output:
it is high-frequency, latency-tolerant, and produces no meaningful return value, so a
round-trip + doorbell *per write* would dominate. Console output uses an **asynchronous
append-buffer** instead — the fire-and-forget counterpart to §8.1.

- Each core has its own **SPSC console ring** in the shared `rings_phys` region. Producer = that
  core's kernel (the `write(1/2, …)` and `console::print` paths). Consumer = the **console-owner
  core** (core 0).
- A non-owner core's write **appends to its ring and returns immediately** (return the full byte
  count — a tty write never guaranteed synchronous delivery anyway). No SGI, no blocking, no
  round-trip on the hot path.
- The owner **drains all cores' rings** on its timer tick / idle loop. A single *coalesced*
  doorbell (one SGI when the owner is halted/idle, gated by a "dirty" flag) prevents output from
  being stuck behind an idle owner — but it is one SGI per drain batch, not per byte.
- **Backpressure:** if a ring fills (owner draining too slowly), the producer yields and retries
  (preserves log integrity) or drops with a surfaced `[N bytes dropped]` marker. Default: yield.
- **Ordering:** FIFO within a core. Cross-core interleave is best-effort; drain per-core in
  line-sized chunks, and optionally tag lines with the core id for debugging.

The same append-ring pattern generalizes to any output-only, no-reply sink (kernel logging,
metrics). Inbound console (keyboard/stdin) — if needed — stays synchronous (§8.1), since the
reader blocks for input.

**DONE (2026-06-29).** Implemented as `akuma_smp::ConsoleRing` (a host-tested SPSC byte ring,
`CONSOLE_RING_CAP` = one 4 KiB page per core) in `MachineConfig::console_rings[MAX_CORES]`. The
kernel `console::print`/`print_dec`/… all funnel through one `emit()` chokepoint; on a secondary
whose per-core ring is set (`smp::set_console_ring`, a replicated `.bss` static so each core sets
only its own), `emit` appends to that ring and returns — the UART is unmapped in the secondary's
restricted table. A BSP **drainer system thread** (`smp::start_console_drainer`, spawned from
`run_async_main` once preemption is live — like the SSH server) reads each ring a page at a time
and writes the UART. Why this shape and not "just messages on the inbox": console is high-volume,
so a 16-byte `Msg` per write would be tiny and would flood the low-rate control inbox; a dedicated
byte ring **is** the batched form of "send console to the owner kernel," and is the seam to later
move the console to a userspace server. Verified SMP=2/4: each secondary prints `[core N] …` for
itself (buffered in its ring from bringup, flushed when the drainer starts); full boot to SSH
intact. NB: the producer drops on a full ring (no backpressure-yield yet) and the drainer drains
every scheduler quantum (no coalesced-doorbell throttle yet) — both noted future tweaks.

---

## 9. Dynamic memory renegotiation (later)

Two independent PMM bitmaps over disjoint ranges make a transfer well-defined. The protocol rides
the message bus:

```
Core A (releasing [x,y)):                  Core B (receiving):
  1. unmap [x,y) from A's TTBR1
  2. free [x,y) in A's bitmap
  3. flush TLB / cache for [x,y)   ──msg ReleasePages[x,y)──▶
                                             4. map [x,y) in B's TTBR1
                                             5. mark used in B's bitmap
                                  ◀──msg AcceptPages────────  6. ack
```

The only genuinely hard part is the **unmap-flush-before-remap ordering** (coherence), and its
surface is small. The one discipline to keep **now**: `ram_base`/`ram_len` are read at runtime
from the descriptor, never baked into compile-time constants — then renegotiation is a
message-protocol addition, not a format change.

---

## 10. Verification scenario — `hello` + `curl` pinned to core 1

The chosen end-to-end test, because it exercises forwarding in **both directions** plus the
data-movement path in one playbook. This is the **Phase 0 capability split**: **core 0**
`Own`s VFS + networking + console; **core 1** is a stripped compute kernel that
`Proxy`s all three to core 0 (its `caps`: `Vfs=Proxy(0)`, `Net=Proxy(0)`, `Console=Proxy(0)`).
The first split deliberately proxies **both networking and VFS** — the heaviest, most-exercised
subsystems — so the forwarding path is validated under real load before any capability is
split to `Own` on a secondary.

> **Process creation is never cross-core.** A process exists on a kernel only because *that
> kernel's own kernel* created it. So there is **no `SpawnProcess` message** and the BSP never
> injects a process into core 1. The BSP-side herd instead **names the program** core 1 should run
> as its first process, in the `core_init(idx, path)` activation handshake (§6.1 — the path rides
> the `init_program` descriptor slot; the kernel fills it, not herd directly). Core 1's *own*
> kernel then creates that process **locally** — its ELF fetched via forwarded VFS. The named
> program can be the workload directly (the demo names `/bin/hello`) **or** a per-core supervisor
> (`/bin/herd`) that in turn spawns several services with ordinary local syscalls — same mechanism,
> the choice is just which path you put in the service's `core = N` config. Placement therefore
> lives in the BSP herd's config (`core = N`) + the per-core kernel, never a BSP→core spawn route.
> See **`userspace/herd/docs/CORE_AWARE_SCHEDULING.md`**.

Key insight: **exec is recursive forwarding.** Core 1 has no VFS, so loading any binary
(`/bin/hello`, or `/bin/herd`) forces it to forward `open`/`read` back to core 0 to fetch the ELF
bytes — the spawn itself is a *local* core-1 syscall; only the file reads cross. So `hello` alone
already tests local-spawn-on-a-secondary, VFS-read-forward (1→0), and console output (1→0 async).
`curl` then adds socket-forward (1→0). This is the full matrix.

```
Part A — run /bin/hello on core 1 (DONE; spawned LOCALLY by core 1's own kernel)
─────────────────────────────────────────────────────────────────────────────────
(prereq)       : BSP herd calls core_init(1, "/bin/hello"); the BSP kernel writes the
                 init_program[1] slot + sends MSG_CORE_INIT (§6.1). NO cross-core spawn.
core 1 kernel  : (on activation) read init_program[1] = "/bin/hello"; begin local spawn
                 loader open("/bin/hello")   ─ VFS not owned ─
                       ──ForwardSyscall{open}── ring[0] ─▶ core 0 (ext2)  ──fd──▶
                       ──ForwardSyscall{read}── ring[0] ─▶ core 0  ──bytes (§8.1 outbound)──▶
                 map ELF segments into core1 pages; build stack; schedule
hello (core 1) : write(1, "hello\n")  ─ console not owned ─
                       append to core1 console ring (§8.2); return immediately
core 0         : drains core1 console ring on tick ─▶ "hello\n" on serial
hello (core 1) : exit → reaped LOCALLY by core 1's kernel — no cross-core ChildExit

Part B — curl networking on core 1
──────────────────────────────────
curl (core 1)  : socket(AF_INET,…)   ──Forward── ring[0] ─▶ core 0 (smoltcp/rump) ──fd──▶
                 connect(fd, addr)    ──Forward (addr copied in, §8.1 inbound)── ─▶ core 0 ──result──▶
                 sendto(fd, buf, …)   ──Forward (buf → payload/bounce, chunked)── ─▶ core 0 TX
                 recvfrom(fd, buf, …) ──Forward── ─▶ core 0 RX ──bytes ride reply──▶ copyout to curl
                 (stdout of curl → core1 console ring → drained by core 0, as Part A)
```

**Pass criteria:**
1. `hello` runs entirely on core 1 (its CPU, its partition memory, its scheduler) yet its ELF
   came from core 0's ext2 and its output reached core 0's serial.
2. `curl` completes a real HTTP(S) fetch with every socket syscall serviced by core 0's stack.
3. Crucially — **it works with each core's kernel statics physically isolated** (per-core
   `.data`/`.bss`, §4.2). If `PMM`/`POOL`/`TALC` were accidentally shared, this would deadlock or
   corrupt; passing validates the shared-nothing model itself.

**Dependencies:** M0–M2, plus **VFS-read forwarding as the first forwarding target** (exec needs
it before anything else can run). Console ring (§8.2) before output is visible. This is the M3
acceptance test. **Part A (criterion 1) is DONE** — `/bin/hello` runs on core 1, ELF fetched from
core 0's ext2, output on core 0's serial, with statics physically isolated (criterion 3). Part B
(criterion 2, `curl`) is the remaining socket-forward work.

### 10.1 Per-syscall decision tree

When the process pinned to core 1 issues a syscall, core 1's syscall entry classifies it
**before** doing any work. Most syscalls never leave the core; only the ones that touch a
`Proxy`'d capability cross to the owner. The classification is per-syscall, driven by the `caps`
map (§5) — *not* a blanket "secondary forwards everything."

```
                    syscall N on core 1  (EL0 ─SVC─▶ core1 kernel, per-core vectors)
                              │
            ┌─────────────────┴─────────────────┐
            │ Does N touch an OS capability?     │
            │ (VFS / Net / Console / …)          │
            └─────────────────┬──────────────────┘
                 no │                       │ yes
        ┌───────────▼──────────┐            │
        │ LOCAL — handle on     │     ┌──────▼─────────────────────────┐
        │ core 1 with its OWN   │     │ cap = caps[subsystem of N]     │
        │ replicated state.     │     └──────┬─────────────────────────┘
        │ mmap(anon)/brk/mremap │   Own │  Absent │           │ Proxy(owner)
        │ getpid/gettid/clone   │       │         │           │
        │ futex/nanosleep/sched │  ┌────▼───┐ ┌───▼──────┐    │
        │ thread create/exit    │  │ handle │ │ return   │    │
        │ signals, time, tls    │  │ locally│ │ -ENOSYS  │    │
        │ → no cross-core msg   │  │(Own=0) │ │(stripped)│    │
        └───────────────────────┘  └────────┘ └──────────┘    │
                                                                │
                          ┌─────────────────────────────────────┘
                          │ Does N return meaningful data / status the caller waits on?
                          └───────────────┬───────────────────────┬───────────────────┐
                       no, output-only    │                       │ yes (read/open/    │
                       (write(1/2,…),      │                       │  connect/recvfrom/ │
                       console, logging)   │                       │  sendto status, …) │
                          ┌────────────────▼─────────┐   ┌─────────▼──────────────────────────────┐
                          │ ASYNC append (§8.2)       │   │ SYNC forward (§8.1)                     │
                          │ • copyin bytes → core1's  │   │ 1. copyin inbound user buffers          │
                          │   per-core CONSOLE RING   │   │    (path, sockaddr, write data) into    │
                          │ • return byte count NOW   │   │    msg payload / bounce region          │
                          │ • NO doorbell, NO block   │   │    — core1 touches ONLY its own memory  │
                          │ • owner drains ring on its│   │ 2. push ForwardSyscall{nr,args,off,len} │
                          │   tick (coalesced SGI)    │   │    onto ring[owner]; ring DOORBELL SGI   │
                          └───────────────────────────┘   │ 3. PARK caller; core1 scheduler runs    │
                                                           │    other threads (R3b preemptive)       │
                                                           │ 4. owner executes real syscall, writes  │
                                                           │    result + outbound bytes to reply     │
                                                           │ 5. owner pushes Reply; doorbells core1  │
                                                           │ 6. core1 reply handler WAKES caller;    │
                                                           │    copyout outbound bytes → user buf;   │
                                                           │    return result/errno to EL0           │
                                                           └─────────────────────────────────────────┘
```

Notes that make the tree exact:
- **Local-first is the common case.** A pinned compute process spends most syscalls on memory,
  threads, futexes, and time — all `Own`ed locally because R1–R3 gave core 1 its own
  `PMM`/`TALC`/`POOL`/scheduler. Forwarding is the exception, taken only on a `Proxy` capability.
- **`exec` is recursive forwarding** (§10): loading the ELF is just `open`+`read` on a `Proxy`'d
  VFS, so it walks the right-hand SYNC branch before the process's first instruction runs.
- **Direction decides which side copies** (§8.1): inbound args are `copyin`'d by core 1 *before*
  the message; outbound bytes are `copyout`'d by core 1 *after* the reply. The owner only ever
  touches the message payload — never a peer's user page.
- **Async vs sync is about whether a result is awaited**, not about which subsystem: `write` to a
  pipe/file is SYNC (returns a count the caller may depend on); `write` to the console/stdout is
  the ASYNC special-case because a tty write was never a synchronous-delivery guarantee.

### 10.2 End-to-end data flow of one forwarded syscall (`read`, the bulk/outbound case)

The worked lifecycle of a single blocking forward — every arrow is either a CPU-local action or
the **one** shared medium (the ring + bounce region in `rings_phys`). Time flows downward; the two
columns are physically isolated cores that share nothing but the ring.

```
  core 1  (curl/hello — Proxy(VFS)=core0)                 core 0  (owns ext2 / smoltcp / UART)
  ───────────────────────────────────────                ─────────────────────────────────────
  EL0:  read(fd, ubuf, n)
     │  SVC ─▶ core1 kernel syscall entry
     │  classify: VFS = Proxy(0), result awaited ⇒ SYNC
     │  reserve bounce slot off..off+n in rings_phys
     │  build msg = ForwardSyscall{nr=read, fd, off, len=n}
     │  push msg ─▶ ring[0].inbox ───────────────────────▶  (lock-free MPSC enqueue)
     │  trigger_sgi_core(aff0=0, DOORBELL) ──────────────▶  doorbell IRQ on core 0
     │  park caller thread (state=WAITING on fd-reply);
     │  scheduler runs core1's OTHER threads  ░░░busy░░░       core0 IRQ: drain ring[0]
     │                                          ░░░          pop ForwardSyscall{read}
     │                                          ░░░          do REAL read on ext2 into a
     │                                          ░░░          core0 kbuf  (core0's own memory)
     │                                          ░░░          copy kbuf ─▶ bounce[off..off+ret]
     │                                          ░░░          push Reply{ret, off, len=ret} ─▶ ring[1]
     │   doorbell IRQ on core 1  ◀──────────────────────────  trigger_sgi_core(aff0=1, DOORBELL)
     │  core1 reply handler: pop Reply{ret,off,len}
     │  copyout bounce[off..off+ret] ─▶ ubuf      (core1's own user page)
     │  mark caller READY; scheduler resumes it
     │  release bounce slot
  EL0:  read returns ret  ◀── caller wakes with bytes in ubuf
```

Key invariants this picture encodes:
- **Shared-nothing holds at the data layer, not just control.** The bytes live in `ubuf` (core1
  memory) and `kbuf` (core0 memory); they meet only in the **bounce region**, the single piece of
  shared RAM either side may touch. Neither core dereferences a pointer into the other's partition.
- **Blocking is ordinary.** The caller parks exactly like any I/O-blocked thread; the only
  difference from a single-core kernel is the wake source — a peer's doorbell SGI instead of a
  device IRQ. While it waits, core 1's (R3b) preemptive scheduler keeps its other threads running.
- **Small args ride the message; bulk rides the bounce region.** A `ForwardSyscall` slot carries
  the syscall number + scalar args + a `(offset,len)` into the bounce region; `read`/`write`/`send`
  payloads that exceed a slot are chunked or pointed at the bounce region to avoid copy
  amplification (§8.1 "Bulk").
- **Inbound mirror.** `write`/`sendto` are the same diagram reversed: core 1 `copyin`s the user
  buffer into the bounce region *before* the message, and core 0 consumes it directly — so each
  core still touches only its own user memory.

### 10.3 Capabilities dispatch flow (as implemented)

Every syscall a pinned process issues is classified **by the capability it touches** *before* any
work, and that single classification decides who runs it. This is the generic forwarder: there is
one forwarded-syscall message type, not one per syscall.

```
                 EL0 syscall N on a compute core (e.g. core 1)
                 EL0 ──SVC──▶ that core's kernel (per-core vectors)
                                    │
                       ┌────────────▼─────────────┐
                       │ capability_of(N)          │  classify by syscall number
                       └────────────┬─────────────┘
          ┌─────────────────────────┼───────────────────────────┐
          │ Vfs / Net               │ (everything else)          │ Console
          │ (forwardable)           │ Threading/Memory/Time/…     │ write(1/2,…)
          ▼                         ▼                             ▼
  ┌───────────────────┐   ┌──────────────────────┐   ┌──────────────────────────┐
  │ caps map (§5):    │   │ LOCAL — handle on     │   │ ASYNC append (§8.2):     │
  │ owner of this cap │   │ THIS core's kernel    │   │ copy bytes → per-core    │
  └───────┬───────────┘   │ over its replicated   │   │ CONSOLE RING, return the │
   Own=me │  Proxy(owner) │ PMM/POOL/proc table.  │   │ count NOW. No ack, no    │
          │      │        │ getpid/clone/futex/   │   │ doorbell, no round-trip  │
   ┌──────▼───┐  │        │ mmap/nanosleep/…      │   │ (fire-and-forget — an    │
   │ handle   │  │        │ → NO cross-core msg   │   │ ack would defeat the     │
   │ locally  │  │        └──────────────────────┘   │ point; §10.3 note).      │
   └──────────┘  │                                    └──────────────────────────┘
        ┌────────▼──────────────────────────────────┐
        │ FORWARD (generic, §8.1): publish {nr,args} │
        │ to fwd_call[me] + pointer bytes to         │
        │ fwd_bounce[me]; ring owner; PARK caller.   │
        │ Owner runs the REAL syscall on its own     │
        │ resources, copies outbound bytes back,     │
        │ replies ret; caller wakes, copyout, return.│
        └────────────────────────────────────────────┘
```

In code (`src/smp.rs`):
- **`capability_of(nr) -> {Vfs, Net, Local}`** — classifies the syscall number (using the kernel's
  canonical `syscall::nr::*` constants, never magic numbers). Only **Vfs** and **Net** are
  *forwardable*; everything else is **Local**.
- **`capability_owner(self_idx, nr) -> Option<owner>`** — consults the caps map (Phase-0: core 0
  owns Vfs + Net; other cores `Proxy` them). `Some(owner)` ⇒ forward; `None` ⇒ run locally (the cap
  is `Local`, **or** this core `Own`s it). An `Absent` capability returns `-ENOSYS`.
- **Threading/memory/time/signals fall straight through to the local kernel** — they resolve against
  this core's replicated `PMM`/`POOL`/process table, so a pinned process spends *most* syscalls
  local; forwarding is the exception taken only on a `Proxy`'d cap.
- **Console is NOT in this map.** tty output is the §8.2 async append-ring: fire-and-forget, **no
  drain acknowledgement** — the producer returns the byte count immediately and never learns when
  the owner drained it. An ack would reintroduce the per-write round-trip the ring exists to avoid,
  so by design there is none. `write(1/2,…)` therefore never takes the synchronous forward path.
- **The owner side is one generic dispatcher** (`service_forwarded_syscall`): a match on the syscall
  number that runs the real op against core 0's resources. open/read/close are implemented today
  (the exec-fetch path); `write` and the socket set (`socket`/`connect`/`sendto`/`recvfrom`) slot in
  as additional arms with **no new message type** — which is why `curl` is "add arms," not new
  machinery. The classify-and-route above is the *requester's* advisory choice; the owner must also
  **enforce** the boundary — reject a forwarded syscall whose capability the requester wasn't granted
  to `Proxy` here (and reject `Local`-group syscalls outright). That guard is what makes capability
  subtraction real; it's a tracked follow-up (§16) gated on the caps map being data.

---

## 11. Build profile & gating — `release-smp` (default-off)

All multikernel code is gated so the **default build stays byte-for-byte single-core** until
the model is proven. This mirrors the existing `extreme` mechanism (`build.rs` can't tell two
non-`opt-level=z` profiles apart via `OPT_LEVEL`, so a **Cargo feature** is the real
discriminator):

- **Feature `smp`** — *not* in `[features].default`. `build.rs` reads `CARGO_FEATURE_SMP` and
  emits `cfg(kernel_smp)` (register it via `cargo::rustc-check-cfg=cfg(kernel_smp)` like the
  existing profile cfgs).
- **Profile `release-smp`** — `inherits = "release"` in `Cargo.toml`; carries any SMP-specific
  codegen knobs later. The profile sets codegen; the **feature** gates code. They are selected
  together (Cargo profiles cannot auto-enable features), via a helper:
  `scripts/build_smp.sh` → `cargo build --profile release-smp --features smp`
  (paralleling `build_extreme_size.sh`).
- **All multikernel code lives behind `#[cfg(kernel_smp)]`** — secondary bringup, per-core page
  tables, the message bus, forwarding stubs, the descriptor. With the feature off, none of it
  compiles; `cargo build --release` is unchanged and the single-core path is the only path.
- **Off by default until ready to roll** — flip nothing in `default`; SMP is opt-in per build
  until the §10 acceptance test passes reproducibly.

> Open choice for when it lands: whether QEMU `-smp N` in the runner is gated behind the same
> profile (so default runs stay single-CPU) — almost certainly yes; set it in `build_smp.sh` /
> the runner rather than the default `scripts/cargo_runner.sh`.

---

## 12. Phased milestones

All milestones below are built under `--features smp` (§11); the default build is untouched
throughout. The **Phase 0 capability split** (net + VFS + console all `Proxy(0)`, §10) is the
target configuration realized at **M3**; M0–M2 are the bringup/isolation/transport prerequisites.

| Milestone | Goal | Key work |
|---|---|---|
| **M0 — second core spins** ✅ | Core 1 wakes, reads descriptor, parks in a `wfe` loop; BSP sees `state = Online` | PSCI `CPU_ON` (conduit from DTB `/psci`), secondary trampoline (`src/smp.rs`), MPIDR-aff0 core index, descriptor as an identity-mapped `static` (VA==PA → no pre-kernel-gap page needed at M0), x0=`context_id` handoff. Isolation-by-convention (secondaries reuse the BSP boot page tables). |
| **M1 — isolated second kernel** ✅ | Each secondary runs on its OWN restricted page table (shared RO code + descriptor page + private stack/PerCpu; peers UNMAPPED) and **hardware-enforced isolation is PROVEN** (a deliberate cross-core read faults). NOTE: this codebase is a TTBR0 identity-mapped kernel, so M1 is realized by per-core *TTBR0* tables + a per-core `[PerCpu]` private chunk (not the doc's original "replicated .data/.bss via TTBR1" — see §4.2 note). The per-core `pmm::init`/heap/scheduler over a real partition that "full M1" wanted is **done in R1–R3** (§15). Caveat: isolation is enforced secondary→peer; the **BSP** is still all-seeing (its boot tables map all RAM) — see §4.2. | per-core restricted TTBR0 tables (`build_isolated_table`), `secondary_enter_isolated` (TTBR0+SP switch, **`isb` after `msr ttbr0` is mandatory** or the global 1 GB boot block survives in the TLB and isolation leaks), per-core VBAR enforcement self-test |
| **M2 — ping-pong** ✅ | Cores exchange messages over a lock-free MPSC `Ring` (non-blocking); the **cross-core SGI doorbell works** (`trigger_sgi_core(aff0, sgi)` targets a specific peer; each secondary brings up its own GIC receive path — CPU iface + its GICR frames mapped device + SGI/timer enabled — and an IRQ vector); and the secondary loop is now **event-driven**: it sleeps in **`WFI`** and is woken by its **per-core virtual timer** (the heartbeat tick, so liveness advances while parked) or a **doorbell SGI** (a peer rang it). No polling. (Gotcha: `WFE` returns spuriously — `WFI` is the sleep-until-interrupt primitive.) | MPSC `Ring` ✅, `trigger_sgi_core` ✅, secondary GIC receive + IRQ handler ✅, per-core CNTV timer + `WFI` ✅ |
| **M3 — roles + forwarding** | The §10 acceptance test: `hello` + `curl` running on core 1 (spawned locally by core 1's own kernel) | **Part A ✅** — `/bin/hello` runs pinned on core 1: `core_init(idx, path)` activation (§6.1 — NOT a spawn message; the program path rides the `init_program` descriptor slot), VFS-read forwarding (the loader fetches the ELF over the R4a transport), local spawn via the normal loader + the `prepare_user_address_space` overlay hook, console append ring (§8.2) for its output, spawn/exit local to core 1's kernel. **Part B (TODO)** — `curl`: add socket arms (`socket`/`connect`/`sendto`/`recvfrom`) to the owner-side forwarder. |
| **M4 — dynamic memory** | Core A releases pages to Core B at runtime | `ReleasePages`/`AcceptPages` messages, unmap-flush-remap handshake |

### Beyond M4 — fault tolerance & leadership (far off, not scheduled)

A long-horizon goal is **reloading an unresponsive kernel on the fly**: detect a core that has
stopped servicing messages (heartbeat / liveness timeout on its inbox ring), then tear it down,
re-image its partition, and `CPU_ON` it again — without rebooting the machine. That requires:

- **Liveness detection** — per-core heartbeat (e.g. a monotonically advancing counter in the
  descriptor, or periodic ping/pong on the ring) so peers can tell "slow" from "dead."
- **A leadership/consensus mechanism** — to decide *which* core authoritatively declares another
  dead and drives its reload, and to avoid split-brain (two cores both trying to reload a third,
  or a wrongly-accused core that was merely slow). A small ring-based agreement protocol among
  the live cores; the BSP is a natural default coordinator but must itself be replaceable.
- **Capability fail-over** — if the dead core `Own`ed a capability (e.g. networking), peers
  proxying to it must re-point their `caps` to a survivor or block until reload completes.
- **State recovery** — what survives a reload? A re-imaged kernel loses its in-RAM process
  table; processes it hosted are lost unless checkpointed. Likely scope: reload *infrastructure*
  cores (net/storage owners) whose state is reconstructible, not arbitrary compute cores.

**Implication for the present design (cheap to honor now):** keep the descriptor's `caps` map
**re-pointable at runtime** (already true — it's read, not compiled in), and route *all* cross-
core dependencies through the message bus + waiter table (§7) rather than any shared pointer, so
a peer's disappearance is an observable timeout rather than a hang. Nothing else here needs to be
built early; this note exists only to avoid foreclosing it.

---

## 13. Risks & open questions

- **Don't drift into accidental shared-kernel SMP.** The moment two cores share one global static
  (same VA → same physical page), you inherit the full lock refactor. The per-core-TTBR1
  discipline (§4.2) is what prevents this; guard it carefully.
- **Per-core TTBR1 vs. shared boot tables** — staging "isolation by convention" first
  (everyone maps all RAM) is fine for M0–M2 but is *not* real isolation; M1 should aim for
  per-core tables.
- **`akuma_exec` scheduler is shared code with global statics** — confirm that running it as
  per-core *instances* (private `.bss`) needs no source changes beyond the page-table split.
  The shared run queue / single `round_robin_idx` become per-core automatically *only if* their
  statics are physically replicated.
- **Cache/TLB coherence is scoped to shared regions only** (rings, descriptor) — keep that set
  tiny. No general TLB-shootdown protocol needed in the shared-nothing core path.
- **Fixed vs. negotiated partition sizes at first boot** — start fixed (equal or role-weighted);
  negotiation is M4.
- **MAX_CORES on QEMU virt** — set `-smp N` in the runner; confirm DTB enumerates the CPU nodes
  the BSP will `CPU_ON`.

---

## 14. Key file references

| Concern | Location |
|---|---|
| SMP kernel glue (bringup, trampoline, page tables, pump) | `src/smp.rs` |
| SMP pure logic (ring, descriptor, partition, `CoreStateMachine`) | `crates/akuma-smp/` |
| Boot assembly entry, x0 handoff | `src/boot.rs:61`, `src/main.rs:146` |
| Memory detection (DTB) | `src/main.rs:193` |
| Memory layout computation | `src/main.rs:270` |
| PMM init (already takes bounds) | `src/pmm.rs:499`, call site `src/main.rs:564` |
| Boot page tables / identity map | `src/boot.rs:148`, `crates/akuma-exec/src/mmu/mod.rs:54` |
| `get_boot_ttbr0` | `crates/akuma-exec/src/mmu/mod.rs:236` |
| GIC redist + SGI; cross-core doorbell | `src/gic_v3.rs:152`, `trigger_sgi_core` in `src/gic_v3.rs` |
| Per-CPU timer | `src/timer.rs:30` |
| Scheduler / global `POOL` | `crates/akuma-exec/src/threading/mod.rs:2179` |
| Process table / global statics | `crates/akuma-exec/src/process/table.rs` |
| Message bus (reuse) | `crates/akuma-rump/src/sysproxy.rs`, `src/rump_proxy.rs` |
| Transport trait | `crates/akuma-rump/src/sysproxy.rs` |
| Box / namespace isolation | `src/syscall/container.rs`, `crates/akuma-isolation/src/lib.rs` |
| Config struct precedent | `src/main.rs:653` (`ExecConfig`) |

---

## 15. Per-core kernel runtime (Approach 2): R1–R4

M1 gave each secondary an *isolated* address space, but its restricted table maps no
kernel heap/PMM and the kernel's globals (`static PMM`, allocator, `akuma_exec`'s
`POOL`) live at fixed VAs in `.data`/`.bss` that the secondary doesn't map. To run a
real **process** on core 1 (M3), the secondary needs its own runtime. The chosen
mechanism is §4.2's: **replicate the writable sections per core** — map `.data`/`.bss`
(and heap/stack) to PRIVATE physical pages at the SAME VA, so the *same* shared code
resolves every `static` to that core's own instance, with zero code changes.

Staged, each independently verifiable:

| Stage | Goal | Notes |
|---|---|---|
| **R1 — writable replication** ✅ | Secondary gets a private `.data`/`.bss` at the kernel VA | `snapshot_pristine_data()` (first thing in `rust_start`) copies `.data`→`DATA_SNAPSHOT` before any mutation; `replicate_writable_window()` maps `[_data_start,_kernel_phys_end)` to private pages (`.data` from snapshot, `.bss` zeroed). **The descriptor (`MACHINE_CONFIG`, a `.bss` static) is the one thing that must stay SHARED** — replication skips it and maps it shared. Proof: a secondary mutates a shared static into its private copy; the BSP's copy stays pristine. |
| **R2 — per-core PMM + heap** ✅ | Secondary runs `pmm::init`/`allocator::init` over its partition; `alloc` works, BSP PMM untouched | DONE. `PartitionBump` carves the secondary's whole bringup image (page tables + replicated `.data`/`.bss` + stack + PerCpu) from its OWN partition (never the BSP `pmm`); `build_isolated_table` identity-maps the partition as 2 MiB RW blocks and records the consumed prefix as `kernel_end`. The secondary then seeds a private heap just above `kernel_end` and runs the unchanged `allocator::init` + `pmm::init` over `[pbase, pbase+len_2mb)` — they resolve to its replicated `static`s, so nothing the BSP owns is touched. `smp::reserve_secondary_partitions` (called right after the BSP's `pmm::init`, before `mmu::init`) removes those ranges from the BSP pool so the two are disjoint. Proof: `run_r2_test` allocs a heap `Vec` + 16 PMM pages and posts the result to PerCpu; the BSP confirms in-partition + BSP-pool-unchanged (verified SMP=2/4). Did **not** call `akuma_exec::mmu::init` on a secondary (it writes the SHARED boot tables). |
| **R3a — per-core COOPERATIVE scheduler** ✅ | Secondary runs `akuma_exec`'s scheduler per-core and switches between kernel threads via `yield_now` | DONE (SMP=2/4). `run_r3a_coop_test`: register the runtime locally (`build_exec_runtime` extracted from `main.rs`, canaries off, secondary stack bounds) since the BSP's `RUNTIME`/`CONFIG` cells are set post-snapshot and a secondary's replicated copy is pristine; `akuma_exec::mmu::set_boot_ttbr0_override(our table)` so spawned threads get OUR TTBR0, not the BSP's (`boot_ttbr0_addr` lives in `.data.boot`, *outside* the replicated writable window, so the asm cell can't be rewritten on a secondary); re-target the scheduler SGI at this PE (`gic::trigger_sgi` hardcodes TargetList bit 0 = BSP); install the real `exception_vector_table`; enable only the scheduler SGI; spawn 2 workers via `spawn_system_thread_fn` (the closure path builds a real `setup_fake_irq_frame` — the bare `spawn(extern fn)` path is register-based and incompatible with the stack-based scheduler) and `yield_now` between them. Proof: 2 threads × 8 yields = 16, then the M2 heartbeat/doorbell loop resumes. |
| **R3b — per-core preemptive scheduler** ✅ | Per-core timer preempts kernel threads on a secondary | DONE (SMP=2/4). `run_r3b_preempt_test` reuses R3a's scheduler: registers a per-core timer handler via `irq::register_handler_no_gic` (the normal `register_handler` calls `gic::enable_irq`, which for INTID<32 pokes **core 0's** redistributor — faults on a secondary); runs on the real `exception_vector_table`; enables PPI 27 in its OWN redistributor + arms CNTV. The handler re-arms CNTV then rings the scheduler SGI **at itself** (`trigger_sched_sgi_self`) — same two-step as the BSP's `timer_irq_handler`. Proof: a preemptive spinner that never yields advances (~12 M iters) only because the timer preempts the never-yielding boot thread (which, being the cooperative idle thread, is preempted past `COOPERATIVE_TIMEOUT_US`=100 ms; run window 300 ms). |
| **R4a — cross-core forwarding TRANSPORT** ✅ | The data-movement round-trip (§8.1): request/reply over the ring + a shared bounce region | DONE (SMP=2/4). The keystone for everything else in R4 ("exec is recursive forwarding"), and the half §8.1 calls *hard*. Added `akuma_smp::FwdBounce` (per-core `[AtomicU8; 256]` slot in the descriptor, the sole shared byte buffer — published by the ring's `ready` Release/Acquire) + `MSG_FWD_ECHO_{REQ,REPLY}`. `run_r4a_fwd_test` (secondary): `copyin` a payload to its bounce slot → push request to the BSP inbox → spin (time-bounded) on the reply → `copyout` + verify. `service_fwd_requests` (BSP): drains its inbox **from the bringup online-wait loop** (else BSP-waits-Online ⇄ secondary-waits-reply deadlock), transforms the bounce payload (byte+1, standing in for the real owner-side syscall), replies. Proof: nonce-matched reply + verified transform = the §8.1 data path works end to end; neither core touched the other's partition. Independent of the scheduler, so it runs even when R3a is skipped. |
| **R4b — per-core syscalls + pinned process** (staged R4b.1–.5) | SVC from EL0 on the per-core vectors; spawn `/bin/hello` (then `curl`) pinned to core 1, forwarding `open`/`read`/`write`/sockets to core 0 (§10) | The §10 acceptance test, staged like R1–R4a. **R4b.1 ✅** scheduler as steady state. **R4b.2 ✅** persistent BSP forward-server thread. **R4b.3** EL0 on the secondary — **R4b.3a ✅** the full secondary user-table kernel view (`build_secondary_user_kernel_view` + `UserAddressSpace::map_kernel_block_2mb`): a user address space on a secondary maps code RO+X identity (handler runs), the `.data`/`.bss` window to ITS private pages (not the BSP's), and its partition identity (`phys_to_virt` at EL1) — peers unmapped; verified by walking all three regions. **R4b.3b ✅** create + `eret` to a pinned EL0 process + syscall dispatch via `sync_el0_handler` → replicated state — subsumed by R4b.4 below (a real EL0 process running on a secondary IS eret-to-EL0 + working syscall dispatch). **R4b.4 ✅ — forwarded exec, full process (§10 Part A, acceptance/12 Milestone 2):** a real EL0 process (`/bin/hello`) runs **pinned to core 1**. herd hands core 1's kernel the program path in the `MSG_CORE_INIT` activation message (`core_init(idx, path)` — NO cross-core spawn, §7); core 1's steady-state loop (`spawn_init_program`) fetches the WHOLE ELF from core 0's ext2 via forwarded `openat`/`read`/`close` (`fetch_file_forwarded`, "exec is recursive forwarding") and spawns it LOCALLY via the normal path (`spawn_process_from_image` → `Process::from_elf`). The per-core kernel-window overlay rides the `ExecRuntime::prepare_user_address_space` runtime hook (set per core in `run_r3a_coop_test`), so the SAME loader builds a correct user table on a secondary. The process's `write(1)` drains to core 0's UART via the §8.2 console ring (`sys_write` routes secondary tty output through `console::print_bytes`); its exit is reaped locally. Verified SMP=2 @2048: `[core 1] init: spawned /bin/hello (pid 1, tid 8)` then `hello (1/10)…(10/10) hello: done`. GOTCHA (cost a few boots): the secondary boot/idle stack was 16 KiB — the deep `load_elf_with_stack` frames overflowed it into the replicated `.data`, corrupting CONFIG/RUNTIME (read back "not registered"); fixed by carving a 256 KiB isolated boot stack (`STACK_PAGES=64`). **R4b.5** sockets — `curl` = add socket arms to `service_forwarded_syscall` (§10 Part B). |
| &nbsp;&nbsp;**R4b.1 — scheduler as steady state** ✅ | Secondary never tears down; real vectors + timer→scheduler run permanently | DONE (SMP=2/4). `run_r3a_coop_test` now returns whether it stood the scheduler up; if so, after announcing Online `secondary_main` enters `secondary_steady_state` (never returns) instead of the M2 `WFI` loop. It registers the timer PPI (R3b's `secondary_timer_preempt_handler`) + the doorbell SGI handler via `register_handler_no_gic`, brings up the GIC receive path for all three per-core sources (`secondary_gic_init` + `scheduler_sgi_enable`), installs `exception_vector_table`, arms CNTV, and runs the heartbeat/debt-protocol drain as the boot thread's idle loop (`yield_now` each pass). The doorbell handler finds PerCpu via a new replicated `SECONDARY_PERCPU` static, **not** TPIDRRO_EL0 (the scheduler claims that register for the current-thread id — the load-bearing difference from the `smp_vectors` path). The idle loop **`WFI`s** when nothing is runnable (it does NOT busy-`yield_now`): a tight yield loop rings the scheduler SGI every pass, pegging the core at 100% and — on a virtualized GIC — flooding the hypervisor with VM exits that starved the BSP's boot; `WFI` keeps the core near-idle (the timer still preempts to any runnable thread and keeps liveness advancing). Proof: heartbeat advances at timer rate (`0→62` over 500 ms vs `~190k` busy-yielding), debt repay works through the idle-loop inbox drain, and the M2 doorbell is serviced via the real IRQ path. |
| &nbsp;&nbsp;**R4b.2 — persistent BSP forward-server thread** ✅ | The BSP services cross-core forwards from a long-running thread, not the transient bringup loop | DONE (SMP=2, MEMORY=2048). `start_fwd_server()` spawns a BSP system thread (like the console drainer, from `run_async_main` once preemption is live) that loops `service_fwd_requests` + `yield_now` for the system's lifetime; the bringup wait loop's inline servicing is KEPT (R4a still needs it). Real forwarding targets (VFS/sockets) only come up post-bringup, so R4b.4+ point this thread at them. Verified two-sided: the thread sets `MachineConfig::fwd_server_ready`; the steady-state secondary, gated on that flag (so the request can only be serviced by the thread, not the exited bringup loop), fires one echo round-trip from its idle loop and verifies the reply → `[core 1] post-bringup forward round-trip PASS` + the thread's `serviced post-bringup forward(s) PASS`. |

**Why R1 is the keystone:** once `static`s are per-core, R2–R4 reuse the *existing*
kernel init/exec code unchanged — the per-core-ness lives entirely in the page tables.

## 16. Deferred cleanup / tech debt (later)

Tracked here so it isn't lost; tackle after the R3b/R4 milestones, not inline.

- **R3a/R3b/R4a leftovers.** All three are bounded bringup probes: each stands its mechanism up,
  proves a property (cooperative yield / timer preemption / forward round-trip), then tears down
  (restores `smp_vectors`, masks IRQs, R3b disables CNTV) and the secondary returns to the M2
  heartbeat WFI loop, leaving a dormant scheduler + terminated worker slots. The `PERCPU_R3_STAGE`
  marker + BSP timeout print are kept as cheap living diagnostics. R4a's `service_fwd_requests` is
  driven inline by the BSP bringup loop (fine for a bringup probe); **R4b** needs a *persistent* BSP
  forward-server (a system thread, like the console drainer), because real forwarding targets (VFS,
  sockets) only come up post-bringup. **R4b** is also where the secondary stops tearing down: it
  runs on the real `exception_vector_table` permanently with the timer wired to the scheduler as its
  steady-state loop, and hosts a real pinned process (the heartbeat/debt work becomes one of its
  threads).

- **Sweep deprecated code (general).** Do a pass over the kernel + crates for dead/deprecated
  code and remove it. (Done so far, 2026-06-29: removed the dead `switch_context` asm + its
  `extern` decls and the deprecated `sgi_scheduler_handler` in `crates/akuma-exec/src/threading`
  — the live scheduler is the stack-based `sgi_scheduler_handler_with_sp` driven by the
  `exceptions.rs` IRQ path. `thread_start`/`thread_start_closure` trampolines are KEPT — the
  modern path still uses them.)
- **Console (§8.2) tweaks.** Producer drops on a full ring (add yield-backpressure?); the drainer
  runs every scheduler quantum (add a coarser cadence + coalesced-doorbell wake); optionally route
  the BSP's own console through a ring too and move the whole console to a userspace server.
- **CoW benchmark gating** — `run_cow_benchmarks()` prints `[BENCH]` every boot on purpose; gate
  behind a flag.

- **Investigate real cross-core network/FS data transfer between live processes.** Beyond the R4a
  echo transport: drive the §10.2 path with *real* userspace producers/consumers split across
  cores — e.g. `httpd` pinned to core 0 (owns Net) and `curl` pinned to core 1 (Proxy(Net)→0), or
  a file read/write where the reader and the FS owner are on different cores. Measure where the
  bytes actually move (bounce region offsets, chunking, copy amplification), confirm shared-nothing
  holds at the data layer under load, and find the throughput/latency cost of the round-trip+
  doorbell vs a single-core syscall. This is the load-test of R4b.4/R4b.5 once they exist.

- **Rename milestone-tag identifiers to descriptive names (whole `smp-attempt-0` branch).** The
  staging tags (R1/R2/R3a/R3b/R4a/R4b…) leaked into IDENTIFIERS, not just comments: e.g.
  `run_r3a_coop_test`, `run_r3b_preempt_test`, `run_r4a_fwd_test`, `PERCPU_R2_PAGES`,
  `PERCPU_R3_YIELDS`, `PERCPU_R4A_OK`, `R3_*`/`R3B_*` consts, `R4A_LEN`, `PERCPU_R3_STAGE`, etc. These
  read like the `CoreBrain`→`CoreStateMachine` rename the owner already asked for — give them
  descriptive names (what they DO, not which milestone added them) and keep the milestone reference
  in doc-comments only. (R4b.3a's new names were done descriptively: `verify_user_table_kernel_window`,
  `overlay_secondary_kernel_window`, `PERCPU_USERTAB_*`.) Do this as one sweep so a future reader
  isn't decoding milestone numbers.

- **Sweep the whole codebase for `console::print` and convert to `safe_print!`.** `src/smp.rs` is
  done (2026-06-29): every hand-rolled `console::print` + `print_dec`/`print_hex` run is now one
  `safe_print!(N, "…{}…", …)` (heap-free stack buffer, routes through the same `emit()` chokepoint,
  so equally safe on a secondary; only the drainer's raw `print_bytes` forwarder is kept). **We
  mostly do NOT need bare `console::print` anywhere** — `safe_print!` is the house convention.
  Remaining `console::print`/`print_dec`/`print_hex` callers elsewhere in `src/` should get the
  same treatment. Keep `print_bytes` only where raw, already-formatted bytes are forwarded.

- **Boot self-test suite assumes full RAM — panics under SMP partitioning.** The BSP runs the
  whole `process_tests`/threading/memory suite on every boot; under `--features smp` the BSP keeps
  only its partition (e.g. 128 MB of a 256 MB default with SMP=2), and RAM-sensitive tests fail —
  `test_mmap_file_oom` PANICs (`oversized file mmap (lazy=true) unexpected exit 0, want -11`) because
  the OOM threshold no longer matches. Workaround today: boot SMP with more RAM (`MEMORY=2048`).
  Real fix: gate/skip the RAM-tuned self-tests (or scale their thresholds to the *partition* size)
  when `kernel_smp` + multi-core, or don't run the full suite on the BSP under SMP.

- **Put the BSP on its own restricted partition table (close the isolation asymmetry).** Today
  isolation is enforced secondary→peer, but the BSP still runs the global identity map (all RAM
  mapped), so it can read/write any secondary's partition. It deliberately relies on this for
  bringup verification (reading PerCpu markers). To make isolation symmetric, give the BSP a
  restricted TTBR0 like the secondaries and route its verification through the shared descriptor
  instead of direct peer reads. Until then, "shared-nothing" holds for secondaries only (§4.2).

- **Owner-side capability guard on forwarded syscalls (enforcement, not just routing).** Today
  `service_forwarded_syscall` dispatches whatever forwarded syscall arrives — the secondary's
  classify-and-route (`capability_of`/`capability_owner`) is *advisory*. The owner must ENFORCE the
  boundary: on a `MSG_FWD_SYSCALL_REQ` from core C, reject (e.g. `-ENOSYS`/`-EPERM`) unless the
  syscall's capability group is one C was actually granted to `Proxy` to *this* owner — i.e.
  `caps[C][capability_of(nr)] == Proxy(me)`. Also reject `Local`-group syscalls outright: a `Local`
  syscall must never be forwarded (it belongs on the requester's own kernel), so receiving one is a
  misconfiguration/attack. This is what makes capability *subtraction* real — a core with
  `Net = Absent` that forwards a socket call anyway gets refused at the owner, not silently served.
  Depends on the caps map being data (below); pairs with it.

- **Caps map belongs in the descriptor (§5), not hardcoded.** `capability_owner` currently
  hardcodes the Phase-0 split (core 0 owns Vfs + Net). Make `caps` a real per-core field in
  `MachineConfig` — `[Capability×owner]` per core — read at runtime so the split is configurable
  and stays re-pointable for fail-over (§12 leadership note).

- **Use `syscall::nr::*` constants everywhere — no magic syscall numbers.** The generic forwarder's
  dispatch table + classifier (`capability_of`, `service_forwarded_syscall`) now use the kernel's
  canonical `syscall::nr` constants. Sweep the codebase for any remaining bare syscall-number
  literals and replace them; honor this convention in new code.

- **Boxes × VFS in a multikernel.** A `box` (container/namespace, `src/syscall/container.rs`) scopes
  mounts/rootfs/fd-namespace for a process. When the process is pinned to a compute core but its VFS
  is `Proxy`'d to the owner, decide where the namespace lives: the owner resolves paths, so the
  box's mount table / chroot must be visible to (or forwarded with) the owner-side `openat` — likely
  the forwarded `openat` carries the box id and the owner resolves within that box's namespace.
  Open question; design before a real boxed process runs on a secondary.

- **Per-core chroot with shared read-only system dirs (idea, 2026-06-29).** Rather than giving
  each core a full box/namespace, give the capability-VFS owner a simple split-resolution rule for
  forwarded `openat`: a small set of **read-only system dirs** (`/bin`, `/usr`, …) resolve to ONE
  **common** tree shared by all cores (they're immutable, so sharing is safe and saves duplicating
  the toolchain/libs), while **everything else** resolves under a **per-core chroot** at
  `/srv/cores/<N>` (core 1 → `/srv/cores/1`, etc.). So each core gets an isolated writable root for
  free, with the heavy read-only system content shared. The owner picks common-vs-chroot by the
  leading path component when it services the forwarded `openat` (it already resolves all paths —
  this is just a prefix table + a per-requesting-core root). Later, even **core 0 (BSP) chroots to
  `/srv/cores/0`**, so no core is special-cased and the host root isn't directly visible to any
  workload. Composes with the caps map (the chroot root + shared-RO set could be per-core descriptor
  fields) and supersedes needing full boxes for the common "isolated rootfs per core" case. Pairs
  with the boxes-×-VFS item above (a box id would select a namespace *within* the core's chroot).

- **Per-core fd/socket affinity (forwarding-model invariant).** Because a process belongs to
  exactly one core (it's pinned, with its address space in that core's partition), every fd /
  socket it opens — including ones whose backing capability is `Proxy`'d to the owner core — is
  logically *owned by the process's core*: only that core ever issues syscalls against the fd, so
  the forward request/reply for a given fd always travels the **same core→owner edge**. The owner
  (core 0) keeps the real `struct file`/socket; the process's core holds only an opaque handle it
  forwards. Consequence to honor when building R4b.4/R4b.5: the fd table the EL0 process sees is
  the *secondary's* (its own replicated state), mapping local fd → `(owner core, remote fd)`; the
  owner need not track which core a remote fd came from beyond the reply route, and there is no
  fd-sharing across cores (no cross-core `dup`/SCM_RIGHTS) — a simplifying invariant, not a
  limitation, since processes don't migrate. Keeps each socket's traffic on one deterministic
  ring edge.

- **Flip core activation to herd-managed by default (currently interim BSP auto-init).** The
  park→`MSG_CORE_INIT`→init lifecycle is in (cores park after their soundness self-tests and
  await activation; a watchdog `CPU_OFF`s an un-activated core; `core_init` re-`CPU_ON`s it). The
  userspace seam is wired both ends: the `core_init` syscall (`nr::CORE_INIT` = 327 → `smp::core_init`)
  and `/proc/cores` (the BSP-served, machine-global state table), and herd reads `/proc/cores` and
  calls `core_init` for non-online secondaries (`init_secondary_cores`). BUT for now the **BSP
  auto-activates** every secondary during bringup (so the role self-tests R3a/R3b/R4a/R4b.3a still
  run at boot and the existing BSP-side PerCpu reporting works) — so herd's calls are a no-op
  (cores already `online`). To make `MULTIKERNEL_INIT_HERD` real (herd drives activation by
  default): (1) gate the BSP auto-init loop in `bringup_secondaries` on `!(config::AUTO_START_HERD
  && config::MULTIKERNEL_INIT_HERD)`; (2) in herd-managed mode have bringup wait for `STATE_PARKED`
  (not `ONLINE`) and report only the soundness checks; (3) move the role-probe PASS/FAIL reporting
  from BSP-PerCpu-reads (which only work while bringup is still waiting) to the **secondary's own
  console ring**, since the role probes now run later (when herd activates the core), after bringup
  has returned. Dependency for (3): **the secondary console ring isn't draining** in the SMP boot —
  no `[core N] …` ring lines (nor the BSP's own `fwd-server thread spawned` line) appeared in the
  `SMP=2` boot log despite a full boot; investigate the `start_console_drainer` / `run_async_main`
  path under `kernel_smp` first, or herd-managed boots will show no role results.
