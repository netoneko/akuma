# amd64 memory: closing the six gaps, and the wrong-errno lesson underneath them

**Date:** 2026-09-07
**Scope:** the five-item list at the end of
`docs/archive/AKUMA_AMD64_MEMORY_GAPS.md` — `pread64`, `madvise`,
`/proc/<pid>/`, `mremap`, and file-backed `mmap` — taken in that order and
verified between each.
**Status:** all five closed. **amd64 is 8/10 on the memory probes**, on both
rigs, which is the ceiling without signal delivery (trunk A2).

---

## The result first

| | before | after |
|---|---|---|
| boot self-tests, qemu/tcg `SMP=1` | 323 / 0 | **357 / 0** |
| boot self-tests, qemu/tcg `SMP=4` | 332 / 0 | **366 / 0** |
| boot self-tests, firecracker `SMP=1` | 313 / 0 | **347 / 0** |
| boot self-tests, firecracker `SMP=4` | 322 / 0 | **356 / 0** |
| `mem_suite` probes, both rigs | 4 / 10, 0 unexpected | **8 / 10, 0 unexpected** |
| `smapsdirty` sub-probes | 2 FAIL, 2 DIVERGE | **0 FAIL, 2 DIVERGE** |

Every rig gained the same **+34** checks, which is worth noting on its own: the
two boot paths (PVH under TCG, PVH under KVM) and the two core counts all ran the
same new arms, so nothing added here is accidentally single-core or
accidentally-one-machine.

The two probes still failing are `mprotectlb` and `eager_mprotect_probe`. Both
need a `SIGSEGV` **handler** and a *signalled* wait status; both are trunk A2;
neither is a memory-mapping defect and `mprotect` itself is verified working.
They are the only two entries left in `EXPECTED_FAIL`.

---

## The theme: an errno is an instruction to userspace

Item 2 was the smallest change in the list and it is the one that explains the
rest. `MADV_FREE` answered `ENOSYS`; it should have answered `EINVAL`. No
behaviour is implemented either way. But `redis-server` reads `EINVAL` as "older
kernel, presumably unaffected" and **starts**, and reads anything else as a kernel
it cannot trust and **exits**. The same value, delivered as a different number,
is the difference between a working database and a refusal.

Once that is in view it turns up three more times in this pass, twice as a bug
already in the tree:

- **`pread` on a pipe.** `ESPIPE` is an answer about *seekability*; `EBADF` says
  the descriptor is closed. musl's `FILE` layer falls back to `read()` on the
  first and gives up on the second, so the wrong one there turns a working
  fallback into a failure.
- **`mmap` on a descriptor with no bytes.** `EACCES` — refused — rather than a
  mapping full of zeros, which is the whole reason file-backed `mmap` was
  `ENOSYS` in the first place. See item 5.
- **`mremap` with `MREMAP_FIXED`.** Refused with `EINVAL` rather than quietly
  placing the mapping somewhere else. The AArch64 kernel currently ignores this
  flag; that divergence is recorded at the site rather than fixed there, because
  changing its answer wants its own A/B and nothing in the tree passes the flag.

And once as a *self*-inflicted version of the same thing, in item 3: `access(2)`
and `open(2)` gave different answers about whether a file existed.

---

## 1. `pread64` — one arm, and a correction to the brief

x86_64 syscall 17 was not dispatched at all. `amd64/src/fd.rs` grew
`sys_pread64`, which is `sys_read` with one difference that is the entire
contract: **it does not move `file.position`.** The two cannot share a body for
exactly that reason.

The file model made it easy — `fd.rs` caches a whole file in `Entry::data` at
`open`, so a positional read is a slice.

`Syscall::Pread64` went into `akuma-syscalls-abi` rather than the raw-number
match in `usermode.rs`, with both architectures' numbers (x86_64 17, aarch64 67),
because that crate's header asks for exactly that: "Add an entry when a caller
needs it, with both numbers, and the round-trip tests will hold you to it."
`Madvise` and `Mremap` went with it for the same reason.

**The brief said this would unblock `mmapsum`. It did not, and the source says
so.** `mmapsum` hashes one file four ways and only the first arm is `pread`; the
other three are `mmap`. Adding `pread64` moved the failure from
`mmapsum: pread failed at 0` to `mmapsum: mmap failed`, and the probe stayed red
until item 5. That is not an argument against doing item 1 first — it is one arm
and it unblocks a great deal of real software — but *read a probe's source before
claiming which change turns it green*.

What did verify item 1 end to end was arithmetic rather than a probe. The staged
file is 262144 bytes of `'A'`, so its FNV-1a-64 digest is computable on the host:
`0b67390edcf62325`. The guest's `read:` line — produced by a loop of 1 MiB
`pread`s at increasing offsets — printed the same value. That covers the bytes,
the offsets, and the short-read-at-EOF that terminates the loop, in one number.

