# Behind the orphan bug: a write permission fault with no recovery path, and a half-written linker output — 2026-08-07

**Status**: **PARTIALLY FIXED, `-j4` still not green.** Three distinct failures now
separated. One (**C**, below) is root-caused and fixed with a two-sided host test.
One (**A**) has its *unrecoverability* fixed — the fault handler can now repair the
page state instead of dying — while what corrupts the page state remains unknown.
One (**B**) is characterised but unfixed, and is now the dominant blocker. The
[grace-expired hard kill](GRACE_EXPIRED_HARD_KILL_ORPHANS.md) fix is **confirmed
holding**. All changes uncommitted.

| | what it is | state |
| --- | --- | --- |
| **A** | write permission fault with no recovery path | recovery path added (`[EAGER-UPGRADE]`, fires); origin of the bad page state still unknown |
| **B** | linker leaves a truncated/empty output, cargo execs it | **unfixed — the current blocker** |
| **C** | a thread survives its thread group's `exit_group`, parked in `FUTEX_WAIT` forever | **root-caused and fixed** |

This doc exists because both failures were previously reported only as "run 1 hit a
zero-byte `rust-lld` output" and "run 2 got `SIGSEGV in clone_thread`" — two
one-line summaries that are, respectively, wrong and unattributed. Each turned out
to be a different animal.

---

## 1. Where the saga stood

`kill_thread_group`'s grace expiry was hard-killing threads it did not own, which
stranded processes with no thread and hung their parents' `wait4` forever. That was
fixed and A/B-verified on 2026-08-06 (commit `3c4bced`). Two runs on the fixed arm
then failed *differently*, and the session ended without attributing either.

This session reproduced both on a fresh boot of the same configuration
(`release-smp-shared` + `devbox-smoltcp,no-tests`, `SMP=4`, `MEMORY=4096`,
`disk_selfhost_fixtest.img`, cold in-guest `cargo build -j4`).

**The grace-kill fix holds.** Across every run here: `[KTG-STALE]` fires (kills
correctly *refused* against recycled slots) and `[PROC-ORPHAN]` is **zero**. The
class is closed; what follows was behind it.

Both failures reproduce in **2–12 minutes**, which is far cheaper than the
800-second futex stall the previous sessions were chasing. Neither presents as a
futex stall.

---

## 2. What this session established

| | Failure A | Failure B |
| --- | --- | --- |
| Presents as | cargo dies, `rc=139` | `Exec format error (os error 8)` |
| Reproduced at | 24 crates (56.8 s), 20+ crates (160.6 s) | 20 crates (72 s) |
| Immediate cause | EL0 write permission fault, no recovery path | cargo execs a truncated, headerless binary |
| Kernel or userspace? | **Kernel** — the fault handler had two outs and took neither | **Undetermined** — the file is a half-written linker output |
| Previously reported as | "SIGSEGV in clone_thread" | "zero-byte `rust-lld` output" |
| Both prior labels were | unattributed (that line prints for *any* fatal fault in a CLONE_VM thread) | wrong (the file is 457 KB, not zero) |

---

## 3. Failure A — a write permission fault with no recovery path

### 3.1 The symptom

```
[T279.47] [Fault] Data abort from EL0 at FAR=0x30c5225d, ELR=0x3002a178, ISS=0x4f
[Fault]  tid=11 ttbr0_live=0x1700006915b000 ttbr0_proc=0x1700006915b000
[Fault] Process 14 (/usr/local/bin/cargo) SIGSEGV after 56.78s
```

`ISS=0x4f` decodes as **WnR=1, DFSC=0x0F** — a *write* **permission** fault, not a
translation fault. The page is mapped and valid; the store was refused. Note
`ttbr0_live == ttbr0_proc`: this is **not** the
[trampoline / AS-mismatch class](TRAMPOLINE_STALE_PROCESS_RELR.md), which is fixed
and stayed fixed.

