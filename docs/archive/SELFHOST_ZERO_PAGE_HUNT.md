# `rustc` ICE "found [0, 0, 0, 0]" in the self-host build — an elimination record (2026-08-15)

> **Status: the ODHT ICE is still OPEN. A real, pervasive use-after-free was found
> and FIXED along the way (§8) — it was not the cause.**
>
> `sys_munmap`'s whole-region arm freed the frame its *region record* named instead of
> the one the *live PTE* held, ~11,000 times per self-host build. Fixing it took
> `[PMM-POISON]` from 18 to **0** and turned cargo's `signal: 11` deaths into clean
> compile failures. The ODHT ICE was unmoved, so it is a different bug — and it now
> reproduces on ~every clean build, which is better hunting ground than the 60% it
> sat at before.
>
> Six other candidate mechanisms were killed by measurement across ~50 instrumented
> builds (§4). The instrument that cracked the UAF is the `FreeSite` tag (§8): it
> named the culprit in **one boot** after four arms of guessing.
>
> **This document previously claimed a root cause and a fix. That claim was wrong**
> and is retained in §5 as a worked example of a mechanism that explained the symptom
> perfectly and was still not happening. Every instrument added along the way is
> permanent and is what made the elimination possible — read §8 before adding more.

## 1. The symptom

In-guest `cargo build --release -p akuma -j4 --offline`, **reproducible on every
`cargo clean`**:

```
thread 'rustc' panicked at rustc_metadata/src/rmeta/def_path_hash_map.rs:56:13:
decode error: Expected header tag [79, 68, 72, 84] but found [0, 0, 0, 0]
```

`[79,68,72,84]` is `"ODHT"`, the odht on-disk hash table header in crate metadata.
It fails in **pairs at the same second** on `enumn` + `zerocopy-derive` — the only two
proc-macro crates cargo builds in parallel at that stage, and the two that depend on
`syn`.

That pairing is what made a *file*-side explanation so attractive, and it is a
coincidence of scheduling, not of file identity. See §6.

## 2. Measured reproduction rates

Every arm: same `devbox.img`, `MEMORY=4096`, `SMP=4`, `cargo clean` before each build.

| arm | green | ODHT-zeros ICE | rustc `signal: 11` |
|---|---|---|---|
| `main` (b585aedf) | 1 / 3 | 0 / 3 | 2 / 3 |
| branch + `filesz` clamp | 0 / 4 | 2 / 4 | 2 / 4 |
| + `FPCACHE_VERIFY_HITS` | 0 / 3 | 2 / 3 | 1 / 3 |
| + `PROT_NONE` file guard | 0 / 5 | 4 / 5 | 1 / 5 |
| + `DONTNEED` file guard | 0 / 4 | 3 / 4 | 1 / 4 |
| + `race_free` ref guard | 0 / 6 | 4 / 6 | 2 / 6 |
| + `adopt_user_frame` (§7) | 0 / 10 | 6 / 10 | 4 / 10 |
| + `munmap` stale-frame fix (§8) | 0 / 10 | **10 / 10** | **0 / 10** |
| **`7e379b17` — branch as-found, no fixes at all** | 0 / 6 | 5 / 6 | 12 poison |
| §8 fix, one-walk (final) | 0 / 6 | 6 / 6 | 0 poison |

**The ICE is pre-existing on this branch and none of this hunt caused it.** The
as-found row is the control that proves it: same crates, same signature, with none of
the changes below applied. (`d2c312bb`, the commit before that, does not boot at all —
it wedges in a `[WATCHDOG] Preemption disabled ~108ms at step 6 tid=0` loop, which is
why `7e379b17` is the meaningful baseline.) The premature free is pre-existing too —
12 poison events on the unmodified branch, 0 after §8.

That control was run late, after a challenge, and should have been run *first*: the
very first instrumented arm already had a fix compiled into it, so for most of this
hunt there was no clean baseline to compare against.

### The ~60% was an artefact — the ICE was always ~100%

The last arm looks like a regression (6/10 → 10/10) and is the opposite. **The two
failure modes were competing for the same builds.** `enumn` and `zerocopy-derive`
build late — they need `syn`, which needs `proc-macro2` + `quote` + `unicode-ident`.
Every non-ICE failure in every earlier arm died *before* that stage:

```
5x proc-macro2   2x quote   2x akuma-primitives   1x embedded-io-async   1x smoltcp
```

