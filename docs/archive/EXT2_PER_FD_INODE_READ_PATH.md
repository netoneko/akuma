# Per-fd inode caching: `read(2)` stops re-walking the path (2026-08-27)

Status: **history.** The read lever
[`EXT2_WRITEBACK_DESIGN.md`](EXT2_WRITEBACK_DESIGN.md) § D-4 named and did not
build — "the next real read lever is per-fd inode caching, not the block cache"
— implemented, plus the three ext2 defects that had to be fixed first to make it
safe. The fixes are in the tree.

Companion docs: [`EXT2_WRITEBACK_DESIGN.md`](EXT2_WRITEBACK_DESIGN.md) (D-1..D-11
and the write-back measurements), [`EXT2_WRITEBACK_FOLLOWUP_FIXES.md`](EXT2_WRITEBACK_FOLLOWUP_FIXES.md)
(§9 is where this lever was identified, and §8 is the wall-clock trap this
measurement had to design around),
[`SELFHOST_ZERO_PAGE_HUNT.md`](SELFHOST_ZERO_PAGE_HUNT.md) §14 (the inode-pin
machinery this reuses, and what happens without it).

---

## The problem

`read(2)` on a file fd called `Filesystem::read_at(&f.path, ..)`. Every call ran
a full `lookup_path_internal` directory walk — one `read_inode` plus one
directory-data read **per path component** — before touching a byte of the file.
The write-back cache had already made the data itself warm, so by mid-2026 this
walk was most of what a read cost.

The mmap/exec fault path never had this problem: it resolves the inode once and
calls `read_at_by_inode`. The fd had no such thing to resolve *to*.

## What changed

`KernelFile` (`crates/akuma-exec/src/process/types.rs`) gains two fields:

| field | why |
|---|---|
| `inode: u32` | resolved once in `sys_openat`; `0` means "read by path" |
| `pin: InodePin` | keeps that inode's data alive for the fd's lifetime |

`sys_openat` resolves through `crate::vfs::open_file_inode`; `sys_read` and
`sys_pread64` (and so `readv`/`preadv`/`preadv2`, which route through them) call
`crate::fs::read_at_open_file`, which reads by inode when there is one and by
path when there is not.

Both fields are private, and `with_inode` is the only way to set either — the pin
and the number cannot drift apart. Everything else is `Clone`/`Drop`: `dup`,
`clone_deep_for_fork`, `close`, `close_all` and `exec`'s table clear all stay
balanced without knowing the pin exists, exactly as the mmap regions do.

### Which opens do *not* get an inode

- **Filesystems with no inode addressing.** `resolve_inode` / `read_at_by_inode`
  default to `NotSupported` in the `Filesystem` trait, so procfs, `MemoryFilesystem`
  and every synthetic node keep their path-based read untouched. Only ext2,
  `SubdirFs` and `OverlayFs` implement them.
- **`/etc/mtab`**, which is a resolve-time synthetic served *ahead of* `with_fs`.
  An image carrying a real on-disk `/etc/mtab` would otherwise have its stale
  bytes served in place of the live mount list. This is the one exclusion that is
  a judgement call rather than a capability check, and it is asserted by test.

Directories need no exclusion — see defect 1 below.

## Three defects that had to be fixed first

Reading by a raw inode number is only safe if the filesystem cannot pull that
number out from under the reader. `InodePin` existed for the mmap case; the fd
case walked straight into the gaps it did not cover.

### 1. `read_at_by_inode` did not refuse directories

`read_at` returns `NotAFile` (→ `EISDIR`) for a path naming a directory.
`read_at_by_inode` had no such check — not wrong, merely unreachable, because
until now only the mmap/exec fill path called it and neither maps a directory.
With a directory fd carrying an inode, `read(2)` reaches it, and would have
returned **raw dirent bytes** instead of `EISDIR`. Fixed by adding the same check.

