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
| 11 | Prefault file fill short/errored (`prefault_user_range`) | `[FILL-SHORT/prefault]` | **856/build on the as-found kernel — root cause found + FIXED, §12** |

Rows 7 and 10 matter for the method: both were **real bugs** that explained the symptom
plausibly, were fixed, and changed nothing. Fixing a real bug is not evidence that it
was *this* bug. Row 11 is the counterexample that kept that rule honest: when it was
finally fixed, the rate moved (0 → 3/10 green) — and note that row 1's `0` reading did
not cover it, because row 1's instrument sat on a path a prefaulted page never takes.

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
2. **The ODHT ICE is still unexplained.** ~~Nothing here fixed it, and it now fires on
   ~every clean build.~~ **Partially explained 2026-08-15 — §12:** the prefault
   inode-stub zero pages were found and fixed, moving green builds 0 → 3/10.
   **§13 then scored the residue:** it is garbage-byte decode errors (two modes),
   not zeros — a writer/reader coherence class, tracked as T5 there.
   What is left after §4 and §8: the corruption is not the file,
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

## 12. The prefault fill ran through a failing stub — FOUND + FIXED (2026-08-15, later that day)

Full write-up: [`PREFAULT_INODE_STUB_ZERO_PAGES.md`](PREFAULT_INODE_STUB_ZERO_PAGES.md).
The short version:

