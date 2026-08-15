# The `[0,0,0,0]` self-host ICE — the fix record

> **Both root causes found and fixed, 2026-08-15.** What is left of this document
> is the summary, the elimination table (so nothing here gets re-litigated), the
> theories that stayed open, and the method rules the hunt paid for.
>
> 1. **Prefault file fills ran through a stub hook** — `read_at_by_inode` had been
>    registered as `Err(-1)` since a March crate extraction, and the call site
>    dropped the result, so every prefault of a file-backed page installed a silent
>    zero page (856/build). Green builds 0/10 → 3/10.
>    [`PREFAULT_INODE_STUB_ZERO_PAGES.md`](PREFAULT_INODE_STUB_ZERO_PAGES.md).
> 2. **The inode lifecycle was not honored across `mmap`** — `unlink` freed and
>    reissued an inode a live mapping still named, giving that mapper `Ok(0)` zero
>    pages or, after reuse, another file's bytes. Fixed with a pin on the mapping
>    plus a deferred free in ext2.
>    [`SELFHOST_ZERO_PAGE_HUNT.md`](SELFHOST_ZERO_PAGE_HUNT.md) §14–§15.
>
> **The ICE is not *proven* closed, and the self-host build still fails.** What
> dominates that workload now is a different, older defect — quarantine poison
> appearing inside mapped file pages — which has its own live handoff:
> [`MAPPED_PAGE_PREMATURE_FREE_FIX.md`](MAPPED_PAGE_PREMATURE_FREE_FIX.md).
> Start there, not here.

## The symptom

In the Akuma kernel repo (bare-metal Rust, AArch64, QEMU virt), the self-hosted build
failed on essentially every clean build. `rustc`, running **inside** the guest, panicked:

```
thread 'rustc' panicked at rustc_metadata/src/rmeta/def_path_hash_map.rs:56:13:
decode error: Expected header tag [79, 68, 72, 84] but found [0, 0, 0, 0]
```

`[79,68,72,84]` is ASCII **`ODHT`**, the on-disk hash-table header in crate metadata.
This is **never a compiler bug** — it means the kernel handed `rustc` zeroed memory
where real bytes belonged.

It failed on `enumn` and `zerocopy-derive`, **in the same second**, every time. Those are
the only two proc-macro crates cargo builds in parallel at that stage and the only two
that depend on `syn`. The pairing is a scheduling coincidence, not a fact about those
files — a session spent hours on file-side theories because of it, and the eventual
root causes were in the mapping path both times.

## Reproduce (still the way to re-score any of this)

```bash
scripts/build_devbox_smoltcp.sh
overlays/devbox/run-smoltcp.sh            # boots devbox.img, SMP=4, MEMORY=4096, ssh on :2222
# in-guest, cwd MUST be the manifest dir:
cd /tmp/akuma && cargo clean && \
  RUSTC=/usr/local/bin/rustc /usr/local/bin/cargo build --release -p akuma -j4 --offline
```

- **`cargo clean` is mandatory.** It is the only thing that makes cargo recompile
  `enumn`/`zerocopy-derive`. A green *incremental* build proves nothing.
- The `ssh` CLI is blocked by policy here; drive it from Python `subprocess`.
- Poll the console log for `sshd started|Started sshd` to detect boot. `grep` is
  `ugrep` — **pass `-a`** or it silently matches nothing on these logs.
- The kernel artifact is `target/aarch64-unknown-none/release/akuma`, **not**
  `target/release/akuma`.

## Already eliminated — do not re-litigate without new evidence

Each was measured by an instrument reading **zero on builds that reproduced the ICE**.
All instruments are still in the tree.

| Theory | Instrument | Reading |
|---|---|---|
| Fill read short/errored (**demand-fault** site) | `[FILL-SHORT]` / `DP_FILE_FILL_SHORT` | 0 |
| Prefault file fill short/errored (`prefault_user_range`) | `[FILL-SHORT/prefault]` / `DP_PREFAULT_FILL_SHORT` | **856/build on the as-found kernel — ROOT CAUSE #1, found + FIXED 2026-08-15** (`docs/archive/PREFAULT_INODE_STUB_ZERO_PAGES.md`): the `read_at_by_inode` runtime hook was an `Err(-1)` stub and the call site dropped the result. Green builds 0/10 → **3/10**. |
| `file_page_cache` serves wrong bytes | `[FPC-BAD]` (`config::FPCACHE_VERIFY_HITS`, re-reads every hit from disk) | 0 across **4.3M hits** |
| `PROT_NONE` file region zero-filled | `[DA-NONE-FILE]` | 0 |
| `MADV_DONTNEED` on a file page | `[DONTNEED-FILE]` | 0 |
| Write path corrupts output | 46 MB `cp` + `md5sum` in-guest | byte-exact |
| rlib has holes on disk | offline block-map walk, 1540 blocks | 0 holes |
| mmap wrong end-to-end, cross-process | `bootstrap/bin/fpcpoison`, 4 concurrent procs over `libsyn` | ALL PASS |
| `lto="thin"` cost | host A/B | +1.4% RSS, no cliff |