Seven boot self-tests cover what a guest cannot reach: reading from an offset
while the cursor sits at EOF (a `pread` implemented as seek-read-seek returns 0
there), the cursor being unmoved afterwards, past-the-end returning 0, a negative
offset being `EINVAL`, `ESPIPE` on the console, and `EBADF` on a closed fd.

## 2. `madvise` — the errno, and then rather more than the errno

The errno was the ask. What landed is the whole call, decided by
`akuma_syscalls_mem::madvise::action` — the same host-tested function the AArch64
kernel dispatches on, so `MADV_FREE`'s deliberate `EINVAL` and "every
unrecognised advice reports success" cannot drift between targets.

`MADV_DONTNEED` is implemented for real, and that is the part with substance:

- The walk is `paging::for_each_leaf_in_range`, not a per-page loop. Every absent
  page is `PageAction::Nothing` anyway, so skipping an absent subtree whole is not
  an optimisation here — it is what bounds the work by *what is mapped* instead
  of by a length ring 3 chose.
- The per-page rule is the crate's `dontneed_page_action`, whose input that
  matters is the **CoW share count**: a frame another address space can see must
  not be zeroed in place, or a `fork` peer's live page is wiped. That is the
  null-`Rc` corruption of `docs/archive/CARGO_HEAP_NULL_RC.md`, and this target
  reaches it trivially — `fork` is how every shell command starts.
- **Only pages inside a recorded region are touched.** A range handed to
  `madvise` can cover pages no `mmap` region ever claimed: the ELF image's text
  and data are mapped by the loader and are deliberately not regions. Zeroing one
  would destroy the running program's code *permanently*, because nothing here
  can re-read it. Linux drops the page and restores it from the file, so skipping
  it is **closer** to Linux than acting — this is a narrowing, stated at the site,
  not a shortcut.

`MADV_WILLNEED` is a documented no-op. Only anonymous mappings are lazy here, so
pre-faulting installs a zero frame that reads exactly as the demand fault would
have produced; `madvise(2)` is explicitly advisory about *when* memory is
committed. The comment states the condition under which that stops being true —
the day a file mapping becomes lazy — because pre-faulting a file-backed lazy page
with zeros is what silently zeroed every weight page of a `llama.cpp` model mmap
on AArch64 (`docs/archive/BKL_VFS_CARVE_OUT.md` §10).

### `madvshared` was passing by not running

This is the find worth keeping. `madvshared` was green before this change and
green after, and the two greens mean opposite things. Before, `madvise` was
`ENOSYS` and all three sub-tests printed `madvise unsupported, skipped`. After:

```
madvshared: child-advises/parent-intact  PASS — parent kept 4096/4096 bytes
madvshared: parent-advises/child-intact  PASS
madvshared: control/self-zeroed          PASS — 4096/4096 bytes zero after own advise
```

The suite's `verdict()` cannot catch this — the probe scored *itself*, and its
skip is a legitimate PASS. It is the silent-pass trap one level in from where the
harness can see, and the only defence is reading a probe's output rather than its
verdict when the feature under it changes.

## 3. `/proc/<pid>/` — the files were already there; `access` could not see them

The brief expected three of the five files `smapsdirty` wants to be missing.
They were not. `/proc/self/{stat,status,cmdline}` were being served, `open`
answered for them and `stat` answered for them — and `access(2)` said `ENOENT`,
because `sys_access` went straight to the disk and never consulted the `/proc`
synthesis at all.

`render_proc_file`'s own header says one function serves `open` and `stat` "so
the two can never disagree about what exists". There was a **third** caller that
had never been pointed at it. Nothing failed loudly: busybox mostly `open`s, and
the one caller that probes with `access` first is a program deciding whether the
kernel has a `/proc` at all.

The fix routes `sys_access` through the same `proc_metadata`. The self-test
`proc_consistency_check` now asserts the invariant in both directions — a path
that exists answers 0 from all three, a path that does not answers `ENOENT` from
all three — because the invariant was cheap to state and expensive to notice.

`maps` and `statm` were then added, via two new renderers in the shared
`akuma-procfs` (six host tests). Two decisions in them are load-bearing:

- **They are served for the calling process only.** `maps` and `statm` describe
  an *address space*; `PROCS` is keyed by scheduler slot, the spawn table by pid,
  and nothing joins them. Every real reader of these two reads its own. The
  directory listing narrows to match — advertising a name in `/proc/<pid>` whose
  `stat` then says `ENOENT` is how `ls` prints "No such file or directory" about
  its own listing.
