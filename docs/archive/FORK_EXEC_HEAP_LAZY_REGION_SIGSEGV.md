# `( cmd; more-cmds ) &` SIGSEGV — deterministic wild write in a forked-but-not-yet-exec'd child

Found 2026-07-30 on branch `another-smp-attempt-0` while chasing fresh BKL attribution data
for [BKL_VFS_CARVE_OUT.md](BKL_VFS_CARVE_OUT.md) §7. **Partially investigated, not fixed.**
One real, separate correctness bug was found and fixed along the way (a fork lazy-region
propagation gap — §"Propagation fix landed" below), but it does **not** explain this crash;
that fix was verified to leave the crash fully reproducible. The kernel bug itself is still
open, worked around only at the test-harness level
(`scripts/bkl_smp_regimen/payload/job.sh`).

## Symptom

Any real program exec'd from inside a **multi-statement backgrounded subshell** segfaults
immediately, deterministically:

```sh
( wget -q -O /tmp/out.bin http://10.0.2.2:8899/p32.bin; echo $? > /tmp/out.rc ) &
```

→ `Segmentation fault`, exit status 139 (SIGSEGV), every single time. No concurrency needed —
reproduces with exactly one backgrounded job, and reproduces identically at `SMP=1`.

## What does *not* trigger it

| pattern | result |
|---|---|
| `wget -q -O /tmp/out.bin $URL &` (no subshell, single command) | works |
| `( true; echo done > /tmp/x ) &` (subshell, but a trivial command) | works |
| `( wget -q -O /tmp/out.bin $URL; echo $? > /tmp/out.rc ) &` (subshell, real command) | **SIGSEGV** |
| 4 concurrent copies of the crashing pattern | SIGSEGV, same as 1 |
| the crashing pattern at `SMP=1` | SIGSEGV, same as `SMP=4` |

