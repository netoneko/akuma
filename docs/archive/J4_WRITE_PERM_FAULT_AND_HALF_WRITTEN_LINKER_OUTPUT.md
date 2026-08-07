# Behind the orphan bug: a write permission fault with no recovery path, and a half-written linker output — 2026-08-07

**Status**: **PARTIALLY FIXED, `-j4` still not green.** Three distinct failures now
separated. One (**C**, below) is root-caused and fixed with a two-sided host test.
One (**A**) has its *unrecoverability* fixed — the fault handler can now repair the
page state instead of dying — while what corrupts the page state remains unknown.
One (**B**) is characterised but unfixed, and is now the dominant blocker. The
[grace-expired hard kill](GRACE_EXPIRED_HARD_KILL_ORPHANS.md) fix is **confirmed
holding**. A fourth shape (**D**) — rustc threads parked in `FUTEX_WAIT` for the
bulk of the build, distinct from C — surfaced in a fresh repro and is
**not yet root-caused**; see §7. All changes uncommitted.

| | what it is | state |
| --- | --- | --- |
| **A** | write permission fault with no recovery path | recovery path added (`[EAGER-UPGRADE]`, fires); origin of the bad page state still unknown |
| **B** | linker leaves a truncated/empty output, cargo execs it | **unfixed — the current blocker** |
| **C** | a thread survives its thread group's `exit_group`, parked in `FUTEX_WAIT` forever | **root-caused and fixed** |
| **D** | an rustc thread-group *leader* (never reaching `exit_group`) parked in `FUTEX_WAIT` almost since its own start | **observed once, not root-caused** — see §7 |

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

## 7. Failure D — an rustc leader thread parked almost since its own start (2026-08-07, not root-caused)

Found while re-running the repro to widen the `execve` tracer and hunt Failure B
(§8 covers that instrumentation). This run never reproduced B; it hit something
else first and was killed once the shape was captured, on the reasoning that a
stall this reproducible will resurface on its own and can be root-caused then,
rather than spending this session's remaining budget on a fourth investigation
mid-stream. **Recorded here, not fixed, not fully explained.**

### 7.1 The symptom

Cold `-j4` build, same repro as always. Progress was normal (25 crates at T+21s,
104 crates by roughly T+380s), then **stopped advancing entirely**: `grep -c
Compiling /root/j4.log` returned 104 for the rest of the run, sampled every 20s
for 9+ minutes (the outer harness's own stall detector fired at the 3-minute
mark). 11 processes remained alive throughout — this is not a full kernel wedge;
SSH and `busybox ps`/`grep` kept working the entire time.

### 7.2 The evidence

`[FUTEX-DUMP]` at `T1140.27` (build started at T~0, so ~19 minutes in):

```
[FUTEX-DUMP] 7 keys
  key tgid=14 uaddr=0x30859818 waiters=1
    tid=13 bitset=0xffffffff queued_for=753934600us hist=puSXEpuSXEpuSXEp
  key tgid=14 uaddr=0x30a555c8 waiters=1
    tid=14 bitset=0xffffffff queued_for=758672226us hist=--------------Ep
  key tgid=14 uaddr=0x3196b300 waiters=1
    tid=12 bitset=0xffffffff queued_for=159068us hist=puSXEpuSXEpuSXEp
  key tgid=315 uaddr=0x3cda5fc4 waiters=1
    tid=32 bitset=0xffffffff queued_for=1062208017us hist=pWuXEpWuXEpWuXEp
  key tgid=315 uaddr=0x3d90e5e8 waiters=1
    tid=27 bitset=0xffffffff queued_for=1062191730us hist=pWuXEpWuXEpWuXEp
  key tgid=968 uaddr=0x3cda5fc4 waiters=1
    tid=19 bitset=0xffffffff queued_for=893967513us hist=pWuXEpWuXEpWuXEp
  key tgid=968 uaddr=0x3d90f5e8 waiters=1
    tid=30 bitset=0xffffffff queued_for=893940891us hist=pWuXEpWuXEpWuXEp
```

