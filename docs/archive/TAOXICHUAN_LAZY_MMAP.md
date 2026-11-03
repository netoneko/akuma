# Taoxichuan mapped every `mmap` page at `mmap` time

**Status: FIXED 2026-11-03** in `crates/akuma-tokio-kernel-amd64` (`src/lazy.rs`, the memory arms in
`src/lib.rs`), with the record encoding in `crates/akuma-pte-amd64/src/lazy.rs` and the leaf writer
`akuma_boot_amd64::paging::set_leaf_in`. Found during [`INFINITY_ON_TAOXICHUAN.md`](INFINITY_ON_TAOXICHUAN.md).

## Symptom

At `-j3` on a 3 GiB guest, a `rustc` failed to create a codegen thread: `failed to spawn work thread`, an ICE.
musl's `pthread_create` had got `ENOMEM` from the thread-stack `mmap` while most of the guest's memory was
held by pages nothing had touched.

## Cause

Taoxichuan's `mmap` allocated and zeroed every page of every mapping, and filled file mappings from the
inode, before returning. Nothing was demand-paged. So:

- a thread stack cost its full size (rustc's codegen threads ask for about 17 MB each), touched or not;
- each `rustc` held a private copy of every page of `librustc_driver` (311 MB) that ldso mapped, touched or
  not;
- `mremap` growth zeroed the new tail and copied the old contents through a 1 KiB buffer;
- a `PROT_NONE` reservation cost its full size in frames too (`PROT_NONE` is not enforced here, so
  reserved memory was mapped read-write).

The amd64 kernel moved to demand paging in 2026-09, with the `akuma-mmap` region table and shared file pages
(fpcache). Taoxichuan has no region table: its `akuma-user-layout` records only the slab cursor, its holes
and writable shared file mappings.

## Fix

**The record of a page that is not there yet lives in that page's own leaf entry.** x86 ignores every bit
of a leaf whose `P` bit is clear, so `mmap` now writes one *lazy* leaf per page and allocates nothing. The
encoding (`akuma_pte_amd64::lazy`, host-tested):

| bits | field |
|---|---|
| 0 | `P`, clear |
| 1 | tag: a lazy page, not an empty slot |
| 2, 3 | writable, executable once present |
| 4 | file-backed |
| 5 | `PROT_NONE`: never filled (added by [`TAOXICHUAN_PROT_NONE_AND_SHARED_FILE_PAGES.md`](TAOXICHUAN_PROT_NONE_AND_SHARED_FILE_PAGES.md)) |
| 6..=37 | inode |
| 38..=63 | file page index (offsets below 256 GiB) |

`lazy::fill` makes a lazy page present: a zeroed frame, or one read from the inode with the tail past EOF
zeroed, mapped with the recorded permissions. Three paths reach it:

- a not-present page fault (`fault_trap_locked`): file pages from ring 3 only, because a ring-0 fault may
  arrive with filesystem state held;
- the kernel's own user copies (`TaskWalk`), which walk the page table through the physmap and never fault;
- `akuma_user_access::validate_user_range`, which every glue arm calls before touching a user buffer. It
  calls the prefault hook for a page that is not present, and Taoxichuan never registered one, so the
  first lazy buffer handed to `read(2)` would have been `EFAULT`. It registers `lazy::prefault` now.

Because the record is the leaf, every per-page edit carries it:

| operation | what it does to a lazy leaf |
|---|---|
| `munmap`, `MAP_FIXED` over a range, `brk` regrow | `release_pages` clears it (`unmap_page_in` clears only present leaves) |
| `mprotect` | rewrites its permission bits (`lazy::reprotect`) |
| fork | copied verbatim into the child (`akuma-user-tree`'s clone plan skips a non-present leaf) |
| `mremap` move | moved, along with present leaves, frame and bits untouched (`lazy::move_range`): no copy |
| `mremap` grow, `brk` grow | the new pages are lazy zero-fill |
| `MADV_DONTNEED` | a present page **becomes** a lazy zero-fill leaf and its frame is freed, in batches of 64 after one shootdown; a lazy file page keeps its record, so it rereads the file, as on Linux |

**Inode lifetime.** A lazy file leaf names its inode by number, so the inode must outlive every address
space that can still fault it. `lazy::pin` holds an `akuma_primitives::InodePin` per `(root, inode)`,
which ext2's deferred free respects. A fork child inherits its parent's pins, and `free_user_root` drops
them. This is coarser than Linux: an unlinked file's blocks are freed when the last process that mapped it
is reaped, not when its last page is unmapped. If a pin cannot be recorded, the mapping is filled eagerly.

**Still eager:**

- writable `MAP_SHARED` file mappings, whose writeback walks present frames and which fork copies;
- file mappings at an unaligned offset;
- the ELF image and the initial stack.

**Found on the way.** `brk` regrow mapped fresh frames over the pages an earlier shrink had left mapped,
leaking the old frames. Regrow now releases the range first.

## Verification

- Host: `akuma-pte-amd64`, 17 passed (7 new for the encoding).
- TCG, SMP=4, the new kernel: `mmaplazy` PASS (17 checks, including fork, unlink-while-mapped, `mprotect`
  splits and two processes sharing one file), `lazybuf` 8/8, `mremapmove` PASS, `mmapsum /bin/busybox`
  (the four whole-file digests agree, and the two-thread half digests match the halves computed on the
  host), `readback` PASS, `madvshared` PASS, `cowstale` PASS, `bssfork` PASS, `decommitprobe` PASS,
  `unixrw` PASS. A dynamically linked `git init/add/commit` works.
- TCG: a 1 GiB anonymous mapping with two pages touched raised `used` by 4.4 MB. Most of that is the 2 MB
  of page tables the reservation needs.
- Ryzen guest, the failing configuration rerun: a clean `-j3` build of the Infinity closure on **3 GiB**.
  On the eager kernel this ran out of memory at about 195 crates after 732 s (`rustc` ICE: `failed to spawn
  work thread`). On this kernel all 278 crates built, `rc=0`, in 790 s. Peak `used` (sampled every 5 s) was
  2.86 GB of 3.14 GB; after the build `used` was back at the boot figure (532 MB), so no frame leaked. No
  `[Fault]` line on the console.

Unchanged by this fix (the deployed kernel gives the same results under TCG):

- `mprotectlb` `rw_to_none_read` and `eager_mprotect_probe` PHASE2 fail, because `mprotect(PROT_NONE)` is
  a no-op on Taoxichuan. Fixed next, in
  [`TAOXICHUAN_PROT_NONE_AND_SHARED_FILE_PAGES.md`](TAOXICHUAN_PROT_NONE_AND_SHARED_FILE_PAGES.md).
- `tcc -run` fails in `mprotect`: W^X refuses to make written pages executable (`wx=0` lifts it).
- gcc reports `cannot execute 'cc1': posix_spawnp: No such file or directory`. Not a memory bug: the ssh
  session had no `PATH` ([`SSHD_SESSION_WITHOUT_PATH.md`](SSHD_SESSION_WITHOUT_PATH.md)).

## Still open

- **Sharing** and **`PROT_NONE`**, left open here, are fixed in
  [`TAOXICHUAN_PROT_NONE_AND_SHARED_FILE_PAGES.md`](TAOXICHUAN_PROT_NONE_AND_SHARED_FILE_PAGES.md).
- **A page table per reservation.** A lazy reservation costs 2 KiB of tables per MiB before any page is
  touched. A 4 GiB `PROT_NONE` cage would be 8 MB of tables.

## Background

- [`INFINITY_ON_TAOXICHUAN.md`](INFINITY_ON_TAOXICHUAN.md): the run that hit it.
- [`TAOXICHUAN_BUGS_FOUND_2026_10_30.md`](TAOXICHUAN_BUGS_FOUND_2026_10_30.md): the 16 GiB slab, the
  address-space half of the same rustc failure.
- `amd64/src/mm.rs`: the region-table design this kernel does not have.
