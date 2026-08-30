# `akuma-ext2`: `unsafe` audit and a cleanup plan

**Date:** 2026-08-30. **Status:** audit verified and §4 revised the same day —
every claim in §1–§4.2 was checked against the source; §4.2a's finding stands
but its fix, and all of §4.3–§4.7, are superseded by the reap-based design in
§4.5 below. The lock crate lands **without** wiring; adoption is a separate step.

**Landed 2026-08-30 (steps 1–3 of §5).** The layout assertions (step 1) and the
explicit parse/serialize codec (step 2 — all 14 blit sites plus the §3 symlink
pair, with the §2.3 mount validation) are in `akuma-ext2`, which fell from 18
production `unsafe` sites to 2 (+1 in `cfg(test)`); the residual three are the
lock sites, untouched until step 4. Round-trip and corrupt-image rejection
tests live in `crates/akuma-ext2/src/tests.rs` (`cargo test -p akuma-ext2`, 99
green). The lock crate (step 3) landed as **`akuma-locks-rw`** with
`#![forbid(unsafe_code)]` and the `bkl_model.rs`-pattern model checker — with
two deliberate deviations from §4.5, both resolving in the direction of the
§8 law:

- **The lock carries no `T`.** §4.5 sketches `RecoverableRwLock<T>` behind
  `forbid`, which stable Rust cannot express (minting `&mut T` from `&self`
  needs an `UnsafeCell` deref). The protocol is value-free and pure atomics;
  a consumer composes its own `UnsafeCell<T>` against the tickets, where the
  exclusivity proof lives beside the value's owner.
- **No global registry, no free `reap_tid(i)` sweep.** Enumerating locks is
  the business of whoever owns them (the VFS mount table); wiring (step 4)
  registers one reaper hook — the `init_inode_freed_hook` shape — whose body
  calls each mount's `lock.abandon_tid(i)`. The crate allocates nothing and
  depends on no allocator; every host test is a fresh instance.

Two additions the sketch implied but did not name: a writer-priority bit
(`WWAIT`, re-asserted per failed attempt, with a ghost-heal in the read loop)
and a floor-guarded two-step reader-hold publication so a sweep racing an
acquisition cannot underflow. Boot-suite (`verify_trim.py`) and `e2fsck`
verification ran separately; §6 tracks them.

Follow-on from `docs/archive/AKUMA_EXEC_SPLIT_AGAIN.md`, which took `akuma-bkl`
and `akuma-elf` from "irreducible" to `#![forbid(unsafe_code)]` and `akuma-pmm`
from 5 sites to 3. `akuma-ext2` is the next-largest non-enforced crate that is
not a device driver.

## 1. The numbers

```
crates/akuma-ext2   2,875 production code   18 unsafe sites (+1 test)   98.1% safe lines
```

**Verified 2026-08-30.** All 19 are in `src/ext2.rs`. The family split reconciles
exactly: `read_unaligned` ×7 (1110, 1512, 1687, 2326, 2401, 2502, 2526),
`from_raw_parts` ×7 (1443, 1517, 1704, 2436, 2470, 3168, 3184 — the last two are
production `mkdir` code, not test), `copy_nonoverlapping` ×2 (2586, 2648 — the
symlink pair), `force_unlock_write` ×3 (1193 in `cfg(test)` `try_read_state`,
1238 `read_state`, 1272 `write_state`). 18 production + 1 test. `lib.rs`,
`tests.rs` (zero `unsafe`) and `build.rs` are clean.

## 2. The struct-blit family (14 sites)

Four structures — `Superblock`, `BlockGroupDescriptor`, `Inode`, `DirEntryRaw` —
all `#[repr(C, packed)]`, read with `core::ptr::read_unaligned` off a `&[u8]` and
written with `core::slice::from_raw_parts` over `&self`:

```rust
// read, e.g. ext2.rs:1512
Ok(unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const _) })

// write, e.g. ext2.rs:1443
let buf = unsafe {
    core::slice::from_raw_parts(
        &state.superblock as *const Superblock as *const u8,
        size_of::<Superblock>(),
    )
};
```

### 2.1 The finding: nothing checks the layout

There are **no layout assertions** anywhere in the crate — no
`const _: () = assert!(size_of::<Superblock>() == 1024)`, no per-field offset
checks. Compare `akuma-exec`, which does exactly this for `ProcessInfo`.

