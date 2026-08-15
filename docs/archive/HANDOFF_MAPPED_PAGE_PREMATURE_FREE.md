# Handoff prompt — poison bytes inside a mapped file page (self-host build)

*Paste everything below the line into a fresh session. It is written to be
self-contained; the referenced docs add depth but are not required to start.*

---

## The task

In the Akuma kernel repo (bare-metal Rust, AArch64, QEMU virt), the self-hosted
build fails most of the time. The dominant failure is **not** a compiler bug and
**not** the old `[0,0,0,0]` ODHT ICE — it is the linker rejecting a `.rlib` that
was written correctly seconds earlier:

```
rust-lld: error: a section [index 30] has an invalid sh_name (0x5000feed)
          offset which goes past the end of the section name string table

rust-lld: error: …/libzerocopy-….rlib: Archive::children failed: truncated or
          malformed archive (terminator characters in archive member "\216A" …
          for the archive member header for \x00P\x8eA\xce\xfa\xed\xfe…)

rust-lld: error: …/libakuma_exec-….rlib(….rcgu.o): invalid sh_type for string
          table section [index 1]: expected SHT_STRTAB, but got Unknown
```

**Read the bytes.** `\x00P\x8eA\xce\xfa\xed\xfe` little-endian is
`0xFEEDFACE418E5000`, and `0x5000feed` is the same value seen at a different
alignment. That is **`akuma_pmm` quarantine poison**, which the PMM writes into a
frame when it is freed.

So: **a physical frame backing a file-backed mapping was freed, and poisoned,
while the linker still had it mapped.** The linker then read the poison as file
content. Your job is to find who frees it.

This is a use-after-free with an unusually generous witness: the poison is
self-identifying and names its own frame.

## Why this is the right thing to chase

- It reproduces on **both** the current tree and the as-found control at the same
  rate (~1 green in 3–4 clean builds), so it is not a recent regression.
- The two root causes of the older ODHT ICE are both fixed
  (`docs/archive/SELFHOST_ZERO_PAGE_HUNT.md` §12 and §15), and neither moved this.
- Every other instrument is silent on these builds: `[FILL-SHORT]` **0**,
  `[E2-EOF]` **0**, `[PMM-POISON]` **0**. The kernel does not notice.

That last point is the interesting one. The quarantine's own detector never
fires, so whatever drops the final reference believes it is entitled to.

## Decode the poison first — it names the frame

`akuma_pmm`'s codec is `poison = 0xFEEDFACEDEAD0000 ^ pa`, so:

```
0xFEEDFACEDEAD0000 ^ 0xFEEDFACE418E5000 = 0x9F235000
```

`pmm::report_poison_value(tag, word)` already does this decode and prints the
frame, who freed it, and how its refcount reached zero — it reads
`akuma_pmm::FreeSite` out of the quarantine ledger. **That instrument named a
UAF culprit in one boot** during an earlier arm of this hunt
(`SELFHOST_ZERO_PAGE_HUNT.md` §8). Reach for it immediately rather than reasoning
about the code first; this codebase has repeatedly punished the reverse order.

The gap to close: the poison is currently only decoded when the *kernel* faults
on it. Here it is read successfully by userspace as ordinary file data, so
nothing triggers a report. **Getting a `FreeSite` for the frame is the whole
first milestone.**

Suggested first instrument — cheap, and it will name the site directly: on the
demand-fault file-fill path (`src/exceptions.rs`, `demand_page_lazy_region`), after
filling a page from the file, check whether the just-read bytes decode as poison
(`pmm::poison_word_frame`) and print the frame plus its free record if so. A file
fill that reads poison out of the block layer means the *cache* handed over a
freed frame; one that produces poison only later means the frame was freed after
installation. Those are different bugs and this distinguishes them in one boot.

## Where to look — ranked

The frame is a **file page**, which narrows this a great deal. Anonymous-memory
and heap theories are out; anything that can drop a reference on a shared file
page is in.

1. **`src/file_page_cache.rs` refcounting.** The module's invariant is "the cache
   holds exactly one reference per entry, mappers hold their own". Three sites
   drop that reference — `insert`'s eviction, `invalidate_inode`, `shrink` — and
   each calls `pmm::free_page_at`, which is supposed to free only when nobody
   still maps it. Verify that invariant actually holds rather than assuming it.
   **A known, unfixed leak lives here and shows the accounting is not airtight:**
   `insert` returns early when a peer already cached the key, but its
   `cow_ref_inc(frame.addr)` sits *after* the closure and runs anyway. That one
   leaks rather than corrupts — but a mirror-image slip on the other side of the
   same accounting would be exactly this bug.
