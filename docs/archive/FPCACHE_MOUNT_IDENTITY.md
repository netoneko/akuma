# The file page cache learns which filesystem it is caching (2026-08-27)

Status: **history.** Fixes finding **F-1** of
[`EXT2_WRITEBACK_DESIGN.md`](EXT2_WRITEBACK_DESIGN.md) and the keying half of
**D-9**. The capacity half of D-9 is still open — see "What this does not do".

---

## The defect

`src/file_page_cache.rs` keyed on `(inode, file_offset)`. An inode number is
meaningless without the filesystem that issued it, and this kernel can have more
than one: `mount(2)` with fstype `ext2` brings up a second `Ext2Filesystem` over
another disk (`MOUNT_IN_NS`, `src/syscall/container.rs`), and its inode numbers
come from the same small range as the root's.

So a mapping of inode 12 on `/data` could be handed the cached page of inode 12
on `/`. Silent wrong bytes, in the cache whose entries are mapped as **executable
text**. Scope was RO mapped pages only, which is why nothing had tripped over it
— overlays avoid it by construction (`OverlayFs::new` requires every layer on one
filesystem), and until a second ext2 is actually mounted there is only one
issuer.

## The fix

The key is now `(inode, mount id, file offset)`, and the mount id comes from the
mount table.

**Inode-major ordering is load-bearing**, not stylistic. Lookups are exact-key so
their order is free, but `invalidate_inode` is a *range* scan over one inode
across every mount, and it runs on every `write(2)` through
`vfs::invalidate_file_pages`. Only this ordering keeps that `log n + k` instead
of a full sweep of the table per write.

### Identity is assigned, never asserted

`MountSet::mount_with` stamps each mount with an id from a module-private
counter (`akuma-vfs/src/mount.rs`); `ResolvedMount::id` carries it out of every
resolution. Nothing outside that module can set one.

This shape was arrived at by discarding two worse ones:

1. **`Filesystem::fs_id()` on the trait** — rejected. Identity that the
   identified object gets to assert is identity that can be forged: a
   filesystem returning another's id is handed that filesystem's cached pages,
   and the pages in question are executable text. A `Filesystem` implementation
   must no more declare which filesystem it is than a process declares its own
   pid.
2. **An opaque token stored inside `Ext2Filesystem`** — rejected as unnecessary
   once the invalidation side was looked at properly (below). It also meant a
   global counter inside a leaf crate whose value is being pure and
   host-testable.

### Invalidation stays identity-free, deliberately

The awkward constraint was `on_inode_freed`, which fires from inside
`Ext2Filesystem::free_inode` — deep in ext2, which has no idea which mount it was
reached through. It cannot be dropped either: the deferred-free drain frees an
inode unlinked earlier, and between the unlink and the drain a still-live mapping
can fault and **re-populate** the cache, so invalidating at unlink time is not
enough.

Resolved by making [`invalidate_inode`] id-**blind**: it drops every entry with
that inode number on every mount. Over-invalidating costs a re-read;
under-invalidating hands a mapper another file's bytes. So the hook stays
`fn(u32)`, ext2 holds no identity at all, and there is nothing to forge.

### Where the id is captured

| holder | captured at | via |
|---|---|---|
| `LazySource::File::mount_id` | `mmap`, and ELF load | `vfs::resolve_file_id` → `(mount id, inode)` |
| `FileSegmentSource::mount_id` | ELF load | runtime hook `resolve_file_id` (was `resolve_inode`) |

Both resolve the pair in **one** call, so the id and the inode can never describe
different mounts.

`replace_pristine_root` mints a **new** id rather than keeping the jail's:
everything keyed on the old one belongs to the `SubdirFs` being replaced, and the
overlay replacing it must not inherit those entries. Ids are never reused after
unmount for the same reason — a reissued number would let a stale entry match the
filesystem that replaced it, which is the aliasing this exists to prevent, merely
delayed.

## The key tracks the mount, not the box

Worth stating precisely, because "per-box" is the wrong mental model and leads to
the wrong expectations.

Two mappers share a cache entry exactly when they resolved the path to the **same
mount** — which means they are looking at the same filesystem object at the same
mount point, so sharing the page is correct. F-1 was about two *different*
filesystems colliding on one inode number, not about box boundaries.

What that means per box depends on how the box was created, because
`resolve_mount` falls through to the global mount table when a process's own
namespace does not resolve the path:

