# PLAN-pn — Taoxichuan stages 9–11, session pn: the syscalls `rustc`/`cargo` need and the order to enable them

Plan for the supervisor to execute. Taoxichuan must go from "tcc compiles C" to
"`rustc hello.rs`, then `cargo build`, then the fixed point". This plan classifies
every needed syscall/behaviour into one route — **A** already a family crate under
`crates/akuma-syscalls-*` (call the pure decision, keep effects local), **B** in
`akuma-syscalls-glue`/`src/syscall/` but not yet a family crate (extract pure part to a
new `akuma-syscalls-<family>` crate with host tests; effects stay with each caller),
**C** only in `amd64/src/` (extract into a crate first, then pull), **D** nowhere (write
new, in a crate, with the reason no existing crate fits). The standing rule holds: pull
from a crate; extract from `amd64/src/` only what lives only there; write new only when
it exists nowhere. Planning only — no kernel code, no boots. Branch
`rust-org-il-experiment`, push `litter` only.

**Executive answer.** The measured census stops at the first `clone` (rustc's ctrlc
thread). Everything before it is slab size plus a handful of no-op arms — `sigaltstack`,
`membarrier`, the legacy `readlink`(89), a real `mprotect`, `madvise`, and splitting the
one 64 MiB `SLAB` constant (main.rs:642) into a ≥1 GiB mmap slab and a ≥512 MiB brk slab —
and **all of that is INDEPENDENT** of session pm's process-slot switch. Everything at and
after the first `clone` — thread `clone` (which is `akuma_exec::process::clone_thread`,
already a crate call), the `akuma-syscalls-sync` futex family, thread groups and
`exit_group` ordering, `tkill`/`tgkill` + real signal delivery, `socketpair`/`sendmsg`/
`recvmsg` (the spawn handshake, glue `unixsock.rs`), and the linker's writable
`MAP_SHARED` + `msync` writeback (glue `mem.rs`) — is **AFTER-PM**, because every one of
those glue/futex/thread arms looks processes up by pid/tid and parks through the
threading slots. The repo has already done the extractions the ladder needs: the futex
decision is host-tested in `akuma-syscalls-sync` (amd64/src/futex.rs is effects only),
the memory decisions in `akuma-syscalls-mem` + `akuma-mmap`, the wire shapes in
`akuma-syscalls-linux`. New code is needed only for: the x86_64 signal frame
(amd64/src/signal.rs — extract, route C), `sched_getaffinity` (amd64-only, route C),
`getpriority`/`setpriority`/`getrusage`/`rseq` (nowhere, route D), the legacy x86_64
`AT_FDCWD` fold table (amd64/src/usermode.rs:1503-1530 — extract to
`akuma-syscalls-abi`, route C), an ext2 `linkat` arm (cargo hardlinks artifacts), and
Taoxichuan-side effects (dirty-PTE writeback walk; park/wake for futex waits).

**Order.** S1 slab constants + S2 the independent ENOSYS tail (gate: `rustc --version`
exit 0 with a clean `[err]` trace) → S3 pm's init-as-Process (gate: fork from init
works) → S4 clone-thread + futex + robust list (gates: a tcc-built pthread+futex
program; then `rustc hello.rs -o hello && ./hello`) → S5 spawn handshake AF_UNIX →
S6 cargo's fs batch (`pwrite64`, `fsync`/`fdatasync`, `linkat`, `chdir`/`getcwd`, long
argv/E2BIG, `close_range`, `sched_getaffinity`, `getpriority`/`setpriority`) → S7
writable `MAP_SHARED` + `msync` writeback (gate: `cargo build`, output non-empty and
runs) → S8 signals (the ~10,400-`tkill` storm) → S9 the fixed point (stage 12; needs the
memory-model story, out of scope here). ≈300–400 executor calls ≈ 3–4 sessions. If only
half fits: S1–S4 is the thesis — `rustc --version` and a running `hello` — and S6+ waits.

## 0. Premises (HOLDS / WRONG / UNCHECKED per row)

