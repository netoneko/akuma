# Taoxichuan: `MAP_SHARED` was private, and the image was eager

**Status: FIXED 2026-11-03** in `crates/akuma-tokio-kernel-amd64`:

- `src/shmpages.rs` (new);
- `src/lazy.rs`, `fill_shared`;
- the `mmap`, `mremap`, `mprotect`, `munmap` and `MAP_FIXED` paths;
- `release_pages`, `free_user_root`, `clone_user_tree`;
- `place_segment` and `load_task`.

Also in the crates beneath it:

- `akuma-pte-amd64`: the lazy record's `SHARED` bit;
- `akuma-user-tree`: `SharedSpans::ShareWritable`;
- `akuma-user-layout`: anonymous shared records, kept across fork;
- `akuma-fpcache-rw`: `Table::reap_unmapped`.

One fix is in shared glue: `akuma-syscalls-glue`'s `copy_from_user_str`. These close four items that
[`TAOXICHUAN_PROT_NONE_AND_SHARED_FILE_PAGES.md`](TAOXICHUAN_PROT_NONE_AND_SHARED_FILE_PAGES.md) and
[`TAOXICHUAN_LAZY_MMAP.md`](TAOXICHUAN_LAZY_MMAP.md) left open.

## Symptoms

- **`MAP_SHARED | MAP_ANONYMOUS` was not shared across fork.** Fork copy-on-wrote it like a private mapping, so a
  parent and child never saw each other's stores. `fpcpoison`'s round gate lives in such a page and never opened
  (under TCG it looked hung). So does every shared counter between processes.
- **A writable `MAP_SHARED` file mapping was private to each mapper.** Every page was a private frame, allocated and
  read from the file at `mmap` time, and fork copied the span (`SharedSpans::Copy`):
  - two processes mapping one file each saw only their own stores until a write-back;
  - a fork child's stores never reached the parent;
  - a `write(2)` to the file never showed in a mapping that already held the page.

  SQLite's WAL index (`-shm`) is exactly that shape.
- **`MAP_FIXED` over a shared file mapping kept its record.** The pages were released, but the record stayed. At the
  next `msync`, `munmap`, exit or exec, `shared_writeback` walked the record's range and wrote **the new mapping's**
  bytes into the old file.
- **`mremap` of a shared file mapping moved the pages and left the record behind.** The write-back then read
  whatever was at the old address, and the moved pages were never written back.
- **`mmap` accepted a file offset that is not a page multiple**, which Linux refuses with `EINVAL`. It then filled
  each page eagerly from that offset.
- **Every page of the program image and of its interpreter was allocated and copied at `execve`**, and so was the
  whole initial stack. Each exec held a private copy of all of its text and rodata, whether touched or not, and no two
  processes running one binary shared a page of it.

## Causes and fix

**One frame per shared file page** (`shmpages.rs`, amd64's `crates/akuma-kernel-amd64/src/shmpages.rs` on this kernel's frames). The
decisions are `akuma-fpcache-rw`'s, already host-tested for amd64. A writable `MAP_SHARED` file mapping is now lazy
like any other, and its record carries `shared` (bit 38; the page index moved to 39..=63, offsets below 128 GiB).
`lazy::fill_shared` looks the page up by `(root mount, inode, page)`:

- On a hit it maps the frame every other mapper maps.
- On a miss it reads a fresh frame and publishes it, under the crate's generation check: `Inserted`, or `Existing`,
  which frees its own frame and maps the peer's.

The key's mount id is glue's id for `/` (`resolve_file_id`), so glue's write observers match. `shmpages::init`
registers them, so a `write(2)` or a truncate copies into every mapped frame. ext2's inode-freed hook now drops this
table's entries too.

**Fork shares a `MAP_SHARED` span as it stands** (`SharedSpans::ShareWritable`): one frame, writable, no copy-on-write
mark, and refs at 2. `Layout::fork` keeps the shared records, so the child writes back the same pages its parent does.
`mprotect(RW)` no longer privatises a page in a shared span.

**`MAP_SHARED | MAP_ANONYMOUS` is eager and recorded** (`SharedFile::anon`, inode 0): its frames exist before the first
fork, which then shares them. Two lazy records would fill two frames. `shared_writeback` skips anonymous records.