**Two real bugs were found, fixed, and changed nothing** — do not read "I fixed a real
bug" as progress on this one:

1. `sys_munmap`'s whole-region arm freed the frame its *region record* named instead of
   the one the live PTE held (~11k times per build). `[PMM-POISON]` 18 → 0, rustc
   `signal: 11` 4/10 → 0/10. ICE unchanged.
2. `MAP_SHARED|MAP_ANONYMOUS` was CoW-copied by `fork` instead of shared. Fixed. ICE
   unchanged.

## Root cause #2, and the theories that were competing to explain it

The prefault inode-stub fix (see the elimination table) moved green builds 0/10 →
3/10 but did **not** close the ICE. The residue was **scored by failure mode**
(5-build arm on the fix kernel, full cargo logs kept in-guest):

- **Mode A** — `enumn`+`zerocopy-derive` (the usual pairing),
  `rustc_serialize/src/serialize.rs:402` `assertion failed: bytes[len] == STR_SENTINEL`
  and `:136` invalid `Option` discriminant.
- **Mode B** — `akuma-exec` itself, `rustc_type_ir/src/ty_kind.rs:145`
  `invalid enum variant tag while decoding TyKind, expected 0..29, actual 102`.

**The residue is garbage bytes, not zero pages** (`102` = `'f'`; a decoded
string's length prefix lied). Metadata written seconds earlier by a concurrent
rustc decodes wrong — a writer/reader coherence class, not the zero-page class
of the original ICE. The same arm's kernel log holds **313
`[FILL-SHORT] got=Ok(0)`**: demand fills hitting ext2's EOF arm at offsets the
mmap-time `filesz` said were in-file (one victim inode is now a 498-byte
fingerprint JSON — rewritten-smaller or inode-reused, not yet distinguished).
And a heavily-instrumented arm (every ext2 block-cache hit re-read from disk)
went **4/4 green with every instrument silent** — that perturbs I/O timing
enough to close a race window, so it establishes *timing sensitivity*, not a
fix.

**That counters-only arm has since run** (`E2_VERIFY_HITS` off, ten builds,
hunt §14) and reported **10/10 green with zero decode errors — while still
firing 376 `[FILL-SHORT] got=Ok(0)`, on green builds included.** Take the
mechanism from it (§14 root-causes the flood, below), not the rate: the §12 fix
arm scored 3/10 green and this one 10/10 with *only instrumentation* between
them, which is unexplained, and 11 consecutive green builds is a streak, not a
fix. The one confound whose direction is known — the image degrading across the
day — points the wrong way.

**T5b — root cause of the `Ok(0)` flood, CONFIRMED (§14) and FIXED (§15),
2026-08-15 night. The inode lifecycle was not honored across `mmap`.**

`LazySource::File` stores a raw inode number and the mmap-time `filesz` and
holds **no reference on the file** (Linux pins the inode through the mapping's
`struct file`; this kernel drops every reference at mmap time). Meanwhile
`sys_openat`'s `O_TRUNC` arm calls `write_file(path, &[])`, which ext2 applies
**in place** — same inode, blocks freed, `i_size = 0` — and `sys_unlinkat` frees
the inode outright for immediate reuse. A dependent that mapped the artifact
then faults and gets:

- `i_size = 0` → `read_at_by_inode` returns `Ok(0)` → the page stays zero, **or**
- the inode number already reused → **another file's bytes** → Mode A/B garbage.

Both residue modes, one defect. Evidence: all 32 `[E2-EOF]` prints read
`size_now=0x0`; victims are build artifacts written once and mapped by
dependents (top victim: `libzerocopy_derive-….so`, 183 fills); `[UNLINK]` names
the same paths earlier in the same log; `[FTRUNC-0]` never fires, so the actor
is `unlink`/`O_TRUNC`, not `ftruncate`.

**The fix (option 1, hunt §15).** `LazySource::File` now carries an
`akuma_primitives::InodePin`; ext2's `remove_file` drops the name but defers the
truncate and the bitmap free while the inode is pinned, completing them on a
later unlink or inode allocation. The pin is maintained by `InodePin`'s
`Clone`/`Drop` rather than by any call site, so fork propagation, `mprotect`
splits, all four `munmap` clip shapes, `exec`'s clear and `Process::drop` are
balanced by construction. The table is lock-free because `is_pinned` is read
under ext2's state write lock while pins are taken on the fault path.

Watch `defer_leak=` in the `[Mem]` line — it must stay 0. `pin_ovf=` non-zero
means the pin table filled and every inode is being treated as pinned.

- **T5a — demoted, not closed**: `vfs::write_at` writes, *then* invalidates
  `file_page_cache`, so a demand fill interleaved between reading the old disk
  bytes and publishing its frame lands that stale frame *after* the invalidate.
  Plausible, still unverified — and no longer *needed*, since T5b's inode-reuse
  leg explains the garbage-byte modes on its own.

### Theories that were never resolved — still live for any future sighting

These were ranked against the residue and overtaken by T5b before any of them was
tested. None is eliminated; each is here so the next zero-page or garbage-byte
sighting starts from the ranking rather than from scratch.

**T1 — `sys_read`/`sys_pread64`/`readv` short-copies into the user buffer.** The
strongest untested candidate for the *zero-page* shape. Commit `edd91fe7 "safer
memory helpers"` rewrote **every**
user-memory copy on this branch, and nothing has instrumented the read path's *copy*
half. If the syscall returns `n` but copies fewer than `n` bytes out, the tail of a
freshly-allocated (zero) buffer stays zero — exactly `[0,0,0,0]` where a header belongs,
with no error anywhere. Start by counting bytes-read against bytes-copied and printing
on mismatch. `ld` issues ~13,000 `readv`s per link, so the iovec walk matters as much as
plain `read`.

**T2 — an anonymous heap page reads back as zeros.** Same end state as
`docs/archive/CARGO_HEAP_NULL_RC.md` (cargo reading a zeroed `Rc` out of its own heap),
whose *sharing* half was fixed but whose class is open. `rustc` materialises metadata
into its heap. Note the scored residue is **garbage bytes**, which a zeroed-anon-page
mechanism does not produce — T2 explains only a zero-page shape and is therefore
demoted below T5 for the residue (it stays live for any future zero-page sighting).

**T3 — a missing barrier / memory-ordering bug under SMP.** Two rustc processes fail in
the same second. If a frame is zeroed on one core and published before the zeroing is
visible on another, a reader sees zeros. **Decisive experiment: does it reproduce at
SMP=1?** Currently blocked — see below.

**T4 — the ext2 block cache under concurrent readers.** Keyed on physical block number;
`write_block` evicts correctly, but it has not been tested under concurrency. Weakened
by `fpcpoison` passing, not eliminated.

## The blocker that stayed unresolved

**`SMP=1` hard-wedges the box.** `SMP=1 overlays/devbox/run-smoltcp.sh`, same workload:
build 1 completed in 1012 s, build 2 wedged. QEMU pinned at **98% CPU**, console frozen,
last line:

```
[AS-NEW] pid=86 l0=0x80d2e000 asid=0x6e via=clone parent=81
```

Same shape as "Defect A" in `docs/runbooks/selfhost-kernel-build.md` but at SMP=1, where
it had not been recorded before. This blocked T3 **and** every single-core control the
other theories wanted, and it is still open — worth fixing before any arm that needs to
compare one core against four. To inspect a wedge you must boot with `GDB=1`; the
gdbstub is armed at launch, so a VM already wedged is uninspectable.

## Method rules — these were learned expensively

1. **Ten clean builds per arm, minimum.** One arm opened with 3 clean runs (looked
   fixed), then failed 6 in a row. At a ~60% rate, 3 clean runs happen ~6% of the time.
2. **Run the as-found control FIRST.** A whole hunt ran without one; when finally
   measured, the unmodified branch reproduced everything, proving both fixed bugs
   pre-existed. Pick the baseline by *booting candidates* — the obvious parent commit
   (`d2c312bb`) does not boot at all.
3. **Score failure modes separately, and check which crate died.** "Build failed"
   conflated two bugs. Builds killed early (at `proc-macro2`/`quote`) never reach the
   crates that trip the ICE and were miscounted as "no ICE". The ICE looked like 60%;
   it is ~100% conditional on reaching `syn`.
4. **Build wall time is not a metric here** — ±4× variance on identical code
   (42–197 s, unmodified tree included). An arm with implausibly *low* variance is
   suspect, not good news.
5. **An instrument can be broken by the bug it hunts.** `fpcpoison`'s cross-process gate
   used `MAP_SHARED` anonymous memory, which this kernel had silently broken — so its
   "concurrent" rounds ran unsynchronised while still printing `ALL PASS`. Never
   coordinate probe processes through `MAP_SHARED` anon here; use pipes. Bound every
   spin — unbounded, this hung the box instead of warning.
6. **A missing magic number proves nothing without a known-good sibling.** "The guest's
   rlib has no `ODHT`" looked decisive; all four guest rlibs lack it (metadata is
   compressed) and the host comparison was across rustc versions.