| box | namespace | resolves through | keyspace |
|---|---|---|---|
| box 0 (the host) | none | global table | the global mounts' ids |
| jailed (`root_dir != "/"`) | `SubdirFs` at `/` | its own mount — the `path == "/"` arm matches every path, so it never falls through | its own id |
| unjailed (`root_dir == "/"`) | **empty** — `create_box_namespace` only mounts the jail when `root_dir != "/"` | global table, for everything | shares box 0's |

The last row is not a leak: a box with no jail is not isolated at the mount layer
in the first place, and it resolves to literally the same mounts box 0 does. The
cache key faithfully reports that.

For a jailed box this is a **deliberate loss of sharing**: it and the host
mapping the same underlying file now cache it twice, where the inode-only key
shared one copy. That is the intended semantics — a separate `SubdirFs` instance
is a separate filesystem — but it is worth knowing, because deduplicating the
toolchain across concurrent mappers is the reason this cache exists (see the
module header's `-j4` story).

## What this does **not** do

**Capacity is still one global pool.** `CAP_PAGES`, the eviction scan and
`shrink` are shared, so a box thrashing its working set can still evict box 0's
pages — and box 0 is the busiest box there is. Keys give content isolation;
budget isolation is a different mechanism (a per-box reservation, or eviction
that prefers a box's entries over the host's) and is the remaining half of D-9.

Partial mitigation already exists and is worth knowing before sizing the problem:
eviction already prefers entries **nobody maps** (`cow_ref_get(pa) <= 1`), so box
0's pages that are currently mapped by running processes are passed over first.
What is exposed is box 0's *cached-but-unmapped* pages — precisely the ones it
would otherwise hit on its next exec.

## A defect found and left alone

`EVICT_CURSOR` is inert, and always has been. The rotation ranges from
`(0, cursor)`, but inode 0 is never cached, so that bound sorts below every real
key: the first range is the whole table and the second is empty. The scan
therefore starts at the lowest inode every time — exactly the behaviour the
comment above it says the cursor exists to avoid.

Preserved verbatim through the key change (now `(0, 0, cursor)`) so this commit
alters no eviction behaviour, and flagged in the code. Fixing it changes an
eviction policy and wants its own A/B.

## Verification

- `cargo test` (workspace, host): 824 passed. Four new `akuma-vfs` tests cover
  id uniqueness, non-reuse across unmount, `fs_by_id` forgetting an unmounted
  id, re-rooting minting a fresh id, and `resolve_arc_full` agreeing with the two
  narrower resolvers it now backs.
- `re_rooting_mints_a_new_id` confirmed to **fail** without the fix (id-keeping
  line removed, re-run, restored).
- New in-kernel test (`process_tests.rs`, `dp_merged_body`): insert under mount
  A, look up under mount B with the **same inode and offset** — must miss, while
  mount A still hits, and an id-blind `invalidate_inode` drops it. Confirmed to
  **fail** against the old key: an oracle arm keying on `(inode, 0, off)` boots
  to `F-1 ALIAS — mount … was served mount …'s page for inode …`.
- `cargo clippy --release`: clean. Per-crate `--all-targets`: no new warnings
  (`akuma-ext2` back at its pre-existing 18).
- `cargo build --release`, `scripts/build_extreme_size.sh`: clean.
- QEMU boot: all boot self-tests pass, and guest checks for exec, script exec,
  write-invalidate and `stat`.
- **The cache is provably unchanged in behaviour**, which is the result that
  matters for a change whose only intended effect is "an aliasing that could not
  happen yet, now cannot happen":

  | | entries | hits | misses | evict |
  |---|---:|---:|---:|---:|
  | before | 263/78643 | 7104 | 267 | 0 |
  | after | 263/78643 | 6842 | 268 | 0 |

  Same entry count and same miss count on the same boot workload — the ids
  resolve non-zero on the real exec path, so nothing silently stopped caching.
  (`mount_id == 0` disables sharing for a region, so a change that failed to
  thread the id through would look functionally fine and quietly halve memory
  efficiency. That is what this table rules out.)

## Background

Spawned from the per-fd inode work
([`EXT2_PER_FD_INODE_READ_PATH.md`](EXT2_PER_FD_INODE_READ_PATH.md)), which
needed the same question answered — "what names a file across time?" — and
documented the mount-aliasing exposure as a known gap. The mount id is the answer
for both; putting it on the fd as well is the next step, and removes three of the
five heap allocations a warm `read(2)` still makes.
