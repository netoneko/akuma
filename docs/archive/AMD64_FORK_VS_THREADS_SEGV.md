# amd64: a multithreaded process that forks while another thread faults gets a spurious SIGSEGV

**Status: OPEN — reproduced 100 %, root cause not found.** Found 2026-10-19/20 on the ryzen metal
(kernel `7d4d22d6-release-smp-shared`, GRUB entry 20, `smp=8`) when `crush` died at startup.
**Also reproduced locally under QEMU on the Mac at SMP=2** (2026-10-20 addendum at the bottom —
harness, traces, and re-ranked hypotheses there).

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

## Addendum — 2026-10-20 evening: reproduced OFF the metal, traced, and re-ranked (agent session on the Mac)

The ryzen box was unreachable all session (192.168.1.126 down, not pingable), so the hunt ran
locally. **The bug reproduces under QEMU on the Mac**, which changes the logistics of everything
below: no metal, no reboot needed, ~4 min per run.

### The local harness (this is how every claim below was made)

```sh
# repro binary — the Mac has a Go toolchain
GOOS=linux GOARCH=amd64 CGO_ENABLED=0 go build -o target/gofork/gofork \
    scripts/utils/go_fork_stack_repro/main.go
# disk: build the standard image and drop the binary in
sh amd64/mkdisk.sh target/gofork/root.img 128
export PATH="/opt/homebrew/opt/e2fsprogs/sbin:$PATH"   # debugfs is keg-only
debugfs -w -R "write target/gofork/gofork bin/gofork" target/gofork/root.img
# boot it as init, SMP=2; output (and the crash) lands in the log
DISK=$PWD/target/gofork/root.img SMP=2 INIT=/bin/gofork \
    INITARGS=-spawners,8,-rounds,200 SSH_PORT=2224 HTTP_PORT=8081 \
    overlays/devbox-amd64/run.sh > run.log 2>&1
```

TCG (no KVM on Apple Silicon) is fine — the crash is a race that 2 vCPUs hit readily. sshd into
the guest does **not** work (TCP connects, the handshake never completes); booting gofork as
`INIT` avoids that entirely.

### Results (kernel `4b7bdc59` + the instrumentation described below)

| config | result |
|---|---|
| SMP=1 | **passes** (`OK`) — the write-up's open `smp=1` question, answered locally |
| SMP=2, `-spawners 8` | **crashes — the exact metal signature**: `SIGSEGV code=0x1 addr=0x248800000 pc=0x4c7854` (`main.grow`) |
| SMP=8 | crashes; presentations vary (below) |
| SMP=2, children with a 100 ms lifetime (`/bin/busysleep`, a busy-loop Go binary) | **passed** — see "per-fork-cycle race" below |

Crash presentations seen (all are Go-runtime memory corruption, downstream):

1. `SIGSEGV code=0x1` MAPERR at the first page of a `PROT_NONE` arena reservation (the metal
   signature) — analysed in full below; it is **not** a region-list bug.
2. `fatal error: found bad pointer in Go heap` — a heap word holds `0x248490060` while its span
   says `limit=0x24836df80`; span metadata (persistentalloc memory) or a heap word is stale/garbage.
3. `#GP` (SI_KERNEL, addr=0) inside `mheap.allocNeedsZero ← initSpan` — a non-canonical address
   read out of corrupted span metadata. `pmm-forensics` was **silent** in every crash.
4. `newproc1: new g is not Gdead` — a `g`'s status word garbage.

The common denominator: **live parent memory contains garbage that no allocator or refcount
instrument explains** — `0xff…` words and wrong status fields, not zeros. A zero-fill race
(`populate_page` over a live page) would read as nils; this reads as a **lost update or a clobber
via a stale translation**.

### What the instrumentation found (why-codes, and the one full trace)

Landed in the tree this session (see "Tree changes" below): `amd64/src/mm.rs` now counts and
prints every demand fault the kernel **refuses** (`FAULT_REFUSALS` + `note_fault_refusal`, with
the refusing arm named), `cow_write_fault` prints the `akuma-cow` `Fault` outcome, and a
`REGION_TRACE` toggle prints every `MAP_FIXED` replace, region detach and region insert with the
editing pid.

The one run captured end-to-end (`target/gofork/pid1.log`, keep the file while it exists):

- Go grows its heap in 64 MB `sysReserve` chunks, then `sysMap`s it into use in 4 MB `MAP_FIXED`
  pieces: chunk 1 `[0x248000000,+4M)`, chunk 2 `[0x248400000,+4M)` — both traced, both split off
  the `PROT_NONE` reservation correctly (`detach … pieces=1`, RW insert, all `pid=1`).
