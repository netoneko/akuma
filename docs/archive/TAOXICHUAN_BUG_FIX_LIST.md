# Taoxichuan bugfix audit — itemized list

Counting rule: the same as [`BUG_FIX_LIST.md`](BUG_FIX_LIST.md): one item per distinct, dated or named bug confirmed fixed,
indexed by the `docs/archive/` doc that records it. This list holds the fixes to **Taoxichuan**: the tokio kernel (since
B15 `crates/akuma-tokio-kernel-amd64`, before it `taoxichuan/src`), its rigs, the userspace built only for it (the
`linux-abi` libakuma and sshd paths) and the crates extracted for it while only it used them. A fix found through
Taoxichuan in code the other kernels also run (glue, a shared crate) is counted once, in `BUG_FIX_LIST.md`; the list
of those is at the end. Two more rules, because most of these fixes were made while the kernel was being written:

- A syscall the kernel had never served is **not** a fix when it is added, unless the doc records it as the defect
  behind a named failure (cargo hanging on `tkill`, c-ares failing on `recvmsg`).
- A defect in code written the same session and caught by that session's gate **is** counted when the doc records its
  cause (the M4 slice-3 sessions), the same as `SMP_SHARED.md`'s bring-up bugs in the Akuma list.

Subsystem tags are per file, except the experiment's agent log, whose bullets are split by subsystem.

## Statistics

- **Total distinct fixes counted:** 105
- **Docs contributing at least one fix:** 28
- **Subsystem categories:** 8

| Subsystem | Fixes | % | Docs |
|---|---:|---:|---:|
| Syscalls, Files & Pipes | 37 | 35.2% | 6 |
| Exec & ELF Loading | 8 | 7.6% | 2 |
| Processes, Fork, Threads & Scheduling | 33 | 31.4% | 9 |
| Memory & Copy-on-Write | 6 | 5.7% | 2 |
| Signals | 5 | 4.8% | 2 |
| Networking & Readiness | 4 | 3.8% | 2 |
| SMP, Time & Boot | 9 | 8.6% | 4 |
| Build & Integration | 3 | 2.9% | 1 |
| **Total** | **105** | **100.0%** | **28** |

**Largest single write-ups:**

- 59 — `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md` (split over eight categories)
- 6 — `docs/archive/TAOXICHUAN_BUGS_FOUND_2026_10_30.md`
- 6 — `docs/archive/TAOXICHUAN_CLEANUP_PLAN.md`
- 6 — `docs/archive/TAOXICHUAN_SMP.md`

---

## Syscalls, Files & Pipes (37 fixes, 6 docs)

### docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md
- `SyscallFrame` listed r10/r8/r9 in push order where the stack holds r9/r8/r10, so arguments 4 and 6 were swapped for every call with more than three; latent until the first `mmap`, where musl's flags arrived in the offset slot and `echo` ran out of memory (stage 3)
- `utimensat` was dispatched under the asm-generic number 88; x86_64's is 280, so apk's post-install mtime pass got `ENOSYS` and a clean install exited 1 (stage 6C)
- the `UNLINKAT` arm read its flags from argument 4 instead of 3, so `rmdir` never worked (pn S2)
- `ftruncate` was matched under the AArch64 number 46 (`sendmsg` on x86_64) and x86 77 answered `ENOSYS`, so cargo could not write `Cargo.lock`
- `writev` served fds 1 and 2 only and answered `EBADF` for every other fd; musl stdio flushes through `writev`, so busybox `wget`'s request never left and it waited for a reply to nothing (stage 7 H1; regular files had the same hole)
- opening `/dev/null` answered `EACCES` (stage 7 H2)
- `write_file_arm` advanced `*offset` per chunk and also wrote at `*offset + done`, so the second chunk of a multi-chunk `write` landed 1 KiB too far: apk's files came out as alternating data and zeros (stage 6C)
- `openat` still went through the read-only-era arm and answered `EBADF` to apk's open-the-directory-then-lock-relative-to-it calls (stage 6B)
- `stat` did not follow the final symlink, so busybox ash's PATH search saw `S_IFLNK`, judged it "not a program" and printed `Permission denied` without calling fork or execve (stage 6B)
- `lstat` resolved the *parent* through the symlink chain (stage 7, the last G4 fix)
- `read_link_target` read a **fast** symlink (target in the inode's pointer words) as a file, feeding the target bytes to the block mapper; every busybox link on the image is fast, so every SPAWN answered `ENOENT` and sshd fell back to its built-in shell (stage 6B)
- `load_task` started a fresh fd table for an in-place exec too, so ash's tail-exec lost its open fds (stage 7D)
- busybox ash saves fd 1 with `fcntl(1, F_DUPFD, 10)` around a redirect; the spawned shell's fds 0-2 had no table entry, the dup failed with `EBADF`, the restore never ran and `cat`/`wc` output leaked into the file (`260377ff`)
- `child_exit` never released a fork child's fds, so a counted pipe end leaked and the reader never saw EOF (stage 8 pa)
- `open` used a 128-byte path buffer, and cargo's build-script object paths are about 150 bytes: `EFAULT`, "could not write output ... Bad address"
- `readlink("/proc/self/exe")` answered `EINVAL`, which rust-lld's `current_exe` and musl's `$ORIGIN` need; a per-task `exe` path
- `read`/`write`/`readv`/`writev` returned `EFAULT` past 1 MiB; ld flushes its 4.6 MB output in one `writev` and BFD reported "No space left on device"
- `fchmodat`/`chmod` were no-ops, so ld's output executable kept its mode
- path `lstat` reported mode 0644 for every file
- the fd table had 16 slots, so `ld` opening about 20 rlibs got `EMFILE`
- `fcntl` record locks answered `EINVAL`, which SQLite (cargo's cache tracker) reported as error 3850, `IOERR_LOCK`
- `stat`/`lstat` reported `st_ino = 0` for every file, so busybox `cp -r`, which remembers `(dev, ino)`, hard-linked `Cargo.toml` onto `main.rs`
- `fstat` of a pipe said "regular file", so git's `index-pack` read a pipe as a file and died with "pack has junk at the end"
- `open(O_CREAT)` in a missing directory answered `EACCES` instead of `ENOENT`, so git never created `objects/xx`
- `O_NONBLOCK` was ignored on pipes and socketpairs: a read of c-ares' and curl's empty non-blocking wakeup pair parked forever
- `link`/`linkat` was a stub ("no hardlink arm yet") although the VFS has one

### docs/archive/TAOXICHUAN_CLEANUP_PLAN.md
- `akuma_vfs_glue::fs::mark_initialized` was never called, so every glue path answered `ENOENT` (step 2c, the first glue batch)
- glue buffers stdout in `current_channel()`, which for a forked child is its *exit* channel; the console shim keys on `Process.channel` (step 2c)
- the syscall stub's user-RSP stash was not saved and restored around a call, and glue's waits switch threads (step 2c)
- glue's pipe wake sink was not wired; it is amd64's, which wakes the slot with a fresh handle (step 2c)
- SIGPIPE's default action ran glue's `sys_exit_group`, which kills the caller through `akuma-exec`, so the ring row never left `running` and the shell's `wait4` slept forever; the slot-purge hook now records the status and wakes the waiter (step 2c)
- after `readlinkat` became glue's, `readlink("/proc/self/exe")` was answered from procfs, which Taoxichuan never mounted, so the first fixed-point attempt's in-guest `rustc` could not find `librustc_driver` through its `$ORIGIN` RUNPATH; `/proc` is mounted and exec records its path in `Process.image` (`43d79b7c`)

### docs/archive/TAOXICHUAN_CARGO_STALL_REGRESSION.md
- a non-blocking read of an empty pipe answered `EAGAIN` for EOF as well as for "writers still live", so cargo's `read2` loop polled two finished child pipes forever after the first `rustc -vV` (`cf44f840`, fixed in `pipes::read_arm`)

### docs/archive/TAOXICHUAN_GOOSE_SQLITE_STALE_FD_SIZE.md
- `OpenFile::File` recorded the file size once, at `open`, and every read clamped to that copy, so a second open description never saw another's appends: SQLite read a short WAL frame and goose failed with `(code: 522) disk I/O error`; the size is the inode's now
- `dup2` displacing a socket, and both exec close-on-exec sweeps, released the connection unconditionally, so a session shell's `exec 4<file` over its inherited copy destroyed the socket sshd's own fd 4 still named (step 1e, `sock::release_unnamed`)

### docs/archive/TAOXICHUAN_UNAME_SAYS_AKUMA.md
- `uname` answered `Akuma akuma 0.0.8 ...` once it moved to glue's compile-time `UTSNAME`; a kernel now registers its own identity (`proc::set_uts_identity`) and Taoxichuan says `Linux taoxichuan <version>-<sha>-taoxichuan`

### docs/archive/SYSCALL_CONSTANT_AUDIT_2026_10_30.md
- `akuma-ring3-amd64::nr` (Taoxichuan's dispatch table) had `GETPID = 38` and `SENDFILE = 39`, both off by one, so `setitimer` (38) answered the caller's pid while it still had a `GETPID` arm; the third x86_64 table is gone

## Exec & ELF Loading (8 fixes, 2 docs)

### docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md
- `build_stack` sliced backwards from the end of a 512-byte initial stack block and apk's long argv and env overran it by one pointer word: a kernel PANIC on the first install write (stage 6B, `28dc8ba2`)
- `full_argv` was pre-filled with `argv[0]` and sliced one entry past the arguments, so every exec'd image saw a trailing duplicate of `argv[0]` (apk 3 parsed it as a package name); `MAX_ARGV` was 8 (stage 6C, `ea2c5b2a`)
- the interpreter's page loop indexed the whole image at a vaddr-relative offset, dropping `p_offset`, so musl's text was copied from the wrong file offset and the first dynamic exec faulted (M5, `9298f504`)
- the initial stack was one 4 KiB page with argv capped at 16 entries and env at 8, so `collect2`'s ~50 arguments reached `ld` as 16 ("no input files")
- `execve` refused any relative path (`./hello` was `ENOENT`)
- executables over 16 MiB could not be exec'd (`read_file` refuses them); the exec path reads the image in 1 MiB steps through the inode

### docs/archive/TAOXICHUAN_B8_ONE_ELF_LOADER.md
- the interpreter loop sliced `ldso[p_offset..p_offset + p_filesz]` with nothing checking the range against the file, so a short `PT_INTERP` file was an index panic in ring 0; `akuma-elf-exec::load_segments` refuses it
- no segment was checked against the user half, so a segment at or above `0x0000_8000_0000_0000` (or one whose end overflowed) walked the PML4 slots every root shares with the kernel; both are refused before a frame is taken

## Processes, Fork, Threads & Scheduling (33 fixes, 9 docs)

### docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md
- `child_enter` (naked asm) loaded `ef.rsi` early and kept dereferencing the loaded value: a page fault at `cr2=0x20` on the first real SPAWN (stage 6B)
- the fork's page-table clone walked the kernel half too and leaf-copied from frames above RAM: clone only PML4 slots 0..256 (stage 8, `cb19bcca`)
- `fork_arm` did not save and restore the crossing statics, so a later in-place exec restored the fork's dead callee-saved registers (`0963376f`)
- `spawn_arm` and `fork_arm` lowered the child's stack top until a depth-2 exec band sat below the 32 KiB syscall stack array, in read-only data: an indirect jump fetched `rip=0` (`a6822831`)
- fork children took the spawn ring's blind `NEXT_SLOT++ % NSLOTS`, so a four-fork pipeline wrapped onto the live shell's slot and wiped its session buffer (stage 8 pa)
- moving the crossing restore into a function let the compiler use registers across the `asm!` that owns them, and the child resumed with a wrong callee-saved register; the tail is a naked function now (`f759944d`, `b1acc953`)
- the spawn crossing never reserved its ring-slot number in the threading pool, so the pool's recycler could hand it out: the pipeline deadlock (`86d90a10`, `ff698f61`)
- pipe refcounts were not bumped when the fd table was cloned for a fork (`161f55a1`)
- `spawn_arm` ran the child on a slot the pool still marked INITIALIZING, which the picker never takes, so the first preempting tick switched away from it for good: every ssh exec hung with preemption on
- `net::sleep_until`, the loop under `poll`, halted with interrupts masked and never yielded, so rustc's forked `gcc` sat READY forever while the parent polled its pipes
- Taoxichuan had its own pid counter beside `akuma_exec::process::allocate_pid`, so a thread and a fork child got the same pid and one table row
- no x87/SSE state was saved at a context switch, so a preemption or park handed one task's vector registers to the next: rustc ICEs that looked like memory corruption, only in multi-threaded compiles
- `wait4` matched a child's parent by the calling *thread's* pid, so a child forked by one cargo job thread and waited on by another got `ECHILD`; parentage is the thread group
- an exited process's ring slot was recycled by the next fork before its parent waited, erasing the zombie's status (`ECHILD` at `-j4`)

### docs/archive/TAOXICHUAN_BUGS_FOUND_2026_10_30.md
- `execve` answered `EFAULT` ("Bad address") for any call with more than 128 `argv` or `envp` entries or a string over 4096 bytes, because the pointer-array reader mapped "too many" to `None` and the caller mapped `None` to `EFAULT`; cargo's heaviest `rustc` call for this kernel has 344 arguments and a 5192-byte one, so the in-guest build died at its first big crate. The caps are 448/256/32 KiB, an oversize block answers `E2BIG`, and the argument block is 12 pages
- A thread's own `exit` and `exit_group`'s `kill_siblings` freed the ring slot but never unregistered the thread's `Process` (only a group leader's `wait4` did), so every thread leaked a process-table entry and a `cargo build` reached `Process table full (256 slots, 0 reclaimable RETIRED)` at pid ~345 with a gigabyte free; thread exit now retires the row and `clone_thread` drains at syscall entry, the amd64 kernel's fix for the same family
- the guest clock ran 1.52x fast under KVM: with no PIT behind port 0x61 the TSC rate fell back to a fixed 2.5 GHz guess while the LAPIC calibration, a line earlier, had measured the real 3.793 GHz; it now uses `tsc_hz()` from the LAPIC (a TCG guest, where the guess made the clock 2.5x slow, is fixed too)
- `copy_to_user` wrote through the physmap into an unmarked read-only page that several spaces held (and, when the CoW break failed for lack of a frame, discarded the failure and wrote into the still-shared frame), corrupting the other holder; both now answer `EFAULT`
- `open` after `close(0)` could never return 0 (fds 0-2 are implicit console slots and allocation started at 3), so busybox ash's `cmd &` (`close(0); if (open("/dev/null") != 0) error`) failed for every background job; the task now remembers its closed std fds and allocates the lowest free one
- the 1 GiB per-process mmap slab answered `ENOMEM` to `rustc`'s thread-stack `mmap` (it maps a 311 MB `librustc_driver` plus a ~17 MB stack per codegen thread), which musl reports as `EAGAIN` ("failed to spawn work thread", an ICE) with 2 GiB of RAM free; the slab is 16 GiB of reserved address space

### docs/archive/TAOXICHUAN_M4_SLICE3_PE_RESULT.md
- the EXIT arm sent a real fork child's `exit_group` into the M2 `child_exit` because `is_real_task()` could not tell a real child from an M2 one on slot 1, abandoning sshd mid-spawn (Fix A', a bind-time `SpawnSlot.real` bit, `ffb09617`)
- `sched_root_of`/`sched_task_slot_of` hardcoded the shell as threading tid 0 while its tid was its ring slot, so the wake switched the shell back under a stale fork child's CR3 and its `ret` popped 0 (Fix C, `ecaca7b1`)

### docs/archive/TAOXICHUAN_M4_SLICE3_PF_RESULT.md
- the tick switch armed before calibration and the VM exited silently inside `net calibrate`; it arms with the first spawn crossing (`bc5c57e6`)
- a ring-3 tick after the exec'd shell's exit indexed `THREAD_STATES[usize::MAX]`; the tick switch requires a live slotted current (`15cc24e0`)

### docs/archive/TAOXICHUAN_M4_SLICE3_PG_RESULT.md
- `switch_to` loaded the incoming task's CR3 but never `TASK_SLOT`, so after a park-resume and a tick switch the fork child's `writev` gathered zeros from another task's address space and musl spun on the 0 answer (`a4752d3b`)
- the M2 exec arm passed the spawn's save slot to `child_enter`, so ash's in-place exec overwrote sshd's resume frame and the session hung with its bytes already drained (one `EXEC_SAVE` scratch, `5f59e8af`)
- `bind_child_task` left a real fork child's cross record at its defaults (`out_slot: 0`), so `cat`'s output landed in slot 0's buffer and was reported written (`a0eb7664`)

### docs/archive/TAOXICHUAN_M4_SLICE3_PH_RESULT.md
- every guest wait path spun with `pause`, burning the one host core the relay thread shares with the vCPU, so the relay starved and connections stalled; `net::wait_lap` halts (`60658556`)
- the network was pumped only from the stub arms and `nanosleep`; one drained netpoll lap per tick backstops it (`aba585a1`)
- an in-place exec entered the new image with the outgoing task's `fs` base (musl's TLS), so grandfork faulted on its stack guard; both exec crossings zero it (`cde2247d`)

