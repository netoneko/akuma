# Taoxichuan: bugs found 2026-10-30 (unsafe reduction, `-j4` hunt, memory baselines)

Found while cutting `unsafe` in `taoxichuan/src` ([plan status](TAOXICHUAN_UNSAFE_REDUCTION_PLAN_GLM.md)), hunting the
`-j4` wedge ([CoW fork doc](TAOXICHUAN_COW_FORK.md)) and measuring whether the guest can compile its own kernel in 3 GiB.
Rigs: Mac TCG (battery, `cow_leak_gate.py`), ryzen/KVM via `scripts/kvm/taoxichuan_pop.sh` (Pop OS host, guest at
`192.168.1.126:2224`). Status of each entry is stated; nothing here is committed yet.

## 1. `execve` answered EFAULT for large argument blocks — FIXED (build now gets past it)

- **Symptom.** `cargo build -p taoxichuan` inside the guest (3 GiB, `-j4`) died with `Bad address (os error 14)`:
  `[execve] s14 path=/usr/local/rust/bin/rustc` then `[err] s14 nr=59 -14`. Free memory was ~1.1 GiB at that moment, so not memory.
- **Cause: three limits in one function.** `read_spawn_args` (`taoxichuan/src/child.rs`, shared by `spawn`, the real-fork `execve`
  and `exec_replace`) capped `argv` and `envp` at 128 entries each, and each string at 4096 bytes (`STR_MAX`). Exceeding any of them
  returned `None`, which the caller maps to `EFAULT`. Linux answers `E2BIG` for an oversize block, never `EFAULT`. There was also no
  total-size check, so an oversize block reached `ring3::build_stack`'s slice arithmetic and its `MAX_STRS = 512` assert.
- **What cargo actually passes.** Measured on Linux with a `RUSTC_WRAPPER` that logs each call's shape for this same build: the
  heaviest `rustc` call has **344 arguments**, one **5192-byte argument**, ~19.6 KB of arguments, 84 environment variables (Linux host
  environment; the guest's is smaller). The first fix attempt (128 → 256 entries, found by an isolated test) moved the failure from
  ~3m14 to ~4m00 guest time and did not finish the job: the 344-argument and 5192-byte cases were still refused. Reading the cargo
  invocation first would have found all three at once.
- **Reproduced in isolation.** In the guest, `env V0=x … V99=x /bin/true` ran and the same with 200 variables printed
  `env: Bad address`.
- **Fix.** `ARGV_MAX` 448, `ENV_MAX` 256, string cap 32 KiB (read into a `Vec`, not a 4 KiB stack array), `argv + envp <= 512` and a
  total-bytes guard both answering `E2BIG`; the loader's argument block grew from 8 to 12 pages (`ARG_BLOCK_PAGES` in `main.rs`, the stack
  is 16 pages).
- **Why the `-j4` serde/serde_json/fastrand builds never hit it.** Their crates have few dependencies; the argument and environment
  size of a `rustc` call grows with the dependency list, so only a larger crate crosses the caps.
- **Verified:** the isolated 200-variable case failed before the fix. With the final fix the in-guest build executes the `rustc` calls that
  died before and runs on to the failure in section 7. Follow-up (same day): "too many entries" and "a string longer than `STR_MAX`" now answer `E2BIG` too
  (`read_ptr_array`/`read_user_string` return the errno); a non-UTF-8 argument still answers `EFAULT` (the loader path carries `&str`).

## 2. `-j8` crates.io build fails — UNEXPLAINED

Same serde + serde_json + fastrand project, fresh boot each time.

| Guest RAM | Kernel | Result |
|---|---|---|
| 3072 MiB | working tree (unsafe-reduced) | dies at ~45 s: `mmap` ENOMEM, "memory allocation of N bytes failed", free pages hit 0, then `[Fault] ring3 PageFault … code=0x7` and a #GP |
| 3072 MiB | committed `9f1e333f` | same |
| 8192 MiB | working tree | `rustc` (itoa) dies: `[Fault] ring3 GeneralProtection rip=0x3004274b rsp=0x00006fffffffc8a0` (in the dynamic linker, rsp at the stack top) |

