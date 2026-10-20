# Prompt: fix the amd64 fork-vs-threads spurious SIGSEGV

You are running **on Akuma itself** (the ryzen laptop, amd64, bare metal) in
`/src/github.com/netoneko/akuma`, through `crush`. One task: find and fix a kernel bug that makes
a **multithreaded process get a spurious SIGSEGV when it forks while a sibling thread takes page
faults**. It is reproduced 100 % of the time by a 2.8 MB program already installed on this box.

Read `docs/archive/AMD64_FORK_VS_THREADS_SEGV.md` first — it is the write-up, with the evidence
table, what is and is not established, and three ranked hypotheses. This prompt does not repeat it.
**Read its 2026-10-20 addendum too**: the bug now reproduces locally under QEMU on the Mac
(SMP=2, exact metal signature), `smp=1` passes, `pmm-forensics` is silent, and the hypothesis
ranking changed — the addendum's "What the next agent should do first" is the current method.

## The reproduction (run this before and after every change)

```sh
gofork -spawners 0 -rounds 200      # control: always prints OK
gofork -spawners 1 -rounds 200      # BUG: SIGSEGV / "fatal error: fault" every time today
gofork -spawners 4 -rounds 200      # BUG
gofork -spawners 8 -rounds 200      # BUG
```

`/bin/gofork` (source: `scripts/utils/go_fork_stack_repro/main.go`; there is no Go
toolchain on this box — if you want to change the program, ask the user to rebuild it on the Mac).
Success is **20 consecutive passes at each of spawners = 1, 4, 8** and the controls unchanged.
Loop it in `sh` (see `scripts/utils/go_fork_stack_repro/` and the doc); always `timeout 60` a run. Also useful afterwards:
`cd /src/github.com/netoneko/akuma && crush-bin` headless (see the doc) and a real `kbuild`.

## Facts to start from (verify them, don't trust them)

- Go's `os/exec` → `clone(CLONE_VM|CLONE_VFORK|SIGCHLD)`; this kernel serves it as a **copy-on-write
  fork**, parent not suspended: `amd64/src/usermode.rs` (`Syscall::Clone` arm, ~line 2140) →
  `sys_spawn_clone` → `akuma_exec::process::fork_process(child_pid, child_stack)` →
  `Process::fork_from` (`crates/akuma-exec/src/process/`). It demotes the parent's live PTEs to
  read-only, marks them CoW (amd64: PTE bit 9), bumps frame refcounts, snapshots the parent's
  regions for the child, and (since 2026-09-09) shoots down TLBs (`amd64/src/shootdown.rs`).
- The write-fault side is `amd64/src/mm.rs` (`populate_page`, `install_filled_page`, the CoW
  break) deciding through `crates/akuma-cow`: `(pte_writable, marked, refs)` →
  `Retry` / `Fault` / `TakeInPlace` / `Copy`. **`Fault` = the thread gets SIGSEGV.** The order of
  those tests is the substance (`akuma-cow`'s own docs).
- Threads: `amd64/src/thread.rs` (`CLONE_THREAD` siblings share the parent's `space_root`; nothing
  there demotes PTEs). A lazy anonymous region is demand-faulted page by page.
- `AKUMA_AMD64_COW.md` was written for "SMP=1, no threads" and still says so; `thread.rs` says
  the shootdown makes CoW fork work at SMP>1 and that is what runs in practice. **Both statements
  about threads in the COW doc are false now** — that is where the bug lives.
- A neighbouring, *fixed* bug of the same family: `install_filled_page` — two cores took the same
  not-present fault, the BKL serialised the servicing but not the trap, the second overwrote the
  first's frame (`docs/archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md` §14). Read it for the style
  of reasoning and of fix (presence test and map inside one `with_current_address_space` hold).

## Method

1. **Localize with the repro, not by reading alone.** The three runs the write-up lists as
   not yet done matter most, and the user must do the ones that need a reboot: `smp=1` (GRUB
   entry 12/0) and an older kernel. You cannot reboot this machine — ask for them, precisely, and
   meanwhile continue.
2. **Instrument, don't guess.** Add a bounded in-kernel trace (a fixed-size static ring, no
   allocation, printed through `safe_print!` on the `Fault` outcome only, and readable from
   `dmesg`): at the moment `akuma-cow` returns `Fault` for a user write, record `va`, the raw PTE,
   the region it falls in, the frame's refcount, the current core, and whether a `fork_from` for
   the same `space_root` is in flight (a counter or flag you add around the share pass). Gate it
   behind a cargo feature or a runtime toggle so it can stay in the tree. The first useful result
   is *which outcome fires and for what PTE state*.
