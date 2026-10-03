# amd64: "git commit" lost `.git/objects/XX`, then `.git` — fpcache-rw ruled out, two ext2 `ftruncate` bugs found

**2026-10-03.** On the trashcan, twice, and only in `akuma-cli-wgpu`:

```
$ git commit -am 'update docs'
error: unable to write file .git/objects/7d/f282…: No such file or directory
error: invalid object 100644 4b153c0e… for 'src/main.rs'
error: Error building trees
$ git status
fatal: not a git repository (or any of the parent directories): .git
```

The question was whether the same-day shared-writable page table (`akuma-fpcache-rw`,
[`AKUMA_AMD64_SHARED_WRITABLE_PAGES.md`](AKUMA_AMD64_SHARED_WRITABLE_PAGES.md)) caused it.

**Status: the symptom itself did NOT reproduce, and its cause is still unknown.** What this
session established is a negative on fpcache-rw (by code reading and by a stress that could not
make it hurt a directory) and two real, fixed ext2 bugs that the stress turned up on the way.
Do not read the fixes below as "the vanishing `.git` is fixed". Section 5 says what to collect
if it happens again.

## 1. Why fpcache-rw looked guilty, and why it probably is not

The table's hooks (`shmpages::wrote` / `zeroed`) copy into or zero **frames that user processes
have mapped**. Nothing in them touches a disk block. They can only reach a directory by

- the munmap/msync/exit **flush**, which writes a frame to a *path* — guarded by
  `write_back_target_ok` (the path must still resolve to the mapping's `(mount, inode)`; the
  check is not atomic with the later `write_at`, but ext2 would have to reuse the inode number in
  that window), or
