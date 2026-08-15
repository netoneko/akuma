# `rustc` ICE "found [0, 0, 0, 0]" in the self-host build — an elimination record (2026-08-15)

> **Status: NOT FIXED. Four theories killed by measurement, root cause narrowed to a
> use-after-free.** The kernel's own quarantine instrument already reports it:
> *"the kernel FREED this frame while the process still had it"*. §6 has the evidence.
>
> **This document previously claimed a root cause and a fix. That claim was wrong**
> and is retained in §5 as a worked example of a plausible mechanism that measurement
> killed. Every instrument added along the way is permanent and is what made the
> elimination possible — read §7 before adding more.

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

**Result: not yet established.** The arm immediately before this change (the
`race_free` guard alone) still reproduced: 4/6 builds with the ODHT ICE and
`[PMM-POISON]` firing. The first 2 builds after the helper show **0 ICEs, 0 zeros
and 0 `[PMM-POISON]`** — but both still failed on the unrelated `signal: 11` class,
and *two runs is not a result*. Given the rates in §2 (the ICE at ~65%, `signal: 11`
on `main` too), separating "fixed" from "lucky" needs ~10 clean builds per arm.
Do not record this as a fix without them.

## 8. What is NOT done

1. **The premature free is not localised.** The instrument names the frame, the freeing
   tid and the free sequence number; it does not name the call site. The next step is a
   free-site tag in the quarantine record (`freed_by` currently carries only tid+seq),
   which turns "someone freed it" into "`munmap`/`complete_cow_break`/teardown freed it"
   in one boot.
2. **No fix.** Nothing in this hunt fixed the ODHT ICE. Two guards were added
   (`PROT_NONE`-file, `DONTNEED`-file) that are correct on their own terms but never
   fire on this workload; the `filesz` clamp likewise. Removing them is defensible.
3. **`signal: 11` is untouched** and is the more frequent failure. It reproduces on
   `main`.
4. **`fpcpoison` never completed a run in-guest.** 10 rounds × 4 procs over a 6.3 MB
   file is far too slow on a 4-core emulated guest, and its first version busy-waited on
   the start gate forever when the parent was killed — that wedged the box and cost five
   `kill -9`s. Both spins are bounded now (`GATE_SPIN_LIMIT`), but the probe still needs
   to be pointed at a *small* file, and it is aimed at the file-page cache, which §4
   exonerated. It is not the instrument this bug needs.

## Background

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
