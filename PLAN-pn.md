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
