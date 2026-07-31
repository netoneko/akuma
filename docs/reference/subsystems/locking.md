# Kernel locking: the BKL and how carve-outs work

> **Stability: B (verify behaviour).** The rules below are extracted from two
> completed carve-outs (`no-bkl-network`, `no-bkl-vfs`) and are consistent
> across both, but the underlying BKL-removal effort is itself grade **C** —
> check `smp-shared.md` and the archive docs below for what has landed since
> this was written.

This is the distilled "how to add a lock, or remove one, without re-deriving
the lessons from scratch" reference. It is not a history — for the blow-by-blow
of any specific fix, follow the links into `docs/archive/`.

## The model

Under `smp-shared` (real shared-kernel SMP, `cfg(kernel_smp_shared)`), one
kernel runs across all cores. Correctness is anchored by a single **Big Kernel
Lock** (`akuma_exec::sync::KernelLock`, driven via `akuma_exec::bkl`):
**held iff a core is in EL1**, reconciled at every EL transition
(`reconcile_for_spsr`, called from IRQ/exception epilogues), with a fair FIFO
ticket wait so no core starves. Off `smp-shared` it's a zero-cost no-op.

The BKL upgraded the kernel's old single-core invariant (`with_irqs_disabled`,
which only gives mutual exclusion on *one* core) to something SMP-safe in one
stroke, at the cost of serializing everything. The ongoing work is **carving
subsystems out from under it** — not by inventing a new lock hierarchy, but by
noticing that most kernel state already has its own fine-grained lock (fd
table, socket table, ext2 superblock/BGD, block cache, network stack), and the
BKL is simply *redundant* for syscalls that only touch that state. See
`smp-shared.md` for the milestone-by-milestone history (M0–M5) and
`docs/reference/subsystems/smp.md` for the other, share-nothing SMP model this
is not.

Four carve-outs exist today: `no-bkl-network` (all smoltcp net syscalls +
socket `read`/`write`), `no-bkl-vfs` (ext2 read paths, `mmap`, `unlinkat`,
`openat`, `renameat`/`renameat2`, `mkdirat`, `fchmodat`), `no-bkl-process`
(`fork_process`'s CoW page-copy window), and `no-bkl-mm`
(`mprotect`/`madvise`/`munmap`/`mremap`/`mmap`). All four are default-on in
`smp-shared` (since 2026-08-01). `no-bkl-process` is the first that is not about I/O: it overlaps a
CPU-bound page copy with peer-core EL1 rather than a disk or wire wait, and it is
the first to lean on a *page-table* inner lock (`as_lock`) rather than a
subsystem's own state locks. `no-bkl-mm` is the first phase picked by the plan
rather than by attribution (no mm syscall has ever been measured as a significant
BKL holder) — see `BKL_MM_CARVE_OUT.md` §5 — and the first where the audit found
real gaps (an unguarded free-list, a page-table mutation with no `as_lock`) rather
than only rediscovering an existing lock.

## The carve-out playbook

For a given syscall or subsystem:

1. **Scope the window as narrowly as possible.** Wrap only the on-disk/on-wire
   work in the guard (`VfsBklGuard` / `NetBklGuard`), not the whole function.
   Keep outside the window: user-string copies, fd-table-only lookups, and
   every early-error return (`EBADF` etc.) — those don't touch the shared
   state the carve-out is protecting, so they shouldn't pay for a BKL
   drop+reacquire. Cross-core forwarding arms (multikernel bounce) also stay
   outside — they marshal through the BKL-protected bounce and must keep the
   lock.
2. **Don't invent a new coarse lock.** The first instinct — wrap the syscall
   in one new `NETWORK_LOCK`/`VFS_LOCK` — doesn't work for anything that
   blocks: `accept`/`connect`/`recv` and friends yield via `blocking_relax()`,
   and holding a coarse lock across that yield serializes *everything* behind
   one blocked call, which is strictly worse than the BKL (which *is* dropped
   during the wait). Instead, drop the BKL for the syscall's duration and rely
   on the fine-grained locks that state already has. If a coarse
   ordering-enforcement scaffold exists (`akuma_net::locks::NETWORK_LOCK`),
   treat it as documentation/host-test-only, not a hot-path primitive, unless
   it's been proven to survive a blocking wait.
3. **Write a boot self-test that drives the real syscall entry point**
   (`handle_syscall(...)`), covering: the on-disk work happening inside the
   window, dirfd/cwd-relative resolution, early-error paths staying balanced,
   and any fast-path branches (device nodes, etc.) that must stay outside the
   window.
4. **Boot-verify correctness at SMP=2**: full self-test suite green, 0 PANIC/
   WILD, 0 stale dropped-window-ledger heals.
5. **Measure contention with a same-binary A/B**, not just a before/after
   commit comparison — build with the `bkl-profile` feature (`cfg(
   kernel_bkl_profile)`, `src/bkl_profile.rs`), run the identical workload
   twice at SMP=4 with only the new guard toggled, and compare the syscall's
   `[BKLPROF]` cumulative/peak share and total workload spin count. A
   same-binary A/B is what tells "this conversion measurably helped" apart
   from "this syscall was never contended in the first place" — see
   `BKL_VFS_CARVE_OUT.md` §13.3 for a worked example.

   Two rules the campaign learned the hard way about *how* to toggle and *what*
   to sum (`BKL_VFS_CARVE_OUT.md` §17):

   - **Toggle in source, not in cargo features.** Swap the guard out
     (`git show HEAD:src/syscall/fs.rs > …`) and keep the feature set byte-
     identical across both sides. A source edit forces a recompile, so the ELF
     is always newer than the `.bin`; alternating feature sets was what made
     `cargo_runner.sh`'s old `[ "$ELF" -nt "$BIN" ]` guard boot the *other*
     side's kernel behind a "Finished in 0.1s" cargo line. That guard is gone
     (objcopy now always regenerates), but the habit is still the safer one —
     and if you must alternate features, verify the boot is the kernel you
     think it is by grepping the `.bin` for a string only one side contains.
   - **Sum only the workload's windows.** `analyze.py`'s default is whole-boot
     and gets diluted by idle/teardown; filtering by spin magnitude instead of
     by time is worse still, since it counts service bringup as workload on a
     boot whose regimen starts early. Take `drive.py`'s REGIMEN START/DONE (or
     the first/last regimen `execve` in the serial log) and sum `[BKLPROF]`
     per-tag spins over the `t=` windows spanning that interval, on both sides.