The field lists are **correct today** — `Superblock` sums to 1024, `Inode` to
128, `BlockGroupDescriptor` to 32, `DirEntryRaw` to 8 (= `DIR_ENTRY_HEADER_SIZE`,
so the `offset + HEADER <= len` loop guards fully cover each 8-byte read) — but
nothing verifies any of it. Correctness rests on two coincidences holding
simultaneously:

1. **Field order and types match the ext2 spec.** A reordered field or a `u16`
   where the spec says `u32` does not fail to compile and does not panic — it
   silently misparses a real filesystem. On the write path it silently *corrupts*
   one.
2. **Host endianness matches the format.** ext2 is little-endian on disk;
   `read_unaligned` of a `repr(C)` struct is host-endian. There are **2**
   endian conversions in the file (`ext2.rs:2095` `from_le_bytes`, `:2105`
   `to_le_bytes`) — verified. Correct on aarch64 LE; latent everywhere else.

### 2.2 The fix

Replace the blits with explicit offset-based parse/serialize:

```rust
fn parse(buf: &[u8]) -> Option<Superblock> {
    Some(Superblock {
        total_inodes:  u32::from_le_bytes(buf.get(0..4)?.try_into().ok()?),
        total_blocks:  u32::from_le_bytes(buf.get(4..8)?.try_into().ok()?),
        // ...
    })
}
```

Safe, bounds-checked, endianness-explicit, and **each offset is checkable
against the spec by eye**. The crate's tests are the validation: a misplaced
offset fails a round-trip test instead of a live disk.

**Cheap interim step, worth doing regardless and independently:** add
`const _: () = assert!(size_of::<T>() == N)` for all four structs. A handful of
lines, and it converts the silent-misparse class into a compile error. Do this
even if the full refactor is never scheduled.

### 2.3 Added 2026-08-30, found during verification: the unchecked-assumption class is wider than layout

The audit's §2.1 was about field layout. Verification found three *semantic*
validation gaps in the same family, none of which layout assertions catch:

- **`read_inode` heap over-read (UB).** `read_inode` allocates
  `vec![0u8; state.inode_size]` and then `read_unaligned`s a whole `Inode`
  (128 bytes) out of it (ext2.rs:1684-1687). For a rev-1 superblock,
  `inode_size` comes straight off disk with **no minimum check**
  (ext2.rs:1126-1130): a corrupt-but-magic-matching image with
  `inode_size < 128` (or 0) reads past the end of a smaller heap allocation.
  Reachable from a corrupted disk image — exactly what a filesystem driver must
  survive. Fix: reject `inode_size < size_of::<Inode>()` at mount; the parse
  rewrite is the natural place.
- **Mount-path panics.** `block_group_count = (... ) / blocks_per_group`
  (ext2.rs:1133) divides by a disk-supplied value — `blocks_per_group == 0` on a
  corrupt image panics at mount. `1024usize << block_size_log` (ext2.rs:1125)
  shifts by a disk-supplied `u32` — debug panics on a huge value, release wraps
  into a garbage block size.
- These are one `validate()` away from fixed; see §5 step 2.

## 3. The fast-symlink family (2 sites)

`ext2.rs:2586` and `2648` copy in and out of `inode.direct_blocks` reinterpreted
as bytes — an ext2 fast symlink stores its target in the block-pointer fields.
`addr_of!`/`addr_of_mut!` are used correctly here (the struct is `packed`, so a
reference would be misaligned UB).

**Correction 2026-08-30, found during verification:** the target span is
**60 bytes, not 48**. `FAST_SYMLINK_MAX = 60` (ext2.rs:733) while
`direct_blocks: [u32; 12]` is only 48 bytes — the write at 2582-2588 and the
read at 2643-2650 deliberately run **through the three indirect-pointer fields**
that follow contiguously (`indirect`/`double_indirect`/`triple_indirect`,
+12 bytes). That is Linux's exact 15-pointer fast-symlink convention, and
`sectors_used == 0` is the fast/slow discriminator. Consequence for the fix:
the target is bytes **40..100 of the serialized inode** (all 15 pointer words),
not a subslice of `direct_blocks` — an implementation that stops at 48 bytes
silently caps targets at 48 and breaks on-disk Linux compatibility. The pair
falls out of the §2.2 serialize/parse split like any other field window.

## 4. The orphaned-lock family (3 sites) — revised