2. **CoW refcounts on shared file pages** (`akuma_pmm::cow_ref_dec` callers).
   A file page mapped by several processes is shared, not copied; a double
   decrement on unmap frees it under the remaining mappers. `docs/archive/
   COW_PILE_AUDIT.md` §5.6 is the refcount-underflow class.
3. **`munmap` / process teardown** dropping a reference the mapping did not own.
   §8 of the hunt doc fixed one of exactly this shape (`sys_munmap` freeing the
   frame its *region record* named instead of the one the live PTE held). The
   class is not proven exhausted.
4. **Eviction under pressure** — `reclaim_clean_file_pages` and `shrink` both
   free file pages deliberately. A mapped page must survive both; `EVICTIONS_MAPPED`
   in the `[FPCACHE]` line counts when the cache evicts a still-mapped entry.

## Reproduce

```bash
scripts/build_devbox_smoltcp.sh
overlays/devbox/run-smoltcp.sh        # devbox.img, SMP=4, MEMORY=4096, ssh on :2222
```

Then in-guest, from the manifest directory:

```bash
cd /tmp/akuma && cargo clean && \
  RUSTC=/usr/local/bin/rustc /usr/local/bin/cargo build --release -p akuma -j4 --offline
```

- **`cargo clean` is mandatory** — an incremental build proves nothing.
- Drive ssh from Python `subprocess`; the `ssh` CLI is blocked by policy here.
- Run the build **detached** (`nohup … > /tmp/buildN.log`) and poll for a
  `.rc` file. An ssh session that times out mid-build gets scored as a failure
  with no error text, which has already produced one wrong conclusion.
- Poll the console log for `sshd started|Started sshd`; `grep` is `ugrep`, so
  **pass `-a`** or it silently matches nothing on these logs.
- If `/tmp/akuma` is missing or empty, re-stage it: `cp -a /root/akuma/. /tmp/akuma/`.

## Method rules — every one of these was paid for

1. **Ten clean builds per arm.** Non-negotiable. During the session that produced
   this handoff, a **single** as-found control build came back green against two
   red builds on the changed kernel, and the obvious conclusion — "you caused a
   regression" — was **wrong**: extending that control to four builds gave 1 green
   / 4, statistically identical to the changed kernel's 1 / 3. One build is not a
   control arm.
2. **Run the as-found control anyway**, and run it *first*. It is the only thing
   that tells a pre-existing failure from one you just introduced.
3. **Score failure modes separately, and name the crate that died.** "Build
   failed" has conflated distinct bugs repeatedly here. Take the exit status from
   the guest's own build log, never from the harness.
4. **A green build whose instrument fired is not a passing build.** A counters-only
   arm once went 10/10 green while `[FILL-SHORT] got=Ok(0)` fired 376 times, on the
   green builds included. Score the instrument, not just the exit code.
5. **An instrument that perturbs the system cannot adjudicate a fix.** Turning on
   `ext2::E2_VERIFY_HITS` (re-read every block-cache hit from disk) took an arm to
   4/4 green and proved nothing — it doubles I/O on the hot path and serialises the
   interleavings a race needs. Leave it off when scoring rates.
6. **Repair the image before measuring.** Dozens of hard-killed build cycles leave
   `devbox.img` genuinely damaged, and the only symptom is boot degrading to 15+
   minutes behind a watchdog storm — which reads like a kernel regression and is
   not. `e2fsck -fy -D` from a container with `e2fsprogs` (macOS has none), re-run
   until it exits **0**. Recipe: `docs/runbooks/selfhost-kernel-build.md` §5.5.
   Note the repair costs the in-guest `/tmp/akuma` tree, so re-stage and budget a
   cold rebuild.
7. **Fixing a plausible bug is not evidence it was the bug.** Several real fixes
   in this hunt moved the failure rate zero. Require the rate to change.
8. **When two paths can produce the same end state, enumerate the sites before
   trusting an exoneration on one of them.** `[FILL-SHORT]` read 0 across
   reproducing builds and was cited as clearing short fills — while a *different*
   fill site was installing 856 zero pages per build through a path no fault ever
   revisits.

## Tools already in the tree

- `akuma_pmm::FreeSite` + `[PMM-POISON]` — free-site attribution in the quarantine
  ledger. **Start here.**
- `pmm::report_poison_value` / `poison_word_frame` — decode a suspicious qword to
  the frame it belonged to.
- `scripts/ext2read.py` — read `devbox.img` **offline**, no VM running. Pull the
  corrupt `.rlib` out and look at the poison in situ before booting anything.
- `[FPCACHE]` PSTATS line — `evict`, `evict_mapped`, `invalidations`.
- The `[Mem]` counters line — `fill_short`, `unpub`, `fpc_bad`, `pn_file`,
  `munmap_stale`, `pf_fill_short`, and the inode-lifecycle guards `pin`,
  `pin_ovf`, `defer`, `defer_leak` (`defer_leak` must stay 0).
