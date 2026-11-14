# PHASE-2-HEAPRO — the present-RO heap write (the released range that came back)

**Gate:** PHASE-2-HEAPRO · **Date:** 2026-11-14 · **Session:** 13 (goose)
**Feeds:** PHASE-2-NULLFIX's java-arm death (research/PHASE-2-NULLFIX.md, gate log's
post-verdict window), REQUIREMENTS re-rank 2026-11-14 (post-NULLFIX).

## The death (the gate's data, verbatim from PHASE-2-NULLFIX.log)

```
[Fault] ring3 PageFault rip=0x000060000067cd7a rsp=0x00006000014fe4c0 cr2=0x00006000112e9000 code=0x7 -- task killed
[mm] kind=2 0x6000112e9000..0x600011559000 covers fault
[fault-code] rip=0x60000067cd7a pte=0x20dc6025 pa=0x20dc6000 refs=2 read=true bytes=[4c,89,26,c6,46,08,01,...]
```

- `code=0x7`: a user **WRITE to a PRESENT, read-only** page. Instruction bytes:
  `mov %r12,(%rsi)` + `movb $1,0x8(%rsi)` — an oop store with its card mark
  (libjvm text, `r-xs`). rsi=cr2 exactly.
- `pte=0x20dc6025` = P(1) · W(0) · U(1) · A(1) · D(0) · **bit 9 clear**.
  `akuma_pte_amd64::COW = 1 << 9`: the entry is **not CoW-marked**.
- `pa=0x20dc6000 refs=2`: the frame is shared — the read-only-file-page-cache
  shape (`akuma_fpcache`: one frame per `(inode, offset)` per mapper), not
  fork-CoW (a fork demotion carries the mark bit; this one does not).
- `[mm] kind=2` (MM_EVENTS): the faulting VA was **MAP_FIXED-released earlier in
  this task's life** (release event `0x6000112e9000..0x600011559000`), and no
  later `mmap` event (kind 5) covers it in the 128-entry ring.

## D43 — the fault handler was right; the PTE should not have existed

`fault_trap_locked` (akuma-tokio-kernel-amd64) routes code=0x7 to
`cow_resolve` → `akuma_cow::CowFault::decide`: `pte_writable=false`,
`marked=false` (bit 9 clear), `refs=2` ⇒ **`CowAction::Fault`** — a genuine
protection violation, SIGSEGV. The crate's own rule (marked before refs; an
unmarked RO page is read-only *on purpose*) was applied correctly. So the bug
is not the verdict — it is that a **stale, unmarked, read-only PTE survived a
release** and then a fill installed a fresh page over a range no region claims.