- **`prefault_user_range`'s file fill was never instrumented** — `[FILL-SHORT]` sits
  on the demand-fault fill in `exceptions.rs`, and a page the prefault installs is
  *present*, so no fault ever re-visits it. The instrument was structurally blind to
  this site (rule 5's structural cousin, now rule 8 in the handoff).
- The fill calls the `ExecRuntime::read_at_by_inode` hook, registered in `src/main.rs`
  as a stub returning `Err(-1)` since `94d1daf6 "extract akuma-exec"` (2026-03-05) —
  the signature lacked the `path` the real VFS needs, so the extraction left a stub —
  and the call site dropped the result (`let _ =`).
- With `MMAP_FILE_BACKED_LAZY` on, every RO file mmap carries a real inode, so every
  prefault of such a region installed a **zero page with no error, no fault, and no
  cache involvement**.
- Control arm (instrument only), one reproducing build: **856 firings, all
  `got=Err(-1)`**, against `syn`/`managed` `.rmeta` and `.d` artifacts.
- Fix (hook takes `path`, real `vfs::read_at_by_inode` wired; the prefault site was
  its only consumer), 10 clean builds: instrument silent, **3/10 GREEN** — first
  green builds in any arm of this hunt.

**The ICE is not closed.** 7/10 still fail, and the fix arm scored only
GREEN/FAILED — not which crate died or how (rule 3 violated by the very document
that wrote it; the residue needs re-scoring before T1/T2 are re-ranked). **Done
next — §13.**

## 13. The residue, scored: two garbage-byte modes, an Ok(0) flood, and a timing surprise (2026-08-15, evening)

### Scoring the residue properly (rule 3, belatedly)

5 more clean builds on the unchanged §12 fix kernel, each with the full cargo log
kept in-guest (`/tmp/buildN.log`) and the panic bodies extracted:

- builds 2–4: **GREEN**
- build 1 — **Mode A**, `enumn` + `zerocopy-derive` (the usual pairing), but *not*
  the ODHT decode error:
  ```
  rustc_serialize/src/serialize.rs:402:18: assertion failed: bytes[len] == STR_SENTINEL
  rustc_serialize/src/serialize.rs:136:9:  Encountered invalid discriminant while decoding `Option`
  ```
- build 5 — **Mode B**, on `akuma-exec` itself:
  ```
  rustc_type_ir/src/ty_kind.rs:145:33: invalid enum variant tag while decoding `TyKind`,
  expected 0..29, actual 102
  ```

**The residue is garbage bytes, not zero pages.** `actual 102` is `'f'`; the
STR_SENTINEL assertion means a decoded string's length prefix lied. Both modes are
rustc reading .rmeta that decodes to *wrong* bytes — freshly written seconds
earlier by a concurrent rustc. That is a coherence defect between the writer and
the reader of the same file, a different class than anything in §4–§12 (which
were all zero-page shapes).

### The same arm's kernel log: 313 reads that returned `Ok(0)` mid-file

The fix-arm boot's kernel log (10 builds) holds **313 `[FILL-SHORT]` firings,
every one `got=Ok(0)`** — the demand-fault fill hitting ext2's EOF arm at offsets
the mmap-time `filesz` said were inside the file (0x10e000–0x112000 range,
~1.1 MB). Spread across ~11 build-artifact inodes (44149 ×91, 44187 ×77, then
×29 each for a tail of others).

Follow-up on one victim: inode 44076 was read at 1.0–1.1 MB offsets during the
build but **is now a 498-byte fingerprint JSON**. Either the file was rewritten
smaller between mmap and fault (cargo rewrites fingerprints/.d files
constantly), or the inode was freed and reused while a stale lazy region still
named it (`LazySource::File` holds a raw inode number with no lifetime tie to
the file). Which of the two has not been distinguished yet; the Ok(0) flood and
the garbage-byte modes may or may not share a mechanism.

### New instruments (all in-tree)

- `[E2-EOF] inode= off= size_now=` — ext2 `read_at_by_inode`'s EOF arm with a
  non-zero offset; prints the size the reader *actually* saw (rate-limited to
  32 prints, counted in `E2_READ_AT_EOF`).
- `[E2C-BAD] block= first_diff= cached= disk=` — every ext2 block-cache hit is
  re-read from disk and compared (`verify_cached_block`; counter
  `E2_CACHE_VERIFY_MISMATCH`). Bypasses the cache, so it cannot be fooled by the
  entry it checks — the stale-instrument rule applied.
- `path=` added to both `[FILL-SHORT]` variants, so the next firing names the
  file instead of requiring an inode hunt.

### The arm that went 4/4 green — and why that is not a fix

With those instruments built in (kernel otherwise identical to the fix arm), 4
clean builds: **4/4 GREEN**, and all three instruments silent — `[E2-EOF]` 0,
`[E2C-BAD]` 0, `[FILL-SHORT]` 0 (even the 313-event `Ok(0)` class vanished).

Read carefully: `verify_cached_block` **re-reads every cache hit from disk**, so
this kernel does double I/O on the hottest path in the build. That serialises
and spaces out the exact interleavings a coherence race needs. Four greens at a
~60–70% failure rate is ~(0.35)⁴ ≈ 1.5% by chance — plausible but not proof, and
the perturbation is the more likely explanation. This arm establishes
**the failure window is timing-sensitive** (an observation worth having), not
that anything is fixed. The green-streak rule from §2's method box applies: an
implausibly good arm is suspect, and this one *provably* perturbs the thing it
observes. The honest next arm is **counters only, no disk re-reads**, ten
builds, before believing any rate.

### Live theory after this: T5 — writer-vs-reader incoherence on freshly-written files

The evidence shape — garbage bytes in metadata written seconds earlier by a
concurrent rustc, plus EOF-where-filesize-said-not — points at the
write-vs-read side, which no arm has instrumented yet (fpcpoison tested
concurrent *mappers*, not concurrent writer+mappers):

- **T5a — `file_page_cache` publish/invalidate ordering.** `vfs::write_at`
  writes first, *then* invalidates the fpc entries for the path
  (`let r = with_fs(write); invalidate_file_pages(path);`). The hazard window is
  a concurrent demand fill of the same page: it reads the **old** disk bytes,
  the writer lands the new bytes and invalidates, and the fill *then* publishes
  its old-bytes frame — after the invalidate, so nothing removes it. Every later
  mapper of that page gets pre-write bytes: persistent garbage, exactly the
  Mode A/B shape. Mechanism candidate only — not yet verified.
- **T5b — stale `LazySource::File` inode after unlink/rewrite.** The Ok(0) flood
  shape; a lazy region naming an inode whose file was rewritten smaller
  (or freed and reused) mid-build.

T1 (read-copy shortness) remains untested; T3's SMP=1 wedge is still open; T4
(ext2 block cache under concurrency) is weakened-but-alive via the
remove-then-write window in `write_block` (cache remove precedes disk write; a
reader in that window misses the cache and reads the *old* disk bytes) — though
ext2's state RwLock should exclude that, so it needs the lock topology verified
before being believed.

**Done next — §14**, which runs that counters-only arm and root-causes the
`Ok(0)` flood to T5b.

## 14. The `Ok(0)` flood root-caused: the inode lifecycle is not honored across `mmap` (2026-08-15, night)

### The counters-only arm §13 demanded: 10/10 green — and the instrument still fired

`E2_VERIFY_HITS` now gates the `[E2C-BAD]` disk re-read and defaults to **off**,
so this arm is counters-and-prints only — no doubled I/O, none of §13's
perturbation. Ten clean builds:

- **10/10 GREEN.** Zero Mode A/B decode errors, zero `[E2C-BAD]`.
- **Not silent:** 376 `[FILL-SHORT] got=Ok(0)` and 32 `[E2-EOF]` — and the
  events land on **green builds too** (2, 4, 5, 7, 8).

Read the two halves separately, because they say different things.

**The rate is not bankable.** The §12 fix arm scored 3/10 green at 16:48; this
arm scored 10/10 at 18:16, with only `e831afaa` between them — prints and
counters, no functional change. Two arms that disagree by that much on an
unchanged kernel mean at least one of them measured something other than the
kernel, and which one is **not established**. The one confound whose direction
is known points the wrong way: the disk image degraded monotonically across the
day, so image state cannot explain builds getting *greener*. Rule 7's mirror
image — do not read "the rate improved" as "the bug is gone" any more than
"I fixed a real bug" as progress.

**The mechanism is bankable**, because the same arm's log names it. `[E2-EOF]`'s
32 prints are the rate-limit cap, not a count (the count lives in
`E2_READ_AT_EOF`) — but **all 32 read `size_now=0x0`**. Not "smaller than the
caller believed": *empty*.

### The victims name the mechanism

`path=` (added in §13) pays off immediately. The 376 fills, by victim:

| Fills | Inode | Victim |
|---|---|---|
| 183 | 44115 | `…/build/zerocopy-derive/…/out/libzerocopy_derive-….so` |
| 91 | 44037 | `…/build/smoltcp/…/out/libsmoltcp-….rlib` |
| 29 ×3 | 44160/44177/44178 | `…/build/elf/…/out/libelf-….rlib` |
| 15 | 0 | `…/build/akuma/…/out/build_script_build.…-cgu.0.rcgu.o` |

Every one is a build artifact **written once, then mapped by dependents** — and
the top victim is the proc-macro **shared library** of `zerocopy-derive`, one of
the two crates the original ICE always killed (§1's "treat the pairing as a
scheduling coincidence" was right about the *pairing*; the file was a real
victim all along, just not for a file-side reason).

So both residue modes have one shape: **a file is truncated or unlinked while
another process still maps it.**

### The truncate mechanism, read out of the code

- `sys_openat`'s `O_TRUNC` arm calls `crate::fs::write_file(&path, &[])`, and
  ext2's `write_file` truncates **in place** — same inode, blocks freed,
  `i_size = 0`. That is `size_now=0x0` exactly.
- `sys_unlinkat` frees the inode outright, and ext2 hands the number to the next
  file created.
- `LazySource::File` stores a raw inode number plus the `filesz` captured at
  mmap time, and holds **no reference on the file**. Linux pins the inode
  through the mapping's `struct file`; this kernel drops every reference at mmap
  time.

Which gives the whole chain, both modes from one defect:

```
dependent mmaps artifact  →  cargo truncates/unlinks it  →  dependent faults
   → read_at_by_inode sees i_size = 0        → Ok(0)  → page stays zero
   → …or the number is reused by another file → wrong bytes → Mode A/B garbage
```

A single compile per unit was confirmed from the in-guest cargo logs first —
this is not a double-build artifact.

### Catching the actor

Three new prints, all behind `config::SYSCALL_DEBUG_IO_ENABLED`: `[UNLINK]`,
`[O_TRUNC-ZAP]` (only when the file it zeroes has non-zero size), `[FTRUNC-0]`.
One arm, 7 builds:

- **8274 `[UNLINK]`** — the storm is real, ~1000+ per build.
- **2 `[O_TRUNC-ZAP]`**, both `target/.rustc_info.json`, both benign.
- **0 `[FTRUNC-0]`** — `ftruncate` is *not* the actor; unlink is.

The correlation, same log, same path:

```
3135:  [UNLINK]     pid=7    path=…/out/libsmoltcp-4d810ef3f3c72c99.rlib
7259:  [UNLINK]     pid=347  path=…/out/libsmoltcp-4d810ef3f3c72c99.rmeta
21434: [FILL-SHORT] pid=1566 inode=44184 file_off=0x100000 got=Ok(0)
       path=…/out/libsmoltcp-4d810ef3f3c72c99.rlib — page left zero-filled
```

`pid=7` is the `cargo clean` the repro mandates (its first unlinks are
`target/CACHEDIR.TAG` and the `.cargo-*lock` files); the later low pids are
in-build artifact replacement. The correlation is **by path** — `[UNLINK]` does
not print the inode — so it establishes that the victim files are unlinked
before their fills, not that this specific `unlink` freed that specific inode
number.

**T5b is confirmed end-to-end.** T5a (`file_page_cache` publish-vs-invalidate)
is neither confirmed nor *needed*: the inode-reuse leg of T5b produces the
garbage-byte modes too. Demote it; do not close it.

### The arm's 6 non-green builds are three other defects, not the ICE

1. `signal: 6` / exit 101 on the final `--crate-name akuma` LTO compile — the
   known memory-pressure abort class.
2. **New:** `error: could not write output to …-cgu.0.rcgu.o.rcgu.o: No such
   file or directory` — rustc's *own output path* vanishing mid-write. Same
   unlink-storm family, opposite side: the writer, not the mapper.
3. The `Ok(0)` fills themselves.

**None is an ODHT / `STR_SENTINEL` / `TyKind` decode error.** Across the last two
arms — 11 builds — the original ICE did not reproduce once, the longest clean
streak of the hunt.

One scoring correction, rule 3 again: the harness's first pass labelled several
builds "FAILED" that had produced no error lines at all. They were **ssh
timeouts**, not compiler failures. Score from the guest's own build log, never
from the harness's exit code.

### The confound: the image was genuinely damaged

30+ clean-build cycles left `devbox.img` in a state `e2fsck` had to repair —
unattached inodes reconnected to `lost+found`, wrong ref counts, wrong
free-block counts in two groups and in the superblock. Boot had degraded to 15+
minutes behind a ~1900-line watchdog storm. Repaired and re-verified clean
(exit 0, 2.5% non-contiguous); the procedure is in
[`../runbooks/selfhost-kernel-build.md`](../runbooks/selfhost-kernel-build.md)
§5.5, whose older claim that "the disk itself stays clean through all of this"
is now corrected there. **Rate-score nothing on an unrepaired image.**

Post-repair the same image boots to sshd in **11 s with 18 `[WATCHDOG]` lines**,
so the slow boot was the damage, not a kernel regression. Two things the repair
cost, both worth knowing before the next arm: the campaign's working tree
`/tmp/akuma` came back an **empty directory** (its dirents were what the hard
kills destroyed; `e2fsck` could only reconnect the inodes), so `target/` is gone
and the next arm starts cold from the pre-cloned `/root/akuma`; and the 33
reconnected `/lost+found` orphans carry inode numbers in the **same 44xxx range
as the `[FILL-SHORT]` victims** — independent corroboration that the churned
build artifacts are exactly the inodes this defect touches.

### The fix this points at — the lifecycle, not the fill

Clamping the fill would only hide it. Ranked:

1. **Pin the inode for the life of the mapping** (Linux semantics). Take a
   reference at `mmap`, release at `munmap`/teardown; `unlink` unlinks the name
   and defers the inode free until the last reference drops. Correct, and the
   most work — it needs a real open-file/inode reference count in the VFS.
2. **Invalidate on unlink/truncate**: find lazy regions naming the inode and
   drop or zero-cap them. Cheaper, needs a reverse index, and gets the semantics
   *wrong* — Linux keeps the old contents readable through the mapping.
3. **Generation-tag `LazySource::File`**: store a generation with the inode,
   bump it when the inode is freed, and make a stale fill fail loudly instead of
   returning `Ok(0)`. Fixes nothing, but converts silent corruption into a
   detectable error. The minimum honest step, and a prerequisite instrument for
   either real fix.

### Post-repair sanity arm (same night, closes the loop)

After the repair (and after `/tmp/akuma` was restored from the pre-cloned
`/root/akuma`), the gated-print kernel — `[UNLINK]`/`[O_TRUNC-ZAP]`/`[FTRUNC-0]`
now behind `SYSCALL_DEBUG_IO_ENABLED`, clippy clean, 322 host tests green —
booted to sshd in **9 s** and ran **3/3 clean builds green**. Cumulative:
**14 consecutive greens** across three kernels (10 counters-arm + 1 actor-arm +
3 sanity), with zero Mode A/B decode errors in any of them — and the defect
still firing underneath (§ the actor arm's fills on green builds), so the
streak is luck-of-scheduling, not absence of mechanism.

One instrument-semantics change landed with the gating, after the actor arm:
`[E2-EOF]`/`E2_READ_AT_EOF` now fire only when `offset > file_size` — the
shrinkage anomaly. The old condition (`offset >= file_size`, any non-zero
offset) also counted every ordinary read-at-end, which made the counter noise;
the arms above were scored under the old semantics.

### What is NOT done (carrying §11 forward)

- **The ICE is not proven fixed.** 11 green builds is a streak; the mechanism
  that produced the ICE still fires, on green builds included.
- **3/10 → 10/10 across an instrumentation-only change is unexplained.**
- The inode-lifecycle defect itself is **diagnosed, not fixed** — no option
  above is implemented.
- `signal: 11` and the SIGABRT/LTO class are untouched.
- The `SMP=1` wedge still blocks every single-core control (§11).
- **T1** (read/readv copy shortness) remains untested.

## 15. T5b FIXED: the mapping now pins its inode (2026-08-15, night)

§14 diagnosed the lifecycle defect and ranked three fixes. This is option 1 — the
Linux semantics — implemented, because options 2 and 3 both leave a mapper
reading bytes that are not its file's.

### The rule

**A `LazySource::File` region holds a reference on its inode for as long as it
exists, and the filesystem will not free a referenced inode.** `unlink` removes
the name and stops there; the truncate and the bitmap free happen when the last
mapping goes. An unlinked-but-still-mapped inode keeps its size and its block
pointers, which is exactly what lets the mapper go on reading correct data.

### Why it is a `Clone`/`Drop` handle and not a counter

The hard part was never the count, it was the **call sites**. Regions are copied
and destroyed by `push` (including replacement at an existing VA), `remove`,
`clear`, fork's `extend_from_slice` and `replace_with_clone`, `update_flags`'s
three-way split, `munmap_one_overlap`'s four clip shapes, and `Process::drop`.
Hand-maintaining a reference count across those is precisely the kind of ~40-site
bookkeeping §7 of this document already found this tree getting wrong.

So the count is maintained by the type, not by the code:

```rust
LazySource::File { path, inode, file_offset, filesz, segment_va, pin: InodePin }
```

`InodePin::clone` increments and `InodePin::drop` decrements
(`crates/akuma-primitives/src/inode_pin.rs`). Every path above already copies or
drops the `LazySource`, so **all of them became correct at once**, and none of
them mentions the pin. `LazySource::file()` is the only constructor, so a caller
cannot build an unpinned file region either.

Nothing ever *reads* the field. It is load-bearing entirely through its
destructor — which is worth knowing before someone deletes it as dead weight.

### Lock-free, because of where it is called from

The two callers sit on opposite sides of a lock-ordering hazard: pins are taken
and dropped on the demand-fault path, and `is_pinned` is read by ext2 **while
holding its state write lock**. A spinlock would be an AB-BA waiting to happen,
so the table is a fixed 1024-slot open-addressed array of atomics with CAS
updates and tombstone reuse — no allocation, no lock, callable from any context.

### Every failure mode defers a free rather than permitting one

- Keyed on the inode number **alone**, with no filesystem identity, so two mounts
  sharing a number alias. Aliasing can only *add* pins, so it costs a deferred
  free and never a freed mapping. (The deferral list itself is per-filesystem,
  which is not optional — a shared one lets one mount's drain free another
  mount's inode. The first draft had it global and the test suite's parallel
  mounts caught it.)
- Pin table full → the pin is unrecorded, `pin_ovf` counts it, and `is_pinned`
  answers `true` for **everything** until the lost pins are released.
- Deferral list full → the inode is **leaked**, not freed: `defer_leak` counts it
  and `e2fsck` reclaims it. Leaked blocks are recoverable; bytes handed to the
  wrong reader are not.

Drains run on unlink and on inode allocation, so a build's own unlink storm keeps
the list short, and a filesystem cannot report itself full while holding inodes
nothing references.

### A second fix the design requires: invalidate the page cache by inode

`file_page_cache` is keyed on **`(inode, file_offset)`**, and `vfs::remove_file`
invalidates it *by path*, before the unlink. Deferring the free makes that
structurally too early: an unlinked-but-still-mapped file goes on faulting, and
every successful fill **publishes pages under a number that is about to be
reissued**. The next file to take that number would inherit a dead file's cached
pages.

Under the old behaviour this could not happen, by accident rather than design:
the unlinked inode was truncated at once, so those fills returned `Ok(0)`,
`fill_complete` went false, and the pages were **withheld from the cache**
(`DP_FILE_FILL_UNPUBLISHED`). Repairing the fills removes that accident, so the
deferral has to pay for it explicitly.

It does, with a callback at ext2's `free_inode` — the single point where a number
returns to the allocator — invalidating the cache by inode. The pin is what makes
that callback race-free: `LazyRegionMap::lookup` **clones** the `LazySource` for
the caller to use outside the lock, and that clone carries a pin, so a fill in
flight is a live reference and an inode reaching `free_inode` provably has no
fill that could republish behind the invalidation.

### The scare that wasn't: a 1-build control almost sold a false conclusion

Worth recording, because it is rule 1 catching the author of this document.

Two clean builds on the fixed kernel failed with the linker rejecting freshly
built artifacts (`invalid sh_type … expected SHT_STRTAB`, `ELF section name out
of range`), one of them killing `zerocopy-derive` **and** `enumn` in the same
second on the same `libsyn.rlib` — the hunt's signature pairing (§1), reached
through the linker instead of rustc's decoder. A single as-found control build
came back **green**, and the obvious reading was "the regression is yours".

It was not. Extending the control to four builds gave **1 green / 4**, against
the fixed kernel's **1 green / 3** — indistinguishable — and control build 2
failed as `a section [index 30] has an invalid sh_name (0x5000feed)`. The
`0xfeed` is the tell: that is **PMM quarantine poison** read out of a mapped
file page, the same signature as the fixed kernel's
`\x00P\x8eA\xce\xfa\xed\xfe` (`0xFEEDFACE418E5000`). Both arms corrupt, at the
same rate, in the same way.

So the pin fix neither caused this nor cures it. And the near-miss is exactly
what rule 1 exists for: *one* control build is not a control arm, and it pointed
confidently the wrong way.

### What the poison actually tells the next session

This is the sharpest evidence the hunt has produced for the **premature-free**
class (`CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md`, still open), and it is better
evidence than a null `Rc`:

- The poison appears **inside a file-backed mapped page** — a freshly built
  `.rlib` the linker was reading — so a *file page* frame is being freed while
  still mapped. That narrows the search to file-page frame accounting
  (`file_page_cache` refcounts, CoW refs on shared file pages), away from the
  anonymous-heap theories.
- It is **self-identifying**: `poison ^ 0xFEEDFACEDEAD0000` names the frame, so a
  single reproduction plus `report_poison_value` should name the free site
  directly — the `FreeSite` tag is what cracked §8 in one boot.
- `[FILL-SHORT]`, `[E2-EOF]` and `[PMM-POISON]` all read **0** on these builds,
  so the kernel never noticed: the frame was freed and reused with the quarantine
  never firing on it. Whatever drops that last reference believes it is entitled
  to.

One refcount bug spotted while reading that path, unrelated to any of the above
and **not** yet fixed: `file_page_cache::insert` returns early when a peer has
already cached the key, but its `cow_ref_inc(frame.addr)` sits *after* the
closure and runs anyway — so a lost publish race leaks a reference on the
caller's private frame. That leaks rather than corrupts, which is why nothing has
noticed it.

### What this does not fix

- **`open(O_TRUNC)` on a mapped file still truncates in place.** Linux does the
  same (mappers take SIGBUS past the new EOF, where this kernel gives zeros), and
  `[O_TRUNC-ZAP]` fired twice in the whole actor arm, both on
  `.rustc_info.json`. Left alone deliberately; it is not the actor.
- **Only ext2 defers.** `overlay_fs`/`subdir_fs` delegate, so they inherit it for
  ext2-backed files; any future filesystem that frees inodes needs the same
  check.
- The `3/10 → 10/10` rate move of §14 is **still unexplained**, and this fix does
  not explain it.
- T1 (read/readv copy shortness) is still untested; T3's `SMP=1` wedge still
  blocks the single-core control.

### Verification

| Layer | Test | Result |
|---|---|---|
| `akuma-primitives` | 8 tests: refcounting, clone/drop, tombstone reuse, hash collisions, overflow is conservative *and* self-clearing | pass |
| `akuma-exec` | 8 tests: pins follow `push`/`remove`/`clear`/map-drop, fork propagation takes its own reference, same-VA replacement swaps the pin, munmap clip shapes and `mprotect` splits keep the count matching the pieces | pass |
| `akuma-ext2` | 6 tests: **data readable after unlink**, **number not reissued while pinned**, reclaimed once unpinned, unpinned unlink still frees immediately, repeated cycles do not exhaust the list, and a freed number is reported for cache invalidation — at drain time for a deferred free, not at unlink | pass |
| kernel boot suite | `test_unlinked_inode_survives_while_pinned` — the whole path against the live VFS | `PASSED (inode 36 kept its 21 bytes across unlink, not reissued)` |

555 host tests and 275 boot-suite tests pass; kernel `clippy` clean.

**Workload level: not validated, and not claimed.** A 3-build arm on the fixed
kernel scored 1 green; the 4-build as-found control scored 1 green. The fix
closes a defect its tests prove is closed, and moves the self-host build's
success rate not at all — because what dominates that workload now is the
premature-free class above, which this fix does not touch. Anyone re-scoring
should run ten builds per arm (rule 1) on a freshly repaired image (rule 12).

The counters to watch are in the `[Mem]` line: `pin=` (inodes held by live
mappings, rises and falls with the build), `pin_ovf=`, `defer=` (should drain to
0) and **`defer_leak=`, which must stay 0**.

## Background

- [`ZERO_PAGE_ICE_FIX.md`](ZERO_PAGE_ICE_FIX.md) — **the fix record**, and the short
  way in: both root causes, the elimination table, the theories that stayed open,
  the SMP=1 blocker, and the method rules this hunt paid for
- [`HANDOFF_MAPPED_PAGE_PREMATURE_FREE.md`](HANDOFF_MAPPED_PAGE_PREMATURE_FREE.md) —
  **the live handoff that follows this one**: quarantine poison inside mapped file
  pages, now the dominant self-host build failure
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