Knock-on: `OverlayFs::read_at_by_inode` tries each layer and returned a blanket
`FsError::NotFound` when none served the inode — turning that new `EISDIR` into
an `ENOENT` for anything inside a box. It now propagates the upper layer's error,
which (all layers being the same filesystem, by `OverlayFs::new`'s contract) is
the same refusal every layer gives.

### 2. `rename` freed its destination inode with no pin check — the big one

`remove_file` consults `inode_pin::is_pinned` and defers the free. `rename`, which
unlinks its destination's last name in exactly the same way, did not: it
truncated, wrote, and `free_inode`'d unconditionally.

That is **atomic replace** — `write foo.tmp`, `rename foo.tmp foo` — which is
what `cargo`, `apk`, and every editor do all day. So the single path most likely
to yank an inode out from under a live reader was the one path that never
checked. It was already a live hazard for mmap (`SELFHOST_ZERO_PAGE_HUNT.md` §14
is the same defect reached through unlink); per-fd inode caching would have
extended it to every `read(2)`.

Both callers now go through one `Ext2Filesystem::release_last_link`, and `rename`
drains the deferral list the way `remove_file` does.

### 3. Two more asymmetries the shared helper closed

- **Fast symlinks.** `truncate_inode` reads `direct_blocks` as block numbers, but
  a fast symlink stores its target *string* there — so truncating one frees
  whatever blocks those characters happen to spell. `remove_file` guarded;
  `rename` did not. Renaming a file over a symlink was freeing stray blocks.
- **`rename(a, a)`.** POSIX: if both names resolve to the same inode (the same
  path twice, or two hard links to one file), `rename` does nothing and succeeds.
  The old code unlinked that shared inode, dropped its last link, freed it, and
  *then* re-added a directory entry pointing at the freed number. `mv a a`
  destroyed the file and left a dangling entry.

All three were pre-existing, and none is reachable only through this change —
they were found by asking "what else can free an inode a reader still names?"

## Measurement

### The deterministic half

Wall-clock cannot carry this claim on this host: `EXT2_WRITEBACK_FOLLOWUP_FIXES.md`
§8 records the same probe measuring the same commit ~2x apart between sessions,
and this session saw ~10% swings between minutes — the same order as the effect.
So the primary evidence is a work count, which does not move between runs at all.

`reading_by_inode_does_no_path_walk` (`crates/akuma-ext2/src/tests.rs`) counts
directory-tree walks and block-cache accesses on a per-instance counter — per
instance, not the crate's global `CACHE_HITS`, so a parallel `cargo test` cannot
pollute it. Reading the same 3000-byte file 64 times, five components deep:

| | tree walks | block-cache accesses |
|---|---:|---:|
| `read_at` (by path) | 64 | 1280 (**20 per read**) |
| `read_at_by_inode` | **0** | 128 (**2 per read**) |

### The wall-clock half, measured as a difference

`scripts/benchmarks/read_path_ab.py`. Rather than compare two boots — which puts
the host's drift straight into the result — it reads *the same 8 MB* through two
paths of different depth, back-to-back and interleaved, within one boot:

- `/tmp/readab/a/b/c/d/big.bin` — 5 components
- `/shallow.bin` — 1 component

Identical bytes, identical blocks, identical inode work. The only difference is
four extra components to walk, so `deep - shallow` is the path-walk cost and the
host's drift cancels out of it. `bs=1024` (8192 read calls) is the signal;
`bs=65536` (128 calls, same bytes) is the control — a per-syscall effect must
shrink by roughly 64x there or it was never per-syscall.

Four runs per arm, `MEMORY=2048`, arms confirmed distinct (see Verification):

| arm | 8192 x 1 KB reads, deep | shallow | **gap = path-walk cost** | verdict |
|---|---:|---:|---:|---|
| baseline (read by path) | 4981 ms<br>(4304-5577) | 3532 ms<br>(2844-4205) | **1449 ms = 177 us/read** | ranges **disjoint** |
| with per-fd inode | 4083 ms<br>(3781-4243) | 3955 ms<br>(3559-4330) | **128 ms = 16 us/read** | ranges overlap |

The control behaved as a control must: over 128 x 64 KB reads (same bytes, 64x
fewer syscalls) neither arm shows a gap worth reading — baseline `-107 ms`,
with-inode `+8 ms`, both with overlapping ranges and one of them negative.

**The claim is the gap, not the totals.** Reading five components deep used to
cost 177 us per `read(2)` more than reading one component deep, every call,
reproducibly enough that the two ranges do not touch. It now costs 16 us — the
same size as the noise, and in two of the four runs the *shallow* path measured
slower, which is what "no signal" looks like. The walk moved to `open(2)`, where
it happens once.

Absolute times moved the way that implies (deep 1 KB reads: 4981 -> 4083 ms
median, -18%), but that number spans two boots and this host's drift is the same
order, so it is a consistency check and not the result.

## What this also fixed, for free

An fd is now bound to a **file**, not to a name, which is what POSIX has always
said it is:

| | before | after |
|---|---|---|
| read after the file is unlinked | `ENOENT` | the file's bytes |
| read after another file is renamed over the name | the *new* file's bytes | the file the fd opened |

Verified in the guest (`sh -c 'exec 3<a; rm a; cat <&3'` and the `mv b.tmp b`
equivalent), and pinned by `test_read_uses_the_fd_inode` in `src/process_tests.rs`,
which drives the real `openat`/`read`/`close` syscalls against the real ext2 at
boot.

## Known gaps

- **The mount is still selected by path.** `read_at_open_file` resolves the fd's
  path through `with_fs` to find the *filesystem*, then applies the inode number
  to it. If the mount under that path is replaced while the fd is open, or the fd
  is used from a process whose namespace resolves the path to a different mount,
  the number is interpreted against the wrong filesystem. This is inherited, not
  introduced — the mmap fill path has read `read_at_by_inode(path, inode, ..)`
  the same way since it was written. Closing it means an fd holding its resolved
  `Arc<dyn Filesystem>`: a real open-file object, which this kernel does not have.
- **`fstat` and `lseek(SEEK_END)` still go by path**, so on an unlinked-but-open
  fd they fail while `read` succeeds. Not a regression (both used to fail), but
  now visibly inconsistent. Fixing it needs a `Filesystem::metadata_by_inode`.
- **The pin table is 1024 slots and shared with mmap.** Every open file fd now
  takes one. On overflow `is_pinned` answers `true` for everything and nothing
  can be freed, so the failure mode is deferred frees piling up rather than
  corruption — watch `inode_pin::OVERFLOW` and `DEFERRED_FREE_LEAKED`, both
  already in the `[Mem]`/PSTATS dump.

## Verification performed

- `cargo test -p akuma-ext2`: 82/82, and 82/82 again with `--features fs-cache`.
- Each of the four new ext2 correctness tests confirmed to **fail** against the
  pre-fix code (reverted in place, re-run, restored) — the fifth is a
  deliberate unchanged-path control and correctly passes on both arms.
- `cargo test` (whole workspace, host): all suites green.
- `cargo clippy -p akuma-ext2 --all-targets`: back to HEAD's 18 pre-existing
  warnings, none added. `cargo clippy --release` on the kernel: clean.
  `akuma-exec` / `akuma-isolation`: no new warnings.
- `cargo build --release` and `scripts/build_extreme_size.sh`: both clean.
- QEMU boot: `test_read_uses_the_fd_inode` PASSED against real ext2, alongside
  the existing `unlinked_inode_survives_while_pinned`.
- Guest functional checks over SSH: normal read, read-after-unlink,
  read-after-rename-over, `cat` of a directory (`Is a directory`), `/etc/mtab`
  still serving the live mount list.
- Guest A/B, arms confirmed distinct both by `akuma.bin` size (3 318 000 vs
  3 317 992) and behaviourally (the baseline arm answers `ENOENT` to the
  read-after-unlink probe).

## Background

Method note for whoever measures the next read change: the first attempt here
compared absolute `dd` times across two boots and produced a *negative* result
that was pure noise — the same baseline arm measured 4065 ms and 4488 ms medians
twenty minutes apart, against an expected effect of ~14%. The depth-difference
design came out of that failure, and it is the reason the numbers above are worth
anything. `ext2probe`'s mixed workload is even less usable for a per-syscall
effect; see §8 of the followup doc for the same lesson learned the same way.
