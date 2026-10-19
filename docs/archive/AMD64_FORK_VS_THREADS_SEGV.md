# amd64: a multithreaded process that forks while another thread faults gets a spurious SIGSEGV

**Status: OPEN — reproduced 100 %, root cause not found.** Found 2026-10-19/20 on the ryzen metal
(kernel `7d4d22d6-release-smp-shared`, GRUB entry 20, `smp=8`) when `crush` died at startup.

## Symptom

`crush` (v0.98.1, a static Go binary) started in `/src/github.com/netoneko/akuma` — a git
repository — dies within seconds with a Go runtime crash:

```
SIGSEGV: segmentation violation
PC=0x498ed5 m=2 sigcode=2 addr=0x498e35
runtime.memmove()  runtime/memmove_amd64.s:456
runtime.copystack(...)  runtime/stack.go:985
runtime.newstack()  runtime/stack.go:1216
goroutine 1 [copystack]: ... internal/ui/styles.LoadTheme  themes.go:297
```

(variants: `fatal error: fault`, `unexpected signal during runtime execution`). Started in a
scratch directory (`/tmp/...`, not a git repo) the same binary never crashed (many runs, TUI and
headless). The difference is that in a repo crush builds its system prompt at startup by running
`git status` & friends through `os/exec`: the same dump shows four goroutines parked in `wait4`
(`prompt.getGitStatus → shell.Exec → os/exec`) while goroutine 1 grows its stack to ~1.5 MB.

Go's `os/exec` starts children with `clone(CLONE_VM|CLONE_VFORK|SIGCHLD)`, which this kernel serves
as a **copy-on-write `fork`** (`amd64/src/usermode.rs`, the `Syscall::Clone` arm →
`sys_spawn_clone` → `akuma_exec::process::fork_process`) and does not suspend the parent.

## Minimal repro — no crush, no git

`scripts/utils/go_fork_stack_repro/main.go` (2.8 MB static binary, cross-compiled on the Mac:
`GOOS=linux GOARCH=amd64 CGO_ENABLED=0 go build -o gofork .`; installed on the ryzen box as
`/bin/gofork`). N goroutines spawn `/bin/true` in a loop; the main goroutine repeatedly
makes fresh goroutines that must grow a ~1.5 MB stack. Run on the ryzen metal, 4 runs each, 200
rounds:

| `-spawners` | result |
|---|---|
| 0 | **4 / 4 pass** |
| 1 | **4 / 4 crash** |
| 4 | **4 / 4 crash** |
| 8 | **4 / 4 crash** |

Crashes look like `unexpected fault address 0x248800000 / fatal error: fault / SIGSEGV code=0x1
(MAPERR)` and, in one run, `sigcode=2 (ACCERR) addr=PC`. **One concurrent fork is enough.**

Not yet measured (the machine was in use): `smp=1` (GRUB entry 12/0), kernel versions older than
`7d4d22d6`, the Firecracker rig at 1/2/4 vCPUs. These three runs are the first thing to do — they
say whether this is "fork vs. other cores" or "fork vs. other threads on one core".

## What is established

1. The parent process is **multithreaded** (Go runtime: several OS threads, `CLONE_THREAD`
   siblings) and **forks while a sibling thread takes page faults** (fresh goroutine stacks are
   new anonymous mappings, demand-faulted, then written by `memmove`).
2. No kernel log line accompanies the crash (Go installs its own SIGSEGV handler); the kernel
   delivered a SIGSEGV with a `MAPERR` or `ACCERR` code to a page the thread had just mapped.
3. Single-threaded fork is fine: cargo/rustc/gcc spawn constantly and the self-host build passes.
   Multithreaded processes that don't fork are fine (`-spawners 0`).

## What is *not* established — hypotheses, in the order I would check them

Everything below is inference from reading, not from running anything.

- **H1 — CoW share pass vs. a concurrent demand fault in the same address space.**
  `Process::fork_from` demotes the parent's live PTEs to read-only (setting the amd64 CoW marker,
  PTE bit 9) and bumps frame refcounts, and the parent's *regions* are snapshotted for the child.
  A sibling thread that takes a not-present fault on a lazy region, or a write fault on a page
  being demoted, interleaves with that pass. Questions: is there any lock excluding
  `fork_from` from `install_filled_page` / the write-fault path on the same `space_root`? Is the
  marker set *before* the write bit is cleared (the order matters to `akuma-cow`'s decision
  `(pte_writable, marked, refs) → Retry/Fault/TakeInPlace/Copy`: a read-only page with no marker is
  `Fault` = SIGSEGV, which is exactly "ACCERR on a page I just mapped")? Is the shootdown issued
  before the lock that excludes the sibling is released?
- **H2 — frame accounting.** A page installed by the sibling *during* the pass is writable, in no
  ledger snapshot, or counted twice; when the short-lived child execs/exits, teardown drops a
  reference it never took and frees a frame the parent still maps → the parent's next access hits a
  cleared PTE (`MAPERR`). The `cowstale` race class `AKUMA_AMD64_COW.md` says "does not exist here
  yet — no threads" now exists.
- **H3 — the large-stack allocation path** (lazy-region extent bookkeeping for a ≥ 2 MB anonymous
  mapping; compare `docs/archive/` "CoW fork mmap region extent" and `AMD64_ANON_FAULT_DOUBLE_POPULATE`
  = SELFHOST_BUILD_SLOWNESS §14). Less likely, because `-spawners 0` passes with the same stack growth.

## The docs say CoW fork is SMP=1-only. That is stale, and worth knowing before reading the code

- `amd64/src/thread.rs` (module doc, "Why `CLONE_VM` must not…"): "since 2026-09-09 the target has
  a shootdown anyway (`shootdown.rs`), so CoW `fork` is no longer SMP=1 only either".
- `docs/archive/AKUMA_AMD64_COW.md` § Known limits still says "**SMP=1 only, by construction**" and
  "**No threads**, so none of the `cowstale` race class exists here yet"; the `Syscall::Clone`
  comment in `usermode.rs` and `docs/reference/subsystems/memory.md` repeat it.
- In practice CoW fork has run at `SMP>1` for weeks (self-host builds at `-j4`), so the SMP=1 text
  is wrong *and* the "no threads" text is also now wrong: threads exist (`crate::thread`).
- The corrections are dated in those three places (2026-10-20); they state what is known, not that
  the combination works.

## Related

`AMD64_TLB_STUCK_MMAP_REGIONS.md` (the `mmap_regions` shootdown deadlock, fixed `e672447e`),
`AKUMA_AMD64_COW.md`, `project_cow_fork_mmap_region_extent` (memory: `( cmd; cmd ) &` SIGSEGV from
lost mmap extents), `AMD64_CLAUDE_LOGIN_INPUT_STUCK.md` (Bun's startup spin of `vfork`s with a
multithreaded parent — **possibly the same bug**: Claude's `/login` freeze involves ~18 `vfork`s
from a multithreaded Bun while its main thread stalls).

## Next

`proposals/NEXT_AGENT_AMD64_FORK_VS_THREADS_SEGV.md` is the prompt for the fix.
