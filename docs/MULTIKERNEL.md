# Akuma Multikernel — One Kernel Per Core

**Status:** In progress — §11 build gating + **M0 (second core spins) DONE** (2026-06-28),
verified under `SMP=2`: core 1 wakes via PSCI `CPU_ON` (hvc), reports `Online`, BSP confirms.
Build with `scripts/build_smp.sh`; boot with `scripts/run_smp.sh` (or `SMP=2 cargo run
--profile release-smp --features smp`). M1–M4 pending (see §12).
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
- **Effortless process pinning** — a process lives on the kernel instance that spawned it.
  Pinning is "spawn on core N"; migration is an explicit "ship this process to core M" message,
  not a scheduler-affinity subsystem.
- **Function offload / stripped kernels** — a core's `role` (read from boot config) selects
  which subsystems its image initializes. A compute core can omit the network stack and VFS
  entirely and **forward** those syscalls to the owning core (exactly the rump model,
  generalized to peer kernels).
- **Configurable at init** — roles and memory bounds come from a boot-time descriptor, not
  compile-time constants.

---

## 2. Current state of the codebase

**Fundamentally single-core today**, but the right things already use per-CPU hardware state:

- ❌ **No secondary-core bringup at all** — zero PSCI / `CPU_ON`, no `MPIDR_EL1` reads, no
  per-CPU data area, no core ID. `src/boot.rs:104` sets one `STACK_TOP`.
- ❌ `trigger_sgi()` (`src/gic_v3.rs:222`) hardcodes the target list to `| 1` = PE0 only — no
  cross-core IPI targeting yet.
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
4. Build core N's TTBR1 set covering **only** its partition + shared regions.

> ⚠️ The static boot page tables (`src/boot.rs:148`) and `extend_boot_ram_identity_map`
> (`crates/akuma-exec/src/mmu/mod.rs:54`) are currently **global/shared**. For real isolation
> each core needs its own TTBR1 set. Two stages:
> - **Isolation by convention** (quick): all cores share the identity map; behaves because each
>   PMM only hands out its slice. Good enough to stand up M1–M2.
> - **Isolation by hardware** (hardening): per-core tables map only the partition →
>   cross-partition access *faults* instead of silently corrupting a peer.

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

---

> **Implementation note (2026-06-28):** the pure, host-testable half of the SMP
> subsystem now lives in `crates/akuma-smp` (`no_std`): the lock-free MPSC `Ring`,
> the `MachineConfig` descriptor, `partition()`, and a **sans-IO `CoreBrain` state
> machine** for the debt-based memory-reclaim protocol (§9) — driven by `step(Event,
> emit)` so the identical, alloc-free logic runs in an isolated secondary *and* in a
> host simulator. `cargo test -p akuma-smp` exercises the ring under real concurrent
> threads and simulates the protocol across N cores (conservation, repay-your-creditor,
> receiver-zeroing) with zero QEMU. `src/smp.rs` is the kernel glue (asm, PSCI, page
> tables, the pump).

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

Message types (grow as needed): `SpawnProcess`, `ForwardSyscall` (compute core → owning core for
net/VFS), `SignalDeliver`, `ReleasePages`/`AcceptPages` (§4.4), `Shutdown`.

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
split to `Own` on a secondary. A box `B` is created pinned to core 1.

Key insight: **exec is recursive forwarding.** Core 1 has no VFS, so loading `/bin/hello`
forces it to forward `open`/`read` back to core 0 to fetch the ELF bytes. So `hello` alone
already tests spawn-forward (0→1), VFS-read-forward (1→0), and console output (1→0 async).
`curl` then adds socket-forward (1→0). This is the full matrix.

```
Part A — spawn + exec hello on core 1
─────────────────────────────────────
shell (core 0) : spawn_ext(box=B, pin=core1, "/bin/hello")
core 0 kernel  : B pinned to core1  ──SpawnProcess{argv,env,cwd,box}── ring[1] ─▶ core 1
core 1 kernel  : create process in core1 partition; begin exec("/bin/hello")
                 loader open("/bin/hello")   ─ VFS not owned ─
                       ──ForwardSyscall{open}── ring[0] ─▶ core 0 (ext2)  ──fd──▶
                 loader read(fd, …) ×N        ─ bulk via bounce region ─
                       ──ForwardSyscall{read}── ring[0] ─▶ core 0  ──bytes (§8.1 outbound)──▶
                 map ELF segments into core1 pages; build stack; schedule
hello (core 1) : write(1, "hello\n")  ─ console not owned ─
                       append to core1 console ring (§8.2); return immediately
core 0         : drains core1 console ring on tick ─▶ "hello\n" on serial
hello (core 1) : exit ──ChildExit── ring[0] ─▶ core 0 reaps / shell sees status

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
acceptance test.

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
| **M1 — isolated second kernel** 🟡 | Each secondary runs on its OWN restricted page table (shared RO code + descriptor page + private stack/PerCpu; peers UNMAPPED) and **hardware-enforced isolation is PROVEN** (a deliberate cross-core read faults). NOTE: this codebase is a TTBR0 identity-mapped kernel, so M1 is realized by per-core *TTBR0* tables + a per-core `[PerCpu]` private chunk (not the doc's original "replicated .data/.bss via TTBR1" — see §4.2 note). Still TODO for full M1: per-core `pmm::init`/heap/scheduler over a real partition. | per-core restricted TTBR0 tables (`build_isolated_table`), `secondary_enter_isolated` (TTBR0+SP switch, **`isb` after `msr ttbr0` is mandatory** or the global 1 GB boot block survives in the TLB and isolation leaks), per-core VBAR enforcement self-test |
| **M2 — ping-pong** | Core 0 ↔ Core 1 exchange a message over a shared ring + SGI doorbell | fix `trigger_sgi()` affinity targeting, `CoreTransport` over ring, SGI handler drain |
| **M3 — roles + forwarding** | The §10 acceptance test: `hello` + `curl` pinned to core 1 | role table, VFS-read forwarding (first), syscall-forwarding stubs (§8.1), console append ring (§8.2), spawn/exit messages, reuse sysproxy marshaling |
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
| Boot assembly entry, x0 handoff | `src/boot.rs:61`, `src/main.rs:146` |
| Memory detection (DTB) | `src/main.rs:193` |
| Memory layout computation | `src/main.rs:270` |
| PMM init (already takes bounds) | `src/pmm.rs:499`, call site `src/main.rs:564` |
| Boot page tables / identity map | `src/boot.rs:148`, `crates/akuma-exec/src/mmu/mod.rs:54` |
| `get_boot_ttbr0` | `crates/akuma-exec/src/mmu/mod.rs:236` |
| GIC redist + SGI (fix targeting) | `src/gic_v3.rs:152`, `src/gic_v3.rs:222` |
| Per-CPU timer | `src/timer.rs:30` |
| Scheduler / global `POOL` | `crates/akuma-exec/src/threading/mod.rs:2179` |
| Process table / global statics | `crates/akuma-exec/src/process/table.rs` |
| Message bus (reuse) | `crates/akuma-rump/src/sysproxy.rs`, `src/rump_proxy.rs` |
| Transport trait | `crates/akuma-rump/src/sysproxy.rs` |
| Box / namespace isolation | `src/syscall/container.rs`, `crates/akuma-isolation/src/lib.rs` |
| Config struct precedent | `src/main.rs:653` (`ExecConfig`) |