| # | premise | state | evidence |
|---|---|---|---|
| P1 | Taoxichuan serves only the `*at` fs forms; legacy 83/82/87/84/88/89/90/92 have no arms | HOLDS | `taoxichuan/docs/reference/syscalls.md` table |
| P2 | One `SLAB` const (0x0400_0000) bounds both brk and mmap; 312 MB `librustc_driver` ENOMEMs under it, loads at 1 GiB | HOLDS | main.rs:642,1976,1982,2891,3591; supervisor's measurement |
| P3 | No threads: `clone` is the M2 blocking arm (SIGCHLD\[|VM\|VFORK] only, else EINVAL); gettid=1 | HOLDS | syscalls.md row 56; measured EPERM on the ctrlc clone |
| P4 | futex, sigaltstack, membarrier, madvise, tkill/tgkill, socketpair, msync have no arms (ENOSYS) | HOLDS | syscalls.md table + known-gaps tail |
| P5 | `rt_sigaction`/`rt_sigprocmask` are accepted no-ops; no delivery exists | HOLDS | syscalls.md rows 13–14, main.rs:2451 |
| P6 | `mprotect`/`munmap` accepted-and-ignored; no W^X policy; `tcc -run` faults on a non-exec page | HOLDS | syscalls.md rows 10–11; memory.md |
| P7 | mmap is anon + MAP_PRIVATE file fill, eager, no CoW, no page-fault handler (a fault kills the task) | HOLDS | syscalls.md row 9; memory.md; gaps.md pinned exclusions |
| P8 | `rust-toolchain.md`'s two lists are an inventory scored against the old table; it omits the startup census (threads!) and its headline needs correcting | HOLDS | read this session (98 lines) |
| P9 | pm's real fork (`akuma_exec::process::fork_process`, mod.rs:3194) has landed with parked wait4; init is still the boot task (fork from init EPERM); pm S1–S3 pending | HOLDS | processes.md; switch-to-crates.md "the slices are not yet executed" |
| P10 | The syscall-family crates exist and are host-tested: `-abi` (numbers), `-linux` (wire), `-sync` (futex decode/key/table/deadline/WAKE_OP/waitloop), `-mem` (mmap/mremap/madvise/membarrier plan+decode), `-poll`, `-time`, `-eventfd`, `-ipc` | HOLDS | `ls crates/akuma-syscalls-*`; CLAUDE.md:89–116; AKUMA_EXTRACT_MMAP.md §10 |
| P11 | The futex *decisions* live in `akuma-syscalls-sync`; `amd64/src/futex.rs` (502 L) is effects only — user-word reads, the park, the wake | HOLDS | futex.rs:8–11 module header; sync/src/{op,key,deadline,wakeop,waitloop,table}.rs |
| P12 | The glue is comprehensive (`fs,mem,signal,sync,proc,net,unixsock,poll,eventfd,…`) and every arm looks the process up (`owner_pid`, `akuma_exec::threading::…`) — glue rows are AFTER-PM | HOLDS | glue/src file list; mem.rs sys_mprotect:1359; signal.rs sys_sigaltstack:208 (uses `akuma_exec::threading::get/set_sigaltstack`) |
| P13 | Writable `MAP_SHARED` file mappings + writeback already exist as glue decisions: `writeback_shared_pages` (mem.rs:210), the page-table dirty walk `writeback_present_pages` (:251), `sys_msync` (:323); `MmapRegion`+`FileBacking` live in `akuma-mmap/src/region.rs:203` and `akuma-exec` process state | HOLDS | read this session |
| P14 | The spawn handshake exists in glue: `sys_socketpair` (lib.rs:775) + `unixsock.rs` with SCM_RIGHTS (`scm_take/install/release` :339–383) | HOLDS | read this session |
| P15 | Threads are a crate call: `akuma_exec::process::clone_thread(stack, tls, parent_tid_ptr, child_tid_ptr, flags)` (mod.rs:3546); amd64's wiring example is `amd64/src/thread.rs:296 sys_clone_thread`; the clear_child_tid futex wake at thread exit is thread.rs:469 | HOLDS | read this session |
| P16 | The legacy x86_64 → `*at` fold lives only in amd64/src/usermode.rs:1503–1530 (`to_glue(Syscall::Mkdirat, [AT_FDCWD,…])`), guarded by the X86_ONLY disjointness smoke test (:7506–7530, the `ln -s`→utimensat bug) | HOLDS | read this session |
| P17 | `sched_getaffinity` is amd64-only (usermode.rs:2745); `set_robust_list` is glue proc.rs:313; `prlimit64` is glue proc.rs:1400; `getpriority`/`setpriority`/`getrusage`/`rseq` exist nowhere | HOLDS | grep this session |
| P18 | `rustc` needs ≥2 GB RAM for `hello.rs` (archive); amd64 got demand-paged file mappings + `akuma-fpcache` shared frames to stop the 311 MB eager mmap (RUST_TOOLCHAIN_AMD64.md session 5) | HOLDS | AKUMA_SELF_HOSTING.md:125,206; RUST_TOOLCHAIN_AMD64.md:604–640 |
| P19 | cargo's number batch (x86_64): 18 `pwrite64`, 328 `pwritev2`, 74/75 `fsync`/`fdatasync`, 265 `linkat`, 204 `sched_getaffinity` (rustc sizes its codegen pool from it), 157 `prctl`, 95 `umask`, 324 `membarrier` | HOLDS | RUST_TOOLCHAIN_AMD64.md:480–560 |
| P20 | Taoxichuan's QEMU rehearsal runs `-m ${MEM:-3072}` (3 GiB) | HOLDS | scripts/kvm/akuma_rehearse.sh:61 |

Premises checked against the tree this session unless noted; UNCHECKED rows appear
inline in §2 where a detail (not the row itself) was not read.

## 1. How the pieces are composed today (the crate map the routes cite)

