# Deleting files frees no space: ext2 leaks inodes and blocks on unlink

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

## 6. Fix options

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

1. In the guest: compare `du -sx /` against `df`. A large gap is leaked blocks.
2. Offline: `e2fsck -fy devbox.img`
   (`/opt/homebrew/opt/e2fsprogs/sbin/e2fsck`; there is no `e2fsck` on PATH).
   "Free inodes count wrong (X, counted=Y)" — `Y - X` is the leaked inode count.
   "Inode bitmap differences: -(…)" lists them.
3. Once §6.4 is done, read `defer_leak=` and `pin_ovf=` directly. Both must be 0.
4. Do **not** read a full disk as memory pressure. Check `df` before `[HEAP]`.

## Background

- `docs/archive/SELFHOST_KERNEL_HEAP_LEAK.md` — the investigation this surfaced
  during; the kernel-heap leak is a *different* bug, and the two were briefly
  conflated because both end in "the build died".
- `docs/archive/SELFHOST_ZERO_PAGE_HUNT.md` §14 — where the inode-pin guards and
  the deferred free were introduced, and the counters this doc says nobody reads.
- `docs/archive/AKUMA_EXT2_CLEANUP.md` §6.1 — the `rmdir` `links_count`/`dtime`
  fix; the same teardown path, a different defect.