- Then the write at **`0x248800000`** — chunk 3's first page — is refused `why=prot-none`, and the
  `[sig!-at]` line names the region: `[0x248800000, 56 MB) prot=Prot(0)` — the un-splitted
  reservation tail, created by the chunk-2 detach's split.
- **Go never issued a `MAP_FIXED` for chunk 3.** Every region surgery in the whole boot is traced
  and the list is *self-consistent*: nothing was lost, nothing overlapped, nothing was edited by a
  foreign pid.

So the prot-none SIGSEGV is **downstream**: a corrupted stack bound / `g` / span made Go write
into arena space it had never mapped. The kernel's region list, the PMM refcounts, the UAF
quarantine, the premature-free check and the resurrection detector are all clean *and all
mutually consistent* — which kills the easy forms of H2 and redirects the hunt.

### Re-ranked hypotheses

- **H2 (frame accounting) — weakened but not dead.** `pmm-forensics` (quarantine +
  premature-free + resurrection) ran through every crash silent, and the region/refcount story is
  coherent. Whatever corrupts memory either is a *lost write* (no free involved) or bypasses the
  ledger entirely.
- **H1 (fork walk vs concurrent fault) — the PTE-edit interleavings the write-up asked about are
  all closed by `as_lock`**: the amd64 walk (`share_parent_memory_into`) holds the leader's
  `address_space.lock()` for the whole walk, and every fault-side install (`install_filled_page`)
  and CoW break (`cow_write_fault`) takes the same lock. The demote's TLB shootdown broadcasts and
  waits for peer acks — zero `[TLB] stuck` lines in any crash run. What is *not* closed: the
  correctness of that coherence under a peer that holds a **writable** translation of a page the
  walk demoted — the write-through is silent (no fault, no counter) and lands in the frame the
  child now shares, which is exactly a "lost update" from the writer's own point of view… or from
  the reader's, depending on which core the Copy arm then hands a fresh frame to.
- **NEW — per-fork-cycle race with instant-exit children.** Children that live 100 ms (`busysleep`)
  passed while instant `/bin/true` children crash: the corruption rate scales with fork cycles, so
  the trigger is a per-fork window — child teardown (ledger drop, slot recycle, `THREAD_IDENTITY`
  invalidation) or the share pass itself, racing parent activity on the other core. `busysleep`
  simply runs ~100× fewer cycles in the same wall time.
- **H3 (large-stack allocation path) — dead.** `-spawners 0` with the same stack growth passes,
  and the corruption is not in the stack allocation path but in shared runtime structures.

### What the next agent should do first

1. **Keep the local harness.** Every experiment above is one rebuild + one 4-minute QEMU boot. Do
   not wait for the ryzen box; do run the final fix there (the doc's original "Done when" still
   applies, with `/bin/gofork` at 20/20).
2. **Decide lost-write vs clobber with one experiment.** Cheapest known: run the repro under
   `REGION_TRACE=true` plus a per-va *write* watch is not available; instead, add a Go-side canary
   (a goroutine that continuously verifies a few known heap words) or — kernel-side — log
   `cow_write_fault`'s Copy/TakeInPlace arms for VAs inside the gofork arena range with the frame
   and the refs, and look for two live mappings of one frame (a stale-RW peer writes the old
   frame after the Copy arm re-mapped the VA).
3. **Child-teardown focus**: instrument `Process::free`'s ledger drop (frames dec'd per exit) and
   the slot recycle path; then A/B the repro with `/bin/true` vs a child that lives ~1 ms.
4. The original prompt's method (trace on `Fault`, self-test, fix under the invariant, verify slot
   on metal) stands; only the *local* leg of it changed.
5. The CLAUDE `/login` freeze (~18 `vfork`s from multithreaded Bun) remains flagged as possibly
   the same bug — nothing new either way this session.

### Tree changes this session (instrumentation only; behaviour unchanged)

- `amd64/src/mm.rs`: `FAULT_REFUSALS` counter + `note_fault_refusal(va, why)` (prints first 8,
  then every 1024th — a healthy kernel prints nothing); refusal points named `no-region`,
  `prot-none`, `oom`, `install`, `populate`; `REGION_TRACE` toggle (default **off**) with
  `MAP_FIXED`/detach/insert lines carrying the editing pid; `usermode::current_mm_pid()`.
- `amd64/src/idt.rs`: `cow_write_fault`'s `CowAction::Fault` arm prints via
  `note_fault_refusal(addr, "cow-fault (ro, unmarked)")` — the bounded `Fault`-outcome trace the
  fix prompt asked for.
- `amd64/src/usermode.rs`: nothing behavioural; `current_mm_pid` added.
- Everything compiles clean (`cargo check -p akuma-amd64 --target x86_64-unknown-none`), boots,
  and passes the self-test suite (824 passed, 0 failed in this configuration).