So the trigger is specifically: **a process forked but not yet exec'd** (ash's subshell,
needed because there's more than one statement in the parens) **then execs a real
program**. A process that reaches exec via a *direct* single-fork-then-exec (no
intermediate non-exec'd fork) is fine, and forking a program that never execs (`true`) is
also fine.

## Root cause (as far as traced)

The crash is a **write to an unmapped page** (`Data abort`, `ISS=0x46` → DFSC level-2
translation fault, `WnR=1`), not a permission/CoW fault — so it never touches the CoW
refcount machinery in `crates/akuma-exec/src/process/mod.rs` at all.

**It happens in the forked child before it ever reaches `execve`.** This was the biggest
correction versus the first pass at this investigation: the syscall trace right before the
fault (`set_tid_address` + `rt_sigprocmask`, i.e. what a freshly-exec'd musl `_start` does)
initially looked like post-exec libc init, but the kernel log around every crash instance
has **no `[syscall] execve(...)` trace line and no `[mmap]` trace line** for the crashing
pid at all — both are printed unconditionally by their respective syscall handlers, so their
absence means neither syscall was ever made. The crashing pid's own `fork_process` trace
(`[FORK-DBG] parent_pid=P child_pid=N ...`) is the last thing tied to it before
`[FORK-DBG] trampoline ENTRY tid=T` and then straight to `[DA-MISS]`/`[WILD-DA]`. Whatever
those two `set_tid_address`/`rt_sigprocmask` entries are, they are not evidence of a
completed exec (either stale data from a reused per-thread syscall-log slot, or musl's own
`fork()` wrapper doing its post-`clone()` signal-mask restore in the child — not
distinguished). So `replace_image()` is not implicated the way originally suspected; the
break is somewhere in `fork_process` (`crates/akuma-exec/src/process/mod.rs`) or in what the
child touches immediately after the fork trampoline returns it to userspace, still running
unmodified inherited code.

Disassembling the exact faulting instruction (`ELR=0x1004e8f0`, i.e. file offset `0x4e8f0`
in `bootstrap/bin/busybox`, a `static-pie` binary) with `objdump -d`:

```
adrp x0, 0x11f000       ; x0 = 0x1011f000 (this binary always loads at base 0x10000000)
ldr  x0, [x0, #0xf88]   ; x0 = *(0x1011ff88)              — PTR_A, a fixed global in .data
...
ldr  x0, [x0]           ; x0 = *(PTR_A)                    — PTR_B, observed = 0x20120030
str  wzr, [x0, #0x308]  ; *(PTR_B + 0x308) = 0   <-- faults; PTR_B+0x308 = FAR = 0x20120338
```

So the crash is a double-pointer-dereference write: a fixed global holds `PTR_A`, and
`*PTR_A` should be a valid pointer (`PTR_B`) but is instead `0x20120030` — a value that
lands suspiciously close to `next_mmap`'s initial computed value (`0x20120000`, see below),
though no `mmap()` syscall was ever made by this process to justify treating that address as
valid. **Hypotheses ruled out, each with direct evidence:**

- **Not the earlier-suspected heap-lazy-region/mmap-floor boundary in isolation.** True that
  `compute_heap_lazy_size` caps the heap region at exactly `next_mmap`'s initial value
  (`(code_end + 0x1000_0000) & !0xFFFF` = `0x20120000` for this binary), and the fault
  address sits `0x338` bytes past that boundary — but `[DA-MISS]` diagnostics show
  `parent_has_va=false`: **neither the parent nor the child has this VA registered as a lazy
  region**, so it isn't a "forgot to inherit a valid registration" gap either (see next
  section) — it's a value nobody ever validly reserved, in anyone's lineage.
- **Not a fork lazy-region propagation gap.** See "Propagation fix landed" below — a real,
  separate bug was found and fixed here, and confirmed *not* to touch this crash (the fix
  correctly propagates 6/6 parent regions to the child; the crash persists unchanged).
- **Not file-size-driven eager-vs-lazy `execve` loader selection.** `do_execve` picks
  `replace_image` (eager, whole file read) vs `replace_image_from_path` (on-demand) based on
  whether `ext2::read_inode_data` returns `FsError::Internal` for files over 16 MB
  (`crates/akuma-ext2/src/ext2.rs`); `/bin/busybox` is ~1.1 MB, nowhere near that threshold,
  and moot regardless since the crash precedes `execve` entirely.
- **Not an actual `mmap()` call gone wrong.** Confirmed no `[mmap] pid=<N> ...` trace line
  exists for any crashing pid before its crash, so `PTR_B`'s value cannot be a real-but-
  mis-registered `mmap()` return address.
- **Not the previously-fixed vfork/TTBR0 bug** (`project_vfork_stale_ttbr0`, fixed
  2026-07-05) — that one was in the vfork fast path (`vfork_process`); this reproduces via
  the full/CoW fork path (`fork_process`) and needs no vfork at all, and reproduces
  identically at `SMP=1` (ruling out any cross-core TLB-staleness variant of that class of
  bug too).
- **ASID collision** (two live processes sharing an address-space ID, causing stale TLB
  aliasing) was considered but not seriously pursued — `AsidAllocator` supports 256 IDs and
  only ~10-15 processes are ever alive at once in the repro, making a collision implausible
  without a separate leak bug, which wasn't found.

Net: the actual origin of the `0x20120030`/`0x20120338` value — why a fixed global in
busybox's `.data` ends up holding something that looks like an mmap-floor address despite no
mmap ever happening — is still unexplained. It's deterministic (identical `FAR`/`ELR` across
dozens of repro instances, different pids, different sessions, `SMP=1` and `SMP=4` alike),
which points at a kernel-side setup difference for CoW-forked (not-yet-exec'd) children
rather than a genuine race, but the specific mechanism needs more tracing than done here —
likely into exactly what code runs between the fork trampoline returning to userspace and
the first real syscall, for a process that inherited its entire image via CoW rather than
having just loaded it via the ELF loader.

## Propagation fix landed (real bug, but not this one)

While chasing the above, a separate, genuine bug was found and fixed: `fork_process`'s
lazy-region sharing (`crates/akuma-exec/src/process/mod.rs`, both the CoW-fork and legacy
eager-copy branches) only used `cow_share_range` to copy pages **currently resident** in the
parent — it never copied the parent's lazy-region *descriptors* themselves into a fresh
`LAZY_REGION_TABLE` entry for the child. A lazy region the parent registered but hadn't
fully touched yet (a `.data`/`.bss` page nobody had written to since exec, a stack page
deeper than the parent's current usage) would have nothing resident to share, and the child
would end up with **no lazy-region coverage for that VA at all** — not resident (nothing
shared) and not lazy (no entry to demand-page from) — a real, if narrower, path to the same
class of unconditional SIGSEGV on first touch. A single fork off a long-lived, fully
warmed-up process rarely hits this (everything relevant is usually already resident by
then); forking off a process that was itself freshly forked (exactly this bug report's
shell-subshell shape) is exactly where it would bite hardest.

Fixed by extracting a `propagate_lazy_regions_to_child(parent_pid, child_pid)` helper
(`crates/akuma-exec/src/process/children.rs`) that copies every `LazyRegion` descriptor
(VA, size, flags, and `LazySource` — including file-backed sources with their
path/inode/offset) from the parent's table entry into a fresh entry for the child, called
from both `fork_process` branches. Unit-tested directly (no address-space/TTBR0 mocking
needed, since the helper only touches the `LAZY_REGION_TABLE` global) in
`crates/akuma-exec/src/process/children.rs`'s `lazy_region_propagation_tests` module: copies
all parent regions including a `LazySource::File` variant, a no-regions-to-copy no-op case,
and a case confirming it doesn't clobber a child's pre-existing entry at a different VA.

**This fix is real and worth keeping, but it does not explain the crash in this doc.**
Verified directly: after landing it, `[DA-MISS]` for the still-crashing pid showed
`lr_count=6 parent_lr=6` (the fix's propagation working exactly as intended — all 6 of the
parent's regions correctly copied) alongside the unchanged `parent_has_va=false` — the
crashing VA was never registered by anyone in the lineage, so nothing was there to fail to
propagate.

## Evidence preserved

Full kernel log capturing the crash (dozens of repro instances across curl/wget, N=1/2/4,
SMP=1/4): `/private/tmp/claude-502/.../scratchpad/bkl_concurrent_exec_segv.log` (session-local
scratchpad, not committed — regenerate via the repro below if needed).

## Reproduction

```sh
# boot devbox-smoltcp at any SMP, then over ssh:
( wget -q -O /tmp/t0.bin http://10.0.2.2:8899/<any-file-served-locally>; echo $? > /tmp/t0.rc ) &
sleep 5
cat /tmp/t0.rc   # 139 (128 + SIGSEGV), t0.bin absent
```

Kernel log shows, for the crashing PID:

```
[DA-MISS] pid=<N> ppid=<P> va=0x20120338 lr_count=<k> parent_lr=<k> parent_has_va=false
[DP] no lazy region for FAR=0x20120338 pid=<N> (pid has <k> lazy regions)
[WILD-DA] pid=<N> FAR=0x20120338 ELR=0x1004e8f0 last_sc=18446744073709551615
[Fault] Data abort from EL0 at FAR=0x20120338, ELR=0x1004e8f0, ISS=0x46
[Fault] Process <N> (/bin/busybox) SIGSEGV after 0.00s
```

`FAR` and `ELR` are identical across every repro instance (different PIDs, different
sessions, SMP=1 or SMP=4) — this is fully deterministic, not a timing-dependent race.

## Workaround applied (test harness only)

`scripts/bkl_smp_regimen/payload/job.sh` used `( cmd; echo $? > rc; echo done > sentinel ) &`
to background each parallel worker — hitting this bug on every single invocation once real
commands (`curl`, `sha256sum`, `cp`) replaced the trivial ones it started with. Fixed by
writing each worker out as its own tiny script and backgrounding a *single* command instead:

```sh
{
    echo "curl -s -o $D/d$i.bin $URL"
    echo "echo \$? > $D/d$i.rc"
    echo "echo done > $D/w$i.done"
} > $D/worker$i.sh
sh $D/worker$i.sh &
```

`sh $D/workerN.sh &` is one simple command with nothing after it in the backgrounding shell,
so it reaches exec via a direct fork-then-exec (matching the "plain `wget &` works" case
above). Verified clean (SMP=1, all digests exact, 0 SIGSEGV) before re-running the full
SMP=4 campaign.

## What remains

- **The actual bug is still open.** Root-cause where `PTR_A` (the fixed global at
  `0x1011ff88` in busybox's `.data`) gets its value, and why dereferencing it yields
  `0x20120030` specifically for a CoW-forked-but-not-yet-exec'd child. Concretely: trace
  what runs between the fork trampoline (`crates/akuma-exec/src/process/mod.rs`,
  `trampoline ENTRY`) returning to userspace and the process's first real syscall, for a
  process whose entire image arrived via CoW-fork rather than the ELF loader — that's the
  code window this crash lives in, not `replace_image`/execve as first assumed.
- Fix belongs in the kernel, not just the test harness — **any** shell pipeline or script
  anywhere in Akuma that backgrounds a multi-statement subshell around a real command hits
  this today (confirmed: not specific to `curl`/`wget`, not specific to networking).
- Once fixed, the `scripts/bkl_smp_regimen/payload/job.sh` workaround (writing each worker
  out as its own script, backgrounded as a single `sh workerN.sh &` rather than an inline
  `( cmd; ... ) &`) can most likely be reverted to the simpler inline-subshell form, though
  there's no urgency to do so.
- The `propagate_lazy_regions_to_child` fix (previous section) should stay regardless of
  what happens with the main bug — it's an independently real correctness gap.