A build killed by `signal: 11` at `proc-macro2` never reaches the crates that trip
the ICE, and was scored as "no ICE". Removing the premature free (§8) removed those
early deaths — `signal: 11` went 4/10 → **0/10** — so builds now survive to the
proc-macro stage and the ICE shows its real rate: **every single one**.

So the correct reading is: the ICE has been ~100%-conditional-on-reaching-`syn` all
along, and the earlier arms were measuring how often the *other* bug got there first.
**Two failure modes in one metric will lie to you in exactly this direction** — score
them separately, and check *which crate* a failure died on before comparing arms.

Two things to read off this, both important:

- **There are two independent failure modes**, and they must be counted separately.
  `signal: 11` (rustc SIGSEGV) fires on **`main` as well** — it is the pre-existing
  thread-spawn SIGSEGV class ([`../runbooks/debug-thread-spawn-segv.md`], open since
  2026-08-07), not a regression. Anyone bisecting the ODHT bug who counts "build
  failed" will be measuring mostly this.
- **A green incremental build proves nothing.** `cargo clean` is the only thing that
  makes cargo recompile `enumn`/`zerocopy-derive`, so it is the only thing that
  exercises the bug. The on-disk ICE dumps recovered from `devbox.img` cluster into one
  19-minute window and then stop — which reads as intermittency and is not; it is when
  clean builds were being run.

## 3. Recovering evidence without booting

`scripts/ext2read.py` reads `devbox.img` offline — no QEMU, no second VM, no risk to a
live one. It recovered the ICE dumps and their timestamps, `/tmp/akuma/.git/HEAD`, the
build `.rc` files, and the build's **output artifact**: a complete
`target/aarch64-unknown-none/release/akuma`, 3,811,568 bytes, `entry=0x40100000`, all
sections present.

