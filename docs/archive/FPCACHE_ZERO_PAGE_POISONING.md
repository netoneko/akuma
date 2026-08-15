# `rustc` ICE "found [0, 0, 0, 0]" — the file-page cache published pages past EOF (2026-08-15)

> **Status: ROOT-CAUSED AND FIXED.** Three defects, one symptom. The fix is a
> two-line clamp in `sys_mmap` plus a publish gate in the fault path; the
> instrumentation that would have caught it in one boot is now permanent.
> **Not yet verified in the guest** — see §7 for the one command that does it.

## 1. The symptom

Self-host builds (`cargo build --release -p akuma` inside the devbox) died with
`rustc` panicking, **every time after `cargo clean`**:

```
thread 'rustc' panicked at rustc_metadata/src/rmeta/def_path_hash_map.rs:56:13:
decode error: Expected header tag [79, 68, 72, 84] but found [0, 0, 0, 0]
...
error: the compiler unexpectedly panicked. This is a bug
note: rustc 1.99.0-nightly (12c36e253 2026-08-10) running on aarch64-unknown-linux-musl
```

`[79, 68, 72, 84]` is ASCII **`ODHT`** — the on-disk hash table header the `odht`
crate writes into `.rlib`/`.rmeta` metadata. rustc `mmap`s that metadata and got
**zeros**. This is never a compiler bug; it means the kernel served zeroed pages
for a file-backed mapping.

**The shape of the failures is the whole diagnosis.** Eleven ICE dumps recovered
from `devbox.img`, in five clusters, and every cluster is a **pair at the same
second**:

```
22:09:17  zerocopy-derive     22:09:18  enumn
22:19:02  zerocopy-derive     22:19:03  enumn
22:20:44  zerocopy-derive     22:20:44  enumn
22:27:04  zerocopy-derive     22:27:04  enumn
22:28:35  zerocopy-derive     22:28:36  enumn
```

`enumn` and `zerocopy-derive` are the only two proc-macro crates cargo compiles at
that stage, it compiles them **in parallel**, and they are the two that depend on
**`syn`**. `quote` and `proc-macro2` are built alongside and never ICE. So the
corruption is not random: it is one file, read by two processes at once.

## 2. Reading the evidence without booting anything

The VM was already shut down when this started, and the ICE only reproduces on a
clean build. Everything in §1 was recovered **offline from `devbox.img`** with
`scripts/ext2read.py` — no QEMU, no second VM, no risk of disturbing a live one:

- the ICE dumps and their timestamps (the guest clock is UTC and correct: the
  last artifact is stamped 06:06:36 UTC = 09:06 local, minutes before shutdown);
- `/tmp/akuma/.git/HEAD` → which branch the guest tree was on;
- `/tmp/build{1,2,3}.rc` (`0`) and `/root/build.rc` (`101`);
- and the decisive one — **the build's output artifact still existed**:
  `/tmp/akuma/target/aarch64-unknown-none/release/akuma`, 3,811,568 bytes,
  `entry=0x40100000`, `.text` 2,520,268, all sections present. A correct
  self-hosted kernel.