6. **Verify data integrity, not just "didn't crash."** A syscall that early-
   returns an error on every call (broken, not fast) can look identical to a
   correctly BKL-free one in a profiler — `rm -f`/`cp` swallow errors. Check
   real content: sha256 digests across the carve-out, `ls`/`e2fsck` after
   deletes, etc.
7. **Let attribution — not intuition — pick the next target.** Phase 0's
   estimate ("scheduler/IRQ holds ~70% of contended time") was wrong for the
   real workload; measured, it was 27%, and a single syscall (`unlinkat`) was
   72.6%. Converting `unlinkat` didn't just remove its share — it promoted the
   next-largest holder (`openat`, 36.6%) into visibility. Don't convert the
   next syscall on a checklist; profile, then convert whatever the profile
   names. This kept paying off down to small holders: `renameat` (2.8%),
   `mkdirat` (2.6%), and `fchmodat` (1.8%) only showed up at all once the
   driving workload was extended with a `mkdir`+`rename`+`chmod` phase — the
   standing regimen never exercised them. Once every syscall an attribution
   run had ever named was converted, `irq/sched` alone was 66–73% of
   remaining spin — the signal that the next round of effort belongs in
   Phase 3, not further down the VFS syscall list.

## Correctness rules learned the hard way

Each of these was a real bug found during a carve-out, not a hypothetical.

- **Latch the guard's decision at construction; never re-read a runtime
  toggle in `drop()`.** `VfsBklGuard`/`NetBklGuard` consult a runtime on/off
  toggle (for A/B and as a kill switch), and that toggle is genuinely flipped
  while guards are live. A guard that reads it in both `new()` and `drop()`
  can drop the BKL under "on" and then decline to re-acquire under "off" —
  unbalancing the syscall wrapper's `leave_kernel` and corrupting the ticket
  FIFO for every other core. Fix: latch the decision once, in a field, at
  construction. (`BKL_VFS_CARVE_OUT.md` §2.4; host test
  `vfs_bkl_guard_latched_arm_stays_balanced_across_toggle_flip`.)