`Ext2Filesystem` keeps its state in a `spinning_top::RwSpinlock<Ext2State>` with
a side atomic recording the write-lock owner (`write_lock_owner`, ext2.rs:1031;
recorded on acquire at 1256-1262, cleared in `Drop` at 1008-1013 **before** the
inner guard releases). Three acquire paths — `write_state` (1272), `read_state`
(1238), and `try_read_state` (`cfg(test)`, 1193) — each poll every 10,000 spins:
if the recorded owner asks the scheduler as dead, `force_unlock_write()` the
third-party lock and clear the owner cell.

It is a **deadlock breaker**: `panic = "abort"` in every shipped profile
(Cargo.toml release and extreme-size, verified), so a thread killed at an
arbitrary instruction never runs its guard's `Drop`, and every later filesystem
operation spins forever. The killer sites are real: `mark_thread_terminated` is
called cross-thread from `src/syscall/proc.rs:323,455` and
`src/exceptions.rs:3825` while the victim may still be executing on a peer core,
and self-exit from `threading/mod.rs:4153`.

### 4.1 This is NOT the same as `akuma-bkl`'s recovery

Still true, still worth keeping. The two mechanisms share only a slogan:

| | `akuma-ext2` (today) | `akuma-bkl` `KernelLock` |
|---|---|---|
| lock type | `RwSpinlock<T>` (reader/writer) | FIFO ticket lock over a binary `owner` |
| unit | per **thread** (`current_thread_id`) | per **core** (MPIDR aff0) |
| trigger | recorded owner is **dead** (asks the scheduler) | queue **frozen** for `LOST_TICKET_RECOVERY_SPINS` |
| remedy | `force_unlock_write` — release a leaked guard | advance `now_serving` — consume an abandoned ticket |
| `unsafe`? | yes, third-party contract | **no** — pure atomics |

`akuma-bkl` never force-unlocks anything and never consults thread liveness; its
recovery is a CAS on a counter. See §4.3a for the *reason* that difference
exists — it is the root cause of this whole family.

### 4.2 Three copies of the policy, and they drift

`try_read_state` (`cfg(test)`), `read_state` and `write_state` each implement
dead-owner detection separately, with their own retry cadence (`try_read_state`
additionally guards on `attempt > 0` and returns `None` on exhaustion). Folding
them into one type is worth doing for that reason alone.

### 4.2a The recovery is not generation-safe — finding stands, fix superseded

**The finding is real and verified.** `write_lock_owner` holds a bare tid
(ext2.rs:1031). Tids are slot indices into the recycled 256-entry table
(`MAX_THREADS`; `SLOT_GEN` at `threading/mod.rs:382`, bumped on every
reclamation at `:1200` under the winning FREE→INITIALIZING CAS). And
`is_thread_terminated` (`threading/mod.rs:1644-1647`) reads the slot's
**current** state — `TERMINATED || FREE`. So:

```
1. thread T takes the write lock       write_lock_owner = T
2. T is killed holding it              guard leaked (no unwinding — panic=abort)
3. slot T is reclaimed and reissued    SLOT_GEN[T] += 1, new thread owns slot T
4. recovery polls  is_thread_dead(T)   reads slot T's CURRENT state
                                       -> new occupant is RUNNING -> false
5. recovery never fires                filesystem wedged, permanently
```

The window is not theoretical: the slot only has to be reissued before the next
filesystem operation completes its 10,000-spin poll, on a system that — by
construction — was just killing threads. It fails silently: the symptom is "the
filesystem hung". The mechanism the tree already learned this lesson for is
`WakeHandle` (`threading/mod.rs:409-411`, quote verified verbatim):

> **Wait queues must store THIS, not a bare `usize` tid** — a bare tid dequeued
> and then held across a preemption (or simply left behind by a dead waiter)
> wakes whoever owns the slot by then.

Two further holes verified the same day:

- **Leaked read guards are unrecoverable, today.** A thread killed holding an
  `Ext2ReadGuard` leaves spinning_top's reader count elevated forever. No
  recovery fires: `write_lock_owner == 0` fails the check, and
  `force_unlock_write` (even when it does fire) does not drain readers. Writers
  then starve permanently — the same availability loss as §4.2a, through a
  different door, with no tracking of who held how many reads.
