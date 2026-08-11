# Akuma bugfix audit — itemized list

Counting rule: one item per distinct, dated/named bug confirmed fixed/resolved/implemented.
Plans/proposals with no landed fix, duplicate mentions of the same fix, and narrative
table-of-contents sections are excluded (noted per-file where relevant). Subsystem tags
are assigned per *file* (the dominant subsystem of that investigation doc), not per bullet —
a handful of grab-bag docs (e.g. `AKUMA_SELF_HOSTING.md`, `KERNEL_SPLIT_BUGS.md`) mix bugs
from several subsystems under one write-up.

## Statistics

- **Total distinct fixes counted:** 563
- **Docs contributing at least one fix:** 171
- **Subsystem categories:** 15

| Subsystem | Fixes | % | Docs |
|---|---:|---:|---:|
| Syscall / ABI Compatibility Audits | 116 | 20.6% | 15 |
| Memory & Virtual Memory | 89 | 15.8% | 25 |
| Scheduler & Process Management | 74 | 13.1% | 17 |
| SMP & Locking | 70 | 12.4% | 29 |
| Networking | 31 | 5.5% | 13 |
| Userspace Apps & Libraries | 33 | 5.9% | 17 |
| Rump Kernel & Syscall Proxy | 24 | 4.3% | 5 |
| Toolchain & Self-Hosting | 31 | 5.5% | 4 |
| SSH | 14 | 2.5% | 12 |
| VFS & Filesystem | 13 | 2.3% | 9 |
| Boot & Drivers | 9 | 1.6% | 5 |
| Signals & Exceptions | 12 | 2.1% | 5 |
| Misc / Cross-cutting | 14 | 2.5% | 4 |
| Console & Terminal | 15 | 2.7% | 7 |
| Containers | 18 | 3.2% | 4 |
| **Total** | **563** | **100.0%** | **171** |

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

## Syscall / ABI Compatibility Audits (116 fixes, 15 docs)

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

### docs/archive/FUTEX_REQUEUE_LOST_WAKEUP.md
(5 confirmed Linux divergences, measured on real aarch64 Linux vs. Akuma via `userspace/forktest/c_stress/futexops.c`)
- A requeued futex waiter was never removed from its original wait queue, permanently double-registering it — the lost-wakeup generator behind `pthread_cond_broadcast`/`timedwait`; fixed by re-locating the waiter across its whole tgid's queues after every wake-return
- `FUTEX_WAIT_BITSET`/`FUTEX_WAKE_BITSET`'s bitset argument was ignored, letting a selective wake steal a wake owed to a non-matching waiter; fixed by storing/matching the bitset per queue entry
- An unreadable (but non-null) futex timeout pointer was silently treated as "no timeout" instead of `EFAULT`
- `FUTEX_WAKE_OP` never performed the atomic read-modify-write on `uaddr2`; fixed with full op decode + RMW
- `FUTEX_WAKE_OP`'s conditional second wake (on `uaddr2`) was never performed

### docs/archive/CLONE_TIDFLAGS_THREAD_LIST_LOCK.md
- `clone_thread` wrote the child's tid into `CLONE_CHILD_CLEARTID`'s pointer unconditionally regardless of clone flags — musl passes that flag pointing at `__thread_list_lock`, so every thread spawn stamped a live tid into musl's own mutex word, permanently wedging it; fixed by gating all three tid-flag writes on the actual `flags` word

### docs/archive/UNAME.md
- `sys_uname`'s `release`/`version` fields were hardcoded literals disconnected from the build (`release` drifted from `Cargo.toml`; `version` carried no commit/profile info); fixed via `env!("CARGO_PKG_VERSION")` and a `build.rs`-emitted git-SHA + build-profile string