- **Mask IRQs/preemption per *attempt*, never across an unbounded wait.**
  `read_state`/`write_state`-style acquisition loops can spin for a long time
  (they carry orphaned-lock recovery paths for exactly that reason). Masking
  IRQs across the whole wait starves this core's timer for the entire
  contended window — and if the current holder is a thread *on this core*,
  nothing can ever run to release it. Take the preempt/IRQ guard immediately
  before the non-blocking `try_lock()`, keep it only on success, drop it
  before the backoff spin. (§3.1.)

- **Field order in a guard struct is load-bearing.** When a guard carries both
  an inner lock guard and a preemption/IRQ-mask guard, declare the lock field
  *before* the hold field, so the lock releases before preemption/IRQs are
  restored on drop. The reverse order reopens, for an instant, exactly the
  window the guard exists to close. (§3.)

- **An interrupt landing inside a dropped-BKL window will silently convert it
  back to BKL-held, unless you track that on purpose.** The reconcile
  invariant ("BKL held iff EL1") is enforced at every `eret` against the
  *interrupted frame's* SPSR — which is EL1 — so a timer IRQ during a dropped
  window makes the epilogue re-take the BKL and hold it for the rest of the
  window, silently re-serializing what was supposed to be BKL-free. Fixed
  with a **per-thread depth of open dropped-BKL windows** consulted at every
  `eret`: "target is EL1 *and* the resumed thread has an open window" also
  means release. Thread-scoped, not core-scoped, because windows survive
  preemption and cross-core migration. Any new dropper (a new guard type, a
  new blocking-wait site) must go through this same
  `bkl::dropped_window_open()/close()` pair, not a bare `leave_kernel`/
  `enter_kernel`. (§9; `akuma_exec::bkl::DroppedWindowLedger`.)

- **A guard's inner spinlock must mask local IRQs too, if it's reachable from
  a BKL-free window.** Otherwise: the window holds the inner lock, a nested
  IRQ does an unconditional `enter_kernel()` hard-spin wanting the BKL, and if
  the BKL's owner is spinning on that same inner lock, the two cores deadlock
  AB-BA. (Network Phase 2, `PreemptGuard` now masks IRQs for exactly this
  reason.)

- **Never hold a lock (BKL or inner) across a blocking/cooperative wait.** A
  thread parked in `blocking_relax()`/`schedule_blocking()` while holding
  *anything* freezes every peer core that wants it — this was the literal
  cause of the meow→LLM freeze (recv held the BKL across the wait). Route
  every blocking wait through `blocking_relax()`, which itself is `yield_now`
  (or, under smp-shared, `idle_halt`) with no lock held across it. (`smp-
  shared.md` M5d.)

- **Raise signals after releasing locks, not while holding them.** `pipe_write`
  used to call `send_sigpipe()` while holding the global `PIPES` spinlock;
  default disposition terminates the writer inline, which re-enters
  `pipe_close_write` and tries to re-acquire the same lock the caller is still
  holding — a same-core self-deadlock that then starves every peer piled up
  on the BKL. (Network Phase 2, `test_sigpipe_terminate_no_deadlock`.)

- **Don't hard-terminate a sibling thread mid-EL1.** Marking a sibling
  `TERMINATED` while it's preempted inside a kernel excursion stranded
  whatever spinlock it was holding (a forktest child died holding
  `BLOCK_DEVICE`, freezing all later disk I/O). Fix: post a pending-kill
  request, leave the sibling schedulable, and let it self-terminate at its
  own next kernel-exit boundary, where every lock is guaranteed released.
  (`smp-shared.md` M5e.)

- **Route special fd kinds before the guard opens, not through it.**
  Pipe-backed `UnixSocket` fds must not run through the BKL-free socket path
  — dispatch on fd kind before the `NetBklGuard`/`VfsBklGuard` window starts.

- **A leaked dropped-window depth on a recycled thread slot is catastrophic**
  (unrelated EL1 code would run BKL-free). Two independent safety nets: the
  EL1 fault-kill path force-clears the ledger (its destructors never run —
  the kernel stack is abandoned), and EL0 entry with a nonzero depth is a
  tripwire that heals and logs rather than trusting the stale value.

