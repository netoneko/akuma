# Akuma bugfix audit — itemized list

Counting rule: one item per distinct, dated/named bug confirmed fixed/resolved/implemented.
Plans/proposals with no landed fix, duplicate mentions of the same fix, and narrative
table-of-contents sections are excluded (noted per-file where relevant). Subsystem tags
are assigned per *file* (the dominant subsystem of that investigation doc), not per bullet —
a handful of grab-bag docs (e.g. `AKUMA_SELF_HOSTING.md`, `KERNEL_SPLIT_BUGS.md`) mix bugs
from several subsystems under one write-up.

## Statistics

- **Total distinct fixes counted:** 471
- **Docs contributing at least one fix:** 126
- **Subsystem categories:** 15

| Subsystem | Fixes | % | Docs |
|---|---:|---:|---:|
| Syscall / ABI Compatibility Audits | 108 | 22.9% | 11 |
| Memory & Virtual Memory | 85 | 18.0% | 21 |
| Scheduler & Process Management | 71 | 15.1% | 15 |
| SMP & Locking | 49 | 10.4% | 14 |
| Networking | 30 | 6.4% | 12 |
| Userspace Apps & Libraries | 30 | 6.4% | 15 |
| Rump Kernel & Syscall Proxy | 24 | 5.1% | 5 |
| Toolchain & Self-Hosting | 22 | 4.7% | 3 |
| SSH | 12 | 2.5% | 10 |
| VFS & Filesystem | 10 | 2.1% | 7 |
| Boot & Drivers | 9 | 1.9% | 5 |
| Signals & Exceptions | 9 | 1.9% | 3 |
| Misc / Cross-cutting | 8 | 1.7% | 1 |
| Console & Terminal | 3 | 0.6% | 3 |
| Containers | 1 | 0.2% | 1 |
| **Total** | **471** | **100.0%** | **126** |

**Largest single write-ups** (most distinct fixes documented in one file):

- 44 — `docs/archive/GOLANG_MISSING_SYSCALLS.md`
- 21 — `docs/archive/GO_FORK_EXEC_FIXES.md`
- 20 — `docs/archive/GOLANG_IPC.md`
- 18 — `docs/archive/DASH_MISSING_SYSCALLS.md`
- 16 — `docs/archive/AKUMA_SELF_HOSTING.md`
- 15 — `docs/archive/SMP_SHARED.md`
- 14 — `docs/archive/BUN_MEMORY_STUDY.md`
- 14 — `docs/archive/GIT_MISSING_SYSCALLS.md`

---

## Syscall / ABI Compatibility Audits (108 fixes, 11 docs)

### docs/archive/GOLANG_MISSING_SYSCALLS.md
(44 items with explicit `**Status:** Fixed/Implemented` markers — trusted directly per task instructions; includes items 1–14, the 15–18 batch (rt_sigreturn state restore, fork/vfork_complete race, user_va_limit), 19–21, 23–25, 27, 29–32, 37, 39–46, 49–52, 54–55, 57. Items 22/26/28/33 don't exist in the doc's numbering; 34/35/36 duplicate 30/31/32; 38/47/53/56 are explicitly not-fixed or tests-only and excluded.)

### docs/archive/DASH_MISSING_SYSCALLS.md
- §8: incorrect SPSR in forked/execve'd children (parent vs child expectations) — fixed by enforcing SPSR=0
- §9.1: child process context was all zeros
- §9.2: `get_saved_user_context` returned stale PC/SP
- §9.3: missing ProcessInfo write for forked children
- §9.4: `enter_user_mode` zeroed all registers
- §9.5: duplicate PID counters
- §9.6: `execve` did not activate the new address space
- §9.7: exit code contamination via shared ProcessChannel
- §9.8: forked child output invisible
- §9.9: `wait4(pid=-1)` not implemented
- §10.3: pipe reference counting broken across fork and dup
- §10.4: `stat` returned `st_ino=0` for every file
- §10.5: `unlinkat` ignored dirfd
- §10.5: `unlinkat` ignored `AT_REMOVEDIR`
- §11: no canonical-mode (ICANON) line editing in kernel TTY
- §12 Bug 1: raw mode not restored after meow exits (broken echo)
- §12 Bug 2: ICRNL not restored (commands silently swallowed)
- §12 Bug 3: dash's own terminal config overwritten by meow's

### docs/archive/GIT_MISSING_SYSCALLS.md
- Issue 1: no `/dev/null` device
- Issue 2: `mkdirat` returned wrong errno
- Issue 3: `chmod`/`stat` permissions not working
- Issue 4: `O_CREAT` did not create the file on disk
- Issue 5: `clone3` not implemented
- Issue 6: `O_CREAT` ignored the `mode` parameter
- Issue 7: fork didn't copy dynamic-linker pages
- Issue 8: fork didn't copy main binary code pages (dynamic binaries)
- Issue 9: `CLONE_THREAD` not implemented (pthread_create failed)
- Issue 10: `pread64` not implemented (index-pack failed)
- Issue 11: `CLONE_CHILD_CLEARTID` not implemented (pthread_join hung)
- Issue 12: `CLONE_THREAD` exit destroyed the shared FD table (git-clone exit 128)
- Issue 13: `CLONE_THREAD` registered as a `waitpid` child (git hung 110s)
- Issue 14: `futex_wake` missed private-futex waiters (pthread_join hung again)

### docs/archive/BUN_MISSING_SYSCALLS.md
- 20 previously-missing syscalls implemented for bun runtime support (epoll_create1/ctl/pwait, timerfd_create/settime/gettime + read, eventfd2, sched_setaffinity/getaffinity/yield/setparam/getparam, tkill, close_range, ftruncate, fchown, sysinfo, uname, clock_getres)
- Page-tracking corruption on preemption during `bun install`
- `madvise` calls mislabeled as `mremap` in syscall-name logging
- `epoll_event` struct alignment wrong on ARM64 (Bun Install Express crash)
- Bug 1: epoll never reported EPOLLIN for a listening TCP socket
- Bug 2: `accept4`/`accept` blocked instead of returning EAGAIN
- Bug 3: `accept4`/`accept` returned -1 (EPERM) instead of -EAGAIN
- Bug 4: EPOLLIN not reported after remote peer closes connection
- Bug 5: EPOLLHUP not emitted for fully-closed TCP sockets, causing epoll spin