- **The check-then-act race.** Between `is_thread_dead(owner)` and
  `force_unlock_write`, the lock's state can change; `force_unlock_write` is an
  **unconditional store** on a third-party lock, so a stale recovery can release
  a lock a live thread legitimately holds. Narrow, deliberate, but real.

**The earlier fix — generation-tagged owner handles with `mint`/`is_stale`
hooks — is superseded.** It treats the symptom: the lock still *infers* death
from slot state, keeps the polling copies, and keeps the race. §4.5 replaces
inference with a push.

### 4.3 Why you cannot just run the guard's `Drop` — and the root cause beneath it

Everything §4.3 originally said still holds: the guard is a local on the dead
thread's kernel stack; the kernel has no reflection over stack locals; unwinding
is off (`panic = "abort"`) and you cannot unwind *another* thread anyway. A
per-thread cleanup registry is per-acquire cost plus a liveness race at kill
time, because kill sites fire while the victim may still run on a peer core.

**Added 2026-08-30 — the actual root cause, learned from `akuma-bkl`:** the
`unsafe` exists because **ext2 does not own its lock's release operation**.
spinning_top's `force_unlock_write` is an unconditional store whose contract —
*"no guard for this lock exists"* — is a whole-program property ext2 cannot
check. bkl's `KernelLock` has no such problem because the lock owns its state:
release is a CAS on its own counters, so its recovery **performs the same
guarded operation a legitimate release would** — there is nothing to "force",
and the crate carries the recovery through `#![forbid(unsafe_code)]`
(`akuma-bkl/src/lib.rs:50`, verified). The law, from
`AKUMA_EXEC_SPLIT_AGAIN.md` §8: *the question is never whether the operation can
be made safe, it is who owns the thing being vouched for.* §2's blits have an
owner (the byte buffer); the orphan recovery had none, because the lock's state
word belonged to spinning_top.

### 4.3a The design that falls out: recovery *is* release

Give ext2 a lock we own. A flag-word reader/writer spinlock
(`state: AtomicU32`: one writer bit + reader count; per-tid cells beside it):
writer release is `CAS(WBIT → 0)`, reader release is `fetch_sub(1)` — **and the
recovery for a dead owner is the identical operation**. Because both are
CAS-guarded on the lock's own word, a stale recovery cannot double-release, and
at reap time — where the runtime guarantees the tid is 100% dead — it cannot hit
a live holder. Reader leaks become recoverable too: reader holds are counted
**per tid** (the shape `akuma-bkl`'s `ThreadTagTable` already uses,
`akuma-bkl/src/sync.rs:288`), so the sweep swaps the dead tid's count and
subtracts it. Zero `unsafe`. The crate can carry `#![forbid(unsafe_code)]` from
day one, and ext2's three lock sites simply leave.

### 4.4 `ThreadHooks` dies entirely

`akuma_ext2::init_thread_hooks(current_thread_id, is_thread_terminated)`
(`src/fs.rs:47`, verified — both members serve the recovery and nothing else).
In the new design ext2 does not ask liveness questions at all — the runtime
*tells* it. So the static hook table, the `Registered` indirection, and
ext2's tid vocabulary (`write_lock_owner`, the poll loops, the tid bookkeeping)
all go. What remains in `src/fs.rs` is **one optional registration**: the
waiter-side backstop kicker (§4.5). The audit's earlier honest scoping ("the
crate still names `fn(usize) -> bool` in a constructor parameter") is itself
superseded — it names nothing.

### 4.5 Where it lives, and how it wires up — revised 2026-08-30

**New leaf crate: `akuma-recoverable`.** Named for the property, not the
mechanism — deliberately not a sibling-sounding name for `akuma-bkl`, which it
does not generalize (§4.1: per-core BKL protocol, single consumer, stays
untouched; its law and its model-checker discipline are what move, not its
code — and `akuma-exec` has no bkl dependency today, verified).