Three layers, leaf-first (CLAUDE.md:89–116): **`akuma-syscalls-linux`** — numbers, wire
structs, flag tables; **`akuma-syscalls-abi`** — the two-architecture number vocabulary
with the disjointness rule (a row is a claim both ABIs were checked); **one crate per
family** — `-time`, `-sync` (futex), `-poll` (epoll/readiness), `-mem` (mmap/mremap/
madvise/membarrier decisions), plus `akuma-mmap` (the region algebra + `FileBacking`).
`akuma-syscalls-glue` sits above them and owns the *effects-bearing* arms
(`sys_mprotect`, `sys_mmap`, `sys_msync`, `sys_sigaltstack`, `sys_tkill`, `sys_socketpair`,
`sys_close_range`, `sys_set_robust_list`, `sys_prlimit64`, …) — which is why every glue
row needs the process model it looks up (`owner_pid`, `akuma_exec::threading` slots).
amd64 composes exactly this way: `usermode.rs` translates legacy x86_64 numbers into
`Syscall::*` and calls glue; `mm.rs`/`futex.rs`/`thread.rs`/`signal.rs` are the
kernel-side effects (page tables, parks, frames). Taoxichuan today pulls only
`akuma-syscalls-linux` (dirent/uts), `akuma-fdtable`, `akuma-pipes`, `akuma-vfs`/`-ext2`,
`akuma-net`, and — since M4 slice 3 — `akuma-exec`/`akuma-threading` for the real fork
and the slot pool. Its ~90 local arms in `taoxichuan/src/main.rs` are the thing stages
9–11 replace family by family.

