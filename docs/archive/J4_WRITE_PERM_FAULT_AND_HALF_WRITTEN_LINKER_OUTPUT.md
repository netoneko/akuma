# Behind the orphan bug: a write permission fault with no recovery path, and a half-written linker output — 2026-08-07

**Status**: **OPEN.** Two distinct terminal failures isolated, characterised and
partitioned; neither is root-caused to a line yet. The
[grace-expired hard kill](GRACE_EXPIRED_HARD_KILL_ORPHANS.md) fix is **confirmed
holding** — it is no longer what stops `-j4`. Instrumentation for both failures is
in the tree (`[WPF]`, `[MMAP-STALE-PTE]`), uncommitted.

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

## 7. Next steps

1. **Failure A.** The open question is what produces a mapped, read-only,
   `cow_ref=0` page inside an eager mmap region. Instrument the *producer*, not the
   consumer: log the eager-region install with its final PTE flags, and add a
   tripwire on any transition of a user PTE to read-only that is not accompanied by
   a `cow_ref_inc`. Independently: eager mmap regions have **no** permission-upgrade
   path in the fault handler (§3.4), which is a structural gap worth closing whether
   or not it is this bug.
2. **Failure B.** Log every `ld`/`collect2` exit status alongside the parent's
   `wait4` result, and check whether the linker is among the processes torn down by
   `kill_thread_group`. If a non-zero exit is being reported to cargo as success,
   that is the bug and it is shared with the rest of the saga.
3. Fix the discarded return value in `sys_mmap`'s eager install loop on its own
   merits (§5.2), independent of Failure A.
4. Re-run `-j4` only after (1) or (2) lands. Both failures reproduce in minutes;
   there is no need for the long stall runs that dominated earlier sessions.

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