```
akuma-recoverable   (leaf; deps: akuma-primitives, akuma-not-even-once; forbid(unsafe_code))
  RecoverableRwLock<T>   flag-word RW spinlock; release == abandon (§4.3a)
                         writer: owner cell (tid) + CAS(WBIT→0)
                         reader: per-tid hold counts + fetch_sub
  reap_tid(dead_tid)     the sweep: CAS-abandon the writer bit, drain the tid's
                         reader holds. Lock-free, idempotent, CAS-guarded.
  Registered<fn()>       the ONE upward wire: the 10k-spin backstop kicker,
                         degrading to plain spin when unregistered (same
                         degrade-as-today shape as THREAD_HOOKS.get()).
  host model checker     bkl_model.rs pattern: mutual exclusion, deadlock,
                         starvation, recovery-after-abandon.

akuma-exec    -> calls recoverable::reap_tid(i) where TERMINATED→FREE lands
                 (cleanup_terminated / slot scrub — next to preempt::scrub_slot,
                 threading/mod.rs:1184, the precedent for leaf-owned per-slot
                 state scrubbed at rebirth). WIRING IS A SEPARATE STEP.
akuma-ext2    -> state: RecoverableRwLock<Ext2State>; forbid(unsafe_code). ALSO
                 a separate step — nothing in ext2 changes until the crate lands
                 and is judged.
akuma-bkl     -> untouched.
```

Placement notes, resolved after considering `akuma-not-even-once` and
`akuma-primitives`:

- Not `akuma-not-even-once`: its *"core only, permanently"* header is
  load-bearing, and the lock needs `current_tid` — which lives in
  `akuma-primitives`, **above** not-even-once; hosting the lock there would
  force either a duplicated `TPIDRRO_EL0` asm or a tid hook (the thing this
  design exists to delete). What the lock takes from not-even-once is only the
  `Registered` kicker, which a leaf-on-leaf dependency already provides.
- Not `akuma-primitives`: it is the platform crate (`mrs`/MMIO); a lock protocol
  is not platform surface, and spinning_top-or-own-word locking does not belong
  in a crate whose unsafe is supposed to be exclusively CPU-shaped.
- `akuma-bkl` stays as is: §4.1. The new crate cites it; it does not extract it.

Ordering property worth stating: **the runtime never polls the lock, and the
lock never polls the runtime.** Recording happens at acquire (the acquirer's
tid, read natively via `akuma_primitives::preempt::current_tid()` — the audit's
original "no hook needed" argument, which generations had killed and the reap
design revives); recovery happens when the runtime reports a death. The only
upward call is the optional kicker.

### 4.6 Latency — included by construction