7. **Fixing a plausible bug is not evidence it was the bug.** Two real fixes moved
   nothing. Require the ICE rate to change. (The prefault fix on 2026-08-15 was the
   first that did — 0 → 3/10 green.)
8. **A fault-path instrument cannot see a page another path made present.**
   `[FILL-SHORT]` read 0 across reproducing builds and was cited as exonerating
   short fills — but it sat on the demand-fault fill, while `prefault_user_range`
   installed 856 zero-filled pages per build through a site no fault ever
   re-visits. When two paths can produce the same end state, enumerate the sites
   ("who can install this page?") before trusting an exoneration on one of them.
9. **A crate extraction that leaves a stub behind a fn-pointer hook silently
   corrupts every consumer.** `read_at_by_inode` became `Err(-1)` in
   `94d1daf6` because the real VFS needed a `path` the hook signature lacked —
   and its one consumer dropped the result. When a path crosses a runtime-hook
   boundary, grep the registrations for stubs first; that check costs one
   command and would have closed this hunt in March.
10. **An instrument that perturbs the system cannot adjudicate a fix.** The
    cache-hit verifier (re-read every hit from disk) went 4/4 green with every
    instrument silent — but doubling I/O on the hot path serialises exactly the
    interleavings a coherence race needs, so that arm measured the perturbation,
    not the kernel. Rule 4's cousin: low-variance arms are suspect, and so are
    implausibly green ones on an instrumented build. Rate-score on a kernel
    whose instrumentation is counters-only.
