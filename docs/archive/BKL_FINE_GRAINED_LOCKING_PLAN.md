# BKL Fine-Grained Locking Plan

**Status** (2026-08-01): Phases 0–6 complete and default-on in `smp-shared`. **Phase 7
replanned** — the originally-planned removal is not executable; see
[`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md) and §7 below.  
**Strategy**: Phased carve-out, then *wither* the BKL via a per-syscall opt-in list
(§7.3) rather than removing it in one step.  
**Target First**: Networking stack (done)

---

## Overview

This plan breaks up the Big Kernel Lock (BKL) into fine-grained subsystem locks, eliminating it completely through a phased approach. The networking stack is targeted first because it has well-defined boundaries and some existing BKL-drop optimizations.

**Current State** (2026-07-24, as written at the time):
- Single fair FIFO `KernelLock` serializes all kernel (EL1) execution across cores
- Held "iff a core is in EL1", reconciled at EL transitions
- ~~Scheduler/IRQ path holds ~70% of contended BKL time~~ — **WRONG, and this line is
  where the error propagated from.** It was a Phase 0 estimate; when finally measured it
  was 27%, and the 66–73% figure that later seemed to confirm it came from a profiler that
  credited a preempted thread's whole remaining syscall to `irq/sched`. Fixed and
  re-measured: **23.0%**, and ~21–23% fresh at HEAD after Phases 5–6 and the
  `netpoll_drain` carve. Largest remaining holders are `execve` (~22%) and `clone`
  (~10–13%). See [`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md) §1.
- Networking has existing fine-grained locks but still requires BKL at syscall boundaries

**Current State** (2026-08-01, measured):
- Phases 1–6 landed: `no-bkl-network`, `no-bkl-vfs`, `no-bkl-process`, `no-bkl-mm`,
  `no-bkl-drivers`, plus the `netpoll_drain` carve — all default-on in `smp-shared`
- The BKL is still **load-bearing**, not redundant, for six structures whose only
  cross-core guard is `with_irqs_disabled` (audit §2). It is a shim over the single-core
  ownership model `main` still has, not debt this effort added.
- Removal is gated on giving those structures real locks — not on further syscall
  carve-outs

**Goal**: Replace BKL with subsystem-specific locks while maintaining correctness and improving multi-core performance.

---

## Phase 0: Preparation & Analysis ✅ COMPLETE

**Week 1** - Network audit complete

### Network Audit Results

**Current Locking Strategy**:
- **BKL**: Implicitly held during all networking operations (acquired at syscall boundaries)
- **Fine-Grained Locks**: 
  - `NETWORK: Spinlock<NetworkState>` - Interface, sockets, device state
  - `SOCKET_TABLE: Spinlock<Vec<Option<KernelSocket>>>` - Socket descriptor table
  - `NET_STATS: Spinlock<Stats>` - Network statistics
  - Per-socket `wakers: Spinlock<Vec<Waker>>` - Epoll/blocking I/O wakers

**Key State Components**:

| Component | Location | Current Protection | Notes |
|-----------|----------|-------------------|-------|
| Socket Table | `socket.rs:243` | BKL + Spinlock | 128 max sockets |
| Network Interface | `smoltcp_net.rs:106` | BKL + Spinlock | Interface + socket set |
| Packet Buffers | `smoltcp_net.rs` | BKL + Spinlock | TCP/UDP buffers + loopback queue |
| DNS Resolver | `smoltcp_net.rs:100` | BKL + Spinlock | Single DNS socket |
| TCP Connections | `smoltcp_net.rs` | BKL + Spinlock | All in `NetworkState.sockets` |
| Socket Wakers | `socket.rs:139` | BKL + Spinlock | Per-socket epoll support |

**Existing BKL Drop Points**:
- `socket::wait_until()` - Generic blocking wait
- `socket_accept()`, `socket_connect()`, `socket_send()`, `socket_recv()`, `socket_recv_udp()`
- `dns_query()` - 10-second timeout
- All use `blocking_relax()` to drop BKL during waits

**Deadlock Risks Identified**:
- **AB-BA Deadlock**: Holding `NETWORK` while acquiring `SOCKET_TABLE` causes deadlock
- **Preemption Stranding**: Inner spinlocks held across context switches strand locks under BKL
- **Lock Ordering**: `BKL → inner spinlocks` (never reverse)

**Lock Ordering Rules**:
1. BKL (global)
2. NETWORK spinlock
3. SOCKET_TABLE spinlock
4. Per-socket wakers spinlock

---

## Phase 1: Network Lock Foundation

**Week 2** - Add networking locks without changing behavior

### Tasks

1. **Create Lock Infrastructure** (`crates/akuma-net/src/locks.rs`):
   ```rust
   // Global network lock (replaces BKL for network operations)
   pub static NETWORK_LOCK: SpinLock<()> = Spinlock::new(());
   
   // Per-socket lock structure
   pub struct SocketLock {
       inner: Spinlock<SocketState>,
   }
   
   // Lock ordering enforcement
   pub const LOCK_ORDER_NETWORK: u8 = 10;
   pub const LOCK_ORDER_SOCKET_TABLE: u8 = 20;
   pub const LOCK_ORDER_SOCKET: u8 = 30;
   ```

2. **Add Profiling Hooks**:
   - Extend existing BKL profiler to track network lock contention
   - Add network-specific contention metrics
   - Track lock hold times per operation

3. **Document Lock Hierarchy**:
   - `NETWORK_LOCK > SOCKET_TABLE_LOCK > per_socket_lock`
   - Prevent deadlocks through strict ordering
   - Add runtime lock dependency tracking

4. **Create Testing Framework**:
   - Network lock contention tests
   - Deadlock detection tests
   - Performance baseline measurements

### Deliverables
- [ ] `crates/akuma-net/src/locks.rs` with lock definitions
- [ ] Profiling integration with existing BKL profiler
- [ ] Lock hierarchy documentation
- [ ] Network lock contention tests

### Success Criteria
- Lock scaffolding compiles and tests pass
- Profiling shows BKL still held (no functional changes yet)
- No deadlocks in stress tests

---

## Phase 2: Network BKL-Free Path

**Week 3-4** - Make networking operations work without BKL

### Tasks

1. **Create BKL-Free Syscall Entry Points**:

   **Update syscall handlers** in `src/syscall/net.rs`:
   ```rust
   // sys_socket - BKL-free version
   extern "C" fn sys_socket(domain: i32, type_: i32, protocol: i32) -> i64 {
       // Drop BKL before network operations
       akuma_exec::bkl::leave_kernel();
       
       let result = {
           // Acquire network lock
           let _net_guard = NETWORK_LOCK.lock();
           akuma_net::socket::socket_create(domain, type_, protocol)
       };
       
       // Re-acquire BKL before returning
       akuma_exec::bkl::enter_kernel();
       result
   }
   ```

   **Functions to update**:
   - `sys_socket`, `sys_bind`, `sys_listen`, `sys_accept`
   - `sys_connect`, `sys_sendto`, `sys_recvfrom`, `sys_recvmsg`, `sys_sendmsg`
   - `sys_shutdown`, `sys_getsockopt`, `sys_setsockopt`
   - `sys_socketpair` (if implemented)

2. **Update Network IRQ Handler**:

   **Modify `poll()` in `smoltcp_net.rs`**:
   ```rust
   pub fn poll() {
       // Drop BKL before packet processing
       akuma_exec::bkl::leave_kernel();
       
       {
           let _net_guard = NETWORK.lock();
           // Process packets, handle DHCP/DNS, etc.
       }
       
       // Re-acquire BKL after processing
       akuma_exec::bkl::enter_kernel();
   }
   ```