- **Deferred cleanup gated to "must run from thread 0" has no recovery path
  under load.** Slot/resource reclamation that only runs from one specific
  core's idle loop starves the moment that core is never idle — which is
  exactly when reclamation is needed. Prefer: reclaim on demand at the
  allocation-miss site (with a retry), plus a steady-state collector on
  whatever thread runs a busy poll loop, gated by a *time cooldown*
  (protects against recycling a slot too early) rather than a *caller-identity*
  gate (which provides no actual safety — a CAS on the state transition does).
  (§11.4/§11.7.)

## Syscall → lock map

Ground truth as of 2026-08-01, verified against `src/syscall/{fs,net,mem,proc}.rs`
and the inner-lock call sites in `crates/akuma-ext2/src/ext2.rs`,
`crates/akuma-exec/src/process/fd.rs`, `src/block.rs`, `src/vfs/mod.rs`, and
`crates/akuma-net/src/{socket,smoltcp_net}.rs` — not transcribed from the
archive doc's prose. Re-derive rather than trust this table once it's more
than a few months old; grep the guard names below to check it hasn't drifted.

### `no-bkl-vfs` — `src/syscall/fs.rs` / `src/syscall/mem.rs`

| syscall | guard scope | inner lock(s) held while BKL dropped |
|---|---|---|
| `sys_read` (File arm, `fs.rs:390`) | inside `File` match arm | fd table `Spinlock` → ext2 `read_state` (`read_at`, `ext2.rs:1918`) → block cache + `BLOCK_DEVICE` `Spinlock` (`block.rs:244`) |
| `sys_pread64` (`fs.rs:714`) | inside `File` arm | same as `sys_read` |
| `sys_pwrite64` (`fs.rs:782`) | inside `File` arm | ext2 `write_state` (`write_at`, `ext2.rs:1962`) |
| `sys_write` (`fs.rs:807`, `new_if`) | whole fn (match nested in per-chunk loop) | ext2 `write_state` (`write_at`) |
| `sys_lseek` (`fs.rs:1540`, `new_if`) | whole fn | ext2 `read_state` (`metadata`, `ext2.rs:2268`, via `file_size`) |
| `sys_fstat` (`fs.rs:1590`) | inside `File` arm | ext2 `read_state` (`metadata`) |
| `sys_newfstatat` (`fs.rs:1716`) | after `#[cfg(kernel_smp)]` cross-core forward arm | ext2 `read_state` (`is_symlink`, `read_symlink`, `metadata`) |
| `sys_statx` (`fs.rs:1964`) | after path resolution | ext2 `read_state` (`is_symlink`, `metadata`) |
| `sys_getdents64` (`fs.rs:2448`) | after `File`-kind match | ext2 `read_state` (`read_dir`, `ext2.rs:1825`) |
| `sys_openat` (`fs.rs:1381`) | after cross-core forward arm + all `/dev/*`/`/proc/self/exe` fast paths | ext2 `read_state` (`exists`/`lookup_path`), then `write_state` for `O_CREAT`/`O_TRUNC` (`write_file`, `ext2.rs:1869`) + `chmod` |
| `sys_mkdirat` (`fs.rs:2214`) | after dirfd/base-path resolution | ext2 `write_state` (`create_dir`, `ext2.rs:2045`) |
| `sys_fchmodat` (`fs.rs:1845`) | after dirfd/base-path resolution | ext2 `read_state` (`resolve_symlinks`) then `write_state` (`chmod`, `ext2.rs:2286`) |
| `sys_unlinkat` (`fs.rs:2268`) | after dirfd/base-path resolution | ext2 `write_state` (`remove_file` `ext2.rs:2126`, `remove_dir` `ext2.rs:2162`) — the multi-second-hold case (§ archive doc §7.2) |
| `sys_renameat` (`fs.rs:2309`) | whole fn after path-arg copies | ext2 `write_state` (`rename`, `ext2.rs:2234`) |
| `sys_renameat2` (`fs.rs:2343`) | whole fn, incl. `RENAME_NOREPLACE` exists probe | ext2 `read_state` (`exists`) then `write_state` (`rename`) |
| `sys_mmap` eager fill (`mem.rs:360`) | around per-frame disk-fill loop, before install | ext2 `read_state` (`read_at`) |
| `sys_mmap` lazy inode-resolve (`mem.rs:296`) | around `resolve_inode` only | ext2 `read_state` (`lookup_path`) |
| `mmap_eager_to_lazy_fallback` (`mem.rs:175`) | around `resolve_inode` only | same |

