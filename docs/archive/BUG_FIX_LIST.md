# Akuma bugfix audit — itemized list

Counting rule: one item per distinct, dated/named bug confirmed fixed/resolved/implemented.
Plans/proposals with no landed fix, duplicate mentions of the same fix, and narrative
table-of-contents sections are excluded (noted per-file where relevant). Subsystem tags
are assigned per *file* (the dominant subsystem of that investigation doc), not per bullet —
a handful of grab-bag docs (e.g. `AKUMA_SELF_HOSTING.md`, `KERNEL_SPLIT_BUGS.md`) mix bugs
from several subsystems under one write-up.

## Statistics

- **Total distinct fixes counted:** 763
- **Docs contributing at least one fix:** 247
- **Subsystem categories:** 15

| Subsystem | Fixes | % | Docs |
|---|---:|---:|---:|
| Syscall / ABI Compatibility Audits | 132 | 17.3% | 20 |
| Memory & Virtual Memory | 122 | 16.0% | 39 |
| Scheduler & Process Management | 79 | 10.4% | 22 |
| SMP & Locking | 89 | 11.7% | 39 |
| Networking | 58 | 7.6% | 22 |
| Userspace Apps & Libraries | 37 | 4.8% | 20 |
| Rump Kernel & Syscall Proxy | 26 | 3.4% | 6 |
| Toolchain & Self-Hosting | 43 | 5.6% | 7 |
| SSH | 26 | 3.4% | 15 |
| VFS & Filesystem | 27 | 3.5% | 17 |
| Boot & Drivers | 24 | 3.1% | 9 |
| Signals & Exceptions | 15 | 2.0% | 7 |
| Misc / Cross-cutting | 29 | 3.8% | 7 |
| Console & Terminal | 32 | 4.2% | 11 |
| Containers | 24 | 3.1% | 6 |
| **Total** | **763** | **100.0%** | **247** |

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

## Syscall / ABI Compatibility Audits (132 fixes, 20 docs)

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

### docs/archive/REDIS_END_TO_END.md

Same shape as the `*_MISSING_SYSCALLS` docs above — "make one Linux program work" — so it is tagged here per the per-file rule, though three of its ten fixes are Networking and two are VFS. `DEVBOX_ISSUES.md` Issues 14-16 describe the same fixes from the symptom side and are **not** counted again there.

- `connect(2)` handed every call straight to `smoltcp::tcp::Socket::connect`, which rejects any socket that is not `Closed` with `InvalidState` — reported as `ECONNREFUSED`, so the standard non-blocking idiom (`connect` → `EINPROGRESS` → poll → `connect` to collect the result, which is what hiredis and therefore `redis-cli` does) failed against a listener that was up and healthy; the socket's TCP state is now classified first (`connect_step`) — `Established` → success, `SynSent`/`SynReceived` → `EALREADY` for a non-blocking caller or a wait-for-completion that does not re-issue the SYN for a blocking one
- `bind(addr, 0)` on a **TCP** socket stored the literal port `0` (only the UDP arm allocated an ephemeral port), so the following `connect` handed smoltcp `local_port = 0` and got `Unaddressable` — every client that binds before connecting (`busybox nc`, anything setting a source address) failed with what was then reported as `ECONNREFUSED`
- every non-`Established` outcome of `connect` returned `ECONNREFUSED`, the 10-second timeout included, so "nothing is listening", "the connect never completed" and "the local address is unusable" were indistinguishable from userspace — which is what hid the two bugs above behind one symptom; now `ECONNREFUSED` / `ETIMEDOUT` / `EADDRNOTAVAIL` / `EISCONN` / `ENETDOWN` are distinct (`connect_outcome`), and splitting them located the port-0 bug in a single run
- `spawn_process_with_channel_ext` did not honour `#!` scripts (only `do_execve` did), so nothing on the SPAWN abi — herd's services and all of `box run` — could start a script, and every official OCI image's Entrypoint is one; `resolve_shebang_chain` now follows up to 4 hops **inside the namespace override**, because a container's `/bin/sh` exists only in its own mount table
- `exec_shebang` shadowed the interpreter-as-written in the `#!` line with its symlink-resolved target and used the resolved path as `argv[0]`; busybox is a multi-call binary that dispatches entirely on `argv[0]`, so `#!/bin/sh` ran `/bin/busybox` with `argv[0]="/bin/busybox"` and busybox never knew it was meant to be a shell. Both paths now share one parser and one argv rule so they cannot diverge again
- `DEFAULT_ENV`'s `PATH` was `/usr/bin:/bin`, and an OCI image's own `Env` is not propagated through the SPAWN abi (`SpawnOptions` has no env field), so a container's shell could not find the program the image installs under `/usr/local/bin` — every official Entrypoint died on `exec: <prog>: not found`; `DEFAULT_ENV` now carries the full Linux search order
- `getresuid` (148), `getresgid` (150) and `getgroups` (158) were undispatched, and util-linux's `setpriv` treats `ENOSYS` from them as fatal, killing `redis:alpine`'s entrypoint under its `set -e` (this closes the `getgroups` gap `MINIMAL_DEV_BUSYBOX_APPLETS.md` recorded as a "fix shape" rather than a fix)
- `capget` returned success for **any** `hdr.version`; Linux answers an unknown version by writing back the version it does support and returning `EINVAL`, and libcap-ng performs exactly that negotiation by calling `capget` with version 0 to learn the layout — so every later call used a layout the kernel never agreed to, surfacing as `setpriv: activate capabilities: No error information` (musl's `strerror(0)`, i.e. a failure that was not a syscall). Now negotiates properly and reports a full-root set, matching procfs
- `/proc/self/<anything>` did not resolve: the VFS hands procfs the literal path rather than chasing the `self` symlink, so `/proc/self/status` arrived as the string `self/status` and matched nothing, in box 0 and in containers alike, for every file that existed under `/proc/<pid>/`. This is the *actual* root cause behind `LONG_ROAD_TO_REDIS.md`'s "`/proc/self/` is empty" — which was worked around there by adding the missing files, so `/proc/self/smaps` would have failed even once written; fixed properly with `resolve_self`, which leaves the bare `self` symlink alone
- `/proc/<pid>/status` had no `CapInh`/`CapPrm`/`CapEff`/`CapBnd`/`CapAmb` lines, which is where **libcap-ng reads a process's capabilities from** (it does not call `capget`); it returns -1 without setting errno when it cannot. Added alongside `FDSize`/`Groups`, and the four near-identical per-state `format!` arms were merged into one so a future field cannot be added to three of them


### docs/archive/WRITEV_SHORT_WRITE_SPLICE.md
- `sys_writev` did not stop at a **short** write: after an iovec that wrote fewer bytes than it was given, it moved on to the next one, so the tail that never went out was replaced by the following iovec's bytes and the caller — told only a total — resumed from a point that did not correspond to what had actually been written. Short writes are the normal case here (`socket_send` returns whatever fit in smoltcp's 16 KB TX buffer, and it ends with a `poll()` that frees TX space so the *next* iovec usually succeeds), so every socket reply larger than the TX window came out spliced; `redis-cli` reported it as `Protocol error, got "\n" as reply type byte`, and an A/B on the same VM showed 4/16 KiB replies clean while 64 KiB, 256 KiB and 1 MiB corrupted — with the first wrong byte `0x0d` in all three, i.e. the `\r\n` of the next iovec. `sys_readv` had always had the mirror guard

### docs/archive/NCA_MISSING_SYSCALLS.md
- `flock(2)` (syscall 32) was a bare `=> 0` no-op stub — every call unconditionally reported success with zero actual locking, so `sh`'s `flock` applet never errored and a lock-contention probe never saw a second caller block on a first caller's held lock; fixed with real advisory locking (`src/syscall/flock.rs`) keyed by path string, blocking `LOCK_EX`/`LOCK_SH` via 10ms poll-retry, and auto-unlock-on-close wired into both `sys_close` and `SharedFdTable::close_all` — implemented and clean under `cargo check`/`build`/`clippy --release`, not yet re-verified against a booted kernel

### docs/archive/MISSING_NTP_SYSCALLS.md
- `clock_settime` (112), `adjtimex` (171) and `clock_adjtime` (266) were all unimplemented — Akuma could read the clock but never set it, so `date -s`, `rdate`, and `ntpd -q` all had no way to apply a correction; implemented in the new `crates/akuma-time` crate, with `adjtimex`/`clock_adjtime` applying `ADJ_OFFSET`/`ADJ_SETOFFSET` as an immediate step rather than a gradual PLL slew
- On a platform with no RTC (Firecracker exposes no PL031 on aarch64), the guest booted at epoch 0 with no way to correct it, so every outbound TLS connection failed certificate-validity checks ("certificate is not yet valid") even though the CA bundle, DNS, and TCP were all fine; fixed with a boot-time SNTP client (`crates/akuma-time::{boot, sntp}`, wired via `src/ntp_boot.rs`) that runs whenever `utc_time_us()` comes up unset, deriving the wall-clock offset from uptime-relative round-trip timestamps (since the client's own clock has no absolute epoch yet to plug into the classic four-timestamp NTP formula) and applying it via `set_utc_time_us` before IRQs are unmasked

### docs/archive/AKUMA_EXTRACT_SYSCALLS.md
- Converting `sys_sysinfo`'s hand-written `[u8; 112]` byte-offset writes into a `repr(C)` `Sysinfo` struct left its tail padding uninitialized (`#[derive(Default)]` does not zero padding), so the first version handed userspace 4 bytes of live kernel stack on every `sysinfo(2)` call — an info leak invisible to every gate tier because nothing reads those bytes; fixed with a named `_f: [u8; 4]` field plus `defaulted_sysinfo_has_no_uninitialised_bytes`, asserting all 112 bytes are zero
- The syscall epilogue re-used the prologue's process identity across the whole dispatch (Finding A of `IDENTITY_CACHE_SMP_REVIEW.md`, left open there), reachable when `kill_thread_group` retires a `CLONE_THREAD` sibling still inside a blocking syscall and a reclaim drain runs before the epilogue writes — a witness found at depth 2 by exhaustively enumerating `claim`/`retire`/`reclaim` interleavings rather than by a soak; fixed by having the epilogue re-resolve the identity cache after dispatch (`IdentitySource::Reresolve`) and skip its `Process` writes on a miss

## Memory & Virtual Memory (122 fixes, 39 docs)

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

### docs/archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md
- `madvise(MADV_FREE)` did not return `EINVAL`, so allocators that probe for the hint and fall back to `MADV_DONTNEED` on `EINVAL` (jemalloc, mimalloc) instead saw success and used a hint this kernel does not implement, blocking redis's memtest; fixed by returning `EINVAL` (`MADV_DONTNEED`'s own divergence — zeroing the physical frame rather than dropping the mapping, corrupting a CoW/shared peer — is unchanged and tracked via the `dontneed_unaligned`/`dontneed_shared_frame` counters)
- `update_current_user_page_flags` (mmu page-table walk) never checked the `TABLE` bit at L1/L2, so a block descriptor's output address could be misread as a table base and walked as one; found while merging three duplicate page-table-walk implementations into one shared helper, fixed by adding the check `remap_current_user_page`'s copy always had
- three of the four boot-TTBR0 teardown test helpers cleared only the leaf L3 PTE and then freed the page-table frames anyway, leaving the boot L1 pointing at a freed L2 that could be recycled as a new address space's L1 and cause spurious translation faults in later tests; fixed via one `clear_boot_ttbr0_pte` helper (with an explicit `PtClear` depth argument, since one caller genuinely needs leaf-only clearing) used by all four

### docs/archive/COW_PILE_AUDIT.md
- Both EL1 CoW-break paths (`ensure_cow_page_writable`, `try_resolve_el1_cow_fault`) copied the source page's 4 KiB out of `old_pa` *before* taking the lock that the EL0 CoW path's own comment says is required for `old_pa` to stay valid, so a peer core's `munmap` or CoW break could free and quarantine-poison the frame while the copy was still reading it (F1); fixed by moving the copy inside `with_address_space` (`complete_cow_break`'s `TakingAsLock` arm)
- Even after that fix, both EL1 paths still translated the faulting VA and read the CoW refcount *outside* the lock, so `old_pa` could already be stale by the time the hold began (F1b); fixed by re-validating the translation and refcount inside the hold and declining the break — freeing the unused frame — on a mismatch (boot test `cow_break_declines_stale_old_pa`)
- `try_resolve_el1_cow_fault` resolved the CoW-break owner via `read_current_pid()` instead of `address_space_owner_pid_for_fault()` like its two sibling paths, so for a `CLONE_VM` worker thread it operated on the worker's own empty `user_frames` map and its own never-waited-on `as_lock` instead of the thread-group leader's — leaking two physical frames per kernel-side CoW break taken by any threaded process (F2); fixed by switching to the correct owner-resolution function
- `cow_fault_lock`/`cow_fault_unlock` were documented as the cross-PID serialization preventing double-free races in the CoW-break protocol, but they only incremented/decremented a counter nothing else in the tree ever read, so two cores breaking CoW on the same page both "acquired" it and proceeded — the actual correctness came entirely from the unrelated `released_last_va` gate (F3); deleted, with the real invariant documented on that gate instead
- Three of the four sites performing post-fault I-cache maintenance placed the completion barrier (`dsb ish`) *after* publishing the frame (to `file_page_cache` or the PTE) instead of before, so a peer core could fetch instructions from a newly-faulted-in page before the `ic ivau` invalidation had completed inner-shareable — the same defect class as the previously-fixed "x8 race" self-hosting corruption (F4); fixed by routing all six open-coded maintenance sequences through the existing `mmu::sync_icache_range` helper, which retires the barrier before every publication by construction
- A re-entrant `fault_slot_acquire` on the same page by the same thread returned the same `Acquired` variant as a fresh acquire, so an inner guard's release could silently drop the outer guard's hold (F6, established unreachable in this tree but latent); fixed by adding a distinct `FaultSlot::AlreadyHeld` variant that the guard's `Drop` skips releasing on