3. **Handle Blocking Operations**:

   **Update `blocking_relax()` calls**:
   - Already drop BKL, keep behavior
   - Add network lock acquisition after BKL drop
   - Ensure lock ordering is maintained

4. **Test Network Operations**:

   **Stress tests**:
   ```bash
   # Multi-core network stress
   SMP=4 cargo run --profile release-smp-shared --features smp-shared
   
   # Network benchmark
   ./network_stress_test
   ```

### Deliverables
- [ ] BKL-free syscall entry points for all network syscalls
- [ ] Updated IRQ handler with BKL drops
- [ ] Blocking operation compatibility
- [ ] Multi-core network stress tests passing
- [ ] Performance measurements showing contention reduction

### Success Criteria
- Network operations work BKL-free
- Profiling shows ~15-20% BKL contention reduction
- No deadlocks or livelocks at SMP=4
- Network performance maintained or improved

---

## Phase 3: Process Management Locks

**Week 5-6** - Replace BKL for process/thread operations

### Tasks

1. **Design Process Table Locking**:

   **Create `src/process/locks.rs`**:
   ```rust
   // Global process table lock
   pub static PROCESS_TABLE_LOCK: Spinlock<()> = Spinlock::new(());
   
   // Per-process lock structure
   pub struct ProcessLock {
       inner: Spinlock<ProcessState>,
   }
   
   // Process state that needs protection
   pub struct ProcessState {
       pub status: ProcessStatus,
       pub exit_code: Option<i32>,
       pub children: Vec<Pid>,
       pub parent: Option<Pid>,
   }
   ```

2. **Refactor `lookup_process()` Sites** (218+ locations):

   **Audit each usage pattern**:
   ```rust
   // Current pattern
   let proc = lookup_process(pid)?;
   proc.do_something();
   
   // New pattern
   let proc_guard = PROCESS_TABLE_LOCK.lock();
   let proc = lookup_process_locked(&proc_guard, pid)?;
   proc.do_something_locked(&proc_guard);
   ```

   **Key locations to update**:
   - `src/process/table.rs` - Process table operations
   - `src/process/children.rs` - Parent/child relationships
   - `src/threading/mod.rs` - Thread operations
   - `src/syscall/proc.rs` - Process syscalls

3. **Update Scheduler**:

   **Leverage existing `POOL` lock**:
   - Keep `POOL: Spinlock<ThreadPool>` for run queue
   - Add `PROCESS_STATE_LOCK` for process transitions
   - Ensure scheduler can run BKL-free

4. **Create Lifecycle Guards**:

   **For multi-step operations**:
   ```rust
   pub struct ProcessLifecycleGuard {
       _guard: SpinlockGuard<'static, ()>,
   }
   
   impl ProcessLifecycleGuard {
       pub fn new(pid: Pid) -> Self {
           let guard = PROCESS_TABLE_LOCK.lock();
           // Mark process as "in lifecycle transition"
           Self { _guard: guard }
       }
   }
   ```

### Deliverables
- [ ] Process table lock infrastructure
- [ ] Refactored `lookup_process()` sites
- [ ] Updated scheduler with process state locks
- [ ] Lifecycle guard implementation
- [ ] Process management stress tests

### Success Criteria
- Process management works BKL-free
- All `lookup_process()` sites use proper locking
- Scheduler runs BKL-free
- No deadlocks in fork/exec stress tests

---

## Phase 4: VFS and Filesystem Locks

**Week 7-8** - Filesystem operations without BKL

### Tasks

1. **Design VFS Lock Hierarchy**:

   **Create `src/vfs/locks.rs`**:
   ```rust
   // Global mount table lock
   pub static MOUNT_TABLE_LOCK: Spinlock<()> = Spinlock::new(());
   
   // Per-filesystem lock
   pub struct FsLock {
       inner: Spinlock<FsState>,
   }
   
   // Per-inode lock
   pub struct InodeLock {
       inner: Spinlock<InodeState>,
   }
   ```

2. **Update Block I/O Path**:

   **Leverage existing `BLOCK_DEVICE` lock**:
   - Ensure BKL drops don't break new locking
   - Keep block device lock independent

3. **Test Filesystem Operations**:

   **Multi-core file access tests**:
   ```bash
   # Concurrent file operations
   SMP=4 ./fs_stress_test
   
   # Fault path still works
   ./fault_stress_test
   ```

### Deliverables
- [ ] VFS lock hierarchy
- [ ] Per-filesystem locks
- [ ] Per-inode locks
- [ ] Updated block I/O path
- [ ] Filesystem stress tests

### Success Criteria
- VFS operations work BKL-free
- Multi-core file access safe
- Page fault path still works correctly
- No filesystem corruption under load

---

## Phase 5: Memory Management Locks

**Week 9** - Fine-grained memory management locking

### Tasks

1. **Extend Existing `as_lock`**:

   **Address space lock already protects page table edits**
   - Extend to cover more PMM operations
   - Keep per-address-space serialization

2. **Add PMM Locks**:

   **Create `src/memory/locks.rs`**:
   ```rust
   // Per-arena heap locks
   pub static HEAP_ARENA_LOCKS: [Spinlock<()>; 4] = 
       [const { SpinLock::new(()) }; 4];
   
   // Page frame allocation lock
   pub static PAGE_FRAME_LOCK: Spinlock<()> = Spinlock::new(());
   ```

3. **Update TLB Shootdown**:

   **Keep inner-shareable flush** (M3):
   - Maintain cross-core TLB consistency
   - Add coordination for page table updates

### Deliverables
- [ ] Extended `as_lock` coverage
- [ ] Per-arena heap locks
- [ ] Page frame allocation locks
- [ ] Updated TLB shootdown coordination

### Success Criteria
- Memory management works BKL-free
- No heap corruption under multi-core allocation
- TLB consistency maintained

---

## Phase 6: Device Driver Locks

**Week 10** - Isolate device driver locking

### Tasks

1. **Audit Device Drivers**:

   **Devices to check**:
   - UART/console (has some locking)
   - Timer/GIC (mostly per-core)
   - VirtIO devices
   - Any remaining shared device state

2. **Add Per-Driver Locks**:

   **Example for VirtIO**:
   ```rust
   pub static VIRTIO_LOCK: Spinlock<()> = Spinlock::new(());
   ```

3. **Ensure IRQ Handlers BKL-Free**:

   **Update IRQ paths**:
   - Drop BKL before device-specific processing
   - Re-acquire only for cross-subsystem operations

### Deliverables
- [ ] Device driver audit
- [ ] Per-driver locks
- [ ] BKL-free IRQ handlers
- [ ] Device driver tests

### Success Criteria
- Device drivers work BKL-free
- IRQ handlers don't require BKL
- No device-related deadlocks

---

## Phase 7: BKL Removal & Hardening — REPLANNED 2026-08-01

