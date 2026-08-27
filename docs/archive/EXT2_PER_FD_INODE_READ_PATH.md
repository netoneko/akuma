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

`sys_openat` resolves through `crate::vfs::open_file_inode`. Two families then
use it:

| syscall | helper | filesystem method |
|---|---|---|
| `read`, `pread64` (so `readv`/`preadv`/`preadv2`, which route through them) | `fs::read_at_open_file` | `Filesystem::read_at_by_inode` |
| `fstat`, `statx(AT_EMPTY_PATH)`, `lseek(SEEK_END)` | `vfs::metadata_open_file` | `Filesystem::metadata_by_inode` |

Both fall back to the path form when the fd carries no inode.

`newfstatat` with a dirfd deliberately stays path-based: there the fd is a
*base* to join a relative path onto, which an inode number cannot stand in for.

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

Wall-clock on this host has to be earned: `EXT2_WRITEBACK_FOLLOWUP_FIXES.md` §8
records the same probe measuring the same commit ~2x apart between sessions, and
this session measured the same arms **20x apart** depending on whether the host
was otherwise busy (see Background). So the primary evidence is a work count,
which does not move between runs at all — the timings below are real, but they
are the corroboration, not the claim.

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
host's drift cancels out of it. Varying the block size varies the **number of
`read(2)` calls over the same bytes**, which is the other half of the design: a
per-syscall cost must track the call count, not the byte count.

#### Headline, 6 runs per arm, `bs=1024` (8192 reads over 8 MB)

| | baseline (by path) | with per-fd inode | delta | ranges |
|---|---:|---:|---:|---|
| deep (5 components) | **490 ms** (444-526) | **196 ms** (142-198) | **-60%** | disjoint |
| shallow (1 component) | **294 ms** (236-312) | **196 ms** (184-207) | **-33%** | disjoint |
| deep - shallow = walk | 196 ms = **24 us/read** | 0.5 ms = **0 us/read** | | disjoint -> overlap |

Per `read(2)` of 1 KB: **60 us -> 24 us on the deep path (2.5x), 36 us -> 24 us
on the shallow one (1.5x)**. The shallow row is the one worth noticing — a file
sitting directly in `/` was still paying a real per-read walk, so this is not
only a deep-path win.

#### The same thing across block sizes (3 runs per point, medians)

The bytes and the blocks are constant down each column; only the syscall count
changes.

| block size | `read(2)` calls | baseline gap | baseline per read | with-inode gap | with-inode per read |
|---|---:|---:|---:|---:|---:|
| 1 KB | 8192 | **+158.8 ms** | 19.4 us | +1.5 ms | 0.2 us |
| 4 KB | 2048 | **+80.7 ms** | 39.4 us | -3.4 ms | -1.7 us |
| 16 KB | 512 | **+23.0 ms** | 44.9 us | +2.2 ms | 4.2 us |
| 64 KB | 128 | **+4.8 ms** | 37.2 us | -4.5 ms | -35.0 us |

Baseline: the gap **tracks the call count** — 64x fewer syscalls over the same
8 MB shrinks it 33x — which is the signature of a per-syscall cost and not a
per-byte one. With per-fd inode caching it is gone at every block size, twice
going negative, which is what "no signal" looks like.

(The baseline's *per-read* cost is not flat: ~19 us at 1 KB, ~40 us at 4 KB and
above. The same walk cannot get more expensive on its own, so something about
the surrounding read makes it so — plausibly the larger `copy_to_user` evicting
the walk's working set from CPU cache. **Unverified**, recorded because the
number is in the table and would otherwise look like an error. Nothing here
rests on it: the gap disappears either way.)

## The `stat` half, added the same day

Reads went by inode first; `fstat`, `statx(AT_EMPTY_PATH)` and `lseek(SEEK_END)`
were left resolving `f.path`. That is not merely slower, it is **incoherent**:
the same fd would read a file happily and then be told the file does not exist.

Measured directly by reverting `metadata_open_file` to the path form and booting
(the in-kernel test catches it, so the oracle arm panics at boot):

| on an unlinked-but-open fd | by path | by inode |
|---|---|---|
| `read` | the file's bytes | the file's bytes |
| `fstat` | **`ENOENT` (-2)** | `0`, correct size |
| `lseek(fd, 0, SEEK_END)` | **`0`** on a 23-byte file | `23` |

The `lseek` row is the dangerous one. `fstat` failing is at least an error the
caller can see; `SEEK_END` returning 0 silently reports an unlinked-but-open file
as empty, and anything sizing its input that way — `tail -c`, an archive writer
seeking to append — quietly produces the wrong result instead of failing.

`Filesystem::metadata_by_inode` is the new trait method (default
`NotSupported`, so filesystems without inode addressing are untouched), and
ext2's `metadata` and `metadata_by_inode` now share one `metadata_of` body so a
path `stat` and an fd `fstat` cannot disagree about the same file.

Verified in the guest with the tools that actually use these calls:
`tail -c 4 <&3` on an unlinked fd returns the right last four bytes, `wc -c <&3`
returns 16 for a 16-byte unlinked file, and after an atomic replace the fd
reports the **old** file's 5 bytes while the name reports the new one's 23.

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
- ~~`fstat` and `lseek(SEEK_END)` still go by path~~ — **closed the same day**,
  see "The `stat` half" above. What is still path-based: `newfstatat`'s dirfd
  (correctly — it is a base for a relative path), and every other fd operation
  that names a path, notably `ftruncate`, `fchmod`, `fallocate` and `flock`.
  Those mutate through the *name*, so on an unlinked-but-open fd they still fail
  where `read`/`fstat` now succeed. Same fix shape (`*_by_inode` on the trait),
  more surface; nothing in tree needs it yet.
- **The pin table is 1024 slots and shared with mmap.** Every open file fd now
  takes one. On overflow `is_pinned` answers `true` for everything and nothing
  can be freed, so the failure mode is deferred frees piling up rather than
  corruption — watch `inode_pin::OVERFLOW` and `DEFERRED_FREE_LEAKED`, both
  already in the `[Mem]`/PSTATS dump.

## Verification performed

- `cargo test -p akuma-ext2`: 83/83, and 83/83 again with `--features fs-cache`.
- Each of the four new ext2 correctness tests confirmed to **fail** against the
  pre-fix code (reverted in place, re-run, restored) — the fifth is a
  deliberate unchanged-path control and correctly passes on both arms. The
  `stat`-half regression was confirmed the same way, at the kernel: an oracle
  arm with `metadata_open_file` forced back to the path form panics at boot on
  `fstat=-2 seek_end=0`.
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

**Method note, and the reason there are two sets of numbers in this session's
history: check the host's load before believing a guest measurement.** The first
A/B here ran while the host was busy, and every number came out roughly **20x
slower** — 8192 x 1 KB reads took 4083 ms instead of 196 ms — with run-to-run
swings large enough to bury the effect entirely. It produced a *negative* result:
the same baseline arm measured 4065 ms and 4488 ms medians twenty minutes apart,
against an expected effect the noise band swallowed whole. Re-run on a quiet
host, the identical arms and the identical script give disjoint ranges and a
-60% deep-path result.

Two things saved it. The depth-difference design, which cancels drift *within*
one boot and so still showed the right shape (177 us/read -> 16 us/read) even
under load; and the deterministic host counters, which never depended on the
clock at all. `ext2probe`'s mixed workload showed nothing usable in either
condition — see §8 of the followup doc for the same lesson learned the same way.