- a **use-after-free**: a table entry outliving its frame, so a later zero/copy lands in a
  recycled frame (an ext2 directory block's page, say).

I walked every path that frees a table frame (`reap`, `publish`'s lost race, `detach_inode` from
`inode_freed`, `unmap_range`, the `MADV_DONTNEED` release, fork's `adopt_user_frame`, exit
through `release_shared_write_mappings`) and found no way to free a frame while an entry or a
mapper still holds it: all of them go through the PMM's CoW count, and `reap` is gated on
`refs <= 1` under the table lock. Two smaller faults exist — `inode_freed` calls
`detach_inode`, which ignores the mount id (another mount's entry with the same inode number
loses its table reference: coherence, not memory safety), and a flush of a CoW-broken private
copy is copied back over the shared frame by `wrote` — neither reaches a directory.

Also: **git itself never populates the table.** It maps its index and packs read-only. The
hooks are gated on `MappedFileHooks::active()`, false until something maps a file
`MAP_SHARED|PROT_WRITE`. Something else on the box must have been doing that (goose/SQLite,
rustc/cargo, parity-db), which is why the `[FPCACHE-RW]` line in `dmesg` is the first thing to
read after an incident (§ 5).

## 2. The experiment

`userspace/forktest/c_stress/fsintegrity.c`, driven by `scripts/utils/amd64_fsintegrity_check.py`
(local QEMU, `e2fsck -fn` from the host on a **fresh copy** of the root image, only lines *new
relative to the pre-boot fsck* count — `mkdisk.sh` images carry their own fsck noise: inode 309
link count, an unconnected `...` directory).

Two worker kinds on one filesystem at once:

- **gitsim** — what `git commit` does to the tree: `mkdir objects/XX`, create a tmp file in
  `objects/`, write, `rename` into place, `index.lock` + `rename` over `index`, rewrite a
  worktree file with `O_TRUNC`. Keeps a manifest and re-verifies every directory and object it
  ever made every 8 commits (and `.git` after every one). This is the **canary**.
- **mapsim** — what rustc / memmap2 / SQLite do: create, `ftruncate`-**extend**, `mmap`
  `MAP_SHARED|PROT_WRITE`, store, extend again with a one-byte `pwrite`, fork from a thread with
  the mapping live, then end it six ways (`msync`+`munmap`, `munmap`, unlink-before-unmap,
  rename-then-unlink, `ftruncate(0)` under the mapping, exit with the mapping live). Half the
  workers share one `-shm`-like file.

Arms run (all on the kernel at `410117ce`, single root ext2):

| arm | gitsim canary | e2fsck (new lines) |
|---|---|---|
| control, no mapsim, SMP 1, 30 s | clean | clean |
| SMP 1, 2+4 workers, 60 s | clean | `/fsi/shared-shm: i_size 77824, should be 98304` |
| SMP 4, 2+4, 90 s | clean | same shape (`should be 98304`) |
| SMP 4, 3+8, 120 s, twice | clean | same shape (`should be 102400`) |
| SMP 2, 1 GiB RAM, hog holding 800 MiB | 2 workers died at `pmm_free=0` (no OOM killer: user `#PF` on an anonymous page the PMM cannot supply) | same shape |

**No directory or object was ever lost or changed**, with or without fpcache-rw's table in play
(`entries`, `hits`, `misses`, `reaps` in the `[FPCACHE-RW]` line show it was heavily exercised:
1500 fills, 1470 reaps, 0 fill retries per run). The 512 MiB guest never reached ssh; not
investigated.

## 3. What the stress did find: `ftruncate` in `akuma-ext2`

Every mapsim run left `shared-shm` with `i_size` *smaller* than the blocks it holds. Cause, in
`Ext2Filesystem::truncate` (changed 2026-09-22 to fix extend):

1. **A shrink only moved `i_size`.** The comment said "shrink keeps the existing free path";
   there was no such path in the function. Blocks stayed allocated and referenced — a space
   leak, `e2fsck`'s `i_size should be`, and:
2. **Stale bytes came back.** A later extend reuses those blocks through
   `ensure_block(_, true)`, which returns an *existing* block without zeroing. Write `0xAA` × 40
   blocks, `ftruncate(100)`, `ftruncate(40 blocks)`: bytes 100.. read `0xAA`, where POSIX
   promises zeros. The 2026-09-22 test (`ftruncate_extends_with_zero_blocks`) said "frees what it
   drops" in a comment and asserted only the size, which is how it passed.
3. **An extend that failed partway (ENOSPC) returned with `?` before writing the inode.** The
   bitmap kept the blocks it had just handed out; no inode referenced them. A leak. The mirror
   image for a shrink is worse — blocks freed in the bitmap while the inode still points at them,
   i.e. two files sharing a block once the allocator reuses it — and was latent behind the
   no-free bug in (1); the fix has to keep it from becoming live.
4. **An extend over a hole in the file's *last partial block*** allocated that block with
   `zero_leaf = false`, so the bytes before the old EOF read back as whatever the block held
   before. Now `true`.
5. A shrink also leaves staged bitmap / group-descriptor / superblock edits; the function never
   called `flush_meta`.

None of these loses a directory. (3) is the only one that is *adjacent* to directory damage —
a block both a file and something else believe they own — and it needs a failing extend/shrink,
which a 16 GiB box with plenty of free space makes unlikely. It is the reason the error path now
writes the inode back.

### The fix

`crates/akuma-ext2/src/ext2.rs`:

- `free_blocks_from(state, inode, first)` — frees every data block at logical index `>= first`
  through direct, singly- and doubly-indirect levels, and every pointer block it leaves empty,
  zeroing the pointers that named them and decrementing `sectors_used` per free.
  `truncate_inode` is left as it was (it also serves unlink and `O_TRUNC`).
- `truncate` shrink: free from `ceil(length / bs)`, then zero the surviving tail of the new last
  block. Extend: unchanged except `zero_leaf = true` for the partial block.
- Any failure rolls an extend's allocations back (`free_blocks_from(old block count)`), then
  writes the inode and flushes metadata before returning the error.
- Success path calls `flush_meta`.

Tests (`crates/akuma-ext2/src/tests.rs`): `ftruncate_shrink_frees_blocks_and_reextend_reads_zeros`
(**fails on the old code**: free count moved by 0, not 40) and
`ftruncate_shrink_walks_every_indirection_level` (shrink to a point inside direct, singly and
doubly indirect, to a pointer-block boundary and to zero; prefix intact each time; the free
count returns to its starting value exactly). 110 `akuma-ext2` tests pass, stable across
repeated runs. A pin-table global makes an unlink-then-count assertion order-dependent under
`cargo test`'s threads, so the second test does not do that.

Verified end to end: after the fix, `fsintegrity` at SMP 4 (3 gitsim + 8 mapsim, 60 s ×2 and
90 s) leaves `e2fsck` with **zero** new lines, and `shmwrite` (13 rungs) and `shmcoh` still pass.
One run, before the guest was nudged to allocate, showed `Deleted inode … has zero dtime` and
two bitmap `-` lines: an unlinked-while-mapped inode whose reclaim is *lazy* (it happens at the
next `allocate_inode`, per the comment there). The driver now creates and removes a file before
`sync` so a clean image is a fair target; it is an orphan-until-next-allocation window, not a
corruption, and a real crash in that window would leave it for fsck.

## 4. What this does not explain

- **Why the repo, why twice.** `akuma-cli-wgpu` is the only repo whose program maps `/dev/fb0`
  (`mmap_framebuffer`: eager, write-combining, frames not in the ledger, so no teardown path
  frees them — I read `unmap_range`, `UserAddressSpace::drop` and fork's non-write-back arm and
  did not find a way for those to reach the PMM), and it is developed on the box by agents running
  cargo inside it. Neither was exercised here (no framebuffer on the local QEMU `microvm`; no
  agent). Both are candidates for "what is different about this directory", and neither is
  evidence.
- **Why `rename` ENOENT, then no `.git`.** git creates the tmp file in `objects/7d/` (so that
  directory existed), then `rename`s it; ENOENT there, followed by `.git` not resolving from the
  repo, reads like the *lookup* failing or the directory chain being removed, not a data block
  being overwritten. That is a different class from everything the stress could provoke.
- The trashcan kernel's feature set (`no-tests`) and 16 GiB RAM differ from the local runs;
  pressure past 800 MiB of 1 GiB behaves as documented (no OOM killer), not as a path to
  directory loss.

## 5. If it happens again — before touching anything

1. Do **not** `git init`/re-clone over it. Note whether `ls /src/github.com/netoneko/` still
   lists the repo and `ls -la` of it.
2. `dmesg | grep -a 'FPCACHE-RW\|MM-WB\|PMM-\|PANIC'`. `misses=0` means the table never held a
   page this boot (exonerates fpcache-rw outright); `[MM-WB] … FAILED` or any `[PMM-…]` line
   points back at it.
3. Reboot to Ubuntu (`hpbox.py`) and `e2fsck -fn` the Akuma partition **read-only**
   (`/dev/sda1` per the box notes) before the next Akuma boot mounts and writes it. `-fn` output
   naming a directory inode, a block claimed by two inodes, or `.git`'s inode as unreferenced
   decides between "ext2 metadata" and "lookup/mount path"; a clean fsck with the repo present
   on Ubuntu decides the other way (an in-memory lookup failure, which the block cache, the
   mount table's `resolve_mount` clone-then-drop, and fd-by-inode caches are the places to read).
4. Was `akuma-wgpu` running / holding `/dev/fb0`, and was a cargo build inside the repo at the
   time? `ps` before the reboot.

Rerun the local stress with `scripts/utils/amd64_fsintegrity_check.py [--smp 4] [--git 3 --map 8]
[-t 120] [--control] [--hog MiB --mem MiB]`. Needs `e2fsprogs` on the host
(`/opt/homebrew/opt/e2fsprogs/sbin`).

## Files

- **Changed:** `crates/akuma-ext2/src/ext2.rs` (`free_blocks_from`, `truncate`),
  `crates/akuma-ext2/src/tests.rs`.
- **New:** `userspace/forktest/c_stress/fsintegrity.c`,
  `scripts/utils/amd64_fsintegrity_check.py`.
- **Not changed, and why:** `amd64/src/shmpages.rs`, `akuma-fpcache-rw` — no defect found that
  reaches a directory. The `detach_inode`-ignores-mount note is a coherence nit; fixing it
  needs the inode-freed hook to carry the mount id.

## Background

- [`AKUMA_AMD64_SHARED_WRITABLE_PAGES.md`](AKUMA_AMD64_SHARED_WRITABLE_PAGES.md) — the table under suspicion
- [`../reference/subsystems/amd64-shared-write-mmap.md`](../reference/subsystems/amd64-shared-write-mmap.md) — "The three bugs this uncovered" § 1 is the `ftruncate`-extend change that left shrink unfinished
- [`AKUMA_AMD64_SHARED_WRITE_MMAP.md`](AKUMA_AMD64_SHARED_WRITE_MMAP.md)
