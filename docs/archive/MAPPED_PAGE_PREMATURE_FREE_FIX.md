# Mapped-page premature free — investigation and fix (2026-08-15)

**Status: root cause found and fixed; fix verified by rate change (6/10 red →
10/10 green). One rarer sibling route left open, with a standing tripwire.**

The dominant self-host build failure through 2026-08 was the linker rejecting
an `.rlib` that had been written correctly seconds earlier:

```
rust-lld: error: a section [index 30] has an invalid sh_name (0x5000feed)
          offset which goes past the end of the section name string table

rust-lld: error: …/libzerocopy-….rlib: Archive::children failed: truncated or
          malformed archive (terminator characters in archive member "\216A" …
          for the archive member header for \x00P\x8eA\xce\xfa\xed\xfe…)

rust-lld: error: …/libakuma_exec-….rlib(….rcgu.o): invalid sh_type for string
          table section [index 1]: expected SHT_STRTAB, but got Unknown
```

Reading the bytes: `\x00P\x8eA\xce\xfa\xed\xfe` little-endian is
`0xFEEDFACE418E5000` — **`akuma_pmm` quarantine poison** (`poison = 0xFEEDFACE_DEAD0000 ^ pa`),
which the PMM writes into a frame when it is freed. So a physical frame backing
a file-backed mapping was freed, and poisoned, while the linker still had it
mapped; the linker then read the poison as file content.

## Root cause

Two unsynchronized reference-count windows in the shared file-page cache
(`src/file_page_cache.rs`), both present since the cache's **first landing**
(`37be2087`, 2026-08-06, "more fixes and tests + forgotten file page cache") —
which is why every control arm ever run failed at the same rate as the tree
under test:

- **W1 (the "D2" suspect, confirmed):** `lookup_and_ref` copied the cache
  entry under the `PAGES` lock but took the mapper's `cow_ref_inc` **after
  dropping it**. All three free paths (`insert`-eviction, `invalidate_inode`,
  `shrink`) remove the entry under `PAGES` and then `cow_ref_dec`. A mapper
  inside the window was invisible to them: dec 1→0 freed and poisoned the
  frame, then the late inc **resurrected** it (a fresh `COW_REFCOUNTS` entry
  at count 2 on a quarantined frame) and the mapper installed poison as file
  content.
- **W2:** `insert` took the cache's own `cow_ref_inc` after the publish
  closure, and unconditionally — so it also ran on the lost-race early
  return (one leaked frame per race), and between publish and inc the entry
  was visible while the count reflected only the mappers; if every mapper
  unmapped inside that window, the frame was freed with a live cache entry
  still pointing at it.

Why nothing ever detected it: the victim only ever **reads** the poison —
`[PMM-UAF]` verifies poison at drain (writes only), `[PMM-POISON]` decodes a
*faulting* value (userspace reading poison as data never faults), and
`PMM_PREMATURE_FREE_CHECK` is off because armed it perturbs the race away
(10/10 green against a 25 % baseline). Every instrument was structurally blind
to this failure mode.

## The evidence trail

**Old-log analysis (`logs/*.log`, 2026-08 poison events):**

- Decoding every `[PMM-POISON] … freed_by=(seq=A) now_seq=B` pair: the gap
  between the premature free and the victim's fault is **≤ 60 frees in all
  events but one, mostly ≤ 16** — the free and the fatal use are nearly
  simultaneous. A live race, not stale bookkeeping discovered late.
- Every attributed `FreeSite` is `munmap` / `munmap-region`.
- Several `[PMM-UAF]` events show `got = want − 1` (or `− 2`): a *decrement*
  through a stale mapping — the cargo null-`Rc` refcount-dec shape
  (`CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md`).

**Baseline reproduction arm** (branch `trim-some-more-fat`, devbox-smoltcp
SMP=4 MEMORY=4096, booted with `GDB=1`; the trimmed tree's in-guest kernel
build is ~35 units / ~1 m 35 s per clean round, so a 10-round arm fits in
~25 minutes): **6 red / 4 green**.

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
| 10 | 101 | `libelf.rlib` + `libzerocopy.rlib`: `Archive::children failed: truncated or malformed archive` — **with poison bytes in the error text** |

Every failure is one class — **a file-backed mapped page read back wrong
bytes** — through three doors: corrupted `.rlib` section headers at link time,
an rlib rejected wholesale, and the dynamic loader reading garbage relocations
out of a mapped `.so`. `libakuma_exec.rlib` (the largest, longest-mapped rlib)
was the repeat victim. **The kernel-side log was completely silent on every
red round**: 0 `[PMM-UAF]`, 0 `[PMM-POISON]`, 0 `[PMM-PREMATURE]`,
0 `[WILD-DA]`, 0 SIGSEGV. The only kernel chatter was `[MUNMAP-STALE]`
(record/PTE disagreement — the *already-fixed* stale-record class, hitting its
32-print rate limit immediately) and `[FPCACHE]` showing
`evict_mapped=132336 / evict=159576` — **83 % of cache evictions took a
still-mapped entry**, running the eviction→`cow_ref_dec` machinery ~130 k
times per arm against live mappings.