**Entries go by sweep, not by key.** This kernel frees an address space from its page table alone, after the records
that named each page's file are gone (`free_user_root`, from a parked root). So it cannot reap page by page the way
amd64 does. `Table::reap_unmapped` drops every entry whose frame nothing else maps. `release_pages` sweeps when its
range overlaps a shared file mapping, and `free_user_root` sweeps after every teardown. Every path that drops a
mapping of such a page writes it back first:

- `munmap`;
- `MAP_FIXED`, which now writes back and forgets the records under it;
- exit and exec (`shared_flush_all`).

**`mremap` of a range overlapping a shared record is `EINVAL`.** This is a pinned divergence (Linux follows the move),
chosen over a record that names the wrong pages. **`mmap`'s offset must be a page multiple.**

**The image is lazy where a page is wholly one thing** (`place_segment`, given the image's inode on the root
filesystem and pinned like any lazy file page):

- a page entirely inside a segment's file bytes is a file record, so read-only ones fill from the file-page cache, one
  frame for every process running the binary;
- a page entirely past them is a zero-fill record;
- a page cut by the segment's start or its file end is filled at exec, as before.

The interpreter gets the same treatment. The initial stack below the argument block is zero-fill records.
`read_image` still reads the whole file, because `akuma-elf-exec` bounds-checks against it.

## Found on the way: a path in `.rodata` was `EFAULT`

With the image lazy, `git` died at startup: `fatal: could not open '/dev/null' for reading and writing: Bad address`.
`open`'s path was a string in git's `.rodata`, on a page nothing had touched yet. glue's `copy_from_user_str` reads a
string one byte at a time and lets the fault handler map each page, because it has no range to validate until the
string is read. Taoxichuan's fault handler fills only **zero** pages from a ring-0 fault, because a file fill reads the
filesystem, which a ring-0 fault may already hold. So the byte read failed.

That was already true for every lazily mapped library since [`TAOXICHUAN_LAZY_MMAP.md`](TAOXICHUAN_LAZY_MMAP.md): a
path a `.so` keeps in its `.rodata` gave `EFAULT`. It went unnoticed because libc's own strings are in the
interpreter, which was mapped eagerly. `copy_from_user_str` now prefaults each page it enters (one walk per page, not
per byte). On AArch64 that is one extra page-table check per page of path.

## Verification

TCG, SMP=4, herd as init:

- **`shmshare`** (new, `userspace/forktest/c_stress/shmshare.c`) 8/8:
  - `MAP_SHARED|MAP_ANONYMOUS` across fork, on a page the parent touched and one it never did;
  - a shared file mapping inherited by fork;
  - a child that maps the file itself sees the parent's stores, and the parent sees the child's;
  - `pwrite(2)` shows in an already-mapped page;
  - `mprotect(PROT_READ)` and back keeps the page shared;
  - after every mapping goes, `read(2)` returns all the stores;
  - a fresh mapping after that reads the file;
  - an unaligned offset is `EINVAL`.

  The console shows `[fpcache-rw] first shared writable file page published`.
- **`fpcpoison /bin/busybox 4 4`**: all pass, now **gated**. Its round counter is `MAP_SHARED|MAP_ANONYMOUS` and is
  shared at last.
- `protnone` 13/13, `fpcoherent` 6/6, `mmaplazy`, `lazybuf` 8/8, `eager_mprotect_probe`, `decommitprobe`, `cowstale`,
  `bssfork`, `mremapmove`, `readback`, `madvshared` and `unixrw` all pass.
- `mmapsum /bin/busybox`: the four whole-file digests and both two-thread halves equal the host's.
- A dynamically linked `git init/add/commit` works (after the glue fix), and so does `gcc`.

Ryzen guest (3 GiB, 4 vCPUs), herd as init, the like-for-like rerun that
[`TAOXICHUAN_PROT_NONE_AND_SHARED_FILE_PAGES.md`](TAOXICHUAN_PROT_NONE_AND_SHARED_FILE_PAGES.md) left open.
It was a clean `-j3` build of the same four `-p` packages, with `free` sampled every 5 s. It **did not finish: the disk
ran out twice**, so the comparison is incomplete.

