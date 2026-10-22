# `.clone()` Audit — crates/ production code (2026-10-19)

## Background

Prompted by a nightly-clippy sweep for `clippy::redundant_clone` (nightly-only,
inert under the workspace's `nursery = "warn"` on stable). The lint found only
**4 hits, all in test code** (`akuma-exec/src/process/children.rs:1799`,
`akuma-procfs/src/tree_tests.rs:478` and `:558`,
`akuma-mmap/src/region.rs:1136`) — production paths are clean where borrowck
can prove the original is dead. So the interesting question became the 145
production `.clone()` sites the lint *cannot* judge: is each an `Arc` refcount
bump (cheap), a semantically-required allocation (fork/exec ownership), or an
avoidable copy on a hot path?

Census at audit time: **145 production sites** across ~40 files
(plus ~110 in `*_tests.rs` / `*_model.rs`, out of scope).

## Classification

### 1. Arc / Waker refcount bumps — cheap, correct (~60% of sites)

The dominant pattern. Every `Arc<dyn Filesystem>`, channel, namespace,
`TerminalState`, fd-table (CLONE_FILES), signal-action table, `SharedCreds`,
waker and `BoxProxy` clone is a refcount bump. Representative sites:

- `akuma-vfs/src/mount.rs:294,316,403,424` — `mount.fs: Arc<dyn Filesystem>`
- `akuma-exec/src/process/mod.rs:1004-1017` — fork's Arc-share block
  (`memory`, `stdin`, `stdout`, `terminal_state`, `namespace`, `channel`)
- `akuma-exec/src/process/mod.rs:3590-3592` — CLONE_FILES/CLONE_SIGHAND Arcs
- `akuma-exec/src/process/channel.rs`, `children.rs` — channel handles
- `akuma-syscalls-glue/src/poll.rs:559`, `akuma-exec/src/alarms.rs:137` — wakers
- `akuma-isolation/src/lib.rs:46-49` — global namespace singleton

No action. Replacing any of these with borrows would fight the ownership
model (handles outlive the lock they're taken under).

### 2. Semantically required allocations — ownership transfer or lock-scope snapshots (~35%)

Fork/exec/spawn must copy what the child *owns* rather than shares, and
several paths deliberately snapshot under one lock to do I/O outside it:

- `cwd`/`root: String` (`mod.rs:1005-1006`), process image
  `name`/`exe`/`args` (`mod.rs:1031-1033`) — per-fork copy, bounded by
  `PATH_MAX`-ish sizes; the child mutates them later (`chdir`, `execve`)
- `clone_deep_for_fork` (`fd.rs:121-128`) — the fd table is the one
  deliberately-deep fork structure; per-entry ref bumps are shared with `dup`
- `mmap_snapshot: Vec<MmapRegion>` (`mod.rs:3044`) — CoW fork works off a
  snapshot so the parent can run during the copy; documented, bounded by
  `MAX_FORK_MMAP_PAGES`
- `LazySource::file(path.clone(), …)` (`mem.rs:444,591,600`,
  `image.rs:121`, `children.rs:1127,1186`) — each lazy/clip-split region owns
  its source record; the path *is* the flush key
- `SharedFileMapping` snapshots (`mem.rs:307,335,1532`) — mapped under
  `SHARED_FILE_MAPPINGS` and flushed outside it (writeback takes fs locks);
  the comment at `mem.rs:301` states the contract
- `SharedWriteBack` split clones (`mmap/region.rs:182,599,720`) — a
  region split gives each piece its own write-back record
- unixsock `bind`/`getsockname`/`getpeername` name clones
  (`unixsock.rs:533,727,771,787`) — one is stored, the others are marshalled
  into the caller's sockaddr buffer
- `flock.rs:104` — inserting the key into the table
- `fs.rs:210,3690`, `vfs-glue/lib.rs:394` — `cwd` returned by value / moved
  into the process under an IRQ-masked closure that must not allocate
- `proc.rs:933` (`fd_description`), `mod.rs:2168`, `stats.rs:152`,
  `mount.rs:363` — procfs render paths; one bounded String per record read

No action recommended. Some are micro-optimizable (the procfs renders could
stream instead of materializing a String) but each is once-per-syscall and
size-bounded — exactly the tradeoff `ALLOC_PRINT_AUDIT` draws for the console
extended to procfs.

### 3. Findings — avoidable or questionable (5 sites)

#### 3a. `fs.rs:3592` — whole dir cache re-cloned on every `getdents64` call
`crates/akuma-syscalls-glue/src/fs.rs:3592`:

```rust
let entries = if let Some(ref cached) = f.dir_cache {
    cached.clone()      // Vec<DirCacheEntry>, one String per entry, per call
```

`ls` on a large directory loops `getdents64` until it returns 0; every call
after the first re-clones the entire `Vec<DirCacheEntry>` (name `String` per
entry, up to the 64 KiB record bound) only to marshal it into the user buffer
and throw the clone away. Marshalling can read `&cached` directly — the clone
buys nothing, the `fd` already owns the cache. **Real finding, hottest of the
set** (per-syscall, O(entries) allocations per call).

#### 3b. `fs.rs:3636` — miss path builds cache then clones it
Same file, once per directory-open (cache miss): `cache` is built locally,
`cache.clone()` is stored into the fd, and the local is used for marshalling.
Storing first and marshalling from the stored copy removes one full clone per
directory open. Minor next to 3a but the same fix.

#### 3c. `overlay_fs.rs:208` — `prefix.clone()` per path component
`akuma-isolation/src/overlay_fs.rs:208`: every component of every path
resolved through an overlay clones the running prefix `String` (used twice:
as the parent for the component join and for the whiteout probe). A saved
byte-offset into `prefix` gives both borrows without a per-component
allocation. Every file operation on a container rootfs pays this. Real, if
small per call.

#### 3d. `box_registry/mod.rs:133` — whole registry cloned per hierarchy query
`registry_snapshot()` clones `BTreeMap<u64, BoxInfo>` (name + root_dir
strings per box) to answer queries without holding the lock. Frequency is
low (container inspection), so this is a note, not a priority — but a
`Vec<BoxInfo>` snapshot or borrowing under the short lock would bound it to
what the query needs.

#### 3e. `memfs.rs:227` — `read_all` clones the whole file
`akuma-vfs/src/memfs.rs:227`: `FsNode::File { data } => Ok(data.clone())` —
the API returns an owned `Vec<u8>`, so the copy is contractual today. Flagged
because any memfs-backed file read is O(file size) allocation; a caller-passed
buffer (`read`-style) entry point would remove the class. No current hot
caller identified.

## Method (reproduce)

```bash
# The lint sweep (nightly only; nursery group carries the lint but stable skips it):
cargo +nightly clippy --target <host> --workspace \
  --exclude akuma --exclude akuma-amd64 --exclude akuma-scheduler \
  --all-targets -- -W clippy::redundant_clone

# The census:
grep -rn '\.clone()' crates/*/src --include='*.rs'
```

The two bare-metal binaries (`akuma`, `akuma-amd64`) cannot build for the
host target; `src/` (40 sites) was triaged by the same reading, all of them
fork/procfs/console-shaped and none hot.

## Verify

After fixing 3a/3b, `scripts/mem_suite.py` and an `ls -R` on a large
directory inside the devbox should be visibly cheaper in the
`scripts/benchmarks/mem_ab_run.sh` A/B; 3c is covered by the rump
acceptance playbook's container path. Nothing else in this report should
change behavior — that is the point of the classification.