Hypothesis register (the runbook's three):
- **H-A (chosen): stale P=0 lazy records survive every release; `lazy::fill`
  re-serves them.** See D44. Explains refs=2 (fpcache share), unmarked
  (file fills are never CoW-marked), RO, A-set/D-clear, and kind=2 covering the
  fault with no later mmap.
- **H-B (refuted): the CoW break was refused.** Refused it was — but rightly:
  `marked=false` means the PTE was not CoW, and a fork-demoted page would carry
  bit 9 (`X86_COW`). The refusal is the *symptom carrier*, not the defect.
- **H-C (recorded, not this death): the same walk skips `Lookup::Large`
  leaves** — a present 2 MiB leaf inside a released range is jumped by the
  span-skip (`x86_walk_leaves`) and would survive too (stale RW mappings, silent
  corruption). Not this fault (the stale entry is a 4K leaf), and no user-side
  creator of large leaves was found; recorded as a same-class finding for a
  later pass.

## D44 — root cause: the release sweep is present-only, lazy records are P=0

`akuma-tokio-kernel-core/src/lazy_record.rs`: a lazy leaf's **bit 0 is clear by
design** ("Neither CPU reads anything of a leaf whose bit 0 is clear"; the other
63 bits are the software record: TAG=1<<1, WRITE, EXEC, FILE, NONE, SHARED,
inode, page index). `lazy::reserve` installs them per page via
`akuma_pagetable::leaf_table_slot` ("the leaf slot for `va`, **present or
not**").

Every release path funnels into one body (docs/archive/AKUMA_ONE_MUNMAP.md):
`unmap_range` → `UserAddressSpace::unmap_present` →
`rewrite_leaves_in_range` → `x86_walk_leaves`. That walk classifies a lazy leaf
as **`Lookup::Missing { level: 1 }`** (`akuma-pagetable`'s lookup tests the
valid bit first) and the Missing arm **jumps forward without clearing**. The
same on the AArch64 twin (`unmap_and_free_page_no_flush` returns `None` unless
VALID). So after `munmap`, `MAP_FIXED`, `mremap` shrink/move, or exit's shared
release, the range's lazy records — a map of what *used to* live there — are all
still in the leaf slots.

The consumer is the fault path's **first** arm: not-present faults call
`lazy::fill(root, va, files)` *before any region lookup* (it cannot consult
regions — `akuma-tokio-kernel-core` sits below `akuma-exec`). `fill` decodes the
leaf it is handed:
- stale **anon** record ⇒ a fresh zero RW page over the released VA (a write
  that Linux would SIGSEGV silently succeeds — corruption, no trace);
- stale **file** record ⇒ the old file's page read back (RO, fpcache-shared,
  refs≥2) ⇒ the next **write** faults code=0x7, `cow_resolve` sees unmarked ⇒
  SIGSEGV. **That is the observed death, byte for byte.**

JVM story consistent with the window: a read-only file-backed lazy mapping
(`MMAP_FILE_BACKED_LAZY` is on; ld.so/JDK file maps) released by a MAP_FIXED
over `0x6000112e9000..0x600011559000`; the G1 heap later touches that VA; the
stale fill serves the old file page; the oop store dies. The fault VA equals the
released range's start, exactly where the surviving record sits.

R9 precedents searched: `docs/archive/AKUMA_ONE_MUNMAP.md` (the one-body
release — "sweeps the whole range by live PTE": a lazy record is *not* a live
PTE, which is the body's blind spot), `AKUMA_MAP_FIXED_EAGER_LEAK.md` (the
records-vs-pages split is where this stale-record class lives; that fix cleared
records it knew about — eager regions — and the same class recurred for lazy
ones), `lazy_record.rs`'s own contract ("every walker tests the valid bit before
anything else" — so *no* walker can see a stale record), `akuma-cow`'s module
header (grant-vs-deny records; `marked` is a PTE property, not a refcount — the
reason the refusal was correct), `COWSTALE_FORK_THREAD_SEGV.md` (re-read the
live PTE before judging — the CoW arm does). `shanghai/` and `taoxichuan/` have
**no lazy regions at all** (akuma-mmu's own feature note: "no ASID/PCID, no lazy
regions…" describes the sibling lineage), so no precedent exists there; the
class is this lineage's own.

## D45 — the fix: `lazy-release-sweep` (off by default, R6)

In `crates/akuma-mmu/src/lib.rs`, x86 `UserAddressSpace::unmap_present` **only**
— NOT in the shared `x86_walk_leaves`: `reprotect_present` (mprotect) walks the
same leaves and must keep untouched lazy records (mprotect over a lazy range
erasing records would strand not-yet-faulted pages). After the present-leaf
walk, feature-gated:

```rust
#[cfg(feature = "lazy-release-sweep")]
self.clear_lazy_records_in_range(start, end);
// per page: akuma_pagetable::leaf_table_slot(root, va) -> slot;
// if entry != 0 && entry & 1 == 0 { slot.set(0); }
```

- Clearing P=0 slots needs **no TLB flush** (a not-present leaf is never
  cached); the present-leaf walk's own shootdown stands unchanged.
- Order safety: the MAP_FIXED arm releases **before** the new region installs
  (`unmap_range` then `vm_install_region`, glue mem.rs), so the sweep cannot eat
  a fresh region's records; `munmap` has no region to eat.
- Refuses nothing else: any non-present non-zero leaf in a *released* range is
  by definition software's record of what used to be mapped there — release
  means it goes.
- AArch64 twin carries the same hole; recorded, not fixed (denver is amd64; R6
  scope). Feature relay 1:1, the `fold-regpreserve` shape:
  `akuma-mmu` ← `akuma-tokio-kernel-amd64` ← `akuma-tokio-kernel` (amd64-only
  line, the `procfs-diag` shape) ← `akuma-jvm-kernel` ← `denver`.
- R6 evidence: default build byte-compared before/after the change (cfg'd code
  adds nothing when off); clippy clean in default and gate feature sets; host
  tests for the touched crates.

## D46 — the probe: `denver/probe-heapro.sh` + jvmprobe `lazyrel`

probe-nullfix.sh's shape: rig stamps, free-the-slot first, ssh round trip, stop
at the end, console window last. New rig `denver-heapro` + `vm-heapro.spec` (the
nullfix spec's disk and port, new kernel path). jvmprobe grows a `lazyrel` arm
(fork-isolated like mmaparms; the crashy sub-arm is the *proof*):

- **anon arm**: `mmap(NULL, 2 MiB, RW, MAP_PRIVATE|MAP_ANONYMOUS|MAP_NORESERVE)`
  (NORESERVE ⇒ lazy regardless of size), touch two pages, `munmap`, then **write
  the released VA**. Fixed kernel: SIGSEGV (leaf cleared, no record, fill says
  NotLazy) ⇒ `LAZYREL PASS anon-release-kills`. Buggy kernel: `lazy::fill`
  serves a fresh zero page and the write succeeds ⇒ `LAZYREL RED
  stale-anon-write`.
- **file arm**: `mmap(NULL, 2 MiB, RO, MAP_PRIVATE, fd(open /bin/jvmprobe), 0)`
  (demand-paged file records, `MMAP_FILE_BACKED_LAZY` on), read byte 0,
  `munmap`, then **read the released VA**. Fixed: SIGSEGV ⇒ `LAZYREL PASS
  file-release-kills`. Buggy: the stale byte comes back ⇒ `LAZYREL RED
  stale-file-read` (and a *write* there would be the java death shape).
- Arms riding (each fix must stay fixed): tokioprobe, RWX, MAPS, AUXV, MMARMS,
  SYSINFO (recorded, G7's feed), **regpreserve** (≥20 REGPASS, zero REGRED).

## D47 — the verdict (RESULT PASS requires, in order)

1. Rig identity: uname says denver and **NOT** `72be5c55` (the nullfix binary —
   no sweep in it), NOT `800e8c80` / `f0dcacb6` / `3531cf69` (stale staging);
2. tokioprobe RESULT PASS; 3. `RWX PASS rw_to_rx=42` (rides); 4. MAPS PASS +
   AUXV PASS (rides); 5. MMARMS PASS (rides); 6. SYSINFO PASS or INFO recorded
   (G7 owns that verdict); 7. regpreserve all-REGPASS, zero REGRED/CRASH (the
   nullfix fix rides); 8. **LAZYREL PASS anon-release-kills AND
   file-release-kills, zero LAZYREL RED/CRASH** — the fix's java-free proof;
   9. **the java arm MOVED** (D37's honesty rule): the console window carries
   NO fault at `rip=0x000060000067cd7a` — a kill at that rip is an honest FAIL.
   Progress (a different rip, version output, or no kill) is the record; its
   attribution is the next gate's data.

## Composition plan and new-LOC estimate (R5 budget)

1. this artifact (~120 LOC);
2. `crates/akuma-mmu/src/lib.rs`: the cfg-gated sweep in `unmap_present` + one
   helper (~30 LOC with comments);
3. feature lines in five Cargo.tomls (~12 LOC);
4. `denver/probes/jvmprobe.c`: the `lazyrel` arm (~80 LOC);
5. `denver/probe-heapro.sh` (~210 LOC, mostly the nullfix shape);
6. rig staging (`denver-heapro`, `vm-heapro.spec`, image re-injection with the
   new jvmprobe), gate log, RESULT/LOOP_LOG records.

## Experiment proposal (the one-per-phase rule)

None new — PHASE-2-NULLSTORE's recorded proposal (knob
`GOOSE_TOOL_CALL_CUTOFF` at default, judged by this session's `final_ctx` in
COST_LEDGER.md) stands for the phase; the operator owns applying it.