| | lazy-mmap kernel | this kernel |
|---|---|---|
| part 1: clean build, incremental on | 278 of 278 units, `rc=0`, 790 s | 276 of 278, then `No space left on device` writing `rig-core`'s rmeta, 763 s |
| part 2: `target/debug/incremental` (315 MB) removed, `CARGO_INCREMENTAL=0`, resumed | - | 13 more units in 189 s, then `ld: final link failed: No space left on device` on the `infinity` binary |
| peak `used` | **2.86 GB** | part 1: 1.58 GB; part 2: **2.27 GB** |
| peak `used` + `buff/cache` | - | part 1: 1.85 GB; part 2: 2.62 GB (`buff/cache` peak 0.40 GB) |
| `used` after the build | 532 MB (boot) | 530 MB (boot) |

So, up to the final link, peak `used` was 0.59 GB lower than the whole lazy-mmap run, and no frame leaked. Three
things keep this from being like-for-like:

- the final link never ran;
- the last 13 units built without incremental compilation;
- the run was split in two.

The rerun needs a bigger guest disk: [`EXT2_TRIPLE_INDIRECT_AND_LARGE_FILES.md`](EXT2_TRIPLE_INDIRECT_AND_LARGE_FILES.md),
deployed to the host. After `cargo clean`, `e2fsck` on the stopped image found 65 unlinked inodes whose frees ext2 had
deferred and never completed. That was a pin-table defect, fixed separately in
[`INODE_PIN_CONGESTED_WINDOW.md`](INODE_PIN_CONGESTED_WINDOW.md).

Host tests: `akuma-fpcache-rw` 15 (a new sweep test), `akuma-pte-amd64` 19, `akuma-user-layout` 10 and
`akuma-user-tree` 10 all pass. Clippy is clean on Taoxichuan, amd64 and the AArch64 kernel.

## Divergences kept

- **`read(2)` does not see a mapped store until a write-back** (`msync`, `munmap`, exit). Linux's page cache would
  show it at once. The direction `write(2)` → mapping is coherent.
- **A `MAP_SHARED | PROT_READ` mapping uses the read-only file-page cache**, so it does not see a writable mapper's
  stores. Only writable shared mappings use the one-frame table. A read-only cached page a process already maps keeps
  the bytes from before a write, as
  [`TAOXICHUAN_PROT_NONE_AND_SHARED_FILE_PAGES.md`](TAOXICHUAN_PROT_NONE_AND_SHARED_FILE_PAGES.md) records.
- **Nothing refuses a write to a running image (`ETXTBSY`)**. A lazy image page reads the file when first touched, so
  rewriting a binary in place shows its running copies the new bytes. Linkers replace their output by unlink or
  rename, which a pinned inode survives.
- **32 shared records per task** (`MAX_SHARED`, was 8). Past that, a file mapping's writes are lost and an anonymous
  one is copied by fork. Both cases print a console line.

## Still open

- **The like-for-like build measurement**: one unbroken run of the four packages, to the final link, on 3 GiB. It
  needs the bigger disk.
- **Fault signals**: a ring-3 fault still ends the task instead of reaching a `SIGSEGV` handler.
- The ext2 ENOSPC inconsistencies the guest disk showed are recorded in
  [`EXT2_TRIPLE_INDIRECT_AND_LARGE_FILES.md`](EXT2_TRIPLE_INDIRECT_AND_LARGE_FILES.md).

## Background

- [`TAOXICHUAN_PROT_NONE_AND_SHARED_FILE_PAGES.md`](TAOXICHUAN_PROT_NONE_AND_SHARED_FILE_PAGES.md) and
  [`TAOXICHUAN_LAZY_MMAP.md`](TAOXICHUAN_LAZY_MMAP.md): the record and the read-only sharing these build on.
- `docs/reference/subsystems/amd64-shared-write-mmap.md`: amd64's design for the same table.
- `docs/reference/subsystems/syscalls/mem.md`, "Taoxichuan: the lazy record is the leaf": the rules.