`tgid=14` is cargo itself (same convention as §3.2's `Process 14`, confirmed by
this repro's boot too). `tgid=315` and `tgid=968` are two independent rustc
invocations, identified from their (garbled, interleaved — see §8.1) `execve`
lines:

- **pid=315**: `rustc --crate-name build_script_build ... libm-0.2.16/build.rs`,
  started at `T77.60`. `queued_for=1062208017us` (~1062 s) on both its futex keys
  means it started waiting at roughly `T78` — **within a couple seconds of
  starting**, and it is still waiting 1062 s later, i.e. for essentially this
  process's entire observed lifetime.
- **pid=968**: `rustc` compiling `zerocopy-derive` (a proc-macro crate, linked
  against `proc-macro2`/`quote`/`syn`), started ~`T246`. Its futex keys show
  `queued_for≈894s`, i.e. parked almost immediately after starting, same shape.

### 7.3 Why this is *not* Failure C

Failure C's mechanism requires the thread-group **leader to have already called
`exit_group`**, with `kill_thread_group` then declining to terminate a sibling
still parked in an untimed `FUTEX_WAIT`. That is not what the `THR-DUMP` shows
here:

```
tid=27 st=? pid=315 tgid=315 l0=0x7318a000 sc=63 tsc=98 a0=0x3d90e5e8 a1=0x80 elr=0x3004839c
tid=32 st=? pid=319 tgid=315 l0=0x7318a000 sc=-1 tsc=98 a0=0x3cda5fc4 a1=0x80 elr=0x30060cc4
```

`tid=27` has `pid=315 == tgid=315` — **it is the thread-group leader itself**,
and it is one of the two parked threads. Rustc's own main thread never reached
`exit_group`; it is blocked (waiting on a worker, most likely — this looks like
an internal rayon/thread-pool wait-for-completion, not a syscall exit path) at
the same time as a sibling (`tid=32`, `pid=319`) is *also* parked. `[KTG]` never
fires for `my_pid=315` or `my_pid=968` anywhere in this log — `kill_thread_group`
was never invoked for either, because neither leader ever called `exit_group`.
Whatever this is, it is upstream of the machinery Failure C's fix touches, not a
recurrence of it.

The repeated `uaddr=0x3cda5fc4` across two **independent** thread groups (315 and
968) is consistent with a fixed-ASLR-layout static/TLS futex inside rustc's own
binary (same address, same binary, no ASLR — matches the VA-reuse pattern
already noted in §3.4) — i.e. likely the same synchronization primitive in both
cases, not a coincidence.

The `hist=` field's repeating `pWuXEpWuXEp...` cycle (park–Wake–unpark–eXit–Enter,
repeating) on both `0x3cda5fc4` waiters suggests each is being woken periodically
and re-parking without making progress — an unsatisfied-condition loop, not a
single missed wakeup. Not yet understood; recorded verbatim for whoever picks
this up.

Cargo's own three parked threads (`tgid=14`, queued 159–759 s) are the most
likely **downstream** consequence — cargo blocked on subprocess results it will
never get because pid=315/968 never finish — rather than an independent bug, but
this is not confirmed either.

### 7.4 Disposition

Killed and the repro re-run to chase Failure B instead (see §8) — but the
**second** run hit the identical shape again (2-for-2), on `typenum`'s
build-script rustc and another proc-macro-linked rustc, so it was chased
further this session. Findings below; not root-caused to a fix, but narrowed a
lot. **Pick up here.**

### 7.5 What `0x3cda5fc4` actually is (identified from source, not inferred)

`0x3cda5fc4` is the **same address already named in the worked example for the
already-fixed Failure C** (§4a, `a0=0x3cda5fc4`) — confirmed by grep, not
coincidence.

Pulled jobserver-rs 0.1.35's actual source (it's on the host at
`~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/jobserver-0.1.35/src/`
— `Cargo.lock` pins this version; the guest's own copy is reachable over SSH at
`/root/.cargo/registry/src/.../jobserver-0.1.35/` if the host copy ever goes
missing). `HelperState::for_each_request` (`lib.rs:583-606`) — the jobserver
Helper thread's own loop — does an **untimed** `self.cvar.wait(lock)` whenever
`requests == 0`. That matches the observed `a1=0x80` (`FUTEX_WAIT|PRIVATE`,
untimed) exactly, and `HelperState` is one of the first heap allocations
rustc's codegen backend makes when it sets up its jobserver client — early and
deterministic enough that, with no ASLR, its `Condvar`'s futex word lands at
the identical address in every rustc process. That is why it is the *one*
address that is byte-identical across every stuck invocation (pid=315, 968,
26, 737 across two separate runs), while each process's *other* stuck address
(`0x3d90e5e8`/`0x3d90a5e8`/`0x3d90f5e8` — small offset, not identical) is
something else, allocated later, at a point whose exact address depends on
incidental prior heap traffic. **Not yet identified** — candidates are
`imp::Helper::join()`'s `wait_timeout` loop (`unix.rs:349-395`, but that's
*timed*, `a1` would be `0x89`, so this doesn't match as observed) or
`JoinHandle::join()`'s own internal `Parker` (a *different*, untimed
std primitive — matches `futextest.rs` phase 7 below). Needs the same
source-identification treatment §7.5 gave `0x3cda5fc4` before trusting it.