### docs/archive/USER_COPY_FOLD.md
- `mremap`'s payload move validated the source range and never the destination, so a lazy page in the freshly-created destination mapping faulted mid-copy, the byte loop broke out, and the move silently truncated — indistinguishable from success at the call site; fixed by prefaulting the destination through `copy_to_user`
- `is_current_user_range_mapped` tested leaf-PTE *presence*, not EL0-accessibility, so a mapped **kernel** RAM VA (identity-mapped EL1-only in every user address space) passed validation as a legitimate syscall buffer and the EL1 copy loop wrote through it — silent kernel corruption, reachable only because the user VA allocator happens to avoid the kernel VA range as a layout convention rather than a check; fixed by testing the leaf PTE's AP bits instead (`is_page_user_accessible_ptr`), which also correctly rejects `PROT_NONE` pages while still accepting read-only ones
- `BYPASS_VALIDATION` was a single kernel-wide flag rather than per-thread, so one thread's `store(false)` could close another, unrelated thread's validation-bypass window mid-syscall — surfaced by the AP-bit fix when a woken futex waiter's bypass-close cost the main thread its own window, `EFAULT`ing a legitimate wake and leaving a sibling waiter unwoken; fixed by making the flag per-thread (unchanged `store`/`load` call sites) plus a `BypassValidationGuard` so nested windows restore correctly
- `read_current_pid` dereferenced user VA `PROCESS_INFO_ADDR` gated only on "TTBR0 is not the boot address space", so a live-but-uninitialized address space (several boot-test fixtures construct exactly this via a bare `UserAddressSpace::new()`) had nothing mapped at that address and the read was a wild EL1 access that wedged the VM with no output; fixed by checking `is_current_user_range_mapped(PROCESS_INFO_ADDR, ...)` before the read

### docs/archive/FPCACHE_EVICTION_PREFERS_UNMAPPED.md
- `file_page_cache::insert`'s over-cap eviction took whatever entry sat at the rotating cursor regardless of whether anything still mapped it, so evicting a mapped entry freed nothing and cost the next mapper a full `read_at` — the same "more pressure → more eviction → more I/O" spiral the cache exists to prevent, and the self-host build's cache sat pinned at its cap on every measured arm; fixed by preferring an unmapped victim via a bounded scan (falling back to the cursor entry only if none is found), reusing `shrink`'s existing `cow_ref_get(pa) <= 1` test

### docs/archive/MADV_DONTNEED_SHARED_FRAME.md
- `sys_madvise`'s `MADV_DONTNEED` arm called `zero_mapped_page(va)`, which `memset`s the physical frame directly — correct only while the frame has one holder, but after `fork` the frame is CoW-shared, so the `memset` wrote straight through the peer process's live page (proven with a hand-built shared-frame probe: the peer lost its whole page, 0/4096 bytes survived); this was the mechanism behind the cargo-null-`Rc` self-host heap corruption, since jemalloc/mimalloc fall back to `MADV_DONTNEED` after Akuma's `MADV_FREE` returns `EINVAL`; fixed by breaking the CoW share (installing a fresh private zero frame) instead of zeroing in place whenever `cow_ref >= 2`