- **`maps` reads two sources, and the first version was wrong for reading one.**
  Built from the region list alone it came back **empty** for an ordinary
  program, because the ELF image and the initial stack are placed by the loader
  and are deliberately not regions. An empty `maps` is worse than an absent one:
  a reader scanning for the mapping containing an address gets a confident "there
  is none". Present page-table leaves outside every region are now coalesced into
  runs and reported too:

```
00400000-00512000 r-xp 00000000 00:00 0
00711000-00714000 rw-p 00000000 00:00 0
7ffffff7f000-7ffffffff000 rw-p 00000000 00:00 0
```

`statm` reads the *same* walk, after the first version summed the region list
alone and reported `size 0` for a program whose `maps` plainly listed three
mappings. `405 406 0 274 0 131 0`: 274 text + 131 data = 405 = the extents above.
Two files disagreeing about one address space is the shape of bug this whole
`/proc` section keeps producing.

**`smaps` was deliberately not added.** It would flip `smapsdirty`'s first
sub-probe from DIVERGE to PASS and send its fourth down redis's *real* check —
which needs correct `Shared_Dirty` accounting or redis reports a CoW corruption
bug and exits. Serving it badly is worse than not serving it, which is the same
sentence as item 2's, and the probe's own header says so.

## 4. `mremap` — it moves pages; it does not copy them

The decision is shared (`akuma_syscalls_mem::mremap::plan` and `no_move_errno`,
both host-tested, including divergence 5 — a shrink returns the old address with
the tail still mapped). The *effect* is not the AArch64 one, and deliberately.

AArch64 allocates `new_pages` fresh frames and copies the old bytes through a
kernel bounce buffer. That is the only thing it can do with the structures it
has, and it has cost this tree two bugs: a copy loop that `break`ed on the first
lazy destination page and **silently truncated** the mapping
(`docs/archive/USER_COPY_FOLD.md` §5 — which is why `mremapmove` exists), and an
`unwrap_or(NONE)` that turned "the source recorded no protection" into "the
source said `PROT_NONE`" and killed `rustc` mid-build.

This target has a region table and demand paging, so it re-points each present
page at the new virtual address and leaves the frame where it is. No allocation,
no copy, no bounce buffer, and **no truncation is possible** — there is no loop
that can stop early and still look finished. Sparsity falls out rather than
needing a case: a source page never faulted in is not present, nothing is moved
for it, and the destination demand-pages a zero frame on first touch, which is
what touching the source would have done. A copy implementation has to *decide*
what to do there; this one cannot get it wrong.

The frame ledger is untouched on purpose — it counts VAs per frame within one
address space, one VA goes away and one arrives, so the count is unchanged.

```
mremapmove: PASS resident-grow (4194304 bytes)
mremapmove: PASS grown-tail-zero (4194304 bytes)
mremapmove: PASS sparse-grow (4194304 bytes, 1 page in 4 written)
```

## 5. File-backed `mmap` — the danger was real, the remedy was too broad

The refusal said file-backed `mmap` needs a page cache, because "serving a
file-backed mapping as anonymous memory would look like a working call and hand
the caller a file full of zeros". That is right about the **danger** and too broad
about the **remedy**: the zeros are what a mapping with no fill path produces, not
what a private file mapping is.

`MAP_PRIVATE` asks for a private copy of the file's bytes whose writes nobody else
can see. This target can give exactly that, because `fd.rs` already holds every
open file's contents in the kernel. `populate_file_page` allocates a frame, zeroes
it, then overwrites it from the file — and the ordering is the guarantee: **zero
is the value of a byte past EOF and of nothing else.**

What is *not* here is a page cache. Two processes mapping one file hold two sets
of frames, and a file mapping is always eager. That is a cost rather than a
semantic difference for `MAP_PRIVATE`, and it is the state the AArch64 kernel was
in until `src/file_page_cache.rs` landed in 2026-08.

**Writable `MAP_SHARED` is still `ENOSYS`**, and that is the original objection in
its true scope: those writes must reach the file and every other mapper, and a
private copy would accept the write and drop it.

Three guards keep the zero-filled-file failure unreachable, and the third is the
one that matters:

1. `sys_mmap` refuses a descriptor that is not a regular file with `EACCES`.
2. `populate_file_page` refuses it **again**, at the point where the zeros would
   otherwise be written. A guard where the harm happens outlives a guard at the
   caller.
3. The frame is zeroed *before* the fill, never instead of it, and
   `file_bytes_at` returning `None` is a failure of the whole page rather than a
   zero-byte fill.

Verified, rather than reasoned about — three descriptor shapes, on the guest:

```
PASS pipe-read-end refused errno=13
PASS stdout-console refused errno=13
PASS closed-fd-77 refused errno=13
```