- `config::FPCACHE_VERIFY_HITS` — re-reads every cache hit from disk and compares.
  Off by default; see rule 5 before switching it on.

## Background

- `docs/archive/SELFHOST_ZERO_PAGE_HUNT.md` — the full hunt. §8 is the earlier UAF
  of this family and the `FreeSite` instrument that cracked it in one boot; §14–§15
  are the inode-lifecycle root cause and fix; §15's closing sections describe this
  poison evidence and the false-regression scare in detail.
- `docs/archive/ZERO_PAGE_ICE_FIX.md` — the previous handoff, for the ODHT ICE.
- `docs/archive/CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md` — the premature-free class
  as previously audited; §13 ruled out PMM-level UAF *at that time*, and this
  evidence says it needs revisiting.
- `docs/archive/COW_PILE_AUDIT.md` §5.6 — the refcount-underflow class.
- `docs/runbooks/selfhost-kernel-build.md` — the build itself, and §5.5 for image
  repair.

**State:** the ODHT ICE's two known root causes are fixed. This one is open, is
the dominant failure on the self-host workload, and has never been instrumented.
Get a `FreeSite` for the poisoned frame and the rest should follow quickly.

---

## Session log 2026-08-15 — PMM analysis, W1/W2 fixes, resurrection detector

Branch `trim-some-more-fat`, devbox-smoltcp SMP=4 MEMORY=4096, booted with
`GDB=1` (lldb → gdbstub :1234). The trimmed tree's in-guest kernel build is now
~35 units / ~1 m 35 s per clean round, so a 10-round arm fits in ~25 minutes.

### Baseline arm (as-found kernel, 10 clean rounds)

| round | rc | failure signature |
|---|---|---|
| 1 | 101 | `crate heapless required to be available in rlib format, but was not found in this form` |
| 2 | 101 | `libakuma_exec.rlib(…rcgu.o): sh_addralign is not a power of 2` + `sh_addralign is too large` |
| 3 | 0 | — |
| 4 | 101 | ld-musl: `Error relocating /usr/lib/libbfd-2.45.1.so: unsupported relocation type 1908400128` |
| 5 | 0 | — |
| 6 | 101 | `libakuma_exec.rlib(…rcgu.o): invalid sh_type for string table section [index 1]: expected SHT_STRTAB, but got Unknown` |
| 7 | 0 | — |
| 8 | 0 | — |
| 9 | 101 | `libelf.rlib(…rcgu.o): invalid sh_type … expected SHT_STRTAB, but got Unknown` |

Every failure is the same class — **a file-backed mapped page read back wrong
bytes** — through three doors: corrupted `.rlib` section headers at link time
(the handoff signature, verbatim), an rlib rejected wholesale, and the dynamic
loader reading garbage relocations out of a mapped `.so`. `libakuma_exec.rlib`
(the largest, longest-mapped rlib) is the repeat victim. **The kernel-side log
is completely silent on every red round**: 0 `[PMM-UAF]`, 0 `[PMM-POISON]`,
0 `[PMM-PREMATURE]`, 0 `[WILD-DA]`, 0 SIGSEGV — confirming the victim only
*reads* the corruption. The only kernel chatter is `[MUNMAP-STALE]`
(record/PTE disagreement, the *already-fixed* stale-record class, firing its
32-print rate limit immediately) and an `[FPCACHE]` line showing
`evict_mapped=132336 / evict=159576` — **83 % of cache evictions take a
still-mapped entry**, i.e. the eviction→`cow_ref_dec` machinery runs ~130 k
times per arm against live mappings.

### What the old logs pin down (2026-08 poison events, all `logs/*.log`)

- Decoding every `[PMM-POISON] … freed_by=(seq=A) now_seq=B` pair: the gap
  between the premature free and the victim's fault is **≤ 60 frees in every
  event but one, mostly ≤ 16**. The free and the fatal use are nearly
  simultaneous — a live race, not stale bookkeeping discovered late.
- Every attributed site is `munmap` / `munmap-region`.
- Several `[PMM-UAF]` events show `got = want − 1` (or `− 2`): a *decrement*
  through a stale mapping — the null-`Rc` refcount-dec shape.
- The `[WILD-DA]` autopsies include a **writable private** victim page
  (`AP_RW_ALL`, 2-page anon region, `[COW-HIST] dec 0->0`, `tracked=false`)
  freed by *another thread's* munmap. The file cache never serves writable
  pages, so at least one route is two address spaces tracking the same PA with
  **no `COW_REFCOUNTS` entry at all** — a share whose `cow_ref_inc` never
  happened, not a cache miscount.