- At 3 GiB it is memory exhaustion: eight rustc jobs need more than 3 GiB, and when a CoW break (`cow_resolve`'s `Copy` arm) cannot get a
  frame it answers `Fault`, so the task is killed with `code=0x7`. That is the designed out-of-frames path, not a CoW logic error.
  A kernel that kills with SIGSEGV where Linux would run its OOM killer is a policy gap, not investigated further.
- The 8 GiB #GP is **not** explained. It is not OOM and not the `execve` cap of section 1 (different symptom). The committed kernel was
  only run at 3 GiB, so whether the 8 GiB crash predates the unsafe-reduction work is **not established**.
- The pre-CoW control (`4428104c`) could not run the script: it lacks the `O_APPEND` fix of `3839b9a7`, so `printf >> Cargo.toml`
  destroys the manifest and the run fails in 4 s. Next step: the same control with a pre-written `Cargo.toml`, at 8 GiB.
- `-j4` is unaffected: 5 of 5 fresh-boot runs at 3 GiB passed on the unsafe-reduced kernel, ~129 s wall each, free pages back to
  baseline and `cowrefs=0` after each, no `BorrowCell` overlap panic.

## 3. The guest wall clock ran about 1.5x fast — FIXED

The identical build was "3m 09s" by the guest's `cargo`/`time` and 128-130 s by the host's `date`. **Cause:** `net::calibrate` measures the TSC against the PIT, and under KVM
there is no PIT behind port 0x61 (`net: no PIT behind port 0x61 -- TSC rate is a guess (2500/us)` in the boot log), so the clock ran at a fixed guess of
2.5 GHz on a 3.793 GHz TSC: 3793/2500 = 1.52 fast. The LAPIC timer calibration, a line earlier, had already measured the real rate
(`tsc=3795651900 Hz`). **Fix:** with no PIT, `net::use_lapic_rate` takes `akuma_lapic_amd64::tsc_hz()`; the fixed guess is only for a kernel where that has not run.
Now `calibrated 3793 cyc/us` on the ryzen (host `cpu MHz` 3793.029) and `sleep 20` measures 20 s; on TCG it is 996 cyc/us, where the guess would have run the clock 2.5x *slow*.
Every guest-side duration written before 2026-10-30 (including the 3m03-3m52 `-j4` figures in the CoW doc and the 11m41s in section 6) is on the old, fast clock.

## 4. The guest sshd corrupts binary stdin — OPEN

Piping a gzip tarball into `ssh -p 2224 root@… 'tar xz'` fails with `gzip: invalid magic`; base64 text piped to `base64 -d | md5sum`
also returned the empty-input digest. `[SSHD] bridge_process: couldn't open stdin for pid N` appears on the console for commands that
get stdin. Commands whose payload rides in the command string work (all the test scripts use that). Workaround used: the guest
clones the repo from GitHub over HTTPS (public repo, 9 s sparse clone). Cause not investigated (sshd's stdin bridge, `userspace/sshd`).

## 5. Smaller findings

- `git` in the guest printed `fatal: setsid failed: Function not implemented`; `setsid` (112) was unserved. **Fixed:** `setsid`/`getsid`/`getpgid` answer the pid (each task is its own
  session and group leader, there is no job control), `setpgid` accepts.
- **ssh starves during the in-guest build.** While the guest compiles its own kernel, new ssh connections fail with "Connection timed out during banner exchange" and a held session drops with "Timeout, server not responding", although the build is progressing (the console keeps moving, `[reap]` pids climb). The guest has one vCPU and four `rustc`; sshd is simply not scheduled promptly. Poll the console, not ssh, for completion. Not investigated (scheduling policy, `docs/reference/scheduling.md`).
- `TRAMP-MISMATCH tid=N THREAD_PID_MAP=… but table scan found …` was printed once per thread start (hundreds of lines on a serial console during a build). It is a benign
  diagnostic (the map is authoritative; teardown leaves `thread_id` set on purpose), so it is now capped at 16 lines then one "further reports suppressed" (`akuma-exec`, shared with AArch64; tally in `TRAMP_MISMATCH_TOTAL`).
- The `-j4` wedge of the first 3 GiB attempt and one local TCG run (CoW doc) has still not recurred in 5 + 5 clean runs.
- `scripts/kvm/taoxichuan_pop.sh` defaulted to 8192 MiB, which hides leaks; it now defaults to 3072.

## 6. Memory baselines: building the taoxichuan kernel itself at 3 GiB

`cargo build --release --target x86_64-unknown-none -p taoxichuan --features no-tests -j4`, committed tree `9f1e333f`, cold target dir.

| Where | Limit | Result |
|---|---|---|
| Linux (ryzen Pop OS host, nightly-2026-09-19) | cgroup `MemoryMax=3G`, `MemorySwapMax=0` | 10 s, **peak 447 MiB** (`memory.peak`, includes page cache), 0 OOM kills |
| Taoxichuan guest on KVM (nightly-2026-09-29, musl host toolchain) | 3072 MiB RAM | **PASS after the fixes in sections 1, 7 and 8**: 11m41s on the guest clock (~8 min real), 1,433,608-byte kernel, **peak >= 2,460 MiB used** (lowest free seen at a reap line: 156,787 pages = 612 MiB; a floor, sampled only when a process exits), process table peaked at 30 entries. Before the fixes: `execve` EFAULT (section 1), then "Process table full" (section 7), then an `rustc` ICE from the 1 GiB mmap slab (section 8) |
| **Akuma amd64** (full kernel, `e5e23d61`, Firecracker/KVM on the ryzen, 4 vCPU, 3072 MiB) | 3072 MiB RAM | **PASS**, `kbuild -c -j4 -p taoxichuan --features no-tests`, 35 s guest clock (~24 s real), **peak 1,356 MiB used** (MemTotal 3,144,704 kB, lowest MemFree 1,756,584 kB, sampled every 2 s from `/proc/meminfo`), idle 531 MiB used before the build, so the build added ~821 MiB |

So the memory comparison for the same compile is: **Linux 447 MiB peak (cgroup, page cache included) vs Akuma 1,356 MiB peak
(821 MiB above its own idle)**: Akuma needs ~3.0x Linux's peak, ~1.8x if only the build's increment over idle is compared. Neither is close to
3 GiB, so 3 GiB is not what stops the taoxichuan guest below. Caveats: the Akuma figure is a 2 s sample (a short spike can be missed, so it is a
floor); Linux's `memory.peak` counts page cache charged to the cgroup; different toolchains (gnu 09-19 on Linux, musl 09-29 on Akuma and the
taoxichuan guest). The Akuma run used the committed tree `9f1e333f` with a freshly `cargo vendor`ed `vendor/` (the rig's old one lacked
`unicode-width`); source was copied into the image from the host's GitHub clone, not rsynced from the laptop.

The taoxichuan guest has no `/proc/meminfo`. Its idle is the same order: 654,000 free pages of 786,432 at 3072 MiB, i.e. ~517 MiB used
before any work. Both kernels sitting at ~0.5 GiB idle suggests a fixed kernel reservation (heap) rather than a per-process cost; **not
verified**. That idle floor is the first thing to account for when asking why Akuma's peak is higher than Linux's.

**The memory ranking for the same compile is Linux 447 MiB < Akuma 1,356 MiB < taoxichuan guest >= 2,460 MiB.** The taoxichuan figure is
5.5x Linux's. Its likely cause is that file-backed private `mmap` is populated eagerly (every page copied out of the inode at `mmap` time,
`taoxichuan/src/main.rs`'s mmap arm), so each `rustc` pays for its whole 311 MB `librustc_driver` mapping instead of the pages it touches
- the "demand-paged mmap" item at the top of `TAOXICHUAN_EFFICIENCY_PLAN_GLM.md`. Not measured separately; the number is the evidence that
the item matters, not a profile of it.

## 7. Kernel panic: "Process table full (256 slots, 0 reclaimable RETIRED)" - a thread-exit leak - FIXED

- **Symptom.** In the in-guest taoxichuan build, `[PANIC] crates/akuma-exec/src/process/table.rs:73 Process table full (256 slots, 0
  reclaimable RETIRED)` at pid ~345, then ssh "server not responding". With free memory ~1 GiB.
- **Measured, not guessed.** `[reap]` lines now carry `procs=<active>/<retired>`. Before the fix the active count climbed monotonically
  with pid (pid 3 -> 1, 130 -> 94, 205 -> 180, 323 -> 249) while only 83 `[reap]` lines had appeared for 300+ pids: a leak, with 0 retired
  entries, so not a starved collector.
- **Cause.** `clone_thread` registers a real `Process` per thread. The only code that unregistered one was `reap_process`, which runs for a
  group *leader* on its parent's `wait4`. A thread's own `exit` (`task_exit_thread`) and `exit_group`'s `kill_siblings` freed the ring slot
  and left the `Process` ACTIVE for the rest of the boot. Cargo plus four `rustc` (each with a pool of codegen threads) is the first
  workload here with enough thread churn to fill 256.
- **It had been found before.** `AKUMA_AMD64_THREAD_LIFECYCLE.md` §3 is the same family on the amd64 kernel: "every `pthread_create` registers a
  `Process` row ... none of the drain sites is on the thread path". Its fix is the model used here: thread teardown *retires* the row, and
  `clone_thread` *drains at syscall entry*, before claiming anything - never at teardown, because reclaim runs `Process::drop` and the exiting
  thread is still running on its own row. (Reading that doc first would have saved the instrumentation round; the user pointed at it.)
- **Fix** (`taoxichuan/src/child.rs`): `retire_thread_process(pid)` in `task_exit_thread` and `kill_siblings`; `drain_retired()` at the top
  of `clone_thread_arm`.
- **Result.** The count plateaus: pid 3 -> 1, 32 -> 6, 129 -> 20, 208 -> 22, 295 -> 23, peak 30 over the whole 11-minute build. No panic.
- **Still open:** `register_process` is infallible (the note in `table.rs` and `docs/reference/subsystems/memory.md`); a genuine 256-live
  workload would still panic instead of answering `EAGAIN`.
- **Flake seen once, not explained:** one local `cow_leak_gate.py` run failed at round ~10 with "no `[spawn-reap]` line after `sh -c 'true; true'`" on
  the build that retired thread Processes (before the clone-entry drain); two reruns of the same binary passed 20/20, and every later run has.

## 8. `rustc` ICE "failed to spawn work thread" - the 1 GiB mmap slab - FIXED

- **Symptom.** With sections 1 and 7 fixed, `rustc` panicked compiling `taoxichuan` itself: `failed to spawn work thread: Os { code: 11,
  kind: WouldBlock }`. The console shows `mmap` answering ENOMEM (`nr=9 -12`, lengths 17 MB and 311 MB) with ~2.2 GiB of RAM free, and **no**
  `[clone] thread refused` line, so the refusal was musl's `pthread_create` failing on its stack `mmap`.
- **Cause.** `MMAP_SLAB` was 1 GiB of *address space* per process. One `rustc` maps the 311 MB `librustc_driver`, a ~17 MB stack per codegen
  thread and every dependency's rlib, which together exceed 1 GiB. The constant's own comment says "rustc `--version` maps ~1 GiB".
- **Fix.** `MMAP_SLAB` = 16 GiB (`0x4_0000_0000`); it is only a reserved range, far below the stack at `0x7000_0000_0000`.
- **Related, not fixed:** the 16-entry-order `nr=9 -12` failures also hide that the slab is shared by a process's threads, and a mapping is
  populated eagerly (section 6) - so address space was the first wall, and RAM is the next one.


## 9. A kernel write into a shared read-only page was not intercepted - FIXED

`copy_to_user` writes through the physmap, which never faults, so it needs its own CoW check (`cow_prepare_write`). That check had two holes:
1. an **unmarked** read-only page held by several spaces (shared text, or an `mprotect(PROT_READ)` page after a fork) was written through, landing in every holder;
2. when the CoW break itself failed (no frame), the result was discarded (`let _ = cow_resolve(..)`) and the write went ahead into the still-shared frame.
Now `cow_prepare_write` returns whether the write may proceed and `copy_to_user` answers `EFAULT` (as Linux does for a kernel write to a page the user could not write) in both cases; an *unshared* read-only page stays writable to the kernel.

## 10. `rt_sigprocmask` algebra shared with the AArch64 glue; one crate bug found by the differential test

`akuma-syscalls-glue`'s `sys_rt_sigprocmask` now calls `akuma_syscalls_signal::procmask` (it inlined the same formula). The new differential test (exhaustive over `how` 0..6 x 10 masks x 10 masks,
against the glue's old formula kept verbatim) found a real difference in the crate: `SIG_UNBLOCK` used `cur & !(set & !UNBLOCKABLE)`, leaving a stray SIGKILL/SIGSTOP bit in `cur` set forever; it is `cur & !set`, as the glue always had.
Glue's `rt_sigaction` was **not** switched: it is looser than the crate's `check` (a wrong `sigsetsize` silently zeroes the mask instead of `EINVAL`; a *query* of SIGKILL/SIGSTOP is refused), and changing that
would change the AArch64 kernel's behaviour. A pinned divergence, not an accident.

## 11. `COW_REFCOUNTS` was a `BTreeMap` (one heap node per shared frame) - FIXED

`akuma-pmm`'s share-count table was `Spinlock<BTreeMap<usize, u16>>`: a fork of a 100k-page process made 100k heap allocations while holding the lock. It is now
`cow_table::CowTable`: a fixed array of 4096 chunk pointers, each chunk 4096 `u16` counters covering 16 MiB of physical address space, allocated on the first share in
its range and freed with its last count; addresses at or above 64 GiB fall back to a map, so the type is total. A share costs no allocation unless it opens a chunk.
Pinned by a differential test (40 seeds x 4000 random inc/dec over addresses that straddle a chunk edge and the 64 GiB cutoff) against the old map's exact bodies, a 100k-share
test (25 chunks, not 100k nodes), and the existing `cow_refcount_tests`. Cost: 32 KiB of `.bss` for the pointer array (RAM, not image). Shared with the AArch64 kernel;
that kernel was not run here. Leak gate on taoxichuan: delta 0, `shared=15520 copied=320 taken=320`, unchanged.

## 12. `open("/dev/null")` after `close(0)` did not return 0 - FIXED

busybox ash starts every `&` background job with `close(0); if (open("/dev/null", O_RDONLY) != 0) error("can't open '/dev/null'")`: it relies on POSIX's lowest-free-fd rule.
`open_common` serves `/dev/null` with `alloc_fd_from(3, ..)` because fds 0-2 are implicit console slots, so the open returns 3 and every `cmd &` in a script prints
`/bin/sh: can't open '/dev/null': <stale errno>` (and the job does not run). Reproduced on TCG with `(echo b > /dev/null 2>&1 &)`. Found when a launcher used `nohup ... &` inside the guest.
Not a regression (the allocation rule is old). **Fix:** `Task.std_closed` remembers which of fds 0-2 the task closed and has not re-bound (copied on fork); `new_fd_min()` returns the lowest such fd, else 3, and
`open`, `socket`, `accept`, `pipe2` and `socketpair` allocate from there, so `close(0); open("/dev/null")` returns 0. A re-bound std fd closes like any other (the `close` arm only short-circuits an *unbound* one).
Checked on TCG: `(echo b > /dev/null 2>&1 &)`, `exec 0<&-; cat /dev/null`, and a backgrounded `sleep` all run without the error; battery and leak gate (delta 0) green.