### docs/archive/THREAD_SLEEP_MISSING_CLOCK_NANOSLEEP.md
- `std::thread::sleep()` panicked on every call, on every build — Akuma never implemented `clock_nanosleep` (Linux aarch64 syscall #115), which is what Rust's `std` actually calls for `sleep` on `target_os = "linux"` (not plain `nanosleep`); the resulting `ENOSYS` (38) tripped an `assert_eq!` inside `std` that only ever expected 0 or `EINTR` (4) back; fixed by implementing `sys_clock_nanosleep` with full relative/absolute (`TIMER_ABSTIME`) clock handling


## Memory & Virtual Memory (89 fixes, 25 docs)

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

### docs/archive/EXECVE_STACK_LEAK_OOM_HANG.md
- Every successful `execve` leaked its whole syscall stack (ELF file buffer, `args`/`env`/`resolved_path`) because `enter_user_mode`'s `eret` never returns and no destructor runs — ~1MB/exec ratchet that hit the kernel-heap wall after a few hundred spawns; fixed by dropping the buffers explicitly before `eret`

### docs/archive/OOM_KILL_DEFERRED_RECLAIM_GAP.md
- No pressure-driven reclaim of RETIRED processes existed — a fault-killed process's whole address space sat parked until the 100ms `netpoll_maint` cadence, which pressure itself could starve (measured: PMM pinned at 15 free pages through 500 polls); fixed via `process::reclaim`'s `request_retired_reclaim()`/`drain_retired*()` wired into exit-path terminal parks, idle loops, and the page-alloc eviction ladder

### docs/archive/BOOT_SUITE_PMM_DEFERRED_RECLAIM.md
- `test_mmap_file_oom_survives` halted the whole boot suite at its PMM-conservation assert because the deferred process-teardown chain (thread-slot recycle → `unregister_process` → `reclaim_retired_processes`) was never driven by the test itself — not a leak, a suite-harness gap; fixed by having the test force the whole collector chain and converting the fatal assert to a non-fatal `[FAIL]` print

### docs/archive/FILE_PAGE_CACHE_MMAP_AMPLIFICATION.md
- File-backed `mmap` demand faults allocated a fresh PMM frame per process instead of sharing, so N concurrent processes mapping the same toolchain library (e.g. 4× `rustc` mapping a 295MB `librustc_driver.so`) held N physical copies filled by N separate ext2 read sweeps, driving memory pressure → eviction → re-read in a loop that made `-j4` self-host builds scale far worse than the job count justified; fixed by `src/file_page_cache.rs` deduplicating on `(inode, file_offset)`, reusing the existing CoW refcount for teardown


## Scheduler & Process Management (74 fixes, 17 docs)

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

### docs/archive/CURRENT_TRAP_FRAME_STALE_ON_EXIT.md
- `CURRENT_TRAP_FRAME[tid]` was never cleared on process exit or thread teardown, so a recycled thread slot inherited a pointer into an already-freed kernel stack, dereferenced by diagnostic readers (`current_trap_frame_elr`, `dump_thread_resume_points`); fixed by clearing it at slot-recycle, both exit paths, slot-claim, and `enter_user_mode`

### docs/archive/TRIM_FAT_COOPERATIVE_SCHEDULING.md
- ThreadWaker tests (`test_thread_waker_marks_ready`/`_idempotent`/`_roundtrip`) fabricated WAITING/READY state on a bare FREE slot with `Context.sp=0`; removing thread 0's cooperative grace window let the scheduler actually dispatch to the phantom slot, crashing with `[SGI-S FATAL] new_sp=0x0 invalid!` — fixed by spawning a real (never-dispatched) slot instead
- `test_thread_slot_reclaim_on_spawn` assumed its fill-and-terminate loop always ran under the 10ms reclaim cooldown, an assumption that depended on thread 0's old preemption immunity; fixed by measuring elapsed time and only asserting zero-reclaim when it's provably still inside the cooldown window


## SMP & Locking (70 fixes, 29 docs)

### docs/archive/SMP_GO_STRESS_CORRUPTION_FIX.md
(standalone writeup of the same 2026-07-22 investigation SMP_SHARED.md's own
running log already covers — its phantom-SVC, exit/exit_group-to-EL0,
stale-core-id BKL wedge, and deferred-pending-kill fixes are counted there,
not duplicated here. Two boot-self-test bugs surfaced validating those fixes
aren't recorded anywhere else.)
- `process_tests.rs`'s `test_kill_thread_group_reaps_futex_blocked_sibling` fabricated a futex-parked sibling from a bare claimed thread slot forced into WAITING — a state no real thread can be in (WAITING with no saved context); the new deferred-kill wake path (`request_thread_kill`) woke the parked slot and the scheduler dispatched it, halting with `[SGI-S FATAL] new_sp=0x0` on every SMP=1..4 run; fixed by making the sibling a real initialized thread whose trampoline runs the wake→schedule→self-terminate boundary dance end-to-end
- `test_mmap_file_oom_survives`'s lazy-path assertion expected only `-11` (OOM-SIGSEGV) from an oversized mmap, predating clean file-page eviction — which now legitimately lets the process finish with exit 0; the test now accepts both outcomes

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

### docs/archive/SMP_SHARED_ONCPU_GATE.md
- Cross-core stack-sharing corruption from two unguarded windows — the switch-out tail (POOL released before the core is off the outgoing stack) and wake-before-switch-out (a peer resumes a thread from stale `ctx.sp` while it's still running) — fixed via a per-thread `ON_CPU` gate that blocks a thread from being picked until the core is truly off its stack

### docs/archive/STALE_THREAD_SLOT_KILL.md
- `kill_thread_group` PHASE 2 acted on a recorded `thread_id` long after the slot could've been recycled (~10ms cooldown vs. a 2s grace-wait), killing an unrelated live process (a different rustc's `gcc`/`collect2`) and leaving it threadless forever; fixed via a `THREAD_PID_MAP` ownership check in `unregister_process`/`kill_process`/`kill_process_with_signal`

### docs/archive/TRAMPOLINE_STALE_PROCESS_RELR.md
- `entry_point_trampoline` resolved a new thread's `Process` via a first-ACTIVE-match table scan on a stale `thread_id`, running a wrong/unrelated process's image (re-executing ld-musl's RELR relocation loop on its data page — the months-old `N × INTERP_BASE + 0x6c964` crash); fixed via `resolve_thread_process` reading `THREAD_PID_MAP` instead of scanning the table, plus a refusal gate in `Process::run`

### docs/archive/GRACE_EXPIRED_HARD_KILL_ORPHANS.md
- `kill_thread_group`'s grace-expiry branch force-terminated every recorded sibling unconditionally — ignoring its own straggler test and acting on a stale (up to 2s old) `thread_id` snapshot, killing unrelated processes' threads (measured: 261 hard kills, 179 non-stragglers); fixed via `grace_kill_should_terminate` (ownership + pending-kill guard), plus a guard on PHASE 2's `THREAD_PID_MAP` eviction

### docs/archive/KTG_STALE_TID_EXIT_STAMP_J4_HANG.md
- `kill_thread_group` PHASE 2 stamped a thread-group's exit code onto a per-tid channel without re-checking that the recorded tid still belonged to that sibling — during the ~2s kill grace, a dead sibling's slot recycled to an unrelated live process (`ld`, mid-link), forging a clean exit for it; `wait4` reaped the still-running linker, its fd teardown was abandoned mid-sweep, and a leaked pipe write-ref hung an entire `-j4` self-host build forever (rustc blocked in `read()` waiting for an EOF that could never arrive); fixed via the same `THREAD_PID_MAP` ownership guard on PHASE 1 and PHASE 2, plus `SharedFdTable::close_all()` popping one entry at a time instead of snapshot-then-clear so an abandoned teardown no longer loses every still-unclosed fd (verbatim session record: `J4_HANG_LIVE_AUTOPSY.md`)

### docs/archive/J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md
- Failure C: a thread parked in an untimed `FUTEX_WAIT` never reaches the EL1→EL0 boundary that consumes a pending-kill request, so `kill_thread_group`'s grace-wait treated "request no longer pending" as "thread died" while its hard-kill gate refused to act on that same (correct) ownership evidence — sparing the thread twice and hanging its whole group's `wait4` forever; fixed by gating `grace_kill_should_terminate` on ownership alone and requiring actual termination (not just an absent pending-kill flag) before the grace-wait can declare success early

### docs/archive/THREAD_STATES_RACES_TID_GENERATIONS.md
- Four check-then-store races on `THREAD_STATES` (`ThreadWaker::wake` and three TERMINATED-overwrite sites) let a stale wake/ready resurrect a recycled slot with a foreign TTBR0/kernel-stack; fixed via CAS/`fetch_update` transitions that refuse invalid states

### docs/archive/LAZY_REGION_TABLE_ALLOC_UNDER_LOCK_HANG.md
- `LAZY_REGION_TABLE`'s `mmap`/`mprotect` mutators allocated on the heap while holding the lock; an OOM there abandoned the lock via `return_to_kernel`, and the exit path's `clear_lazy_regions` then re-acquired the same (now-wedged) lock forever — the `-j4` self-host freeze of 2026-08-02; fixed structurally by moving the per-pid map onto `Process::lazy_regions`, removing the second global-lock acquisition from the exit path entirely

### docs/archive/EPOLL_MULTI_POLLER_PIPE_FLAKE.md
- `test_epoll_multi_poller_pipe`'s intermittent `woken=1 (expected 2)` (31% at SMP=2, distinct from the earlier always-`woken=0` bug in `EPOLL_PERFORMANCE.md`) was a test-harness defect, not a kernel bug: a 2ms "assume both threads are scheduled" delay with no handshake, plus a wake-budget that exactly equaled the poll-interval fallback (10ms vs. 10ms, a coin flip under scheduler jitter); both fixed in the test

### docs/archive/MPROTECT_TLB_ASID_BUG.md
- `flush_tlb_range` invalidated with `tlbi vale1is`, whose ASID comes from operand bits [63:48] — zero for every user VA, while user processes run under non-zero ASIDs — so `sys_mprotect`'s permission downgrades (musl's guard-page `PROT_NONE`, RELRO GOT `PROT_READ`) never reached the TLB and stayed silently writable; fixed by widening to `vaae1is` (all-ASID), required because `new_shared` puts one L0 table under several live ASIDs at once

### docs/archive/PMM_TALC_LOCK_CYCLE_SILENT_WEDGE.md
- `BitmapAllocator::alloc_pages` allocated a `Vec` **while holding the `PMM` spinlock**, and the kernel heap's OOM-growth path (`PmmOomHandler::handle_oom`) takes `PMM` while `TALC` is held — a cycle between two non-reentrant, ownerless `RawSpinlock`s that froze all four cores with zero console output (the `PMM` side spins inside `with_irqs_disabled`, so the stuck core takes no timer IRQ and looked identical to "busy, not hung" from outside); previously misfiled as the `[BKL] stuck` storm, though the BKL was idle throughout (6 of 7 campaign deaths, vs. 1 genuine storm). Fixed via `alloc_pages_into`, reserving the result `Vec`'s capacity with `try_reserve_exact` *before* taking `PMM.lock()` instead of growing it while the lock is held; A/B-verified 6/25 silent wedges → 0/23 (Fisher p=0.023)

### docs/archive/PAGE_TABLE_UAF_BKL_STORM.md
- `execve` swapped in the new address space with no equivalent of `exit_group`'s "kill sibling `CLONE_VM` threads before dropping the owner" step, so a `CLONE_THREAD` sibling that outlived the phase that spawned it (a parked thread-pool worker — rustc uses one) could keep running under the address space `execve` was about to free, a POSIX-correctness gap and a plausible trigger for page tables being freed and PMM-poisoned while a peer core's `TTBR0_EL1` still pointed at them; fixed via `kill_exec_siblings` in `replace_image`/`replace_image_from_path`, reaping other thread-group members after the new ELF loads and before the destructive address-space swap
- No path that frees page-table frames ever checked whether any core's live `TTBR0_EL1` still pointed at them — three independent routes (a reaper on another core racing the exiting thread's final switch, `exit_group`'s own reclaim freeing the table the calling core still stood on mid-syscall, and a grace-expired hard-killed straggler whose core never ran the switch-out) could each free and poison a page table still installed in a running core's `TTBR0`, which then faulted un-fetchably on its own exception vector while holding the BKL forever (`[BKL] stuck owner=N` storms of tens of thousands of lines); fixed via a per-core live-TTBR0 registry (`ACTIVE_L0`/`PREV_L0` in `crates/akuma-exec/src/mmu/mod.rs`) that defers a page-table frame's free to `PENDING_TTBR_FREES` (`[AS-FREE-DEFER]`) instead of releasing it whenever a peer core is still on that table, draining once the holder has demonstrably moved off; A/B-verified 4 storms in 21 rounds (~19%) → 0 storms in 32 rounds

### docs/archive/CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md
(grab-bag investigation doc chasing a cargo null-`Rc` defect that itself remains open; three unrelated bugs were found and fixed along the way)
- §5.1 (D8): `sys_munmap` matched only a single eager region by exact `start_va`, so an unmap starting mid-region or spanning several regions freed only the first match, reported success, left the rest mapped with its VA never recycled, and skipped lazy regions entirely whenever an eager one matched; fixed by extracting `detach_eager_regions_in_range` to drain every region a range touches (9 host tests + a boot self-test)
- §12.4/§12.7: the BKL's out-of-band `acquire_no_ticket` barge (the BKL-free EL0-preempt reconcile) compensated with a `next_ticket.fetch_add(1)` that kept the ticket counters equal in aggregate, but a waiter that lost the ownership CAS at its own turn abandoned its allocated ticket for a fresh one with nothing left to ever advance `now_serving` past the abandoned slot — a genuinely lost FIFO ticket producing `[BKL] stuck owner=0` storms (lock reads *idle* while cores spin) until a 20M-spin self-heal forced recovery; fixed by having a waiter that loses that CAS keep its ticket and keep spinning in place instead of re-ticketing, and by having the barge leave the ticket queue completely untouched on both acquire and release
- §12.2–§12.4: on a multi-threaded `fork()`, every sibling thread that touches the same demoted page faults at once; the first thread through `fault_slot_acquire` breaks CoW and repairs the PTE, but the threads behind it — now holding a fault for a write that is already legal — had no repair path for pages with no lazy/eager region record (an ELF `.data`/`.bss` page from the image loader is never registered as either), so they fell through to a spurious SIGSEGV and took the whole `CLONE_VM` process down with them; fixed via `stale_write_fault_absorbed`, re-reading the PTE at EL0 permission-fault entry and absorbing (invalidate + retry) whenever it already grants the write, budgeted per-(VA, PTE) so a genuinely-declined repair still runs; A/B-verified 10/10 and 8/8 probe SIGSEGVs → 0

### docs/archive/TRIM_FAT_PROFILES_AND_ACCEPTANCE.md
(latent build-config defects exposed by moving `smp-shared` into the default feature set, i.e. compiling a configuration nobody had compiled before)
- `crates/akuma-exec/src/bkl.rs`'s `KERNEL_LOCK` was gated `#[cfg(kernel_smp_shared)]` while every consumer required `target_os = "none"` too, so the first host build compiling `smp-shared` carried it unreferenced and tripped `dead_code = "deny"`
- `threading::disable_preemption()` called the panicking `runtime()` accessor for a diagnostic timestamp, breaking `PreemptGuard`'s "works in host tests too" contract and failing `akuma-ext2 tests::append_to_file`; now probes `is_registered()` and degrades to `0`
- `crates/akuma-net/src/socket.rs`'s `PreemptGuard` import, consumed only by the `smoltcp`-gated `with_table`, carried no gate of its own, tripping `unused_imports = "deny"` on any build without the native stack — `scripts/build_devbox.sh` had been unbuildable; import now carries its use site's gate


## Networking (31 fixes, 13 docs)

### userspace/sshd/docs/PROCESS_PER_SESSION.md
- `MAX_BACKLOG = 8` (`crates/akuma-net/src/socket.rs`) was a hard ceiling on **simultaneous connection arrivals**, not the soft hint `listen(2)`'s backlog is on Linux: this stack has no SYN queue, a listener *is* a fixed pool of pre-created sockets sitting in `Listen` that `socket_accept` replenishes one at a time, so arrivals past the 8th got a RST regardless of how fast the server accepted. Every caller's requested backlog was silently clamped to it (`libakuma`'s `TcpListener::bind` asks for 128). Measured on devbox-smoltcp/SMP=4: 8/8 connections clean, 12/16, 17/24 before; 16/16 and 24/24 after. Raised to 32 behind the default-on `many-sessions` feature, which also lifts the smoltcp socket table from 32 to 128 on `small-sockets` builds — a 32-deep backlog is meaningless against a 32-socket budget. `kernel_profile_extreme` overrides both back down, so the 4 MB floor is unaffected

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


## Userspace Apps & Libraries (33 fixes, 17 docs)

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

### userspace/herd/docs/SIGNAL_EXIT_HANDLING.md
- herd reaped with `waitpid`, which returns `WEXITSTATUS` only, so a service killed by a signal decoded as exit code 0 and took the clean-exit branch — respawning with no `restart_delay_ms`, `restart_count` reset to 0, `max_retries` never consulted and `ServiceState::Failed` unreachable (a service crashing on startup became a hot restart loop); now reaped with `waitpid_status` and classified through the host-tested `herd::exit::classify`, recording `shell_code()` (128+signal)
- `stop_service` called `libakuma::kill`, which hardcodes signal 0 — the kernel's existence probe, never delivered — so herd cleared `svc.pid`, marked the service `Stopped` and left the process running unsupervised while `start_stopped_services` spawned a second copy; now sends a real SIGTERM via the new `libakuma::kill_signal`

### userspace/meow/docs/MLX_SERVER_TOOL_CALLS.md
- `meow`'s streaming client recognized a completed tool call by a literal byte match, `json.contains("\"finish_reason\":\"tool_calls\"")`, which only matches ollama's compact JSON serializer; `mlx-server`'s spaced `"finish_reason": "tool_calls"` never matched, silently dropping every tool call; fixed via a whitespace-tolerant `json_field_is` helper


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


## Toolchain & Self-Hosting (31 fixes, 4 docs)

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

### docs/archive/SELFHOST_DEVBOX_SMOLTCP.md
(grab-bag doc, `-j4` self-host thread-spawn-SIGSEGV investigation, 2026-08-02 – 2026-08-06)
- `Drop for UserAddressSpace` freed the dying ASID *before* flushing its TLB entries, opening an SMP window where a peer core could allocate and start using the same ASID while stale translations into the dead address space were still live (root cause of small-integer `FAR` values `0`/`5`/`7`); fixed by flushing before freeing
- `clone_thread`'s slot-claim path (`spawn_user_closure_initializing`) had no reclaim-and-retry on a FREE-slot miss, unlike the sibling spawn path — real `pthread_create`-heavy workloads hit `EAGAIN` at iteration ~58-68 of a tight loop despite gigabytes free; fixed by adding the same reclaim-and-retry fallback to the shared `spawn_user_thread_initializing` wrapper covering fork/vfork/clone_thread
- Three different thread-slot-claim paths cleared three different sets of per-slot state; the path every real `pthread_create` takes cleared only the trap frame, so a cloned thread inherited the previous occupant's signal mask, sticky wake flag, and other stale registers; fixed via a single `scrub_thread_slot()` called from every claim/free path
- The effective thread-slot ceiling was capped at `crates/akuma-exec`'s own duplicate `MAX_THREADS=64` constant regardless of `config::MAX_THREADS`, so raising the configured limit (256) silently did nothing (`threadmax` ceiling stuck at ~52-56); fixed by making `config::MAX_THREADS` a re-export of the single source of truth
- `clone_thread` published/returned the process pid (not the thread's own slot/tid) for `CLONE_PARENT_SETTID`/`CLONE_CHILD_CLEARTID`, and `sys_set_tid_address` had the identical bug — `tkill`/`abort`/`raise` from a spawned thread targeted the wrong kernel thread slot entirely (a stray SIGABRT could land on sshd); fixed by publishing the real thread slot
- A fatal `SIG_DFL` signal delivered through the pending-signal path was silently dropped when `try_deliver_signal` found no userspace handler (no default-action fallback existed — also broke `kill(pid, SIGTERM)`); fixed via `apply_default_signal_action`, performing `exit_group` for fatal default-disposition signals
- `THREAD_SIGNAL_MASK` (the authoritative per-thread mask) was never seeded on `fork_process`/`vfork_process`, and was seeded racily-late (after the child was already runnable) on `clone_thread`, reopening exactly the pre-exec signal window callers block signals to protect; fixed by seeding at every creation leaf before the child becomes runnable
- `FUTEX_WAITERS` never removed a thread's queue entry when it died while parked (only self-removal on timeout/EINTR existed), so a dead tid could silently absorb a future `FUTEX_WAKE` meant for a live waiter; fixed via `futex_purge_tid`, called at both `mark_thread_terminated` and the slot recycler
- `DRAINING[tid]` (the pressure-reclaim in-progress flag) had the identical stuck-forever shape — a recycled slot's new occupant inherited a stale "already draining" flag; fixed by clearing it in `scrub_thread_slot`


## SSH (14 fixes, 12 docs)

### userspace/sshd/docs/PROCESS_PER_SESSION.md
- Every SSH session ran as one future inside a single `sshd` process, so `panic = "abort"` — which is process-wide, not future- or thread-scoped — meant *any* panic on *any* connection dropped every other live session with it. `PROTOCOL_UNDER_LOAD.md` fixed the one known trigger (a malformed pre-KEX packet) while explicitly noting the blast radius itself remained; this closes that. Each accepted connection is now served by its own `fork()`ed child, which inherits the socket through the fd-table copy (`FdTable::clone_deep_for_fork` → `socket_clone_ref`, refcounted on close by `remove_socket` — machinery that already existed and was already correct for this case). Zero kernel changes were needed: `docs/MISSING_SOCKET_MACHINERY.md` had concluded the handoff was unbuildable, having surveyed `sys_spawn`, `SCM_RIGHTS` and procfs but not `fork()`, where the fd is never handed over at all. Verified by SIGKILLing one live session under load — exactly one peer ended, three ran to completion, the server kept serving. Bounded by a new `max_sessions` (default 24) against the global `MAX_PROCESSES = 64`, since a fully-occupied session now costs two process slots. On by default (`fork-sessions`); `SSHD_FORK_SESSIONS=0` reverts to the cooperative executor for memory-constrained images

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

### userspace/sshd/docs/PROTOCOL_UNDER_LOAD.md
- Unauthenticated remote crash: `process_unencrypted_packet`/`process_encrypted_packet` computed `packet_len - padding_len - 1` from client-controlled bytes with no check that `padding_len < packet_len`; a single malformed pre-KEX packet (no auth, no valid crypto needed for the unencrypted path) underflowed the subtraction and panicked the later slice bounds-check, and `panic = "abort"` took the whole shared `sshd` process down — dropping every other concurrently-open session with it, not just the offending connection. Verified live on `devbox-smoltcp` (SMP=4): a 10-byte crafted packet killed a bystander session alongside the attacker's, confirmed via `herd`'s exit/restart log and reproduced twice; fixed with the same `payload_len` bound the in-kernel SSH server's own encrypted-path already enforced (that server's unencrypted path, and both of userspace `sshd`'s paths, lacked it before this fix — see the doc for the in-kernel side, tracked separately as unfixed kernel work)

---

## VFS & Filesystem (13 fixes, 9 docs)

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

### docs/archive/CONCURRENT_WRITE_POSITION_RACE.md
- Two threads sharing a fd (`CLONE_FILES` — any pair of `pthread_create`d siblings) calling `write()` close together could corrupt each other's output: `sys_write` read a cloned `.position` under a lock, performed the actual disk I/O with the lock released, then wrote the advanced position back under the lock again — a TOCTOU gap that let two racing writes land at the same on-disk offset (measured: 136/800 64-byte blocks cross-thread-mixed under 4 racing threads); fixed via `SharedFdTable::reserve_write_pos`, which reads and advances the position in one lock hold before I/O starts (`O_APPEND`'s equivalent race via a fresh `file_size()` read is untouched, tracked as a known gap)

### docs/archive/EXT2_BLOCK_CACHE_DEFAULT_AND_CHUNKING.md
- The large ext2 block cache (`fs-cache`) was opt-in and no shipping build (including `release`/devbox) ever opted in, leaving a pathological 256KB/64-slot FIFO ring against a 1MB readahead; fixed by adding it to `default` features (2.7× faster `hello_std`, RAM floor for the `rustc` workload dropped from >2GB to 1GB)
- `ClockBlockCache`'s backing store was one contiguous `Vec<u8>` that geometrically doubled to a single 512MB realloc, ballooning the kernel heap to 1152MB and destabilizing sshd; fixed via chunked `Vec<Vec<u8>>` backing plus a lower cap formula (25%→12.5% RAM, 512MB→128MB max)


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


## Signals & Exceptions (12 fixes, 5 docs)

### docs/archive/GIT_CLONE_STALE_ITIMER_SIGALRM.md
- `ITIMER_DEADLINE`/`ITIMER_INTERVAL` lived outside `scrub_thread_slot`'s per-slot reset discipline, so an armed-and-abandoned itimer (e.g. busybox `wget -T`) outlived its process and fired an instant, unconditional SIGALRM against the next unrelated process to reuse that thread slot, killing `git-remote-https` before it even resolved DNS; now scrubbed to `(0, 0)` on every slot claim like the rest of the per-slot registers
- `check_itimers`'s Ctrl-C-style force-interrupt path fired regardless of `SA_RESTART`, kicking a periodic alarm-based heartbeat (libcurl's low-speed-limit timer) out of an in-progress blocking `write()` to the SSH exec-channel pipe and causing `git clone` of large repos to intermittently `exit(130)` mid-checkout; now gated on the new `SignalAction::wants_itimer_force_interrupt`, which still force-interrupts handler-less (`SIG_DFL`) alarms

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

### docs/archive/NIGHTLY_CARGO_HVF_SIGILL.md
- The `EC=0x0` (undefined-instruction) exception handler hard-killed the process instead of delivering SIGILL, so OpenSSL's ARM-feature-probe idiom (deliberately executing an unsupported instruction inside a SIGILL handler) could never recover, crashing nightly `cargo` under HVF at a fixed PC (`SM3SS1`, FEAT_SM3); fixed by routing `EC=0x0` through `try_deliver_signal` like the other fatal-fault arms


## Misc / Cross-cutting (14 fixes, 4 docs)

### docs/archive/DEVBOX_ISSUES.md
(Issue 5 only. Issue 2's fix is counted under `TERM_POLL_INPUT_PREEMPTION_FIX.md` and Issue 3's under `UART_SMP_INTERLEAVE_FIX.md`, the deep-dives it points at; Issues 4 and 6 are open, Issue 1 did not reproduce. Issue 5's `busybox --install -s` link-target half is counted under `BUILTIN_SSH_REMOVAL.md` above — the bullet here is the invocation-coverage half it did not cover.)
- The busybox applet-symlink step ran only for `--full-busybox`, and no image-build path other than a devbox `bootstrap.sh` rebuild invoked it, so freshly built or refreshed images (`disk_selfhost.img`, and `devbox.img` in Issue 1's "while debugging" note) shipped a `/bin` where every applet but the handful written by a bare `ln -sf` loop was missing — `wc`, `head`, `ps` and the rest answering "not found" while `/bin/busybox wc` worked; the step is now default-on whenever `OVERLAY_DIR` is empty, installs the full roster driven by `busybox --list`, and is mirrored by `overlays/devbox/bootstrap.sh` step 4, never clobbering a real binary the image ships (verified 2026-08-12: zero dangling symlinks under `/bin`)

### docs/archive/EXTREME_SIZE_BUILD_FIX.md
- `mod file_page_cache;` was declared under a `#[cfg(feature = "sc-framebuffer")]`
  that belonged to the following `mod fw_cfg;` (inserted between attribute and
  module by `6f01fe7`), breaking every non-`sc-framebuffer` profile with 15 ×
  `E0433` while ~15 call sites in `fs.rs`/`pmm.rs`/`vfs/mod.rs`/`main.rs`/
  `exceptions.rs` stayed unconditional — `extreme-size` had not compiled since
- The same stray gate silently un-gated `fw_cfg` for `release`/`size`, which had
  been carrying it unconditionally since `27fdf90` intended the opposite;
  re-gating restores that profile's size intent
- `sys_spawn_ext` / `sys_set_box_stack` called `super::container::caller_box_and_pid()`
  unconditionally, but `mod container` is `sc-containers`-gated and both syscalls
  are dispatched regardless (2 × `E0433` on `extreme-size`); helper moved ungated
  to `syscall/mod.rs` rather than stubbed, since a `(0, 0)` stub would make every
  caller look like host/box 0 and defeat `can_access_box`

### docs/archive/KERNEL_SPLIT_BUGS.md
- neatvi showed garbage characters at end of newlines
- Running `hello` from neatvi crashed the kernel
- Second `bun run` crashed with OOM in the anonymous page-fault handler
- Intermittent bun CLONE_VM worker crash during `bun run`
- akuma-net extraction: main smoltcp polling loop was missed
- akuma-net extraction: missing explicit `String` type annotation broke build
- HTTPS `curl` returned "Read error"
- Bug 13: IrqGuard DAIF save/restore regression

### docs/archive/BUILTIN_SSH_REMOVAL.md
- `scripts/populate_disk.sh`'s `busybox --install -s /mnt/disk/bin` linked every applet to the path busybox was *invoked* as (the populate container's mount point), dangling 295 of 304 `/bin` symlinks in the guest (`/bin/head -> /mnt/disk/bin/busybox`); replaced with `busybox --list` + relative `ln -sf`
- The `[TESTS] low-mem … skipping boot self-test suite` message printed with no `cfg` guard, so every `no-tests`/`size` image — which never compiled a suite at all — falsely claimed at boot that it had skipped one; now gated on the same condition as the suite itself


## Console & Terminal (15 fixes, 7 docs)

### docs/archive/TERM_POLL_INPUT_PREEMPTION_FIX.md
- The blocking stdin read (`sys_poll_input_event`, mirrored by `sys_read`'s `Stdin` arm) took `term_state_lock` and its nested `input_waker` spinlock with **preemption disabled but IRQs enabled**, so the post-wake re-acquire could sit in that state for as long as the holder took to become schedulable — 94 seconds in the captured incident — and the preemption watchdog declared the whole VM stuck; all 6 sites now use a per-attempt `try_lock()` retry that holds the preemption guard only on success, bounding the spin regardless of who the holder is
- `sys_poll_input_event` registered and cleared the input waker once per loop iteration; now registered once before the loop and cleared on every exit path (success, timeout, `EINTR`), which `schedule_blocking`'s sticky-wake already tolerates
- `TerminalState::push_input`/`read_input` locked `input_buffer` then `input_waker` with no preemption guard and no IRQ masking at all, unlike every other producer/consumer of that lock — dead code today, so not part of the live wedge, but a latent trap for whoever wires them up; brought to the same discipline

### docs/archive/UART_SMP_INTERLEAVE_FIX.md
- `console::emit` masked IRQs only on the calling core (`irq::with_irqs_disabled`), and DAIF is per-core rather than a lock, so under `smp-shared` two cores could be inside the byte loop hitting the shared PL011 data register at once, interleaving unrelated log lines byte by byte; now a `Spinlock<()>` with an owner-core-ID reentrancy guard (so a panic mid-`emit` on the same core cannot deadlock the panic handler), default-on for `release` and off for the single-core `size`/`extreme-size` profiles

### docs/archive/PIPE_TTY_FIX.md
- Pipe TTY processing root-cause fix

### docs/archive/RICH_TERMINAL_INTERFACE_OVER_SSH.md
- `sys_poll_input_event` deadlock — acquired a spinlock already held in the same call path

### docs/archive/STDCHECK_DEBUG.md
- Layout struct corruption during a function call (workaround applied)

### docs/archive/ALLOC_PRINT_AUDIT.md
(§7 remediation pass only. §1-§6 are the read-only survey that found these and contributed no fixes of their own. The branch's 59 `safe_print!` conversions are excluded as consistency debt, per the audit's own "none of this is a correctness bug".)
- `pmm::dp_counters_line()` returned an `alloc::format!`-built `String` and was called from `log_memory_stats_on_crash`, i.e. from inside the sync-EL1 crash handler — the one place in the kernel whose whole design (`StaticWriter`, "no heap allocation" comment block) exists to avoid the allocator. A fault taken while a core already held the TALC heap lock (a fault inside `alloc`/`dealloc`, or heap corruption — exactly the conditions that produce such a fault) would re-enter that lock and hang the crash handler, losing every diagnostic line after it. The only heap violation ever found inside the crash-handler call tree; fixed by taking `&mut dyn core::fmt::Write` and rendering into the handler's own stack buffer
- `akuma-exec::process::stats::dump()` built the entire `[PSTATS]` line on the heap — a `String` grown in a per-syscall loop, then a second `format!` for the whole line — and handed it straight to `print_str`, on a periodic sweep gated only by `PROCESS_SYSCALL_STATS_ENABLED`, a diagnostics feature meant to stay safe under the memory pressure a heap-free console exists for; now formats into a fixed 224 B stack buffer plus one `safe_print!`
- `memory_monitor`'s `DOUBLE-FREE=` marker was built with `alloc::format!`/`String::new()` and folded into an otherwise heap-free stack buffer — a monitor reporting a PMM/allocator health signal via a path that itself required a healthy heap; now appended conditionally straight into the existing buffer
- `file_page_cache::stats_line()` returned an `alloc::format!` `String` consumed by a single `safe_print!("{}", …)` on the memory-monitor tick; converted to write into the caller's buffer
- `syscall::mem::dontneed_audit_line()` — same shape, same tick, same fix
- Boot self-test `test_poll_bkl_drop` called `ppoll(NULL, 0, NULL, …)` expecting an early return, but that is precisely how musl implements `pause()`; once the "`nfds == 0` is not nothing-to-do" fix landed, `sys_ppoll` blocked on it forever and the self-test suite never reached SSH — hanging every SMP=4 boot at that test. Stale test, not a kernel defect; now passes a zero `timespec`
- `scripts/lockprobe.py` filtered kernel symbols to `0x40100000..0x40400000`, but `.bss` outgrew 3 MB and `KERNEL_LOCK` moved to ~`0x404ce0b8`, so the probe aborted with a misleading `KERNEL_LOCK not found — wrong ELF?` — silently disabling every automatic BKL-storm capture in `j4_selfhost_campaign.py`

### docs/archive/SERIAL_TRACE_TRAFFIC_AUDIT.md
- Three per-event kernel traces (`[IA-DP] file region:` demand-page, `[pipe]` lifecycle, `[mmap]`/`[mprotect]`) printed unconditionally, saturating the single shared UART under a parallel `-j4` build (~270 KB/s, a 115200-baud line ~20x over-saturated) and serializing every logging core on the console lock — turning an in-VM self-host build from "never completes in over an hour" into a 2m21s green run once gated; `DEMAND_PAGE_LOG_ENABLED`, the flag meant to gate the largest of the three, was dead — defined and documented but with zero readers anywhere in the tree — so fixing it required wiring a live check, not flipping an existing one


## Containers (18 fixes, 4 docs)

### docs/archive/BOX_DOCKER_COMPAT.md
- `bootstrap/bin/tar` was never deployed by `userspace/build.sh`, so `/bin/tar` was a busybox applet whose hardlinks go through `link()` — which `sys_linkat` implements as a full file copy that also loses the mode — turning a 1.9 MB busybox layer into 467.7 MB of `0644` copies that a shell's `PATH` search then refused with "Permission denied"; fixed by linking in `akuma_tar`, which applies the archived mode bits (layer store 467.7 MB → 4.1 MB)
- `spawn.rs` registered a spawned process's per-tid exit channel by re-reading `p.channel`, which a concurrent `reattach` had already retargeted at the caller — so a container adopted the *shell's* channel as its identity and its `sys_exit` closed the SSH session; the spawn now registers the channel it created
- a boxed process could mount into its own namespace (and unmount anything but `/`), i.e. mount *over* any path including its own `/proc`; `sys_mount` and `sys_umount2` are now box-0-only, so a namespace is composed entirely from outside before the box runs
- a box's root could be redirected under live processes holding paths resolved against the old one; `replace_pristine_root` now refuses unless `/` is still the birth jail, and re-rooting a box that already has processes is `EPERM`
- tar extraction accepted entries whose paths escaped the target directory (absolute or containing `..`) and had no ceiling on gzip output; escaping entries are now refused and counted (`box pull` fails the layer), and the gzip path caps decompressed output at 512 MB
- an OCI image ships an empty `/proc` and expects something mounted there, so `ps` failed and even `ls /` complained; `box run` now mounts a procfs into the box's namespace from box 0 before the container starts

### docs/archive/TRIM_FAT_DEAD_CODE.md
- `process::kill_box` killed only processes with a matching `box_id` and unregistered that one box, leaving nested child boxes' `BoxInfo` registry entries orphaned and pointing at a dead parent; now snapshots the registry and cascade-kills descendant boxes leaf-to-root via `box_access::cascade_kill_order` before unregistering

### docs/archive/BOX_CONTAINERS.md
- Arguments were not passed to containerized processes

### docs/archive/BOX_ISOLATION_SECURITY_FIXES.md
- `sys_register_box` accepted any box id, name and `root_dir` from any caller — a boxed process could mint a box rooted at `/` (which gets no `SubdirFs`, so its empty namespace falls back to the global mount table) or overwrite box 0's registry entry; now gated on the new `can_register_box`
- `parent_box_id` was hardcoded `None` at registration, so no box ever recorded a parent and every ancestry rule in `box_mod::access`/`hierarchy` was permanently blind; a new box is now recorded as a child of the caller's box, and re-registration preserves the existing parent
- `validate_nested_root` used a bare `starts_with`, accepting a sibling subtree (`/containers/box10` as a "child" of `/containers/box1`) as well as unresolved `..` and relative paths; now matches on a path-component boundary and rejects both
- `sys_spawn_ext` passed `box_id` through unchecked, so a boxed process could spawn a child directly into a sibling's (or any) box — inheriting that box's `box_id`, mount namespace and network routing; now gated on `can_access_box`
- `sys_kill_box` ran no permission check at all: any process could kill every process in any other box; now gated on `can_kill_box`
- `sys_kill_box` removed the victim box's namespace **before** attempting the kill, so a call that then failed (e.g. box 0, which `kill_box` refuses) still stranded a live box without its mounts; the namespace is now dropped only after the kill succeeds
- `sys_set_box_stack` let any process mark any box as rump, routing that box's AF_INET syscalls at a `rump_server` the caller controls; now gated on `can_access_box`
- `sys_umount2` let a boxed process unmount `/` — its own `SubdirFs` jail root — leaving an empty namespace that falls back to the global mount table, i.e. the whole host filesystem, read and write; now refused
- `SubdirFs` concatenated `prefix + path` with no `..` sanitization, contrary to the safety requirement in `BOX_CONTAINERS.md`; `.`/`..` are now resolved and clamped at the virtual root (canonical paths still take the allocation-free stack-buffer path)
- `sys_mount` / `sys_umount2` / `sys_mount_in_ns` did not canonicalize their target, though `MountNamespace` compares mount points literally — an un-normalized target registered a mount point no lookup could match and side-stepped the duplicate check protecting the box root


---

## Files scanned with zero counted fixes (reference docs, open issues, reverted attempts, or pure duplicates of a fix counted elsewhere)

Also re-scanned 2026-08-07: DEVELOPMENT_PRACTICES_REVIEW_AND_ASSESSMENT (pure meta-analysis of process/git history, zero concrete bug-fix content) and BKL_RUSTC_SCALING_BASELINE (re-verified still accurate as perf-not-bugs; its inconclusive `big.rs`-failure investigation is fully resolved later by SMP_SHARED_ONCPU_GATE.md and STALE_THREAD_SLOT_KILL.md, counted there).

Also re-scanned 2026-08-09: CRUSH_MISSING_SYSCALLS, C_STUBS, NEEDLE_SERVER, QJS, and TOP_CORE_COLUMN_PLAN each picked up a one-line "removed as part of a codebase trimming effort" note pointing at TRIM_FAT_PART_3.md; STDCHECK_DEBUG picked up the same note but keeps its existing 1-fix count (Console & Terminal, above) — the note doesn't touch its fix content. None of these notes describe a fix on their own.

Also re-scanned 2026-08-12: CARGO_CRATES_IO_CONNECT_FAIL (root cause isolated, "fix not yet chosen" — five options, none landed), MINIMAL_DEV_BUSYBOX_APPLETS (an applet-coverage survey; its three verification findings — `utimensat` hardcoded to `0`, `getgroups` undispatched, no `/etc/passwd` on the devbox overlay — carry "fix shape" proposals, not fixes), TRIM_FAT_HAND_ROLLED_JSON (an audit of hand-rolled JSON across the tree; the bugs it reproduces in `herd` and `meow` are unfixed), and userspace/box/BOX_RUN (current-state reference; the one fix it mentions is BOX_DOCKER_COMPAT's session-closing bug, counted there). AKUMA_SELF_HOSTING gained only a quick-start section — no change to its count. The same pass found three docs that had never been counted at all, all now listed above: DEVBOX_ISSUES (Misc), and the two deep-dives its Issues 2 and 3 point at, TERM_POLL_INPUT_PREEMPTION_FIX and UART_SMP_INTERLEAVE_FIX (both Console &amp; Terminal).

docs/archive: 4MB_STABLE_AGENT, AI_DEBUGGING, ARCHITECTURE, BKL_DRIVERS_CARVE_OUT, BKL_PHASE7B_PPOLL_CARVE_OUT (piece 2 reverted after A/B caught real corruption), BKL_PHASE7D_THREAD_CONTEXTS (dead/unreachable code removed, not a live bug), BKL_PHASE7F_OPTOUT_LIST, BKL_RUSTC_SCALING_BASELINE, BOX_SUBDIR_FS_LIMITATIONS, C_STUBS, CGI, COMMAND_CHAINING_SSH_BUGS, CONCURRENCY, CONTAINERS_STAGE_1_PLAN, CONTAINERS_STAGE_2_PLAN, CP_MV_IMPLEMENTATION_PLAN, CRUSH_MISSING_SYSCALLS (all gaps, none marked fixed), CWD, DEAD_CODE_ANALYSIS, DEAD_CODE_SWEEP_FINDINGS (findings only, explicitly "nothing here is fixed. No source was edited"), DEV_RANDOM, DEV_ZERO, DOCKER, EMBASSY_REMOVAL, ERRORS_TO_CHECK, EXTREME_STACK_TRIMMING (perf, not bugs), FORKTEST_GO_HANG_FIX (its one fix — the `sys_waitid` ECHILD-on-non-child parentage check — is the exact same 2026-07-22 investigation already counted under SMP_SHARED.md's "forktest_parent (Go) hang" entry), FRANKENLIBC_EVAL, FREEZE_INSTRUMENTATION_PLAN, HEAP_AND_MEMORY_IMPROVEMENTS, HERD, HERD_ADD_AND_PATH_VALIDATION, HIJACK_VS_KERNEL_PROXY (analysis/validation only), IMPLEMENTATION_PLAN (rump phases, milestones only), INTERACTIVE_IO, J4_HANG_LIVE_AUTOPSY (verbatim session record; its 3 fixes are counted once under KTG_STALE_TID_EXIT_STAMP_J4_HANG.md), KILL_COMMAND, LARGE_BINARY_LOAD_PERFORMANCE, LINE_COUNT_ANALYSIS (line-count/dead-code statistics and cross-kernel comparison, not a bugfix), LOCK_REFERENCE, LOOPBACK_TIMEOUT_FIX_PLAN (plan, not landed), MEMORY_LAYOUT (duplicate of AKUMA_SELF_HOSTING §3), MULTIKERNEL, MULTITASKING, MUSL_COMPATIBILITY, NAMESPACES, NATIVE_STACK_INTERNET, NEEDLE_SERVER, NETWORKING_PERFORMANCE_AND_THREAD_SAFETY_ANALYSIS, ON_DEMAND_ELF_LOADER, OOM_BEHAVIOR, OOM_RECOVERY_OPTIONS, PAWS_PLAN, PAWS_TO_SSH_SHELL_PLAN, PHASE01_BUILDRUMP, PHASE1_COMPLETION_BASELINE, PHASE1_NETWORK_LOCK_FOUNDATION, PHASE2_RUMPUSER, PHASE3_KERNEL_TAP, PLAN_SIGSEGV_COMPILE_FIX, POSSIBLE_MEMORY_LEAK, POST_EXIT_PMM_RECLAIM, PROCESS_MEMORY_CLEANUP, PROCFS, PROPER_EXECVE_PLAN, QJS, refactor_plan, RSA_FEATURE_GATE, RUMP_LATENCY_SLEEP_FIX (hypothesis disproven, patches reverted), RUMP_PLUS_HERD, SCHEDULING_TIMING_ISSUES (open/critical, not fixed), SCRATCH, SEPARATE_SHELL_BINARY, SHARED_FD_TABLES, SHELL_ENVIRONMENT_VARIABLES, SHELL_LIMITATIONS, SIGNAL_DELIVERY_FORKTEST_EVIDENCE (summary of fixes counted elsewhere), SMOLTCP_MIGRATION_SUMMARY (duplicate summary), SMP_SHARED_M5_FAULT_LOCK_PLAN, SSH, SSH_PERFORMANCE_FIX_2026, SSH_THREADING_BUG (superseded, duplicate), STRATEGY_A_IMMEDIATE_TUNING, STRATEGY_B_SMOLTCP_MIGRATION (duplicate), STRATEGY_C_IRQ_WAKEUPS, SYSCALL_BLOCKING, SYSCALL_ERRNO_COMPLIANCE_CHANGES, SYSCALL_HARDENING, TCC_LOW_MEMORY, TCP_SEQUENCE_UNDERFLOW_PANIC, TERMINAL_SYSCALLS (duplicate reference), TLS_DOWNLOAD_PERFORMANCE, TLS_INFRASTRUCTURE, TOP_CORE_COLUMN_PLAN, TRIM_FAT_PART_1, TRIM_FAT_PART_2, TRIM_FAT_PART_3 (pure component-removal log, no bugfix content — same shape as TRIM_FAT_PART_2), TWO_VMS_AGENT_DEMO, UNIFIED_CONTEXT_ARCHITECTURE (duplicate of FAR_0x5/THREADING_RACE_CONDITIONS fixes), UNIFIED_PROCESS_ABI, UNSAFE_POINTERS_AND_ATOMICITY, USERSPACE_MEMORY_MODEL, USERSPACE_SOCKET_API, VFS_LOCK_OPTIMIZATION_PLAN, WAIT_QUEUES, MEOW.

userspace: apk-tools/BUILD_NOTES, apk-tools/PIE_LOADER, box/OCI_IMAGE_PULL, box/TESTING (duplicate of libakuma-tls TLS fix), crush/IMPLEMENTATION_DETAILS, forktest/IMPLEMENTATION_PLAN, herd/CORE_AWARE_SCHEDULING, httpd/TIMESTAMPS, libakuma/ALLOCATOR_OPTIONS, libakuma/MKDIR_P_IMPROVEMENTS, libakuma/SYSCALLS, libakuma/TERMINAL_SYSCALLS, meow/CONFIG, meow/HOTKEYS, meow/SHELL, meow/TESTING, scratch/LARGE_FILE_CHECKOUT_OPTIMIZATION, scratch/SIDEBAND_PARSER_FIX (duplicate of docs/archive/SIDEBAND_PARSER_FIX.md), sshd/LIMITATIONS, sshd/MIGRATION_SUMMARY, tar/IMPLEMENTATION_PLAN, tar/STREAMING_EXTRACTION, tcc/DISTRIBUTION_PLAN, tcc/IMPLEMENTATION_DETAILS, tcc/IMPLEMENTATION_PLAN, tcc/LIBTCC1.