### PMM-extraction exoneration

`free_page`'s pipeline in `crates/akuma-pmm` is semantically identical to the
pre-extraction `src/pmm.rs` (diffed against `eb19f23f~1`), and
`lookup_and_ref`'s inc-outside-lock shape predates the extraction. The
extraction is not the regression; the class predates it, matching this doc's
same-rate control measurement.

### Fixes applied this session

1. **W1** — `file_page_cache::lookup_and_ref` now takes the mapper's
   `cow_ref_inc` **inside** the `PAGES` hold. Every free path removes the
   entry under that same hold before dropping the cache's reference, so
   "entry present ⇒ count ≥ 1 ⇒ the inc cannot land on zero". This was the
   "D2" suspect: the old post-unlock inc could race `invalidate_inode` /
   eviction / `shrink`'s dec-to-zero and **resurrect a freed, poisoned frame**
   which the mapper then installed as file content — zero kernel-side
   symptoms, exactly this hunt's signature.
2. **W2** — `file_page_cache::insert` takes the cache's own `cow_ref_inc`
   inside the publish closure, and only when the entry was actually inserted.
   Closes the window where the entry was visible with no cache reference, and
   the lost-race leak (the early return used to inc anyway).
3. **Resurrection detector** — `akuma_pmm::cow_ref_inc` now distinguishes
   *creating* the entry from incrementing an existing one, and when a created
   entry's frame is currently parked in the quarantine
   (`QUAR_PRESENT` hit), prints `[PMM-RESURRECT]` with the frame's free record
   and CoW history, and counts it (`cow_resurrection_count()`). One relaxed
   atomic load, only on the inc-from-zero path — unlike
   `PMM_PREMATURE_FREE_CHECK` it cannot perturb the race. **This closes the
   observability gap this handoff opens with**: a W1-shape resurrection now
   names itself in one boot even though the victim never faults.

Tests: `akuma-pmm` host test (inc-on-parked-frame counts, no double-count),
boot-suite check in `process_tests.rs` (lost-race `insert` neither replaces
the entry nor touches the loser's refcount). Reference docs:
`docs/reference/subsystems/memory.md` → "Frame lifecycle: the free pipeline"
diagrams the whole free pipeline and windows W1–W6.

### Round 10's gift: the disk was never corrupt

Round 10's link error carried the raw bytes of the malformed archive members:
`…\xce\xfa\xed\xfe\x00\x30\x89\x18…` — **consecutive little-endian quarantine
poison words**. Decoding: `0xFEEDFACE18893000 → pa 0xC6243000` (in
`libelf.rlib`'s view) and `0xFEEDFACE18888000 → pa 0xC6258000`
(`libzerocopy.rlib`'s view) — two poisoned frames served as file content in
one round. But `od`-scanning both rlibs **on disk** (round 10's target
survived) found **zero** poison words. The corruption existed only in the
kernel's mapped/file-page-cache view; the file was always correct. This
exonerates the entire write path and pins the bug to a frame freed while
still being served for that file page — the cache-race shape exactly.
(The free ledger had long wrapped by the time it was dumped over the gdbstub
— 36 M frees per arm against a 4096-slot ring — which is why the
`[PMM-RESURRECT]` detector, which fires *at the moment of the race*, is the
right instrument and a bigger ring is not.)

### Post-fix arm — 10/10 GREEN

Same loop, same VM, same disk, kernel with the W1/W2 fixes and the detector:

```
rounds 1–10: rc=0   (baseline: 6 red / 4 green)
[PMM-RESURRECT] 0   [PMM-UAF] 0   [PMM-POISON] 0   [WILD-DA] 0
```

P(10 consecutive greens | baseline green rate 0.4) ≈ 0.4¹⁰ ≈ 1 in 10 000;
Fisher exact on 6/10 vs 0/10 red gives p ≈ 0.005. The rate moved — rule 7 is
satisfied for once. The boot suite on the default profile also passes with
the fixes (275 PASSED, 0 FAILED, including the new lost-race `insert` check).

**Attribution:** the race shipped with the cache's first landing
(`37be2087`, 2026-08-06, "more fixes and tests + forgotten file page cache")
— `lookup_and_ref` was born with the inc outside the `PAGES` hold, which is
why the as-found control always failed at the same rate as every later tree.

**Residual, deliberately left open:** the old `[WILD-DA]` autopsies include a
writable **private** victim page the cache never served (see "What the old
logs pin down"), so a rarer share-without-inc route (fork/CoW-share or
install race) likely survives these fixes. `[PMM-RESURRECT]` and the
`cow_resurrection_count()` counter are the standing tripwire; treat any
future firing as that route surfacing.