**Where the glue-as-is route fits (the user's question).** After pm's S1–S3 (init as a
registered `Process`, sshd on the Linux ABI, clone → `fork_process`), Taoxichuan *has*
what glue requires: one `Process` per task in `akuma-exec`'s table, tid↔pid map, and the
threading-slot park/wake. The amd64 shape — a folded arm in the dispatcher handing glue
`to_aarch64()`-style numbers — then fits for every family where the remaining work is
*effects taoxichuan already has* (fds, ext2, sockets, console). It does **not** fit,
row by row, where the effect behind the arm does not exist yet: `sys_mprotect`/`sys_mmap
MAP_SHARED`/`sys_msync` need real PTE-flag and dirty-walk effects (no page-fault handler,
PTE walk is the kernel's own), and the signal-delivery arms need a frame builder. Those
rows need the extraction/local-effects route first; the fold to glue happens when the
effect exists. Doing it in that order avoids writing a local arm twice.

## 2. The gap table

Columns: syscall/behaviour · needed by · implemented today in (crate or amd64 file) ·
Taoxichuan today · gap and route (A/B/C/D) · pm-dependency · cost in executor tool
calls (rough; a call ≈ one tool round-trip incl. build/boot loops).

### 2a. Before the first `clone` — rustc start (all INDEPENDENT of pm)

| syscall / behaviour | needed by | today (crate / amd64 file) | Taoxichuan today | gap → route | pm? | cost |
|---|---|---|---|---|---|---|
| ELF load of 312 MB `librustc_driver` + mmap slab capacity | rustc start | `akuma-exec` `MmapRegion` model (amd64); taoxichuan bump slabs | fixed 64 MiB mmap slab, ENOMEM | split `SLAB` (main.rs:642) into `MMAP_SLAB` (≥1 GiB, measured) + `BRK_SLAB` (≥512 MiB); register regions → D (local consts, but the *long-term* shape is `akuma-exec`'s per-process space, S7) | no | 3 |
| `brk` | rustc start (musl malloc) | same slab const | grows by mapping inside 64 MiB | same const split; Linux old-break contract already honoured | no | (with row 1) |
| `mprotect` real (incl. PROT_EXEC) | rustc start (5 calls), tcc -run, JIT guards | glue mem.rs `sys_mprotect`:1359 (decision); amd64 mm.rs effects | accepted no-op | local effects arm: flip PTE bits in the task's own root via `akuma-boot-amd64` paging types (already imported, main.rs:103) → **B** (decision already in glue; extract the pure prot/region-split part to `akuma-syscalls-mem` only if glue's is entangled with `owner_pid` — read at extraction time) | no (local arm; fold to glue at pm S6) | 8 |
| `sigaltstack` | rustc start (musl pthread prep) | glue signal.rs:208 (decode) + `akuma_exec::threading::get/set_sigaltstack` (state) | ENOSYS | **A/B**: call glue's decode with the slot's stored triple; the state setter already takes a slot number taoxichuan has | no (slot exists; pid lookup at S6) | 3 |
| `membarrier` | rustc start (1 call; cargo batch 324) | `akuma-syscalls-mem::membarrier` (decode, `SUPPORTED=0x18`) | ENOSYS | **A**: decode from the crate; effect is `mfence`+`sfence` on amd64 (the crate's Barrier effect is aarch64 asm, deliberately kernel-side) | no | 2 |
| `readlink` legacy (89) + the legacy fs set 82/83/84/87/88/90/92/94 (git clone) | rustc start (89), ldso/git | amd64/src/usermode.rs:1503–1530 fold table only | ENOSYS | **C**: extract the fold into `akuma-syscalls-abi` (new `legacy` module: `(x86_64 nr) -> (Syscall, folded AT_FDCWD args)`, `forbid(unsafe_code)`), host test pins the `ln -s`(88)≠utimensat bug and the X86_ONLY disjointness; taoxichuan dispatcher folds into its existing local `*at` arms | no | 6 |
| `madvise` | rustc (thread-stack hints) | `akuma-syscalls-mem::madvise` (decode; the `dontneed_zero_range` divergence is documented, unpinned) | no arm (ENOSYS) | **A**: decode from the crate; effect = no-op for most advice, zero-range for DONTNEED if the slab walks regions | no | 3 |
| `ioctl` ENOTTY, probing ENOENTs | rustc start | local arm | ENOTTY (correct for a serial console) | none — benign, recorded | no | 0 |
| long argv/env + `E2BIG` | every spawn (build scripts pass ~5 KB args) | amd64 fixed defect 1 (argv capped 16, silently — RUST_TOOLCHAIN_AMD64.md:170); the caps+errno decision has no crate | child.rs 16×256 B caps, silent cut | **D**: new `akuma-syscalls-linux`-adjacent pure decision (count/len caps → `E2BIG`) inside the spawn path; raise taoxichuan caps to amd64's (128 KB argv strings) | no | 4 |
| env pass-through (`LD_LIBRARY_PATH`, `$HOME`, `$PATH`) | rustc start (musl `$ORIGIN` needs it: no `/proc/self/exe`), cargo (`$HOME`) | akuma-exec image env; amd64 spawns with explicit env | init has an empty env; `initargs=` csv only | probe P7 decides whether initargs can carry env or the kernel must seed it → D small | no | 2 |

### 2b. The thread wall — codegen threads (all AFTER-PM; pm S1–S3 are the gate)

| syscall / behaviour | needed by | today | Taoxichuan today | gap → route | pm? | cost |
|---|---|---|---|---|---|---|
| thread `clone` (CLONE_VM\|CLONE_THREAD\|…, child stack, TLS, set_tid params) | rustc's rayon codegen workers; musl pthread_create (the ctrlc handler is the *first* one) | `akuma_exec::process::clone_thread` (mod.rs:3546); amd64 wiring `amd64/src/thread.rs:296`; clear_child_tid wake thread.rs:469 | M2 blocking arm, EINVAL on thread flags | **A**: replace the arm with `clone_thread`; effects = fresh syscall band + slot (exists since M4 s3), CR3 shared by construction (CLONE_VM must skip the copy — thread.rs:31–35 is the why) | **yes (S1: init must be a registered Process; S3: clone→fork_process)** | 40 |
| futex — WAIT/WAKE, `FUTEX_WAIT_BITSET` absolute deadlines, `WAKE_OP`, REQUEUE, private vs shared | how every parked thread waits | `akuma-syscalls-sync` (op/key/table/deadline/wakeop/waitloop — host-tested); effects amd64/src/futex.rs | no arm (ENOSYS) | **A**: effects shim in taoxichuan calling the crate — read/write the user word via `copy_from_user`/`copy_to_user`, park = slot WAITING with `prepare_block`-before-membership-test (futex.rs:17–34 is the ordering argument), wake = crate `wake` + threading waker. Do **not** reimplement the table — every archived futex bug was a table/deadline bug the crate now pins | **yes** (parks + pids) | 30 |
| thread groups: tgid vs tid, `exit_group` reaping workers *before* notifying the parent | rayon leader exit; the proc-macro2 wedge (AKUMA_SELF_HOSTING.md §7h) | `akuma-exec` table/children/lifecycle; `futex_wake` hook exec_runtime.rs:474 | one task; exit_group = exit | **A**: comes with clone_thread; the ordering rule is the crate's; gate = a `-j2` compile finishing | **yes** | 12 |
| `set_robust_list` (+ get) | glibc/musl robust futexes (rustc std) | glue proc.rs:313 | ENOSYS | **B→A**: glue's record; the effect is the thread-exit futex wake (row above) | **yes** | 3 |
| `rseq` | libstd probes it | nowhere | ENOSYS | **D**: honest ENOSYS is tolerated by musl guests (amd64 has no arm either); only add if a probe shows a caller dereferencing the answer → probe P1 reads the trace | **yes** | 1 (as ENOSYS) |
| `getpriority`/`setpriority` | rustc threadpool calls opportunistically; **an ENOSYS return was used as a pointer and SIGSEGV'd the build** (AKUMA_SELF_HOSTING.md §7j) | nowhere | ENOSYS | **D**: new pure decision + arm in glue `proc.rs` shape (which=PRIO_PROCESS, who=0 → a fixed nice value); the *return-value* convention (0, not −1-with-errno-0) is what the host test pins | **yes** (pid) | 4 |
| `sched_getaffinity` | rustc sizes its codegen thread pool from it (ENOSYS ⇒ silent −j1) | amd64/src/usermode.rs:2745 only | ENOSYS | **C**: extract the mask decision (single-cpu mask, len clamp) into glue `proc.rs` (or a new `akuma-syscalls-sched` if more of the family arrives: rseq, sched_*); host test pins the clamp errno | no (serves 0/1 mask without a process table — but glue needs one) | 4 |
| `prlimit64` | rustc/cargo stack/limit probes | glue proc.rs:1400 | ENOSYS | **A/B**: pull glue's arm | **yes** | 2 |
| `getrusage` | wait4 rusage, cargo timing | nowhere (taoxichuan wait4 ignores rusage) | ENOSYS | **D**: zeros-payload decision; low priority | **yes** | 3 |
| `tkill`/`tgkill` + dispositions + delivery + `rt_sigreturn` | musl pthread cancellation, cargo's ~10,400 `tkill(SIGUSR1)` storm, ctrlc | glue signal.rs sys_tkill:334 / sys_tgkill:407 / rt_sigaction:10 / rt_sigprocmask:88; frame: amd64/src/signal.rs (x86), `akuma-threading::sigframe.rs` (aarch64 shape only) | no-op sigaction/procmask; no delivery | **B** for the disposition/setdecode arms (glue, after S6) + **C** for the x86 frame: extract amd64/src/signal.rs's frame build/`rt_sigreturn` sanitise into `akuma-threading` as a second repr(C) module beside the aarch64 one (the offset_of!/const-assert pattern, budget 0 unsafe) | **yes** | 80 |

### 2c. The linker wall — `rustc hello.rs`, then cargo (AFTER-PM unless marked)

| syscall / behaviour | needed by | today | Taoxichuan today | gap → route | pm? | cost |
|---|---|---|---|---|---|---|
| fork/exec of the linker (posix_spawn) | rustc → cc/lld | `fork_process` (landed) + execve (served) | works for fork; init can't fork | **A** — blocked only by pm S1 (init as Process) | **yes (S1)** | (pm's) |
| `socketpair` + `sendmsg`/`recvmsg` (AF_UNIX, CLOEXEC, SCM_RIGHTS) | libstd's spawn handshake (RUST_TOOLCHAIN.md §1/§4d) | glue lib.rs:775 + unixsock.rs (scm_* :339–383) | no AF_UNIX at all | **A/B**: glue unixsock + a taoxichuan `UnixSock` fd kind routed in the kind table (the eventfd lesson: an id nothing can read is useless — RUST_TOOLCHAIN_AMD64.md:264–296) | **yes** | 30 |
| `close_range` | libstd closes fds >2 around spawn | glue fs.rs:2317 | ENOSYS | **A/B**: pull; akuma-fdtable owns the iteration | no (needs the fd table only) | 3 |
| `pwrite64` (18), `pwritev2` (328), `fsync`/`fdatasync` (74/75) | cargo's SQLite cache (`SQLITE_IOERR_WRITE` 778 → 1034 named these) | akuma-ext2 write paths; amd64 added the rows | pread64 served (17); no pwrite/fsync arms | **D (local arms over akuma-ext2)**: `pwrite64` = `write_at_by_inode`; fsync/fdatasync = recorded no-op success (no journal); **read the extended errno first** when cargo complains (RUST_TOOLCHAIN_AMD64.md:546) | no | 8 |
| `linkat`/`link` (265/86) | cargo hardlinks artifacts into `target/` | glue fs.rs has the arm; akuma-ext2 trait has no hardlink op | ENOSYS ("no hardlink arm on the ext2 trait yet") | **D**: new `link` op on akuma-ext2 (directory-entry add pointing at an existing inode; pinned test: hardlink of a dir is EPERM) | no | 12 |
| real `chdir`/`getcwd` | cargo resolves config/manifests relative to CWD | `akuma-exec` `set_cwd` (mod.rs:1192) is the state; amd64 fs arms resolve relative to it | getcwd always "/", AT_FDCWD resolves at "/" | **A** (state) + local effect (path walk takes the cwd prefix; the taoxichuan cwd lives with the task, one task ⇒ trivial) | **yes** (per-process cwd) | 8 |
| writable `MAP_SHARED` file mapping + writeback on `munmap`/`msync`/exit | rust-lld's `FileOutputBuffer` — without writeback the output is **0 bytes with cargo reporting success** (AKUMA_SELF_HOSTING.md §7d) | glue mem.rs: writeback_shared_pages:210, dirty page-table walk :251, sys_msync:323; region algebra akuma-mmap region.rs:203 (`FileBacking`) | mmap is anon+MAP_PRIVATE fill; munmap no-op | **B/A**: the decisions are done — taoxichuan needs the *effects*: map the file's pages writable in the task's root, and the writeback walk = its own PTE dirty bits (x86 sets them; eager mappings mean every mapped page is present) → `akuma-ext2 write_at` per dirty page, then `renameat` (served) into place | **yes** (glue looks up the process) | 60 |
| `msync` | the POSIX flush point lld uses | glue mem.rs:323 | ENOSYS | lands with the row above | **yes** | (incl.) |
| eager fork copies rustc's ~2 GB | every linker spawn | akuma-exec fork; amd64 got lazy file mappings + `akuma-fpcache` shared frames (RUST_TOOLCHAIN_AMD64.md session 5) | eager whole-space copy, no fault handler | probe P4 measures it; mitigation ladder: (1) accept slowness for the gate, (2) shared frames for file-backed pages via akuma-fpcache (a crate — route A), (3) demand paging = stage-12 work | **yes** | 0 now / big later |

### 2d. Numbers and census facts worth pinning

- amd64's observed-missing list for one rustc+git run (RUST_TOOLCHAIN_AMD64.md:242):
  77 `ftruncate` (load-bearing — served here), 290 `eventfd2` (**served here** —
  row 290), 95 `umask` (served), 40 `sendfile` (honest ENOSYS; do not invent),
  324 `membarrier`, 204 `sched_getaffinity`, 157 `prctl`, 260 `fchownat` (served).
  **Do not add rows speculatively** — the abi rule is that a row is a claim both
  architectures' numbers were checked.
- The census's benign tail (`ioctl` ENOTTY, ENOENT probes, `stat`(4)/`poll`(7)/
  `open`(2) legacy) needs the fold table or tolerant arms, not new subsystems.

## 3. The order (smallest steps first; each gate is a console line or an exit code)

| step | what | gate (supervisor-runnable, no box needed) | calls |
|---|---|---|---|
| S1 | Split `SLAB`: `MMAP_SLAB` = min(1 GiB, RAM/2), `BRK_SLAB` = min(512 MiB, RAM/4) (main.rs:642,3591,1976–1982,2891) | `rustc --version` as init with `LD_LIBRARY_PATH` set exits **0** printing `rustc 1.100.0-nightly …` (already measured at a 1 GiB slab — this makes it the tree's answer, not a local hack) | 4 |
| S2 | The independent ENOSYS tail, one arm per commit: `membarrier` (crate decode + `mfence`), `sigaltstack` (glue decode + slot state), `madvise` (crate decode, no-op effect), legacy fold table (extract `akuma-syscalls-abi::legacy`, fold 89/82/83/84/87/88/90/92/94 into the local `*at` arms), real `mprotect` (PTE flip in the task's own root) | after each: the `rustc --version` `[err]` trace loses exactly one line; final: `tcc -run hello.c` **prints** (mprotect gate); `git clone` from init stops listing legacy ENOSYSs and fails only at fork | 14 |
| S3 | pm's S1 (init = registered Process, pid floor) — prerequisite, not this plan's code | `fork` from init returns a pid (the git-clone fork refusal clears); pm's own battery | (pm) |
| S4 | clone-thread → `akuma_exec::process::clone_thread`; futex effects shim over `akuma-syscalls-sync`; `set_robust_list` + clear_child_tid wake; `exit_group` ordering comes with the crate | (a) guest C probe: `pthread_create` + `pthread_join` + `futex` wake built with tcc prints `ok` (P3); (b) `rustc --version` stops dying at the ctrlc thread; (c) **`rustc hello.rs -o hello && ./hello` prints `hello`** (needs S5 if the handshake is ENOSYS — try first without it, probe P2 says which) | 70 |
| S5 | AF_UNIX spawn handshake: glue `unixsock` + fd kind + `close_range` | `rustc hello.rs -o hello` succeeds with the standard spawn path (trace shows `socketpair`/`sendmsg`/`recvmsg` served); `./hello` prints | 34 |
| S6 | cargo's batch: `pwrite64`/`pwritev2`/`fsync`/`fdatasync` (ext2), `linkat` (new ext2 op), `chdir`/`getcwd`, long argv + `E2BIG`, `sched_getaffinity` (extract), `getpriority`/`setpriority` (new), `prlimit64`/`getrusage` | `cargo build` of a one-file crate gets past the SQLite warnings; every `[err]` line in the trace is identified before the next row is added (the amd64 discipline) | 40 |
| S7 | Writable `MAP_SHARED` + `msync` writeback (glue + dirty-PTE walk) | **`cargo build` exit 0 and `./target/debug/hello` is non-empty and prints** — the zero-binary archive failure is the gate that must not pass silently | 60 |
| S8 | Signals: glue dispositions + x86 sigframe extraction into `akuma-threading` + tkill/tgkill delivery | a cargo build completes without a wedged worker; the `[err]` trace shows `tkill` served ~10.4k times (P6) | 80 |
| S9 | Fixed point (stage 12): kernel builds in guest | out of scope here — needs the memory model (demand paging or much more RAM) | — |

**Cut line.** If only half the budget lands: S1–S4 (+S5 if S4's hello gate needs the
handshake). That is the thesis — a real `rustc` starting threads and parking them —
and every row in S6+ is additive on top.

## 4. Probes for the supervisor (each: exact command, expected output, what it decides)

The supervisor's loop: guest = ext2 root (Alpine minirootfs + musl-dev + libgcc +
nightly-x86_64-unknown-linux-musl `1.100.0-nightly (420ed2a0c)`), `init=<path>
initargs=<csv>`, the kernel prints one `[err]` line per failing syscall. No boots by
me; all of these run on the Mac.

| # | probe | expected / decision |
|---|---|---|
| P1 | **The `rustc hello.rs` census.** Boot `init=<rustc shim> initargs=hello.rs,-o,hello,out` (env per P7), capture the whole console. Count distinct syscalls and every `[err]` line up to the first hard stop. | Decides which of §2b/§2c rows fire before the thread wall and in what order (the census in the brief is `--version` only). Compare against §2d's amd64 list: any new number is checked against `akuma-syscalls-abi` before an arm exists. |
| P2 | **Separate codegen from link.** Same boot, `initargs=--emit=metadata,hello.rs` (no linker). | If it succeeds at the thread wall (i.e. fails only on `clone`), the post-S4 gate `rustc --emit=metadata hello.rs` proves threads/futex without the S5 handshake; if it fails earlier, the census names what. Decides whether S5 gates S4's hello. |
| P3 | **A pthread+futex program, tcc-built in guest.** Once S4 lands: `tcc -run` (or compile-then-run) a C file doing `pthread_create` ×2, a `pthread_mutex` lock/unlock (⇒ futex WAIT/WAKE private), `pthread_join`, printing `ok` per step. | The futex/clone wiring's first gate — fails before any rustc run is wasted. A lost wake hangs (timeout needed: alarm(30) + `abort`). Expected: `ok`×4, exit 0. |
| P4 | **Memory of the compile.** At S1's slab sizes, boot with `-m 3072` and `-m 6144`; run the P1 census; record the peak from the kernel's region high-water prints (add a one-line print if absent — it is a diagnostic, not a kernel change) and whether `[OOM]` fires. | Decides whether `hello.rs` fits 3 GiB (archive: rustc wants ≥2 GB) and how much headroom the eager linker fork needs. Decides the RAM recommendation for the gates and whether the fork-copy row hurts wall-clock enough to matter for the gate. |
| P5 | **The linker's output path.** After S6 but *before* S7: `rustc hello.rs -o hello; ls -la hello; ./hello`. Also `rust-lld --version` directly if present. | If the binary is **0 bytes while cargo/rustc reports success** — the archive's recorded failure — S7 is confirmed as the blocker and cannot be cut. If lld falls back to `write()` when MAP_SHARED fails, S7 demotes below S8. |
| P7 | **What initargs can carry.** Boot `init=/bin/env initargs=A=1,B=2` (busybox env). | Prints whether initargs arrive as env, argv, or nothing — decides how `LD_LIBRARY_PATH`/`$HOME`/`$PATH` get seeded (§2a last row) and whether the shim needs a wrapper script. |
| P6 | **The signal storm count.** During the first full `cargo build` (post-S7): count `tkill`/`tgkill` lines in the trace. | The archive says ~10,400 `tkill(SIGUSR1)`; the real number sizes S8 (delivery) vs honest-ENOSYS. If the build completes with ENOSYS tkill, S8 demotes to "musl cancellation correctness" rather than a cargo blocker. |

(Order P7 → P1 → P2 → P4 first: none need kernel changes. P3 waits on S4; P5/P6 wait
on S6.)

## 5. Risks and unknowns (cheapest resolution each)

| risk | why it bites | cheapest resolution |
|---|---|---|
| **Memory** — rustc ≥2 GB (archive), 3 GiB QEMU rehearsal, eager fork doubles peak at the linker spawn | `[OOM]` = halt; no pressure ladder | P4 measures it. Mitigations in order: bigger `-m` (TCG tolerates 6 GiB), `--emit=metadata` split gates, `akuma-fpcache` shared file frames (route A, a crate), demand paging only at stage 12. Do **not** start the mmap-model rewrite for this. |
| **The zero-byte linker output** (§7d archive) — cargo reports success | the fixed-point gate lies | P5 names it before S7 is cut. The writeback machinery exists in glue — this is effects work, not design work. |
| **Signal storm** ~10.4k tkill; musl pthread cancellation needs RT signal delivery | a wedged worker looks like a slow build | P6 counts. Honest ENOSYS carried amd64's early builds; delivery lands at S8 with the x86 frame extraction. `PTHREAD_KILL_EINTR_DELIVERY_STARVATION.md` and `GIT_CLONE_STALE_ITIMER_SIGALRM.md` are the two archive incidents to re-read before S8 (stale-hazard check: they describe the *old* scheduler's starvation, not the slot pool's). |
| **Stale archive hazards on futex/threads** | the archive's futex/thread docs (FUTEX_REQUEUE_LOST_WAKEUP, THREAD_*, §7g missed wakeup) predate `akuma-syscalls-sync` | grep the archive before writing any futex code; the crate's host tests supersede the war stories. The §7g missed-wakeup class is closed by `prepare_block`-before-membership-test (futex.rs:17–34) — preserve that ordering in the taoxichuan shim verbatim. |
| **Double work: local arms vs glue fold** (pm S6) | S2's local mprotect/madvise arms could be rewritten at S6 | written as thin arms over the crate decisions from day one, so the S6 fold is a dispatch-table change, not a rewrite. Each local arm's comment names its glue successor. |
| **argv/env silent truncation** (amd64 defect 1 shape) | build scripts pass ~5 KB args; a silent cut is a wrong-answer-not-error | E2BIG first (fail loud), then raise caps; probe P1 verifies the real arg lengths. |
| **`getpriority` ENOSYS-as-pointer crash** (archive §7j) | the build unit SIGSEGVs | the D row returns a real value, and its host test pins the return convention. Cheap; do it with S6, not after a crash. |
| **PK2 + rare boot stall still open** (gaps G1/G2) | more slots + more waits stress the open items | sequence the gates so a stall is attributable: S4 lands after pm's own PK gates; if the rare stall reproduces, stop and bisect there, not in futex code. |
| **G3: no pump while a spawned child runs** | a long rustc compile may starve TCP (stage-9 cargo fetch) | out of this plan's path (fetch is stage 9, this is 10–11); noted for the network session. |

## 6. Cost estimate

| step | calls | cumulative |
|---|---|---|
| S1 slab | 4 | 4 |
| S2 independent tail | 14 | 18 |
| S3 pm S1 | (pm's ~30) | 48 |
| S4 threads+futex | 70 | 118 |
| S5 AF_UNIX handshake | 34 | 152 |
| S6 cargo fs batch | 40 | 192 |
| S7 MAP_SHARED writeback | 60 | 252 |
| S8 signals | 80 | 332 |

≈330 executor calls ≈ 3–4 sessions. **Cut at 165 calls:** S1–S5 (rustc hello runs;
cargo waits). **Cut at 60:** S1–S2 + pm S1 (rustc --version clean, fork from init
works) — the smallest state that unblocks every later row.

## 7. Docs read this session

`taoxichuan/docs/README.md`; `reference/syscalls.md` (whole table);
`reference/memory.md`; `reference/processes.md`; `reference/switch-to-crates.md`;
`reference/rust-toolchain.md` (the doc corrected in this commit);
`taoxichuan/docs/gaps.md` (head); `CLAUDE.md` syscall-family paragraph (:89–116);
`docs/archive/AKUMA_EXTRACT_MMAP.md` (§1, §7); `docs/archive/RUST_TOOLCHAIN_AMD64.md`
(:242–640: the missing-numbers tables, cargo's batch, session 5 lazy/shared frames);
`docs/archive/AKUMA_SELF_HOSTING.md` (§1.4 RAM, §7 section map); the family-crate
sources `akuma-syscalls-sync/src` (module list), `akuma-syscalls-mem/src` (incl.
membarrier.rs header), `akuma-syscalls-glue/src/{mem,signal,proc,fs,unixsock,lib}.rs`
(arm lines cited in §2), `akuma-mmap/src/region.rs` (FileBacking:203),
`akuma-exec/src/process/mod.rs` (fork_process:3194, clone_thread:3546, set_cwd:1192,
mmap_regions:797), `amd64/src/{futex,thread,signal,usermode,mm}.rs` (the wiring
examples cited in §2), `scripts/kvm/akuma_rehearse.sh:61` (RAM).

### Questions for the user the docs cannot answer

1. **Slab now vs process memory model first?** S1 grows the constants (an hour of
   work, honest about being a stop). The alternative — pulling `akuma-exec`'s
   `MmapRegion`/per-process space into taoxichuan now — kills the slab row
   permanently but couples stage 10 to a memory-model rewrite mid-ladder. I planned
   constants now, model at stage 12; say if you want it the other way.
2. **Is `cargo build`'s gate allowed to use a vendored/offline crate?** Stage 9
   (`cargo fetch`) needs the network (eventfd2 is served; HTTPS + G3 pump is the
   network session's problem). If the stage-10 gate must build *the fetched* tree,
   the order inside S6 changes (staging first). The amd64 path staged with
   `git archive | tar` + `cargo vendor` (RUST_TOOLCHAIN_AMD64.md:496).
3. **Signals before or after MAP_SHARED?** I put MAP_SHARED (S7) first because the
   linker gate is load-bearing and the archive says early builds survived honest
   ENOSYS signals; if you expect to run a pthread-heavy workload sooner, swap S7/S8.
4. **Does the weekly token budget cover pm's S1 landing first?** Every §2b row is
   AFTER-PM; if pm slips, the only deliverable is S1+S2, and this plan's cut lines
   should move up a level.