3. Write a **kernel boot self-test** that reproduces without Go: a process with two threads, one
   faulting fresh anonymous pages in a loop while the other forks-and-reaps in a loop, a few
   thousand iterations. It must fail before your fix and pass after (the amd64 suite is
   `amd64/src/` self-tests registered in `src/` — find how `fork`/`thread` tests are written there
   and copy; the user runs the suite).
4. Fix the cause. Prefer the smallest change that makes the *invariant* true (e.g. the share pass
   and the fault path exclude each other per address space, or the marker is set before the write
   bit is cleared and under the same hold), and state the invariant in the code and in
   `docs/reference/subsystems/memory.md`. If the fix serialises, measure the cost (a fork of a
   large process, and a threaded process faulting while another forks) — do not make single-threaded
   `fork` slower to fix a threaded one.
5. Put what you learn into the crate that owns the decision when it is pure: if the bug is a missing
   outcome in `akuma-cow`'s table, add the case with a host test first.

## Ground rules

- **Never `git commit`, `git push`, `git reset`, `git rebase`.** The user drives all commits.
  **No subagents.** Do not reboot this machine, and do not touch `/boot` — you may build with
  `kbuild` (about 10 minutes, run it detached: `nohup sh -c 'kbuild -j 1 > /root/kb.log 2>&1' &`)
  and install **only** to the verify slot:
  `sh scripts/install_kernel_amd64.sh /root/ktarget/x86_64-unknown-none/release/akuma-amd64 /boot/akuma-amd64.verify`;
  then **stop and tell the user** to arm GRUB entry 26 from Pop (that boots the verify kernel with
  entry 20's command line) and to run `gofork` there. Never overwrite `/boot/akuma-amd64`.
- This box's traps: an ssh/crush session has **no `PATH`** (use full paths or `. /etc/akuma-dev.env`);
  closing a pty does not kill its children — `kill -9` strays by pid; never `pkill -f <pattern>`;
  use `-j1` for builds (`-j4` has corrupted builds here).
- Kernel conventions in `CLAUDE.md` apply: analyse every new path for allocations (the fault path
  must not allocate), console output only through `safe_print!`, a new crate's unsafe budget is 0,
  `amd64` never `x86`.
- Don't change behaviour you were not asked to; if you find a second bug, write it down in the
  archive doc and leave it.

## Done when

`gofork` passes 20/20 at spawners 1, 4 and 8 on the verify kernel (the user confirms on the
metal); the new self-test fails without the fix and passes with it; `cargo test` for any host
crate you touched, `cargo check -p akuma-amd64 --target x86_64-unknown-none` and clippy
`-D warnings` clean; the AArch64 build still compiles (ask the user — no `aarch64` target here)
and nothing AArch64-only changed unless you can say why; `docs/archive/AMD64_FORK_VS_THREADS_SEGV.md`
gets a "Root cause" and "Fix" section with what you measured; the stale "SMP=1 only / no threads"
text in `AKUMA_AMD64_COW.md` is rewritten (not just annotated) to the truth; a row in
`docs/README.md`'s matrix is updated from "Open" to the fixed status.

## Report back

Final message: the root cause in two sentences; the diff summary; the `gofork` numbers before and
after; the cost measurement; what you could not verify here and the exact commands the user must
run (arm entry 26, `gofork` loops, the amd64 boot suite — expect **804 passed, 0 failed** — and the
AArch64 build). If one of the hypotheses in the write-up was wrong, say so first.