**Not converted (still fully BKL-held):** `sys_dup`/`sys_dup3` (`fs.rs:1121`,
`1146`), `sys_close`/`sys_close_range` (`fs.rs:1429`, `1476`), `sys_fstatfs`
(`fs.rs:1081`, synthesized — no real VFS touch but no guard either),
`sys_fcntl` (`fs.rs:2107`), `sys_fchmod` (`fs.rs:1797`, calls `write_state`
BKL-held), `sys_fallocate`/`sys_ftruncate`/`sys_truncate` (`fs.rs:1863/1880/1894`,
call `write_state` BKL-held), `sys_faccessat2` (`fs.rs:2046`), `sys_getcwd`
(`fs.rs:2088`, no I/O — cached `proc.cwd`, no guard needed), `sys_symlinkat`
(`fs.rs:2361`), `sys_linkat` (`fs.rs:2380`, copy not hardlink), `sys_readlinkat`
(`fs.rs:2394`), `sys_fchdir`/`sys_chdir` (`fs.rs:2508`, `2529`).

**Footnote for whoever converts `truncate` next:** `Ext2Filesystem::truncate`
(`ext2.rs:2331-2355`) takes only `read_state()` — a shared guard — even though
it mutates the inode via `write_inode`. Works today because `write_inode`/
`write_block` only need `&Ext2State` (the block-cache `Spinlock` guards the
actual mutation, not the `RwSpinlock`), but it means truncate's mutation isn't
serialized against concurrent readers the way `write_file`/`create_dir` are.
Not currently exposed to a BKL-free window — audit this before converting
`truncate`/`ftruncate`.

### `no-bkl-network` — `src/syscall/net.rs`

`NetBklGuard` is purely compile-time gated (no runtime toggle, no latching
needed — unlike `VfsBklGuard`).

| syscall | guard scope | inner lock(s) held while BKL dropped |
|---|---|---|
| `sys_socket` (`net.rs:123`) | whole fn | fd table; `SOCKET_TABLE` `Spinlock` (`akuma-net/src/socket.rs:256`) |
| `sys_bind`/`sys_listen` (`net.rs:239`, `269`) | whole fn | `SOCKET_TABLE`; `NETWORK` `Spinlock` under `PreemptGuard` (`smoltcp_net.rs:106`) |
| `sys_accept`/`sys_accept4` (`net.rs:286`, `332`) | whole fn, incl. blocking wait (no lock held across it) | `SOCKET_TABLE`; `NETWORK` |
| `sys_connect` (`net.rs:388`) | whole fn | same |
| `sys_getsockname`/`sys_getpeername` (`net.rs:426`, `455`) | whole fn | `SOCKET_TABLE`; `NETWORK` |
| `sys_sendto`/`sys_recvfrom` (`net.rs:508`, `594`) | after the unix-socket pipe-routing check (deliberately BKL-held to avoid an AB-BA with a nested IRQ) | `SOCKET_TABLE`; `NETWORK` |
| `sys_setsockopt`/`sys_getsockopt` (`net.rs:723`, `807`) | whole fn | `SOCKET_TABLE`(; `NETWORK` for getsockopt) |
| `sys_sendmsg`/`sys_recvmsg` (smoltcp build, `net.rs:901`, `1051`) | after unix-socket iovec check | `SOCKET_TABLE`; `NETWORK` |
| `sys_resolve_host` (DNS, `net.rs:1317`) | whole fn | `NETWORK` |
| `sys_read`/`sys_write` on `Socket` fd (`fs.rs:417`, `913`) | inside `Socket` match arm | `SOCKET_TABLE`; `NETWORK` |

Not converted / correctly guardless: `sys_socketpair` (AF_UNIX, pipe-only,
never touches `SOCKET_TABLE`), `sys_shutdown` (hardcoded no-op), `sendmsg`/
`recvmsg` on the non-smoltcp rump-sysproxy build (pipe-only path).

### Adjacent BKL-drop sites outside the named carve-outs

