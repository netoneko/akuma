# Prefault file fills ran through a stub that always failed — 856 silent zero pages per build (found + FIXED 2026-08-15)

The first real, rate-moving cause of the `[0,0,0,0]` self-host ICE
([`SELFHOST_ZERO_PAGE_HUNT.md`](SELFHOST_ZERO_PAGE_HUNT.md) §12). Not the whole
story — see §Result — but the single biggest contributor measured so far.

## The mechanism: three defects stacking

1. **The runtime hook was a stub.** `ExecRuntime::read_at_by_inode` was
   registered in `src/main.rs` as `|_inode, _off, _buf| Err(-1)` — always fails —
   since commit `94d1daf6 "extract akuma-exec"` (2026-03-05). The signature lacked
   `path`, and the real `vfs::read_at_by_inode` needs it (`with_fs` dispatches on
   the path prefix to find the right filesystem root), so wiring it up meant a
   signature change that nobody made. The stub shipped instead.
2. **The only consumer swallowed the error.**
   `akuma_exec::mmu::user_access::prefault_user_range` fills an inode-backed lazy
   file page through that hook and dropped the result on the floor (`let _ =`),
   a pattern that predates the `edd91fe7` user-copy fold (it sat in
   `src/syscall/mod.rs:528-530` before). Short fill or hard error, the freshly
   `alloc_page_zeroed` frame keeps its zeros — and is installed anyway.
3. **The page is then present, so nothing ever re-checks it.** This is why every
   earlier instrument read 0:

   - `[FILL-SHORT]` instruments the **demand-fault** fill in `src/exceptions.rs`.
     A page the prefault installs never faults, so that path never runs for it.
   - `[FPC-BAD]` verifies **cache hits**; these pages were never in the cache.
   - The 46 MB `cp` + `md5sum` exoneration exercises `read()` into **anonymous
     heap** buffers — prefault fills nothing there (`LazySource::Zero`).

   A fault-path instrument is structurally blind to a page another path made
   *present*. "The fill never comes up short" was true of the only fill site the
   instrument could see.

## Why the prefault fills file pages at all

`MMAP_FILE_BACKED_LAZY = true` unconditionally, so every read-only file mmap
(rustc's rlibs/rmeta, the linker's inputs) is a `LazySource::File` lazy region
with a **real inode** (from `resolve_file_extent`). Any syscall that validates a
user range over such a region with `Prefault::Yes` — `read`/`pread64`/`write`
destinations and sources included — runs `prefault_user_range` over it, which
takes the `inode != 0` arm: the stub. Which exact userspace pattern puts a
read/write buffer inside a file-backed lazy region was not traced; the
instrument prints `pid/inode/file_off/va` precisely so that question is
answerable if it ever matters.

## Measured

- **Control (instrument only, as-found kernel), reproducing build:** **856
  firings**, every one `got=Err(-1)`, against build artifacts (`libsyn`
  dependencies' `.rmeta`, `managed`'s `.rmeta`/`.d`) at 1–3 MB file offsets, from
  rustc-spawned children. The same build ICE'd on `zerocopy-derive` with the
  usual `decode error: Expected header tag [79, 68, 72, 84] but found
  [0, 0, 0, 0]`.
- **Fix arm (real hook wired), 10 clean builds, same protocol:** instrument
  silent (0 firings across all 10), 0 `[PMM-POISON]`, no wedges, all 10 builds
  ran to completion — **3/10 GREEN**, versus 0 green in every arm previously
  measured.

The rate moved 0 → 3/10, so by the hunt's own rule this was *a* cause, not
necessarily the only one.

## The fix

- `ExecRuntime::read_at_by_inode` is now `fn(&str, u32, usize, &mut [u8])` and
  registered with the real `crate::vfs::read_at_by_inode`. The prefault site in
  `user_access.rs` was its **only** consumer (grep-verified), so nothing else was
  quietly depending on the stub's failure.
- The fill site checks `got == Ok(read_len)` and prints
  `[FILL-SHORT/prefault] ... — page installed zero-filled` on mismatch — same
  contract as the demand-fault `[FILL-SHORT]`. The range is already clamped to
  `filesz`, so a short fill here is a defect, never EOF.
- Counter `akuma_pmm::DP_PREFAULT_FILL_SHORT` (`pf_fill_short` in the
  `dp_counters_line` dump). Note that dump only surfaces from the exceptions
  path on this profile — the console print is the operative guard.

## What remains

7/10 builds still fail. Scoring gap owned up front: the fix arm recorded only
GREEN/FAILED per build, not which crate died or how (hunt rule 3) — the residue
is unattributed. Next steps, in order: re-run scoring failure modes separately;
then the read-path copy instrument (T1) and the anonymous-heap side (T2) target
the residue; the SMP=1 control remains blocked by the wedge.

## Background

- [`SELFHOST_ZERO_PAGE_HUNT.md`](SELFHOST_ZERO_PAGE_HUNT.md) — the full hunt;
  §12 is this arm, §10 the theory table it closes a row of.
- [`HANDOFF_ZERO_PAGE_ICE.md`](HANDOFF_ZERO_PAGE_ICE.md) — the self-contained
  handoff prompt, updated for this find; method rule 8 came out of it.
- [`USER_COPY_FOLD.md`](USER_COPY_FOLD.md) — the fold that moved the prefault
  (and its swallowed fill result) into `akuma-exec` verbatim.
