# Deleting files frees no space: ext2 leaks inodes and blocks on unlink

**Status: FIXED 2026-09-03** — `DEFERRED_FREE_SLOTS` 256 -> 4096, `#[must_use]`
on `DeferredFrees::push`, and a forced drain-and-retry before `release_last_link`
will give up. Verified on a live guest by `ext2probe`'s new pinned phase:
`defer_leak` **944 -> 0**, reclamation of unlinked-while-mapped blocks
**21 % -> 85 %** (the remaining 15 % is the lazy drain, not loss — see §2.3), and
a host regression test that fails at the old bound. The §3.1 amplifier is
**deliberately still there**; §6 says why.

**Date:** 2026-09-03
**Status:** **OPEN** — root-caused and measured, not fixed.
**Reproduce:** on any devbox with a build tree, `cargo clean` (or `rm -rf` a large
directory) and watch `df`. The space does not come back.

---

## 1. Symptom

```
/src/github.com/mpiorowski/late-sh # cargo clean
     Removed 1897 files, 276.8MiB total
/src/github.com/mpiorowski/late-sh # df -h
/dev/vda                  6.0G      6.0G     10.5M 100% /
/src/github.com # rm -rf mpiorowski/          # a further ~100 MB
/src/github.com # df -h
/dev/vda                  6.0G      6.0G     11.3M 100% /
```

376 MB of files removed, **zero blocks returned**. The filesystem fills
monotonically until every write fails with `ENOSPC`, and the failure surfaces
wherever the disk happens to be touched next — which is almost never the delete
that caused it.

## 2. Measured

`e2fsck -fy devbox.img` offline, after ~14 in-guest clean builds:

| | before fsck | after fsck |
|---|---|---|
| free blocks | **2 883** / 1 572 864 | **592 391** / 1 572 864 |
| used | 6.0 GB (100 %) | 3.9 GB |
| free inodes (superblock) | 284 431 | — |
| free inodes (counted) | — | **314 526** |

**2.4 GB of blocks and 30 095 inodes recovered by `e2fsck` alone.** The report is
pages of `Inode bitmap differences: -(6513--6525) -(6527--6528) …` — inodes
marked in-use in the bitmap that no directory entry references, each still
pinning its data blocks.

### 2.0 Second offline fsck, 2026-09-03 — and what the differences look like

Same image, after ~15 more in-guest builds across three campaigns
(`/opt/homebrew/opt/e2fsprogs/sbin/e2fsck -fy devbox.img`, exit 1 = errors fixed):

| | before | after |
|---|---|---|
| free blocks | 267 247 / 1 572 864 | **603 547** / 1 572 864 |
| free inodes | 296 199 / 393 216 | **315 123** / 393 216 |
| free space | 1.02 GiB | **2.30 GiB** |

**+1 314 MiB and +18 924 inodes recovered.** For scale, deleting the two largest
disposable files on the image (a 508 MB model, a 114 MB container image) would
have returned 672 MB — **offline fsck is worth roughly twice every deletion you
could make**, and it needs no boot.

**Every bitmap difference is a `-`.** 3 491 block ranges, zero `+`:

```
Inode bitmap differences:  -1576 -(6510--6581) -6583 -(6585--6587) -(16830--16862) …
Block bitmap differences:  -8113 -(14772--14781) -(15143--15151) -(82278--82307) …
```

A `-` means the on-disk bitmap marks the block/inode **allocated** while e2fsck's
traversal finds nothing referencing it. That is the §3 mechanism visible directly
on disk: `unlink` removed the directory entry and zeroed `links_count`, but the
deferred inode-free and block-free were abandoned, so the bits were never cleared.

Two details worth correcting against the phrasing elsewhere in this doc:

- **Nothing goes to `lost+found`.** `Unattached inode` = 0 and `lost+found` = 0 in
  this report. An "unattached inode" is one with `links_count > 0` and no dirent;
  these have `links_count == 0`, so e2fsck just clears the bit. The recovery is
  clean and lossless — no fragments to reattach, no data to sift.
- **The 27 `Free blocks count wrong for group #N (0, counted=X)` lines are a
  consequence, not a second bug.** The group descriptors are corrected *because*
  the orphaned bits were cleared. Do not go looking for separate
  group-descriptor accounting drift.

### 2.1 In-guest, controlled: 49.4 MB deleted, **0 blocks returned**

Measured 2026-09-03 on a saturated guest (`df` already at 0 MB free), deleting a
directory whose contents were pure scratch:

```
df free BEFORE:            0 MB
du -sx /tmp:           50 233 KB
rm -rf /tmp/*
df free AFTER:             0 MB   (delta +0 MB)
du -sx /tmp:              800 KB   <- the files really are gone
```