### docs/archive/TAOXICHUAN_EXIT_GROUP_RUNNING_SIBLING.md
- `exit_group`'s `kill_siblings` marked a sibling RUNNING on another core TERMINATED, which does not stop it: it made a `futex` call with its `Process` retired, parked, and was revived; running siblings now get `request_thread_kill` and the exiting thread waits for them

### docs/archive/TAOXICHUAN_METAL_SH_SPAWN_EPERM.md
- the vm-dev image carried the cooperative sshd, whose private `SPAWN` init (a pool thread since S1) refuses with `EPERM`, and `trace_err`'s per-number cap hid the answer: every `sh -c` fell back to the built-in shell; the image runs the `linux-abi` sshd, `spawn_arm` says why it refused and `SPAWN` is never capped (`d616a7f9`)

### docs/archive/TAOXICHUAN_SSH_STDIN_PUMP.md
- the `linux-abi` `spawn_full` piped only stdout, so the child inherited the spawner's fd 0 and sshd's pump wrote into a stdin the child never read, with no EOF either: every ssh exec that sent stdin hung; `SpawnResult` carries the write end of a real stdin pipe (`15ff2709`)

## Memory & Copy-on-Write (6 fixes, 2 docs)

### docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md
- `mmap` demanded `fd == -1` for `MAP_ANONYMOUS` while libakuma passes 0 (Linux ignores it), so sshd's first allocation answered `ENODEV` (stage 4)
- the mmap area was a 1 GiB bump allocator with a no-op `munmap`, so a big `rustc` ran out of address space with 300k pages free; `munmap`/`mremap`/`MAP_FIXED` free frames and reuse holes
- rust-lld writes its output through a writable `MAP_SHARED` file mapping, which was never written back: the file stayed empty and cargo got `Exec format error` running the build script
- every image had a fixed 64 KiB stack and cargo's main thread faulted past it; a fault just under the stack grows it up to 8 MiB

