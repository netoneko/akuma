# BKL Fine-Grained Locking Plan

**Status**: Phase 0 Complete - Network Audit Done  
**Strategy**: Phased Removal (eliminate BKL completely)  
**Target First**: Networking stack

---

## Overview

This plan breaks up the Big Kernel Lock (BKL) into fine-grained subsystem locks, eliminating it completely through a phased approach. The networking stack is targeted first because it has well-defined boundaries and some existing BKL-drop optimizations.

**Current State** (2026-07-24):
- Single fair FIFO `KernelLock` serializes all kernel (EL1) execution across cores
- Held "iff a core is in EL1", reconciled at EL transitions
- Scheduler/IRQ path holds ~70% of contended BKL time
- Networking has existing fine-grained locks but still requires BKL at syscall boundaries

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

## Phase 7: BKL Removal & Hardening

**Week 11-12** - Eliminate BKL entirely

### Tasks

1. **Remove BKL from Syscall Entry**:

   **Update `rust_sync_el0_handler`**:
   ```rust
   extern "C" fn rust_sync_el0_handler(frame: *mut UserTrapFrame, esr: u64, far: u64) -> u64 {
       // NO BKL acquisition
       let result = rust_sync_el0_handler_inner(frame, esr, far);
       result
   }
   ```

2. **Update Context Switch Path**:

   **Remove BKL reconciliation**:
   - Clean up SPSR handling
   - Remove `reconcile_for_spsr` logic
   - Simplify EL transitions

3. **Remove BKL Infrastructure**:

   **Delete**:
   - `KernelLock` implementation (`crates/akuma-exec/src/sync.rs`)
   - BKL profiler (or migrate to general lock profiler)
   - BKL entry/exit points

4. **Stress Testing**:

   **Extended testing**:
   ```bash
   # 24-hour SMP=4 stress test
   SMP=4 timeout 86400 cargo run --profile release-smp-shared --features smp-shared
   ```

### Deliverables
- [ ] BKL removed from syscall entry
- [ ] Context switch path updated
- [ ] BKL infrastructure deleted
- [ ] 24-hour stability tests passing

### Success Criteria
- BKL completely removed
- System stable at SMP=4 under sustained load
- No deadlocks or livelocks
- Performance improved over BKL baseline

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

### Phase 2 - Network BKL-Free Path
- [ ] BKL-free syscall entry points
- [ ] Updated IRQ handler
- [ ] Blocking operations compatible
- [ ] Stress tests passing
- [ ] Performance measured

### Phase 3 - Process Management Locks
- [ ] Process table locking designed
- [ ] `lookup_process()` refactored
- [ ] Scheduler updated
- [ ] Lifecycle guards implemented
- [ ] Tests passing

### Phase 4 - VFS and Filesystem Locks
- [ ] VFS lock hierarchy created
- [ ] Per-filesystem locks
- [ ] Per-inode locks
- [ ] Block I/O path updated
- [ ] Tests passing

### Phase 5 - Memory Management Locks
- [ ] `as_lock` extended
- [ ] PMM locks added
- [ ] TLB shootdown updated
- [ ] Tests passing

### Phase 6 - Device Driver Locks
- [ ] Device drivers audited
- [ ] Per-driver locks added
- [ ] IRQ handlers BKL-free
- [ ] Tests passing

### Phase 7 - BKL Removal & Hardening
- [ ] BKL removed from syscall entry
- [ ] Context switch path updated
- [ ] BKL infrastructure deleted
- [ ] 24-hour tests passing

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
- **Backward compatibility**: Default build (single-core) unaffected until Phase 7
- **Performance first**: Networking chosen first due to well-defined boundaries
- **Safety critical**: Extensive testing at each phase
- **Documentation heavy**: Lock ordering and rules well-documented to prevent deadlocks

**Last Updated**: 2026-07-24  
**Status**: Phase 0 Complete, ready to begin Phase 1