11. **A green build whose instrument fired is not a passing build.** The
    counters-only arm went 10/10 green while `[FILL-SHORT] got=Ok(0)` fired 376
    times — on green builds included. A zero page lands where it lands; whether
    it lands somewhere fatal is luck, which is why the rate swings so hard.
    Score the *instrument*, not just the exit code — and take the exit code from
    the guest's own build log, never the harness's: several "FAILED" labels in
    the actor arm were ssh timeouts with no compiler error at all.
12. **Repair the image before rate-scoring anything.** 30+ clean-build cycles
    left `devbox.img` with unattached inodes and wrong free-block counts, and
    boot degraded to 15+ minutes behind a ~1900-line watchdog storm. `e2fsck -fy`
    it (recipe in `docs/runbooks/selfhost-kernel-build.md` §5.5) and re-run until
    it exits 0. An arm run on a damaged image is uninterpretable.

## Tools already in the tree

- `scripts/ext2read.py` — read `devbox.img` **offline**, no VM running: ICE dumps,
  `.git/HEAD`, build artifacts. Do this before booting anything.
- `bootstrap/bin/fpcpoison` — cross-process file-mapping integrity probe (fix its gate
  to use pipes first).
- `bootstrap/bin/shmanon` — `MAP_SHARED` anon fork semantics, both legs.
- `akuma_pmm::FreeSite` — free-site attribution in the quarantine ledger; `[PMM-POISON]`
  prints `site=`. This named a UAF culprit in **one boot** after four arms of guessing.
  Reach for attribution instrumentation earlier than feels justified.
- Counters in the `[Mem]` line: `fill_short`, `unpub`, `fpc_bad`, `pn_file`,
  `munmap_stale`, `pf_fill_short`; `[FPCACHE]` carries `evict_mapped`. Note the
  `dp_counters_line` dump only surfaces from the exceptions path on the devbox
  profile — the console prints (`[FILL-SHORT]`, `[FILL-SHORT/prefault]`) are the
  operative guards.