The Helper's wait is woken only by `HelperThread::request_token()` (someone
wants a token) or `HelperThread::drop()` (shutdown: `producer_done = true` +
one `notify_one()`) — both in `lib.rs:552-566`.

### 7.6 The wake was never attempted — not lost, just never issued

`[FUTEX-WAKERING]` (`docs/runbooks/debug-futex-lost-wakeup.md` §2) for
`uaddr=0x3cda5fc4` shows **zero `same-addr` entries in either entire boot** —
not "for this tgid", for *any* tgid, across the whole build. Per that runbook's
own step 2: no wake in that namespace at all means the bug, if any, is
**upstream of the futex code** — some userspace logic that should have called
`request_token()` or dropped the `HelperThread` never got there. This rules out
re-litigating the already-fixed scheduler race (§6a in
`GRACE_EXPIRED_HARD_KILL_ORPHANS.md`-adjacent territory / the 2026-08-05
`publish_waiting_and_take_pending_wake` fix): that fix closed a **wake-issued
but-dropped-by-the-scheduler** gap (`hist` ending `...EpW` with no `u`). What
we see here ends `...Ep` with **no `W` at all, ever** — a different shape.

### 7.7 A tempting but probably wrong lead — recorded so it isn't re-chased blind

`docs/archive/FUTEX_REQUEUE_LOST_WAKEUP.md` (2026-08-04) fixed a
`pthread_cond_broadcast`/`FUTEX_REQUEUE` lost-wakeup class, explicitly flagged
in that doc as *"the plausible candidate for the `typenum` lost-wakeup
stall"* — and that doc's own text admits *"not reproduced before the fix and
not re-reproduced after... not a confirmed root-cause-and-cure. A `-j4`
self-host build that survives past the point it previously stalled is the
decisive re-test."* That re-test never happened, and **`typenum` is exactly
what stalled in this session's second run** (pid=26). Tempting to declare this
the same bug recurring.

**Probably not the mechanism, reasoned but not proven:** that fix targets
`pthread_cond_broadcast`, which is musl's C-level `pthread_cond` machinery
(uses `FUTEX_REQUEUE`). jobserver-rs calls plain `std::sync::Condvar`, and
Rust's std on `target_os = "linux"` (musl included — `target_env` doesn't
gate this selection) uses its **own raw-futex** `Mutex`/`Condvar`/`Parker`
primitives, bypassing musl's pthread_cond entirely: plain `FUTEX_WAIT`/
`FUTEX_WAKE`, never `REQUEUE`. If that's right, the `typenum` name match is
coincidence — whatever races at this point in the dependency graph happens to
surface on `typenum`, not the same bug returning. **Not verified** — would be
worth confirming which lock implementation Rust std actually selects for this
exact target triple before fully discarding this lead.

### 7.8 The concrete next step: an untried, already-built probe