These call `akuma_exec::bkl::dropped_window_open()/close()` directly (same
ledger primitive, no `VfsBklGuard`/`NetBklGuard` struct):

| site | toggle | scope | inner lock |
|---|---|---|---|
| `sys_execve` ELF read (`proc.rs:649`, inside `sys_execve` `proc.rs:544`) | `exec_bkl_drop_enabled()` (`smp_shared.rs:87`) | whole-file `read_file` — **only** in the single-image smp-shared build (`not(kernel_profile_size)`, `not(kernel_smp)`); size build reads just a 256-byte shebang header, multikernel build forwards cross-core, neither takes this window | ext2 `read_state` |
| Data-Abort demand-page fill (`exceptions.rs:2964`/`3017`) | `fault_bkl_drop_enabled()` (`smp_shared.rs:68`) | per-page fill loop only; page-table install is separate, BKL-held | ext2 `read_state` |
| Instruction-Abort demand-page fill (`exceptions.rs:3487`/`3533`) | same | mirror of Data-Abort path | same |

### `no-bkl-process` — `crates/akuma-exec/src/process/mod.rs`

Phase 3, landed 2026-07-31 and **on by default in `smp-shared`** since the same
day, like the other two.

Contention-confirmed by a same-source `bkl-profile` A/B at SMP=4 on the standing
regimen: **`clone` 19.5% → 2.5%** of workload-window contended time (23.9M → 2.8M
spins, 8.6×), dropping it from the #2 holder to a minor one, with total workload
spins down 9% and 6/6 digests exact on both sides.
`ProcessBklGuard` (`process/bkl_guard.rs`) mirrors `VfsBklGuard`: runtime toggle
`process_bkl_drop_enabled()` (default on, latched at construction), same
dropped-window ledger.

| site | guard scope | inner lock(s) held while BKL dropped |
|---|---|---|
| `fork_process` CoW share/demote pass | the copy loop only — after the `for_each_process` sibling scan, the `LAZY_REGION_TABLE` snapshot, and `propagate_lazy_regions_to_child` (all hoisted BKL-held ahead of it); before steps 5–8 | thread-group **leader's** `Process::as_lock` in 64-page chunks (`FORK_AS_CHUNK_PAGES`, IRQ-masked via `AsLockHold`), plus `COW_REFCOUNTS` `Spinlock` and the PMM/allocator locks |

Two things about this one differ from the fs/net pattern and are easy to get
wrong:

- **It's the thread-group leader's `as_lock`, not the forking thread's.**
  `CLONE_THREAD` siblings each get a fresh `Spinlock` in their own `Process`
  while sharing one address space; the fault handler picks its owner with
  `address_space_owner_pid_for_fault()` (TTBR0 → the non-shared process owning
  that L0), so fork resolves the same way. Taking `parent.as_lock` from a worker
  thread would hold a lock nothing else waits on.
- **The hold is chunked, and the PTE read + `cow_ref_inc` + demote + range-flush
  are one hold.** `AsLockHold` masks IRQs, so it cannot span the copy (timer
  starvation, and the AB-BA against a BKL-holding `munmap` that wants
  `as_lock`); but the four steps above must be atomic against a peer's CoW fault
  or fork can `cow_ref_inc` a frame that fault just freed. That constraint is
  what merged the demote into the share pass — it used to be a separate second
  walk.

**Still fully BKL-held in `fork_process`:** steps 1–3 (allocation only, nothing
to relieve) and steps 5–8 — `ProcessInfo` write, `get_saved_user_context` /
`update_thread_context` (`THREAD_CONTEXTS` is an unlocked `UnsafeCell`),
`spawn_user_thread_initializing`, `register_process` + `mark_thread_ready` (the
publication point). Those touch state with no inner lock, where the BKL *is* the
lock — the original audit's finding there stands. Also still BKL-held: the
eager-copy (non-CoW) fork branch (unreachable, `COW_FORK_ENABLED = true`) and
`sys_clone`'s routing layer (`VFORK_WAITERS`, nanosecond-scale). `execve`'s
`replace_image` destructive window remains the single most dangerous carve-out
target in the space and is untouched.

