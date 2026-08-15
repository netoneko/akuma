# Handoff prompt — the `[0,0,0,0]` self-host ICE

*Paste everything below the line into a fresh session. It is written to be
self-contained; the referenced docs add depth but are not required to start.*

---

## The task

In the Akuma kernel repo (bare-metal Rust, AArch64, QEMU virt), the self-hosted build
fails on essentially every clean build. `rustc`, running **inside** the guest, panics:

```
thread 'rustc' panicked at rustc_metadata/src/rmeta/def_path_hash_map.rs:56:13:
decode error: Expected header tag [79, 68, 72, 84] but found [0, 0, 0, 0]
```

`[79,68,72,84]` is ASCII **`ODHT`**, the on-disk hash-table header in crate metadata.
This is **never a compiler bug** — it means the kernel handed `rustc` zeroed memory
where real bytes belonged. Find out which memory, and why.

It fails on `enumn` and `zerocopy-derive`, **in the same second**, every time. Those are
the only two proc-macro crates cargo builds in parallel at that stage and the only two
that depend on `syn`. Treat the pairing as a scheduling coincidence, not a fact about
those files — a previous session spent hours on file-side theories because of it.

## Reproduce

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
| Fill read short/errored | `[FILL-SHORT]` / `DP_FILE_FILL_SHORT` | 0 |
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

## Live theories, ranked

**T1 — `sys_read`/`sys_pread64`/`readv` short-copies into the user buffer.** The
strongest untested candidate. Commit `edd91fe7 "safer memory helpers"` rewrote **every**
user-memory copy on this branch, and nothing has instrumented the read path's *copy*
half. If the syscall returns `n` but copies fewer than `n` bytes out, the tail of a
freshly-allocated (zero) buffer stays zero — exactly `[0,0,0,0]` where a header belongs,
with no error anywhere. Start by counting bytes-read against bytes-copied and printing
on mismatch. `ld` issues ~13,000 `readv`s per link, so the iovec walk matters as much as
plain `read`.

**T2 — an anonymous heap page reads back as zeros.** Same end state as
`docs/archive/CARGO_HEAP_NULL_RC.md` (cargo reading a zeroed `Rc` out of its own heap),
whose *sharing* half was fixed but whose class is open. `rustc` materialises metadata
into its heap. **Every instrument so far points at the file side, which is now closed** —
so instrument the anonymous side: the anon demand-page arm, the CoW break, and
`mprotect`/`mremap` remapping live data.

**T3 — a missing barrier / memory-ordering bug under SMP.** Two rustc processes fail in
the same second. If a frame is zeroed on one core and published before the zeroing is
visible on another, a reader sees zeros. **Decisive experiment: does it reproduce at
SMP=1?** Currently blocked — see below.

**T4 — the ext2 block cache under concurrent readers.** Keyed on physical block number;
`write_block` evicts correctly, but it has not been tested under concurrency. Weakened
by `fpcpoison` passing, not eliminated.

## Blocker you will hit immediately

**`SMP=1` hard-wedges the box.** `SMP=1 overlays/devbox/run-smoltcp.sh`, same workload:
build 1 completed in 1012 s, build 2 wedged. QEMU pinned at **98% CPU**, console frozen,
last line:

```
[AS-NEW] pid=86 l0=0x80d2e000 asid=0x6e via=clone parent=81
```

Same shape as "Defect A" in `docs/runbooks/selfhost-kernel-build.md` but at SMP=1, where
it has not been recorded before. This blocks T3 **and** any single-core control for the
other theories, so it may be worth fixing first. To inspect a wedge you must boot with
`GDB=1` — the gdbstub is armed at launch, so a VM already wedged is uninspectable.

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
   nothing. Require the ICE rate to change.

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
  `munmap_stale`; `[FPCACHE]` carries `evict_mapped`.

## Full history

`docs/archive/SELFHOST_ZERO_PAGE_HUNT.md` — every arm, every measurement, the wrong fix
kept deliberately as a worked example. `docs/archive/FPCACHE_EVICTION_PREFERS_UNMAPPED.md`
covers a cache-policy bug found along the way.

**State: the ICE is unfixed.** 0 green builds in every arm measured.