### docs/archive/KNOWN_ISSUES.md
(Only unique items not already documented as the authoritative source elsewhere; items 8/9/12 are the same fixes as GOLANG_MISSING_SYSCALLS.md #23/#24/#27 and are not double-counted here.)
- #6: bun HTTPS fetch hang — `epoll_pwait` computed the absolute deadline instead of the per-iteration sleep
- #6: related — EPOLLET edge not reset after a drained `recvfrom`/`recvmsg`
- #7: `apk`/ppoll/select hang — same per-iteration-deadline bug in `sys_ppoll`/`sys_select`
- #10: devbox tap0 `poll()` always-ready — RX fiber busy-polled instead of blocking
- #10: `epoll_check_fd_readiness` had no match arm for `FileDescriptor::Tap`, defaulting to always-ready
- #10: `rump_server`'s idle loop self-rearmed a 1ms timer forever
- #11: BSP idle loops spun on bare `yield_now()` with no halt, pinning host CPU at ~100%
- #11: CPU-time accounting billed a thread's whole quantum residency even while WFI-halted

### docs/archive/CURL_MISSING_SYSCALLS.md
- `recvmsg` did not zero `msg_controllen`
- Non-blocking sockets not supported
- `eventfd2` (syscall 19) not implemented
- `read()`/`write()` on UDP sockets called TCP-only functions
- `sys_socket` returned wrong errno for IPv6
- `getsockopt` stub did not write to userspace
- `ppoll`/`pselect6` reported TCP sockets as always writable

### docs/archive/NODEJS_MISSING_SYSCALLS.md
- Unknown syscall 90 — `capget`
- Kernel heap OOM on file-backed mmap
- `sys_fcntl` missing fd validation — 46MB kernel heap leak

### docs/archive/SPLIT_SYSCALLS.md
- `sys_nanosleep` — fabricated rewrite broke the libakuma raw-value ABI and looped incorrectly
- `sys_pselect6`/`sys_ppoll` — rewritten with different fd-readiness logic instead of copied; reverted to match original

### docs/archive/APK_MISSING_SYSCALLS.md
- mmap region placement overlapped PIE code; `mmap_start` now computed dynamically 256MB after code end

### docs/archive/BUSYBOX_MISSING_SYSCALLS.md
- `wait4` ignored the `rusage` pointer, leaving it uninitialized instead of zero-filled

### docs/archive/XBPS_MISSING_SYSCALLS.md
- 16 previously-missing/broken syscalls implemented for XBPS: `uname`, `flock`, `umask`, UDP socket support, `sendmsg`/`recvmsg`, `getsockname`/`getpeername`, `setsockopt`/`getsockopt`, `utimensat`, `fdatasync`/`fsync`, `fchmod`, `madvise`, `readv`, mmap file-backed mappings, `openat` dirfd support + path canonicalization, `fchownat`, `unlinkat` proper error codes


## Memory & Virtual Memory (85 fixes, 21 docs)

### docs/archive/BUN_MEMORY_STUDY.md
- GIC/UART MMIO collision with the heap
- Heap exhaustion — hardcoded 64MB limit replaced with `compute_heap_lazy_size()`
- VirtIO MMIO collision as heap grew to reach `0x0A00_0000`
- 128GB mmap rejection blocking JSC's gigacage allocation
- Fork `stack_top` bug
- Eager ELF loading exhausted physical memory (needed lazy loading)
- Unaligned segment page placement
- Kernel data abort on lazy mmap pages / kernel identity mapping gap (opencode crash) — resolved by routing `phys_to_virt()` through TTBR1
- Safe user memory access principled fix — resolved the `bun install` hang from malformed DNS-response reads
- `sysinfo` hardcoded 256MB total RAM / wrong `mem_unit` byte offset
- Missing `statx`/`truncate` syscalls
- `EPOLLET` (edge-triggered epoll) not implemented
- JSC Gigacage SIGSEGV — `mprotect` on a sub-range clobbered the entire lazy region's flags
- `sys_io_setup` wrote a bogus small integer instead of a real mmap'd `aio_ring` VA (opencode hang)

### docs/archive/NODEJS_LIBUV_IMPLEMENTATION.md
- Bug 1: `mprotect` eager allocation exhausting physical memory
- Bug 2: demand paging mapped anonymous pages read-only
- Bug 3: exception handler ignored permission faults
- Bug 4: kernel-side pointer validation rejected lazy pages
- Bug 5: `sys_munmap` blindly unmapped eagerly-mapped pages
- Bug 6: exception handler had no fallback for eager mmap regions
- Bug 7: non-atomic page-table-entry creation (race condition)
- Bug 8: exception handler used the wrong process for CLONE_VM threads
- Bug 9: memory syscalls used the wrong process for CLONE_VM threads
- Bug 10: `sys_munmap` ignored length for eager regions (partial unmap destroyed the entire region)
- Bug 11: `ensure_user_pages_mapped` tracked frames on the wrong process for CLONE_VM
- Bug 12: `sys_munmap` removed safety-net PTE clearing
- Bug 13: `sys_mprotect` didn't update lazy-region flags

### docs/archive/FIX_MEMORY_MAPPING.md
- 1A: no OOM fallback in demand-paging readahead
- 1B: `fault_mutex` missing an RAII guard
- 1C: timer re-arm hardening gap
- 6: `MAP_POPULATE` flag not implemented
- 6: `MADV_WILLNEED` advice not implemented
- 8A: fork lazy-copy hang
- 8B: signal frame `uc_stack` bug
- 8C: IC flush + signal delivery interaction bug
- 10A: IC flush delivered SIGSEGV with wrong context
- 10B: IC flush replayed SVC with wrong register state, spurious `io_setup`
- 12: `io_submit` WILD-DA crash
- 13: CoW EL1 signal-delivery crash — kernel write to a CoW-RO altstack page bypassed the fault handler

### docs/archive/LOW_MEMORY_ENVIRONMENT.md
- OOM hardening — small PMM emergency reserve so the kernel survives and kills the process instead of panicking
- `tcc hello.c` OOM at 8MB — root cause A: dead ELF size-profile gates
- `tcc hello.c` OOM at 8MB — root cause B: kernel heap watermark was one-way (never reclaimed)
- Kernel-heap growth runaway during llama's concurrent graph-buffer mmap burst
- meow→ollama OOM at 7MB — lazy-ELF segment-boundary zeroing bug
- Interpreter loader exceeded `HEAP_SLURP_MAX`
- `kernel_profile_size` read the whole file instead of 256 bytes for shebang detection
- File-backed lazy `mmap` needed before process start (`MMAP_FILE_BACKED_LAZY`)
- Extreme thread-spawn crash (EC=0x25) — `memset` on a wrapped near-null stack base

### docs/archive/FAR_0x5_AND_HEAP_CORRUPTION_FIX.md
- Bug 1: FAR=0x5 kernel panic — `read_current_pid()` read garbage without checking TTBR0
- Bug 2: heap corruption during concurrent execution
- Bug 3: PMM spinlock without IRQ protection
- Bug 4: `talc_realloc` gap in IRQ protection
- Bug 5: PROCESS_TABLE lock without IRQ protection
- Bug 6: missing TLB flush in `activate()`
- Bug 7: missing TLB flush in `switch_context`
- Bug 8: incomplete TLB flush in activate/deactivate

### docs/archive/GO_BINARY_VA_SPACE.md
- forktest_parent OOM — VA space exhaustion root cause
- Follow-up: fork `code_start` SIGSEGV
- Follow-up: vfork signal-interrupted wait
- Follow-up EL1 crash Bug 1: `fork_process` missing THREAD_PID_MAP entry
- Follow-up EL1 crash Bug 2: `clone_thread` plain EL1 store to a CoW-RO page

### docs/archive/MEMORY_SYSCALL_STUB_FIXES.md
- Fix 1: signal delivery for synchronous faults
- Fix 2: `mremap` EFAULT for unmapped addresses
- Fix 3: `mremap` lazy-region handling
- Fix 4: `set_robust_list`
- Fix 5: `membarrier` command dispatch

### docs/archive/LLAMA_MMAP_OOM_KERNEL_ABORT.md
- Userspace `std::bad_alloc` under memory pressure
- Intermittent kernel hang under concurrent mmaps
- Kernel-heap growth runaway (real root cause of the OOM abort)

### docs/archive/FORK_EXEC_HEAP_LAZY_REGION_SIGSEGV.md
- `( cmd; more-cmds ) &` SIGSEGV — grandchild fork lost inherited mmap regions (double-pointer deref through a fixed global)
- Separate fork lazy-region propagation gap — `propagate_lazy_regions_to_child` missing

### docs/archive/GO_FORKTEST_DEBUG.md
- Pattern 3: PROT_NONE lazy region accessed without a prior `sysMap`
- Pattern 4: QEMU TCG `DC ZVA` EC=0x15 misrouting for `stp xzr, xzr` (Go 1.26 GreenTeaGC)

### docs/archive/PMM_DOUBLE_FREE_AND_EL1_CRASH.md
- Bug 1: `user_frames` refcount over-free (unmap/Drop ignored the refcount)
- Bug 2: ELF heap-slurp — `spawn.rs` read the whole binary into the 8MB heap, the real EC=0x22 cause

### docs/archive/ALLOCATOR_FIXES_AND_IMPROVEMENTS.md
- Virtual address space exhaustion under long-running workloads, addressed via kernel VA reclamation + hybrid allocator

### docs/archive/ALLOCATOR_REALLOC.md
- `realloc` calling `munmap` directly hung the kernel; worked around via a deferred free queue

### docs/archive/COW_OPTIMIZATIONS.md
- CoW fork correctness restored (fork was "just broken", not merely slow) before the perf work in this doc

### docs/archive/HEAP_CORRUPTION_ANALYSIS.md
- Userspace heap corruption — layout-sensitive bug in the allocator

### docs/archive/HEAP_CORRUPTION_INVESTIGATION.md
- EC=0x0E (illegal execution state) crashes

### docs/archive/IDENTITY_MAPPING_DEPENDENCIES.md
- Raw PA used instead of `phys_to_virt()`-translated VA across PMM/allocator/ELF-loader/MMU/VirtIO call sites (fixed 2026-03-13)

### docs/archive/KERNEL_OOM_ALLOCATION_FIX.md
- Kernel panic on large memory allocation failure

### docs/archive/NET_BOUNCE_OOM_KERNEL_ABORT.md
- Net syscalls' 64KB bounce buffer was an infallible allocation, aborting the whole kernel under pressure

### docs/archive/OOM_VIRTUAL_ADDRESS_SPACE_EXHAUSTION.md
- Memory failure — virtual address space exhaustion root cause

### docs/archive/USER_STACK_SIZE_INCREASE.md
- User stack size increased 64KB→128KB after a root-caused overflow


## Scheduler & Process Management (71 fixes, 15 docs)

### docs/archive/GO_FORK_EXEC_FIXES.md
- 1: PROCESS_INFO_ADDR overwritten by `cow_share_range`
- 2: `clone(flags=0)` routing / garbage flag cascade fork-bombed
- 3: `clone_thread` stack=0 crash
- 4: `sys_kill` ignored the signal argument (always reported exit 137)
- 5: `exit`/`exit_group` returned to userspace instead of terminating the thread
- 6: zombie processes — `sys_exit` skipped `unregister_process`
- 7: missing `tgid` (thread-group leader ID) for goroutine-thread signal delivery
- 8: `futex` on an unmapped address returned EFAULT, breaking Go's exit coordination
- 9: `is_interrupted` flag never cleared, so blocking syscalls returned EINTR forever after one SIGTERM
- 10: `copy_to_user_safe` byte-by-byte writes silently EFAULT'd Go's `mp.procid` page
- 11: EL1 user-copy fault-handler fast path caused a POOL-lock deadlock (reverted)
- 2026-04-10: exit_group ordering fix (close_all deadlock)
- 2026-04-10: boot-test thread-leak fix
- 2026-04-10: boot-test crash fixes (thread-state manipulation)
- 2026-04-10: fatal signal in clone thread failed to trigger exit_group
- 2026-04-10: ext2 spinlock deadlock
- 2026-04-10: signal frame layout bug (uc_mcontext offset)
- 2026-04-10: lazy region lookup miss (RESOLVED)
- 2026-04-12: SIGURG delivery to uninitialized Go threads
- 2026-04-12: sigaltstack inheritance bug in `clone_thread`
- 2026-04-12: ext2 orphaned lock recovery

### docs/archive/GOLANG_IPC.md
- Missing executable permission for signal handlers (v2026-03-21)
- Post-success hang: parent epoll vs child exit (2026-03-22)
- Demand-paging icache invalidation bug (2026-03-22)
- Fd number allocation: monotonic counter → lowest-available (2026-03-22)
- SIGSEGV after exit_group in a CLONE_THREAD group
- Process identity collision after `kill_thread_group` (SIGSEGV at 0x6006c15c)
- Kernel hang Bug 1: fd-table spinlock deadlock
- Kernel hang Bug 2: orphan children not killed on parent exit
- Kernel hang Bug 3: `CLONE_PIDFD` pidfds not marked O_CLOEXEC
- `O_APPEND` not honoured in `sys_write`
- Use-after-free of page tables during `exit_group`
- SIGSEGV/Hang investigation update (2026-03-26) fix
- Go Build SIGSEGV four-bug follow-up (2026-03-26), incl. `alloc_mmap` straddle bug
- `si_code=0` causing Go crash (SIGSEGV treated as software signal)
- FAR=0x0 null deref in Go `asm` — root cause 1: nil `g.m` dereference
- FAR=0x0 null deref in Go `asm` — root cause 2: "killing process" log was a lie (didn't actually kill)
- Spurious wakeup in FUTEX_WAIT
- EventFd use-after-exec EBADF
- mmap VA recycling — root cause A: `free_regions` recycling caused an infinite mmap/munmap loop
- mmap VA recycling — root cause B: `alloc_mmap` not IRQ-safe for CLONE_VM goroutine threads

### docs/archive/UNIFIED_PROCESS_ABI_IMPLEMENTATION_ISSUES.md
- 1: TTBR0 ASID corruption — raw TTBR0 register value used as a physical address without masking ASID bits
- 2: SPAWN/EXECVE signature mismatch — kernel expected a flat null-separated buffer, libakuma sent a `char**` array
- 3: string visibility in ABI tests — missing DSB/ISB barriers after writing test data via the identity map
- 4: missing null termination in libakuma syscall wrappers (`&str` isn't null-terminated)
- 5: VFORK/EXECVE bridge PID leak — `sys_execve` returned 0 instead of the real spawned PID
- 6: stack alignment — `StackBuilder` didn't round the final SP down to 16 bytes

### docs/archive/CONTEXT_SWITCH_BUGS.md
- Bug 1: SPSR=0x4 (EL1t mode) crash
- Bug 2: nested IRQ context corruption
- Bug 3: deadlock in cleanup + timer IRQ
- Bug 4: allocator deadlock in IRQ handler
(Bug 5, NULL pointer deref, was still open at time of writing — not counted)

### docs/archive/THREADING_RACE_CONDITIONS.md
- Dangling `pool_ptr` after lock release
- `POOL.data_ptr()` read without holding the lock
- `CURRENT_THREAD` timing race — fixed via CPU register (TPIDRRO_EL0)
- POOL + allocator deadlock — fixed via IrqGuard around `POOL.lock()`

### docs/archive/FORK_MMAP_AND_WAIT_STATUS_FIX.md
- Bug 1: forked children crash — missing mmap regions
- Bug 2: signal deaths encoded as normal exits (wrong wait-status encoding)
- Bug 3: `set_brk` page mapping started at a non-page-aligned address

### docs/archive/LOCK_FREE_THREADING.md
- Thread-pool `Spinlock` contention caused SSH hangs/input staggering — fixed via lock-free atomic thread states (incl. the INITIALIZING-state race that let the scheduler run an uninitialized thread, FAR=0x0)
- Embassy-net `RefCell` double-borrow panics under concurrent SSH polling
- Async yielding inside `block_on` context hung — fixed with a `YieldOnce` future

### docs/archive/THREAD_SCHEDULING_INVESTIGATION.md
- Issue 1: memory leak in `realloc`
- Issue 3: `munmap` not properly implemented

### docs/archive/TTBR0_AND_THREADING_FIXES.md
- TTBR0 corruption during thread spawning
- Zombie thread cleanup gap

### docs/archive/CONTEXT_SWITCH_FIX_2026.md
- Thread 0 format-panic crash (January 2026 context switch investigation)

### docs/archive/HERD_BLOCKING_FIX.md
- Herd process spawn hung the kernel when `AUTO_START_HERD` was enabled

### docs/archive/INITIALIZING_RACE_CONDITION_FIX.md
- INITIALIZING slot race condition

### docs/archive/SENDTO_PREEMPTION_FIX.md
- `sys_sendto` called with preemption disabled

### docs/archive/STACK_CORRUPTION_ANALYSIS.md
- Thread-slot cleanup race — closure lifetime vs. stack lifetime

### docs/archive/THREAD_STACK_ANALYSIS.md
- EC=0x25 thread-spawn crash — `memset` on a wrapped near-null lazy stack base


## SMP & Locking (49 fixes, 14 docs)

### docs/archive/SMP_SHARED.md
- M2c bug 1: 16KiB secondary stack overflow
- M2c bug 2: BKL acquire re-entrancy self-deadlock (nested timer IRQ)
- M2c bug 3: voluntary reschedules rang PE0 instead of self
- M2c bug 4: idle-loop contention livelock at SMP=4 vs. missed worker pickup
- Profiler `HOLDER_TAG` false-sharing tipped a flaky test into a wedge
- LifecycleGuard was a no-op — multi-step process lifecycle ops weren't actually preemption-safe
- Per-core `VOLUNTARY_SCHEDULE` — a single global flag let a peer's tick steal a voluntary yield
- BKL fair-FIFO ticket leak — self-healing acquire added (root mechanism still open, but the deadlock itself is resolved)
- Cross-core CoW/TLB: missing `dsb ish` barrier in `demote_range_to_ro`
- Cross-core CoW/TLB: CoW fault serialization was per-PID not per-physical-page, causing a double-free race
- forktest_parent (Go) hang — `sys_waitid` blocked on non-child processes instead of returning ECHILD
- Phantom-SVC misclassification — ESR/FAR read after the BKL wrapper's preemptible spin window, using stale syndrome registers
- `exit`/`exit_group` returned to EL0 when `current_process()` was None (CLONE_VM sibling)
- `enter_kernel`/`leave_kernel` read `current_core_id()` in preemptible context, racing a mid-read migration
- Terminated-thread lock leak — hard-terminated siblings stranded locks held mid-EL1 (fixed via deferred pending-kill)

### docs/archive/BKL_VFS_CARVE_OUT.md
- §9: `[BKL] stuck` regression — IRQ-epilogue reconcile converted every dropped window into a BKL-held run at the next timer tick
- §10.3: `MADV_DONTNEED`/`MADV_WILLNEED` zero-filled file-backed lazy pages instead of pre-faulting them
- §11.7: thread-slot reclamation starved under load (p50 24s / max 192s), stalling `fork` for minutes
- §16: profiler tag-restore bug — `rust_irq_handler_with_sp` stamped `HOLD_TAG_IRQ` unconditionally, miscrediting a preempted thread's syscall to `irq/sched`
- §18: `irq/sched` BKL-wait attribution was an artifact of per-core (not per-thread) tag tracking

### docs/archive/STABILITY_URGENT_ISSUES.md
- Idle kernel deadlock — `RUNTIME` spinlock self-deadlock from an IRQ handler
- SSH lifecycle guard never dropped — function-scope guard placement bug
- SSH connection counter permanently drifted after a panic-unsafe path
- Connect-storm stall root-caused and fixed (Phase 2)
- SSH exec of external binaries lost output — `execute_external_interactive` ignored a channel argument

### docs/archive/EPOLL_PERFORMANCE.md
- Lock inversion deadlock #1 — EPOLL_TABLE ↔ PROCESS_TABLE
- Lock inversion deadlock #2 — NETWORK ↔ SOCKET_TABLE
- Epoll multi-poller pipe test failure
- Zombie processes after CLONE_VM thread-group exit

### docs/archive/PROCESS_TABLE.md
- Stage D: `list_processes()` fixed to a two-phase collect-then-build to avoid a lock-order issue
- Stage B: writer (fork's `register_process`) starvation under RwSpinlock
- Stage C: reads not wrapped in `with_irqs_disabled`
- Stage C: allocator could stall/deadlock with IRQs disabled during a process-table read

### docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md
- Phase 2: socket refcount missing on `clone_deep_for_fork`, causing cross-socket data injection / SSH stream corruption
- Phase 2: `wait_until` busy-spun holding the BKL forever under constant network progress (no relax after 4 fruitless rounds)
- Phase 2: pipe `SIGPIPE` self-deadlock — signal raised while holding the `PIPES` spinlock

### docs/archive/BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md
- On-demand reclaim in `register_process` self-deadlocked (non-reentrant spinlock re-entry from an unrelated zombie's teardown)
- Unbounded pipe buffer growth ran the allocator inline while holding `PIPES`, replaying the SIGPIPE-in-lock deadlock via OOM
- `sys_exit_group` released its fd table AFTER notifying its reaper, so a peer-reaped thread never released fds — `head`/`yes` pipe never drained

### docs/archive/BKL_MM_CARVE_OUT.md
- `ProcessMemory::free_regions`/`alloc_mmap()` had no lock at all
- `sys_mmap`'s OOM/reclaim sweep mutated page tables with no `as_lock` hold

### docs/archive/BKL_PHASE7E_ACCESS_HALF.md
- Three sites (`ensure_cow_page_writable`, `try_resolve_el1_cow_fault`, signal handler/restorer RX fix-up) edited live PTEs with no `as_lock` hold
- `cleanup_process_fds`'s `strong_count == 1` gate never fired for externally-killed multithreaded groups (deferred reclaim), stranding pipes/sockets

### docs/archive/BKL_PROCESS_CARVE_OUT.md
- fork-corruption bug (cross-core CoW/TLB, see SMP_SHARED.md) validated fixed and promoted to default-on
- `cargo_runner.sh` didn't always regenerate the kernel binary before boot (stale `.bin`)

### docs/archive/BKL_PHASE7A_TIMER_IRQ_CARVE_OUT.md
- `ALARM_QUEUE`'s `critical_section` lock gave no real cross-core exclusion under `smp-shared`; replaced with a real Spinlock

### docs/archive/BKL_PHASE7C_OPENAT_RESIDUAL.md
- `sys_openat`'s `VfsBklGuard` opened after `resolve_symlinks` already did real ext2 I/O under the BKL

### docs/archive/BKL_PHASE7_AUDIT.md
- §4.1: BKL's own ticket accounting broken — `acquire_no_ticket` advanced `now_serving` without allocating a ticket

### docs/archive/NETWORKING_DEADLOCK_INVESTIGATION.md
- Priority inversion between the network poller and VFS-lock-holding threads (Strategy A)


## Networking (30 fixes, 12 docs)

### docs/archive/NETWORKING_POLLING_AND_ACK_FIXES.md
- 1: smoltcp 10ms delayed ACK (primary throughput killer)
- 2: `poll_input_event` syscall argument order mismatch
- 3: main loop always yielded regardless of work done
- 4: `wait_until` only polled once per iteration
- 5: no poll after recv/send
- 6: `top`'s first-frame CPU calculation

### docs/archive/EPOLL_EL1_CRASH_FIX.md
- Fix 1: EL1 data-abort recovery with landing pad
- Fix 2: epoll cleanup missing on explicit `close`
- Fix 3: eagerly-allocated (non-lazy) stack region caused stack-overflow SIGSEGV
- Fix 4: EpollFd incorrectly inherited across fork
- Fix 5: `EPOLL_CLOEXEC` not honored

### docs/archive/MULTIKERNEL_NETWORKING_EXPERIMENT.md
- Reply latency bounded by the timer tick
- Stage 2 (B): kernel `copy_to_user` fault on curl's lazy receive-buffer page truncating DNS replies
- Fixed (D): kernel RX-DMA truncation for frames >~586 bytes
- `--service` fallback recursion — herd forking itself repeatedly, exhausting a fixed resource

### docs/archive/SMOLTCP_MIGRATION_CHALLENGES.md
- Shared `SocketSet` conflict between VirtIO and Loopback interfaces dropped local packets
- Ephemeral port blindness — `connect` passed port 0, smoltcp doesn't auto-allocate
- Waker misses — `TcpStream::read`/`write` returned Pending without registering a waker
- DHCP deconfiguration flakiness — no static-IP fallback when QEMU's Slirp NAK'd the lease

### docs/archive/VIRTIO_RECEIVE_FIX.md
- Bug 1: receive buffer never posted to the VirtIO device
- Bug 2: VirtIO net header not stripped from the receive buffer
- Bug 3: incorrect Ethernet MTU

### docs/archive/USERSPACE_NETWORKING_SUCCESS.md
- `TcpSocket` moved after `accept()`, corrupting it — fixed by boxing immediately
- Empty responses — `socket.abort()` called before data transmitted

### docs/archive/DHCP_LOOPBACK_TEST_FIX.md
- DHCP loopback test root-cause fix

### docs/archive/LOOPBACK_ARP_RATE_LIMIT_BUG.md
- Loopback test crash — smoltcp global ARP rate limiter

### docs/archive/MEOW_OLLAMA_TIMEOUT_FIX.md
- meow↔ollama connection timeout root cause fix

### docs/archive/SOCKETSET_EXHAUSTION_FIX.md
- `SocketSet` exhaustion panic — fixed-size borrowed slice panicked instead of growing/handling gracefully

### docs/archive/TCPSTREAM_CORRUPTION_FIX.md
- `TcpStream::read` corrupted pointer causing a recurring EL1 data abort

### docs/archive/VIRTIO_MMIO_LEGACY_TO_MODERN.md
- `force-legacy` incorrectly defaulted to true, masking the modern VirtIO MMIO v2 path


## Userspace Apps & Libraries (30 fixes, 15 docs)

### docs/archive/DOOM.md
- SIGSEGV in `R_Init` — `strncpy` stub off-by-one corrupted an adjacent struct field
- `STCFN33 not found` crash — `vsnprintf` didn't handle integer precision (`%.3d`)
- Extreme WAD-loading slowness — re-read entire file per lump instead of mmap-once
- SSH disconnect after ~60s — TCP socket timeout too short
- SSH disconnect after ~120s — server didn't answer SSH keepalive global requests during exec
- Kernel OOM panic during ANSI rendering — unbounded process channel buffer growth
- ANSI art scrolled instead of rendering in-place — raw_mode checked at wrong time
- Syscall mismatch after merge — doom binary built against stale syscall numbers
- SSH log spam during gameplay — `SSH_MSG_CHANNEL_WINDOW_ADJUST` not handled

### docs/archive/SIDEBAND_PARSER_FIX.md
- Sideband pkt-line parser root-cause fix
- Chunked-transfer CRLF consumption bug
- Silent error swallowing in libakuma-tls

### userspace/scratch/docs/PACK_PARSING_CRASH.md
- Problem 1: trial decompression handled incorrectly
- Problem 2: incorrect bytes-consumed estimation
- Real root cause: stack overflow (FAR address inside the stack guard region)

### userspace/libakuma-tls/docs/ERROR_HANDLING_FIX.md
- Non-streaming HTTP(S) reads silently swallowed I/O errors, returning `Ok` with truncated data
- Streaming `read_chunk` reads returned `StreamResult::Done` for a connection failure, indistinguishable from clean EOF

### userspace/libakuma/docs/ALLOCATOR_MEMORY_FIX.md
- `DeferredFreeQueue` capacity too small (16 slots), leaking old buffers past that count
- Queue only flushed on `dealloc`, so allocation-heavy code (e.g. `scratch clone`) leaked before any deallocation occurred

### userspace/scratch/docs/GIT_ADD_FIX.md
- Bug 1: `read_dir` returned true for regular files
- Bug 2: incorrect staging count for existing files

### docs/archive/PACKAGES.md
- `pkg install` buffered entire HTTP responses in a kernel-heap `Vec<u8>`, OOM-panicking on large downloads

### docs/archive/SCRATCH_CLONE_DECOMPRESSION_FIX.md
- scratch clone zlib decompression root-cause fix

### docs/archive/SQLD.md
- Duplicate `argv[0]` — shell passed the program path twice in the argument list

### userspace/libakuma-tls/docs/TLS_BUFFER_TRUNCATION_FIX.md
- TLS buffer truncation bug

### userspace/libakuma/docs/POLL_INPUT_EVENT_FIX.md
- Terminal input polling overflow

### userspace/llama.cpp/docs/LLAMA_CPP_MMAP_PATCH.md
- File-backed mmap KV-cache buffer-type selection gate corrected vs. the original plan (checked after `buft` is resolved)

### userspace/scratch/docs/CHECKOUT_PERFORMANCE.md
- O(n²) `sys_read` usage during checkout — switched to block-level `read_at`

### userspace/scratch/docs/MEMORY_LEAK_FIX.md
- Allocator leak — userspace deferred free queue too small during `realloc`-heavy operations

### userspace/scratch/docs/POSSIBLE_MEMORY_LEAK.md
- scratch memory-leak case, fixed (Feb 5 2026)


## Rump Kernel & Syscall Proxy (24 fixes, 5 docs)

### docs/archive/OPTIONAL_SMOLTCP.md
- `sendmsg` UnixSocket passthrough missing for rump box 0's own channel I/O
- WAITPID pid ↔ rump-fd numeric collision misrouted `waitpid` through the rump proxy, hanging sessions
- One-shot `ssh host <cmd>` didn't spawn the child (only `shell` channel-request type recognized)
- `clone_thread` copied a stale/bogus TTBR0 from `THREAD_CONTEXTS`, wedging the box on `curl https://host`
- `fork_process` had the identical stale-TTBR0 bug (git/wget fork+exec hung the VM)
- `vfork_process` had the identical stale-TTBR0 bug (the one `git clone` actually hit)
- c-ares DNS failure — `fcntl(F_SETFD)` on a rump socket returned EOPNOTSUPP, fatal to c-ares
- `socketpair()` hijacked by the rump proxy purely by syscall number, breaking every Rust subprocess spawn
- CLOEXEC-pipe EBADF — rump proxy also hijacked `sendto`/`recvfrom` by syscall number instead of fd type
- Concurrent SSH: `fcntl(F_SETFL, O_NONBLOCK)` on a rump socket was hard EOPNOTSUPP'd

### docs/archive/FIBER_HANDOFF.md
- Networked sysproxy `rump_server` under fiber — handshake banner: kernel handshake read timed out (errno 5)
- Networked sysproxy under fiber — idle loop used `sleep()` instead of blocking `nanosleep`
- Networked sysproxy under fiber — a third coupling bug in the non-blocking-recv path (2026-06-24 batch)
- LATENCY root-caused & fixed — the 16s cost was not per-syscall, but a scheduling artifact
- tap-fd poll support: RX fiber busy-polled instead of blocking (2026-07-06)
- tap-fd poll support: a second busy-floor uncovered after the first fix — idle main loop 1ms self-rearm
- Update (2026-07-07): remaining ~100% host CPU — two further bugs (BSP idle-loop spin + CPU-time accounting lie)

### docs/archive/RUMP_SYSPROXY_LATENCY_FIX.md
- Scheduler `reset_sched()` helper needed — a stale global overwrote state before it resumed
- sshd TTY path opened its fd per-session instead of once and reusing it
- Phase 3j race — global overwritten before resume; fixed by capturing into a local
- Phase 3q Tier-1 fix for the ~48ms/leg Akuma scheduler round-trip cost

### docs/archive/RUMP_SYSPROXY.md
- Bug fixed en route to Phase A dispatch (kernel can't drain its ProcessChannel early in boot) — fixed via idle-through-`nanosleep`
- Self-interception bug — the sysproxy server drove its own channel and intercepted its own replies

### docs/archive/ARCHITECTURE_QUESTIONS.md
- `ifcreate` hang — `rumpuser_clock_sleep` didn't release the rump CPU around its sleep


## Toolchain & Self-Hosting (22 fixes, 3 docs)

### docs/archive/AKUMA_SELF_HOSTING.md
- §3: boot self-test VA collision causing MEMORY≥8G `map_user_page` crash
- §5: link step stdin-EOF killed the SSH session (parked forever pre-waker-registration-check)
- §7d: rustc futex deadlock — FUTEX_WAIT_BITSET absolute timeout treated as relative
- §7d: MAP_SHARED writeback — lld's writable file-backed mmap silently downgraded to MAP_PRIVATE, 0-byte link output
- §7g: fault_mutex poison recovery — dead thread mid-fault permanently poisoned the per-page demand-paging lock
- §7g.1: clear_child_tid always-wake correctness fix (gate only the user-memory write, not the wake)
- §7h: exit_group reaped CLONE_THREAD siblings AFTER notifying the parent — orphaned rayon workers stuck forever
- §7i/7j: argv >1KB silently truncated (dropping trailing args) instead of E2BIG — smoltcp build wall
- §7i/7j: getpriority/setpriority ENOSYS return used as a pointer → WILD-DA crash
- §7j: "x8 race" — missing `dc cvau` before `ic ivau` left stale instructions in I-cache after RW→RX permission flip
- §7k.2: kernel wedge — fault-with-IRQs-masked left `yield_now` looping forever, escalating a process-local fault to a VM-wide hang
- §7k.3: release-profile system-thread stack was smaller (64KB) than size/extreme profiles — stack-size inversion
- §7k.3: intermittent rustc SIGSEGV root cause — signal mask was per-process not per-thread under a SIGUSR1 storm
- §7k.4/7k.6: intermittent rustc register corruption — stale-I-cache spurious `svc`
- §7k.5: `rt_sigsuspend` was an unconditional `=> 0` stub
- §7k.5: `tgkill` ignored `tgid`, should return ESRCH on mismatch

### docs/archive/RUST_TOOLCHAIN.md
- §1: `socketpair` (AArch64 syscall nr 199) unimplemented
- §4a.3: `lseek` on a non-seekable fd returned EINVAL instead of ESPIPE
- §4b′: multithreaded-fork dropped sibling thread stacks (lazy regions enumerated by pid not tgid, eager mmap regions not shared)
- §4c: CLOEXEC fds closed even on a failed `execve` (should survive to report the errno)
- §4d: `recvmsg`/`sendmsg`/`recvfrom`/`sendto` didn't route `UnixSocket` fds, EBADF on the CLOEXEC handshake pipe

### docs/archive/RUST_TOOLCHAIN_ISSUES.md
- Scheduler/timer freeze during `execve` under load


## SSH (12 fixes, 10 docs)

### docs/archive/SSH_TERMINAL_SIZE_FIX.md
- `TIOCGWINSZ`/`TIOCSWINSZ` in-kernel shell fix
- `TIOCGWINSZ`/`TIOCSWINSZ` userspace sshd fix (separate gap)

### userspace/sshd/docs/FLOW.md
- `bridge_process` idle loop called blocking `sleep_ms` with no `.await`, starving the executor and blocking other sessions
- `sshd`'s `TerminalState` was inherited by every `spawn_pty` child, cross-delivering stdin wakeups between sessions

### docs/archive/BOX_PTY_INTERACTIVE_SHELL.md
- `pty=true` fix for box/custom-sshd regressed the in-kernel `:2222` shell; needed the same fix applied there too

### docs/archive/SSH_ECHO_LATENCY_FIX.md
- SSH echo latency root-cause fix

### docs/archive/SSH_STAGGERING.md
- No-op waker in `block_on`

### docs/archive/SSH_STREAMING_ARCHITECTURE.md
- All SSH output arrived ~1 second after connection instead of progressively

### docs/archive/SSH_TERMINAL_KEY_TRANSLATION_FIX.md
- SSH terminal key translation root-cause fix

### docs/archive/USERSPACE_SSHD_FIX.md
- Userspace sshd failure on shared-kernel SMP mode

### userspace/sshd/docs/EXIT_STATUS_FIX.md
- `ssh host cmd` always returned exit code 255 regardless of the remote command's real exit status

### userspace/sshd/docs/INTERACTIVE_SHELL_BRIDGE_DRAIN_FIX.md
- Interactive-shell bridge lost command output (empty stdout on connection close)

---

## Files scanned with zero counted fixes (reference docs, open issues, reverted attempts, or pure duplicates of a fix counted elsewhere)

docs/archive: 4MB_STABLE_AGENT, AI_DEBUGGING, ARCHITECTURE, BKL_DRIVERS_CARVE_OUT, BKL_PHASE7B_PPOLL_CARVE_OUT (piece 2 reverted after A/B caught real corruption), BKL_PHASE7D_THREAD_CONTEXTS (dead/unreachable code removed, not a live bug), BKL_PHASE7F_OPTOUT_LIST, BKL_RUSTC_SCALING_BASELINE, BOX_SUBDIR_FS_LIMITATIONS, C_STUBS, CGI, COMMAND_CHAINING_SSH_BUGS, CONCURRENCY, CONTAINERS_STAGE_1_PLAN, CONTAINERS_STAGE_2_PLAN, CP_MV_IMPLEMENTATION_PLAN, CRUSH_MISSING_SYSCALLS (all gaps, none marked fixed), CWD, DEAD_CODE_ANALYSIS, DEV_RANDOM, DEV_ZERO, DOCKER, EMBASSY_REMOVAL, ERRORS_TO_CHECK, EXTREME_STACK_TRIMMING (perf, not bugs), FRANKENLIBC_EVAL, FREEZE_INSTRUMENTATION_PLAN, HEAP_AND_MEMORY_IMPROVEMENTS, HERD, HERD_ADD_AND_PATH_VALIDATION, HIJACK_VS_KERNEL_PROXY (analysis/validation only), IMPLEMENTATION_PLAN (rump phases, milestones only), INTERACTIVE_IO, KILL_COMMAND, LARGE_BINARY_LOAD_PERFORMANCE, LOCK_REFERENCE, LOOPBACK_TIMEOUT_FIX_PLAN (plan, not landed), MEMORY_LAYOUT (duplicate of AKUMA_SELF_HOSTING §3), MULTIKERNEL, MULTITASKING, MUSL_COMPATIBILITY, NAMESPACES, NATIVE_STACK_INTERNET, NEEDLE_SERVER, NETWORKING_PERFORMANCE_AND_THREAD_SAFETY_ANALYSIS, ON_DEMAND_ELF_LOADER, OOM_BEHAVIOR, OOM_RECOVERY_OPTIONS, PAWS_PLAN, PAWS_TO_SSH_SHELL_PLAN, PHASE01_BUILDRUMP, PHASE1_COMPLETION_BASELINE, PHASE1_NETWORK_LOCK_FOUNDATION, PHASE2_RUMPUSER, PHASE3_KERNEL_TAP, PLAN_SIGSEGV_COMPILE_FIX, POSSIBLE_MEMORY_LEAK, POST_EXIT_PMM_RECLAIM, PROCESS_MEMORY_CLEANUP, PROCFS, PROPER_EXECVE_PLAN, QJS, refactor_plan, RSA_FEATURE_GATE, RUMP_LATENCY_SLEEP_FIX (hypothesis disproven, patches reverted), RUMP_PLUS_HERD, SCHEDULING_TIMING_ISSUES (open/critical, not fixed), SCRATCH, SEPARATE_SHELL_BINARY, SHARED_FD_TABLES, SHELL_ENVIRONMENT_VARIABLES, SHELL_LIMITATIONS, SIGNAL_DELIVERY_FORKTEST_EVIDENCE (summary of fixes counted elsewhere), SMOLTCP_MIGRATION_SUMMARY (duplicate summary), SMP_SHARED_M5_FAULT_LOCK_PLAN, SSH, SSH_PERFORMANCE_FIX_2026, SSH_THREADING_BUG (superseded, duplicate), STRATEGY_A_IMMEDIATE_TUNING, STRATEGY_B_SMOLTCP_MIGRATION (duplicate), STRATEGY_C_IRQ_WAKEUPS, SYSCALL_BLOCKING, SYSCALL_ERRNO_COMPLIANCE_CHANGES, SYSCALL_HARDENING, TCC_LOW_MEMORY, TCP_SEQUENCE_UNDERFLOW_PANIC, TERMINAL_SYSCALLS (duplicate reference), TLS_DOWNLOAD_PERFORMANCE, TLS_INFRASTRUCTURE, TOP_CORE_COLUMN_PLAN, TRIM_FAT_PART_1, TRIMMING_FAT_PART_2, TWO_VMS_AGENT_DEMO, UNIFIED_CONTEXT_ARCHITECTURE (duplicate of FAR_0x5/THREADING_RACE_CONDITIONS fixes), UNIFIED_PROCESS_ABI, UNSAFE_POINTERS_AND_ATOMICITY, USERSPACE_MEMORY_MODEL, USERSPACE_SOCKET_API, VFS_LOCK_OPTIMIZATION_PLAN, WAIT_QUEUES, MEOW.

userspace: apk-tools/BUILD_NOTES, apk-tools/PIE_LOADER, box/OCI_IMAGE_PULL, box/TESTING (duplicate of libakuma-tls TLS fix), crush/IMPLEMENTATION_DETAILS, forktest/IMPLEMENTATION_PLAN, herd/CORE_AWARE_SCHEDULING, herd/SIGNAL_EXIT_HANDLING (explicitly "proposed, not implemented"), httpd/TIMESTAMPS, libakuma/ALLOCATOR_OPTIONS, libakuma/MKDIR_P_IMPROVEMENTS, libakuma/SYSCALLS, libakuma/TERMINAL_SYSCALLS, meow/CONFIG, meow/HOTKEYS, meow/SHELL, meow/TESTING, scratch/LARGE_FILE_CHECKOUT_OPTIMIZATION, scratch/SIDEBAND_PARSER_FIX (duplicate of docs/archive/SIDEBAND_PARSER_FIX.md), sshd/LIMITATIONS, sshd/MIGRATION_SUMMARY, tar/IMPLEMENTATION_PLAN, tar/STREAMING_EXTRACTION, tcc/DISTRIBUTION_PLAN, tcc/IMPLEMENTATION_DETAILS, tcc/IMPLEMENTATION_PLAN, tcc/LIBTCC1.


## VFS & Filesystem (10 fixes, 7 docs)

### docs/archive/STAT_AND_UNLINKAT_FIX.md
- Root cause 1: `stat()` returned `st_ino=0` for every file
- Root cause 2: `unlinkat()` ignored `dirfd` and flags
- Root cause 3: pipes broken in dash (missing `dup3` + `pipe2` stub)

### docs/archive/SYMLINK_ELF_LOAD_FIX.md
- Symlinked ELF binaries (e.g. `/bin/ls` → busybox) loaded before symlink resolution
- `busybox --install` returned "Operation not permitted"

### docs/archive/EXT2_FIRST_DATA_BLOCK_FIX.md
- ext2 `first_data_block` off-by-one

### docs/archive/GETDENTS64_DIR_CACHE_FIX.md
- getdents64 directory cache root-cause fix

### docs/archive/GIT_KERNEL_HANG.md
- git kernel hang root-cause fix

### docs/archive/GO_COMPILE_CRASH_DEBUGGING.md
- Go `compile` crash — kernel xattr code path bug

### docs/archive/WRITE_AT_SYSCALL.md
- `O_TRUNC` not honored in `sys_openat`


## Boot & Drivers (9 fixes, 5 docs)

### docs/archive/QEMU_HVF_ISV_BUG.md
- Root cause 1: GICv2 MMIO programming model (`isv` assertion)
- Root cause 2: physical timer (CNTP) trapped under HVF
- Root cause 3: IC IVAU on a not-yet-mapped user VA
- Root cause 4: post-indexed (writeback) MMIO store on the extreme profile

### docs/archive/LLAMA_CPP_AKUMA_VS_ALPINE_PERFORMANCE_GAP.md
- GICv2 `isv=0` assertion crash under HVF — fixed by a GICv3 driver
- Futex debug logging left on by default, producing log spam under load

### docs/archive/BOOT_STACK_BUG.md
- Boot stack collided with kernel/heap memory; moved to a fixed address 32MB above the kernel base

### docs/archive/DEVICE_MMIO_VA_CONFLICT.md
- UART MMIO clobbering by the heap fixed via L3 device page-table entries

### docs/archive/DYNAMIC_DTB.md
- RAM under-detected at large `MEMORY` sizes (DTB placement/overlap)


## Signals & Exceptions (9 fixes, 3 docs)

### docs/archive/SIGNAL_HELL.md
- Thread-group kill/exit_group failures — cause A: fake TIDs the state array couldn't represent
- Thread-group kill/exit_group failures — cause B: tests asserting behavior the code never had (+ a `!0u64`→`0u64` cleanup-mask bug)
- Pending signal bitmask failures (4 tests)
- STP instruction decoder — `stp_xzr_misroute_decode` (not a scale bug; decoder never doubled anything)
- Thread safety — `fake_thread_ids_safe`
- crush bug: `exit_group(0)` reported as `-9`
- crush goroutine stall — caused by debug-log UART contention, not a real hang

### docs/archive/SIGCHLD_DELIVERY_FIX.md
- SIGCHLD was never delivered to the shell, hanging the `wait` builtin

### docs/archive/SIGNAL_DELIVERY.md
- `SA_RESTART`/ELR-backup bug — ELR wrongly rewound for syscalls that had already completed successfully (e.g. `FUTEX_WAKE`)


## Misc / Cross-cutting (8 fixes, 1 docs)

### docs/archive/KERNEL_SPLIT_BUGS.md
- neatvi showed garbage characters at end of newlines
- Running `hello` from neatvi crashed the kernel
- Second `bun run` crashed with OOM in the anonymous page-fault handler
- Intermittent bun CLONE_VM worker crash during `bun run`
- akuma-net extraction: main smoltcp polling loop was missed
- akuma-net extraction: missing explicit `String` type annotation broke build
- HTTPS `curl` returned "Read error"
- Bug 13: IrqGuard DAIF save/restore regression


## Console & Terminal (3 fixes, 3 docs)

### docs/archive/PIPE_TTY_FIX.md
- Pipe TTY processing root-cause fix

### docs/archive/RICH_TERMINAL_INTERFACE_OVER_SSH.md
- `sys_poll_input_event` deadlock — acquired a spinlock already held in the same call path

### docs/archive/STDCHECK_DEBUG.md
- Layout struct corruption during a function call (workaround applied)


## Containers (1 fixes, 1 docs)

### docs/archive/BOX_CONTAINERS.md
- Arguments were not passed to containerized processes


---

## Files scanned with zero counted fixes (reference docs, open issues, reverted attempts, or pure duplicates of a fix counted elsewhere)

docs/archive: 4MB_STABLE_AGENT, AI_DEBUGGING, ARCHITECTURE, BKL_DRIVERS_CARVE_OUT, BKL_PHASE7B_PPOLL_CARVE_OUT (piece 2 reverted after A/B caught real corruption), BKL_PHASE7D_THREAD_CONTEXTS (dead/unreachable code removed, not a live bug), BKL_PHASE7F_OPTOUT_LIST, BKL_RUSTC_SCALING_BASELINE, BOX_SUBDIR_FS_LIMITATIONS, C_STUBS, CGI, COMMAND_CHAINING_SSH_BUGS, CONCURRENCY, CONTAINERS_STAGE_1_PLAN, CONTAINERS_STAGE_2_PLAN, CP_MV_IMPLEMENTATION_PLAN, CRUSH_MISSING_SYSCALLS (all gaps, none marked fixed), CWD, DEAD_CODE_ANALYSIS, DEV_RANDOM, DEV_ZERO, DOCKER, EMBASSY_REMOVAL, ERRORS_TO_CHECK, EXTREME_STACK_TRIMMING (perf, not bugs), FRANKENLIBC_EVAL, FREEZE_INSTRUMENTATION_PLAN, HEAP_AND_MEMORY_IMPROVEMENTS, HERD, HERD_ADD_AND_PATH_VALIDATION, HIJACK_VS_KERNEL_PROXY (analysis/validation only), IMPLEMENTATION_PLAN (rump phases, milestones only), INTERACTIVE_IO, KILL_COMMAND, LARGE_BINARY_LOAD_PERFORMANCE, LOCK_REFERENCE, LOOPBACK_TIMEOUT_FIX_PLAN (plan, not landed), MEMORY_LAYOUT (duplicate of AKUMA_SELF_HOSTING §3), MULTIKERNEL, MULTITASKING, MUSL_COMPATIBILITY, NAMESPACES, NATIVE_STACK_INTERNET, NEEDLE_SERVER, NETWORKING_PERFORMANCE_AND_THREAD_SAFETY_ANALYSIS, ON_DEMAND_ELF_LOADER, OOM_BEHAVIOR, OOM_RECOVERY_OPTIONS, PAWS_PLAN, PAWS_TO_SSH_SHELL_PLAN, PHASE01_BUILDRUMP, PHASE1_COMPLETION_BASELINE, PHASE1_NETWORK_LOCK_FOUNDATION, PHASE2_RUMPUSER, PHASE3_KERNEL_TAP, PLAN_SIGSEGV_COMPILE_FIX, POSSIBLE_MEMORY_LEAK, POST_EXIT_PMM_RECLAIM, PROCESS_MEMORY_CLEANUP, PROCFS, PROPER_EXECVE_PLAN, QJS, refactor_plan, RSA_FEATURE_GATE, RUMP_LATENCY_SLEEP_FIX (hypothesis disproven, patches reverted), RUMP_PLUS_HERD, SCHEDULING_TIMING_ISSUES (open/critical, not fixed), SCRATCH, SEPARATE_SHELL_BINARY, SHARED_FD_TABLES, SHELL_ENVIRONMENT_VARIABLES, SHELL_LIMITATIONS, SIGNAL_DELIVERY_FORKTEST_EVIDENCE (summary of fixes counted elsewhere), SMOLTCP_MIGRATION_SUMMARY (duplicate summary), SMP_SHARED_M5_FAULT_LOCK_PLAN, SSH, SSH_PERFORMANCE_FIX_2026, SSH_THREADING_BUG (superseded, duplicate), STRATEGY_A_IMMEDIATE_TUNING, STRATEGY_B_SMOLTCP_MIGRATION (duplicate), STRATEGY_C_IRQ_WAKEUPS, SYSCALL_BLOCKING, SYSCALL_ERRNO_COMPLIANCE_CHANGES, SYSCALL_HARDENING, TCC_LOW_MEMORY, TCP_SEQUENCE_UNDERFLOW_PANIC, TERMINAL_SYSCALLS (duplicate reference), TLS_DOWNLOAD_PERFORMANCE, TLS_INFRASTRUCTURE, TOP_CORE_COLUMN_PLAN, TRIM_FAT_PART_1, TRIMMING_FAT_PART_2, TWO_VMS_AGENT_DEMO, UNIFIED_CONTEXT_ARCHITECTURE (duplicate of FAR_0x5/THREADING_RACE_CONDITIONS fixes), UNIFIED_PROCESS_ABI, UNSAFE_POINTERS_AND_ATOMICITY, USERSPACE_MEMORY_MODEL, USERSPACE_SOCKET_API, VFS_LOCK_OPTIMIZATION_PLAN, WAIT_QUEUES, MEOW.

userspace: apk-tools/BUILD_NOTES, apk-tools/PIE_LOADER, box/OCI_IMAGE_PULL, box/TESTING (duplicate of libakuma-tls TLS fix), crush/IMPLEMENTATION_DETAILS, forktest/IMPLEMENTATION_PLAN, herd/CORE_AWARE_SCHEDULING, herd/SIGNAL_EXIT_HANDLING (explicitly "proposed, not implemented"), httpd/TIMESTAMPS, libakuma/ALLOCATOR_OPTIONS, libakuma/MKDIR_P_IMPROVEMENTS, libakuma/SYSCALLS, libakuma/TERMINAL_SYSCALLS, meow/CONFIG, meow/HOTKEYS, meow/SHELL, meow/TESTING, scratch/LARGE_FILE_CHECKOUT_OPTIMIZATION, scratch/SIDEBAND_PARSER_FIX (duplicate of docs/archive/SIDEBAND_PARSER_FIX.md), sshd/LIMITATIONS, sshd/MIGRATION_SUMMARY, tar/IMPLEMENTATION_PLAN, tar/STREAMING_EXTRACTION, tcc/DISTRIBUTION_PLAN, tcc/IMPLEMENTATION_DETAILS, tcc/IMPLEMENTATION_PLAN, tcc/LIBTCC1.
