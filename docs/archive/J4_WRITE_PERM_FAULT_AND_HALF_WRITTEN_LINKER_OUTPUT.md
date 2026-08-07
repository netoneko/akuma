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
| **D** | an rustc thread-group *leader* (never reaching `exit_group`) parked in `FUTEX_WAIT` almost since its own start | **reproduced with a tight, fast probe — see §7.9.** Two mitigations (direct cross-core wake SGI, periodic revalidation of untimed waits) landed and were re-verified **not** to fix it (§7.10) — new evidence shows the wake is never *issued* on the key at all (the notifying thread never reaches `notify_all`), which no futex-layer mitigation can rescue. §7.12 (2026-08-07, next session): a from-scratch reimplementation of the barrier independently reproduces the identical `hist=uSepuSepuSep` signature, ruling out anything specific to jobserver-rs or std's exact `Barrier` codegen. The leading "never scheduled from birth" theory was tested two ways — a scheduler override forcing never-run threads to the front, and a direct `LAST_CORE` measurement — and **both cleanly disprove it**: every thread in a hung run has run on a real core at least once. Still not root-caused. |

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

> **Confirmed by the §7.9 repro (2026-08-07).** The stuck primitive's observed
> futex op is `a1=0x80` (`FUTEX_WAIT|PRIVATE`, untimed) and the cycling mutex
> waiters are `a1=0x89` (`FUTEX_WAIT_BITSET|PRIVATE`, timed) — both **raw
> futex**, never `FUTEX_REQUEUE`. So Rust std on `aarch64-unknown-linux-musl`
> does use its own raw-futex Condvar (not musl's pthread_cond), and the
> 2026-08-04 `FUTEX_REQUEUE`/`pthread_cond_broadcast` fix is **not** the
> mechanism here. This lead can be retired.

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

## 7.9 §7.8 stress test: POSITIVE — a tight, fast kernel repro (2026-08-07)

**The §7.8 hypothesis was confirmed.** Running the probe under real 4-core SMP
contention reproduces the lost-wake — but only the jobserver-*shape* primitive,
not `futextest_rs`'s 1:1 patterns. This converts Failure D from a ~15-minute
rustc build into a **~25-second deterministic-on-demand repro**.

### The probe

A new focused probe was written: `userspace/selfhost_repro/jobserver_stress.rs`.
It matches jobserver's `HelperState::for_each_request` shape (§7.5) more closely
than `futextest.rs`'s phase 4: a **multi-producer / single-consumer** `Mutex`+
`Condvar` with an **untimed** `cvar.wait()` (the exact `0x3cda5fc4` primitive,
`a1=0x80`), plus a `Barrier` phase (one-to-many wake), a `park`/`unpark` MPSC
phase, and a spawn/join churn phase. Each phase prints `start`/`ok`; a missing
`ok` is the repro. Env knobs (`JS_PHASE`, `JS_PRODUCERS`, `JS_REQUESTS`, …) run
one phase at a time. Cross-compile on host:
`rustc --target aarch64-unknown-linux-musl -C linker=aarch64-linux-musl-gcc -O
userspace/selfhost_repro/jobserver_stress.rs`, or compile in-guest with
`rustc -O jobserver_stress.rs` (the self-host image has rustc).

### The result — and the condition that gates it

| config | outcome |
| --- | --- |
| 4 concurrent `barrier` copies, **no** background load | **0 / 4 hang** |
| 4 concurrent `barrier` copies, **+ 4 CPU hog loops** | **4 / 4 hang** |
| 4 concurrent copies of the **all-phases** probe + 4 hogs | `condvar-mpsc` and `barrier` both hang; `futextest_rs` (1:1) passes all 7 phases |

The gate is **real scheduler preemption pressure**, exactly the condition the
already-fixed 2026-08-05 bug needed (`debug-futex-lost-wakeup.md` §4a: "needs a
*concurrent* waker on another core to land inside it"). Without the CPU hogs the
4 barrier copies never preempt each other at the losing window and complete
cleanly. `futextest_rs` passes regardless — its 1:1 producer/consumer does not
hit the window — which is *why* §5 of that runbook measured it at 95/96 on both
arms of the cross-process-key fix: it cannot reproduce *this* class either.

### The shape, from `[FUTEX-DUMP]` / `[FUTEX-WAKERING]` / `hist=`

Four barrier processes (e.g. tgid 6987–6990), each with **one thread permanently
parked on a Condvar futex word** (a static address, e.g. `0x30338ac0`, identical
across the 4 processes — no ASLR), `a1=0x80` (**untimed** `FUTEX_WAIT|PRIVATE`),
`queued_for` climbing to **220 s** and beyond:

```
key tgid=6987 uaddr=0x30338ac0 waiters=1
  tid=19 bitset=0xffffffff queued_for=220072704us hist=pWuXEpWuXEpWuXEp
key tgid=6988 uaddr=0x30338ac0 waiters=1
  tid=20 ... queued_for=220044945us hist=pWuXEpWuXEpWuXEp
```

- `hist=pWuXEpWuXEpWuXEp` — a **healthy** `Enqueue→park→Wake→unpark→eXit`
  cycle repeating, then a **final `Ep`**: the thread enqueued and parked and
  the wake that should follow (`W`) **never arrives**. (Distinct from §4a's
  `...EpW` — woken but never unparked. Here the wake is never *issued* on this
  key while the waiter is queued.)
- The **Mutex** futex word (`0x30130fa0` / `0x30130fc0`) by contrast **cycles
  normally**: `waiters` comes and goes, `queued_for` stays in the 5–50 s range,
  same healthy `pWuXEp…` hist. The mutex is being acquired/released fine.

### What that shape says about the root cause

1. **Untimed waits hang; timed waits survive.** The stuck threads are all
   `a1=0x80` (untimed). The cycling mutex waiters are `a1=0x89`
   (`FUTEX_WAIT_BITSET`, **timed**). The kernel loses wakes for *both* — but a
   timed wait's deadline (`WAKE_TIMES`) eventually reschedules it, re-running the
   futex value re-check and recovering. An untimed wait (`deadline = u64::MAX`)
   has **no deadline rescue**, so one lost wake strands it forever. This is
   exactly why jobserver's *untimed* `cvar.wait` (§7.5) is the address that
   sticks, and why the bug is invisible on `-j1` (no concurrent waker to land in
   the window) but surfaces under `-j4` + load.

2. **The wake is not reaching the futex layer for this key** (`hist` ends `Ep`,
   no `W`). That is the §7.6 "no wake issued in this namespace" shape, not §4a's
   "issued but dropped by the scheduler". The implication is a chain: the thread
   that should call `Condvar::notify_*` (the barrier's last arrival / the
   jobserver token requester) never reaches the `notify`, because *it* is itself
   parked on an untimed wait whose single wake was lost. Somewhere at the head
   of the chain is a thread whose wake genuinely vanished between
   `futex_do_wake`'s dequeue and `schedule_blocking` returning.

3. **The residual window is in the `schedule_blocking` ↔ `wake_by_handle`
   handshake**, §4a-adjacent. `publish_waiting_and_take_pending_wake` closed the
   entry-check-vs-`WAITING`-store gap under a local IRQ mask + SeqCst pair, and
   that pair is re-verified airtight on paper. A remaining hole is not yet
   identified by reading; the repro is now cheap enough to instrument directly
   (see Next steps).

### A candidate worth testing (not confirmed)

`futex_check_and_enqueue` re-reads the futex word via `copy_from_user_safe`, which
is `__arch_copy_user_memory` (a plain byte load) wrapped in `compiler_fence` — a
**compiler** barrier only, no CPU acquire/`DMB ISH`. The waker's value-change
(Rust std's `Condvar::notify_one` does `fetch_add` then `FUTEX_WAKE`) is **outside**
the `FUTEX_WAITERS` lock. Linux's futex has the same structure and relies on the
hash-bucket lock handoff carrying the happens-before of the userspace store; if
Akuma's spinlock pair or the plain user load doesn't provide the equivalent
ordering under contention, a waiter can re-read a stale value, re-enqueue, and
park past a wake that already ran. **Not proven** — the intermittent,
preemption-gated nature fits a timing window better than a consistent ordering
bug, and the lock structure mirrors Linux. Test it by making the value load
acquire-ordered and re-running the repro; if the hang rate drops to zero, this
was it.

### A separate bug found along the way: `alarm()` / `SIGALRM` does not work

`alarm(3)` followed by `pause()` ran past 12 s and did not terminate — `alarm()`
is either not arming the timer or `SIGALRM` is not being delivered. This means
**any probe that relies on `alarm()` for a self-kill timeout will hang forever on
a lost wake** instead of timing out (the `jobserver_stress` probe's
`JS_TIMEOUT_SECS` is inert in-guest); the host/guest-side stress harnesses here
work around it with an external `kill -9` watchdog. Worth its own investigation;
recorded so the next session doesn't build a timeout on top of `alarm()`.

### Reproduce it (deterministic, ~25 s)

Boot `release-smp-shared` + `devbox-smoltcp,no-tests`, `SMP=4`, `MEMORY=4096`,
`DISK=disk_selfhost_fixtest.img`. SSH in, then:

```sh
# (one time) compile the probe in-guest, or scp the host cross-build
rustc -O /tmp/jobserver_stress.rs -o /tmp/jobserver_stress   # if source is present
# 4 CPU hogs are REQUIRED — without them the window is never hit (0/4 hang)
hog() { while :; do :; done; }
for h in 1 2 3 4; do hog & done; HOGS=$!
for i in 1 2 3 4; do
  JS_PHASE=barrier JS_BARRIER_THREADS=4 JS_BARRIER_ROUNDS=8000 \
    /tmp/jobserver_stress > /tmp/r$i.log 2>&1 &
done
sleep 25
for i in 1 2 3 4; do grep -q DONE /tmp/r$i.log && echo "$i PASS" || echo "$i HANG"; done
kill -9 $HOGS
```

Expect `HANG` on all 4 copies. `[FUTEX-DUMP]` (within 60 s) shows the stuck
untimed condvar waiters; the cycling mutex waiters are the control. The guest
helper scripts used this session are at `/tmp/repro_hog.sh`, `/tmp/narrow.sh`,
`/tmp/stress_futex.sh` (not committed — recreate from this section).

### 7.10 Two mitigations landed, neither fixes the repro (2026-08-07, next session)

Two changes were left uncommitted at handoff: `wake_core` (a direct scheduler
SGI to a woken thread's last-known core, `crates/akuma-exec/src/threading/mod.rs`
`ThreadWaker::wake` + `src/smp_shared.rs`) and `FUTEX_REVALIDATE_US` (a periodic
bounded re-park for untimed waits, `src/syscall/sync.rs::sys_futex`, tried at
5 ms/50 ms/200 ms). **Both were re-verified against the exact §7.9 repro on a
freshly rebuilt tree and neither closes it — `hangs=4` at every revalidation
interval tried, with `wake_core` also active.** (The tree didn't even build at
handoff — an unrelated `setitimer` patch landed in the same uncommitted diff had
`copy_to_user_safe`'s `dst`/`src` args swapped; fixed, see §7.11.)

`[FUTEX-DUMP]` after a hung run shows the revalidation safety net **is** firing
as designed — the 4 stuck condvar waiters (`0x30338ac0` in this build) show
`hist=uSepuSepuSepuSep`, a clean repeating **u**npark→**S**elf-remove→r**e**-enqueue→**p**ark
cycle from the periodic re-park — but every cycle ends `p`, never `W`. Per
[`../runbooks/debug-futex-lost-wakeup.md`](../runbooks/debug-futex-lost-wakeup.md)
§2, "no entry for that key at all" reading: **no wake is ever issued on that key**,
which the revalidation safety net cannot rescue by construction — it re-checks
the futex *value*, and the value never changes because nothing ever calls
`notify_all`. This is exactly the §7.9 point 2 hypothesis ("the thread that
should call `notify_*` never reaches it because it is itself parked on a lost
wake") and rules out "the mitigations just need tuning" — the fix has to be
upstream of both `sys_futex` and the scheduler wake path, in whatever the
*notifying* thread of each barrier is blocked on. **Next step: instrument which
thread in each stuck process (`tid` in the same `tgid`, not shown queued on
`0x30338ac0`) is the one due to call `notify_all`, and find what it's actually
doing** — the `[THR-DUMP]` block for one stuck tgid this session showed 4 other
threads per process either mid-syscall (`sc=-1` between calls) or `st=?`
(WAITING) on the barrier's own `Mutex` word (`0x30130fa0`, healthy
`pWuXEpWuXEp` cycle) — none of them looked obviously wedged from a single
snapshot, so this needs the same kind of tight, repeatable probe §7.8 built,
not another read of one dump.

### 7.11 Two real, unrelated bugs fixed this session (2026-08-07)

Neither touches the Failure D root cause above, but both were flagged in §7.9
as open and are now closed:

1. **Build break**: `sys_setitimer`'s old-value write (`src/syscall/time.rs`)
   called `copy_to_user_safe(dst, src, len)` with `dst`/`src` swapped —
   `(&raw const old).cast::<u8>()` (a `*const u8`, meant to be `src`) passed
   where `dst: *mut u8` is expected. Didn't type-check; the tree was in this
   state at handoff. Fixed by swapping the two arguments back.
2. **`alarm()`/`pause()` now works** (§7.9 flagged this as broken and "worth
   its own investigation" — it's a real, three-part bug, unrelated to the
   scheduler/futex investigation above):
   - `sys_ppoll` (`src/syscall/poll.rs`) short-circuited `nfds == 0` to an
     immediate `return 0`. Linux's `ppoll(NULL, 0, timeout, sigmask)` is the
     standard idiom musl's `pause()` compiles down to, and it's supposed to
     *block* until the timeout or a signal — the short-circuit made `pause()`
     return instantly instead of blocking at all.
   - Once that was fixed to actually block, `sys_ppoll`'s loop was still
     missing the `should_interrupt_blocking_syscall()` check that its sibling
     `sys_epoll_pwait` already has a few functions up — so an infinite
     `ppoll(NULL, 0, NULL, ...)` had no way to ever return: no fds means
     `ready_count` never exceeds zero, and `infinite=true` means the timeout
     branch never fires either. Added the same check `sys_nanosleep` and
     `sys_epoll_pwait` already use.
   - Even with both fixed, `alarm(3); pause();` still hung. `check_itimers`
     (`src/syscall/time.rs`, added this session to drive `ITIMER_REAL`) pends
     SIGALRM with plain `pend_signal_for_thread`, but `should_interrupt_
     blocking_syscall`'s handler-aware half
     (`current_thread_has_pending_interrupt`, `crates/akuma-exec/src/process/
     children.rs`) deliberately does **not** treat a handler-less
     default-disposition signal as interrupt-worthy — its own doc comment
     explains why: `sys_tkill` is supposed to apply a fatal default action
     *inline*, at send time, in the sender's own context, so a "merely
     pending" default-fatal signal is assumed to mean "blocked, wait for the
     mask to lift." `check_itimers` runs in **timer-IRQ context on whichever
     core took the interrupt** — "current process" there is whatever got
     interrupted, not the itimer's owner, so it structurally cannot apply the
     fatal action inline the way `sys_tkill` does. It has to follow `sys_kill`'s
     pattern instead: set the target's `ProcessChannel::interrupted` flag
     (`is_current_interrupted()`, the *unconditional* first half of
     `should_interrupt_blocking_syscall`, checked regardless of disposition)
     so the blocking syscall gives up with `EINTR`, and let the signal's
     actual default action apply safely at the target thread's own next
     syscall-return dispatch (`src/exceptions.rs`, `take_pending_signal`),
     where "current process" is correct again. But the existing
     `interrupt_thread(tid)` helper alone wasn't sufficient either: it sets
     the flag via `get_channel`/`PROCESS_CHANNELS[tid]`, a map populated once
     at the *original* spawn point and never re-registered per fork/exec —
     while what the target will actually read back is `current_channel()`,
     which tries `Process::channel` (inherited by value through every fork,
     so it survives arbitrarily many fork/exec generations) *first*. A
     process a few generations removed from the original registration point
     — e.g. anything an SSH session execs — has a populated `Process::channel`
     but no `PROCESS_CHANNELS[tid]` entry, so `interrupt_thread` alone
     silently no-ops. Fixed by also setting `Process::channel`'s flag
     directly via `find_pid_by_thread` + `lookup_process_shared`. Verified
     end-to-end in-VM: `alarm(3); pause();` now blocks ~3.1 s and is killed
     with `Alarm clock` / exit 142, and `alarm(2)` interrupting a
     `nanosleep`-based loop (which happened to already work, via the
     short-sleep-then-syscall-return path rather than this one) still does.
   - This same `PROCESS_CHANNELS`-vs-`Process::channel` divergence likely
     affects `sys_kill`'s existing `interrupt_thread` calls too, for any
     target process several fork/exec generations from where its channel was
     registered — not fixed there, out of scope here, but worth knowing if
     Ctrl-C/`kill` is ever seen not reaching a deeply-nested child.

---

## 7.12 A from-scratch reproduction, and the "never scheduled" theory cleanly disproven (2026-08-07, next-next session)

Picked up §7.10's own next step: instrument which thread in a stuck process is
the one due to call `notify_all`, and find out what it's actually doing.
**Result: not root-caused, but one specific, plausible theory is now closed
by direct measurement, and two reusable diagnostics were added.**

### The probe: a from-scratch barrier, not `std::sync::Barrier`

`userspace/forktest/selfhost_repro/jobserver_stress.rs`'s `phase_barrier` was
rewritten around a hand-rolled `InstrBarrier` — the exact same algorithm
`std::sync::Barrier` uses internally (`Mutex<{count, generation}>` + `Condvar`,
`wait_while`) — so it could be instrumented from the inside: per-thread step
atomics (locking / evaluating leader-vs-follower / about-to-notify /
notified / about-to-wait / woken), a shadow copy of the mutex-protected
state for lock-free reading, and a watchdog thread that dumps state on a
stall. Reusing the **identical algorithm** (not just "a barrier") matters:
if this reproduces the same hang, it rules out anything specific to
jobserver-rs's call pattern or std's exact `Barrier` codegen — the bug is in
the primitive shape itself (Mutex+Condvar+generation under SMP contention),
confirmed below.

The watchdog itself turned out to be an unreliable narrator — see "A blind
spot in the probe" below — so its own `[barrier-instr] STUCK` dump never
fired even across an 800+ second hang. Recorded so the next session doesn't
rebuild the same watchdog design expecting it to work; the kernel's own
`[FUTEX-DUMP]`/`[THR-DUMP]` turned out to be the reliable source of truth
throughout, which is itself a useful lesson (§7.6's runbook is right that a
userspace probe can be starved by the exact thing it's trying to observe).

### Reproduced cleanly: the exact §7.10 signature, independent of jobserver-rs

4 concurrent copies (`JS_PHASE=barrier JS_BARRIER_THREADS=4
JS_BARRIER_ROUNDS=8000`) + 4 CPU hogs, `release-smp-shared` + `SMP=4`, same as
§7.9's recipe. `[FUTEX-DUMP]` on a hung run:

```
key tgid=92 uaddr=0x1088bad8 waiters=1
  tid=21 queued_for=43155us hist=uSepuSepuSepuSepuSep
```

Identical to §7.10's `hist=uSepuSepuSep...`: the periodic-revalidation safety
net cycling exactly as designed (unpark → self-remove → re-enqueue → park),
never once seeing a real `W`. Confirmed again: the futex value never changes
because nothing ever calls `notify_all` — this is not a wake lost in transit,
it's a wake never issued.

### Theory: "some newly-cloned threads are starved from birth" — tested two ways, both negative

`THR-DUMP`'s `elr=` (`crates/akuma-exec/src/threading/mod.rs`,
`dump_thread_resume_points` — reads `ctx.elr` from `get_context(tid)`, the
**scheduler's saved-context slot**, not the live syscall trap frame) showed
several threads per stuck process frozen at the exact same address across
every 30 s sample for 200+ seconds. Symbolized against the unstripped probe
binary (`aarch64-linux-musl-addr2line`/`objdump`), that address is
`__clone+0x20` — literally the `cbz x0, ...` right after the `clone()`
syscall returns, before the child has executed a single instruction of its
actual thread body. A tempting read: these threads were created and then
never scheduled again, ever — which would also tidily explain why the
probe's own watchdog thread (itself freshly `clone()`d the same way) never
ran its polling loop even once.

`docs/reference/subsystems/thread-lifecycle.md` "Open ⚠ leaf traces" (§5.3b)
independently documents an **open, unresolved** bug in exactly this territory
— a `clone_thread` child reading stale/already-freed memory on its first
instruction — which made the theory look more credible, not less. But that
bug manifests as a visible `[Fault] SIGSEGV in clone_thread`, and this run
had **zero** `[Fault]` lines total — so if related, it isn't the same
manifestation.

**Tested directly, two ways, both cleanly negative:**

1. **Falsification via intervention.** Added `config::PRIORITIZE_NEVER_SCHEDULED`
   (`src/config.rs`, wired through `ExecConfig`/`schedule_indices` in
   `crates/akuma-exec/src/threading/mod.rs`): when true, the scheduler
   unconditionally prefers any `READY` thread with `LAST_CORE == 0xFF` (the
   existing "never scheduled" sentinel, already correctly scrubbed on slot
   claim — see thread-lifecycle.md §1's `scrub_thread_slot` note) over the
   wakeup-locality hint and the normal round-robin scan. Rebuilt, rebooted,
   re-ran the identical repro: **same outcome** — all 4 processes still died
   via the `JS_TIMEOUT_SECS` `SIGALRM` (`code=-14`) at the same ~90–120 s
   mark as the unfixed baseline. If any thread were genuinely starved from
   birth, this override would have caught and run it on the very next
   scheduling decision (many times a second under this load); it made no
   difference at all.
2. **Direct measurement.** Added `last_core=` to every `[THR-DUMP]` line
   (cheap, read-only, kept permanently). Re-ran the repro with the scheduler
   unmodified and captured a full dump mid-hang: **every single alive
   thread — including every thread frozen at the `__clone+0x20` address, and
   every thread parked on the barrier's own Condvar — showed a real core
   number (0–3), never `255`.** Every thread has run at least once. The
   "never scheduled from birth" theory is closed by direct evidence, not
   inference.

Net effect: `ctx.elr` staying fixed at `__clone+0x20` for 200+ seconds does
**not** mean "never ran" — it apparently isn't refreshed by every
scheduling-out path (voluntary vs. involuntary/timer-preempted context
switches may differ here; not yet investigated, and not needed to close this
theory). Symbolizing a static `elr` sample is not sufficient evidence of a
stuck thread on its own; `last_core=` (or better, a monotonic
"scheduled-out count" if a future session wants finer resolution) is the
thing to trust.

### Two reusable diagnostics left in the tree (both off/inert by default)

- **`last_core=` in `[THR-DUMP]`** — permanent, zero-cost, answers "was this
  thread ever scheduled" directly instead of by ELR inference.
- **`config::PRIORITIZE_NEVER_SCHEDULED`** (default `false`) — a targeted
  scheduler override for bisecting "starvation" vs. "genuine logic/wake bug"
  in any future SMP hang; flip to `true` for one A/B, not meant to ship on.

### A side finding, not investigated further: userspace sshd wedges under a connection burst

While uploading the probe binary, ~90 rapid successive SSH connections (one
per `echo '<b64-chunk>' >> file`, a chunking approach abandoned in favor of a
single-connection `base64 -d` piped over stdin — see "Method notes" below)
left the guest's userspace `sshd` (`/bin/sshd`, from `herd`) completely
wedged: `[PSTATS]` showed its syscall count frozen solid across three
consecutive 30 s samples (`nanosleep`/`accept` counts identical at T210,
T240, T270), with **zero** entries in `[FUTEX-DUMP]` at the same time
(`table empty`) — so whatever wedged it, it wasn't parked in a tracked futex.
Not reproduced deliberately or investigated; recorded because it's a real,
apparently deterministic wedge (happened both times a rapid connection burst
was attempted) that would bite anyone SSHing into this image aggressively.
Worth its own session if it recurs.

### Method notes for the next session

- **Cross-compile, don't build in-guest, for a probe this size.** `rustc
  --target aarch64-unknown-linux-musl -C linker=aarch64-linux-musl-gcc -O`
  on the host; `aarch64-linux-musl-{gcc,strip,nm,objdump,addr2line}` are all
  present via Homebrew.
- **Strip before uploading.** The unoptimized `-O` static build is ~4.7 MB;
  `aarch64-linux-musl-strip` gets it to ~530 KB. Transferring the unstripped
  binary as a single base64 blob over this guest's SSH implementation was
  observed to take minutes for ~6 MB of base64 (SSH channel throughput here
  is slow for bulk data — not investigated further, just budget for it or
  strip first). **Keep the unstripped copy on the host** — `addr2line`/`nm`
  need it and the guest never does.
- **`busybox sh`'s `ash` has no `env` builtin.** `env FOO=bar cmd` fails with
  `env: not found` (exit 127) and silently produces a `busybox.static`
  `[PROC-EXIT] ... code=127` line that looks like the target program itself
  exited abnormally, not like a shell builtin error — costs real time to
  notice. Use plain `FOO=bar cmd` (POSIX var-prefix assignment, no `env`
  needed) instead.
- **A burst of many quick SSH connections in a tight loop is itself
  dangerous** — see the sshd wedge above. Prefer one persistent connection
  (pipe a whole payload over stdin) to N short-lived ones.
- **`[THR-DUMP]`'s `elr=` is the scheduler's saved-context slot
  (`get_context(tid)`), not the live syscall trap frame** (that's `a0`/`a1`,
  read separately from `CURRENT_TRAP_FRAME`). Don't conflate a static `elr`
  sample with "never ran again" without corroborating it against
  `last_core=` or CPU-time accounting first — see above.
- **GDB is available and was not used this session, but should be next
  time.** `GDB=1`/`GDB_WAIT=1` exposes a gdbstub on `:1234`
  (`scripts/cargo_runner.sh`). Since the kernel ELF (unlike the stripped
  probe) still has full debug symbols, GDB can read `THREAD_STATES`,
  `THREAD_CONTEXTS`, `LAST_CORE`, `FUTEX_WAITERS`, etc. directly out of live
  guest memory without adding a new `tprint!`/rebuild/reboot cycle per
  hypothesis — likely much faster iteration than this session's
  print-then-rebuild-then-reboot loop (~2 minutes per iteration) for
  whatever comes next.

### Next steps

The "never scheduled" theory being closed narrows, not widens, the mystery:
every participant thread runs at least sometimes, yet the barrier still
deadlocks with the exact "wake never issued" signature. Candidates left
standing, in roughly the order §7.9/§7.10 already reasoned through them:

1. The arrival-count increment itself is lost under contention for a
   specific thread (it never becomes the 4th/last arrival, or two threads
   race and one's increment is clobbered) — would need instrumenting the
   `Mutex`-guarded `count`/`generation` fields directly, ideally via GDB
   watchpoints or a kernel-side value-change tripwire rather than another
   userspace probe (given this session's watchdog-reliability lesson).
2. A thread does reach the leader branch and calls `notify_all`
   (`FUTEX_WAKE`), and *that syscall itself* doesn't complete — test by
   adding a kernel-side tripwire in `sys_futex`'s `FUTEX_WAKE` arm that logs
   entry separately from the existing completion-only `tsc`/`[futex-dbg]`
   tracing, so an entered-but-never-exited wake call would be visible for
   the first time.
3. `docs/reference/subsystems/thread-lifecycle.md` §5.3b's open
   `clone_thread`-reads-stale-memory bug is still an open, uninvestigated
   possibility for a *quieter* (non-crashing) manifestation than its known
   crash — not ruled in or out this session, since zero `[Fault]` lines
   doesn't rule out a variant that corrupts state without a hard fault.

### 7.13 A live GDB cross-check, and one more loose end for next time (2026-08-07)

`aarch64-elf-gdb` (`brew install aarch64-elf-gdb`) attaches to the existing
`GDB=1` gdbstub (`scripts/cargo_runner.sh`, `:1234`) fine, but this build has
**no DWARF debug info** (`release-smp-shared` inherits `release`, no
`debug = true`) — `print <symbol>` fails with "No symbol table is loaded"
even though `nm`/`objdump` (symtab-only) work and PC values do resolve to
function names. Use raw address arithmetic against `nm` output instead:
`aarch64-elf-nm target/aarch64-unknown-none/release-smp-shared/akuma | grep
THREAD_CONTEXTS` etc., then `x/Nxg <addr>` in gdb. **Do not run bare `bt`** —
with no CFI/`.debug_frame` info gdb's frame-pointer heuristic produced 7000+
duplicate garbage frames before anything useful; stick to `x`/`info
registers`/`info symbol`.

Cross-checked `THREAD_CONTEXTS[30]` (the tid `[FUTEX-DUMP]` named as the
`0x1088bad8`-condvar waiter) directly from live guest memory: `LAST_CORE=0`,
`TOTAL_CPU_TIMES=0x4d103` (≈317 ms of real accumulated run time — corroborates
§7.12's `last_core=`/`cpu_us=` finding independently, via a completely
different tool), and `Context.elr=0x100da6e8` (matches `[THR-DUMP]` exactly,
confirming the offset math). But `Context.x30=thread_start_closure` — the
*kernel* resume point (see the `get_saved_kernel_resume` doc comment,
`crates/akuma-exec/src/threading/mod.rs`) still sits at the very first
instruction of thread bootstrap, before `blr x19` even calls into the actual
closure. `x30`/`elr` are written together in exactly three places, all
one-time bootstrap init (`crates/akuma-exec/src/threading/mod.rs:1310-1311,
3869-3870, 3985-3986`, all `ctx.x30 = ctx.elr = thread_start_closure as
*const () as u64`) plus one `elr`-only site (`update_thread_context`,
`:3250`, fork/execve-specific — not relevant to a `thread::spawn` worker).
No Rust-level write updates `x30` for a normal cooperative park/resume, so
the actual save/restore for `schedule_blocking`'s context switch almost
certainly happens in hand-written assembly outside grep's reach here — that
asm routine (find it via `commit_switch`'s callers / the vector table) is
the concrete next thing to read, not something to infer from this session's
data. Whether it's writing `x30` correctly for this class of thread and
`elr` is the field lagging, or the reverse, decides whether "frozen `x30`"
means anything at all — genuinely open, not concluded either way.

### 7.14 Resolved: `Context.x30`/`.elr` are dead fields after thread creation — `get_saved_kernel_resume`'s doc comment is stale

§7.13's loose end, closed by reading the actual switch handler. Not
`commit_switch` (that's `ThreadPool`'s bookkeeping helper, called from
inside the handler below) — the real entry point for **every** context
switch, voluntary (`schedule_blocking`'s self-SGI) and involuntary (timer
preemption) alike, is `sgi_scheduler_handler_with_sp`
(`crates/akuma-exec/src/threading/mod.rs:3026`, invoked from
`rust_irq_handler_with_sp`, `src/exceptions.rs:1858`). Its entire outgoing-
context save is two lines:

```rust
(*old_ctx).sp = current_sp;
...
(*old_ctx).ttbr0 = current_ttbr0;
```

That's it. No write to `.elr`, `.x30`, or any of `x19`-`x29` — ever, for any
switch, for the rest of a thread's life. Grepping the whole file for `.elr`
confirms it: only 4 write sites total, and none of them run for a plain
`clone_thread` (`std::thread::spawn`) worker —

- 3 identical one-time bootstrap sites (`:1310-1311`, `:3869-3870`,
  `:3985-3986`), all `ctx.x30 = ctx.elr = thread_start_closure as *const
  () as u64` — run once, when a slot is first claimed, to give the very
  first cooperative switch *into* a brand-new thread somewhere valid to jump
  to.
- 1 site in `update_thread_context` (`:3250`, `ctx.elr = user_context.pc`
  only, `.x30` untouched) — but that function's only 3 callers
  (`crates/akuma-exec/src/process/mod.rs:2803,2958,3235`) are all in the
  fork-a-new-process family (copies `parent_tid`'s sigaltstack, registers a
  fresh `child_pid` in `THREAD_PID_MAP`), not `clone_thread`.

So for any ordinary worker thread, `Context.x30`/`.elr` are written once at
creation and **never again** — confirming §7.13's `x30=thread_start_closure`
finding is not a hang symptom, it's simply what every thread's `x30` shows
forever after its first switch, hung or not. The *real*, live register state
for a switched-out thread — including the actual current `ELR_EL1` — lives
on its own kernel stack, in the 832-byte IRQ frame `Context.sp` points to
(`src/exceptions.rs:266-278`: fixed layout, `ELR` at `sp+240`, `SPSR` at
`sp+248`, confirmed by the two live tripwires already reading those same
offsets at `:1871` and `:3161`). `get_saved_kernel_resume`'s doc comment
("`x30` is where it will resume in kernel code") describes a design this
codebase no longer implements — stale documentation, not a bug in the
switch logic itself. `[THR-DUMP]`'s `elr=` field reads this same dead
`Context.elr`, so **every `elr=` value this doc (and every prior session's
`[THR-DUMP]` reading, including the original §3 write-permission-fault
diagnosis and every `elr=0x...` cited throughout §7) has ever shown for a
non-freshly-forked thread is this stale, frozen-since-creation value, not a
live resume point.** That doesn't invalidate conclusions drawn from `a0`/`a1`
(read from the separately-tracked, genuinely live `CURRENT_TRAP_FRAME`) or
from `[FUTEX-DUMP]`'s own bookkeeping — those are unaffected — but any
reasoning that leaned on a *specific* `elr=` value meaning "this is where
the thread currently is" should be revisited.

**One open loose end, not resolved:** §7.13's raw memory dump showed `.elr`
(`0x100da6e8`) genuinely differing from `.x30` (`thread_start_closure`) for
`tid=30` — but per the 4-site accounting above, both are only ever written
*together*, to the *same* value, for a plain `clone_thread` worker. Nothing
found this session explains how they diverged. Either a fifth write site
exists that grep missed (worth a `.elr\s*=` regex sweep with different
whitespace, or a search for raw pointer writes bypassing named-field syntax
entirely), or the live GDB read itself raced an update mid-dump in a way
that produced a misleading snapshot. Not chased further — flagging so the
next session doesn't treat this specific pairing as fully explained.

**Fix worth making, not done this session:** point `[THR-DUMP]`'s `elr=`
(and `get_saved_kernel_resume`) at `*(u64*)(Context.sp + 240)` instead of
`Context.elr`, and correct or remove the stale doc comment. Low risk (read-
only diagnostic code), but wasn't done here since it needs its own
rebuild+reboot+repro cycle to verify against a live hang, and this session's
budget was already spent confirming the *cause* of the staleness rather than
shipping the fix.

### 7.15 The `elr=` fix landed and verified live — threads are moving, none caught inside `notify_all` (2026-08-07, next session)

Made the fix §7.14 specified: `dump_thread_resume_points` and
`get_saved_kernel_resume` (`crates/akuma-exec/src/threading/mod.rs`, ~4198 and
~4284) now read `*(u64*)(Context.sp + 240)` (guarded: `0` if `Context.sp == 0`,
i.e. never yet switched out via the IRQ path) instead of the dead
`Context.elr`. Rebuilt (`release-smp-shared` + `devbox-smoltcp,no-tests`),
booted fresh on `disk_selfhost_fixtest.img`, re-ran the exact §7.9/§7.12 repro
(4× `jobserver_stress barrier`, `JS_BARRIER_THREADS=4 JS_BARRIER_ROUNDS=8000
JS_TIMEOUT_SECS=90`, + 4 CPU hogs, `SMP=4 MEMORY=4096`).

**The fix works as designed.** `elr=` values are no longer a single frozen
`__clone+0x20` — they're varied, and symbolize to real, distinct functions
(`aarch64-elf-addr2line -f -e target/.../akuma <addr>`):

| `elr=` | Symbol | Meaning |
| --- | --- | --- |
| `0x40136c5c` | `gic::trigger_sgi_self` | voluntary yield into `schedule_blocking` — the common "genuinely parked" PC |
| `0x401ba81c` | `secondary_shared_start` | idle-core spin loop (tid 1–3) |
| `0x4012a13c` | `rust_sync_el0_handler` | mid syscall-entry dispatch |
| `0x401a74f4` / `0x401a64e8` | `handle_syscall` | mid generic syscall dispatch |
| `0x4021ecb4` | `akuma_exec::process::children::read_current_pid` | mid a kernel helper call, not a wait primitive at all |

Reproduced the hang cleanly (`[FUTEX-DUMP]` showed the identical §7.9/§7.12
signature: `tid=16,17,18,19` — one per `tgid` 42–45 — parked on
`uaddr=0x3053cac0 a1=0x80`, `hist=uSepuSepuSep...`, `queued_for` climbing past
140 s). Two `[THR-DUMP]` snapshots were captured 30 s apart (T210 and T240) for
the same tids while the hang was in progress, to test the fix's real
discriminating power — a frozen PC across both samples means genuinely stuck,
a changed PC means real forward progress:

- **The four `[FUTEX-DUMP]`-named waiters (`tid=16,17,18,19`) showed
  `elr=0x40136c5c` (`trigger_sgi_self`) in *both* samples, unchanged.** This
  independently corroborates `[FUTEX-DUMP]`'s own bookkeeping via a completely
  different mechanism (a raw memory read of the saved IRQ frame, not the futex
  table): these threads really are sitting in the ordinary voluntary-park path,
  not lying dormant somewhere else. Not a new fact, but the first time it's
  been confirmed by something other than the futex subsystem's own accounting.
- **Other threads in the same `tgid`s moved between the two samples** — e.g.
  `tid=30` (`tgid=44`) went `rust_sync_el0_handler` → `handle_syscall`;
  `tid=33` (`tgid=45`) went `trigger_sgi_self` → `read_current_pid`; `tid=22`
  (`tgid=42`) went from mid a timed `FUTEX_WAIT_BITSET` on the barrier's own
  Mutex word (`0x10070f58`, `tsc=98`) to fully back in userspace (`tsc=-1`,
  `sc=-1`, state `R`, no live syscall at all). These are real state
  transitions, not sampling noise — direct confirmation (via a third,
  independent mechanism, after §7.12's `last_core=`/`cpu_us=` and §7.13's GDB
  cross-check) that participant threads in a hung barrier process are not
  wedged as a group. Only the specific thread each `[FUTEX-DUMP]` key names is
  actually stuck; its siblings keep working (contending for the Mutex,
  re-entering syscalls, returning to userspace) around it.
- **No sample caught any thread's live PC inside `Condvar::notify_all` /
  `pthread_cond_broadcast` / the barrier's leader branch, or inside
  `sys_futex`'s `FUTEX_WAKE` arm.** Expected from only two 30-second-spaced
  snapshots against what should be a fast call — this doesn't rule that thread
  out, it just means point-in-time sampling is the wrong tool to catch it. The
  §7.12 next-steps item 2 (a kernel-side tripwire logging `FUTEX_WAKE` *entry*,
  not just its existing completion-only trace) is still the right instrument
  for this, and is now more clearly the next step than reading more `elr=`
  snapshots would be.

**A separate, unplanned finding, not investigated:** all 4 probe processes
this run also logged a single spawned-thread panic each (different `tid` each
time: 20, 21, 11, 32) — `` thread '<unnamed>' (N) panicked ... assertion `left
== right` failed: left: 38, right: 4 `` at `library/std/src/sys/thread/
unix.rs:581:17` of the in-guest `/usr/bin/rustc`'s bundled std (commit
`31fca3adb283cc9dfd56b49cdee9a96eb9c96ffd` — did not match either locally
installed host toolchain's `unix.rs:581`, which is unrelated code in both, so
the line can't be resolved without that exact std source). Did **not** affect
the outcome: a panic in a non-main spawned thread only kills that thread by
Rust's default unwind semantics, and `[FUTEX-DUMP]` still showed the barrier
threads parked normally throughout. All 4 processes still self-terminated
correctly at `T294.77`–`T294.99` via `code=-14` (`SIGALRM`,
`JS_TIMEOUT_SECS=90`) — an independent re-confirmation that §7.11's
`alarm()`/`SIGALRM` fix still holds under this exact repro. Flagged so a
future session recognizes the panic message and doesn't mistake it for part of
the barrier bug; not chased further (this session compiled the probe
**in-guest**, per the method notes, which pulled in whatever std the disk
image's `/usr/bin/rustc` bundles — not necessarily the same std a host
cross-compile would link, so this may be specific to that toolchain and not
reproducible via the host cross-compile path prior sessions used).

**Net effect on the root-cause search:** narrows further, doesn't close it.
"Some thread is stuck somewhere unexpected" and "the whole process is wedged"
are now both ruled out by direct, trustworthy PC evidence — only the specific
condvar-waiting thread per process is actually parked; everyone else is live.
The remaining candidates are exactly §7.12's numbered list: (1) the
arrival-count increment lost under contention, or (2) a `notify_all` call
entered but never completing inside the kernel. Point-in-time `elr=` sampling,
now trustworthy, has done what it can — next session should build the
`sys_futex` `FUTEX_WAKE`-entry tripwire (§7.12 item 2) rather than take more
snapshots.

### 7.16 §7.12's two remaining candidates narrowed further by reading, not sampling (2026-08-07, same session)

Read the actual futex kernel code (`src/syscall/sync.rs`) end to end rather
than adding more instrumentation, to test the two candidates §7.12 left
standing directly against the implementation.

**The "missing acquire barrier" theory (§7.9's "candidate worth testing") is
now considered unlikely, not confirmed dead.** `FUTEX_WAITERS` is a
`spinning_top::Spinlock` (`src/syscall/sync.rs:49`) — a well-audited external
crate, not a hand-rolled one, and it provides real acquire/release ordering on
`lock()`/`unlock()`, not just mutual exclusion. Reasoned through the actual
protocol both `futex_check_and_enqueue` (waiter) and `futex_do_wake` (waker)
follow under that lock: whichever of the two acquires `FUTEX_WAITERS` first
is guaranteed (by the lock's own ordering) to have its effects visible to
the other once it acquires the lock second — either the waiter sees the
waker's new value and returns `EAGAIN` without enqueuing, or the waker's
search (acquired after) finds the waiter still queued and wakes it. This
protocol doesn't depend on `copy_from_user_safe`'s compiler-fence-only
barrier (§7.9's specific suspect) for correctness, because the *lock itself*
is what's supposed to carry the ordering — not proof positive (not
re-verified against the actual `spinning_top` source), but the theory no
longer looks like the leading suspect it did in §7.9.

**A concrete, checkable third candidate — a per-thread tgid-key mismatch
sending a wake to the wrong bucket — was tested and ruled out.** `futex_key_tgid`
resolves identity per-thread via `read_current_pid()` → `THREAD_PID_MAP` →
that pid's `Process.tgid`
(`crates/akuma-exec/src/process/children.rs:366-432`). If one thread in a
barrier's process resolved a *different* tgid than its siblings (plausible in
the abstract: `clone_thread` gives every new worker thread its **own**,
uniquely-allocated `child_pid` — `crates/akuma-exec/src/process/mod.rs:3128,
3246` — and `read_current_pid` looks that up through `THREAD_PID_MAP` and then
reads `.tgid` off of *that* pid's `Process` struct, rather than everyone
sharing one canonical identity directly), its futex ops would land in a
different `(tgid, uaddr)` bucket than its siblings' — exactly reproducing
"wake never reaches this key" with nobody actually stuck on anything. Both of
the two paths that degrade to a *wrong* key already have their own counters
and log lines (`FUTEX_KEY_DEGRADED_TO_SHARED` for `read_current_pid()`
returning `None`; `TGID_RESOLVE_MISSES` / `[identity] WARNING: THREAD_PID_MAP
pid not ACTIVE...` for a `THREAD_PID_MAP` entry naming a retired pid) — grepped
the full boot log from the §7.15 hang for both: **zero hits, either one.**
Ruled out for this run. (Does not rule out a *third*, undiagnosed
mis-resolution path with no counter — not found, but not exhaustively
searched either.)

**Re-derived a sharper form of §7.9 point 2 that narrows between the two
remaining candidates.** The periodic revalidation safety net
(`FUTEX_REVALIDATE_US = 200_000`, `src/syscall/sync.rs:846`) re-checks the
*exact* futex word every 200ms using the *same* captured `val` for the whole
untimed wait (never refreshed mid-loop — only a genuine wake, signal, or a
changed value ends the loop; there is no timeout branch for `deadline ==
u64::MAX`, confirmed by reading the loop directly). `Condvar::notify_all()`'s
first action, in every futex-based `std::sync::Condvar` implementation, is a
plain userspace atomic `fetch_add` on its own internal epoch — synchronous,
requires no kernel involvement, and happens *before* the `FUTEX_WAKE` syscall
is even issued. That means: if `notify_all()` is ever called at all —
**even if its subsequent `FUTEX_WAKE` syscall then hung or never
returned** — the epoch bump would already be visible in memory, and the
waiter's very next 200ms revalidation would observe it and return `EAGAIN`
(unblocking, regardless of the wake syscall's own fate). Over the 140-200+
second hangs observed (700+ revalidation cycles), on cache-coherent SMP
hardware, "the value never changes" cannot be explained by a slow-to-propagate
write — coherency doesn't hide a write forever, only delay when it's
observed, and 200ms × hundreds of cycles is not a delay any real coherency
protocol produces. **This converts §7.9 point 2 from "maybe issued, wake
lost" to "very likely never called at all"** — i.e. candidate 2
(`FUTEX_WAKE` entered but not completing) is now the *less* likely of
§7.12's two remaining candidates, and candidate 1 (the arrival count itself
never reaches `n`) is the one worth instrumenting next. Caveat: this argument
assumes QEMU's simulated ARM SMP is genuinely cache-coherent for a plain
atomic RMW with no explicit barrier — believed true (QEMU's TCG/HVF backends
model coherent shared memory), not independently re-verified this session.

**Also found, while reading `clone_thread` for the tgid-mismatch check: a
confirmed, separate, unrelated bug** — a TOCTOU race on file position for
threads sharing a fd (`CLONE_FILES`), found via a probe's log file
consistently missing its own first line of output. Root-caused, fixed, and
verified with a dedicated raw-`write()` reproduction (0/800 corrupted blocks
after the fix, vs. 136/800 before); full writeup:
[`CONCURRENT_WRITE_POSITION_RACE.md`](CONCURRENT_WRITE_POSITION_RACE.md).
**Not the Failure D hang** — a distinct bug, found along the way, real and
fixed. Flagged here only because it's a plausible (unconfirmed) contributor to
this doc's own still-open Failure B; see that doc's "Not fixed" section.

**Revised next step:** candidate 1 (arrival count lost) now outranks
candidate 2 (wake entered but hung) as the thing worth instrumenting.
`InstrBarrier`'s own `shadow_count`/`shadow_gen`/`last_notify_tid` atomics
(`userspace/forktest/selfhost_repro/jobserver_stress.rs`) already exist for
exactly this, lock-free and readable without perturbing the mutex — but the
watchdog thread that's supposed to print them on a stall has its own
reliability problem (§7.12: never fired even across an 800+ second hang). Two
ways to get at that data without depending on the watchdog: (a) a GDB
memory read of those atomics' live addresses mid-hang (now that GDB usage is
established, §7.13), reading the *actual* count each stuck process is at —
directly answers "does count ever reach anywhere near `n`, or does it stall
at a stable-but-wrong value"; or (b) instrument `sys_futex`'s
`FUTEX_WAIT`/`FUTEX_WAKE` match arms directly for the barrier's **mutex**
word (`0x10070f58`-shaped, not the condvar word) to log every acquire/release
transition with a timestamp — since if the mutex's own wake/unlock protocol
is what's actually misbehaving (not literally "stuck," since it visibly
cycles, but perhaps *starving* one specific thread — the barrier's would-be
4th arrival — indefinitely under contention), that would show up as a
FIFO-ordering or fairness violation in the mutex's own wake sequence, not as
a "no wake issued" signature at all.

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

0. **Failure D now has a 25-second deterministic repro — root-cause it first
   (§7.9).** The cheapest open question. With `jobserver_stress` + 4 CPU hogs,
   untimed `Condvar`/`Barrier` waits hang 4/4 while timed waits cycle. Pin the
   residual wake-loss window in the `schedule_blocking`↔`wake_by_handle`
   handshake: enable `FUTEX_DBG_ENABLED` (rebuild) and run the minimal barrier
   repro to capture the exact WAIT/WAKE/value sequence around a hang, OR add a
   targeted tripwire that fires when `futex_do_wake` dequeues a waiter
   (`hist`→`W`) whose `schedule_blocking` does not return within a short window
   (no following `u`). Also test the §7.9 acquire-load candidate. `alarm()`
   being broken (§7.9) means any in-guest timeout must be an external
   `kill -9` watchdog, not `alarm()`.
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