### docs/archive/TAOXICHUAN_COW_FORK.md
- `openat` ignored `O_APPEND`, so `echo b >> f` overwrote from offset 0 and `printf ... >> Cargo.toml` destroyed the manifest
- `arch_prctl(ARCH_GET_FS)` wrote user memory by hand through the physmap, bypassing the CoW break; it goes through `copy_to_user`

## Signals (5 fixes, 2 docs)

### docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md
- `rt_sigaction`/`rt_sigprocmask` were no-ops that never wrote `oldact`/`oldset`, so std's child-side `signal(SIGPIPE, SIG_DFL)` read stack garbage as `SIG_ERR` and `Command::spawn` failed with a stale errno ("File exists")
- `tkill` was `ENOSYS`, so after a failed build cargo hung joining its jobserver helper thread; `tkill`/`tgkill` interrupt the target and `poll` and pipe `read` return `EINTR` (`f41c2891`)

### docs/archive/TAOXICHUAN_SIGNAL_DELIVERY.md
- the dispatcher restored the user-`rsp` stash at exit, which undid the `rsp` a signal frame or `rt_sigreturn` set, so a handler ran on the interrupted stack and never reached the restorer
- `kill`/`tkill` on the caller itself set an interrupt flag that made its next glue call answer `EINTR` without running, so musl's `raise` lost its mask restore; the flag is consumed at every return to ring 3
- exit never reparented children, so a killed tree's grandchildren stayed registered under a dead parent and `killtree` exhausted the 48-slot ring by its third round