- `[E2-EOF]` — ext2 `read_at_by_inode`'s EOF arm **with `offset > file_size`**
  (a plain read-at-end is `offset == file_size` and is deliberately silent and
  uncounted, or the counter is pure noise). Prints the size the reader actually
  saw; counter `E2_READ_AT_EOF`, prints capped at 32 — **the cap is not a
  count**, read the counter.
- `[E2C-BAD]` — ext2 block-cache hit vs. direct disk re-read (counter
  `E2_CACHE_VERIFY_MISMATCH`), behind `ext2::E2_VERIFY_HITS`, **default off and
  load-bearing**: the re-read doubles I/O on the hot path and perturbs the
  timing it observes (the 4/4-green arm above). Turn it on only to chase T4, and
  never trust a rate scored with it on.
- `[UNLINK]`, `[O_TRUNC-ZAP]`, `[FTRUNC-0]` — the actor prints that caught T5b
  (`src/syscall/fs.rs`), behind `config::SYSCALL_DEBUG_IO_ENABLED`. `[UNLINK]`
  is `target`-scoped and still runs ~1000+/build, so expect a large log.
- `[FILL-SHORT]` variants print `path=` — this is what named the victims.

## Full history

`docs/archive/SELFHOST_ZERO_PAGE_HUNT.md` — every arm, every measurement, the wrong fix
kept deliberately as a worked example; §12 and
`docs/archive/PREFAULT_INODE_STUB_ZERO_PAGES.md` cover the root cause found 2026-08-15;
§13 scores the residue (garbage-byte modes, the Ok(0) flood, the timing-perturbation
result) and ranks T5; **§14 root-causes the flood to T5b** (inode lifecycle not honored
across `mmap`) with the actor caught in the act; **§15 is the fix** — the pin, the
deferred free, the page-cache invalidation it required, and the poison evidence that
became the next hunt. `docs/archive/FPCACHE_EVICTION_PREFERS_UNMAPPED.md`
covers a cache-policy bug found along the way.
`docs/archive/MAPPED_PAGE_PREMATURE_FREE_FIX.md` is the live handoff that follows
this one.

**State: both known root causes fixed; the ICE itself is not proven closed.**

- **Fixed:** prefault inode-stub zero pages (§12) — 0/10 → 3/10 green.
- **Fixed:** T5b, the inode lifecycle across `mmap` — diagnosed §14, fixed §15.
  Mappings pin their inode (`akuma_primitives::InodePin` inside
  `LazySource::File`), and ext2 drops the name but defers the truncate and the
  bitmap free while an inode is pinned. 21 host tests plus a boot self-test
  (`unlinked_inode_survives_while_pinned`) cover it.
- **Unresolved:** the last three arms went 14/14 green with no decode error
  (10 counters-arm + 1 actor-arm + 3 post-repair sanity) — the longest clean
  streak of the hunt — but the defect still fired on those green
  builds, and the 3/10 → 10/10 move came with no functional change. That move is
  still unexplained, so treat the ICE as **latent rather than proven gone**, and
  re-score only on a freshly `e2fsck`-repaired image (runbook §5.5). A green
  build whose instrument fired is not a passing build (rule 11).
- **Now dominant, and where to go next: the premature-free class.** On the
  post-fix workload both arms (fixed 1/3 green, as-found control 1/4) fail the
  same way — the linker rejecting a freshly built `.rlib` whose bytes contain
  **PMM quarantine poison** (`0xFEEDFACE…`, seen as `invalid sh_name
  (0x5000feed)` and `\x00P\x8eA\xce\xfa\xed\xfe`). A *file-backed mapped page* is
  being freed while still mapped, with `[PMM-POISON]` never firing. The poison
  names its own frame (`^ 0xFEEDFACEDEAD0000`), so one reproduction plus
  `report_poison_value`/`FreeSite` should name the free site — that instrument
  cracked §8 in a single boot. Start there, not on the ODHT decode error.
- **Still open:** `open(O_TRUNC)` truncates a mapped file in place (matches
  Linux, and it was never the actor); T1 (read/readv copy shortness) untested;
  T3 blocked by the `SMP=1` wedge; T4 weakened; T5a demoted and no longer needed
  to explain anything. Also unfixed: `file_page_cache::insert` increments the
  frame refcount even on the lost-publish-race path that returns early — a leak,
  not corruption.