That last point killed the reported complaint ("the build never produces a
binary") before any debugging: with `CARGO_BUILD_TARGET` set the kernel lands
under `target/aarch64-unknown-none/release/`, and `target/release/` holds only
build-script output. Worth doing this recovery **first**; it is minutes, and it
reorders the whole investigation.

Two things it also ruled out, both of which had looked guilty:

- **`lto = "thin"`** (landed the day before, `a7827b91`). Measured on the same
  tree: peak linker RSS 793,952,256 → 805,044,224 (+1.4%), relink 8.4 s → 10.2 s.
  Not the 2.0× / OOM cliff `runbooks/selfhost-kernel-build.md` advertises, and the
  *successful* 06:06 build ran with it on. **That runbook paragraph overstates the
  cost and should be corrected.**
- **The `ARCH64_UNKNOWN_NONE_RUSTFLAGS` typo** in the build recipe. Real, but
  inert here: the build runs with cwd = `/tmp/akuma`, so cargo finds the tree's
  own `.cargo/config.toml` and applies `-Clink-arg=-Tlinker.ld` anyway. It only
  bites from a foreign cwd — and then it is silent and total (16,320-byte ELF,
  `entry=0x0`, no `.text`, `Finished`, exit 0). See
  `runbooks/selfhost-kernel-build.md`.

## 3. What was actually wrong

Three separate defects, each of which alone is survivable and which together
produce a permanent, cross-process wrong page.

### 3a. `filesz` was the *mapping* length, not the *file* length — the root cause

`src/syscall/mem.rs`, both `LazySource::File` construction sites:

```rust
let source = akuma_exec::process::LazySource::File {
    path, inode, file_offset: offset, filesz: len, segment_va: mmap_addr,
};
```

`len` is the `mmap` length. **`mmap` may legally map more than the file holds** —
the tail past EOF reads as zeros for that mapping and SIGBUSes on write. So
`filesz` was describing the mapping, not the file.

The fault path decides shareability by testing against `filesz`
(`src/exceptions.rs`, Pass A and the publish gate):

```rust
let full = va >= segment_va && va + 0x1000 <= segment_va + filesz;
```

which is exactly the rule `src/file_page_cache.rs` documents for itself, and
documents the *reason* for:

> **Fully covered by file data.** A page straddling `filesz` has a zero-fill tail
> whose length depends on the *mapping*, not the file, so two mappers can
> legitimately disagree about its contents.

Passing the mmap length defeats the rule it is stated in terms of. Every page
between EOF and the end of the mapping was classed **fully covered**, so:

1. `read_at_by_inode` clamps to the real size (`end = min(offset + buf.len(),
   file_size)`, and `Ok(0)` once `offset >= file_size`) → a **short read**;
2. the frame came from `alloc_pages_zeroed`, so what was not read stays **zero**;
3. the page passed the publish gate and was `insert`ed into `file_page_cache`
   under `(inode, file_off)`.

Step 3 is what makes it lethal. The cache is **global and cross-process**: the
next mapper of that `(inode, file_off)` — any process — gets a *hit*, takes the
zeros, and never touches the disk. One mapping's past-EOF tail becomes every
process's file content.

That is the pairing in §1. The first faulter poisons the entry; the second
consumes it in the same second.

### 3b. The fill's `Result` was discarded

Both fill sites in `demand_page_lazy_region` read like this:

```rust
let _ = crate::vfs::read_at_by_inode(path, inode, file_off, page_buf);
```

`read_at_by_inode` returns `Result<usize, FsError>` — **it carries the byte
count**. Discarding it means a short read, an `IoError`, and a file that genuinely
contains zeros are all indistinguishable, and the page is published either way.
This is why the bug had no diagnostic of any kind: there was nothing to print.

### 3c. Publishing was not conditional on the fill succeeding

Even with 3a fixed, any future short read (a real I/O error, a truncated file
racing the fault) would still be published to the shared cache. The publish had no
notion of whether the frame was actually filled.

## 4. The fix

**`src/syscall/mem.rs`** — one helper, used by both sites:

```rust
fn resolve_file_extent(path: &str, offset: usize, len: usize) -> (u32, usize) {
    match crate::vfs::file_size(path) {
        Ok(file_len) => (
            crate::vfs::resolve_inode(path).unwrap_or(0),
            core::cmp::min(len, (file_len as usize).saturating_sub(offset)),
        ),
        Err(_) => (0, len),
    }
}
```

`filesz` now describes file data. Pages past EOF stop being "fully covered", so
they are neither read nor published — they are just this mapping's zero tail,
which is what they always were. The size lookup shares the `VfsBklGuard` window
the `resolve_inode` call already opened, so it costs no extra BKL hold.

The `Err` arm is the conservative direction: the mapping still works (read extent
falls back to `len`), but `inode = 0` disables **both** `lookup_and_ref` and
`insert`, because without a size there is no way to tell a real page from a
past-EOF one.

**`src/exceptions.rs`** — capture the fill result and gate the publish:

```rust
fill_complete = got == Ok(len);
...
if fill_complete {
    crate::file_page_cache::insert(inode, file_off, pf, is_exec);
} else {
    crate::pmm::dp_count(&crate::pmm::DP_FILE_FILL_UNPUBLISHED, 1);
}
```

A short-filled frame stays private to the faulting process. It does not get to
speak for every other one.

**`src/pmm.rs`** — `DP_FILE_FILL_SHORT` and `DP_FILE_FILL_UNPUBLISHED`, both in
`dp_counters_line` (so they appear in the periodic `[Mem]` line *and* the crash
dump), plus a `[FILL-SHORT]` / `[FILL-SHORT/single]` print naming
`pid`/`inode`/`file_off`/`want`/`got`/`va`.

**Non-zero `fill_short` is always a defect.** The range is already clamped to
`filesz` by the caller, so a short read at that layer is not EOF. The two tags are
kept distinct because the single-page fallback arm never publishes — it poisons
only the faulting process — so the tag alone says which class you are looking at.

## 5. The probe

`userspace/forktest/c_stress/fpcpoison.c` (built by `userspace/build.sh`, copied to
`bootstrap/bin/`):

```
fpcpoison <path> [rounds] [nprocs]        # defaults: 20 rounds, 4 procs
```

Parent digests the file per-page via `read()` (the known-good VFS path) into a
`MAP_SHARED` array; each round forks N **processes** that map the file fresh and
verify every page behind a spin gate, so they fault the same pages at the same
instant. A mismatching page is reported with its offset and **whether it is
entirely zeros** — that is what separates this bug from generic corruption, and
the offset correlates directly with `[FILL-SHORT]`'s `file_off`.

**`mmapsum.c` cannot cover this class and is not a substitute.** Its `mt` arm uses
threads, which share one address space and one mapping; the poisoning only travels
through the cache's cross-process `(inode, file_off)` key. This needed real
processes.

Calibrated ALL PASS on the host (6 rounds × 4 procs over a 3.7 MB file, 908
pages), so it has no false positives of its own. A FAIL in the guest is the kernel.

## 6. Why it was deterministic after `cargo clean`, and why that looked like flakiness

The on-disk ICE dumps cluster in one 19-minute window and then stop, with a clean
build afterwards — which reads as intermittency and is **wrong**. `cargo clean`
forces the whole dependency graph to rebuild, which is the only thing that makes
cargo compile `enumn`/`zerocopy-derive` and therefore the only thing that reads the
offending pages. An incremental rebuild never touches them. The window is not the
bug coming and going; it is when clean builds were being run.

Corollary for anyone bisecting this class: **a green incremental build proves
nothing.** Reproduce with `cargo clean` or not at all.

Verified against the disk, so it is not the explanation you might reach for first:
`libsyn-*.rlib` (6,305,048 bytes, 1540 blocks) has **zero holes** — every block
pointer inside the file is allocated, across direct, single- and double-indirect.
The zeros were never on disk. `devbox.img` uses 4096-byte blocks, so
double-indirect addresses 4.3 GB and ext2's missing triple-indirect support
(`get_block_num` → `Err(NotSupported)`) is unreachable here.

## 7. What is NOT done

**Guest verification.** Everything above is code inspection plus offline forensics
on `devbox.img`, and the host gate (`cargo build --release` clean, clippy clean,
533 host tests, probe ALL PASS). The kernel has **not** been booted with the fix.
One clean build settles it:

```bash
scripts/build_devbox_smoltcp.sh && overlays/devbox/run-smoltcp.sh
# in-VM:
cd /tmp/akuma && cargo clean && cargo build --release -p akuma -j4 --offline
/bin/fpcpoison /tmp/akuma/target/release/build/syn/*/out/libsyn-*.rlib 20 4
```

Expect: the build completes, `fpcpoison` prints ALL PASS, and `fill_short=0` in the
`[Mem]` line. **A `[FILL-SHORT]` line after this fix is a different defect** — 3a is
closed, so it would mean a genuine I/O failure, and its `file_off`/`want`/`got`
names it.

**A regression test.** There is no boot test that maps a file with `len > size` and
asserts the past-EOF pages are not published. That is the test this whole document
is an argument for, and it did not get written.

**The cache key has no filesystem identity.** `(inode, file_off)` is a `(u32,
usize)` with no device/mount component, and inode numbers are only unique *within*
a filesystem. Two mounts with overlapping inode numbers alias in this cache. Not
reachable in the single-ext2 devbox, not touched here, and a separate change.

## Background

- [`FPCACHE_UNDERSIZED_AT_LOW_RAM.md`](FPCACHE_UNDERSIZED_AT_LOW_RAM.md) — the other
  live defect in this cache (sizing, not correctness)
- [`../reference/subsystems/memory.md`](../reference/subsystems/memory.md),
  `src/file_page_cache.rs` module docs — the eligibility rules 3a violated
- [`COW_PILE_AUDIT.md`](COW_PILE_AUDIT.md) §12 — the three-pass demand-paging body
  the fill and publish sites live in
- [`LTO_RELEASE_PROFILE.md`](LTO_RELEASE_PROFILE.md) — the `lto = "thin"` arm ruled
  out in §2, and the self-host measurement it lists as still missing
- [`../runbooks/selfhost-kernel-build.md`](../runbooks/selfhost-kernel-build.md) —
  the build this broke; §2 corrects its LTO guidance