That last one is worth doing first, because it answered the reported complaint ("the
build never produces a binary") before any debugging started: with `CARGO_BUILD_TARGET`
set the kernel lands under `target/aarch64-unknown-none/release/`, and `target/release/`
holds only build-script output.

## 4. Ruled out, with the instrument that would have fired

Each row is a theory that was killed by a counter reading zero **across builds that
reproduced the ICE** — not by argument.

| Theory | Instrument | Reading |
|---|---|---|
| Fill read short/errored, leaving a zeroed frame | `[FILL-SHORT]`, `DP_FILE_FILL_SHORT` | **0** |
| `file_page_cache` serves bytes that aren't the file's | `[FPC-BAD]`, `DP_FILE_CACHE_MISMATCH` — re-reads the page from disk and compares on **every hit** | **0** across **4,334,431 hits** |
| A `PROT_NONE` *file-backed* region auto-committed with a zero frame | `[DA-NONE-FILE]`, `DP_PROTNONE_FILE_REGION` | **0** |
| `MADV_DONTNEED` zeroing a file-backed page (`MADV_DONTNEED_SHARED_FRAME.md` "Still open" #2) | `[DONTNEED-FILE]`, `DONTNEED_FILE_BACKED` | **0** |
| The write path corrupts rustc's output | 46 MB `cp` + `md5sum` in-guest | byte-exact |
| The rlib has holes on disk | offline block-map walk of all 1540 blocks | zero holes |

The second row is the load-bearing one: **the shared file-page cache is exonerated**,
and with it the whole "one poisoned `(inode, file_off)` entry" family.

### A control that saved a wrong conclusion

"The guest's `libsyn.rlib` contains no `ODHT` magic, the host's does" looked like proof
of a corrupt write. It is not: **all four** guest rlibs contain zero `ODHT`, including
`unicode-ident` (53 KB) whose consumers compile fine, and every archive is structurally
valid (`!<arch>`, correct members). This rustc stores metadata compressed, and the
host comparison was across a different rustc version. Always check a small,
known-good sibling before believing a magic-number absence.

(`grep` in this environment needs `-a` on binaries or it silently matches nothing —
that trap produced a false "0 occurrences" on the host side too.)

## 5. The wrong fix, kept as a worked example

`sys_mmap` set `filesz: len` — the **mmap length**, not the file size — at both
`LazySource::File` sites. Since `mmap` may legally map past EOF and the fault path
decides shareability by testing `va + 0x1000 <= segment_va + filesz`, every page
between EOF and the end of the mapping was classed "fully covered by file data",
short-read to `Ok(0)`, left zeroed, and published to the shared cache.

That is a real latent bug and the clamp in `resolve_file_extent()` is retained. **It is
not this bug**: `[FILL-SHORT]` never fired, so the path was never taken. It was
adopted because it explained the symptom, not because anything showed it happening —
which is the whole failure mode this document exists to record.

## 6. Where the evidence actually points: a use-after-free

The kernel already reports it. From a failing build's console:

```
[PMM-POISON] x0=0xfeedfacea575f000 is quarantine poison for pa=0x7bd8f000 —
  the kernel FREED this frame while the process still had it.
  freed_by=(tid=11 seq=1253186) now_seq=1253188 cow_ref=0
[x19] pid=316 va=0x3113f000 pa=0x7bd8f000 FREE=false cow_ref=0 tracked=false
       last_free=(tid=11 age=6)
       head=0xfeedfacea575f000,0xfeedfacea575f000,0xfeedfacea575f000,0xfeedfacea575f000
  [REGIONS] va=0x3113f000 claimed_by=1 start=0x3113c000 pages=8 flags=0x60000000000040
  [PTE]     va=0x3113f000 raw=0x6000007bd8ff4f ap=AP_RW_ALL(writable)
```

Read it carefully, because every field matters:

- **`freed_by=(tid=11)` but the mapper is `pid=316`** — a frame freed by one task while
  another address space still had it in a live, writable PTE. Cross-process.
- **`cow_ref=0`** — the refcount believed nobody shared it. That is why `free_page`
  released it instead of declining. Either an increment was lost on the sharing path or
  a decrement ran twice.
- **`tracked=false`** — the mapping address space does not list the frame among its own,
  so its teardown/`munmap` accounting never knew about it.
- **`[REGIONS] … pages=8`** — this is an *eager* mmap region, not a lazy one.
- 10 `[PMM-POISON]` reports, 20 `[WILD-DA]`, and 24 `[COW-HIST]` in a single failing
  build. The `[WILD-DA]` syscall traces show **`NR 215` (`munmap`)** immediately prior.

**How this produces the ODHT zeros.** A prematurely freed frame returns to the PMM, is
handed to the next requester through `alloc_page_zeroed*`, and is **zeroed** — while the
original process still maps it. The victim reads zeros out of memory it still owns.
Metadata that rustc decompressed into that page reads back as `[0, 0, 0, 0]`, with no
short read, no cache miss and no error anywhere. That is precisely the residue §4 left.

It also explains the pairing in §1 without any file-identity story: `enumn` and
`zerocopy-derive` are simply the two processes alive and allocating hardest at that
moment.

**This is the same family as `CARGO_HEAP_NULL_RC.md`** (cargo reading a zeroed `Rc`
pointer out of its own heap), whose *sharing* half was fixed 2026-08-14 in
`MADV_DONTNEED_SHARED_FRAME.md`. The poison reports above are from a kernel that
already has that fix, so this is a **different premature-free path** reaching the same
end state. `MADV_DONTNEED_SHARED_FRAME.md` "Still open" item 4 — `complete_cow_break`
discarding `cow_ref_dec`'s result — is the nearest documented candidate and has not been
excluded.

## 7. Two accounting mechanisms, ~40 hand-maintained sites

The premature free traces to frame lifetime being tracked **twice**, by maps that
count different things and were reconciled by hand:

| | `UserAddressSpace::user_frames` | `COW_REFCOUNTS` |
|---|---|---|
| `crates/akuma-exec/src/mmu/mod.rs:464` | per address space, `BTreeMap<PA, u32>` | `crates/akuma-pmm/src/lib.rs:1102`, global, `BTreeMap<PA, u16>` |
| counts | VAs per PA **inside this AS** | **address spaces** holding the PA |
| used for | teardown enumeration, `tracks_user_frame` | `free_page`'s "may I release?" |

Neither is redundant — only the first can enumerate, only the second answers the
free question in O(1). But the rule connecting them ("one address space
contributes exactly one global reference, however many VAs it maps") lived in ~40
call sites. A full inventory, tests excluded:

**Both mechanisms in one function — 2**

- `crates/akuma-exec/src/process/mod.rs:241` `cow_share_and_demote_range` —
  `cow_ref_inc@298` + `track_user_frame@321`. Correct (fork).
- `src/exceptions.rs:1747` `complete_cow_break` — `track_user_frame@1781,1805` +
  `cow_ref_dec@1824`. Correct shape, but **the `dec` result is discarded**
  (`MADV_DONTNEED_SHARED_FRAME.md` "Still open" #4).

**`cow_ref_*` only, asymmetric by contract — 2**

- `src/file_page_cache.rs:127` `lookup_and_ref` — `cow_ref_inc@136`. Takes a
  reference the *caller* must reconcile, so the inc and the track ended up in
  different functions, different passes, and on opposite sides of the `as_lock`
  hold. **This is where the underflow lived.**
- `src/file_page_cache.rs:160` `insert` — `cow_ref_inc@209`, the cache's own
  reference. Deliberate: the cache is a holder that is not an address space.

**`track_user_frame` only — 13** (correct for private frames, `cow_ref` 0 = single
owner): `mmu/mod.rs:960` (definition), `user_access.rs:343`, `image.rs:206,282`,
`process/mod.rs:2371,2758` (`fork_process`), `exceptions.rs:1497,1609,1637`
(`demand_page_lazy_region`), `:1906`, `:1967`, `:3960,4339`, `aio.rs:85`,
`mem.rs:637,738,923,1130`.

Two of those are worth a second look: `demand_page_lazy_region` tracks frames whose
`inc` happened in another function entirely, and `fork_process` tracks during a
*sharing* operation with no `inc` here, relying on `cow_share_and_demote_range` —
unverified.

### What was changed

`UserAddressSpace::adopt_user_frame(frame, caller_holds_ref) -> bool` maintains both
maps under one `IrqGuard` + `user_frames` hold, reads "first VA for this PA here?"
from the same map it updates, and *returns* whether the caller's reference was
surplus so it cannot be forgotten on one arm and not the other.
`drop_surplus_shared_ref` — the old separate reconciliation pass, unbalanced on the
lost-install-race arm — is deleted.

On atomicity: `AsLockHold` is already `IrqGuard` + spinlock, so a pair inside it is
uninterruptible locally **and** excluded cross-core for that address space. Masking
IRQs alone gives only the first half at SMP=4, which is why the fix is "put both
updates in the existing hold" rather than "add an IRQ guard". Lock order is
`as_lock` → `user_frames` → `COW_REFCOUNTS`, the last a leaf that must stay
innermost.

**Result: it does NOT fix the ICE.** 10 clean builds on the helper: **0 green,
6/10 with the ODHT ICE, 18 `[PMM-POISON]` reports.**

This arm is also the clearest warning in the whole hunt about sample size. The
**first three** builds after the helper landed came back 0 ICEs / 0 zeros /
0 `[PMM-POISON]`, which looks exactly like a fix. Runs 4-9 then produced the ICE six
times running. Three clean builds at a ~60% failure rate happen about 6% of the
time — often enough to fool you, and the temptation to stop and write it up is at
its strongest precisely there. **Ten clean builds per arm is the floor**, and the
same trap is recorded independently in
`project_stress_ab_needs_deterministic_probe`.

So `adopt_user_frame` is a structural improvement — it removes a real underflow and
collapses ~40 hand-maintained sites to one owner — but it is **not** this bug, and
the premature free is still happening (18 poison reports prove the frames are still
being freed under their mappers).

## 8. The free-site tag, and the premature free it caught (FIXED)

`FreeSite` (`crates/akuma-pmm/src/lib.rs`) tags every free with the code path that
made it, packed into the ledger's spare bits (`site << 48 | tid << 32 | seq` — `tid`
is bounded by `MAX_THREADS`, so the site rides for free: no extra array, no extra
store on the free path). `free_page_at` / `record_free_at` / `last_free_record_at`
carry it; `[PMM-POISON]` prints it.

**It named the culprit in one boot.** 6/6 premature frees came back
`site=munmap-region`, and splitting `sys_munmap`'s three arms into distinct codes
pinned it to the whole-region arm.

### The bug

`sys_munmap`'s whole-region arm freed the frame the **region recorded**, not the
frame the **live PTE** holds:

```rust
Some(&frame) => {
    let _ = aspace.unmap_page_no_flush(va);
    if aspace.remove_user_frame(frame) { to_free.push(frame); }   // frames[i], stale
}
```

The record goes stale whenever something replaces a mapping without rewriting the
region's frame list — `complete_cow_break` installing a private copy,
`MADV_DONTNEED`'s share-break, a CoW write fault. The consequences are a matched
pair: the **recorded** frame is freed although this process no longer maps it (and a
peer may), and the frame that *was* mapped is unmapped and never freed. A
use-after-free and a leak from one stale index. The `None` arm immediately below it
already read the live PTE; this arm never did.

Fixed by trusting the PTE (`aspace.translate(va)`), falling back to the record only
when nothing is mapped.

### Measured

| | before | after |
|---|---|---|
| `[MUNMAP-STALE]` (stale record hit) | — | **11,255 per build** |
| `[PMM-POISON]` | 18 per 10 builds | **0 / 10** |
| rustc `signal: 11` | 4 / 10 | **0 / 10** |
| cargo exit | mostly `139` (SIGSEGV) | `101` (clean compile failure), 10/10 |
| ODHT ICE | 6/10 | 10/10 — *unmasked*, see §2 |

So the premature free was real, pervasive (11k hits in one build) and is **gone**, and
the wild SIGSEGVs went with it — `signal: 11` at 0/10, and cargo's exit code moving
from `139` to `101` across the board, is that change showing up from outside the VM.
**The ODHT ICE is a different bug.** Its apparent rise from 6/10 to 10/10 is the
SIGSEGV class no longer killing builds before they reach `syn` (§2) — it now
reproduces on every clean build, which is better hunting ground, not a regression.

Cost: `translate()` per page roughly doubles `munmap` wall time on this workload
(~50 s → ~100 s per build). Worth optimising (the walk can be hoisted), not worth
reverting.

## 9. `MAP_SHARED|MAP_ANONYMOUS` was not shared (found + FIXED 2026-08-15)

A side finding that invalidates instrumentation rather than explaining the ICE, and
which anything cross-process on this kernel needs to know.

`mmap(MAP_SHARED|MAP_ANONYMOUS)` **behaves exactly like `MAP_PRIVATE`**: `fork` copies
it instead of sharing it, so a child's write is invisible to the parent. Probe:
`userspace/forktest/c_stress/shmanon.c` (in `userspace/build.sh` -> `bootstrap/bin/`),
which checks both legs in one run — testing only the `MAP_SHARED` leg cannot tell
"sharing is broken" from "the child never ran". Calibrated correct on macOS arm64;
the guest fails the shared leg and passes the private one.

```
MAP_SHARED  parent sees 0x0  -> *** NOT SHARED - behaves like MAP_PRIVATE ***
MAP_PRIVATE parent sees 0x0  -> isolated (correct)
```

**How it was found, and why that matters more than the bug.** `fpcpoison` lines its
children up on a spin gate in a `MAP_SHARED` page so they fault the same pages at the
same instant. In-guest, the parent never observed `ready` reaching 4 — it timed out on
every round and released anyway:

```
warning: only 0/4 children reached the gate; releasing anyway
```

So every "concurrent, cross-process" round it has run on this kernel actually ran
**unsynchronised**, and its `ALL PASS` is far weaker evidence than it reads as. Had the
gate stayed unbounded (as originally written) this would have hung the box instead of
warning — the bounded spin is what surfaced it.

**Rule this establishes: an instrument can be broken by the bug it is hunting.** The
gate failed silently and the probe still printed `ALL PASS`, which reads as evidence
and was not. A bounded spin is what turned a hang into a visible warning; unbounded, it
had simply wedged the box.

### The fix

`fork` shares every region through `cow_share_and_demote_range`, which demotes both
sides to RO so the first write breaks CoW into a private copy. That is the correct
default and precisely wrong for `MAP_SHARED`: it makes writes *diverge*, which is the
opposite of what the flag asks for.

- `MmapRegion::shared_anon` records the flag at `mmap` time
  (`MAP_SHARED` and **not** file-backed — file-backed `MAP_SHARED` is a separate
  mechanism, `SHARED_FILE_MAPPINGS` writeback).
- `process::share_rw_range` is the fork-time counterpart of
  `cow_share_and_demote_range`: same reference accounting (one `cow_ref_inc` per
  distinct PA per address space, deduped the same way), but the child gets the
  parent's PTE flags **verbatim** and the parent is **not** demoted. No TLB
  maintenance — the parent's PTEs are untouched and the child's AS has never run.
- The flag propagates through CoW inheritance (`inherit_mmap_regions_for_cow_child`)
  and through region **splits** on partial `munmap`, or a grandchild silently stops
  sharing.

Verified in-guest, matching the host calibration exactly:

```
MAP_SHARED  parent sees 0x5eed  -> SHARED (correct)
MAP_PRIVATE parent sees 0x0     -> isolated (correct)
```

533 host tests pass; fork is exercised by every build in the arms above.

### What it unblocked

With a working gate, `fpcpoison` finally ran as designed — genuinely concurrent,
cross-process — and passed on the rlib that actually trips the ICE:

| file | rounds x procs | result |
|---|---|---|
| `libquote` (516 KB, 126 pages) | 5 x 4 | ALL PASS |
| **`libsyn` (6.3 MB, 1540 pages)** | 3 x 4 | **ALL PASS** |

So the file mapping is correct end-to-end under concurrency, for the exact file whose
metadata rustc reads zeros from. Combined with §4, that closes the file side properly
rather than by inference.

## 10. Theories — killed, and live

### Killed, each by an instrument that would have fired

Every row was measured **on builds that reproduced the ICE**, not argued.

| # | Theory | Instrument | Reading |
|---|---|---|---|
| 1 | Fill read short/errored, leaving a zeroed frame | `[FILL-SHORT]` | 0 |
| 2 | `file_page_cache` serves bytes that aren't the file's | `[FPC-BAD]`, re-reading every hit from disk | 0 / **4,334,431 hits** |
| 3 | `PROT_NONE` file region auto-committed with a zero frame | `[DA-NONE-FILE]` | 0 |
| 4 | `MADV_DONTNEED` zeroing a file-backed page | `[DONTNEED-FILE]` | 0 |
| 5 | Write path corrupts rustc's output | 46 MB `cp` + `md5sum` | byte-exact |
| 6 | The rlib has holes on disk | offline block-map walk, 1540 blocks | 0 holes |
| 7 | Premature free (UAF) delivering recycled zeroed frames | `[PMM-POISON]` + `FreeSite` | **real, FIXED** (§8) — ICE unchanged |
| 8 | The mmap path is wrong end-to-end, cross-process | `fpcpoison`, 4 concurrent procs on `libsyn` (1540 pages) | ALL PASS |
| 9 | `lto = "thin"` memory/time cliff | host A/B | +1.4% RSS, no cliff |
| 10 | `filesz` past-EOF pages published as zeros | `[FILL-SHORT]` | **real latent bug, fixed** — never fires here |

Rows 7 and 10 matter for the method: both were **real bugs** that explained the symptom
plausibly, were fixed, and changed nothing. Fixing a real bug is not evidence that it
was *this* bug.

### Live, ranked

**T1 — `sys_read`/`sys_pread64` short-copies to the user buffer.** The strongest
untested candidate, and the most conspicuous gap: `edd91fe7 "safer memory helpers"`
rewrote **every** user copy on this branch, `sys_read` included, and nothing in this
hunt instrumented the read path's *copy* half. If the syscall returns `n` but copies
fewer than `n` bytes into userspace, the tail of a freshly-allocated (zero) buffer stays
zero — which is exactly `[0,0,0,0]` where a header should be, with no error anywhere.
Test: count bytes-read against bytes-copied in `sys_read`/`sys_pread64`/`readv` and
print on mismatch. Note `ld` issues ~13,000 `readv`s per link (§ PSTATS above), so
`readv`'s iovec walk is as interesting as `read`.

**T2 — an anonymous heap page reads back as zeros.** The same end state as
[`CARGO_HEAP_NULL_RC.md`](CARGO_HEAP_NULL_RC.md), whose *sharing* half was fixed but
whose class is not closed. rustc materialises crate metadata into its heap; a heap page
that loses its contents produces this exact panic. Every instrument in this hunt points
at the **file** side, which is now closed — so instrument the anonymous side: the
anon demand-page arm, the CoW break, and `mprotect`/`mremap` remapping live data.

**T3 — a missing barrier / memory-ordering bug under SMP.** `enumn` and
`zerocopy-derive` fail in the *same second*, compiled in parallel. If a frame is zeroed
by one core and published before the zeroing is visible to another, a reader sees zeros.
**The decisive experiment is whether it reproduces at SMP=1 — and that is currently
BLOCKED, see below.**

**T4 — the ext2 block cache under concurrent readers.** Keyed on physical block number;
`write_block` evicts correctly, but nothing here tested it under concurrency. Weakened
by row 8 (`fpcpoison` passes concurrently), not eliminated.

### Blocked: the SMP=1 experiment wedges the box

`SMP=1 overlays/devbox/run-smoltcp.sh`, same workload: build 1 completed in **1012 s**
(result unread — ssh died before it could be queried), build 2 **hard-wedged** the VM.
QEMU at **98% CPU**, console frozen, last line:

```
[AS-NEW] pid=86 l0=0x80d2e000 asid=0x6e via=clone parent=81
```

That is the [`selfhost-kernel-build.md`](../runbooks/selfhost-kernel-build.md) "Defect A"
shape (100% CPU, console silent, no `PANIC`) but at **SMP=1**, where it has not been
recorded before. Console-stopped is the load-independent wedge signal; ssh timing out is
not, and on this box ssh has historically stayed responsive during builds, so the two
together are the tell.

Until this is fixed or worked around (boot with `GDB=1` and attach — the gdbstub must be
armed at launch, so a wedge on a VM booted without it is uninspectable), T3 cannot be
tested, and **any single-core control for any other theory is unavailable too**.

## 11. What is NOT done

1. ~~**The premature free is not localised.**~~ **DONE — §8.** The `FreeSite` tag
   named it in one boot.
2. **The ODHT ICE is still unexplained.** Nothing here fixed it, and it now fires on
   ~every clean build. What is left after §4 and §8: the corruption is not the file,
   not the cache, not a short read, not `madvise`, and not a premature free. The next
   suspect is the *other* end — the page rustc decompresses metadata **into** (its
   anonymous heap), which is where `CARGO_HEAP_NULL_RC.md` found its zeros too.
   Instrument the anonymous side, not the file side.
3. **Three guards fire zero times on this workload** and are correct only on their own
   terms: the `filesz` clamp (§5), `PROT_NONE`-file, `DONTNEED`-file. Keeping them is
   defensible; so is deleting them. They are not load-bearing for anything measured.
4. ~~**`munmap` got ~2× slower**~~ **DONE.** The first fix called `translate(va)` and
   then `unmap_page_no_flush(va)` — two identical four-level walks per page, when
   `unmap_and_free_page_no_flush` already walks once and returns the PA (the arm right
   below it used it). Both arms collapse into that one call, so the region's frame
   record is consulted *only* to report that it disagreed. Builds went 98-147 s →
   **43-44 s**, i.e. faster than the unmodified branch: the stale-frame bug leaked as
   well as over-freed (the frame that *was* mapped was unmapped and never returned),
   so removing it removes ~11k leaked frames per build and the reclaim churn they
   caused. That last step is inference from wall time, not a direct measurement.
3. **`signal: 11` is untouched** and is the more frequent failure. It reproduces on
   `main`.
4. **`fpcpoison` never completed a run in-guest.** 10 rounds × 4 procs over a 6.3 MB
   file is far too slow on a 4-core emulated guest, and its first version busy-waited on
   the start gate forever when the parent was killed — that wedged the box and cost five
   `kill -9`s. Both spins are bounded now (`GATE_SPIN_LIMIT`), but the probe still needs
   to be pointed at a *small* file, and it is aimed at the file-page cache, which §4
   exonerated. It is not the instrument this bug needs.

## Background

- [`HANDOFF_ZERO_PAGE_ICE.md`](HANDOFF_ZERO_PAGE_ICE.md) — **self-contained handoff
  prompt** for picking this up cold: repro, elimination table, ranked live theories,
  the SMP=1 blocker, and the seven method rules this hunt paid for
- [`CARGO_HEAP_NULL_RC.md`](CARGO_HEAP_NULL_RC.md) — the same end state (zeroed page in
  a live process), the task brief the earlier hunt ran from
- [`MADV_DONTNEED_SHARED_FRAME.md`](MADV_DONTNEED_SHARED_FRAME.md) — the fixed *sharing*
  half, and the four "Still open" items, one of which (#4) is a live candidate here
- [`CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md`](CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md)
  §13 — the audit that ruled out PMM-level UAF *at that time*; §6 above is evidence it
  needs revisiting
- [`COW_PILE_AUDIT.md`](COW_PILE_AUDIT.md) §5.6 — the refcount-underflow class
- [`../runbooks/debug-thread-spawn-segv.md`](../runbooks/debug-thread-spawn-segv.md) —
  the `signal: 11` mode this bug is tangled with
- [`../runbooks/selfhost-kernel-build.md`](../runbooks/selfhost-kernel-build.md) — the
  build; its LTO guidance is corrected there (thin costs +1.4% RSS, not a cliff)