A second run faulted at a different address with the **same faulting PC**:

```
[T178.52] [Fault] Data abort from EL0 at FAR=0x31b5723d, ELR=0x3002a178, ISS=0x4f
```

`ELR=0x3002a178` (with `x30=0x3002a07c`) in both. One code site, two addresses.

### 3.2 Why this is a kernel bug by construction

`rust_sync_el0_handler`'s data-abort arm has **exactly two** ways to resolve a write
permission fault (`src/exceptions.rs`, the `is_permission_fault` block):

1. the **CoW break** — taken only when `crate::pmm::cow_ref_get(old_pa) > 0`;
2. behind it, the **lazy-region permission upgrade** — taken when
   `lazy_region_lookup_for_page_fault` finds a region whose flags are not `none`.

Reaching SIGSEGV means both declined. Nothing in the `[Fault]` block says which, so
the three candidate causes are indistinguishable after the fact. Hence `[WPF]`
(`print_write_perm_fault_diag`), which re-derives all of their inputs at the SIGSEGV
site:

```
[WPF] pid=14 as_owner=14 va=0x31b57000 pa=0x7ab47000 mapped=true cow_ref=0
      lazy_self=NONE lazy_owner=NONE have_owner=true free=667597
```

Read the fields:

- `mapped=true pa=0x7ab47000` — the PTE is valid. The page is present and
  read-only.
- **`cow_ref=0`** — the page carries no CoW reference, so the break is skipped by
  design.
- **`lazy_self=NONE lazy_owner=NONE`** — *no lazy region covers this VA at all*, so
  the upgrade is skipped by design.
- `have_owner=true`, `free=667597` — not a missing owner, and nowhere near OOM.

So the process holds a **mapped, read-only page that no region describes and no CoW
reference protects**, and writes to it. Both outs decline correctly given their
inputs; the bug is upstream, in whatever produced that page.

### 3.3 What is ruled out

- **OOM** — `free=667597` pages.
- **Missing address-space owner** — `have_owner=true`.
- **AS mismatch / stale trampoline** — `ttbr0_live == ttbr0_proc`.
- **Region looked up under the wrong pid.** A natural suspicion, since
  `mmap_regions` live on the thread-group leader. Dead twice over:
  `lazy_region_lookup_for_page_fault` resolves the address-space owner *internally*
  before falling back to the passed pid, and here `pid == as_owner == 14` anyway.
- **`mprotect` clobbering a whole region on a sub-range call** (the class fixed
  2026-03-14). `LazyRegionMap::update_flags` splits correctly into up to three
  pieces; re-read and confirmed, not regressed.
- **An eager `mmap` silently inheriting a stale PTE.** This was the leading theory
  and it is **not confirmed** — see §5.2.

### 3.4 What the VA looks like

`0x31b57000` sits in cargo's Rust thread-stack arena. The surrounding traffic is
unmistakable:

```
[mmap]     pid=14 len=0x203000 prot=0x0 flags=0x22 = 0x31b11000 (lazy)
[mprotect] pid=14 addr=0x31b13000 len=0x201000 prot=0x3
...
[mmap]     pid=14 len=0x10000  prot=0x3 flags=0x22 = 0x31b4c000 (eager)
```

That is std's thread-stack idiom — reserve `PROT_NONE`, then `mprotect` the usable
2 MB `RW`, leaving a guard page — followed much later by a **64 KB eager mmap landing
inside the same 2 MB range**, i.e. VA recycled through `free_regions` after the
thread exited. Several guard VAs are `mprotect`ed `PROT_NONE` **two and three times**
over the run, confirming heavy VA reuse.

The faulting page belongs to the *later, eager* mapping. Eager mmaps register **no
lazy region** — which is exactly why `lazy_*=NONE`, and which means an eager region
has no permission-upgrade path at all. That is a structural gap regardless of what
made this page read-only.

---

## 4. Failure B — a half-written linker output

### 4.1 The symptom