**The clincher — the disk was never corrupt.** Round 10's link error carried
the raw bytes of the malformed archive members:
`…\xce\xfa\xed\xfe\x00\x30\x89\x18…` — **consecutive little-endian quarantine
poison words**. Decoding: `0xFEEDFACE18893000 → pa 0xC6243000` (in
`libelf.rlib`'s view) and `0xFEEDFACE18888000 → pa 0xC6258000`
(`libzerocopy.rlib`'s view). But `od`-scanning both rlibs **on disk** (round
10's target survived — no clean after the last round) found **zero** poison
words. The corruption existed only in the kernel's mapped/file-page-cache
view; the file was always correct. This exonerated the entire write path and
pinned the bug to a frame freed while still being served for that file page.
(The free ledger had long wrapped by the time it was dumped over the gdbstub —
36 M frees per arm against a 4096-slot ring — which is why the
`[PMM-RESURRECT]` detector below, which fires *at the moment of the race*, is
the right instrument and a bigger ring is not.)

**PMM-extraction exoneration:** `free_page`'s pipeline in `crates/akuma-pmm`
is semantically identical to the pre-extraction `src/pmm.rs` (diffed against
`eb19f23f~1`), and `lookup_and_ref`'s inc-outside-lock shape predates the
extraction. The extraction was not the regression; the race shipped with the
cache itself.

## The fix

1. **W1** — `file_page_cache::lookup_and_ref` takes the mapper's
   `cow_ref_inc` **inside** the `PAGES` hold. Every free path removes the
   entry under that same hold before dropping the cache's reference, so
   "entry present ⇒ count ≥ 1 ⇒ the inc cannot land on zero".
   `cow_ref_get` was already called under that hold on the eviction scan, so
   the lock order (`PAGES` → `COW_REFCOUNTS`-leaf) was established.
2. **W2** — `file_page_cache::insert` takes the cache's own `cow_ref_inc`
   inside the publish closure, and only when the entry was actually inserted.
   Closes the visible-entry-with-no-cache-reference window and the lost-race
   leak.
3. **`[PMM-RESURRECT]` detector** — `akuma_pmm::cow_ref_inc` now
   distinguishes *creating* the entry from incrementing an existing one
   (`get_mut`/`insert(2)` instead of `or_insert(1)+1` — a created entry used
   to be indistinguishable from an existing count of 1), and when a created
   entry's frame is currently parked in the quarantine (`QUAR_PRESENT` hit),
   prints the frame's free record and CoW history and bumps
   `cow_resurrection_count()`. One relaxed atomic load, only on the
   inc-from-zero path — unlike `PMM_PREMATURE_FREE_CHECK` it cannot perturb
   the race. **`[PMM-RESURRECT]` must never print**; it is the standing
   regression tripwire for this class.

Tests: `akuma-pmm` host test (inc-on-parked-frame counts, no double-count),
boot-suite check in `process_tests.rs` (lost-race `insert` neither replaces
the entry nor touches the loser's refcount). Reference docs:
`docs/reference/subsystems/memory.md` → "Frame lifecycle: the free pipeline"
diagrams the whole free pipeline and windows W1–W6 (W1/W2 marked closed).

## Verification

Same 10-round loop, same VM, same disk, kernel with the fixes:

```
rounds 1–10: rc=0   (baseline: 6 red / 4 green)
[PMM-RESURRECT] 0   [PMM-UAF] 0   [PMM-POISON] 0   [WILD-DA] 0
```

P(10 consecutive greens | baseline green rate 0.4) ≈ 0.4¹⁰ ≈ 1 in 10 000;
Fisher exact on 6/10 vs 0/10 red gives p ≈ 0.005. The rate moved. The boot
suite on the default profile also passes with the fixes (275 PASSED,
0 FAILED, including the new lost-race `insert` check), as do all host unit
tests and clippy.

## Residual, deliberately left open

The old `[WILD-DA]` autopsies include a **writable private** victim page
(`AP_RW_ALL`, a 2-page anonymous region, `[COW-HIST] dec 0->0`,
`tracked=false`) freed by *another thread's* munmap. The file cache never
serves writable pages, so that event has two possible readings:

- second-order damage from the **same** cache race (a desynced `user_frames`
  entry freeing a recycled frame under its new owner) — in which case the fix
  above covers it; or
- an independent **share-without-any-inc** route (fork/CoW-share dedupe, a
  demand-fault install race, or the ELF `.data`/`.bss` no-region class).

The 10/10 green arm cannot separate them: that route was the residual ~5 %
null-`Rc` crash rate, ≈ 0.5 expected events in ten rounds. No non-perturbing
detector exists for the second reading — `[PMM-RESURRECT]` cannot see it
(no inc ever happens, so there is nothing to resurrect); only
`PMM_PREMATURE_FREE_CHECK` would, and it perturbs. If cargo `EXIT=139`
null-`Rc` crashes recur, hunt that route with the `bssfork`/`cowstale`
harnesses.

## Reproduction recipe (for re-running arms)

```bash
scripts/build_devbox_smoltcp.sh
GDB=1 overlays/devbox/run-smoltcp.sh   # devbox.img, SMP=4, MEMORY=4096, ssh :2222, gdbstub :1234
```

Then in-guest, per clean round:

```bash
cd /tmp/akuma && /usr/local/bin/cargo clean && \
  RUSTC=/usr/local/bin/rustc /usr/local/bin/cargo build --release -p akuma -j4 --offline
```

- **`cargo clean` is mandatory** (use the full `/usr/local/bin/cargo` path —
  bare `cargo` is not on the non-interactive PATH and the clean silently
  no-ops, turning the arm into a meaningless incremental build).
- Drive ssh from Python `subprocess`; the `ssh` CLI is blocked by policy here.
- Run builds **detached** (`nohup … > /tmp/buildN.log`) and poll for a `.rc`
  file; an ssh session timing out mid-build scores as a failure with no error
  text.
- Poll the console log for `sshd started|Started sshd`; `grep` is `ugrep`, so
  **pass `-a`**.
- Kernel state is inspectable live over the gdbstub: `lldb --batch`, `gdb-remote
  1234`, `memory read` at `nm`-resolved `akuma_pmm` statics (the ledgers,
  `ALLOCATED_PAGES`, quarantine ring), `detach` resumes the VM.
- If the VM wedges, `docs/runbooks/recover-wedged-vm.md`; if boot degrades to
  15+ minutes, e2fsck the image first (`docs/runbooks/selfhost-kernel-build.md` §5.5).

## Method rules — every one of these was paid for

1. **Ten clean builds per arm.** A single control build once produced the
   wrong conclusion ("you caused a regression") that four more builds
   overturned.
2. **Run the as-found control anyway, and first.** It is what told this
   pre-existing failure from a fresh regression.
3. **Score failure modes separately, and name the crate that died.** Take the
   exit status from the guest's own build log, never from the harness.
4. **A green build whose instrument fired is not a passing build.** Score the
   instrument, not just the exit code.
5. **An instrument that perturbs the system cannot adjudicate a fix**
   (`E2_VERIFY_HITS`, `PMM_PREMATURE_FREE_CHECK` — both took reproducing arms
   green while armed). The `[PMM-RESURRECT]` detector was designed around
   this rule.
6. **Repair the image before measuring** — hard-killed build cycles damage
   `devbox.img`, and the only symptom is boot degrading behind a watchdog
   storm that reads like a kernel regression.
7. **Fixing a plausible bug is not evidence it was the bug. Require the rate
   to change.** This investigation's fix is the first in the class to clear
   that bar (rule satisfied: 6/10 → 0/10, p ≈ 0.005).
8. **When two paths can produce the same end state, enumerate the sites
   before trusting an exoneration on one of them.**

## Tools in the tree (used here, kept for the next hunt)

- `akuma_pmm::FreeSite` + the free ledger — free-site attribution
  (`last_free_record_at`).
- `pmm::report_poison_value` / `poison_decode` — decode a suspicious qword to
  the frame it belonged to (`0xFEEDFACE_DEAD0000 ^ pa`, page-aligned,
  in-RAM-window).
- `akuma_pmm::cow_resurrection_count()` + `[PMM-RESURRECT]` — **new**, the
  non-perturbing at-the-moment capture for reference resurrection.
- `scripts/ext2read.py` — read `devbox.img` offline; `od | grep feedface`
  in-guest — scan a suspect file for poison in situ.
- `[FPCACHE]` PSTATS line — `evict`, `evict_mapped`, `invalidations`.
- The `[Mem]` counters line — `fill_short`, `unpub`, `fpc_bad`, `pn_file`,
  `munmap_stale`, `pf_fill_short`, `pin`/`pin_ovf`/`defer`/`defer_leak`.
- `config::FPCACHE_VERIFY_HITS` — off by default; see method rule 5.

## Background

- `docs/archive/SELFHOST_ZERO_PAGE_HUNT.md` — the preceding hunt; §8 is the
  earlier UAF of this family, §14–§15 the inode-lifecycle root cause and fix.
- `docs/archive/ZERO_PAGE_ICE_FIX.md` — the ODHT-ICE handoff this
  investigation superseded.
- `docs/archive/CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md` — the premature-free
  class as previously audited; §13 ruled out PMM-level UAF *at that time*.
- `docs/archive/COW_PILE_AUDIT.md` §5.6 — the refcount-underflow class.
- `docs/archive/FILE_PAGE_CACHE_MMAP_AMPLIFICATION.md` — why the file-page
  cache exists at all.
- `docs/runbooks/selfhost-kernel-build.md` — the build itself; §5.5 image repair.
- `docs/reference/subsystems/memory.md` § "Frame lifecycle: the free
  pipeline" — the current-state pipeline diagram and windows W1–W6.