The first FS operation after a holder dies no longer stalls ~10k spins: the
sweep fires at the TERMINATED→FREE transition (`cleanup_terminated` runs from
`process/exec.rs:62,133,214` — the killer's own teardown), and the waiter-side
kicker preserves today's property that *any* waiter alone can unblock the
system even if a reap is late. No separate step, nothing extra to build.

### 4.7 Testability — the set-once objection dissolves

The original §4.7 argued a crate-level `Registered` predicate could not be
re-armed per test. Moot: there is no predicate. The host tests construct the
lock, `mem::forget` a guard (or take one under a fake tid), call `reap_tid`,
and assert recovery — no thread system, no VM, no set-once cells. The
model checker adds exhaustion where the old code had one live-VM race path.

## 5. Plan, in order

1. **Layout assertions** for the four structs (§2.2 interim). A few lines, no
   behaviour change.
2. **Explicit parse/serialize**, one struct at a time, smallest first
   (`DirEntryRaw` → `BlockGroupDescriptor` → `Inode` → `Superblock`). 14 sites
   → 0. The symlink pair (§3) falls out with `Inode` — as bytes 40..100 of the
   serialized form, Linux-compatible. Add the §2.3 validation (inode_size,
   blocks_per_group, block_size_log) as part of the new `parse`/mount path.
3. **`akuma-recoverable`** per §4.5 — lands **without wiring**: crate, tests,
   model checker, `forbid(unsafe_code)`. Nothing imports it yet.
4. **Wiring — landed 2026-08-31.** ext2 adopts the lock, `threading` calls the
   sweep at the TERMINATED→FREE transition, `src/fs.rs` registers the kicker,
   `THREAD_HOOKS`/`write_lock_owner`/the three poll loops are deleted, and
   `akuma-ext2` takes `#![forbid(unsafe_code)]`. §7 records what the step
   actually cost and the four places the sketch above was wrong.

## 6. Verification

- `cargo test -p akuma-ext2` for the §5 step-2 conversion — round-trip
  (`parse(serialize(x)) == x`) tests per struct as it converts, including a
  60-byte fast symlink and a name-padded `DirEntryRaw` (`rec_len` > actual).
- `cargo test -p akuma-recoverable` for §5 step 3 — model checker (mutual
  exclusion, deadlock freedom, starvation, abandon-after-death, double-abandon
  idempotence, reader-leak drain) plus API-level tests: forget a guard under
  tid 7, `reap_tid(7)`, assert writability; both kicker branches; registry
  churn (create/drop many locks).
- `scripts/verify_trim.py --tier all` diffed against a baseline worktree — the
  boot suite's `[FS Tests]` and the `fs_cache_warm_reread_hits` /
  `shared_file_mmap_writeback` / `unlinked_inode_survives_while_pinned` cases
  all exercise this crate. Plus the ext2 probes.
- **`e2fsck` the disk image after a write-heavy run** (and again after the
  self-host build) — the check that actually catches a bad offset.
- For §4.2a specifically: until step 4 lands, the bug stands in production;
  the regression test lives in the new crate's suite (§4.7) and the wiring step
  must show it failing on the old path and passing on the new one.

### 6.1 Verification run, 2026-08-31 — steps 1–3 carry no regression

Run against `4f3cf79c` (steps 1–3 landed; `akuma-locks-rw` still unwired), with
`b9ea61e0` — the commit **before** the codec (`a463585d`) — as the baseline arm.
Note the baseline is not `HEAD~1`: `6f143f98` and `4f3cf79c` only add the lock
crate and docs, and nothing imports the crate yet, so diffing against them would
have compared the codec to itself.

| check | result |
|---|---|
| `cargo test -p akuma-ext2` | 99 pass / 0 fail (100 after §6.2) |
| `cargo test -p akuma-locks-rw` | 17 pass / 0 fail (model checker + API) |
| `verify_trim.py --tier 1` | clippy clean on all four feature sets; 1064 host tests, 0 fail |
| `verify_trim.py --tier 2`, A/B'd against `b9ea61e0` | **summaries identical**, every key: `pass_marker` 100/100, `fail_set` empty, all 16 exercises `ok`, at both SMP=1 and SMP=4 |
| `ext2probe-host`, A/B'd against `b9ea61e0` | **every device-I/O counter identical**; the two dumped images differ in **3 bytes** — the pid digits in the probe's own `/ioprobe_<pid>` name |
| counters vs `crates/akuma-ext2/README.md` | match the recorded `+write-back` column (create 8.0 wr/file, 2 MB write 3.1x, delete 7.0 wr/file, flat dir O(1), 0 reads on a warm `stat`) |

A byte-identical on-disk image across a mixed 3200-file workload is a stronger
statement than any count: the explicit parse/serialize codec reproduces the
struct blits exactly.

**`e2fsck` without a VM.** §6 asks for an `e2fsck` after a write-heavy run, and
until now that meant booting, working the guest, killing QEMU and reading the
damage — which confounds a codec bug with the write-back cache's
no-sync-on-shutdown behaviour (`BKL_VFS_CARVE_OUT.md` §12.4). `ext2probe-host`
now takes `--dump PATH`, writing the post-workload, **post-`sync()`** image out;
`e2fsck -fn` on that has no VM, no kill and no dirty-cache confound in it. That
is the check that actually catches a bad offset, and it runs in seconds:

```bash
cd userspace && HOST=$(rustc -vV | grep '^host:' | cut -d' ' -f2)
cargo run --release -p ext2probe --bin ext2probe-host \
  --no-default-features --features host-probe --target $HOST \
  -- pristine.img --dump after.img
e2fsck -fn after.img            # must exit 0
```

Start from a freshly `mke2fs`'d image (`-F -b 4096 -L AKUMA`, `e2fsck`-clean),
not `disk.img` — a long-lived `disk.img` carries its own accumulated damage from
past hard kills (`docs/README.md`, the `devbox.img` row) and cannot answer this
question.

### 6.2 What that `e2fsck` found: `rmdir` leaked a link count. Fixed 2026-08-31

The first clean-image run reported **15 directory inodes** as *"inode N is in
use, but has dtime set"*, cascading into *"zero-length directory"*,
*"unallocated block #0"* and *"unconnected directory"*. The baseline arm
reported the identical 15, so the codec was exonerated immediately — but the
bug is real, and the deterministic host repro is what made it readable.

`Ext2Filesystem::remove_dir` set `deletion_time`, wrote the inode and freed the
number, but **never zeroed the directory's own `hard_links`**. It decremented
only the *parent's*. A directory lives at `hard_links = 2` (`.` plus the
parent's entry), so every `rmdir` left a freed inode whose on-disk record still
claimed two links — exactly the pairing `e2fsck` treats as corruption. `unlink`
never had it: its caller decrements to zero before `release_last_link` writes
the record.

One line (`inode.hard_links = 0;`) plus a crate test
(`remove_dir_zeroes_the_directorys_link_count`, which fails `left: 2, right: 0`
without the fix). After: `e2fsck -fn` on the dumped image **exits 0, no
findings**, with identical file and block totals (2021 files, 4691 blocks) and
**identical device-I/O counters** — the fix costs nothing.

This supersedes `BKL_VFS_CARVE_OUT.md` §12.4, which attributed the same
signature to an inode-bitmap block left dirty in the write-back cache and
discarded when QEMU was `kill`'d. That reading cannot hold here: `sync()` ran,
reported zero remaining writes, and the image was dumped afterward. §12.4's own
decisive test (reproduced at SMP=1) was right that it is neither a concurrency
tear nor a BKL effect; the cause is simply in `remove_dir`.

## 7. Step 4 as built (2026-08-31)

Landed as written, with four corrections the sketch could not have known.

### 7.1 The value had to go one crate further out than §4.5 said

§5 step 4 promised `akuma-ext2` would take `#![forbid(unsafe_code)]`, and §4.5's
landing note said a consumer "composes its own `UnsafeCell<T>` at the value's
owner". **Those two cannot both hold**: the deref is `unsafe`, so doing it in
ext2 costs ext2 the ban. The promise was written before the no-`T` deviation.

Resolved by making the composition parametric, in a new leaf crate
**`akuma-locks-rw-cell`** (206 lines, 8 `unsafe` sites: six `UnsafeCell` derefs
and the `Send`/`Sync` impls). It is generic over `T` and so never names
`Ext2State`, which stays `pub(crate)` exactly as before — no encapsulation was
spent to get the value out. The obligation it discharges is about the lock word,
not the payload, which is the same bargain `lock_api` already makes for every
`spinning_top` lock in this tree. Net: two enforced crates plus a 206-line
unenforced one, in place of a 3,043-line unenforced driver.

`lock_api::RawRwLock` was the first choice and reading the protocol ruled it
out: it wants tid-less `lock_exclusive()` / `unsafe unlock_exclusive()` as
separate calls, but `RecoverableRwLock` releases through a ticket's `Drop` and a
read release needs the tid whose hold it drains. Implementing it would have
meant `mem::forget`-ing the ticket and exposing public raw
`release_write`/`release_read(tid)` — reintroducing the very
"release a lock you may not hold" surface `force_unlock_write` had.

A `dyn Any` formulation also works (ext2 would downcast, still writing no
`unsafe`) and was rejected on cost, not on safety: a `Box` per mount, a downcast
per acquire on all 25 acquisition sites, an `unwrap` on a compile-time fact, and
the loss of `const fn new`. It buys no encapsulation the generic lacks.

### 7.2 The three poll loops did not just die — they moved, once

§4.4 said the loops go. They could not simply be deleted: ext2's per-attempt
`StateHoldGuard` (a `PreemptGuard` under `no-bkl-vfs`) must cover the winning
`try_*` and the resulting hold but **never** the backoff — masking across an
unbounded wait starves the core's timer, and a holder on that core could then
never run to release. A plain blocking `read()`/`write()` cannot express that.

So the loop moved *into* `akuma-locks-rw` as `read_as_holding` /
`write_as_holding`, which call a caller-supplied `hold()` before each attempt
and drop it before each spin. ext2's three ~30-line loops became three lines
each, and the protocol details they used to carry — the `WWAIT` re-announcement,
the ghost-`WWAIT` heal, the backstop cadence — are now in one place that
[`model`] speaks for.

### 7.3 `akuma-exec` needed a *new* hook, not the existing one

§4.5 has exec call `recoverable::reap_tid(i)`. There is no such function (the
shipped crate has no registry), and the obvious existing hook is wrong.
`SLOT_PURGE_CALLBACK` — the futex waiter purge — fires from **two** places, and
one is `mark_thread_terminated`, the instant a peer kill lands: there the slot
is merely TERMINATED, `ON_CPU` may still be set, and the thread can still be
executing inside the critical section whose lock the sweep would release.
Dropping a queue entry that early is safe; releasing a lock is not.

So step 4 adds `set_slot_reap_callback`, invoked at exactly one point — after
the `ON_CPU` gate, after the cooldown, after the TERMINATED→INITIALIZING CAS has
excluded every spawn, immediately before the FREE store. That is the reap
contract, verbatim.

### 7.4 The sweep must not go through the VFS mount table

The mount table already enumerates every filesystem and has the `sync_all`
precedent, which makes it the obvious place to sweep from. It is a deadlock.
`MountTable::resolve` returns a `&dyn Filesystem` **borrowed from the table**,
so its callers hold `src/vfs::MOUNT_TABLE` across filesystem calls: thread A
holds the mount table and blocks in ext2 on a lock a dead thread left held,
while the recycler blocks on the mount table trying to release it.

`src/vfs/ext2.rs` keeps its own four-slot `Weak` registry instead, written at
mount and read at reap, never covering device I/O or a filesystem call — so it
cannot be part of a cycle. `Weak` so a registration cannot pin an unmounted
filesystem forever.

The backstop kicker is `akuma_exec::threading::reclaim_terminated_slots`, the
collector-independent recycler pass (`BKL_VFS_CARVE_OUT.md` §11.4). A waiter
that has spun 10 000 times therefore drives the very sweep that frees it —
which is the property the old `is_thread_dead` poll was reaching for, without
asking a question that cannot be answered.

### 7.5 What it cost

Device-I/O counters over the full `ext2probe-host` workload are **identical**
to `86c2cfd7`, and `e2fsck -fn` on the dumped image exits 0. `verify_trim
--tier all`: clippy clean on all four feature sets, 1078 host tests (0 fail),
and the boot summary **identical on all 46 keys** at SMP=1 and SMP=4.

The end-to-end probe cannot resolve the lock itself — the whole run is ~0.21 s,
mostly reading the image into RAM, against a 10 ms host clock — so the fast path
is measured directly (`scripts/benchmarks/locks_rw_ab.sh`, best of 7 × 20 M
uncontended acquire/release pairs, reproducible to ±0.02 ns/op):

| path | `spinning_top::RwSpinlock` | `RecoverableCell` | Δ |
|---|---:|---:|---:|
| write acquire+release | 1.39 ns | 2.58 ns | **+1.19 ns** |
| read acquire+release | 3.13 ns | 3.96 ns | **+0.83 ns** |

+86% and +27% in relative terms, and irrelevant in absolute ones: ext2 acquires
once or twice per filesystem operation, and one operation costs microseconds —
a warm 4 KB read excursion is ~2050 ns (`EXT2_READ_PATH_STAGE_PROFILE.md`), so
this is ~0.05% of it, against ~8 device writes per file create at a virtio
round-trip each. The contended path is deliberately not measured: its cost is
dominated by how long the holder holds, which is device I/O.

What it bought: the three `force_unlock_write` sites and the thread hooks are
gone, `akuma-ext2` is enforced `#![forbid(unsafe_code)]` (3,043 production
lines, the largest crate yet to make the move — enforced crates 21 → **22 of
32**), the §4.2a recycled-tid bug is fixed by construction, and **leaked read
holds are recoverable for the first time** — the old design tracked one writer
tid and nothing else, so a thread killed holding a read lock blocked every
future writer forever with no code path that could notice.

Three regression tests in `akuma-ext2` pin it: a thread killed holding the write
lock, one killed holding two read locks, and a sweep of an unrelated tid leaving
a live holder alone. The first is §6's "must show it failing on the old path"
case — the old path could not pass it at all, because nothing in a host test
ever marks a thread dead, and recovery there was gated on asking.

## Background

- `docs/archive/AKUMA_EXEC_SPLIT_AGAIN.md` §7.9, §8.6, §8.8 — the three prior
  `unsafe` reductions this follows, and the lesson they share: the question is
  never whether the *operation* can be made safe, it is **who owns the thing
  being vouched for**. §2 has an owner (the byte buffer, trivially). §4 did not
  — §4.3a gives it one.
- `akuma-bkl/src/bkl_model.rs` — the host model checker whose pattern the new
  crate replicates; and `akuma-bkl/src/lib.rs:50` — the proof that
  recovery-without-unsafe is reachable when the lock owns its state.
- `docs/reference/crate-safety.md` — `akuma-ext2` is listed at 18 sites,
  "`repr(C)` on-disk structures read through raw byte buffers". Accurate; §5 is
  the path to zero.