```
error: failed to run custom build command for `embedded-io-async v0.6.1`
Caused by:
  could not execute process `.../out/build_script_build` (never executed)
Caused by:
  Exec format error (os error 8)
```

Kernel side, `execve` → `replace_image` → ENOEXEC, then the caller retries and falls
back to `busybox ash <binary>`, which is the shell's ENOEXEC fallback.

### 4.2 The file is genuinely bad on disk — verified three ways

The previous session's note ("zero-byte `rust-lld` output") is wrong on both counts:
the file is 457 252 bytes, and the linker was GNU `ld` via `collect2`, not
`rust-lld`. What it actually looks like:

```
mode  0666      size 457252      first 64 bytes all zero
first non-zero byte at offset 0x40 — a valid PT_LOAD Elf64_Phdr
71 of 112 file blocks are holes (block pointer == 0)
```

Three independent reads, to separate "bad on disk" from "bad read path":

1. **In-guest read** while the VM was live — zeroed header.
2. **Cold reboot, same disk** — byte-identical (`sha256` match). Rules out a
   dirty/stale in-memory cache.
3. **Host-side read of the raw image with an independent ext2 parser**
   (`ext2read.py`, no Akuma code in the path) — byte-identical again.

(3) is the one that matters: (2) still reads through Akuma's own ext2 driver, so a
block-mapping bug would reproduce identically in both directions. Only an
independent parser separates "the write path lost data" from "our block mapping is
wrong in both directions". It is genuinely bad on disk.

### 4.3 The control disk — which corrects the obvious reading

"71 of 112 blocks are holes" looks damning, and the obvious conclusion — writes
silently failing to allocate blocks — is **wrong**. Scanning a second, older image
(`disk_selfhost.img`, a different build from 2026-08-04) for the same artifacts:

| build script | fixtest size | control size | fixtest holes | control holes | magic | mode |
| --- | --- | --- | --- | --- | --- | --- |
| quote | 738304 | 738304 | 14/181 | **14/181** | `7f454c46` | 0777 |
| generic-array | 764376 | 764376 | 7/187 | **7/187** | `7f454c46` | 0777 |
| proc-macro2 | 765024 | 765024 | 6/187 | **6/187** | `7f454c46` | 0777 |
| **embedded-io-async** | **457252** | 744256 | **71/112** | 12/182 | **`00000000`** | **0666** |

Identical hole counts on two independently produced disks ⇒ **sparse linker output
is normal and deterministic**; `ld` legitimately seeks over alignment padding. Three
of the four "holed" files on the failing disk are byte-for-byte the size of their
known-good counterparts. Across the whole `target/` tree only 4 of 167 files have
any holes, and 3 of them are correct.

One file is wrong, and the **mode bits** are the tell: every good linker output is
`0777`, the broken one is `0666`. GNU `ld` creates its output `0666` and makes it
executable at the end. A `0666` output, truncated by 287 KB, with its ELF header
never written, is a linker that **started writing and never finished**.

### 4.4 What is ruled out

- **Stale read cache** — §4.2, three ways.
- **Writable `MAP_SHARED` mmap writeback.** Zero `shared-writable` mmaps and zero
  `shared-writeback` lines in the whole run; the linker did not use mmap output, so
  `SHARED_FILE_MAPPINGS` / `writeback_shared_pages` are not involved.
- **An ext2 sparse-block / allocation bug** — §4.3, the control disk.

### 4.4a Three instances, one signature: mode `0666`

Failure B has now been caught on three different crates in three runs —
`curve25519-dalek`, `embedded-io-async`, `typenum` — always a **build-script link**,
never the same crate twice. The instances look different in size and shape:

| instance | size | control size | ELF magic | mode |
| --- | --- | --- | --- | --- |
| `embedded-io-async` | 457252 | 744256 | zeroed header, phdrs intact at `0x40` | **0666** |
| `typenum` | **0** | 675600 | — (empty) | **0666** |