**49.4 MB of files removed, exactly zero blocks reclaimed.** Not "less than
expected" — nothing. This is the §3.1 amplifier at full saturation: once the pin
table overflows, `is_pinned` answers `true` for everything, every unlink defers,
and the 256-slot `DeferredFrees` array is permanently full, so each subsequent
unlink drops its inode *and its blocks* on the floor.

The operational consequence is stronger than "deleting files frees no space":
**once saturated, nothing in userspace can reclaim disk at all.** Not `rm`, not
`cargo clean`, not truncation. Both tables are in *memory*, so only a reboot
clears them — and only until they saturate again. For a long-lived box (the
cluster plan: an LLM box plus an agent box up for days) that is a hard
operational ceiling, unrelated to any memory-pressure work.

### 2.2 The per-build rate, and it is independent of RAM

The `cargo clean` + rebuild loop is **idempotent**: `clean` removes `target/`, the
build recreates it, so at the end of every *successful* build the tree is the same
size. Therefore the growth in `df used` **between two consecutive successful
builds** is leaked blocks, and no `du` is needed to see it. (A *failed* build
leaves a partial tree, so its delta is not comparable — only clean→clean counts.)

| arm | build 1 → 2, `df used` | delta |
|---|---|---|
| `MEMORY=2048` | 5201 → 5342 MB | **+141 MB** |
| `MEMORY=4096` | 5208 → 5353 MB | **+145 MB** |
| `MEMORY=4096` (2 → 3) | 5353 → 5501 MB | **+148 MB** |

Two independent arms agree within 3 %, so the leak is **~145 MB per build and a
pure function of the filesystem, not of memory pressure.** That is what caps a
campaign at ~7 builds per 6 GB image regardless of RAM: at `MEMORY=4096` the run
produced 7/7 clean builds and then died on `ld: final link failed: No space left
on device`, with **zero** kernel faults of any kind.

It also explains the rate's size. It is not that unlink partially fails; after the
first saturation *nothing* is ever freed, so every build's `target/` accumulates
in full.

Snapshot of the same image from inside the guest, for the §7 recipe:

| | |
|---|---|
| `df used` | 4.98 GiB |
| `du -sx /` (files that exist) | 3.46 GiB |
| **leaked** | **1.52 GiB — 31 % of used space** |

**Measurement gotcha:** use `df /dev/vda`, **not** `df /`. On this guest `df /`
resolves to the *proc* mount and reports `Available=0`, which reads as a full disk
on an image with a gigabyte free — it aborted a measurement run once before being
noticed.

### 2.3 `ext2probe` pinned phase — the defect isolated, before and after

`userspace/ext2probe` gained a two-phase space-reclamation section that models
this defect directly (`reclaim_unpinned` / `reclaim_pinned`). The pinned phase is
the one that matters: create 1200 files, `mmap` each **file-backed** and close the
fd so the *mapping* holds the `InodePin`, `unlink` all 1200 while mapped so every
free is deferred, `munmap`, then ask `statfs` how much came back.

| | pre-fix | post-fix |
|---|---|---|
| `reclaim[unpinned]` (control) | 100 % | 100 % |
| `reclaim[pinned]` | **21 %** (17 424 / 81 616 KB) | **85 %** (69 580 / 81 616 KB) |
| `[INODE] defer_leak=` | **944** | **0** |
| verdict | `SPACE LEAK` | `SPACE OK` |

Two numbers make the mechanism unambiguous:

- **Pre-fix `17 424 KB` returned = 272 files' worth of 64 KB, against a 256-slot
  list.** 256 entries fitted and were freed correctly; `defer_leak` came out at
  exactly **944 = 1200 - 256**. The bound *was* the leak.
- **Post-fix the missing 15 % is accounted for, not lost.** `defer=177` at the
  sample, and `81 616 - 69 580 = 12 036 KB` ~ `177 x 64 KB`. Those blocks are
  queued and will be freed by the next drain. This is why `RECLAIM_OK_PCT` in the
  probe is 80 and not 100: a correct lazy drain legitimately leaves a tail.

The control phase reading 100 % in both columns is what localises the defect —
the ordinary immediate-free path was never broken.

**Note on `pin_ovf`.** It reads 0 in both runs, but it is a *live gauge*, not a
cumulative counter: `release` decrements it when it finds no entry, so it
self-clears once pins drop and cannot tell you whether it overflowed mid-run.
Do not read `pin_ovf=0` after the fact as "the table never overflowed".

## 3. Root cause

`crates/akuma-ext2/src/ext2.rs`. Unlinking an inode that a live mapping still
holds cannot free it immediately — the mapping must keep reading real data — so
the free is deferred until the last [`InodePin`] drops. The deferral list is a
**fixed 256-slot array**:

```rust
const DEFERRED_FREE_SLOTS: usize = 256;

fn push(&self, inode: u32) -> bool {
    for slot in &self.slots { … }          // find an empty slot
    DEFERRED_FREE_LEAKED.fetch_add(1, Ordering::Relaxed);
    false                                   // caller must leak it
}
```

When the list is full `push` returns `false` and **the caller leaks the inode
deliberately** — the inode number and every block it owns stay marked allocated
forever. This is a designed trade (`DEFERRED_FREE_LEAKED`'s own doc: *"Leaked
blocks are recoverable (`e2fsck` reconnects them to `lost+found`); bytes handed
to the wrong reader are not, which is why this is the direction the overflow
falls. Non-zero here means the bound needs raising."*). The bound was never
raised because nobody could see the counter — see §4.

### 3.1 The amplifier: pin-table overflow makes *everything* deferrable

`akuma_primitives::inode_pin` holds pins in a **1024-slot** table, and its module
header states the failure mode plainly:

> *"If the table has no room, the pin is not recorded and `OVERFLOW` counts it.
> While that count is non-zero `is_pinned` answers `true` for **everything**."*

So once more than 1024 inodes are pinned at one moment, every unlink takes the
deferred path, the 256-slot list saturates within a few hundred deletes, and
every delete after that leaks outright. A `-j4` self-host build has four rustc
processes plus cargo mapping rlibs and rmeta files; exceeding 1024 concurrent
pins is ordinary, not exceptional.

**This is the leading hypothesis for why the leak is so large, and it is not yet
confirmed** — `pin_ovf` has never been observed non-zero because it is not
printed (§4). Confirming it is one line of instrumentation, see §6.

## 4. Why it went unnoticed for so long

`DEFERRED_FREE_LEAKED` is surfaced in exactly one place — `dp_counters_line`
(`crates/akuma-kernel-core/src/pmm.rs`), which renders
`… pin= pin_ovf= defer= defer_leak=` with the comment that `defer_leak=`
**"must stay 0"**.

That function is reached only through `ExceptionHooks::dp_counters_line`, i.e.
**from the sync-EL1 crash handler**. The tripwire that says "must stay 0" is
printed only once the kernel is already crashing. In normal operation — including
every self-host campaign ever run — nobody has ever seen it.

## 5. What this has been mistaken for

`ENOSPC` does not announce itself as a disk problem. In the run that exposed
this, a self-host campaign trial failed as:

```
error: failed to write to `…/stub.rmeta`: No space left on device (os error 28)
error: could not compile `akuma-fpcache` (lib) due to 1 previous error
```

while the investigation in flight was a **kernel-heap** leak, and the natural
reading of "build died, memory pressure" was memory. Earlier sessions recorded
the symptom without the cause (`devbox.img` "fills across sessions"; ENOSPC
"masquerades as net bugs"). Treat an unexplained build failure on a long-lived
guest as a `df` question first.

A related trap: `du` and `df` disagree, because the leaked blocks belong to no
file. `du -sx /` will report far less than `df` says is used, and that gap **is**
the leak — it is a cheap in-guest test that needs no reboot.

## 6. Fix options — and what was actually done (2026-09-03)

**Applied: 1 (bounded form), 2 (as a retry), and 4. Option 3 deliberately not.**

1. **Bound raised, not made growable.** `DEFERRED_FREE_SLOTS` 256 -> 4096
   (16 KB of `.bss`). Sized against *peak concurrent* deferrals rather than the
   cumulative total, because the list drains as soon as pins drop — `defer`
   returns to 0 after every probe run. A growable `Vec` was rejected: the
   deferral list is touched from `&mut Ext2State` paths, and an unbounded list
   would trade a recoverable **disk** leak for an unbounded **kernel-heap** one
   the moment anything did latch `is_pinned` on (§3.1).
2. **Applied as a drain-and-retry rather than an immediate free.** On a full list
   `release_last_link` now forces `drain_deferred_frees` and retries the push.
   The doc's original phrasing — "fall back to an immediate free when the inode
   is not actually pinned" — is **not** safe as written: under §3.1 you cannot
   distinguish "no pin recorded" from "pin lost to overflow", so freeing on that
   basis risks handing a live mapping another file's blocks. The retry gets the
   same benefit (entries become drainable the instant their last mapping goes)
   with none of that risk. Plus `#[must_use]` on `push`, because the whole defect
   was its `bool` being discarded at the one call site.
3. **The amplifier is still there, on purpose.** `is_pinned` still answers `true`
   for every inode while `OVERFLOW > 0`, and that can still latch permanently:
   `release` only decrements when it finds no entry, and a *leaked pin holder*
   never calls `release` at all. It was left alone because the measurement said
   it was not load-bearing for this leak — `pin_ovf=0` and `defer=0`, i.e. the
   drain worked — and rewriting overflow semantics in a lock-free table on a
   hunch is how a recoverable disk leak becomes a corrupted mapping. It is now
   *visible* on `[INODE]`, which is the precondition for fixing it if it fires.
   If you do fix it, confine the overflow to the hash region that overflowed
   (`slot_of(inode)`'s neighbourhood) rather than poisoning every inode; that
   keeps the conservative direction and cuts the blast radius from 1/1 to ~1/64.
4. **Done: the tripwire is visible.** `[INODE] pin= pin_ovf= defer= defer_leak=`
   now prints on the same 30 s cadence as `[FSCACHE]`
   (`akuma-kernel-glue`), with a `*** LEAKING INODES+BLOCKS ON UNLINK ***`
   suffix when `defer_leak > 0`. Previously these four were reachable only from
   `dp_counters_line`, which only the sync-EL1 **crash handler** calls — which is
   §4's entire answer to "why did nobody see this".

Regression coverage: `pinned_unlinks_beyond_the_old_bound_leak_nothing`
(`crates/akuma-ext2/src/tests.rs`) holds 300 pins live — above the old bound,
below the new — and asserts all 300 are queued, none counted leaked, and the
queue empties. It **fails at the old bound** (`left: 256, right: 300`), verified.
It needs the new `manyinodes.ext2` fixture because `test.ext2` has only 256
inodes — exactly the bound under test, so it runs out of inodes before it can
overflow the list.

### 6.1 The original option list, for reference

1. **Raise the bound / make the list growable.** The cheapest change, and the one
   the counter's doc already asks for. A `Vec` on the kernel heap removes the
   cliff entirely; the objection is that the deferral list is touched from the
   filesystem's `&mut Ext2State` paths, so an allocation there needs the usual
   audit. Note this alone does **not** fix §3.1 — with the pin table overflowed,
   the list grows without bound because nothing is ever unpinned.
2. **Never leak: fall back to an immediate free when the inode is not actually
   pinned.** The overflow path currently leaks *without re-checking* whether the
   inode needs deferring at all. Under §3.1 that check is exactly the one that
   would have said "no pin recorded, free it now".
3. **Fix the amplifier.** `is_pinned` answering `true` for everything is a safe
   default for *reads* but a catastrophic one for *frees*. Either size the pin
   table to the workload, make it growable, or separate "may still be read" from
   "may be freed" so an overflowed table degrades to slower rather than lossy.
4. **Make the tripwire visible.** `defer_leak=` and `pin_ovf=` belong on the
   periodic `[FSCACHE]`/PSTATS cadence, not only in the crash handler. This is
   two lines and it is what turns the next occurrence into a five-minute
   diagnosis. Do this one regardless of which of 1-3 is chosen.

## 7. What to check when this recurs

1. In the guest: compare `du -sx /` against `df /dev/vda` (**not** `df /` — that
   reports the proc mount, `Available=0`). A large gap is leaked blocks.
2. **Is it already saturated?** `rm -rf` a few MB of scratch and re-check `df`. If
   free space does not move **at all**, the pin table has overflowed and no
   userspace cleanup will reclaim anything until a reboot — do not waste time
   deleting things (§2.1). Deleting *early in a fresh boot*, before the tables
   fill, is the only in-guest reclaim that works.
3. Offline, and this recovers far more than any deletion:
   `e2fsck -fy devbox.img`
   (`/opt/homebrew/opt/e2fsprogs/sbin/e2fsck` — **it is installed**; plain
   `which e2fsck` fails because it is not on PATH, which has twice been misread
   as "no e2fsck available").
   "Free inodes count wrong (X, counted=Y)" — `Y - X` is the leaked inode count.
   "Inode bitmap differences: -(…)" lists them.
4. Once §6.4 is done, read `defer_leak=` and `pin_ovf=` directly. Both must be 0.
5. Do **not** read a full disk as memory pressure. Check `df` before `[HEAP]`.

## Background

- `docs/archive/SELFHOST_KERNEL_HEAP_LEAK.md` — the investigation this surfaced
  during; the kernel-heap leak is a *different* bug, and the two were briefly
  conflated because both end in "the build died".
- `docs/archive/SELFHOST_ZERO_PAGE_HUNT.md` §14 — where the inode-pin guards and
  the deferred free were introduced, and the counters this doc says nobody reads.
- `docs/archive/AKUMA_EXT2_CLEANUP.md` §6.1 — the `rmdir` `links_count`/`dtime`
  fix; the same teardown path, a different defect.