## Networking & Readiness (4 fixes, 2 docs)

### docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md
- `getsockname` wrote `sin_family` in host byte order (`to_be_bytes`), so c-ares read `0x0200` (stage 9)
- a socket `dup` + `close` destroyed the connection behind the other fd (git dups its socket and closes the copy); sockets close on the last reference across all tasks, and an exiting task releases its sockets
- `recvmsg` and `sendmsg` were unserved, so cargo's c-ares could not receive DNS replies ("Could not resolve host") while musl-resolver `curl` worked

### docs/archive/TAOXICHUAN_TOKIO_MULTI_EPOLL_FINDING.md
- an edge-triggered eventfd reported only its first wake: mio's waker is written on every wake and never read, so a rising-edge-only model lost the rest and tokio's multi-thread runtime progressed only on its timeout; `epoll::on_eventfd_written` re-arms every `EPOLLET` entry naming the eventfd (GET 24,968 ms -> 53 ms)

## SMP, Time & Boot (9 fixes, 4 docs)

### docs/archive/TAOXICHUAN_SMP.md
- `switch_to` left CR3 alone for an idle thread, so an idle core sat on a finished task's root while another core reaped it and freed its page tables: a triple fault; an idle thread loads the kernel root
- an idle core picked and switched without the lock while `switch_to` read state another core was mutating: `[SWITCH BADFRAME]` and `InvalidOpcode rip=1`; the idle lap takes the lock
- `can_run` pinned only idle slots above `NSLOTS`, so an AP picked the BSP's idle thread and ran on its stack
- the AP boot root `__ap_pml4` is linked VMA = LMA, and subtracting `KERNEL_VMA` wrapped: the AP triple-faulted before printing anything
- the `EPOLLET` edge-drain hooks sat inside the `trace-err` block, so a plain kernel neither built (`-D dead-code`) nor cleared an edge (`72585f1d`)
- an AP read its APIC ID before setting `IA32_APIC_BASE` enable, which Akuma's SVM model starts clear, so the first 4-vCPU metal boot died on a nested page fault at `0xfee00020` (`dac8a275`)