So "zero-byte output" (the earlier session's note) and "truncated with a zeroed
header" are the *same* bug caught at different points, not two bugs — which is why
that note read as wrong against the `embedded-io-async` instance and right against
this one.

The invariant across all of them is the **mode**. `ld` creates its output `0666` and
makes it executable at the end; every *correct* build script on both disks is `0777`.
A `0666` output means the linker never got to the end. Combined with cargo going on
to *execute* it, the failure is: a child that did not finish its work, whose exit
status the parent read as success.

### 4.5 Leading hypothesis (not proven)

Cargo went on to *execute* the output, so cargo believed the link command **exited
successfully**. Combined with §4.3 that gives: a child that did not finish its work,
whose exit status was reported to the parent as success. That is the same family as
the rest of this saga — child death and exit-status propagation — rather than a
filesystem bug.

Not proven. In particular, "no `ld` exec line mentions the crate" is **not**
evidence the linker never ran: the `execve` tracer truncates its args well before
the output path.

---

## 4a. Failure C — a thread that outlived its own `exit_group`

Found by letting A and B get out of the way: with the fault handler no longer
killing cargo, one run reached **88 crates** and then stalled with the *original*
symptom this whole saga started from — untimed `FUTEX_WAIT`, ~3 % CPU.

### The evidence

`[THR-DUMP]` gives the whole story in one line. rustc `tgid=122` has exactly **one**
thread — `tid=16`, belonging to pid 123 — and no thread for its leader, pid 122:

```
tid=16 st=? pid=123 tgid=122 ... a0=0x3cda5fc4   (futex, queued 557 s)
```

And 557 s earlier, at T42.34:

```
[KTG] my_pid=122 my_tgid=122 by_tid=22 code=0 siblings=2 first=Some((123, Some(16)))
```

rustc 122 exited **normally** (`code=0`) with two siblings to kill. `tid=16` was
never terminated. Because it never exits, its `Process` is never reaped, so cargo's
`wait4` never returns — cargo's `tid=13` is parked 583 s on the same dump. The whole
build is held by one thread that ignored a kill.

Not the previous bug: `[PROC-ORPHAN]` is **0** (pid 123 *has* a thread — this is the
inverse shape, a thread that outlived its group), and neither `[KTG-STALE]` line in
the run names pid 122 or 123, so the grace-kill fix did not refuse this one.

### The root cause

Two predicates in `kill_thread_group` both treat *"the kill request is no longer
pending"* as *"the thread died"*:

```rust
// grace-wait completion
crate::threading::is_thread_terminated(tid) || !crate::threading::has_pending_kill(tid)
// and the hard-kill gate
has_pending_kill(tid) && table::pid_for_thread(tid) == Some(sib_pid)
```

The request is consumed at the **EL1→EL0 boundary** — `take_thread_kill_request()`
in `rust_sync_el0_handler`, the only consumer in the tree. A thread parked in an
**untimed `FUTEX_WAIT` never reaches that boundary**: `request_thread_kill` wakes it,
it re-checks its futex, finds it unsatisfied, and re-parks. So anything that clears
the flag without the thread dying makes the grace loop declare success immediately —
the 2 s expiry never runs — *and* makes the hard-kill gate refuse. The thread is
spared twice over and parked forever.

Ownership (`pid_for_thread(tid) == Some(sib_pid)`) is the real safety property; it is
what stops the hard kill from taking out a recycled slot's new owner. The pending
flag was only ever evidence about *timing*, and after a 2 s grace it is evidence of
nothing.

### The fix

- `grace_kill_should_terminate` gates on **ownership alone**.
- The grace-wait completion test requires actual termination (or that the slot is no
  longer ours), so a sibling that swallowed its request now reaches the 2 s expiry
  and gets hard-terminated instead of surviving.

Regression: `grace_kill_forces_a_real_straggler_but_spares_recycled_and_quiet_slots`,
whose third assertion had to be **inverted**. It previously read "a sibling with no
pending kill request … must be left to self-terminate at its own boundary". That
rationale holds only if the thread will *reach* a boundary; this failure is the proof
that it need not. The assertion now requires such a sibling to be terminated, with
the reasoning recorded inline so the next reader does not "fix" it back.

---

## 5. False trails, and the corrections

### 5.1 "71 of 112 blocks are holes, so writes are being dropped"

Stated confidently mid-investigation and wrong. Sparse linker output is normal.
Caught only by comparing against a second disk carrying the same artifacts — the
cheapest control available, and it should have been the *first* step, not a late
one. **A hole count is meaningless without a known-good copy of the same file.**

### 5.2 "The eager mmap inherited a stale PTE"

A well-supported theory: `map_user_page_no_flush` **refuses** a VA whose PTE is
already valid and reports that by returning `false`, and `sys_mmap`'s eager install
loop was **discarding that flag** (`let (table_frames, _) = ...`). That would hand
userspace the previous occupant's page and permissions while it believed it had
fresh zeroed memory — producing exactly the `cow_ref=0 lazy=NONE` signature of §3.2.

`[MMAP-STALE-PTE]` was added to catch it. It **never fired** across a full run. The
discarded return value is still a real latent defect worth fixing on its own merits,
but it is **not** the source of Failure A. Recorded here so the next session does
not re-derive it.

This is the third time in this saga that a mechanically plausible theory survived
code reading and died on a tripwire. Build the probe.

### 5.3 Two diagnostics that mislead

- **`[Fault] SIGSEGV in clone_thread, calling exit_group`** is printed for *any*
  fatal fault in a thread whose address space is shared. It names the teardown path,
  not the cause. Reporting a run as "SIGSEGV in clone_thread" attributes nothing —
  read `ISS` and `FAR` instead.
- **`[THR-DUMP]`'s `pid=`/`tgid=` columns** come from `find_pid_by_thread`, the same
  stale table scan behind the trampoline bug. The per-thread `tsc=` (exact syscall)
  and the futex table's `tgid` are trustworthy; the `pid=` column is not. The `sc=`
  column is process-wide, not per-thread — use `tsc=`.

---

## 6. Instrumentation added (uncommitted)

- **`[WPF]`** — `print_write_perm_fault_diag` in `src/exceptions.rs`, called from the
  EL0 data-abort SIGSEGV path. Fires only for write permission faults on the way to
  a fatal signal. Prints `cow_ref`, resolved PA, region flags under both pids, owner
  presence and free-page count — i.e. the inputs to every recovery path that
  declined. This is what turned Failure A from "cargo crashed" into a partitioned
  question in one run.
- **`[MMAP-STALE-PTE]`** — `src/syscall/mem.rs`, in `sys_mmap`'s eager install loop.
  Counts pages where `map_user_page_no_flush` refused an already-valid PTE and names
  the range and surviving PA. Never fired (§5.2); worth keeping as a tripwire for an
  invariant that is currently unenforced.

---

## 6a. What the fixes are, and what the runs did and did not prove

**Failure A — the recovery path (`MmapRegion::flags` + `[EAGER-UPGRADE]`).** An eager
`mmap` installs its pages up front and registers **no lazy region**, and
`MmapRegion` recorded extent and frames but no protection. So a read-only page
inside a writable eager mapping reached neither the CoW break (no `cow_ref`) nor the
lazy-region upgrade, and there was no third path: SIGSEGV by construction. The fix
gives `MmapRegion` a `flags` field, threads the real protection through `sys_mmap`,
`sys_mremap`, the munmap split and the fork child, teaches `sys_mprotect` to update
it, and adds the symmetric upgrade to the fault handler — **gated on the region
actually being writable**, so `mprotect(PROT_READ)` and `PROT_NONE` guard pages still
fault as they must.

The default for the unrecorded case is `NONE`, not `RW_NO_EXEC`, deliberately: these
flags only ever *grant* a write, so an unknown protection must grant nothing. A
permissive default would silently defeat `mprotect` on every region built through the
bare constructor. The host test is two-sided on exactly that — flipping the default
back fails it on the asserted line.

`[EAGER-UPGRADE]` **fired twice** in a later run, so the path is real and exercised.
But this repairs the *symptom*: what leaves a page mapped read-only with `cow_ref=0`
inside a writable eager region is still unknown, and is still worth finding.

**Do not credit the fix with the 88-crate run.** That run showed zero `[WPF]`, zero
SIGSEGV — and zero `[EAGER-UPGRADE]`. The repair never fired, so the improvement from
24 to 88 crates is unattributed and may be variance. Two runs, two different
failures, is not an A/B.

**A regression risk introduced here, stated plainly.** Failure C's fix makes the
grace-wait loop require *actual termination*, so the 2 s expiry — and the hard kill
behind it — is now **reachable more often** than when the loop could short-circuit on
a consumed flag. The run after that change died at 7 crates (Failure B on `typenum`)
against 88 the run before. That is one run against one run of a **racy** failure, so
it attributes nothing (§5.2's lesson applies to this doc's own results). It is
recorded because it is the obvious thing to A/B next, not because there is evidence
for it. The 7 hard kills in that run were all `pending_kill=true`, i.e. targets the
old predicate would also have taken.

---

## 7. Next steps

1. **Failure B is now the blocker — start here.** Log every `ld`/`collect2`/`cc` exit
   status next to the parent's `wait4` result, and check whether the linker is among
   the processes torn down by `kill_thread_group`. The `0666` mode (§4.4a) says the
   linker stopped early; cargo executing the output says cargo saw success. Find
   which of those two is the lie. Note the `execve` tracer truncates its args, so
   "no `ld` line names the crate" proves nothing — widen the trace first.
2. **A/B the Failure C fix against the 2 s grace-expiry reachability** (§6a). Same
   kernel, toggle only the completion predicate, several runs per arm, and count
   Failure B occurrences per rustc invocation — not per run, since runs end at
   different depths.
3. **Failure A's producer.** What leaves a page read-only with `cow_ref=0` inside a
   writable eager region? Instrument the producer, not the consumer: a tripwire on
   any user PTE transitioning to read-only without a matching `cow_ref_inc`. The
   `[EAGER-UPGRADE]` path now masks the symptom, so this will get quieter, not
   louder — do it before the evidence disappears.
4. Fix the discarded return value in `sys_mmap`'s eager install loop on its own
   merits (§5.2), independent of Failure A.
5. All three failures reproduce in **2–12 minutes**; there is no need for the long
   stall runs that dominated earlier sessions.

---

## Background

- [`GRACE_EXPIRED_HARD_KILL_ORPHANS.md`](GRACE_EXPIRED_HARD_KILL_ORPHANS.md) — the
  bug immediately in front of these two, fixed 2026-08-06 and confirmed holding here.
- [`TRAMPOLINE_STALE_PROCESS_RELR.md`](TRAMPOLINE_STALE_PROCESS_RELR.md) — the RELR /
  AS-MISMATCH class, ruled out for Failure A by `ttbr0_live == ttbr0_proc`.
- [`STALE_THREAD_SLOT_KILL.md`](STALE_THREAD_SLOT_KILL.md) — the `Process::thread_id`
  family these all belong to.
- [`SELFHOST_DEVBOX_SMOLTCP.md`](SELFHOST_DEVBOX_SMOLTCP.md) — the `-j4` self-host
  effort overall.
- [`runbooks/debug-thread-spawn-segv.md`](../runbooks/debug-thread-spawn-segv.md),
  [`runbooks/debug-futex-lost-wakeup.md`](../runbooks/debug-futex-lost-wakeup.md) —
  the runbooks this session did **not** need; neither failure here is a futex stall.