Background: [`BKL_PROCESS_CARVE_OUT.md`](../../archive/BKL_PROCESS_CARVE_OUT.md)
— §§1–8 are the original audit (which concluded no carve-out was possible), §9
is what actually landed and why §2.4a's "the parent's page tables have no lock"
was wrong: the CoW fault handler was already editing those same PTEs BKL-free
under `as_lock`, so the inner lock existed and fork just wasn't taking it.

### `no-bkl-mm` — `src/syscall/mem.rs`

Phase 5, landed 2026-08-01, promoted to `smp-shared` default-on 2026-08-01.
Picked by the plan, not by attribution (no mm syscall has ever measured as a
significant BKL holder).

| syscall | guard scope | inner lock(s) held while BKL dropped |
|---|---|---|
| `sys_mprotect` (`mem.rs`) | after `proc` resolves | `Process::as_lock` (PTE flag edits); `LAZY_REGION_TABLE` |
| `sys_madvise` (`mem.rs`) | after `proc` resolves, per match arm | `Process::as_lock` (prefault install / zero-mapped); `LAZY_REGION_TABLE`; PMM (`MADV_WILLNEED`'s batch alloc) |
| `sys_munmap` (`mem.rs`) | after `proc` resolves | `Process::vm_lock` (region lookup/removal + free-list); `Process::as_lock` (PTE unmap); `SHARED_FILE_MAPPINGS`; `LAZY_REGION_TABLE`; PMM (frame free) |
| `sys_mremap` (`mem.rs`) | after `proc` resolves once (not re-looked-up inside the window) | `Process::vm_lock`; `Process::as_lock`; `LAZY_REGION_TABLE`; PMM |
| `sys_mmap` (`mem.rs`) | after `proc` resolves | `Process::vm_lock` (free-list alloc via `vm_alloc_mmap`); `Process::as_lock` (PTE install); `LAZY_REGION_TABLE`; PMM; `SHARED_FILE_MAPPINGS`; nested `VfsBklGuard` windows for on-disk fill (ledger depth-counted, safe to nest) |

Two gaps closed as a prerequisite (not present before this phase, not a "nothing to
build" finding like net/vfs/process):

- **`ProcessMemory::free_regions`/`alloc_mmap()`** was a plain unguarded `Vec` —
  every mm syscall now goes through `Process::vm_alloc_mmap`/`vm_free_mmap`, which
  fold it under the existing `vm_lock` (same IRQ-disabled, pure-bookkeeping
  discipline `vm_lock` already enforces for `mmap_regions`).
- **`sys_mmap`'s OOM/reclaim fallback** (`reclaim_clean_file_pages` →
  `try_evict_ro_page`) mutated page tables with no `as_lock` hold at all — fixed
  with a per-page (not per-sweep) `as_lock_hold`.

Background: [`BKL_MM_CARVE_OUT.md`](../../archive/BKL_MM_CARVE_OUT.md) — §1 is the
audit (the two gaps above), §3 is verification (boot self-test at SMP=2/4, the
`mmap_stress`/`mmap_file`/`mmapsum`/`fpfault`/`neonfault` + `llama-bench` suite,
the contention regimen), §4 is a same-binary Akuma-vs-native-Linux tok/s comparison
(bonus, not part of the carve-out itself), §5 explains why this phase has no
before/after contention number the way `unlinkat`/`netpoll_drain` do.

## Attribution tooling

`bkl-profile` (cargo feature → `cfg(kernel_bkl_profile)`, `src/bkl_profile.rs`)
turns on a per-tag BKL-hold profiler for the whole boot and prints a **delta**
histogram every 10s from the async-main loop, crediting spins to whatever the
*owning* core was doing when a waiter first observed contention. It's
measurement-only and perturbs timing, which is why it's a separate feature
rather than part of `smp-shared` — never compare absolute spin counts across
sessions, only shares/ranks within one same-binary run, and prefer the
`[BKL] stuck` threshold counter (present in every build) as the
profiler-independent cross-check.

**Attribution is thread-scoped** (since 2026-07-31, `BKL_VFS_CARVE_OUT.md` §18).
A kernel excursion belongs to a thread — it survives preemption and can resume
on another core — so the authoritative tag lives in a per-thread table
(`sync::ThreadTagTable`, indexed by `current_thread_id()`), and the per-core
`HOLDER_TAG` the waiter samples is a *cache* of it. Three operations maintain
the invariant `HOLDER_TAG[c] == THREAD_TAG[thread running on c]`:
`set_holder_tag` (kernel entry) writes both; `set_core_tag_transient` (the IRQ
dispatch stamp) writes only the core cache, so the interrupted thread's tag is
never clobbered; `load_thread_tag_to_core` reloads the cache wherever the
current thread changes (`set_current_thread_register`, which every switch path
funnels through) and at the IRQ epilogue.

This replaced a per-core-only tag that was **not** trustworthy: a tick that
context-switched handed the incoming thread the "irq/sched" label, and a thread
preempted inside a long BKL-held syscall never re-entered the kernel to correct
itself, so it ran that syscall's whole remainder labelled "irq/sched". Because
the long excursions are the ones that get preempted, the artifact pooled in one
bucket — on the campaign's SMP=4 regimen, `irq/sched` measured **88.8%** before
the fix and **23.0%** after, on a matched same-disk A/B. Do not compare a
pre-2026-07-31 `irq/sched` share with a later one.

Buckets: `0..=499` syscall numbers, `500` fault, `501` irq/sched, `502` idle,
`503` netpoll, `511` unknown. `idle`/`netpoll` name the two long-lived kernel
threads that hold the BKL with no syscall or fault entry point (the per-core
idle loops; the async-main smoltcp drain) — before they were named, their holds
inherited whatever their core last did, which is how they fed `irq/sched`.
`unknown` now means "held by a thread that never passed a tagging site", a real
answer — except in a build without `bkl-profile`, where `tag=511` everywhere
just means the profiler isn't compiled in.

Read attribution over the **workload windows**, not whole-boot
(`BKL_VFS_CARVE_OUT.md` §17.2): `scripts/bkl_smp_regimen/analyze_workload.py
--auto <serial.log>` derives the interval from the regimen's own `execve`
footprint and sums per-tag spins over just those windows. `analyze.py`'s default
whole-boot view is diluted by bringup and idle.

```
cargo build --profile release-smp-shared --features devbox-smoltcp,no-tests,bkl-profile
```

Driving workload: `net4` (concurrent downloads → net + ext2 write), `read4`
(concurrent reads with a working set bigger than the block cache), `cp2`
(read+write in one process), `rm` (mutating syscalls). See
`BKL_VFS_CARVE_OUT.md` §11.1 for the exact harness.

## Background

- [`BKL_VFS_CARVE_OUT.md`](../../archive/BKL_VFS_CARVE_OUT.md) — the VFS
  carve-out, worked in full: guard latching, IRQ-mask-per-attempt, the
  dropped-window ledger root-cause/fix, and the `unlinkat`/`openat`
  attribution + conversion sessions.
- [`BKL_PROCESS_CARVE_OUT.md`](../../archive/BKL_PROCESS_CARVE_OUT.md) — the
  Phase 3 audit of `clone`/`fork_process`/`execve`: why no carve-out was
  implemented (no inner lock on process/CoW state; the BKL is the lock), and
  what prerequisites would unblock one.
- [`BKL_MM_CARVE_OUT.md`](../../archive/BKL_MM_CARVE_OUT.md) — the Phase 5
  `mprotect`/`madvise`/`munmap`/`mremap`/`mmap` carve-out: the two real locking
  gaps it found and closed, and why (unlike the other three) it has no
  attribution-backed contention number.
- [`BKL_FINE_GRAINED_LOCKING_PLAN.md`](../../archive/BKL_FINE_GRAINED_LOCKING_PLAN.md)
  — the overall phased plan; §"Phase 2" has the network carve-out's design
  notes (why a coarse `NETWORK_LOCK` doesn't work, the SIGPIPE self-deadlock,
  the AB-BA nested-IRQ fix).
- [`SMP_SHARED.md`](../../archive/SMP_SHARED.md) — full shared-kernel SMP
  progress log (M0–M5).
- [`smp-shared.md`](smp-shared.md) — current-state milestone status for
  shared-kernel SMP.
- [`../../runbooks/debug-smp.md`](../../runbooks/debug-smp.md) and
  [`../../runbooks/debug-smp-fork-corruption.md`](../../runbooks/debug-smp-fork-corruption.md)
  — action-first procedures for BKL wedges and fork/CoW corruption.