### docs/archive/TAOXICHUAN_ONE_CLOCK_TIME_FOLD.md
- a timed park on an otherwise idle core never expired: the wake pass runs inside `x86_yield` and the idle loop yielded only for READY threads; it also yields on `wake_is_due` (exposed when `nanosleep` stopped spinning)

### docs/archive/AKUMA_AMD64_SCHED_CRATE.md
- the trace build's idle-lap hang dump ran on every idle core with no lock and panicked (`BorrowCell mutably borrowed`) when four cores reached the lap count together; it is cpu 0 only, under the lock

### docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md
- `lapic_init` ran before the PMM and heap were registered and its `map_device` allocates page-table frames: a boot panic (`9dc684ef`)

## Build & Integration (3 fixes, 1 doc)

### docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md
- `akuma-boot-amd64` assembled its AT&T `boot.s` unconditionally, which broke the Mac's pre-commit hook (`att_syntax` is x86-only); the `global_asm!` is gated on `target_arch = "x86_64"` (stage 1)
- `crates/akuma-ring3-amd64` went into the workspace `default-members`, so the AArch64 kernel build compiled its x86 `naked_asm!`: 55 assembler errors (stage 2)
- `akuma-net`'s new `socket_local_addr` lacked `#[cfg(feature = "smoltcp")]`, which broke the AArch64 `extreme-size` build at the merge (`1dd4c29a`)

---

## Found through Taoxichuan, counted in `BUG_FIX_LIST.md`

These are fixes to code every kernel runs, so the Akuma list counts them:

- `DUP_LOST_O_NONBLOCK.md`: a `dup` lost `O_NONBLOCK` in glue and `akuma-fdtable` (tokio's signal driver parked forever).
- `TAOXICHUAN_B9_MADVISE_PIDFD_SIGSUSPEND.md`: glue's `pidfd_open` accepted `O_CLOEXEC`; `rt_sigpending` was served by no kernel.
- `TAOXICHUAN_BUGS_FOUND_2026_10_30.md`: `akuma-syscalls-signal`'s `SIG_UNBLOCK` left SIGKILL/SIGSTOP set; `akuma-pmm`'s CoW
  table was a `BTreeMap`.
- `AKUMA_VIRTIO_IN_TREE.md`: the in-tree virtio-net `send` left out the header.
- `AMD64_BOX_SPAWN_EXT_REGRESSION.md`: the glue `exec` feature, gated for Taoxichuan's audit surface, broke amd64's `box run`.
- `TAOXICHUAN_CLEANUP_PLAN.md`: amd64's self-test build had not compiled since the boot-kit extraction.

## Files scanned with zero counted fixes

Scanned 2026-11-02, closing `rust-org-il-experiment`.

- **Plans** (in-flight or carried out elsewhere; a fix they predicted is counted in the result or the log):
  TAOXICHUAN_AMD64_EXTRACTION_PLAN_GLM, TAOXICHUAN_COW_FORK_PLAN_GLM, TAOXICHUAN_CRATE_EXTRACTION_PLAN,
  TAOXICHUAN_EFFICIENCY_PLAN_GLM, TAOXICHUAN_M4_PI_STALL_PLAN, TAOXICHUAN_M4_SCHED_LITE_PLAN, TAOXICHUAN_M4_SLICE3_PE_PLAN,
  TAOXICHUAN_M4_SLICE3_PF_PLAN, TAOXICHUAN_M4_SLICE3_PG_PLAN, TAOXICHUAN_M4_SLICE3_PH_PLAN,
  TAOXICHUAN_M4_SLICE3_VERDICT_PLAN, TAOXICHUAN_PROCESS_MODEL_PLAN, TAOXICHUAN_STAGE6_PLAN, TAOXICHUAN_STAGE6_REST_PLAN,
  TAOXICHUAN_STAGE7_TRIM_PLAN, TAOXICHUAN_STAGE8_PL_SPAWN_PARK_STALL_PLAN, TAOXICHUAN_STAGE8_PM_STANDARD_SLOTS_PLAN,
  TAOXICHUAN_STAGES9_11_PN_RUST_SYSCALLS_PLAN, TAOXICHUAN_UNSAFE_REDUCTION_PLAN_GLM, TAOXICHUAN_SWITCH_TO_CRATES.
- **Measurements and records:** TAOXICHUAN_B10_SYSCALL_CENSUS, TAOXICHUAN_RUST_SYSCALLS_CENSUS_PN,
  TAOXICHUAN_RUST_TOOLCHAIN_INVENTORY, TAOXICHUAN_FIXED_POINT (its pitfalls are counted under TAOXICHUAN_BUGS_FOUND or are
  procedure), TAOXICHUAN_STAGE1_LOGS, TAOXICHUAN_WX_POLICY (a new policy, not a fix), MINIMAL_SELF_HOSTING_EXPERIMENT (the
  brief).
- **Open by their own text:** TAOXICHUAN_J4_BKL_CONVOY (the in-guest `-j4` BKL convoy; its p3 repair is a rig change);
  the SMP=4 ring-3 `#GP` and the `-j8` failure (TAOXICHUAN_CLEANUP_PLAN, TAOXICHUAN_COW_FORK).
- **Not counted twice:** the agent log's entries for sessions pe and ph name the fixes counted under their RESULT docs; its
  fixed-point entry names the TAOXICHUAN_BUGS_FOUND fixes; the cleanup plan's rows 1e, B8, B9 and X2 name fixes counted under
  their own docs. Rig fixes (`epoch=` on the TCG rig, legacy virtio-mmio on the rigs, `boot_dev.sh` stale-kernel boots, the
  fixed-point drivers' ssh and `nc` hangs) are tooling, not kernel fixes.
- **Fix commits with no doc to cite** (uncounted): `1d331327` (`wait4` learns whose child a zombie is, the `PARENT_OF` map,
  session pj), `4440d69f` (`idle_main` halts instead of spinning, pj), `cf8a5468` (the parked-resume `fs` base defect, a
  per-slot `IA32_FS_BASE` shadow, pi), `cf1b88e4` (`fork_arm` keyed on the syscall number: musl issues raw `vfork`/`fork`
  with no flags word), `4d84edd2` (apk's live repositories line dialled `:8123` on every exec and hung with no listener),
  `12eeff10` (the SMP kernel lock is a ticket lock; an unfair spin starved a core that asks rarely), `e3026c94`
  (`rt_sigsuspend` + `rt_sigreturn`: busybox ash's `wait` no longer hangs), `10f84fc5` (the poll-pipe kind fix).

## Background

- [`BUG_FIX_LIST.md`](BUG_FIX_LIST.md): the Akuma list and its counting rule.
- [`docs/runbooks/update-bug-fix-list.md`](../runbooks/update-bug-fix-list.md): how both lists are updated and verified.
- [`MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`](MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md) and
  [`TAOXICHUAN_CLEANUP_PLAN.md`](TAOXICHUAN_CLEANUP_PLAN.md): where most of these were first written down.