And the content, from `mmapsum`, whose four digests must agree with each other
*and* with the value computed on the host:

```
read:  0b67390edcf62325     <- pread
mmap1: 0b67390edcf62325     <- demand-paged content
mmap2: 0b67390edcf62325     <- resident-page stability
madv:  0b67390edcf62325     <- after MADV_WILLNEED
```

A zero-filled mapping cannot produce that digest, which is what makes agreement
here a proof rather than a coincidence.

The offset path is exercised by no probe in the tree, so it was verified with a
throwaway: a 3-page file mapped at offset 4096 matched byte for byte; a mapping
starting at the last page had a correct real page and a past-EOF page reading
zero; an unaligned offset was `EINVAL`. A permanent probe for this was **not**
added — every source in `userspace/forktest/c_stress/` is now auto-enrolled in
`verify_trim.py`'s `EXERCISES`, and adding one wants an aarch64 arm this pass did
not run. Left as a decision for whoever wants the coverage.

---

## What is still open

- **A page cache.** Writable `MAP_SHARED` file mappings are `ENOSYS`, and
  `MAP_PRIVATE` file mappings cost one set of frames per mapper. Both want the
  same structure — `akuma-fpcache` exists and is host-tested, and is the obvious
  thing to reach for. The B trunk is *not* finished; it is unblocked.
- **A file mapping is eager**, so mapping a file larger than free memory is
  `ENOMEM` at `mmap` rather than a fault later. Pinned in the module header, and
  it is the reason `MADV_WILLNEED` can be a no-op — the two must move together.
- **`/proc/<pid>/smaps`** is absent on purpose (see item 3), and `maps`/`statm`
  are served for the calling process only.
- **`MREMAP_FIXED`** is refused here and *ignored* on AArch64. One of those is
  wrong; nothing in the tree passes the flag, so neither was chosen under
  pressure.
- **Signals (A2).** `mprotectlb` and `eager_mprotect_probe`. 8/10 is the ceiling
  until they land.
- **`akuma-procfs`'s `render_statm`/`render_maps_line` have no AArch64 caller.**
  They are shared-crate code with one consumer, which is how `akuma-dmesg` and
  `akuma-procfs` itself both started; the AArch64 `ProcFilesystem` can adopt them
  whenever someone wants `/proc/self/maps` there.

## Was aarch64 disturbed?

No, and this was checked rather than asserted. `src/` is untouched. The two
shared crates that changed took **190 insertions and 0 deletions**
(`git diff --numstat`), so no existing item was edited. `akuma-syscalls-abi` has
exactly one consumer in the tree — `amd64` — so its three new variants are
unreachable from the other kernel. The new `akuma-procfs` items have no consumer
outside their own crate. The aarch64 kernel builds, and the full host suite
(`cargo test`) is green including `akuma-syscalls-abi`'s round-trip and
table-disagreement tests, which is what holds a new variant to having both
numbers right.

## A harness note that cost a cycle

`scripts/utils/amd64_trials.py` forwards ssh on **2244** by default, and so does
the `mem_suite` recipe in every runbook. Running a probe VM by hand on 2244 and
then starting the trials harness makes QEMU fail to bind, which surfaces as
`NO TALLY — the boot produced no self-test line` in **6 seconds** and reads
exactly like a kernel that died before printing. The harness's own docstring
warns about this for port 2222 and the trap simply moved. Boot hand-run VMs on a
different port (2255 works), and note the harness's cleanup `pkill`s the
forward — so it will also kill the VM you were using.

`scripts/vm_ready.py` cannot reach the amd64 guest at all: it takes no identity
argument, and that image authorises exactly one key. The runbooks' `vm_ready.py
2244` line will never return for this target. Not fixed here; worth fixing.

## Background

- `docs/archive/AKUMA_AMD64_MEMORY_GAPS.md` — the measurement this closes, with
  the two corrections it earned.
- `docs/archive/AKUMA_AMD64_MMAP_REGIONS.md` — B1/B2, what this is built on.
- `docs/archive/AKUMA_SELF_HOSTING_AMD64.md` — the unlock tree; A2 is signals.
- `docs/archive/LONG_ROAD_TO_REDIS.md` — why the errno rather than the feature.
- `docs/archive/GRANT_RECORDS_VS_DENY_RECORDS.md` — why `mremap` carries
  "recorded nothing" across rather than turning it into `PROT_NONE`.
- `docs/archive/USER_COPY_FOLD.md` §5 — the truncation `mremapmove` was written
  for, and which this implementation cannot reproduce.
- `docs/archive/CARGO_HEAP_NULL_RC.md` — the corruption `MADV_DONTNEED`'s
  share-count rule prevents.
- `docs/runbooks/amd64-bare-metal-loop.md` — running the probes on both rigs.
