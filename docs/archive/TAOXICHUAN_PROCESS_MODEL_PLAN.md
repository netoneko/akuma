# Taoxichuan process model (stages 8-12) — PLAN (session N, 2026-10-26)

Planning only: this session boots nothing and changes no kernel or crate source. Its
deliverable is this document, on the pattern of `TAOXICHUAN_STAGE6_REST_PLAN.md`.
The fact it starts from: `taoxichuan/` has no `fork`/`clone` — one process shape,
`SPAWN` (run-to-completion nested child, `taoxichuan/src/child.rs`, 702 lines; one
level only, `nested_active` → `EPERM`, `child.rs:244-245`). The amd64 kernel has the
full machine — fork with CoW, a real process per thread, `execve`, `wait4`, pipes —
and, the load-bearing discovery of this session, **it is not welded shut**: the amd64
`sys_fork` is already a thin "fold" over `akuma_exec::process::fork_process`
(`amd64/src/usermode.rs:5520`, whose comment lists the five slices that made that
true). The composition question is therefore not "how do we cut a seam" but "what did
amd64 have to build to consume the shared code, and can taoxichuan build the same
twin". Every statement below was checked against the tree 2026-10-26 unless marked
(U#).

## 1. Needs per stage

Ground rules, checked: musl's `posix_spawn` is `clone(CLONE_VM|CLONE_VFORK|SIGCHLD)`
(`amd64/src/usermode.rs:2210-2226` names it as the arm every compile driver dies on);
musl `system()`/`popen()` are built on `posix_spawn` (popen adds `pipe2` + `dup2` +
`waitpid`) (U1: settle in §6). busybox ash forks for every non-final command of a
`;` sequence, every pipeline member, and every subshell — `1ee0dcdd`, recorded in
`TAOXICHUAN_STAGE7_TRIM_PLAN.md` §3 H3; redirects are `dup2`, collection is `wait4`.

| stage | gate | process-model calls it needs | served today? |
|---|---|---|---|
| 7 | curl HTTPS | none new: one process, no children | yes (modulo PT_INTERP — stage-6 plan M5, an exec-path item, not a process item) |
| 8 | git clone | `git` spawns `git-remote-https origin <url>` as a helper over two pipes (`AKUMA_SELF_HOSTING.md:403`, `AMD64_TRASHCAN_ISSUES.md:81`), then `git index-pack --stdin` (small packs: `unpack-objects`), while the parent pumps helper→packer and parks in `wait4` — **two children concurrent with the parent**. Needs: `fork` (`CLONE` w/o `CLONE_VM`), the `posix_spawn` arm (`CLONE_VM\|CLONE_VFORK\|SIGCHLD`), `execve`, `wait4`, `exit_group`, `pipe2`, `dup2/dup3`, `fcntl(FD_CLOEXEC)`, `getpid/getppid`. SIGCHLD may stay undelivered if `wait4` works off child-exit channels (see U6) | no: no fork arm at all (`STAGE7_TRIM` §3: `CLONE` appears only in `nr_name`), no `pipe2`, `waitpid_arm` is a slot poll (`child.rs:648`, returns 0 "still running") |
| 9 | cargo fetch | threads (`CLONE_THREAD\|CLONE_VM\|CLONE_FS\|CLONE_FILES\|CLONE_SIGHAND`), `futex`; child processes: cargo either fetches via libgit2 in-process or shells to `git` (U2). Cache locks: `flock`/`fcntl` | no threads, no futex |
| 10 | rustc | rayon threads; spawns the linker (`cc`/`ld`) via `posix_spawn` with output pipes; `wait4`; **spawn depth ≥ 2** (rustc under cargo; today SPAWN forbids nesting) | no |
| 11 | cargo build | all of 9+10 plus many concurrent children, jobserver pipes, `poll` on child pipes, `wait4` with `WNOHANG` | no |
| 12 | fixed point | nothing new over 11 (extraction is a file copy) | — |

Served today and reusable as-is: `SPAWN` (`child.rs:238`), `EXECVE` (`exec_replace`,
`child.rs:392`), `EXIT` (`child_exit`, `child.rs:585`), `DUP/DUP2/DUP3`, `FCNTL`
(scope unchecked, U3), `GETPID/GETPPID`, `RT_SIGACTION/RT_SIGPROCMASK` accepted as
no-ops (`main.rs:1993`).

## 2. Inventory of the amd64 path

Line counts are `wc -l` measured 2026-10-26. "Welded to" = what a consumer must
supply or adopt; the fork row is the prototype and the others follow its pattern.

| piece | where | crate | welded to | lines |
|---|---|---|---|---|
| `fork` | `amd64/src/usermode.rs:5520` → `fork_process` at `crates/akuma-exec/src/process/mod.rs:3194` | `akuma-exec` | `Process`+slot table (`akuma-slot-table`, 603); `mmu::UserAddressSpace::new` (dual-arch, `akuma-mmu`); child task spawn (`spawn_child_thread_and_publish`, `mod.rs:2531` → `akuma-threading`); `LifecycleGuard` (no-op without `smp-shared`); two `ExecRuntime` hooks: `alloc_process_info`, `fork_share_memory` | process/ 13,256 (mod.rs 4,125) |
| `vfork`/posix_spawn | `usermode.rs:2193` Clone arm → `sys_spawn_clone` → `fork_process(stack_ptr)` (child enters on the clone-supplied stack, return 0) | `akuma-exec` | same, plus x86 clone-arg order (tls last) | (same) |
| `clone` thread | `amd64/src/thread.rs:296 sys_clone_thread` | `akuma-threading` | thread pool + context switch (x86 arms in-crate, 101 x86_64 sites); `ThreadRuntime` callbacks (uptime, SGI, wake-core, EOI, print) + `X86ArchHooks` (`can_run`, `idle_slot`, `x86_pick_next`) | thread.rs 643; threading 7,955 |
| scheduler/preemption | `amd64/src/sched.rs` registers `X86ArchHooks`, drives the tick | amd64 in-tree | timer source, per-cpu idle loop | sched.rs 3,093 |
| `execve` | `usermode.rs:5112 sys_execve` → `amd64/src/loader.rs` load+`build_stack` → `Process::install_image` (load\|install seam, `AKUMA_AMD64_EXECVE_INSTALL_IMAGE.md`) | `akuma-elf` (170) + `akuma-exec` `image.rs` (480) + amd64 loader (710) | eager-mapping loader stays per-kernel; `install_image` is shared; `ExecRuntime::read_at_by_inode` for prefault | 710+170+480 |
| `wait4`/`waitid` | `usermode.rs:2293` → `crates/akuma-syscalls-glue/src/proc.rs:1072 sys_wait4` → child channels | `akuma-exec` `children.rs` (2,392) + glue | `CHILD_CHANNELS`/`ChildReaping`, `is_child_of_group` (tgid) | 2,392 |
| `exit`/`exit_group` | `usermode.rs:1943-1954` → process exit → channel write, slot free | `akuma-exec` lifecycle/table | lifecycle 125, table.rs 923 | — |
| `pipe2` | `amd64/src/fd.rs:1087` → glue `pipe::sys_pipe2`; `MAX_PIPES` ledger `amd64/src/pipe.rs` (369) | `akuma-pipes` (397) + `akuma-pipe` (229), pure | pipe table with waiters; **not** deps of `akuma-exec` (enter via glue/fd arms) | 397+229+369 |
| `dup`/`dup3`/`fcntl` | `amd64/src/fd.rs` (3,819) | `akuma-fdtable` (817) | `SharedFdTable::clone_deep_for_fork` (`akuma-exec` `process/fd.rs`, 858) | 3,819+817+858 |
| `kill`/signals | `usermode.rs:1987` → `amd64/src/signal.rs sys_kill`; sigreturn asm `.Lsig_return` (the `AMD64_FORK_VS_THREADS_SEGV.md` fix) | amd64 in-tree + `akuma-exec` `process/signal.rs` (807, `SharedSignalTable`) | frame build/restore per kernel | 1,578+807 |
| CoW fork share | `fork_share_memory` = `usermode.rs:3403` → `cow_share_and_demote_range` (`mod.rs:337`); write fault `amd64/src/idt.rs:776` | `akuma-cow` (140, pure decision) | page-table walk (`akuma-mmu` x86_64 side over `akuma-pagetable`+`akuma-pte-amd64`), frame ledger `akuma-user-space` (273) | 140+5124 |
| ExecRuntime glue | `amd64/src/exec_runtime.rs` — registration + hooks, "the `akuma-exec` runtime + config this target registers, and nothing else" | `akuma-exec` (consumer side) | **this is the file taoxichuan must write a twin of** | 642 |

Note: `akuma-user-access` (1,091; EL0/user copy boundary) and `akuma-exec-core`
(31; `UserContext`/frames, forbid-clean leaf) ride along inside `akuma-exec`.

## 3. Dependency diff

`[dependencies]`: amd64 lists **77** entries; taoxichuan lists **19** (`boot-amd64
comconsole gdt idt-amd64 trap-amd64 ryzen-amd64 alloc pmm primitives virtio ext2 vfs
cpu ring3-amd64 fdtable syscalls-linux syscalls-abi syscalls-poll net`). The
process-model adds, with `cargo tree -p <crate> --target x86_64-unknown-none -e
normal` node counts **run and checked this session**:

| crate | nodes | note |
|---|---:|---|
| `akuma-exec-core` | 6 | pure leaf |
| `akuma-cow` | 1 | pure decision |
| `akuma-pipe` / `akuma-pipes` | 1 / 2 | pure FIFO / table |
| `akuma-fdtable` | 1 | already a taoxichuan dep |
| `akuma-mmu` | 29 | dual-arch; x86_64 side needs `physmap_contains`, `akuma-pagetable`, `akuma-pte-amd64`, `akuma-user-space` |
| `akuma-threading` | 40 | pool + switch; needs `mmu`, `mmap`, `bkl` |
| `akuma-exec` | 84 | 19 direct `akuma-*`: adds tty, isolation, el0-entry, slot-table, mmap, syscalls-log + `spinning_top`/`embedded-io-async`/`lock_api`/`log` |

All seven resolve for `x86_64-unknown-none` — checked, not assumed. The one crate
that does **not** come with `akuma-exec` is the syscalls glue (`akuma-syscalls-glue`
+ its `akuma-vfs-glue` fs arms): amd64's wait4 rides glue, but the glue would drag
the whole fs-arm layer in; see §4/§6 (U4).

## 4. Seam options per piece

| piece | options | choice | cost |
|---|---|---|---|
| fork/vfork/wait4/children | **pull** `akuma-exec` whole / extract a leaf / write new | **PULL** — amd64 already proved the consumption pattern (`sys_fork` fold, 5 slices; `exec_runtime.rs` 642 lines is amd64's entire registration twin) | taoxichuan writes its `exec_runtime.rs` twin (~400-650) + a sched-lite (§ below); dep tree +84 nodes; main risk is timer/trap integration (U1) |
| fork, temporary | H3-f: SPAWN-shaped **blocking fork written new** (copy tracked regions via `add_region`, run nested, reap) — the option `TAOXICHUAN_STAGE7_TRIM_PLAN.md` §3 already priced | **CUT** as M2 scaffold only: lifts the one-command gate with zero new crates. Dead end otherwise: SPAWN is one-level (depth-2 spawn fails at stage 10) and a parked parent cannot pump a pipe >64K (stage 8 deadlocks) | ~150-250 lines, no new deps; throwaway once M4 lands |
| pipes | pull `akuma-pipes`+`akuma-pipe` / own tiny FIFO | **PULL** (pure, 1-2 nodes) | arms only, ~50 lines |
| fd dispatch | fix READ/WRITE/LSEEK/FSTAT to consult the fd table before console (H3 rewiring, `STAGE7_TRIM` §3) | **do first, no new crate** | ~10 lines per arm |
| threads | pull `akuma-threading` / cooperative own scheduler | **PULL** — unavoidable for 9-11 (rustc rayon, cargo) | needs sched-lite: taoxichuan twin of the `sched.rs` hook registration + a tick source (~300-600 lines); biggest single risk (U1) |
| signals | pull `process/signal.rs` + write sigreturn / none | **DEFER**: stage 8 can pass with SIGCHLD never delivered if wait4 reads exit channels (U6); sigreturn path written only when a probe demands it | 0 now; ~200 later |
| execve unify | keep taoxichuan loader / adopt `install_image` seam | **CUT at the seam**: keep taoxichuan's eager loader initially (amd64 also keeps its own loader — precedent in `AKUMA_AMD64_EXECVE_INSTALL_IMAGE.md`); `install_image` shared. PT_INTERP extract (stage-6 M5, `akuma-elf-exec`) lands before stage 7 and is reused by every later exec | ~0 (the M5 extraction is stage-7 work, already planned) |
| CoW | adopt `akuma-cow` + PF handler / eager copy in `fork_share_memory` | **EAGER COPY first** — taoxichuan has no page-fault handler (fault = task death, `main.rs:214`), and eager copy is what its SPAWN already does. `akuma-cow` (140, pure) adopted when rustc-class heaps make copies the cost (stage 9-10) | 0 now; PF handler is stage-9+ work |

**The honest alternative, weighed:** taoxichuan depending on `akuma-exec` whole takes
it from 19 to ~65-70 `akuma-*` deps. It is then close to the amd64 kernel: what
remains amd64-only is the device/hypervisor half (kvm, nvme, xhci, wifi, pci, fbcon —
the ~11,500 skippable lines plus the ~6,000 hypervisor lines of
`MINIMAL_SELF_HOSTING_EXPERIMENT.md` § The carve) and the in-tree `mm.rs`/`fd.rs`/
`sched.rs` acting halves. What is lost: the claim that taoxichuan is a distinct
minimal composition — it becomes the amd64 ABI floor with its own trap/console. The
experiment's own rule answers this: the goal is pulling crates, not writing them;
being close to amd64 *is* the point of the composition (`MINIMAL…` § Process).
`taoxichuan/src` grows ~4,916 → ~5,500-6,000 lines (runtime twin + sched-lite +
arms); nothing is deleted.

## 5. Milestones

Gates run over `/bin/ssh -p 2224 root@localhost '<cmd>'`; boots/tokens are guesses.

| # | milestone | mini-gate | boots | tokens (M) |
|---|---|---|---|---|
| M1 | fd dispatch order (H3 rewiring) | existing per-command gates unchanged + `sh -c`-free `echo x > /tmp/f` via one exec then `cat /tmp/f` — bytes land in file not console | 1 | 0.3 |
| M2 | **blocking fork (H3-f) + real WAIT4** — the cheapest lift of the biggest limit | `sh -c 'echo x > /tmp/f; cat /tmp/f; wc -c /tmp/f'` → `x`, then `2` | 1-2 | 0.5-1 |
| M3 | pipes | `sh -c 'echo hi \| cat'` → `hi` | 1-2 | 0.5 |
| M4 | **akuma-exec adoption**: runtime twin, sched-lite (tick, `X86ArchHooks`, `ThreadRuntime`), `fork_process` with eager-copy `fork_share_memory`, wait4 on child channels | `userspace/forktest` static probe: two children run concurrently, parent reaps both (`forktest/c_stress` names the arms not yet served — acceptable) | 3-6 | 2-4 |
| M5 | exec depth 2 + PT_INTERP (stage-6 M5 extraction consumed here if not already at stage 7) | `sh -c 'git --version'` after `apk add git`; `/usr/bin/curl --version` (stage 7 close-out) | 2-4 | 1-2 |
| M6 | signals minimum (SIGCHLD generation only if U6 demands) | forktest kill-probe rc 0 | 1-3 | 0.5-1 |
| M7 | **git clone (stage 8)** | `git clone https://<lan-mirror>/small.git /tmp/r; git -C /tmp/r fsck` agrees with host clone | 4-8 | 2-4 |
| M8 | threads + futex (stage 9) | `cargo fetch` of a pinned graph | 4-8 | 2-4 |
| M9 | rustc + linker spawn depth (stage 10) | in-guest `rustc hello.rs` + run | 4-8 | 2-4 |
| M10 | cargo build (stage 11) → fixed point (stage 12) | the ladder's own gates | 6-12 | 3-6 |

Order rationale: M2 is the H3-f write-new exception, justified only because the full
fork (M4) needs the scheduler and M2's gate unblocks every session-level gate
immediately; M4 is the one multi-session milestone (extraction is already done by
amd64 — taoxichuan is only the second consumer, so `run_gates.sh` parity on the amd64
side is free).

## 6. Unknowns that could kill the plan (each with its read-only settle)

| # | unknown | settle |
|---|---|---|
| U1 | does `akuma-threading` schedule on taoxichuan's trap/timer plumbing (x86 arms proven only against amd64's IDT + LAPIC/tick)? | read `ThreadRuntime`/`X86ArchHooks` field lists (`crates/akuma-threading/src/lib.rs`, `amd64/src/sched.rs:34-62`); grep each callback's LAPIC/SGI dependence; write the taoxichuan twin on paper first |
| U2 | which git transport the pinned cargo uses (libgit2 in-process vs `git` CLI child) — decides if stage 9 needs stage 8's fork at all | host: `strings $(command -v cargo) \| grep -m1 -i libgit2` |
| U3 | `FCNTL` arm scope in taoxichuan (FD_CLOEXEC? F_DUPFD?) — git/cargo pass cloexec fds everywhere | `grep -n "FCNTL\|F_DUPFD\|FD_CLOEXEC" taoxichuan/src/main.rs` |
| U4 | does wait4 force `akuma-syscalls-glue` (and its `akuma-vfs-glue` fs arms) into taoxichuan? | read `glue/src/proc.rs:1072-1140` for its imports; if glue is too wide, taoxichuan re-implements wait4's ~40 lines against `akuma-exec`'s children API directly (copy, not extract) |
| U5 | vfork (`CLONE_VM\|CLONE_VFORK`) with no parent suspension: git's helper protocol assumes child-exec-before-parent-resumes more strictly than Go did | read `usermode.rs:2210-2260` + `CLONE_AUDIT.md`; probe in M5 with a `posix_spawn`-heavy binary before M7 |
| U6 | is "SIGCHLD never delivered" observationally OK for git/musl (zombie reaping is channel-based, not signal-based)? | read `children.rs` channel semantics + musl `wait` behavior; decide before M6 |
| U7 | image/heap budget: +84-node dep tree into the microvm-sized image | host-side size check of the linked kernel at M4 start, zero boots |

## 7. What this changes in the ladder

Stages 8-12 are reachable by this route — the amd64 kernel already runs every call
they need through the same crates, and taoxichuan would be its second consumer, not
a reimplementation. Per-stage cost: stage 8 is the bulk (M1-M7, roughly 4-8 sessions
at this cadence); stage 9 adds threads/futex (M8, the `akuma-threading` payoff);
stage 10 adds spawn depth + the bigger mmap family (M9); stage 11 is orchestration
on top; stage 12 adds nothing new. The single point of failure is M4 (the scheduler
twin, U1); if it stalls, M2's blocking fork keeps the `;`-gate class of checks alive
but stages 8-12 proper wait on it — there is no cheaper route to concurrency than a
runnable-task scheduler, and every remaining gate needs one.