`userspace/selfhost_repro/futextest.rs` (→ built as `futextest_rs`) already
exists and already exercises exactly the right primitives in isolation:
`phase_condvar` (Mutex+Condvar producer/consumer — jobserver's `for_each_request`
shape) and `phase_park_unpark` (`std::thread::park`'s raw Parker — a candidate
for the *other*, unidentified address). Both reportedly passed before, but
**only run in isolation, never under real 4-core contention matching actual
`-j4` pressure** — which is precisely the condition the already-fixed
2026-08-05 scheduler bug needed to manifest at all (see
`debug-futex-lost-wakeup.md` §4a: "why `-j4` and not `-j1`... needs a
*concurrent* waker on another core to land inside it").

**Do this next:** run many concurrent copies of `futextest_rs` (or a version
with more rounds / more producer threads, matching jobserver's actual
multi-producer/single-consumer shape more closely than phase 4's 1:1 pattern)
under real SMP load — ideally alongside genuine background CPU/scheduler
pressure, not alone. Much cheaper per iteration than a full rustc build (~15
min to reproduce vs. seconds), and if it reproduces there, it's a clean kernel
bug with a tight repro instead of one buried inside rustc. If it does **not**
reproduce even under heavy stress, that's evidence the bug is specific to
rustc's actual usage pattern (e.g. the `f(self)`-outside-the-lock timing in
`for_each_request`, or genuine resource pressure under Akuma's constraints)
rather than a raw primitive bug — pivot to instrumenting rustc's own jobserver
call sites at that point instead.

Also still open: identify `0x3d90e5e8`-style precisely (§7.5), and check
whether `[TERM]`/`[Fault]`/`[kill]`/`[PROC-ORPHAN]` ever name these specific
tgids (checked for tgid=315 and tgid=26 this session: **zero hits, both
runs** — ruling out "a sibling died before it could wake me" as the cause).

---

## 8. Instrumentation added for Failure B, this session (2026-08-07, uncommitted)

Landed to make Failure B attributable per §9's original next-steps item 1 ("the
`execve` tracer truncates its args well before the output path... widen the
trace first"), then spent chasing Failure D above before B reproduced again:

- **`src/syscall/proc.rs`, `sys_execve`**: the `[syscall] execve(path=...,
  args=...)` trace buffer was 192 bytes (`tprint!(192, ...)`), which truncates a
  linker's full argv — including the `-o <output>` path — well before the
  interesting part, exactly as §4.5 warned. Widened to 2048 bytes. Confirmed
  safe: `KERNEL_STACK_SIZE` is 1 MB and `safe_print!`/`tprint!` call sites
  elsewhere already use buffers up to 1024 bytes at comparable (syscall-handler)
  stack depth.
- **`src/syscall/proc.rs`, `sys_exit_group`**: added an unconditional (not gated
  behind `SYSCALL_DEBUG_NET_ENABLED`, which is off by default and would have
  hidden this) `[PROC-EXIT] pid=... tgid=... name=... code=...` trace, so every
  process's exit code can be correlated by pid against its `execve` line without
  needing the net-debug build. This is what identified, e.g., that `pid=1404`
  (rustc compiling `fdt`) exited normally with `code=0` only ~3 s after
  starting (§7's stall is unrelated to this pid — noted here only as an example
  of the new trace in use).
- **`crates/akuma-exec/src/process/mod.rs`, `kill_children_whose_parent_in`**:
  added `[ORPHAN-KILL] parent_pid=... child_pid=... already_exited=... name=...`,
  rate-limited like the existing `[KTG]` family. This is the path (invoked from
  `return_to_kernel` via `kill_child_processes_for_thread_group`) that
  force-reaps a still-alive forked child when its parent's whole thread group
  exits — `already_exited=false` on a linker process (`cc`/`collect2`/`ld`)
  would mean it was SIGKILL'd mid-write with no flush/close, which would produce
  exactly Failure B's `0666`-mode truncated-file signature. **Never fired in
  this run** (0 occurrences) — Failure B did not reproduce this session, so this
  remains an untested hypothesis, not a ruled-out one. `kill_thread_group`
  itself (the other candidate named in §9's original item 3) was re-read and
  confirmed to only ever touch `CLONE_THREAD` siblings (`p.tgid == tgid`), never
  forked child *processes* — so it structurally cannot be the mechanism that
  kills a linker; `kill_children_whose_parent_in` is the actual candidate for
  that role, hence the new tracer's placement.

### 8.1 A gap this run exposed: interleaved console output

Multiple cores print to the same UART concurrently with no per-line locking, so
under `-j4` many log lines — including `execve` lines carrying the evidence this
session needed — arrive **byte-interleaved** with lines from other cores (e.g.
`"PID 968"` split by a `[PSTATS]` line from a different core landing mid-write).
Worked around here by reading the raw log with a byte-oriented scan (find the
substring, print surrounding context, strip embedded `[T..]` fragments) rather
than line-oriented `grep`, and cross-checking against `[mmap]`/`[PSTATS]` lines
carrying the same pid, which are shorter and interleave less. Not fixed; a
per-core or lock-protected console buffer would remove the need for this
workaround, but that is out of scope here.

#### 8.1a Why the existing multikernel fix doesn't cover this

There already *is* a fix for exactly this class of problem, but it doesn't reach
this build. `src/console.rs`'s `emit()` chokepoint (`docs/reference/subsystems/
console.md`):

```rust
fn emit(bytes: &[u8]) {
    #[cfg(kernel_smp)]
    if crate::smp::console_emit(bytes) {
        return;
    }
    crate::irq::with_irqs_disabled(|| {
        for &b in bytes { UART.write(b); }
    });
}
```

Under **multikernel** (`kernel_smp`, the `smp` feature — one isolated kernel per
core), a secondary can't even map the UART (it's not in its restricted table),
so it routes output through a per-core `ConsoleRing` (`akuma_smp::ConsoleRing`,
host-testable) in the shared `MachineConfig` descriptor; a dedicated BSP
drainer thread (`src/smp.rs`, `start_console_drainer`/`drain_console_rings`)
empties every ring to the real UART, one core at a time, so only the BSP ever
touches the hardware. That part genuinely serializes output correctly.

But `#[cfg(kernel_smp)]` and `#[cfg(kernel_smp_shared)]` are mutually exclusive
builds, and this whole investigation runs on `release-smp-shared` +
`smp-shared` — **real shared-kernel SMP**, all cores executing one kernel image
concurrently. There is no `kernel_smp_shared` branch in `emit()` at all, so
every core falls straight to the `irq::with_irqs_disabled` UART-write loop,
which only masks the *local* core's IRQs — it does nothing to stop a second
core writing the same UART register at the same instant. Hence the
interleaving above.

A further asymmetry matters if this gets ported: multikernel deliberately keeps
the **BSP's** own path synchronous and ring-free (it's the one core guaranteed
to still work if the drainer thread is dead or unscheduled). `kernel_smp_shared`
has no such asymmetric "safe" core — every core is a peer. So a *naive* full
port (every core → its own ring, one drainer thread empties them all, nobody
writes UART directly) would make console output dependent on that drainer
being alive and scheduled. Checked how real that risk is rather than assuming:
read the two places that print kernel-fatal diagnostics, and both terminate in
a loop with **no further scheduling, ever**:

- `#[panic_handler]` (`src/main.rs:144-165`): prints the message via
  `console::print`, then `halt()` → ARM semihosting exit / bare `wfi`. No yield
  after the print.
- The unrecoverable branch of `rust_sync_el1_handler` (`src/exceptions.rs`,
  the true kernel-mode fault path — distinct from the *recoverable* per-process
  `[Fault]`/`[WPF]` dumps this doc's evidence is built on, which the kernel
  keeps running and scheduling after): prints the full register/page-table
  dump via `safe_print!`, then `loop { asm!("wfe") }` forever.

Those are the **only two** call sites that print-then-never-schedule-again.
Everything else — every other `tprint!`/`safe_print!` in normal kernel/syscall
code, *including* the recoverable EL0 `[Fault]` path — keeps scheduling
afterward, so a single drainer thread would flush a ring within one quantum,
same as the existing multikernel doc's own claim. So the risk isn't "a
background thread now gates all debugging output"; it's two specific,
identifiable call sites that need to keep writing the UART directly. A single
drainer thread for everything else is low-risk — closer to the multikernel
design's actual intent than the vaguer "full port" framing first written here.

**Design, not yet implemented:**

1. Add a plain shared per-core `ConsoleRing` array (reuse the existing
   `akuma_smp::ConsoleRing` type — already host-tested, already lock-free SPSC)
   indexed by `current_core_id()` (`crates/akuma-exec/src/bkl.rs:274` — a bare
   `mrs mpidr_el1` read, zero dependencies, safe to call from a fault handler).
2. `emit()` gets a `#[cfg(kernel_smp_shared)]` branch: every core, including
   whichever one is acting as primary, writes to its own ring instead of the
   UART.
3. One drainer thread (same shape as `start_console_drainer`/
   `drain_console_rings`, generalized to read the new array instead of the
   `MachineConfig` descriptor) — `loop { drain_all_rings(); yield_now(); }`.
   Only this thread ever touches `UART.write`, so no lock is needed at all.
4. The two call sites above (`panic()`, and `rust_sync_el1_handler`'s fatal
   branch) keep today's synchronous `irq::with_irqs_disabled` direct-UART-write
   loop, untouched — never routed through the ring.

An alternative for (2)-(4) — a bare spinlock around the existing direct-write
loop instead of a ring — was considered and rejected as the primary plan: it
adds lock contention on the exact hot path (`-j4` logging volume) the ring
avoids entirely, for no benefit now that the drainer-liveness risk turned out
to be two well-identified call sites rather than an open-ended one. Still
worth keeping in mind as a fallback if the ring approach hits an unforeseen
snag.

Not implemented this session — recorded here so the design isn't rediscovered
from scratch next time `-j4` log evidence needs cross-core attribution.

---

## 9. Next steps

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