### docs/archive/PREFAULT_INODE_STUB_ZERO_PAGES.md
- `ExecRuntime::read_at_by_inode` had been registered as an always-failing `Err(-1)` stub since a March 2026 crate extraction (the real `vfs::read_at_by_inode` needed a `path` argument the hook signature lacked), and its only caller, `prefault_user_range`, silently dropped the error — so every syscall that prefaulted a read-only file-backed lazy page (rustc's rlibs/rmeta, linker inputs) installed a silently zero-filled page instead of the file's real content, 856 times per self-host build; fixed by giving the hook the `path` parameter and wiring it to the real `read_at_by_inode`

### docs/archive/MAPPED_PAGE_PREMATURE_FREE_FIX.md
- `file_page_cache::lookup_and_ref` copied the cache entry under the `PAGES` lock but took the mapper's `cow_ref_inc` only after dropping it, so a free path (`insert`-eviction, `invalidate_inode`, `shrink`) could see the refcount at 1, decrement to 0, and free-and-quarantine-poison the frame inside that window — the late `inc` then resurrected a fresh refcount on the now-poisoned frame, which is the dominant self-host build failure (the linker rejecting a just-written `.rlib` whose bytes decoded as PMM quarantine poison), reproducing on 6/10 clean builds; fixed by taking the `inc` inside the same `PAGES` hold
- `file_page_cache::insert` took the cache's own `cow_ref_inc` after the publish closure and unconditionally, leaking one frame on every lost insert race and leaving a window where a fully-unmapped cache entry still pointed at a frame nothing referenced; fixed by taking the `inc` inside the publish closure, only on an actual insert

### docs/archive/SELFHOST_ZERO_PAGE_HUNT.md
- `sys_munmap`'s whole-region arm freed the frame the region record named instead of the frame the live PTE actually held, so any operation that replaced a mapping without rewriting the region's frame list (a CoW break, `MADV_DONTNEED`'s share-break, a CoW write fault) left `munmap` freeing the wrong frame — a matched premature-free-and-leak pair, 11,255 stale-record hits in one build; fixed by trusting the PTE (`translate(va)`), falling back to the region record only when nothing is mapped
- `mmap(MAP_SHARED|MAP_ANONYMOUS)` behaved exactly like `MAP_PRIVATE` — `fork` CoW-copied the region instead of sharing it, so a child's writes were invisible to the parent; fixed via `process::share_rw_range`, the fork-time counterpart to the CoW-share path that hands the child the parent's PTEs verbatim with no RO demotion
- `LazySource::File` held no reference on the inode number it recorded at `mmap` time, so `open(O_TRUNC)` or `unlinkat` could truncate, free and reissue an inode a live mapping still named — a dependent mapper then read back zero pages, or another file's bytes entirely, which was the root cause of a long-running self-host-build ICE; fixed via `akuma_primitives::InodePin`, a `Clone`/`Drop`-based pin that defers ext2's truncate and bitmap-free until every mapping referencing the inode is gone

### docs/archive/EXTREME_SSHD_KERNEL_STACK_OVERFLOW.md
- The `extreme-size` profile's 64 KB user-thread kernel stack was ~10 KB too small for the userspace-sshd session path (measured 74 KB), so every ssh session ran off the end of its kernel stack and corrupted whatever frame sat below it — usually the session process's own L3 page table, unmapping part of its heap and producing a SIGSEGV with no relationship to the real cause; fixed by raising `USER_THREAD_STACK_SIZE` to 128 KB on `extreme`
- `ENABLE_STACK_CANARIES` painted a canary at every stack base and `check_all_stack_canaries()` existed to check them, but nothing anywhere called it, so a stack overrun that corrupted a page table left no diagnostic trail; fixed by checking the canary at thread teardown and from a periodic idle-maintenance pass for still-live threads, printing `[STACK-OVERFLOW] tid=N ran off its NKB kernel stack`

### docs/archive/AKUMA_SCHEDULING_EXTRACTION.md
- The file-page cache cap was a fixed `RAM/8` with no headroom, so a single mmap'd file larger than the cap (a 532 MB llama.cpp model against a 512 MB cap at `MEMORY=4096`) evicted its own still-mapped hot pages every pass, costing the next mapper of the same file a `read_at` off ext2; fixed with an elastic cap (`FPCACHE_INFLATE_PCT`, default +20%, granted when free RAM clears a 2x headroom threshold and withdrawn only below 1x, hysteresis-gated so a workload parked on the line can't flap the cap) — real for any workload with more than one mapper of the same oversized file (concurrent `rustc`s on one `.so`, boxes sharing a rootfs), though it did not move the single-mapper llama.cpp throughput it was raised to fix

### docs/archive/FPCACHE_MOUNT_IDENTITY.md
(Fixes finding F-1 of `EXT2_WRITEBACK_DESIGN.md` plus the keying half of D-9. D-9's capacity half is still open, and the doc's "a defect found and left alone" is by its own title not fixed here.)
- The file page cache was keyed without any notion of *which* filesystem a page came from, so the same inode number on two different mounts shared one cache entry — a box and the host, or two mounts of different images, could read each other's file data; fixed by assigning each mount an identity at mount time and folding it into the key, with invalidation deliberately left identity-free

### docs/archive/GRANT_RECORDS_VS_DENY_RECORDS.md
- `mprotect(PROT_READ)`/`mprotect(PROT_NONE)` was not enforced across `fork` — the write-fault handler's CoW-break arm fired on `cow_ref > 0` alone and handed the writer a private writable copy regardless of protection; fixed by gating the CoW break on the region's recorded protection
- `MmapRegion::owned()` defaulted `flags` to `NONE`, documented as "protection unrecorded", with no way to distinguish that from an explicit `PROT_NONE` once `flags` gained a denying reader — every region built without explicit flags was silently denied a write; fixed by `MmapRegion::prot_recorded`/`recorded_prot()`
- `update_eager_region_flags` recorded a sub-range `mprotect` against the *whole* region, so `mprotect(PROT_NONE)` on one page also marked its neighbours' pages as denied; `mprotect` now splits eager regions (`akuma_mmap::mprotect_eager_regions_in_range`) so each piece's flags describe only that piece
- `sys_mremap` turned an unknown source protection into an explicit `PROT_NONE` via `old_flags.unwrap_or(NONE)`, denying every `mremap` of a lazy or sub-range source — hit on every allocator `realloc`; fixed to fall back to `MmapRegion::owned()` (unrecorded) instead
- `fork`'s region copy carried only the `flags` value and dropped whether it had ever been recorded, so a child of an unrecorded parent stated an explicit `NONE` and was denied writes its parent would have granted; fixed by carrying `prot_recorded` through the copy too

### docs/archive/AKUMA_EXTRACT_MMAP.md
- `madvise(addr, len, MADV_DONTNEED)`'s zero-range rounding guarded the first addition (`saturating_add`) but not the page-alignment rounding after it, so a `len` near `usize::MAX` wrapped `end` to 0 and produced a ~4.5e15 page count — an unbounded kernel loop inside an `MmBklGuard` window, reachable from unprivileged userspace since `len` was passed straight from a user register with no validation; fixed by validating the range against the user VA limit at the syscall boundary (`madvise::range_fits_user_va`) before any page-count arithmetic, plus saturating arithmetic as a second line of defense
- `MAP_FIXED`'s kernel-VA overlap guard (`fixed_overlaps_kernel_va`) computed `pages * 4096` with no overflow protection, so a mapping length near `usize::MAX` wrapped the computed `map_end` back down to `addr` and the guard answered "no overlap" for a mapping spanning the kernel's own identity map; presented as a hang rather than a compromise, since `sys_mmap` then looped `for i in 0..pages { aspace.unmap_page(va) }` before it could corrupt anything; fixed by validating `len` at the syscall boundary (`mmap::len_too_large`) plus saturating arithmetic

### docs/archive/BUSYBOX_HASH_MISCOMPUTE.md
- `sync_el1_handler` (`src/exceptions.rs`) saved only x0-x3/x29/x30, but the EL1 paths that *resolve* a fault (`try_resolve_el1_cow_fault`, `try_resolve_el1_user_copy_lazy_fault`) `eret` back to re-execute the faulting instruction, so a page fault mid-copy replayed the widened multi-register `stp` store with x4-x18 holding leftover handler state instead of the copy's own live data — busybox `md5sum`/`sha*sum` returned a wrong, non-deterministic digest for an unmodified file roughly 40-50% of the time, for any file over one page; fixed by making the vector transparent to x4-x18, guarded by `test_el1_sync_exception_preserves_gprs`


## Scheduler & Process Management (79 fixes, 22 docs)

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

### docs/archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md
- `write_stdin` (`process/channel.rs`) used the same drop-oldest FIFO semantics as the (already-fixed) stdout side, so stdin past `MAX_BUFFER_SIZE` (1 MiB) was silently dropped rather than backpressured; fixed by making it a short write instead — `sys_write`'s `File` arm returns `EAGAIN` on a zero-byte accept to avoid spinning, and sshd's stdin fd joined the non-blocking set with a residue queue and deferred EOF

### docs/archive/SCHEDULING_INVESTIGATION.md
- Expired sleepers/pollers rejoined the back of the round-robin queue instead of running next, and the 10 ms timer tick meant every sleep/poll deadline paid a full round (~35 ms floor at SMP=1, ~13 ms more per additional runnable thread) — measured as terminal output forwarded in ~27 Hz bursts; fixed via `WAKE_DEADLINE_PREEMPT` (arms the existing `PREEMPT_WAKE_TID` run-next hint from the deadline wake-pass instead of only from `ThreadWaker::wake`) plus dropping `TIMER_INTERVAL_US` 10 000→1 000, profile-gated (`extreme-size` keeps 10 ms) — A/B'd clean on release SMP=1/SMP=4, the 4 MB extreme-size floor, and devbox-smoltcp SMP=4

### docs/archive/SELFHOST_CARGO_BUILD_REGRESSION.md
- `fork_process`'s CoW-fork path collected every sibling thread's eager mmap regions into a fixed `Vec::with_capacity(2048)`, silently dropping regions past the cap with only a warning; a `cargo`/`rustc` build's many concurrently-live worker threads routinely crossed 2048 aggregate sibling regions, and if the dropped set held the child's about-to-be-used heap/stack range, its very next syscall (observed: `chdir()` on `std::process::Command`'s fork-before-exec path) faulted `EFAULT` — fixed with a two-pass collect (count under IRQs-disabled, allocate exactly that size, then collect) instead of a fixed-capacity guess

### docs/archive/IDENTITY_CACHE_LAZY_RESTAMP.md
- The per-thread identity cache added to speed up `getpid` was stamped once at thread-map insert time and never repaired on a lost race, so under thread churn the cache ran at a **0.11% hit rate** — ~556 million slow-path table scans (lock + map walk + IRQ-masked process-table scan) per run instead of the fast path it was built to replace; fixed via a bounded lazy re-stamp on miss (`identity_get` re-runs `identity_store_locked` under an IRQ mask, `MAX_REPAIR_ATTEMPTS = 4`, budget reset only when both the pid and tgid halves have resolved — two earlier reset rules each made an unresolvable entry re-scan the whole table on every syscall instead of exhausting its budget), restoring a 99.999% hit rate

### docs/archive/KTG_GRACE_EXPIRY_KILL_INTERRUPT.md
- `exit_group` paid its full 2 s kill-grace per multithreaded process: a thread parked in an untimed `FUTEX_WAIT` (or any yarn-driven blocking wait) was woken by the deferred-kill request but re-parked, because neither `sys_futex`'s re-evaluation nor `should_interrupt_blocking_syscall` consulted `PENDING_KILL` — both read only signal paths — so the wake was consumed by the very loop it was meant to end and the only exit was grace expiry plus hard kill; fixed by adding the kill check first in both readers, returning `EINTR` that unwinds to the thread's self-termination boundary (boot test `test_pending_kill_interrupts_blocking_wait`, probe `userspace/forktest/c_stress/futexkill.c`)

## SMP & Locking (89 fixes, 39 docs)

### docs/archive/IRQ_HANDLER_TABLE_DEADLOCK.md
- `register_handler` called `gic::enable_irq()` **while holding `IRQ_HANDLERS`**, the non-reentrant spinlock `dispatch_irq` takes from the interrupt vector, so a line delivering on that core before the guard dropped self-deadlocked it — with the BKL still held, surfacing as an `SMP>1` boot freeze with `[BKL] stuck: owner=1 waiter=2/3/4`. Fixed with a fixed `[Option<IrqHandler>; 256]` table (also removing up to 49 `Vec::push` heap allocations inside that lock), publishing under `with_irqs_disabled`, and moving `enable_irq` outside the lock

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
(grab-bag investigation doc chasing a cargo null-`Rc` defect that itself remains open; six bugs were found and fixed along the way — some unrelated to it, some on its own path but not enough to close it)
- §5.1 (D8): `sys_munmap` matched only a single eager region by exact `start_va`, so an unmap starting mid-region or spanning several regions freed only the first match, reported success, left the rest mapped with its VA never recycled, and skipped lazy regions entirely whenever an eager one matched; fixed by extracting `detach_eager_regions_in_range` to drain every region a range touches (9 host tests + a boot self-test)
- §12.4/§12.7: the BKL's out-of-band `acquire_no_ticket` barge (the BKL-free EL0-preempt reconcile) compensated with a `next_ticket.fetch_add(1)` that kept the ticket counters equal in aggregate, but a waiter that lost the ownership CAS at its own turn abandoned its allocated ticket for a fresh one with nothing left to ever advance `now_serving` past the abandoned slot — a genuinely lost FIFO ticket producing `[BKL] stuck owner=0` storms (lock reads *idle* while cores spin) until a 20M-spin self-heal forced recovery; fixed by having a waiter that loses that CAS keep its ticket and keep spinning in place instead of re-ticketing, and by having the barge leave the ticket queue completely untouched on both acquire and release
- §12.2–§12.4: on a multi-threaded `fork()`, every sibling thread that touches the same demoted page faults at once; the first thread through `fault_slot_acquire` breaks CoW and repairs the PTE, but the threads behind it — now holding a fault for a write that is already legal — had no repair path for pages with no lazy/eager region record (an ELF `.data`/`.bss` page from the image loader is never registered as either), so they fell through to a spurious SIGSEGV and took the whole `CLONE_VM` process down with them; fixed via `stale_write_fault_absorbed`, re-reading the PTE at EL0 permission-fault entry and absorbing (invalidate + retry) whenever it already grants the write, budgeted per-(VA, PTE) so a genuinely-declined repair still runs; A/B-verified 10/10 and 8/8 probe SIGSEGVs → 0
- §13.5a: a fatal EL0 fault called `notify_child_channel_exited_pub` *before* tearing the thread group down, so the woken parent's `wait4` could have a peer core reap the crashed process first — `return_to_kernel` then resolved no current process and skipped its entire cleanup block, orphaning every `CLONE_VM` sibling in `FUTEX_WAIT` with a live `Process` row and a pinned address space; fixed by routing all six fatal-fault terminal paths in `src/exceptions.rs` through `sys_exit_group`'s ordering (`kill_thread_group` → close fds → notify parent → self-terminate) via a new `fatal_signal_group_exit` helper (boot self-test `test_fatal_fault_group_exit_precedes_parent_notify`, probe `userspace/forktest/c_stress/segvgroup.c`)
- §13.9: `COW_REFCOUNTS` counts *address spaces* (the first share inserts 2, parent + child) while `user_frames` counts *VAs*, but all three CoW-break sites in `src/exceptions.rs` discarded `remove_user_frame`'s `#[must_use]` bool and called `cow_ref_dec` unconditionally, so an address space mapping one frame at several VAs surrendered its reference on the first break and the next holder's decrement freed a frame still mapped RW — a use-after-free whose quarantine poison then read back as a pointer; fixed by gating the decrement on `remove_user_frame` returning true (boot self-test `test_cow_break_dec_only_on_last_va`)
- §13.9.4: the matching increment side, `cow_share_and_demote_range` (`crates/akuma-exec/src/process/mod.rs`), incremented once per *(VA, PA)* entry, so a parent mapping one frame at several VAs over-counted and the frame outlived its last unmapper (a leak); fixed by taking one reference per distinct PA per address space (boot self-test `test_fork_cow_share_incs_once_per_frame`)

### docs/archive/TRIM_FAT_PROFILES_AND_ACCEPTANCE.md
(latent build-config defects exposed by moving `smp-shared` into the default feature set, i.e. compiling a configuration nobody had compiled before)
- `crates/akuma-exec/src/bkl.rs`'s `KERNEL_LOCK` was gated `#[cfg(kernel_smp_shared)]` while every consumer required `target_os = "none"` too, so the first host build compiling `smp-shared` carried it unreferenced and tripped `dead_code = "deny"`
- `threading::disable_preemption()` called the panicking `runtime()` accessor for a diagnostic timestamp, breaking `PreemptGuard`'s "works in host tests too" contract and failing `akuma-ext2 tests::append_to_file`; now probes `is_registered()` and degrades to `0`
- `crates/akuma-net/src/socket.rs`'s `PreemptGuard` import, consumed only by the `smoltcp`-gated `with_table`, carried no gate of its own, tripping `unused_imports = "deny"` on any build without the native stack — `scripts/build_devbox.sh` had been unbuildable; import now carries its use site's gate

### docs/archive/COW_PILE_AUDIT.md
- The page-table-UAF free gate (`ACTIVE_L0`/`PREV_L0`) only checked TTBR0 values live on cores, not saved thread contexts, so a thread preempted mid-exit — or externally killed — could leave a dying address space's L0 referenced only by its *saved* `ctx.ttbr0`, invisible to the gate, which then freed and quarantine-poisoned it; when that zombie thread was later revived through an existing WAITING-over-TERMINATED overwrite route, the scheduler SGI installed the freed L0 into `TTBR0_EL1` and every subsequent instruction fetch — including the exception vector itself — took a translation fault, wedging the core in a silent recursive fault loop with no console output and no time-jump signal (F8); fixed by adding a saved-context scan (`any_saved_ctx_on_l0`) to the free gate (boot test `test_as_drop_defers_while_saved_ctx_on_l0`)

### docs/archive/PMM_EXTRACT.md
- The CoW/thread-admission probe was being invoked as `bssfork spread=1` across multiple investigation sessions, but `bssfork.c`'s CLI is positional (`rounds threads spread`), not `key=value` — the string `"spread=1"` parsed as `rounds=0` via `strtoul`, so the fork loop never ran and every worker thread was flagged `[never ran]` before the scheduler touched it, chased as a real CoW/frame-allocation regression across several sessions; fixed by running the correct positional form (`bssfork 20 8 1`) and adding it to `scripts/verify_trim.py` as its own exercise alongside the mis-invoked one

### docs/archive/COWSTALE_FORK_THREAD_SEGV.md
- The `[EAGER-UPGRADE]` page-permission repair rewrote AP bits that already said writable — it was really just `update_current_user_page_flags`'s trailing `flush_tlb_page` doing the actual work, dressed up as a permission fix; fixed by checking the PTE first and, when the write is already permitted, only invalidating the stale TLB entry instead of rewriting page-table state that was already correct
- The stale-write-fault absorb above ran only at EL0 write-fault entry, before the loser waited on the per-page fault slot — a queued loser still reached `cow_ref==0` with no region record and died for a legal write (`ap_rw=true`); fixed by re-running the absorb after the fault-slot wait and again at SIGSEGV delivery (SMP=4 in-boot rate ~30-60% → 1/15 hammer storm / 0/8 classic; one hammer survivor with the old signature remains)

### docs/archive/SMP_SECONDARY_IDLE_STACK_CANARY.md
- Every `SMP=N>1` boot printed `SMP-1` false `[STACK-OVERFLOW]` reports: `adopt_current_as_core_idle` registered each secondary's static `.bss` boot stack in the pool but never painted a canary on it, and the sweep cannot tell an unpainted canary from a smashed one — which also left those per-core idle stacks permanently un-monitorable, since the reporter latches per slot on `base`; fixed by painting the canary where the stack is registered (closes `DEVBOX_ISSUES.md` Issue 11)

### docs/archive/SMP_SECONDARY_TICK_KILLED_BY_WFI_PROBE.md
- The boot-time host-WFI probe permanently disarmed every *secondary* core's virtual timer: IRQ 27 is a per-CPU PPI but `irq::register_handler` writes one shared dispatch table, so each secondary's periodic tick landed in `probe_irq_nop` → `akuma_timer::disarm()`, and a secondary arms its tick exactly once at bringup with nothing to re-arm it — cores came online then sat in WFI forever with all work on core 0 (`smp_shared_{scheduler,userspace,migration}` FAILED, `core1=0`); fixed by publishing the probing core in a `PROBING_CORE` atomic so only that core disarms and a secondary re-arms and returns
- The eight `preempt::tests` all operated on thread slot 0 (host builds report tid 0 for every thread) while `cargo test` ran them in parallel, so one test's `disable_preemption`/`scrub_slot` corrupted another's counter under load — and the resulting panic aborted the crate's run, truncating the host suite to 482 tests and masquerading as a commit having disabled tests; fixed by serializing the six state-touching tests behind a poison-recovering `Mutex` that scrubs slot 0 on both acquire and drop

### docs/archive/SMP_ADOPTED_IDLE_SLOT_CLOBBER.md
- `threading::init` stored `FREE` over the `RUNNING` state of thread slots secondary cores had already adopted (self-test image brings secondaries up before init), handing live slots back to the allocator for the next `claim_free_slot` to hand out a second time — fixed by skipping adopted core-idle slots in the state-reset loop
- The same init's stack pre-allocation loop overwrote those slots' `stacks[i]` and `exception_stack_top` with fresh PMM stacks nothing was executing on, so `validate_current_sp`, the canary check and the high-water probe all read the wrong memory for a live core — and it silently vacuumed the boot suite's `spurious == 0` canary assertion, which is why the bug above went unnoticed; fixed by the same `is_adopted_core_idle` guard, with regression `test_core_idle_slots_survive_init`

### docs/archive/CROSS_CORE_THREAD_COLLAPSE.md
- `CNTKCTL_EL1` was never programmed, so EL0 reads of `CNTVCT_EL0`/`CNTFRQ_EL0` stayed trapped to EL1 (~1M pairs/s under a multi-threaded compute workload, 30-80% of every core), and the EC=0x18 trap emulation returned 0 for every register except `CTR_EL0` — userspace's hardware clock was frozen at zero, which turned ggml's spin/park heuristics pathological; fixed by setting `CNTKCTL_EL1 = 0b10` (EL0VCTEN) at boot and secondary bringup plus real-value fallbacks in the trap emulation (llama.cpp decode `-t 1`: 36.0 → 45.6 tok/s)
- The SGI scheduler switch performed an unconditional `tlbi vmalle1` (flushes all ASIDs on the core, including the kernel's own translations) on every context switch, even between sibling threads of the same process sharing one TTBR0 — with a 1 ms tick, two threads alternating on one core paid a full TLB + walk-cache rebuild up to 1000x/s over an mmap'd model's working set; fixed by skipping the `msr ttbr0_el1` + flush when the incoming thread's TTBR0 already matches the live one (`-t 2` decode: 1.61 → 2.5-4.2 tok/s)
- The involuntary-tick idle fallback switched a RUNNING thread out to the per-core idle thread whenever the scheduler scan found no other READY thread, even though nothing else was runnable, so every CPU-bound single-busy-thread bounced core → idle → another core every tick (idle threads billed ~39% CPU each); fixed by returning "keep running" from the fallback instead, gated on the tick having interrupted EL0 (an EL1-interrupted thread still needs the idle bounce as the BKL's release valve)
- Round-robin displaced a RUNNING thread for any other READY thread with no preference for routing that work to an idle core first, so barrier-synchronized worker threads (llama.cpp `-t 2`+) bounced off-CPU every tick while their partner spun waiting on the barrier, degrading to futex park/wake at ~1ms per round trip; fixed with a bounded displacement-immunity mechanism (a thread may decline displacement for up to 4 consecutive ticks unless an idle peer core is found first) — `tg16 -t 2` 2.5 → 21.8-22.3 tok/s (13.5x)

### docs/archive/BLOCKING_RELAX_YIELD_SMP4_REGRESSION.md
- Commit `1a29c9c3` dropped `blocking_relax()`'s leading `yield_now()` for every caller to speed up the socket wait loop (+27% HTTP throughput), but the spawn/exec/reap waiters — woken by another thread on their own core, not by a device interrupt — genuinely need that yield to hand off, so removing it kernel-wide permanently wedged `SMP=4` in the spawn/exec/reap path (boot suite: 294 passed → 23 passed, wedged); fixed by splitting the primitive into `blocking_relax_net` (no yield) wired only into `NetRuntime::blocking_relax`, keeping the yield everywhere else (294 passed, +30% HTTP throughput preserved)

### docs/archive/SECOND_LISTENER_SMP1_FREEZE.md
- At `SMP=1`, a non-idle socket waiter parked via `idle_halt` (the yield-less `blocking_relax_net` path) disabled preemption for the whole halt without ever marking itself WAITING, so it stayed the only RUNNING thread on the sole core, holding it in an uninterruptible `wfi` loop that no timer tick or voluntary reschedule could ever displace — reproduced by starting a second listening server (`httpd`/`nginx`) after a first one was already bound and parked in `accept`, freezing the whole kernel with no panic; fixed by gating the preempt-disable (and matching `HOLD_TAG_IDLE`) on `IS_IDLE_THREAD[tid]`, so a non-idle halter's `wfi` stays preemptible

## Networking (58 fixes, 22 docs)

### docs/archive/AKUMA_NET_SPLIT.md
- `VirtioSmoltcpDevice::receive` built its tx token as `unsafe { &mut *(&raw mut *self) }` while the rx token was live, so two `&mut` to the same device existed at once — UB by the language's rules independently of whether the NIC raced them. The fix is a reorder, and works because `take_rx_frame` returns a raw pointer whose provenance is the BSS frame arena rather than `self`; `LoopbackAwareDevice::receive` had already solved the same problem 200 lines below
- The frame-slot accessors (`rx_buf`, `tx_buf`, `loopback_buf`) were `unsafe fn` whose stated contract was `slot < RING`, computing `base.add(slot * LEN)` with nothing enforcing it — a slot index that desynchronised from its ring wrote past the array into whatever BSS followed, with no fault and no counter. Replaced by `FrameArena::slot_ptr`, which bounds-checks and returns `None`, plus a per-slot borrow flag so a second exclusive borrow is refused rather than aliased
- `MsgHdr` was 56 bytes with only 52 named, the one struct in `akuma-syscalls-linux` carrying *implicit* padding, so the layout test's `transmute` to `[u8; 56]` read four uninitialised bytes. `sys_recvmsg` is safe today only because `read_user_into` overwrites all 56 from user memory first — a property of one call flow, not of the type. Fixed by naming the tail (`_pad3: u32`, no size or offset change) and pinned with a `const` assertion
- `is_valid_handle` transmuted smoltcp's private `SocketHandle` to an index and bounds-checked it against `MAX_SOCKETS`, which covers only one of the two ways `SocketSet::get_mut` panics: an in-range handle whose socket had already been freed by the `pending_removal` sweep reached `None` and panicked, and this kernel's `#[panic_handler]` calls `halt()` — every core stops. Replaced by a membership test against the live socket set, which needs no transmute and catches both

### docs/archive/LONG_ROAD_TO_REDIS_PART_2.md
- `sys_pselect6` passed `None` for its waker, alone among the three poll syscalls, so `select(2)` could only ever wake on the 10 ms `BLOCKING_POLL_INTERVAL_US` tick however fast the peer answered — cargo's vendored libcurl compiles the `select()` branch, so every cargo network wait rode the tick while `poll(2)` callers were woken immediately
- `sys_pselect6` had no `should_interrupt_blocking_syscall()` check, which `epoll_pwait` and `ppoll` both had: a process blocked in `select()` could not be interrupted by Ctrl-C or `kill`, and `alarm()` + `select()` slept through its own signal (the unfixed kernel slept the full 300 ms timeout and returned 0 instead of `EINTR`)
- `dup`/`dup3`/`fcntl(F_DUPFD)` matched only 4 of the 6 `FileDescriptor` variants then `_ => {}`, so `dup(eventfd)` and `dup(rump_socket)` produced unreferenced aliases and the first `close()` destroyed the object under the survivor; the three drifted copies were deleted for one `akuma_exec::process::clone_fd_refs`, shared with `clone_deep_for_fork`
- The `[epoll] ctl` trace in `src/syscall/poll.rs` was **ungated** while every neighbouring trace sat behind a compiled-`false` flag, so it printed on every `epoll_ctl` ADD/MOD — ~40 bytes out the emulated 16550 at one MMIO trap per byte, on the request path, and 99.3 % of a running guest's console output. Gating it took a redis 8.8.0 UNIX-socket round trip from 303 µs to 41 µs; 14 further per-operation traces (`socket`, `connect`, `sendto`, `setsockopt`, unix-socket and timerfd/eventfd creation) were gated with it

### docs/archive/REDIS_ROUND_TRIP_CEILING.md
- Akuma emitted a bare ACK **and** the response for every request — 1.97-2.00 TX packets per RX packet in every `[NICSTAT]` window, at every core count and on both branches — because `socket.set_ack_delay(None)` disabled delayed ACK outright rather than tuning it. Removing the duplicate ACK is **+43 %** on the round-trip ceiling

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


### docs/archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md
- A TCP socket still in `SynSent` was reported read-closed — `is_active() && !may_recv()` is the same pair a peer's FIN produces, so a *connecting* socket raised `EPOLLIN`+`EPOLLRDHUP` and a non-blocking `recv` on it returned `Ok(0)`; a tokio/hyper client that polled inside the one-round-trip SYN window concluded the connection was dead and parked forever **without ever sending its request** (~1 run in 3 at a 64 KiB POST body), fixed by gating both predicates on `tcp_reached_established`
- Blocking TCP `read`/`recv` was capped at an undeclared 30 s (and `write`/`send` at 5 s), returning `ETIMEDOUT` at a deadline no client had set — a 35 s-delayed response died at 30069 ms, and it killed **mid-stream** reads just as readily (a 40 s idle after a good first chunk died at 30125 ms), so the symptom was never specific to the first byte; both now block indefinitely unless the caller sets a timeout
- `SO_RCVTIMEO`/`SO_SNDTIMEO` were accepted by `setsockopt` and silently discarded, with no `getsockopt` arm at all, so a 2 s read timeout actually fired at 30041 ms (the cap above) and the client could not detect the loss; now a real per-socket `struct timeval` with POSIX zero-means-block-forever, `EINVAL` on a malformed value, and a working 16-byte readback
- The `EPOLLET` **write** edge was never re-armed — `epoll_on_fd_drained` reset `EPOLLIN` and had no `EPOLLOUT` counterpart — so a client that filled the 16 KB transmit buffer and waited for `EPOLLOUT` could wait forever; intermittent because `epoll_pwait` drives `smoltcp_net::poll()` at the top of its own loop and usually flushed the buffer before `can_send()` was ever *observed* false. Added `epoll_on_fd_write_blocked`, called from `sendto`/`sendmsg`/`write` on every short write and every `EAGAIN`

### docs/archive/TOKIO_PIPE_EPOLL_HANG.md
- `PipeRead` ignored `O_NONBLOCK` and blocked instead of returning `EAGAIN`, and a pipe's `EPOLLET` `EPOLLIN` edge was never re-armed on read/EAGAIN (nor was `EPOLLHUP` ever reported once the last writer closed), so any edge-triggered reader (tokio/mio, and therefore `nca`) that drained a child's stdout then went back to `epoll_wait` for EOF hung forever, looking like a healthy-but-stuck process; fixed by honoring `fd_is_nonblock` in `PipeRead`'s `sys_read` arm and calling `epoll_on_fd_drained` on every pipe read and `EAGAIN`, plus reporting `EPOLLHUP` once the last writer is gone
- The `Stdin` arm of `sys_read` had the identical two defects (no `O_NONBLOCK` check, no `epoll_on_fd_drained` re-arm), independently and never carried over from the `PipeRead` fix above — since `nca`'s keystroke path also goes through edge-triggered epoll (crossterm's default `mio` backend) on fd 0, a read after draining one keystroke blocked inside the syscall for the full idle gap until the next one arrived, manifesting as multi-minute input freezes; fixed the same way, mirroring the `PipeRead` fix in the `Stdin` arm

### docs/archive/AKUMA_NET_ISSUES.md
- No virtio-net RX interrupt was registered at all (`src/main.rs` enabled only the timer IRQ) because `gic_v3::enable_irq`'s SPI arm only wrote `GICD_ISENABLER`, whose reset state is Group 0 while the kernel only enables Group 1 — an SPI configured that way never reaches the CPU; every packet wait therefore fell back to the 3 ms timer tick (measured 4.9-5.2 ms average park); fixed by programming `GICD_IGROUPR`/`GICD_IPRIORITYR`/`GICD_IROUTER` before `GICD_ISENABLER`
- An SPI is delivered to one core only, so a `blocking_relax` waiter halted on a different core never saw the NIC's interrupt and slept to the next tick regardless; fixed by broadcasting the scheduler SGI from `nic_irq_handler`, rate-limited by a `NIC_WAKE_PENDING` doorbell coalescer so a burst rings once per drain instead of once per packet (median request latency 2,148us → 645us)
- Socket slots exhausted under a round-trip HTTP workload (26% connection errors at ~175 req/s) — a closed socket sat in `pending_removal` until TCP teardown or the 30s `SOCKET_GC_TIMEOUT_US` expired, and connection-per-request traffic retired sockets faster than that GC allowed; fixed with pressure-driven socket reclaim (26% errors → 0%)
- The netpoll drain re-armed the NIC doorbell (`NIC_WAKE_PENDING.store(false)`) *after* draining, not before — a packet arriving between the last `poll()` returning false and the re-arm found the doorbell already set, so the interrupt handler raised no broadcast and the packet's wake was then erased by the re-arm, leaving every core to sleep to the next tick; fixed by re-arming before the drain instead of after (+65% req/s, p99 2.3x-2.7x tighter, p50 to Linux parity)

### docs/archive/CARGO_CRATES_IO_CONNECT_FAIL.md
- `sys_pselect6` never wrote the caller's `exceptfds` set back, so any `select(2)`-based caller reading it after a call saw whatever was passed in as still set; the nightly musl toolchain's vendored libcurl (built without `HAVE_POLL`) uses the `select(2)` branch and puts a connecting socket in `exceptfds` to watch for `POLLPRI`, so `FD_ISSET` stayed true, libcurl synthesised a false `POLLPRI`, mapped it to `CURL_CSELECT_ERR`, and discarded sockets that had already reached `Established` with `SO_ERROR == 0` — `cargo fetch`/`build` looped on "spurious network error" forever; fixed by zeroing `exceptfds` on both the ready and timeout path (`src/syscall/poll.rs`)

### docs/archive/SMOLTCP_STALE_CONNECTING_HANDLE_PANIC.md
- `socket_close` queued a closed socket in `pending_removal` for GC but never purged it from `net.connecting` (the list `poll()` uses to enforce the non-blocking-connect timeout), so a socket closed while still `SynSent` sat in both lists at once; the next `poll()` freed its `SocketSet` slot via the `pending_removal` sweep, then the `connecting` sweep immediately after dereferenced the now-freed handle and smoltcp panicked unconditionally — deterministic for any non-blocking `connect()` closed before the handshake finishes, not a rare race; fixed by purging the `connecting` entry in the same step `socket_close` queues the handle for removal

### docs/archive/UNIX_SOCKET_IMPROVEMENTS.md
- `SHUT_RD` on a unix socket returned 0 immediately instead of draining already-buffered bytes first, silently discarding a complete message the peer had successfully sent (Linux returns the buffered bytes, then EOF)
- `UnixTable::pair` never set `peer_creds`, so `SO_PEERCRED` reported pid 0 for both ends of every `socketpair`, breaking any daemon that identifies its peer by pid
- `bind` on a unix socket path created a plain regular file (`mode=0o100644, S_ISSOCK=false`) instead of a real socket node, so a client that checks `S_ISSOCK` before connecting — the normal thing to do — refused to talk to a working socket; fixed by adding `S_IFSOCK`/`EXT2_FT_SOCK` and a real `create_socket_node` path
- `socket(AF_UNIX, ...)` returned `EAFNOSUPPORT` inside a `stack=rump` box because `rump_proxy` intercepted every socket-family syscall and forwarded it to NetBSD's sysproxy, which has no AF_UNIX; fixed by letting AF_UNIX fall through to the native path (a unix socket has no wire, so this doesn't weaken the proxy's network-isolation guarantee)
- `recvmsg`/`getsockopt`/`setsockopt` on a unix socket answered `ENETDOWN` on the rump-only build because the syscall-ungating list missed those three, so a unix socket got a *network* error for having no network
- the rump-only devbox target had not compiled for some time: four `#[cfg(feature = "smoltcp")]` gates were lost, the worst in `akuma-net/src/lib.rs` where a doc comment and a `pub use` inserted between the attribute and `pub mod smoltcp_net` silently relocated the gate onto the re-export, producing 40+ "unlinked crate `smoltcp`" errors from `scripts/build_devbox.sh`; all four gates restored

## Userspace Apps & Libraries (37 fixes, 20 docs)

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

### docs/archive/LIBAKUMA_AUDIT.md
- `libakuma::fstatat` passed the raw `&str` pointer with no NUL terminator — the one path-taking wrapper of fourteen that skipped `format!("{}\0", path)` — so the kernel's `copy_from_user_str` walked past the string into adjacent memory (usually harmless, occasionally EFAULT, rarely a different file); its only caller had been pre-terminating by hand, which is now dropped

### docs/archive/PAWS_DUPLICATED_ARGV0.md
- `paws` passed its full argument vector — including `args[0]`, the command name it had already consumed for its own path lookup — straight through to `spawn`, which itself prepends the resolved path as `argv[0]`, so every child process received its own name twice in `argv`; invisible for multicall binaries like busybox (which expect and re-dispatch on a leading applet name) but fatal for `tcc`, which read the duplicate as an input file (`"file 'tcc' not found"`); fixed by `.skip(1)`-ing the command name at all four of `paws`'s spawn sites

### docs/archive/SCHEDULING_INVESTIGATION.md
- `nca`'s `--no-tui` `Repl::run()` (an `async fn`) called reedline's blocking `read_line()` directly with no `spawn_blocking`, monopolizing one of tokio's 4 worker threads for as long as a keystroke was pending and starving every other task scheduled on it (event-fanout, IPC command consumer, subagent consumer); fixed by moving the call onto `tokio::task::spawn_blocking` (`crates/tui/src/repl.rs`)
- `nca`'s TUI event bridge (`spawn_tui_bridge`, `crates/tui/src/tui/bridge.rs`) called a blocking `std::sync::Mutex::lock()` directly inside a `tokio::spawn` task once per event; the render loop held that same lock across its own synchronous `terminal.draw()` pty I/O, so a slow draw blocked the bridge's tokio worker on the lock and the render loop's own input `poll()` — sequenced after its critical section on the same thread — never got reached, producing multi-minute input freezes; fixed by moving the lock-and-mutate onto `tokio::task::spawn_blocking`


## Rump Kernel & Syscall Proxy (26 fixes, 6 docs)

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
- `box use` accepted a bare hex box id only with a `0x` prefix, so pasting the id exactly as `ps` prints it (hex, e.g. `185c61f8b7`, while `/proc/boxes` writes decimal) fell through to a name lookup and missed; `boxes::resolve` now takes the name, `0x`-prefixed hex, bare hex and decimal, with **bare hex tried last so a name that is also valid hex stays a name** (`db` = 0xdb = 219 and is a real box name). An all-digit hex id still reads as decimal by construction — use the `0x` form

### docs/archive/ARCHITECTURE_QUESTIONS.md
- `ifcreate` hang — `rumpuser_clock_sleep` didn't release the rump CPU around its sleep

### docs/archive/RUMP_SSHD_FORKED_SESSION_CLOSES_SOCKET.md
- On the rump devbox, every ssh session reset at kex (`kex_exchange_identification: Connection reset by peer`): `RumpSocket` was the one fd family `clone_deep_for_fork` did not refcount, so a forked sshd session's parent `drop(stream)` closed the socket out from under its own still-running child; fixed by refcounting `RumpSocket` the same way every other fd family already was (superseded the wrong DHCP-path diagnosis in `DEVBOX_ISSUES.md` Issue 10)


## Toolchain & Self-Hosting (43 fixes, 7 docs)

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

### docs/archive/USERSPACE_BUILD_SH_OUT_OF_WORKSPACE_MEMBERS.md
- `userspace/build.sh` still built the four submodule-backed members with `cargo build -p <name>` after they were dropped from `userspace/Cargo.toml`'s `members`, so `set -e` killed the script at `meow` (10th of 21) and `tcc`/`tar`/`sshd`/`llama-cpp`/`wavplay`/`scratch`/`nca` were never built at all — `nca`'s `-p` name had never been right either, its package is `native-cli-ai`; now built through `--manifest-path` from an explicit member table that also skips a member whose submodule is absent
- `llama.cpp` and `nca` carried no `[workspace]` table (unlike `meow`/`tcc`), so building them standalone failed with "current package believes it's in a workspace when it's not", and excluding them in `userspace/Cargo.toml` only moved the identical error up to the repo-root workspace; fixed with an empty `[workspace]` in each manifest
- the repo-root `.cargo/config.toml` contributes a *relative* `-Clink-arg=-Tlinker.ld`, which rustc resolves against its cwd — the workspace root — so `meow`/`tcc` built through their own manifests died at the link step with "cannot find linker script linker.ld"; fixed by passing an absolute `-T` via `CARGO_ENCODED_RUSTFLAGS`, which *replaces* the config-file rustflags where a `--config` override would merge with them and keep the relative path
- the binary copy loop looked under `userspace/target/` for every member, but a member that is its own workspace has its own `target/`, so a freshly built `meow`/`tcc` was never copied into `bootstrap/bin`
- `llama-cli` and `nca` were in the copy list even though their own build scripts install them into `bootstrap/bin`, so every successful build printed `Warning: Binary … not found` for both
- every path in the script is relative to `userspace/` while the documented invocation is `userspace/build.sh` from the repo root, where the first `cargo build -p libakuma` resolved against the kernel workspace and failed; fixed by anchoring with `cd "$(dirname "$0")"`, which also fixes toolchain/target resolution for the out-of-workspace members

### docs/archive/RAW_BLOCK_DEVICE_FD.md
- Adding `crates/akuma-scheduler`'s `sched-sim` CLI binary to the workspace's `default-members` made a bare `cargo run --release` (and every `overlays/devbox/run*.sh` script) fail with "could not determine which binary to run" since cargo now had two candidate binaries; fixed with `default-run = "akuma"` in the root `Cargo.toml`, disambiguating `cargo run` without touching `cargo build`/`cargo test`

### docs/archive/EXT2_WRITEBACK_FOLLOWUP_FIXES.md
(§2's three `#[allow(...)]` removals and §3's six clippy warnings are cleanup with no defect attached; §7 answers a question, §8 records host wall-clock drift, and §9 retires D-4's premise — none is a fix.)
- `extreme-size` did not build
- A test in the write-back suite asserted nothing, so it passed regardless of the behaviour it named
- `ext2probe-host` could never be built `--release` — the profile had never been exercised, so the release path was broken on first use
- `cargo build --release` left a stale `akuma.bin` behind: the flat binary is produced by a separate step, so a plain `cargo build` silently kept the previous image and every boot ran the old kernel
- The linker wrapper would have broken the self-hosted build

## SSH (26 fixes, 15 docs)

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

### docs/archive/SSHD_CHANNEL_WINDOW_NEVER_ADJUSTED.md
- sshd advertised a 1 MiB inbound SSH channel window at channel-open and never sent `SSH_MSG_CHANNEL_WINDOW_ADJUST`, so no session could ever carry more than 1 MiB of stdin — a transfer at exactly that size hung with no error until the client's own timeout fired; the same `0x100000` number as `MAX_BUFFER_SIZE` had coincidentally been hiding a second, independent stdin-overflow bug (`TRIM_FAT_EMBARASSING_DUPLICATIONS.md` Phase 0 item 5); fixed by sending the window adjustment (verified with 4 MiB through `cat` and 8 MiB through `sha256sum`, both byte-exact)

### userspace/sshd/docs/CLIENT_REAL_SERVER_INTEROP_FIX.md
- The `ssh` client's handshake-phase call sites (`CHANNEL_OPEN` confirmation wait, `expect_channel_reply` for `pty-req`/`exec`/`shell`, and `authenticate`'s auth-response waits) each read exactly one packet and treated any other message type as fatal, so a real-world server interleaving `SSH_MSG_GLOBAL_REQUEST` (OpenSSH's `hostkeys-00@openssh.com`), `SSH_MSG_CHANNEL_WINDOW_ADJUST`, or `SSH_MSG_USERAUTH_BANNER` (all legal per RFC 4253/4252 at those points) killed the connection with "expected X, got message type N" even though nothing was actually wrong; fixed by looping all three call sites on those message types the same way the client's own interactive `pump` loop already did, including applying `WINDOW_ADJUST`'s send-window credit instead of discarding it — verified against a real OpenSSH server and `late.sh`, which previously failed at each of the three message types in turn

### userspace/sshd/docs/SECURITY_IMPROVEMENTS.md
- `take_unencrypted_packet`/`take_encrypted_packet` indexed the message-type byte without checking `packet_len` was large enough for it to exist — a 5-byte packet panicked (`panic = "abort"` kills the whole process), reachable pre-auth by any TCP peer on the unencrypted path and post-auth by a malicious server on the encrypted one; fixed by rejecting `packet_len < 2` before either index
- The same two functions could compute `payload_len = packet_len - padding_len - 1 == 0` from a legitimate-looking length field and then slice `[6..5]` (`start > end`), an unconditional panic in Rust regardless of buffer contents; fixed by rejecting `payload_len < 1`
- The `Option`-returning packet parser conflated "not enough bytes yet" with "these bytes are fully buffered and permanently malformed (bad MAC, bad length)", so a caller that treated both as "keep waiting" spun forever re-parsing the same stuck bytes — a pre-auth hang for a MITM that corrupts one byte during the unencrypted phase; fixed with a three-state `TakePacket` enum (`Ready`/`Incomplete`/`Malformed`) so `Malformed` now disconnects instead of looping
- The ephemeral X25519 KEX secret and the persisted Ed25519 identity key both drew from `SimpleRng`, a 64-bit xorshift PRNG (fine for its original non-secret uses — KEXINIT cookie, anti-fingerprinting padding — but collapsing a 256-bit key's effective security to ~64 bits); fixed by pulling both from real hardware entropy (`getrandom()`) instead
- `Connection::input_buffer` had no size cap, so a peer claiming an enormous `packet_len` and trickling bytes (or never finishing) could grow it without bound — a memory-exhaustion DoS; fixed with `MAX_INPUT_BUFFER` (1 MiB), checked after every socket read on both the blocking handshake path and the non-blocking interactive pump
- `read_version_line` looped on pre-version banner lines (legal per RFC 4253 §4.2) with no cap on the *count* of lines, so a hostile peer that never sent a line starting with `SSH-` could hold the handshake open indefinitely; fixed with `MAX_BANNER_LINES = 100`
- `generate_and_save` (new identity key) and `add_known_host` (TOFU acceptance) discarded the `Result` of their `mkdir_p`/file-write calls, so a silent persistence failure meant a freshly generated identity key was reported saved when it wasn't (next connection silently generates a different key) or a TOFU-accepted host key was "forgotten" (next connection re-prompts as if the host key changed); fixed by checking the `Result` and warning with the path and errno on failure
- `sshd.conf`'s `disable_key_verification = true` bypassed `publickey` auth entirely for any client, reachable by one typo or one copy-pasted dev config in a binary nobody built with that in mind; fixed by gating it behind a new off-by-default Cargo feature (`insecure-disable-key-verification`) — without the feature the flag still parses but is ignored, with a loud warning naming why
- The interactive pump's `CHANNEL_DATA`/`CHANNEL_EXTENDED_DATA`/`CHANNEL_WINDOW_ADJUST`/`CHANNEL_REQUEST`/`CHANNEL_CLOSE` handling never checked the packet's `recipient channel` field against `LOCAL_CHANNEL`; fixed by validating it explicitly and skipping a mismatched frame instead of acting on it
- `zeroize` (zeroes `ed25519-dalek` key material on drop) was off for both `sshd` and the `ssh` client via a shared `default-features = false` on `akuma-ssh-crypto`, leaving the host key and identity/ephemeral KEX secrets able to linger in freed heap memory after use; fixed by re-enabling `zeroize` explicitly for both binaries while leaving the unrelated `fast` (speed-only) feature off

---

## VFS & Filesystem (27 fixes, 17 docs)

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

### docs/archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md
- ext2 thread hooks were read/written through a bare `static mut` with no synchronization — a genuine data race between the hook-registering thread and readers retrying a lock acquisition; fixed by reusing `akuma-exec`'s existing lock-free `OnceCopy<T>` cell (release store at `init_thread_hooks`, acquire load at each read) rather than inventing a second mechanism
- `test_openat`'s pre-test clean-slate step and its post-test teardown tracked different file lists — the symlink case's `link.txt`/`target.txt` were only in teardown's list, so a crashed run's leftover symlink inputs survived into the next boot's clean-slate step; fixed by one shared `LEFTOVERS` list used by both

### docs/archive/NCA_FD_NONBLOCK_TOCTOU.md
- `sys_close` cleared an fd's `cloexec` flag immediately after freeing its table slot but its `nonblock` flag only after the slot's resource-cleanup match arm ran; in that window a concurrent thread on the same shared fd table (`CLONE_THREAD`) could `alloc_fd` the exact same fd number for an unrelated new pipe, inherit the previous occupant's stale `nonblock` bit (or have its own legitimately-set flag wiped out by the closer's now-late clear) — real `cargo`/`rustc` spawns from cargo's multi-threaded jobserver hit this as a spurious `EAGAIN` on what should be a blocking child-error-pipe read (24/40 failures reproduced under 4 concurrent threads); fixed by clearing `nonblock` immediately alongside `cloexec` in `sys_close`/`sys_close_range`, copying `nonblock` onto `newfd` in `sys_dup3` (which shares the open-file-description on real Linux but didn't propagate the flag here), and defensively clearing both flags in `alloc_fd_from` before a freed fd number is ever reissued

### docs/archive/APK_OTMPFILE_DIR_FD.md
- `sys_openat` neither implemented `O_TMPFILE` nor rejected write-mode opens of directories, so apk-tools 3's atomic-write path (`openat(dirfd, ".", O_RDWR|O_TMPFILE)`) silently succeeded with a writable fd on the directory itself, and every subsequent `write()` failed `EISDIR` at the wrong syscall — `apk update`/`apk add` installed files but never wrote the database; fixed by answering `O_TMPFILE` with `EINVAL` (apk falls back to `.tmp`+`renameat`, which works) and write-mode opens of an existing directory with `EISDIR` at open() time (closes `DEVBOX_ISSUES.md` Issue 20)

### docs/archive/DEVFS_MISSING.md
- `/dev` had no real directory backing — `ls /dev` showed nothing, and `stat`/`newfstatat`/`statx` only recognized `/dev/null` and `/dev/zero` via two independently-hardcoded copies, so `/dev/random`, `/dev/urandom`, `/dev/dsp`, and every block device all `stat()`ed `ENOENT` despite `open()`ing successfully; fixed with a single device table (`crates/akuma-vfs/src/dev.rs`) wired into `list_dir`/`metadata`/`exists`, replacing the duplicated `sys_newfstatat`/`sys_statx` special-casing — boxes deliberately get no synthetic `/dev` entries at all (except `null`/`zero`/the rump tap)

### docs/archive/RAW_BLOCK_DEVICE_FD.md
- `open()` on a `/dev/vdX` block-device node returned `ENODEV` unconditionally since a raw block fd had no consumer; fixed with a `BlockDev` file-descriptor variant wired through `read`/`write`/`lseek`/`fstat`, with a write-open of a *mounted* device refused `EBUSY` (checked once at `open()` time via `device_is_mounted`) so a raw write can't go behind `Ext2Filesystem`'s block cache

### docs/archive/EXT2_PER_FD_INODE_READ_PATH.md
(The per-fd inode cache itself is a read-path optimization and is not counted; these are the four pre-existing defects that had to be fixed before an fd could hold an inode across calls, found by asking "what else can free an inode a reader still names?")
- `read_at_by_inode` did not refuse directories, so a `read(2)` on a directory fd walked directory blocks as file data instead of returning `EISDIR`
- `rename` freed its destination inode with no pin check, so an fd holding that inode was left naming a freed number — the same class the per-fd cache would have made reachable on every open file
- `truncate_inode` reads `direct_blocks` as block numbers, but a fast symlink stores its target *string* there; `remove_file` guarded against it and `rename` did not, so renaming a file over a symlink freed whatever blocks the target characters happened to spell
- `rename(a, a)` (the same path twice, or two hard links to one inode) unlinked the shared inode, dropped its last link, freed it, then re-added a directory entry pointing at the freed number — `mv a a` destroyed the file and left a dangling entry, where POSIX requires a successful no-op

### docs/archive/EXT2_PERFORMANCE_AUDIT.md
(2026-08-26 follow-up only. Fixes A, B and D-lite of that section are write-deferral/zero-fill optimizations with no defect attached and are not counted; these two are the defects found alongside them.)
- `write_dir_range`'s cross-block `rec_len` merge was latent-wrong when a dirent edit spanned a block boundary
- `add_dir_entry` / `remove_dir_entry` rewrote every block of the directory per call via `write_inode_data`, making filling or emptying a directory O(N²) in its size rather than O(1) per edit

### docs/archive/UTIMENSAT_STUB_TOUCH.md
- `utimensat` (`nr::UTIMENSAT`) was a bare `=> 0` stub that always reported success, so busybox `touch`'s ENOENT-then-create idiom never fired and `touch newfile` exited 0 while creating nothing — a stub whose success return actively suppressed the file creation; fixed with a real handler (`fs::sys_utimensat`) returning `ENOENT`/`EBADF`/`EFAULT`/`EINVAL` from the real dirfd/path/times validation, matching Linux's argument-before-path-lookup ordering
- `akuma_vfs::Filesystem` had no operation to set an inode's timestamps at all, so `touch -d`/`touch -t` silently changed nothing and a plain re-`touch` of an existing file never refreshed its mtime (`make` would never see a target as newer); fixed via `Filesystem::set_times(path, atime, mtime)` (each an `Option`, `None` meaning `UTIME_OMIT`) implemented in ext2 (mirroring `chmod`, deliberately not bumping mtime for an atime-only call) and the mtime half in memfs

## Boot & Drivers (24 fixes, 9 docs)

### docs/archive/AKUMA_FIRECRACKER_KVM.md
(The Firecracker/KVM port. §3.1's `GICD_IROUTER` aliasing is counted under `GICD_IROUTER_ALIASING.md` below, the deep-dive it points at; §3.11 is explicitly "not a bug" (two hypervisors racing for host port 2222); §5.2 (nondeterministic `akuma_net::init` hang) and §5.3 (spinning DHCP settle loop) are open.)
- §3.2 `TPIDRRO_EL0` — KVM hands a vCPU the poison value `0x1de7ec7edbadc0de` where QEMU zeroes it, so `current_tid()` read it as a tid and halted the core before `threading` had installed a real one; zeroed with `msr tpidrro_el0, xzr` at both entry points (`boot.rs` for the BSP, `secondary_entry_shared` for PSCI-woken secondaries)
- §3.3 `ramfb::init` took a data abort (`EC=0x25`, `FAR=0x8000012008`) touching an fw_cfg selector register on a machine that maps nothing there — Firecracker has no fw_cfg device, and unlike QEMU's clean "file not found" the register access itself faults; fixed with a compile-time `AVAILABLE` gate on both public entry points of `src/fw_cfg.rs`
- §3.4 The kernel-text tripwire range was written with a stale `0x6000_0000` upper bound, so at `KERNEL_PHYS_BASE = 0x8030_0000` it inverted to `0x8030_0000..0x6000_0000` — permanently empty, making `kernel_text` always false and flooding every timer tick with `[IRQ POISON]`; five sites in `src/exceptions.rs` shared the literal
- §3.5 The same inverted range in `akuma-exec`'s scheduler fired `[SGI-S POISON]` on every context switch, and carried a **fourth** mirrored copy of the kernel load address — dead, never referenced, hidden by the module's `#![allow(dead_code)]`; deleted, and both sites now read one runtime window (`akuma_exec::mmu::is_kernel_text`, installed once via `set_kernel_text_window`)
- §3.6 Firecracker validates the virtio status handshake and QEMU does not, so a driver that jumped straight to `DRIVER_OK` left block init *looking* healthy while no request ever completed; fixed without forking the dependency by `SteppedMmioTransport` in `crates/akuma-virtio/src/transport.rs`, which overrides `set_status` alone to walk the status bits in order
- §3.7 `SCTLR_EL1.SA0` — KVM's reset value enables EL0 SP-alignment checking where QEMU's does not, killing every userspace binary deterministically at the same instruction; cleared as a single bit rather than by reconstructing `SCTLR_EL1`, which would hand-roll the architecturally RES1 fields. Whether the initial user SP is genuinely misaligned is left open
- §3.8 Firecracker attaches no entropy device unless the config says `"entropy": {}`, and without it three boot-suite tests fail on `getrandom` returning `EIO` — a runner-config omission that reads as a kernel bug because QEMU's runner always supplies one; `run.sh` now always attaches it (290/0/0)
- §3.9 The virtio-net header is 12 bytes under `VERSION_1`, not 10, and `virtio-drivers` 0.7.5 sized it by `MRG_RXBUF` — shifting every received frame two bytes left and presenting first as "ext2 mount hangs", then as "DHCP doesn't work"; fixed by bumping to 0.13.0, at a cost of ~9 mechanical retypings across `akuma-virtio`
- §3.10 On a 4 GB microVM Firecracker places the FDT around 6 GiB while `boot.rs` statically maps `[0, 3 GiB)`, so reading `x0` faulted before the kernel printed a word about memory — and `extend_boot_ram_identity_map` cannot help, since it needs the RAM size the FDT is being read to discover; fixed with `mmu::ensure_boot_identity_covers(dtb_ptr)` immediately before `detect_memory`
- §3.12 Removing `max_level_off` from `akuma-net`'s `log` dependency resurrected 25 previously-dead `log::` statements, several inside preemption-disabled sections, wedging a single-vCPU guest — a print is a lock acquisition, and one inside a section that disables preemption has nothing to yield to on a one-core machine
- §5.1 Inbound RX never reached the guest: Firecracker's virtio-net reads nothing off the host tap until the **total** capacity of posted receive descriptors reaches `MAX_BUFFER_SIZE` = 65562 bytes, so a single 2 KB buffer left every inbound frame dropped into `no_rx_avail_buffer` with no guest-visible error — TX correct on the wire, `DHCPOFFER` answered, no `DHCPREQUEST`, host ARP unanswered. `RX_BUFFER_LEN` is now 65568; verified end-to-end 2026-08-21 (DHCP lease + operator SSH session) per `AKUMA_FIRECRACKER_TERRAFORM.md` §10

### docs/archive/GICD_IROUTER_ALIASING.md
- `GICD_IROUTER` writes landed on the **redistributor**: the distributor was mapped as a single 4 KiB page while `GICD_IROUTER` lives at offset `0x6000`, making `DEV_GIC_DIST_VA + 0x6000` exactly `DEV_GICR_SGI_VA`. Latent on QEMU only because its `GICD_IROUTER` resets to 0, which targets core 0 — the value the code wanted — and would corrupt redistributor state for real at INTID >= 128. Fixed by giving each device a *span* rather than a page in `akuma_primitives::addr`, plus a `const` no-overlap assertion (`DEV_WINDOW_NO_OVERLAP`) and two host tests; the predecessor test compared base addresses only, which is why a 64 KiB device declared as one page went unnoticed

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

### docs/archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md
- `rng.rs`: `copy_len` was clamped to the caller's requested remaining length rather than `to_read` (the actual descriptor-completion length), and a `copy_len == 0` completion spun the outer loop forever since `bytes_read` never advanced; fixed by clamping to `to_read` and rejecting zero-length completions
- `rng.rs`: `VirtqAvail`/`VirtqUsed`'s `idx`/`flags`/`*_event` fields were read/written as plain `u16`s with no synchronization between producer and consumer; fixed by making them `AtomicU16` with a release store on publish and an acquire load on completion (the pre-notify `fence(SeqCst)` kept, since it orders against a Device-memory MMIO store the atomics don't cover)

### docs/archive/AKUMA_BOOT_EXTRACTION.md
- The `devbox-smoltcp` boot's `sshd.conf` carried `start_delay_ms = 10000`, tuning inherited from the rump profile's DHCP-handshake wait, but under smoltcp the network stack is already up synchronously before `herd` even starts, so the delay was pure dead time on every boot; set to `0` in `overlays/devbox/rootfs/etc/herd/enabled/sshd.conf` (the rump case still needs the 10s value, kept in `bootstrap/etc/herd/core2/sshd-rump.conf`)


## Signals & Exceptions (15 fixes, 7 docs)

### docs/archive/CTRL_C_SIGINT_DELIVERY.md
- Ctrl-C never interrupted a foreground child over `ssh -tt` (repro: `tail -f`): **no line discipline in the tree generated `SIGINT` at all**. Fixed in the kernel as a process-group broadcast; the first attempt — patching sshd to target `foreground_pgid` as a single pid — was wrong and is recorded as such

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

### docs/archive/PTHREAD_KILL_EINTR_DELIVERY_STARVATION.md
- `PENDING_SIGNALS` is cleared at the moment of delivery and was the only bit `current_thread_has_pending_interrupt` read, so `rt_sigreturn`'s immediate re-delivery of the next queued signal always cleared it before a blocked syscall's wait loop was scheduled to notice — under a signal source fast enough to keep the deliver→handler→`rt_sigreturn`→deliver chain saturated, `pthread_kill` could never interrupt the blocking syscall it targeted; fixed (necessary but, on its own, not sufficient) via a separate per-thread `DELIVERED_SIGNALS` mask, set once at the `try_deliver_signal` chokepoint and consulted alongside the pending bit
- Even with that mask, a signal-woken thread rejoined the back of the round-robin run queue, and signal delivery had unbounded priority over resuming the syscall it interrupted (each handler return re-delivered the next pending signal immediately), so under a 10ms-cadence signal sender the interrupted syscall was starved indefinitely rather than merely raced; fixed via `SIGNAL_WAKE_PREEMPT` (runs a signal-woken thread on the next switch) plus `SIGFRAME_ACTIVE` (bounds delivery to one handler per unit of userspace progress, consulted by `rt_sigreturn`)


## Misc / Cross-cutting (29 fixes, 7 docs)

### docs/archive/AKUMA_FIRECRACKER_TERRAFORM.md
(Host-side tooling for the AWS metal Firecracker host, `../akuma-terraform`. §9's eight bugs; the §10 Akuma-side results are verifications of fixes counted elsewhere, not new fixes, and §7's traps are AWS behaviours rather than bugs in this project.)
- Alpine's `vmlinuz-virt` is an EFI zboot PE wrapper (`MZ` + `zimg`, payload offset at 8) since `CONFIG_EFI_ZBOOT`, not a raw ARM64 `Image`, so offset 56 held payload (measured `0x818223cd`) and Firecracker rejected it with `InvalidImageMagicNumber`; the dump now inflates the gzip payload at 51832 first
- The FDT dump filtered console lines to the base64 alphabet *before* stripping `\r`, and every CRLF-terminated line failed the character class — discarding the whole blob and producing an empty decode indistinguishable from a guest that never dumped
- The FDT header-magic check compared a host-endian word (`od -tx4` → `0xedfe0dd0`) against big-endian `0xd00dfeed` and failed on a correct blob; compare bytewise (`od -An -tx1`)
- `dnsmasq` could not reopen its own `/var/tmp` log under `fs.protected_regular = 2` — it creates the file as root then `fchown`s it to `nobody`, so run 1 succeeded and every later run exited 3, failing `akuma-fc-net.service` and leaving tap0 up with no DHCP, no NAT and no ssh forward on **every reboot but the first**; log and pid moved to `/var/log` and `/run`, which are not sticky
- Stage 50's bare `rustup target add aarch64-unknown-none` ran from `/`, outside the cargo project, so it resolved to the default toolchain (stable) while the tree pins `channel = "nightly"` — the bare-metal std landed on the toolchain the kernel build never uses and 15 no_std crates failed at once with `E0463: can't find crate for core`, while `rustup target list --installed` showed it present; both toolchains now get it explicitly
- `bin/push-akuma.sh` passed `--info=progress2` to what is **openrsync** on macOS 15, which has no `--info` and exits with a usage dump; compounded because `--delete` had already removed the previous tree, so the failed push left *no* source on the host and the next build failed with `overlays/devbox-firecracker/build.sh: No such file or directory`. Switched to `--stats`, and a `--from git` transport (the host clones from GitHub, 1.8 s) is now the default over an operator uplink measured at 53 KB/s
- `bin/package-rootfs.sh` expanded `"${ARGS[@]}"` on an empty array under `set -u`, which is an unbound-variable error on bash 3.2 (macOS `/bin/bash`), so the no-argument invocation — the usual one — died before reaching the builder; now `${ARGS[@]+"${ARGS[@]}"}`
- Stages `60-akuma-image.sh` / `62-ecr-image.sh` were never staged to `/opt/akuma/bin` on the running host, and `build-image.sh` checked only after uploading 45 MB over the slow link — 13 minutes to reach `command not found`; scp'd into place, and the prerequisite belongs before the upload rather than after (the `user_data` change that stages them remains unapplied in terraform state)

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

### docs/archive/IDENTITY_CACHE_SMP_REVIEW.md
(Harness fixes only. Of the doc's two identity-cache findings, **Finding A — the syscall epilogue reusing the prologue's `Process` across an open-ended dispatch — was fixed 2026-08-29** and is counted under `AKUMA_EXTRACT_SYSCALLS.md`, not here. Finding B, `identity_get`'s ACTIVE check not covering slot recycling, remains **open**: both were found by inspection, and an SMP=4 audit build instrumented to observe the first counted zero occurrences, so no kernel change was made. The added `IDENTITY_AUDIT` counters are instrumentation, not a fix.)
- `scripts/forktest_smp_matrix.py` reported 14/14 FAIL at SMP=2 and SMP=4 on a healthy kernel: five of the seven configs passed no `-duration`, and `forktest_parent` defaults that flag to 0 = "run until all children finish", so those runs could never complete inside the harness's own `duration + 30` s timeout
- The harness's reader thread returned at the boot marker, leaving nothing draining QEMU's stdout: every per-test log stopped at boot (so the `[PANIC]`/`WILD-DA`/`[SGI-S POISON]`/`[WATCHDOG]` grep could only ever match boot output, and reported "no crash" for a run it could not see), and once the 64 KB pipe filled QEMU blocked on write and the VM stalled — turning a 14 s `combined_light` into a 50 s "timeout". Closing the log under that still-running thread also raised `ValueError: I/O operation on closed file`
- Readiness was decided by matching `"Started sshd"` in console text, which an unlocked UART tears across herd's separate `print()` calls (`[herd] Started ` + another core's `[syscall] bind(...)` + `sshd (pid= 2)`) — 4 of 7 SMP=4 boots, 0 of 7 at SMP=2; replaced with an SSH-banner probe, since a bare `connect()` is not readiness either (QEMU's user-mode hostfwd accepts before the guest listens)

### docs/archive/ERROR_HANDLING_AUDIT.md
- `wait4`/`waitid` discarded `write_user_val`'s error and reaped the zombie on the next line, so an unmapped `status_ptr` destroyed the child's exit status irrecoverably while `wait4` still returned the pid as though it had reported it; fixed to report-then-reap and return `EFAULT` when the report fails, at all five sites (Linux-compatible)
- `unmap_page`/`update_page_flags` and both `_no_flush` variants returned a `Result` no implementation could ever populate with `Err`, which had taught three test call sites to grow vacuous `is_ok()`/`is_err()` assertions proving nothing; fixed by returning `()` from all four and replacing the vacuous assertions with `read_l3_page_entry` read-backs that prove the PTE actually changed
- `spawn_worker_demo` printed the number of workers *attempted*, unconditionally, for a loop explicitly allowed to fail — a partial spawn reported as a full one in the line `cores_that_ran_workers()`'s boot self-test is read against; fixed to count successes
- `rx_frame`/`tx_frame`/`tx_discard` handed out `&'static mut [u8]` into a `static mut` NIC buffer, so two calls with the same slot were instant aliasing UB no caller could discharge; fixed by returning `*mut [u8]` like the raw-pointer `rx_buf`/`tx_buf` accessors already in the same files


## Console & Terminal (32 fixes, 11 docs)

### docs/archive/VEC_AUDIT.md
- `crates/akuma-terminal`'s canonical-mode `canon_buffer` grew one byte per keystroke with no cap and was drained only by a line terminator, so a peer writing to a tty in canonical mode and never sending `\n` grew kernel heap without limit. Capped at `MAX_CANON = 4095` (Linux N_TTY's own ceiling), dropping — and deliberately not echoing — input beyond it, while the `\n`/VEOF paths stay uncapped so a full line can always still be terminated

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

### docs/archive/TTY_SHENANIGANS.md
- `isatty(0)` (the `TCGETS` path of `sys_ioctl`) decided tty-ness from the process's I/O *channel* rather than the fd table, so a `cat file | less` pipeline child — fd 0 `dup2`'d to a `PipeRead` but still inheriting the shell's terminal channel — reported itself as a tty, making busybox `less` take its no-FILE-argument tty branch and print a usage banner instead of paging the pipe; fixed by gating on the fd table entry for fd 0/1/2 first (only `Stdin`/`Stdout`/`Stderr` are a tty), keeping the channel check as a second gate for the sshd exec-channel case
- Akuma had no `/dev/tty`, so a pager reading keystrokes from the controlling terminal (never from stdin, which for `git log | less` is the pipe carrying the paged content) fell back to reading stdin instead and hung forever with typed keys draining into the pipe; fixed with a `FileDescriptor::DevTty` variant resolved per-syscall via the caller's channel, plus a static `/dev/tty` device-table node so `stat`/`ls` see it
- Adding `/dev/tty` support deleted the fd-table type gate from the `isatty` fix above, reopening the exact `cat file | less` usage-banner regression; fixed by restoring a single fd-table type match (`Stdin | Stdout | Stderr | DevTty` pass, else `ENOTTY`) that covers both the original tty fds and the new `/dev/tty` fd
- The same regression left `sys_ioctl`'s terminal-ioctl gate as a bare `fd > 2` cutoff, which unconditionally rejected the newly-introduced `/dev/tty` fd (never 0/1/2) for every terminal ioctl — breaking `crossterm`'s raw-mode setup (`tcgetattr`/`tcsetattr` issued directly on an opened `/dev/tty` fd) with a `reader source not set` panic in Helix; fixed by the same fd-table type match above rather than a numeric cutoff
- The `/dev/tty` work also dropped the `FIONREAD` arm for `Stdin`, so `ioctl(FIONREAD)` on stdin or `/dev/tty` always reported zero buffered bytes regardless of actual pending input; restored, and extended to cover `DevTty` for the same reason as the ioctl gate
- Round 4: `box run` handed the box's process the *caller's* `TerminalState` instead of one scoped to the box, so terminal mode changes made inside a box leaked across the box boundary onto the caller's terminal

### docs/archive/CONSOLE_LOG_COST.md
- The `syscall/mod.rs` epilogue's `[EFAULT]` diagnostic cost ~250 µs per call (~2,400 ns/byte of console output, one `write_volatile` VM exit per byte with no buffering) on every EFAULT-returning syscall — 99.94% of the syscall's own cost and userspace-drivable by looping a bad-address call, degrading the whole VM since console writes serialise across cores; fixed by turning it off (`SYSCALL_ERRNO_DIAG_ENABLED = false`), 249,806 ns → 150 ns
- The errno-diag gate had narrowed to `result == EFAULT` only — a workaround for an `EINVAL` flood from `readlinkat` probes during cargo builds — silently making its `ENOSYS`/`EINVAL` arms and the whole `mmap`-`EINVAL` decode its own comment called load-bearing unreachable dead code; fixed by widening the gate to `EFAULT || ENOSYS || EINVAL` so the code handles what it claims to
- `madvise_dontneed_range`'s first pass took `lazy_region_lookup_for_pid` — a process-table walk, IRQ mask and `lazy_regions.lock()` — once **per page** of a `MADV_DONTNEED` range, to consult a map that never changed; fixed by taking the lock once for the whole range (30.1× → 4.8× median vs `getpid`, loaded host)
- A guest `cargo build`'s serial console was unreadable: seven ungated per-call trace families userspace can drive (`execve` argv, `[TERM]`, the four `[signal]`/`[sigreturn]` delivery lines, `[KTG]`, `[pipe] DESTROY`, the execve PATH-probe miss, `[FS] read_file`) were **91.9% of the log** (12.8× reduction A/B on identical work); the `[FS] read_file` arm was its own bug — a `path.contains("git")` substring filter that silently matched every path under a `github.com` checkout — and the `[TERM]`/`[KTG]` rate-limit budgets were not gates, so their gate check moved before the counter fetch; gated behind existing subsystem flags plus one new `SIGNAL_TRACE_ENABLED`

### docs/archive/SYSCALL_TRACE_AUDIT.md
- `akuma-syscalls-time`'s `[clock-diag]` performed two user-memory reads plus a ~130-byte two-line `log::warn!` on every `clock_gettime(clock_id > 0x1000_0000)` call from userspace — the worst single instance found, since it also read user memory before printing; gated behind the `akuma-syscalls-time/debug-info` feature, off by default
- 25 previously-ungated print sites across `src/syscall/` (`aio.rs`, `msgqueue.rs`, `mem.rs`, `fs.rs`, `timerfd.rs`, `pidfd.rs`, `proc.rs`, `net.rs`, `pipe.rs`, `mod.rs`), each reachable in an unprivileged userspace loop, gated behind existing subsystem flags (`SYSCALL_DEBUG_INFO_ENABLED`, `MEM_SYSCALL_TRACE_ENABLED`, `SYSCALL_DEBUG_NET_ENABLED`, `PIPE_TRACE_ENABLED`) with no new knobs added
- `akuma-exec`'s per-thread-recycle trace ran unconditionally on every thread recycle, which a `-j4` build triggers constantly; gated behind `lifecycle_trace_on()`, folded to `cfg!(feature = "debug-info") && config().syscall_debug_info_enabled` so the compile-time half is checked first and the whole call site can fold away (measured −4,460 bytes `.text` / −1,688 bytes `.rodata` on `release`)
- Three `syscall/aio.rs` stubs (`io_submit`/`io_cancel`/`io_getevents`) computed an IRQ-masked `AIO_CONTEXTS.lock().contains_key(&ctx)` probe on every call, outside their debug gate, purely to choose which string to print, even though all three unconditionally return 0; moved inside the gate, so the stubs are now bare `return 0`
- The generic `[SC]` prologue trace formatted `args[0..3]` for every syscall number not on a hand-maintained "noise" list, but the list was never updated when new argument-less `FastPath::Leaf` syscalls (`AKUMA_GET_VERSION`, the uid/gid additions) were added, so it would have printed stale register contents for exactly the calls the entry-vector change stops restoring `x0`–`x5` for; fixed by deriving `debug_io_suppressed` from `takes_no_args(nr) || <noise list>` so the predicate can't drift from the contract again
- `scripts/mem_suite.py`'s no-silent-pass guard treated a dropped ssh round-trip identically to a dead probe and failed the whole suite on a `SILENT (rc=0)` result that reran clean by hand; fixed by retrying once, and only on `SILENT` — a `FAIL` line or bad exit code is never retried


## Containers (24 fixes, 6 docs)

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

### docs/archive/BOX_RUN_OVERLAYFS.md
- `box pull` had always extracted layers with busybox tar (`bootstrap/bin/tar` was never deployed by `userspace/build.sh`), and busybox tar creates hard links via `link()`, which `sys_linkat` implements as a full `read_file`+`write_file` copy that also drops the mode bits — so the busybox image's 410 hardlinks to one binary became 410 real `0644` copies (467.7 MB extracted from a 1.9 MB layer), and every copied binary then failed a `PATH` search's `access(X_OK)` check with `EACCES`; fixed by shipping Akuma's own `tar` and applying the archived mode bits on extraction (layer store dropped to 4.1 MB)

### docs/archive/REATTACH_STALE_CHANNEL_HANG.md
- `sys_read`'s stdin loop and `sys_poll_input_event`'s blocking branch each fetched the process's `Arc<ProcessChannel>` once before entering their blocking wait and kept reusing it across every park/wake cycle; `sys_reattach` repoints `Process::channel` to a new `Arc`, so a process already parked in a blocking read at grab time woke correctly but kept checking the old, abandoned channel forever — typed input into a `box grab`bed session never reached the target even though the syscall reported success; fixed by re-resolving `current_channel()` on every loop iteration instead of caching it once
- nothing stopped a second `box grab` on the same pid from silently stealing the channel out from under a still-active first grab, and the first grabber had no way to notice; added `Process::grabbed_by` and a `force`/`-d` flag on `sys_reattach` (mirroring `screen -r`/`-d`) so an unheld target still succeeds, an already-held target fails with a new `EBUSY` errno, and `-d` detaches the previous holder first
- `box grab`'s `waitpid` loop could never tell that its target had exited, because `reattach` does not reparent the grabbed pid — `wait4`/`waitpid` on a non-child pid returns the same "nothing to report" as "still running"; fixed by falling back to a `kill(pid, 0)` liveness probe when `waitpid` reports nothing
- reattaching to a full-screen app left it looking frozen or misdrawn because nothing told it its terminal had effectively changed; `sys_reattach` now copies the caller's `term_width`/`term_height` onto the target's `TerminalState` and sends `SIGWINCH` on every successful reattach, matching `screen`/`tmux`'s attach behavior
- `-d`-detaching a previous holder was a raw crate-internal force-stop (`kill_process_with_signal` manipulating process state directly, no disposition-aware signal path, no terminal cleanup) instead of a real kill, so the displaced client's terminal was left in whatever raw/alt-screen state the grabbed app had put it in; fixed by moving the detach to the syscall boundary, writing a terminal-reset escape sequence into the previous holder's channel while the connection is still up, then delivering a real `SIGTERM` through `sys_kill` so the previous holder runs its normal `exit_group` teardown

---

## Files scanned with zero counted fixes (reference docs, open issues, reverted attempts, or pure duplicates of a fix counted elsewhere)

Also scanned 2026-08-27 (the `obviously-more-fixes` branch, ahead of closing it for the syscalls-refactor branch): AKUMA_SYSCALL_PERFORMANCE_AUDIT (the `getpid` floor taken 410 ns → 150 ns by a per-thread identity cache — an optimization with no defect attached, counted the same way LTO_RELEASE_PROFILE and BKL_RUSTC_SCALING_BASELINE are; its four deferred follow-ups are open, and the SMP>1 soak it defers is IDENTITY_CACHE_SMP_REVIEW, whose Finding A was since fixed 2026-08-29 and is counted under AKUMA_EXTRACT_SYSCALLS, leaving only its Finding B open), EXT2_READ_PATH_STAGE_PROFILE ("instrument landed, no behaviour changed" by its own status line — the `read-profile` feature and two probes, nothing on the read path modified), EXT2_WRITEBACK_DESIGN (in-flight design record; its finding F-1 is fixed by FPCACHE_MOUNT_IDENTITY, counted there, and its D-9 capacity half and §237 "what is still open" list remain open), USER_COPY_BYTE_LOOP (widening the user-copy byte loop — a measured optimization, no defect; its own header retracts the "~17 µs of fixed cost" claim as inferred rather than measured), and USER_MANAGEMENT_AND_BOXES ("Design investigation. Nothing here is implemented."). USER_COPY_FOLD gained only a cross-reference note pointing at USER_COPY_BYTE_LOOP — no change to its existing count. (ERROR_HANDLING_AUDIT, mentioned below as carrying fix shapes rather than fixes, **since gained four real fixes on 2026-08-30** and now has its own subsection under Misc / Cross-cutting. ASID_EXHAUSTION_TIGHT_THREAD_LOOP is root-caused and measured but **Status: OPEN** — `pthread_create` fails at ~251 serial iterations in a tight loop because ASIDs leak from address spaces whose `Drop` never ran, against `MAX_ASID = 256`; not fixed, counted nowhere.)

Also re-scanned 2026-08-25 (the `more-fixes` branch, ahead of merging it): BENCHMARK_PERFOMANCE_ATTEMPT_1 (a benchmark record; its one actionable finding — nginx's lost epoll wakeup — is open and lives in NGINX_LOST_WAKEUP), MOUNT_MISSING_SYSCALLS ("nothing in this doc is implemented by this session" by its own status line; §7 is a build list), NGINX_LOST_WAKEUP (**open**: nginx's `epoll_wait` misses readiness wakes and is rescued by the 10 ms `backstop_us`, so requests cost ~17 ms; hypothesis with strong circumstantial support, no fix), HTTPD_ACCEPT_HANG (**open**: `httpd` stops answering while the process is still alive and logs no error; observed 2-3 times, not reliably reproduced, failing stage not established), REDIS_BENCHMARK_HOST_CONTENTION_LIVELOCK (resolved, but the `while(1)` is in `redis-benchmark`'s own `writeHandler` — "the guest is not involved in the hang at all", so no fix landed in this codebase), REDIS_ROUND_TRIP_STAGE_TRACE (read entirely out of already-checked-in logs — no new boot, no new build; its §4 "688 µs" framing is retired by LONG_ROAD_TO_REDIS_PART_2 §9, counted there), and SYSCALL_LAYER_AUDIT (the duplication audit that *found* the `dup`/`dup3`/`fcntl` refcount gap; the fix is counted under LONG_ROAD_TO_REDIS_PART_2, not duplicated here). VEC_AUDIT's findings #1 (`map_user_page`'s per-call page-table frame list) and #2 (`SOCKET_TABLE`) are `Vec`-to-fixed-array conversions with no bug attached and are not counted; its #3 is the `irq.rs` deadlock, counted under IRQ_HANDLER_TABLE_DEADLOCK; only its terminal `canon_buffer` fix is counted (Console & Terminal, above).

Also re-scanned 2026-08-07: DEVELOPMENT_PRACTICES_REVIEW_AND_ASSESSMENT (pure meta-analysis of process/git history, zero concrete bug-fix content) and BKL_RUSTC_SCALING_BASELINE (re-verified still accurate as perf-not-bugs; its inconclusive `big.rs`-failure investigation is fully resolved later by SMP_SHARED_ONCPU_GATE.md and STALE_THREAD_SLOT_KILL.md, counted there).

Also re-scanned 2026-08-09: CRUSH_MISSING_SYSCALLS, C_STUBS, NEEDLE_SERVER, QJS, and TOP_CORE_COLUMN_PLAN each picked up a one-line "removed as part of a codebase trimming effort" note pointing at TRIM_FAT_PART_3.md; STDCHECK_DEBUG picked up the same note but keeps its existing 1-fix count (Console & Terminal, above) — the note doesn't touch its fix content. None of these notes describe a fix on their own.

Also re-scanned 2026-08-15 (completing the `TRIM_FAT_EMBARASSING_DUPLICATIONS.md` cross-reference audit): AKUMA_PRIMITIVES_EXTRACTION (pure crate-extraction record for `akuma-primitives` — every "finding" is dead code removed or a design question resolved as "not a divergence," none is a fixed defect) and LTO_RELEASE_PROFILE (a measurement-driven config decision — `[profile.release]` `lto = "thin"` — not a bugfix). COW_PILE_AUDIT, USER_COPY_FOLD and PMM_EXTRACT were the other three `TRIM_FAT_EMBARASSING_DUPLICATIONS.md` cross-references; their fixes are counted above under their own doc subsections.

Also re-scanned 2026-08-15 (the whole branch's unaudited archive docs, ahead of closing it): CARGO_HEAP_NULL_RC (the task brief for the cargo-null-`Rc` hunt; explicitly defers its fix to MADV_DONTNEED_SHARED_FRAME, counted there — "not duplicated here, so this file cannot drift from it"), HOST_TESTS_AUDIT (a boot-test-to-host-test movability survey; every finding is a scaffolding recommendation, none a landed fix), LINUX_COMPATIBILITY_ISSUES ("this list is a byproduct, not an audit" — a register of known ABI divergences, explicitly open except the one `mremap` fix, which is counted under USER_COPY_FOLD.md), REPR_C_SIGFRAME_STATX (Phase 7's `#[repr(C)]` hardening pass — behaviour-preserving by its own claim; its two Linux-layout divergences are found and *deliberately not fixed*, and its narrowing behaviour changes are inherited from the already-counted USER_COPY_FOLD.md AP-bit fix, not new here), SMP_FORK_EXEC_CORRUPTION_FIX (a restored 2026-07-21/31 dossier; every fix in it — the `demote_range_to_ro` DSB barrier, `LifecycleGuard`'s active preemption-disable, the BKL ticket-leak self-heal, `COW_FAULT_LOCK`'s non-fix — is a duplicate mention already counted under SMP_SHARED.md, BKL_PROCESS_CARVE_OUT.md, or COW_PILE_AUDIT.md's F3), TRIM_FAT_REMOVAL_FEASIBILITY (feasibility/scoping analysis for the in-kernel SSH/shell/editor and `libakuma` removals — no landed fix of its own; the removal it scoped is counted under BUILTIN_SSH_REMOVAL.md), UNSAFE_AUDIT (the audit that spawned USER_COPY_FOLD.md, REPR_C_SIGFRAME_STATX.md and the RNG driver fixes — every "DONE"/"FIXED" marker in it points at one of those, none is new), and ZERO_PAGE_ICE_FIX (the umbrella summary of the self-host `[0,0,0,0]` ICE hunt; its two named root causes are counted under PREFAULT_INODE_STUB_ZERO_PAGES.md and SELFHOST_ZERO_PAGE_HUNT.md §14–§15, and the "two real bugs found and fixed" it lists verbatim are counted under SELFHOST_ZERO_PAGE_HUNT.md §8–§9).

Also re-scanned 2026-08-12: CARGO_CRATES_IO_CONNECT_FAIL (root cause isolated, "fix not yet chosen" — five options, none landed; **fixed 2026-08-20, now counted under Networking**), MINIMAL_DEV_BUSYBOX_APPLETS (an applet-coverage survey; its three verification findings — `utimensat` hardcoded to `0`, `getgroups` undispatched, no `/etc/passwd` on the devbox overlay — carry "fix shape" proposals, not fixes), TRIM_FAT_HAND_ROLLED_JSON (an audit of hand-rolled JSON across the tree; the bugs it reproduces in `herd` and `meow` are unfixed), HERD_PLUS_BOX (a proposed restructuring, explicitly not implemented — it will be renamed `TRIM_FAT_HERD_PLUS_BOX` and re-scanned when it lands), and userspace/box/BOX_RUN (current-state reference; the one fix it mentions is BOX_DOCKER_COMPAT's session-closing bug, counted there). AKUMA_SELF_HOSTING gained only a quick-start section — no change to its count. The same pass found three docs that had never been counted at all, all now listed above: DEVBOX_ISSUES (Misc), and the two deep-dives its Issues 2 and 3 point at, TERM_POLL_INPUT_PREEMPTION_FIX and UART_SMP_INTERLEAVE_FIX (both Console &amp; Terminal).

Also re-scanned 2026-08-21 (the `improve-portability` branch's archive docs, ahead of closing it — the list had not been touched in the branch's 19 commits): PORTING_POSSIBILITIES (the options survey that preceded the Firecracker port — every entry is a candidate weighed and either taken or dropped, none a landed fix; the port it chose is counted under AKUMA_FIRECRACKER_KVM.md above). DEVBOX_ISSUES gained **Issue 25** (`fw_cfg`'s base address is a hardcoded VA, not a reading) which is explicitly **OPEN** and design debt rather than a bug, so that file's count is unchanged; its §3.3 relative — the fw_cfg *fault* the Firecracker boot took — is counted under AKUMA_FIRECRACKER_KVM.md, not here. RUMP_SYSPROXY gained one landed fix (the `box use` bare-hex resolution gotcha) and is bumped by 1 above. The branch's other new docs are out of scope for this file by kind, not by emptiness: `proposals/FIRECRACKER_PORT.md` is an in-flight proposal, `docs/reference/firecracker/{README,memory-map,disk-and-volumes,fdt/README}.md` and `docs/runbooks/{run-on-firecracker,dump-firecracker-fdt}.md` are current-state reference and procedure, and `overlays/devbox-firecracker{,-aws}/README.md` are component READMEs.

docs/archive: 4MB_STABLE_AGENT, AI_DEBUGGING, ARCHITECTURE, BKL_DRIVERS_CARVE_OUT, BKL_PHASE7B_PPOLL_CARVE_OUT (piece 2 reverted after A/B caught real corruption), BKL_PHASE7D_THREAD_CONTEXTS (dead/unreachable code removed, not a live bug), BKL_PHASE7F_OPTOUT_LIST, BKL_RUSTC_SCALING_BASELINE, BOX_SUBDIR_FS_LIMITATIONS, C_STUBS, CGI, COMMAND_CHAINING_SSH_BUGS, CONCURRENCY, CONTAINERS_STAGE_1_PLAN, CONTAINERS_STAGE_2_PLAN, CP_MV_IMPLEMENTATION_PLAN, CRUSH_MISSING_SYSCALLS (all gaps, none marked fixed), CWD, DEAD_CODE_ANALYSIS, DEAD_CODE_SWEEP_FINDINGS (findings only, explicitly "nothing here is fixed. No source was edited"), DEV_RANDOM, DEV_ZERO, DOCKER, EMBASSY_REMOVAL, ERRORS_TO_CHECK, EXTREME_STACK_TRIMMING (perf, not bugs), FORKTEST_GO_HANG_FIX (its one fix — the `sys_waitid` ECHILD-on-non-child parentage check — is the exact same 2026-07-22 investigation already counted under SMP_SHARED.md's "forktest_parent (Go) hang" entry), FRANKENLIBC_EVAL, FREEZE_INSTRUMENTATION_PLAN, HEAP_AND_MEMORY_IMPROVEMENTS, HERD, HERD_ADD_AND_PATH_VALIDATION, HIJACK_VS_KERNEL_PROXY (analysis/validation only), IMPLEMENTATION_PLAN (rump phases, milestones only), INTERACTIVE_IO, J4_HANG_LIVE_AUTOPSY (verbatim session record; its 3 fixes are counted once under KTG_STALE_TID_EXIT_STAMP_J4_HANG.md), KILL_COMMAND, LARGE_BINARY_LOAD_PERFORMANCE, LINE_COUNT_ANALYSIS (line-count/dead-code statistics and cross-kernel comparison, not a bugfix), LOCK_REFERENCE, LOOPBACK_TIMEOUT_FIX_PLAN (plan, not landed), MEMORY_LAYOUT (duplicate of AKUMA_SELF_HOSTING §3), MULTIKERNEL, MULTITASKING, MUSL_COMPATIBILITY, NAMESPACES, NATIVE_STACK_INTERNET, NEEDLE_SERVER, NETWORKING_PERFORMANCE_AND_THREAD_SAFETY_ANALYSIS, ON_DEMAND_ELF_LOADER, OOM_BEHAVIOR, OOM_RECOVERY_OPTIONS, PAWS_PLAN, PAWS_TO_SSH_SHELL_PLAN, PHASE01_BUILDRUMP, PHASE1_COMPLETION_BASELINE, PHASE1_NETWORK_LOCK_FOUNDATION, PHASE2_RUMPUSER, PHASE3_KERNEL_TAP, PLAN_SIGSEGV_COMPILE_FIX, POSSIBLE_MEMORY_LEAK, POST_EXIT_PMM_RECLAIM, PROCESS_MEMORY_CLEANUP, PROCFS, PROPER_EXECVE_PLAN, QJS, refactor_plan, RSA_FEATURE_GATE, RUMP_LATENCY_SLEEP_FIX (hypothesis disproven, patches reverted), RUMP_PLUS_HERD, SCHEDULING_TIMING_ISSUES (open/critical, not fixed), SCRATCH, SEPARATE_SHELL_BINARY, SHARED_FD_TABLES, SHELL_ENVIRONMENT_VARIABLES, SHELL_LIMITATIONS, SIGNAL_DELIVERY_FORKTEST_EVIDENCE (summary of fixes counted elsewhere), SMOLTCP_MIGRATION_SUMMARY (duplicate summary), SMP_SHARED_M5_FAULT_LOCK_PLAN, SSH, SSH_PERFORMANCE_FIX_2026, SSH_THREADING_BUG (superseded, duplicate), STRATEGY_A_IMMEDIATE_TUNING, STRATEGY_B_SMOLTCP_MIGRATION (duplicate), STRATEGY_C_IRQ_WAKEUPS, SYSCALL_BLOCKING, SYSCALL_ERRNO_COMPLIANCE_CHANGES, SYSCALL_HARDENING, TCC_LOW_MEMORY, TCP_SEQUENCE_UNDERFLOW_PANIC, TERMINAL_SYSCALLS (duplicate reference), TLS_DOWNLOAD_PERFORMANCE, TLS_INFRASTRUCTURE, TOP_CORE_COLUMN_PLAN, TRIM_FAT_PART_1, TRIM_FAT_PART_2, TRIM_FAT_PART_3 (pure component-removal log, no bugfix content — same shape as TRIM_FAT_PART_2), TWO_VMS_AGENT_DEMO, UNIFIED_CONTEXT_ARCHITECTURE (duplicate of FAR_0x5/THREADING_RACE_CONDITIONS fixes), UNIFIED_PROCESS_ABI, UNSAFE_POINTERS_AND_ATOMICITY, USERSPACE_MEMORY_MODEL, USERSPACE_SOCKET_API, VFS_LOCK_OPTIMIZATION_PLAN, WAIT_QUEUES, MEOW.

userspace: apk-tools/BUILD_NOTES, apk-tools/PIE_LOADER, box/OCI_IMAGE_PULL, box/TESTING (duplicate of libakuma-tls TLS fix), crush/IMPLEMENTATION_DETAILS, forktest/IMPLEMENTATION_PLAN, herd/CORE_AWARE_SCHEDULING, httpd/TIMESTAMPS, libakuma/ALLOCATOR_OPTIONS, libakuma/MKDIR_P_IMPROVEMENTS, libakuma/SYSCALLS, libakuma/TERMINAL_SYSCALLS, meow/CONFIG, meow/HOTKEYS, meow/SHELL, meow/TESTING, scratch/LARGE_FILE_CHECKOUT_OPTIMIZATION, scratch/SIDEBAND_PARSER_FIX (duplicate of docs/archive/SIDEBAND_PARSER_FIX.md), sshd/LIMITATIONS, sshd/MIGRATION_SUMMARY, tar/IMPLEMENTATION_PLAN, tar/STREAMING_EXTRACTION, tcc/DISTRIBUTION_PLAN, tcc/IMPLEMENTATION_DETAILS, tcc/IMPLEMENTATION_PLAN, tcc/LIBTCC1.