> **The version of this phase originally planned here is not executable.** It was
> audited after Phases 2–6 all landed default-on, and the audit
> ([`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md)) found both of its premises wrong. The
> original four tasks are preserved verbatim in §7.0 below; §7.1 onward is the
> replacement.

### 7.0 What this section originally said, and why it fails

The original plan was: (1) strip `bkl::enter_kernel()`/`leave_kernel()` out of
`rust_sync_el0_handler`, (2) delete the `reconcile_for_spsr` logic from the context-switch
path, (3) delete `KernelLock` + the BKL profiler + the entry/exit points, (4) run a
24-hour SMP=4 stress test.

Two disqualifying findings:

- **The motivating number was an artifact.** This document's Overview claimed
  "Scheduler/IRQ path holds ~70% of contended BKL time," and §16 of the VFS carve-out
  measured 66–73% — but with a profiler that credited a preempted thread's whole
  remaining syscall to `irq/sched`. Fixed and re-measured: **23.0%**, and ~21–23% fresh
  at HEAD after two further carve-outs. The BKL is *not* primarily a scheduler lock; the
  process-lifecycle syscalls (`execve` ~22%, `clone` ~10–13%) are the largest remaining
  holders, and they are precisely the ones with no inner lock.
- **Task 1 alone is unsound.** Removing BKL acquisition from syscall entry converts every
  BKL-dependent structure into a cross-core race in one step. Six such structures remain
  (audit §2), headed by the ~274-site `&'static mut Process` family whose entire safety
  argument is `with_irqs_disabled` — single-core mutual exclusion.

### 7.1 Prerequisites, cheapest-first

Deliberately **not** in contention rank — `execve` outranks all of 7a–7d but has no inner
lock, so converting it first would build on the thing that needs replacing. 7a–7c address
~30% of measured contention without touching the process table.

| | target | what it needs | audit ref |
|---|---|---|---|
| **7a** | alarm queue + `critical_section` | Real `Spinlock` on `ALARM_QUEUE`; per-core (not process-global) nesting counter, or drop the `critical_section` dep for `IrqGuard` + a Spinlock. Then IRQ 27 dispatch runs BKL-free. One file, smallest blast radius, biggest single tag. | §2.3 |
| **7b** | `ppoll` / `epoll_*` | Ordinary carve-out — `EPOLL_TABLE` exists. Plus move the BKL-held `smoltcp_net::poll()` at `poll.rs:925` into a dropped window (the §20 `netpoll_drain` precedent). | §3 |
| **7c** | already-carved residual | Measurement first, not code: `sys_openat` at ~10% for a *converted* syscall means the window starts too late or the re-acquire costs more than expected. | §3 |
| **7d** | `THREAD_CONTEXTS` + `Process::context` | Either prove `POOL`'s state machine already guarantees "not running on any CPU" (fix = corrected SAFETY comment + host test over the transitions), or add per-slot ownership. Same problem class as `Process::context`; solve together. | §2.2 |
| **7e** | process table | Two separable halves — see 7.2. | §2.1 |
| **7f** | wither the BKL | See 7.3. **Not** the original tasks 1–3. | §5 |

### 7.2 7e: the process table, in two halves

**Do not build `PROCESS_TABLE_LOCK`.** The original Phase 3 plan (§201–270 of this
document) called for `PROCESS_TABLE_LOCK` + per-process `ProcessLock`;
`BKL_PROCESS_CARVE_OUT.md` §9.2 rejected that shape as exactly the coarse lock the
playbook warns against — it would be held across millisecond-scale work.

- **Access half — a large refactor with a shipped precedent.** `lookup_process_shared`
  (`process/children.rs:341`) already replaced `&'static mut` exclusivity with `&self`
  methods plus an explicit `Process::as_lock`, and carries the whole M5b BKL-free
  page-fault path (17 sites). Extend that pattern and delete
  `lookup_process`/`current_process`.

  **But the accessor edit is the tail, not the work.** `Process` has ~40 fields and locks
  for about a third: `vm_lock` (`mmap_regions`, `memory.free_regions`), `as_lock` (page
  tables), `fault_mutex`, `fds`, `stdin`/`stdout`, `terminal_state`, `signal_actions`. The
  remaining ~25 are plain fields mutated through `&mut Process` — `name`, `state`,
  `context`, `parent_pid`, `pgid`/`tgid`, `brk`/`initial_brk`, `entry_point`, `memory`,
  `args`, `cwd`, `exited`, `exit_code`, `dynamic_page_tables`, `lazy_regions`,
  `thread_id`, `spawner_pid`, `box_id`, `delegate_pid`, `clear_child_tid`,
  `robust_list_*`, `signal_mask`, `address_space`. So the real sequence is: **group the
  fields by access pattern → give each group a lock or prove it single-writer → then
  convert the sites.** This half proceeds with the BKL still in place and is worth doing
  on its own merits.

  Current production surface: `current_process()` 142, bare `lookup_process` ~124,
  `get_process_ptr` 8 — ~274 sites, against only **6** uses of the safe
  `with_process(pid, f)` API.

- **Free half — the one genuinely undesigned piece.** `unregister_process`
  (`table.rs:63`) nulls the slot and drops the `Box`, running `Process::drop` →
  `UserAddressSpace::drop`. No inner lock covers this, and it is not only self-teardown:
  peer cores free *other* PIDs' `Process` at `process/mod.rs:1116` (sibling teardown),
  `:1209` (box kill) and `:241` (`kill_process`). `lookup_process`'s stated safety
  argument ("a process can't be freed during a syscall by its own thread") covers the
  self case only. Needs deferred reclamation: epoch/RCU (Phase 8 already floats RCU), or
  the time-cooldown pattern `reclaim_terminated_slots` already uses for thread slots.

### 7.3 7f: don't remove the BKL — invert its default

This replaces original tasks 1–3, and it is the part of this replan that changes the risk
profile rather than just the ordering.

**The problem with a big-bang removal** is that the default inverts. Today an un-audited
EL1 path is *safe by default* — the BKL serializes it whether anyone reasoned about it or
not. After removal it is a *race by default*. You cannot grep for a missing lock, and the
failure mode is silent corruption under load, not a panic. Every bug this campaign found
was caught by a digest mismatch or a wedge — i.e. after the fact.

**Instead:** change `rust_sync_el0_handler` from *"always acquire"* to *"acquire unless
this syscall is on the converted list."* Seed the list **empty** — byte-identical
behaviour, a no-op commit — then move syscalls across one at a time under the existing A/B
+ digest discipline.

```rust
// Sketch. The list is the single source of truth for "who no longer needs the BKL".
extern "C" fn rust_sync_el0_handler(frame: *mut UserTrapFrame, esr: u64, far: u64) -> u64 {
    let bkl_free = syscall_is_converted(syscall_nr_of(frame, esr));
    if !bkl_free { akuma_exec::bkl::enter_kernel(); }
    let ret = rust_sync_el0_handler_inner(frame, esr, far);
    if !bkl_free { akuma_exec::bkl::leave_kernel(); }
    ret
}
```

Why this is the better shape:

- **Bisectable.** A regression names one syscall, not "the removal."
- **Keeps the per-syscall kill switch.** Every phase here leaned on one
  (`vfs_bkl_drop_enabled`, `mm_bkl_drop_enabled`, `drivers_bkl_drop_enabled`, …). A
  big-bang removal discards that capability exactly when it is most needed.
- **The BKL withers instead of being deleted.** When the un-converted list is empty,
  `KernelLock`, `reconcile_for_spsr`, the dropped-window ledger and all five guards are
  *provably* dead code, and deleting them is bookkeeping rather than a behavioural
  change. Original tasks 2–3 stop being a risky step and become cleanup.
- **It collapses the guards.** The guards currently mark "BKL-free is permitted *here*";
  an opt-in list expresses the same thing per syscall, so the ledger + guard machinery
  can be removed at the end instead of carefully preserved through the transition.
- **Reconciliation must survive until the list is complete.** `reconcile_for_spsr` and the
  dropped-window ledger stay live for the whole traversal — a converted syscall is
  precisely a permanently-open dropped window, so the ledger's invariant is what makes the
  mixed state safe. Deleting it early is the one way to get this wrong.

**Remaining conversion surface for the traversal** (nothing measured above the noise
floor, so there is no attribution signal to guide *or validate* these — the same blind
spot `BKL_MM_CARVE_OUT.md` §5 flagged for Phase 5, which is the phase that found two real
locking gaps): 14 syscall families with zero carve-out — `poll`, `proc`, `signal`,
`sync`/futex, `term`, `pipe`, `msgqueue`, `eventfd`, `timerfd`, `aio`, `container`,
`time`, `pidfd`, `log` — plus ~13 leftover `fs` syscalls (`dup`, `close`, `fcntl`,
`fchmod`, `fallocate`/`ftruncate`/`truncate`, `faccessat2`, `symlinkat`, `linkat`,
`readlinkat`, `chdir`/`fchdir`, `fstatfs`).

### 7.3a 7g: audit which locks can become atomics — before the BKL is deleted

**Ordering: after the traversal, before the deletion.** Not an optimization pass for Phase
8, and not something to do early either. It has to come *after* 7f's traversal because
that is what enumerates the locks a BKL-free window can actually reach, and *before* the
deletion because a lock removed outright is one fewer entry that has to keep its
discipline forever.

**The observation this comes from.** Every "make this hold safe" fix in this campaign has
been a *discipline* fix — mask IRQs, shorten the span, hoist the user copy out. Discipline
is a standing obligation: it has to be re-derived by every future reader and re-checked at
every new call site, and `locking.md`'s "Correctness rules learned the hard way" list is
the record of what happens when it lapses. A lock that becomes a plain atomic stops being
an obligation at all. It cannot AB-BA against the BKL, cannot be held across a blocking
wait, cannot be taken from an IRQ epilogue, and drops off the load-bearing inventory.

**The concrete precedent.** Phase 7f tranche 3 hit this on `UTC_OFFSET_US`
(`src/timer.rs`), a `Spinlock<Option<u64>>` reachable BKL-free through
`futex(FUTEX_WAIT_BITSET|CLOCK_REALTIME)`. The obvious fix was to mask IRQs around its two
sites. The better fix — the one that landed — was to notice it guards **one scalar with no
other state published alongside it** and make it an `AtomicU64` with a sentinel for the
old `None`. Two loads became one, the AB-BA became unrepresentable, and the entry left the
inventory. Nothing in the campaign's process would have surfaced that; it was noticed by
asking the question directly.

**The audit's shape.** Walk the load-bearing inventory in
[`../reference/subsystems/locking.md`](../reference/subsystems/locking.md) and classify
each entry:

| class | test | action |
|---|---|---|
| single scalar, no companion state | does anything else have to change atomically with it? | replace with an atomic + sentinel (the `UTC_OFFSET_US` shape) |
| flag / counter / generation number | is it only ever read to decide, never read-then-written as a unit? | atomic; use CAS where the read-then-write *is* the unit |
| small fixed struct, one writer | is there exactly one writer, or is the writer already serialized elsewhere? | seqlock or per-writer atomics; **write the ownership proof down** — the phase's own §7.2 lesson |
| collection (`BTreeMap`, `Vec`, ring) | — | keep the lock; atomics do not apply. `FUTEX_WAITERS`, `EPOLL_TABLE`, `PIPES` are all this class |
| anything held across I/O or a blocking wait | — | keep the lock; the problem is the span, not the primitive |

**Do not convert a lock to atomics to make it "faster."** The reason is correctness
surface, not cycles — an atomic that replaces a lock guarding two related fields trades a
contention problem for a torn-read problem, which is strictly worse and much harder to
see. The `getcwd` rejection in
[`BKL_PHASE7F_OPTOUT_LIST.md`](BKL_PHASE7F_OPTOUT_LIST.md) §4.5 is the same mistake in the
opposite direction: `proc.cwd` is a `String`, so no amount of atomics makes an unlocked
read of it safe.

**Deliverable:** a table in the reference doc marking each inventory entry
converted / keep-the-lock / needs-an-ownership-proof, so the entries that survive to the
post-BKL world are ones someone decided to keep rather than ones nobody re-examined.

### Deliverables
- [ ] **7a** `ALARM_QUEUE` locked, `critical_section` per-core, IRQ 27 dispatch BKL-free
- [ ] **7b** `ppoll`/`epoll_*` carved; `smoltcp_net::poll()` in `sys_ppoll` BKL-free
- [ ] **7c** carved-residual re-measured; `sys_openat`'s ~10% explained
- [ ] **7d** `THREAD_CONTEXTS` + `Process::context` ownership proven or locked
- [ ] **7e** `Process` fields grouped and locked; ~274 accessor sites converted;
      `lookup_process`/`current_process` deleted; deferred reclamation for the free path
- [ ] **7f** per-syscall opt-in list landed empty; all families traversed
- [ ] **7g** every load-bearing lock classified atomic-able / keep / needs-proof (§7.3a);
      the single-scalar ones converted **before** the BKL infrastructure is deleted
- [ ] BKL infrastructure (`KernelLock`, `reconcile_for_spsr`, the ledger, the five guards)
      deleted as dead code
- [ ] Extended SMP=4 stability run (the original "24 hours"; see §7.4 on the bar)

### 7.4 Success criteria

- Every item in audit §2 has a real lock or a written, tested ownership proof —
  **this, not "the BKL is gone," is the phase's actual goal**
- The opt-in list is empty, and `KernelLock`/`reconcile_for_spsr`/the ledger/the five
  guards are deleted as unreachable
- **Every surviving lock in the load-bearing inventory was deliberately kept** — 7g
  (§7.3a) classified each one, the single-scalar entries became atomics, and the rest
  carry a written reason. The post-BKL inventory should be the locks someone chose, not
  the ones nobody re-read.
- Sustained SMP=4 load with **0 `[BKL] stuck`, 0 `RECOVERED` (the new
  `kernel_lock_recoveries()` tripwire), 0 PANIC/WILD/SPURIOUS, 0 stale dropped-window
  heals, and exact digests** — the campaign's standing bar, which is stronger evidence
  than wall-clock uptime alone
- **The rustc/cargo scaling curve improves** (SMP=1 `-j1` → SMP=4 `-j4` speedup). This is
  the throughput metric the campaign has been missing: `contention_spins` is a proxy,
  digests are correctness-only, and the existing llama.cpp tok/s comparison is
  compute/mmap-bound and barely touches process lifecycle. A Rust build hammers exactly
  the un-carved holders (`execve`, `clone`, `openat`). Baseline it **before** starting 7a
  — harness + prompt in
  [`../runbooks/bkl-phase7-workplan.md`](../runbooks/bkl-phase7-workplan.md), results in
  `BKL_RUSTC_SCALING_BASELINE.md`. If that curve turns out to be flat for reasons other
  than the BKL (e.g. ext2-read-bound), it re-orders this phase — which is exactly why it
  goes first.
- No regression on the single-core default build (unaffected by construction: every BKL
  entry point is a `cfg(kernel_smp_shared)` no-op shim)

---

## Phase 8: Performance Optimization

**Week 13-14** - Optimize lock granularity after BKL removal

### Tasks

1. **Profile New Lock Contention**:

   **Use existing profiler**:
   - Identify new contention points
   - Measure lock hold times
   - Find hot paths

2. **Optimize Hot Paths**:

   **Consider RCU for read-mostly data**:
   ```rust
   // Example: process table lookups
   pub fn lookup_process_rcu(pid: Pid) -> Option<Arc<Process>> {
       // RCU read-side critical section
       rcu_read_lock();
       let proc = PROCESS_TABLE.get(pid);
       rcu_read_unlock();
       proc
   }
   ```

   **Lock sharding for highly contended structures**:
   ```rust
   // Example: socket table sharding
   pub static SOCKET_TABLE_SHARDS: [Spinlock<Vec<Socket>>; 8] = 
       [const { SpinLock::new(Vec::new()) }; 8];
   ```

   **Sequence locks for statistics**:
   ```rust
   pub static NET_STATS_SEQLOCK: SeqLock<NetStats> = SeqLock::new(NetStats::default());
   ```

3. **Benchmark Improvements**:

   **Measure throughput gains**:
   - Network throughput (iperf, netperf)
   - File I/O performance (fio, bonnie++)
   - Context switch overhead
   - Memory allocation latency

### Deliverables
- [ ] Lock contention analysis
- [ ] RCU implementation (if needed)
- [ ] Lock sharding (if needed)
- [ ] Sequence locks (if needed)
- [ ] Performance benchmarks

### Success Criteria
- Measurable improvement in multi-core throughput
- No regression in single-core performance
- Lock contention within acceptable bounds
- Documented performance gains

---

## Risk Assessment & Mitigation

### High-Risk Areas

1. **Process Table Operations** (218+ sites)
   - **Risk**: Complex interactions, hard to audit completely
   - **Mitigation**: 
     - Extensive auditing and testing
     - Gradual migration with runtime checks
     - Automated lock usage verification

2. **Lock Hierarchy Deadlocks**
   - **Risk**: New locks can introduce deadlock patterns
   - **Mitigation**:
     - Strict ordering rules with enforcement
     - Lock dependency tracking
     - Runtime deadlock detection

3. **Context Switch Correctness**
   - **Risk**: BKL reconciliation is complex
   - **Mitigation**:
     - Keep BKL until all subsystems verified
     - Extensive context switch testing
     - Special attention to EL transitions

4. **Blocking Operation BKL Drops**
   - **Risk**: Drops may be incomplete or incorrect
   - **Mitigation**:
     - Audit all blocking operations
     - Ensure lock ordering maintained
     - Stress test blocking paths

### Testing Strategy

1. **Unit Tests**:
   - Lock type tests
   - Lock ordering tests
   - Deadlock detection tests

2. **Integration Tests**:
   - Multi-core stress tests (`forktest`, network benchmarks)
   - Lock dependency verification
   - BKL drop verification

3. **Stress Tests**:
   - Long-running stability (24+ hours)
   - Mixed workload tests
   - Contention stress tests

4. **Performance Tests**:
   - Throughput benchmarks
   - Latency measurements
   - Scalability tests (SMP=1,2,4,8)

### Success Criteria

- [ ] BKL completely removed
- [ ] No regression in single-core performance
- [ ] Measurable improvement in multi-core throughput
- [ ] Stable operation at SMP=4 under sustained load
- [ ] No deadlocks or livelocks in testing
- [ ] All existing tests pass

---

## Progress Tracking

### Phase 0 ✅ COMPLETE
- [x] Network audit complete
- [x] Lock requirements identified
- [x] BKL usage patterns documented
- [x] Deadlock risks assessed
- [x] Phase 1 tasks planned

### Phase 1 - Network Lock Foundation ✅ COMPLETE
- [x] Lock infrastructure created (`crates/akuma-net/src/locks.rs`)
- [x] Profiling hooks integrated (NetworkLockStats, tracking functions)
- [x] Lock hierarchy documented (in code comments)
- [x] Testing framework created (unit tests in locks.rs)

**Status**: Lock scaffolding compiles successfully, all tests pass on host

**Completed Deliverables**:
- Global `NETWORK_LOCK` and `SOCKET_TABLE_LOCK` spinlocks
- Lock ordering enforcement with panic on violations
- Profiling statistics (contention counts, spins, violations)
- Lock holder tracking for watchdog monitoring
- Comprehensive unit tests for lock ordering
- Integration with existing akuma-net module structure

### Phase 2 - Network BKL-Free Path ✅ DEFAULT-ON for `smp-shared` (2026-07-24)
- [x] BKL-free syscall entry points — all 15 smoltcp net syscalls, plus the
      `read(2)`/`write(2)` Socket arms (sshd's hot path)
- [ ] Updated IRQ handler (`poll()` still runs under whatever BKL state the caller has)
- [x] Blocking operations compatible (no inner spinlock held across `blocking_relax`)
- [x] Boots + SSH login verified at SMP=2 (no wedge, no abort)
- [x] Stress-tested at SMP=4: real BitTorrent swarm (aria2c, 8 peers, ~3.2 MiB/s,
      83 MiB) + ssh hammer — 0 wedges, 0 stream corruption, 0 aborts
- [x] Enabled by default: the bin `smp-shared` feature now includes
      `no-bkl-network` (default single-core builds verified byte-identical;
      SMP=4 boot suite green, same counters as HEAD). Remove it from the
      `smp-shared` feature list in Cargo.toml to A/B against the BKL-held path.
- [ ] A/B contention measurement (BKL wait-time with/without the drop)

**Design note — why the implementation diverges from the pseudo-code above.**
The Phase-2 sketch wrapped each syscall in a *new coarse* `NETWORK_LOCK`
(`crates/akuma-net/src/locks.rs`). That does not work for the blocking syscalls:
`accept`/`connect`/`recv`/`dns_query` yield via `blocking_relax()` (which drops the
BKL so a peer can drive the stack), and holding a coarse lock across that yield would
serialize *all* network syscalls behind one blocked socket — strictly worse than the
BKL, which IS dropped during the wait.

Instead, the `no-bkl-network` feature (bin feature → `cfg(kernel_no_bkl_network)`,
only active with `smp-shared`) makes each net syscall **drop the BKL for its whole
duration** and rely on the *already-existing* fine-grained locks for cross-core
mutual exclusion:
- the per-process fd table — `SharedFdTable`'s three spinlocks
  (`crates/akuma-exec/src/process/fd.rs`),
- the socket descriptor table — `akuma_net::socket::SOCKET_TABLE`,
- the network stack — `akuma_net::smoltcp_net::NETWORK` (held under a `PreemptGuard`),
- and the heap (`TALC` spinlock) for bounce-buffer alloc/free.

All four are real spinlocks acquired under `with_irqs_disabled` / `PreemptGuard` and are
correct across cores *without* the BKL — the BKL was redundant for them. Blocking waits
hold none of the inner spinlocks, so two cores can block in network syscalls at once.
The drop/re-acquire is an RAII guard (`NetBklGuard` in `src/syscall/net.rs`); re-acquire
on drop keeps the syscall wrapper's single `leave_kernel` balanced, and a nested
IRQ/fault re-taking the BKL meanwhile is harmless because `enter_kernel` is idempotent —
the same contract the `exec_dropped_bkl` and file-fault BKL-drop paths already use.
Host test `kernel_lock_midexcursion_drop_reacquire_stays_balanced` (sync.rs) proves the
ticket accounting stays balanced under this pattern (incl. peer contention in the window).

The coarse `NETWORK_LOCK` + ordering-enforcement scaffolding in `locks.rs` is **not**
wired into the hot path (its global `HELD_LOCKS` bitmap is per-process, not per-core, so
it is host-test/documentation scaffolding only); the fine-grained locks above are the
real "network lock instead of BKL".

**Trade-off to watch:** each net syscall now takes the BKL twice (drop + re-acquire),
roughly doubling ticket churn on the network hot path. A verified SMP=2 SSH session saw
6 `[BKL] RECOVERED` self-heals (0 `stuck`, 0 aborts) — the pre-existing ticket-leak
self-heal doing its job under the extra churn. A/B contention measurement + confirming
this doesn't raise the recovery rate materially is the remaining Phase-2 work.

**2026-07-24 SMP=4 attribution + hardening (this session).** Sustained load
(`apk add`, then an aria2c torrent swarm) at SMP=4 showed two bad symptoms with
`no-bkl-network`; an identical run on a BASELINE build (`devbox-smoltcp,no-tests`,
no feature) reproduced BOTH, so neither is a feature regression:

1. *SSH stream corruption* (`Bad packet length 0x17030300` = a TLS record header
   inside the SSH stream) — cross-socket data injection from a missing socket
   refcount: `clone_deep_for_fork` copied `FileDescriptor::Socket(idx)` with no
   ref bump while every close path (`close_all` at exit, exec's cloexec sweep,
   `close(2)`, `dup2`-replace) destroyed the socket unconditionally, so the first
   closer freed the smoltcp handle under every other fd; the reused handle then
   spliced two live TCP streams. FIXED: `KernelSocket::refs` +
   `socket_clone_ref()` (bumped by fork/dup/dup2/F_DUPFD, mirroring
   `pipe_clone_ref`), refcount-aware `remove_socket`, boot self-test
   `test_socket_refcount_survives_first_close`.
2. *Hard BKL wedge* (`[BKL] stuck`, owner frozen, all cores IRQ-masked in
   `acquire`, guest timer starved → watchdog mislabels it "host sleep/wake") —
   TWO distinct shapes, both fixed:
   - `wait_until` (socket.rs) only called `blocking_relax()` when a poll round
     made NO progress; under constant swarm traffic every round reports
     progress, so a blocking waiter (accept has NO timeout) busy-spun holding
     the BKL forever. FIXED: after 4 fruitless progress-rounds the waiter
     relaxes anyway, bounding the hold.
   - **Pipe SIGPIPE self-deadlock** (lldb-root-caused live, 100% reproducible
     via `aria2c … | head -1`): `pipe_write` raised `send_sigpipe()` while
     HOLDING the global `PIPES` spinlock (IRQs masked). Default disposition
     terminates the writer INLINE — tkill → `sys_exit_group` → `close_all` →
     `pipe_close_write` → re-acquire `PIPES` → the core self-deadlocks on its
     own lock while holding the BKL; every peer core piles into
     `KernelLock::acquire`. FIXED: the signal is raised after the locked
     section (src/syscall/pipe.rs); boot regression test
     `test_sigpipe_terminate_no_deadlock` (`yes | head -n 1`). This is the
     probable root of the long-standing "SMP=4 hard wedge family".

**Feature-specific latent deadlock found by audit + closed.** In the dropped-BKL
window a nested IRQ runs an unconditional `enter_kernel()` hard-spin
(exceptions.rs `rust_irq_handler_with_sp`, the "preempted EL1" path). If the
window holds an inner spinlock the current BKL owner wants — the async-main
poller spins on `NETWORK` near-constantly — the two cores deadlock AB-BA.
Hardening shipped under the feature:
- `PreemptGuard` (guards `NETWORK`/`SOCKET_TABLE`) now also MASKS local IRQs for
  the hold (`akuma-net/no-bkl-network` feature, forwarded from the bin crate), so
  the window is nest-free;
- `epoll_on_fd_drained` takes `EPOLL_TABLE` under `with_irqs_disabled`
  (unconditionally — harmless, and exec's BKL-drop window reaches epoll too);
- pipe-backed `UnixSocket` fds are routed BEFORE the guard in
  sendto/recvfrom/sendmsg/recvmsg (the pipe/fs paths must not run BKL-free);
- heap (`talc_alloc/dealloc/realloc`), fd table, PMM, and `vm_with_regions` were
  audited and already run under `with_irqs_disabled` — no change needed.
Residual (shared with the PRE-EXISTING `exec_dropped_bkl` / file-fault windows,
not new): page-table frame vecs and the COW fault map are bare spinlocks touched
from BKL-free windows; same AB-BA shape at much lower duty. Tracked as the
likely root of the long-standing "SMP=4 hard wedge family".

Also extended coverage: `read(2)`/`write(2)` on Socket fds (sshd's actual hot
path — its PSTATS shows `read=`/`write=`, not recvfrom) now take `NetBklGuard`
too; both use kernel bounce buffers so no user-memory fault can occur inside the
socket locks.

### Phase 3 - Process Management Locks — PARTIALLY SHIPPED (2026-07-31)

See **[BKL_PROCESS_CARVE_OUT.md](BKL_PROCESS_CARVE_OUT.md)**: §§1–8 are the audit
(2026-07-31, morning), §9 is the carve-out that landed the same day.

Headline, in two parts.

**The audit said no carve-out was possible.** The §16.3 attribution's top
unconverted holder (`clone` 22.5%) was walked step-by-step against the VFS
playbook, and every step consuming significant BKL-held time appeared to touch
state with no inner lock (`THREAD_CONTEXTS` `UnsafeCell`, the process table's
`&'static mut Process`, the parent's live page tables during CoW demote).

**One of those three was wrong.** The parent's page tables *do* have an inner
lock — `Process::as_lock` — and the CoW fault handler was already editing the
same PTEs BKL-free under it (eleven `AsLockHold::new(&owner.as_lock)` sites in
`src/exceptions.rs`, inside the `fault_bkl_drop_enabled()` window). `fork_process`
was simply the one page-table mutator in the kernel that never took it. Once it
does, step 4 fits the playbook's model exactly: the BKL is redundant because the
state already carries a finer lock. That is `no-bkl-process`.

The audit's other two findings stand: `THREAD_CONTEXTS` and the process table
have no inner lock, so steps 5–8 (context capture, thread spawn,
`register_process` + `mark_thread_ready`) are **still fully BKL-held**, as is
`replace_image`'s destructive window. The carve-out is step 4 only.

**The process-table lock sketch below (step 1) was NOT needed and was NOT
built.** The page-copy window never touches the process table: the child is
private stack-local state, the parent fields it reads are immutable during a
syscall, and the one table walk it did contain (the `for_each_process` sibling
mmap scan) was hoisted to before the window. Building a `PROCESS_TABLE_LOCK`
held across a milliseconds-long copy would have been the "new coarse lock"
anti-pattern; refactoring 218+ `lookup_process` sites onto `with_process(pid, f)`
remains worth doing on its own merits but is a separate task, not a prerequisite
for this. Same outcome as Phases 2 and 4: the sketched lock hierarchy turned out
to be unnecessary, and the work was a BKL-drop guard plus inner-lock discipline.

The `LifecycleGuard` sketch (step 4 below) **was built and is active**
(`crates/akuma-exec/src/process/lifecycle.rs`); it complements the BKL rather
than replacing it, and it is what lets the carve-out's chunked `as_lock` holds
not worry about being preempted mid-copy.

- [x] Lifecycle guards implemented (active `disable_preemption`, not a no-op)
- [x] ~~Process table locking designed~~ — **unnecessary for this carve-out** (see above)
- [x] ~~`lookup_process()` refactored (218+ sites)~~ — **not a prerequisite**; still
      worth doing independently
- [ ] Scheduler updated
- [x] fork-corruption bug FIXED and VALIDATED (2026-07-31: SMP=4 fork-hammer, 3 boots × 10 rounds, 0 faults)
- [x] `no-bkl-process` feature + `cfg(kernel_no_bkl_process)` + runtime A/B toggle
- [x] `fork_process` CoW share/demote pass converted: chunked leader-`as_lock` holds
      (`FORK_AS_CHUNK_PAGES = 64`, IRQ-masked), demote merged into the share pass so
      PTE-read + `cow_ref_inc` + demote + range-flush are atomic per page against a
      peer's CoW fault
- [x] Boot self-test `test_fork_bkl_drop` (ledger balance + latching + real
      share/demote across chunk boundaries, both toggle positions) passing at SMP=2
- [x] Fork-hammer at SMP=4, 3 boots × 10 rounds: 0 faults, 0 partial-output
      children, 0 `[BKL] stuck` — matched by the BKL-held baseline
- [x] Contention A/B under `bkl-profile` at SMP=4: **`clone` 19.5% → 2.5%**
      (23.9M → 2.8M spins, 8.6×; #2 holder → minor), total workload spins −9%,
      6/6 digests exact both sides
- [x] Incidental: `FORK_IN_PROGRESS` no longer leaks on the OOM early-return (RAII)
- [x] Promoted to DEFAULT-ON for `smp-shared` (2026-07-31), same as net and vfs
- [ ] Not carved: steps 5–8, `replace_image`, the eager-copy (unreachable) fork branch

### Phase 4 - VFS and Filesystem Locks — PARTIALLY SHIPPED (2026-07-25)

See **[BKL_VFS_CARVE_OUT.md](BKL_VFS_CARVE_OUT.md)** for what actually shipped and why it
diverges from the sketch above.

Headline: the new VFS lock hierarchy below was **not built and is not needed** — every piece
of VFS state already carries a fine-grained lock, so (exactly as with net in Phase 2) the
work is a BKL-drop guard plus inner-lock hardening. No new locks were introduced.

- [x] ~~VFS lock hierarchy created~~ — unnecessary; existing locks suffice (see that doc §1)
- [x] ~~Per-filesystem locks~~ / ~~Per-inode locks~~ — unnecessary, same reason
- [x] `no-bkl-vfs` feature + `cfg(kernel_no_bkl_vfs)` + runtime A/B toggle
- [x] `PreemptGuard` lifted from `akuma-net` to `akuma-exec::sync`
- [x] ext2 `state` guard hardening (IRQ-masked **per try**, not across the wait — the sketch
      had this wrong; see that doc §3.1)
- [x] Block I/O path — covered transitively, no change needed (that doc §3.2)
- [x] Phase 2a: read-path syscalls converted (9 syscalls)
- [x] Boot self-test + host tests passing at SMP=2
- [x] `[BKL] stuck` regression root-caused + FIXED (that doc §9): the IRQ-epilogue
      reconcile was converting every dropped window into a BKL-held run at the first timer
      tick; fixed with a per-thread dropped-window ledger consulted at every eret. Applies
      to ALL droppers (vfs, net, exec, file-fault), 0 stuck in the re-run regimen.
- [~] Phase 2b/2c/2d: `openat`/`close`/`dup`/`fcntl`, the mutating syscalls, `chdir` family.
      **Phase 2c first target `unlinkat` DONE 2026-07-30** (carve-out doc §12): the §11.6 72.6%
      attribution culprit dropped to *absent*, SMP=4 `[BKL] stuck` 598–704 → 0 on the identical
      regimen (6/6 digests exact). Remaining 2c list is now evidence-led — §12 names `openat`
      (Phase 2b, 36.6%) as the next holder, not a 2c syscall. (Original urgency data point: one
      `rm` of a 735 MB file = ~40 s BKL hold, 274 stuck warns — that hold is now gone.)
- [x] Phase 2e: eager file-backed `mmap` arm — fill-before-install inside a
      `VfsBklGuard` window + windowed `resolve_inode` (that doc §10). Verified with the
      `userspace/forktest/c_stress` mmap stress tools + llama.cpp mmap model-load
      end-to-end; the llama leg exposed and fixed a pre-existing
      `madvise(MADV_WILLNEED)` bug that zero-filled file-backed lazy pages (§10.3).
- [x] A contention signal — `bkl-profile` feature + `src/bkl_profile.rs` land the per-tag
      BKL-hold attribution dump (`[BKLPROF]` delta histogram every 10 s). **Collected at SMP=4
      (that doc §11.6): `unlinkat` 72.6%, `irq/sched` 26.9%, `openat` 0.3%, everything else
      <0.2% — `read`/`write` (the Phase 2a conversions) contribute ~nothing.** So Phase 2a is
      done, **Phase 2c is the whole remaining VFS win and its first target is specifically
      `unlinkat`**, and the Phase 0 "scheduler/IRQ ≈70%" estimate is wrong for this workload
      (it is 27%) — Phase 3 keeps its place but does not jump the queue.
- [x] SMP=4 stress — FIRST RUN 2026-07-29 (that doc §11). Result: no wedge, no corruption,
      0 PANIC/WILD/SPURIOUS — but **~600–700 `[BKL] stuck` per run** where SMP=2 produced 0,
      so the §9 ledger fix bounded holds at SMP=2 only. Attribution (§11.6) named `unlinkat`
      (72.6%); the "Phase 2c must not land before those holds are attributed" gate was met, 2c's
      `unlinkat` landed (§12, 2026-07-30) and **SMP=4 stuck is now 0**. Remaining §11 blockers
      stand: thread-slot reclaim FIXED (§11.7), `wait`/SIGCHLD still open (§11.3).
      Two pre-existing blockers found by the campaign (neither a carve-out regression, both
      reproduce at SMP=1):
      **(a) thread-slot reclamation starved under load** (p50 24 s / max 192 s against a 10 ms
      cooldown → `fork` stalled for minutes, then failed with `No available user threads` while
      GBs were free) — **FIXED** (that doc §11.7): reclaim-on-demand at both spawn sites plus a
      100 ms collector in the async-main loop, keeping the cooldown and dropping only the
      "thread 0 collects" gate. Same regimen went from unfinished-in-1800 s to 152 s; boot
      self-test `test_thread_slot_reclaim_on_spawn` added.
      **(b) the shell's `wait` builtin never returns** — the kernel delivers no SIGCHLD at all
      (`grep -r SIGCHLD` finds only clone-flag parsing). STILL OPEN (that doc §11.3); it makes
      `&`+`wait` unusable, so parallel shell workloads must join on sentinel files.
- [x] Combined net+VFS large-download I/O regimen (that doc §8, re-run post-fix §9.4)

### Phase 5 - Memory Management Locks — COMPLETE (2026-08-01, `no-bkl-mm`, default-on in `smp-shared`)

See **[BKL_MM_CARVE_OUT.md](BKL_MM_CARVE_OUT.md)** for the full writeup. Headline,
same shape as Phases 2/3/4: the sketched new locks below were **not built** —
`as_lock`/`vm_lock`/`LAZY_REGION_TABLE`/PMM already covered every mm syscall's state
except two real gaps (an unguarded `ProcessMemory::free_regions`, and `sys_mmap`'s
OOM-reclaim path missing an `as_lock` hold on its page-table writes), both closed by
reusing existing locks rather than adding new ones.

Unlike Phases 2–4, this phase was picked by the plan, not by attribution — no mm
syscall has ever measured as a significant BKL holder (`mmap` was 2.4% of the pool
before the `netpoll_drain` carve cut it 67%). So there is no before/after contention
number here the way `unlinkat` (72.6%→absent) or `netpoll_drain` (57.2%→absent) got
one — see that doc's §5.

- [x] ~~`as_lock` extended~~ — unnecessary; `Process::as_lock` already covers every
      mm-syscall PTE edit (same lock `fork_process`'s CoW pass and the fault handler
      already use BKL-free)
- [x] ~~PMM locks added~~ — unnecessary; `PMM`/`FRAME_TRACKER`/`COW_REFCOUNTS` already
      self-locked, never held across a yield
- [x] ~~TLB shootdown updated~~ — unnecessary; TLB flushes already happen inside the
      existing `as_lock` holds
- [x] Real gap found and fixed: `ProcessMemory::free_regions`/`alloc_mmap()` had no
      lock at all — folded under the existing `Process::vm_lock` via two new methods,
      `vm_alloc_mmap`/`vm_free_mmap`
- [x] Real gap found and fixed: `sys_mmap`'s OOM/reclaim sweep
      (`reclaim_clean_file_pages` → `try_evict_ro_page`) mutated page tables with no
      `as_lock` hold — fixed with a per-page (not per-sweep) hold
- [x] `no-bkl-mm` feature + `cfg(kernel_no_bkl_mm)` + runtime A/B toggle
      (`mm_bkl_drop_enabled`/`set_mm_bkl_drop_enabled`, latched at construction like
      `VfsBklGuard`)
- [x] `sys_mprotect`/`sys_madvise`/`sys_munmap`/`sys_mremap`/`sys_mmap` converted
- [x] Boot self-test `test_mm_bkl_drop` (ledger balance across early-error + real
      unmapped-VA paths + an mmap/munmap round trip + the kill switch), PASSED at
      SMP=2 and SMP=4 (real QEMU boots): 0 PANIC/WILD, 0 stale-ledger heals
- [x] Real-PTE-install correctness (what the boot self-test structurally can't cover
      — `map_user_page_no_flush` reads the live TTBR0_EL1) validated end-to-end with
      the same tools Phase 2e used: `mmap_stress`/`mmap_file`/`mmapsum`/`fpfault`/
      `neonfault` + `llama-bench`, all clean, matching or exceeding the original
      Phase 2e table
- [x] Contention regimen (`net4→read4→cp2→rm`, SMP=4): 6/6 digests exact, 0 stuck,
      total workload spins 47.3M → 42.6M (~10% cut) — but mm syscalls don't appear
      as named holders in this regimen either before or after, so the cut isn't
      attributable specifically to this carve (see the doc's §5)
- [x] **Promoted to `smp-shared`'s default bundle** (2026-08-01). The audit +
      boot-suite + stress-tool verification was accepted as sufficient evidence
      on its own, matching the bar `no-bkl-process` cleared.

### Phase 6 - Device Driver Locks
- [x] Device drivers audited (`BKL_DRIVERS_CARVE_OUT.md` §1 — most work already done by
      `no-bkl-vfs`/`no-bkl-network`; virtio-gpu does not exist in this codebase)
- [x] Per-driver locks: already present (`RNG_DEVICE`, `SOUND_DEVICE`, `FB_STATE`);
      `DriverBklGuard` added to drop the BKL around them
- [x] `sys_getrandom`, `/dev/urandom` read/pread, `/dev/dsp` write, `fb_init`/`fb_draw`/
      `fb_info` converted
- [x] Boot self-test `test_drivers_bkl_drop`; **promoted to `smp-shared` default**
      (2026-08-01)
- [~] IRQ handlers BKL-free — **deferred to Phase 7a**. All virtio devices are polling;
      the only IRQ handler is the timer (PPI 27), which is scheduler-coupled
      (`BKL_DRIVERS_CARVE_OUT.md` §2)

### Phase 7 - BKL Removal & Hardening — REPLANNED (see §7)
Audited 2026-08-01; the four items below were the original plan and are **not
executable** as written ([`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md)):
- [x] ~~BKL removed from syscall entry~~ → replaced by the per-syscall opt-in list (§7.3)
- [x] ~~Context switch path updated~~ → `reconcile_for_spsr` + the ledger must **survive**
      the traversal; deleting them early is the way to get this wrong
- [x] ~~BKL infrastructure deleted~~ → becomes dead-code cleanup once the list is empty
- [ ] Extended SMP=4 stability run (bar restated in §7.4 — signal counts + digests, not
      just uptime)

Replanned prerequisites:
- [ ] 7a alarm queue + `critical_section` (→ timer IRQ dispatch BKL-free)
- [ ] 7b `ppoll`/`epoll_*` carve-out
- [ ] 7c carved-residual re-measurement (`sys_openat` ~10%)
- [ ] 7d `THREAD_CONTEXTS` + `Process::context` ownership
- [ ] 7e process table: field grouping + locks, ~274 accessor sites, deferred reclamation
- [ ] 7f opt-in list landed empty, all 14 families + ~13 `fs` syscalls traversed,
      infrastructure deleted

Landed during the audit:
- [x] `KernelLock::acquire_no_ticket` ticket-accounting fix (`now_serving` was advancing
      without an allocation → `reticket-skipped` storms; 46 → 0 at SMP=4)
- [x] `sync::kernel_lock_recoveries()` counter + host test
      `kernel_lock_no_ticket_acquire_release_stays_balanced` + boot self-test
      `test_no_bkl_ticket_recoveries`

### Phase 8 - Performance Optimization
- [ ] Lock contention analyzed
- [ ] Optimizations implemented
- [ ] Benchmarks run
- [ ] Performance documented

---

## References

- **Current SMP Design**: `docs/reference/subsystems/smp-shared.md`
- **SMP Debugging**: `docs/runbooks/debug-smp.md`
- **BKL Implementation**: `crates/akuma-exec/src/sync.rs`
- **Network Code**: `crates/akuma-net/src/`
- **Progress Log**: `docs/archive/SMP_SHARED.md`

---

## Notes

- **Phased approach**: Each phase builds on the previous, can be stopped if issues arise
- **Backward compatibility**: Default build (single-core) unaffected — **including through
  Phase 7**, since every BKL entry point is a `cfg(kernel_smp_shared)` no-op shim. The
  original note said "until Phase 7"; the §7.3 opt-in list preserves that property.
- **Performance first**: Networking chosen first due to well-defined boundaries
- **Safety critical**: Extensive testing at each phase
- **Documentation heavy**: Lock ordering and rules well-documented to prevent deadlocks
- **Estimate honestly, or not at all** (added 2026-08-01): 7a–7d are each days. 7e's
  access half is roughly a week of mechanical work *after* the field-ownership design,
  which is the genuinely uncertain part. The un-carved-family traversal and the free-path
  reclamation should not be estimated before the field grouping exists.
- **Two premises this plan got wrong**, both by asserting instead of measuring, and worth
  re-reading before trusting any other number here: the Phase 0 "~70% scheduler/IRQ"
  estimate (§Overview), and Phase 3's "no inner lock exists" for the parent page tables —
  `as_lock` was already there and the fault handler was already using it
  (`BKL_PROCESS_CARVE_OUT.md` §9.1). Both cost a phase's worth of misdirection.

**Last Updated**: 2026-08-01 (Phase 7 replanned; Overview + Phase 6/7 checklists
corrected)  
**Status**: Phase 0 Complete, ready to begin Phase 1