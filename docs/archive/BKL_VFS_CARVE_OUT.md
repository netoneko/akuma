# `no-bkl-vfs` — Carving VFS out of the Big Kernel Lock

Phase 4 of [BKL_FINE_GRAINED_LOCKING_PLAN.md](BKL_FINE_GRAINED_LOCKING_PLAN.md), implemented
2026-07-25 on branch `another-smp-attempt-0`. Mirrors the `no-bkl-network` carve-out
(Phase 2, that doc's §631) and reuses its hardening discipline.

**Status: Phase 1 (foundation) + Phase 2a (read-path syscalls) + Phase 3.1 (ext2 hardening)
shipped and verified at SMP=2. The §8 `[BKL] stuck` regression is ROOT-CAUSED AND FIXED
(§9: per-thread dropped-window ledger; 0 stuck in the re-run regimen). Phases 2b–2e
(mutating syscalls, `mmap` eager arm) NOT started. SMP=4 stress NOT run.**

---

## 1. What this actually is

The plan's Phase 4 sketch called for building a new VFS lock hierarchy
(`src/vfs/locks.rs`, per-filesystem and per-inode locks). **That is not what shipped, and
it isn't needed.** Every piece of state the VFS touches already carries its own
fine-grained lock, so the BKL is redundant for fs syscalls — exactly the situation net was
in. The carve-out is therefore:

1. An RAII guard (`VfsBklGuard`, `src/syscall/fs.rs`) that **drops** the BKL for the
   duration of an fs syscall and **re-acquires** it on every return path.
2. **Hardening** of the inner locks that the now-BKL-free window can hold, so a nested
   exception can never be taken while one is held.

No new locks were introduced.

### Pre-existing locks relied upon

| State | Lock | Location | Hardened by |
|---|---|---|---|
| fd table | `Spinlock` | `akuma-exec/src/process/fd.rs` | **already** — every accessor wraps its hold in `with_irqs_disabled` |
| ext2 superblock/BGD | `RwSpinlock` | `akuma-ext2/src/ext2.rs` | §3 below |
| ext2 block cache | `Spinlock` | `akuma-ext2/src/ext2.rs` | §3, transitively |
| virtio-blk device | `Spinlock` | `src/block.rs` | §3, transitively |
| mount table | `MOUNT_TABLE` | `src/vfs/mod.rs` | released before I/O in `with_fs` — no change |

---

## 2. Foundation

### 2.1 `PreemptGuard` lifted to `akuma-exec::sync`

It lived in `akuma-net/src/runtime.rs`, wired through `NetRuntime` callbacks so the net
crate could stay decoupled from `akuma-exec`. VFS runs far earlier in boot than the net
runtime registers, so the guard was moved to `akuma-exec::sync`, where it calls
`threading::disable_preemption` and `irq_save_mask` directly — no callback indirection.

- `irq_save_mask` / `irq_restore` promoted `pub(crate)` → `pub`.
- `akuma-net` gained an `akuma-exec` dependency and now re-exports the type, so existing
  `use crate::runtime::PreemptGuard` sites keep working unchanged.
- The dead `NetRuntime::disable_preemption` / `enable_preemption` fields were removed,
  along with their registrations in `src/main.rs` and `src/smp.rs`.
- No cycle: `akuma-exec → akuma-isolation → akuma-vfs`, and `akuma-vfs` is a leaf.

The IRQ-masking arm now fires under `any(no-bkl-network, no-bkl-vfs)`.

### 2.2 Feature wiring

`no-bkl-vfs` in the bin crate forwards to `akuma-exec`, `akuma-ext2`, and `akuma-net`;
`build.rs` emits `cfg(kernel_no_bkl_vfs)` from `CARGO_FEATURE_NO_BKL_VFS`. It is included
in `smp-shared` by default. Remove it from that feature list to A/B at compile time.

Cargo unifies features across the graph, so `akuma-ext2` does **not** need to forward
`smp-shared` — its `no-bkl-vfs` flag only gates whether ext2 *calls* the guard.

### 2.3 Runtime toggle

`smp_shared::vfs_bkl_drop_enabled()` / `set_vfs_bkl_drop_enabled()`, default **on**,
mirroring `FAULT_BKL_DROP_ENABLED` / `EXEC_BKL_DROP_ENABLED`. Allows A/B without a rebuild
and serves as a kill switch.

### 2.4 `VfsBklGuard` — the latching requirement

**This is the one non-obvious correctness constraint, and the plan got it wrong.**

`NetBklGuard` is gated purely on cfgs, so it cannot go out of balance. `VfsBklGuard`
consults a *runtime* toggle, and that toggle is genuinely flipped while guards are live
(the A/B boot self-tests flip it between phases; it is also a kill switch). A guard that
read the toggle in **both** `new()` and `drop()` would, on an ON→OFF flip mid-syscall,
drop the BKL and then decline to re-acquire it — and the syscall wrapper's single
`leave_kernel` would then advance `now_serving` for a ticket this core does not own,
corrupting the FIFO for every other core.

The guard therefore **latches** its decision in a `dropped_bkl` field at construction and
never re-reads the toggle. Host test:
`sync::tests::vfs_bkl_guard_latched_arm_stays_balanced_across_toggle_flip` replays both the
latched guard and the unlatched counter-example.

`VfsBklGuard::new_if(bool)` returns `Option<Self>` for the sites where the on-disk work
spans a whole function rather than one `match` arm.

---

## 3. Inner-lock hardening (ext2)

`Ext2ReadGuard` (previously a bare type alias for `RwLockReadGuard`) and `Ext2WriteGuard`
now each carry a `StateHoldGuard` — `akuma_exec::sync::PreemptGuard` under `no-bkl-vfs`, a
ZST otherwise. Field order is load-bearing: `inner` is declared **before** `hold`, so the
lock is released before preemption/IRQs are restored. The reverse order would reopen, for
an instant, exactly the window the guard exists to close.

### 3.1 IRQs are masked around the `try`, never the wait

The plan said to wrap `read_state`/`write_state` **acquisition** in `PreemptGuard`. That is
wrong and dangerous. Both are *unbounded* spin loops — they carry a 10,000-attempt
orphaned-write-lock recovery path precisely because a wait can be long. Masking local IRQs
across the wait would starve this core's timer for the whole contended window, and if the
current holder were a thread on this core, nothing could ever run to release it.

The shipped shape takes the guard **per attempt**, immediately before the non-blocking
`try_read()`/`try_write()`, and either keeps it (success — it now covers the hold) or drops
it before the backoff spin:

```rust
loop {
    let hold = state_hold_guard();
    if let Some(inner) = self.state.try_read() {
        return Ext2ReadGuard { inner, hold };
    }
    drop(hold);
    // ...orphan recovery + backoff spin, with IRQs live...
}
```

This gives the property actually needed — no nested exception *while holding* the lock —
with a bounded masked window.

### 3.2 `block_cache` and `BLOCK_DEVICE` need no guard of their own

The plan listed both as separate wrap targets. They aren't:

- All six `block_cache.lock()` sites are in functions taking `state: &Ext2State`, i.e. they
  are only reachable with a state guard already held.
- All block-device access funnels through the `crate::vfs::ext2` `BlockDevice` impl, reached
  only from `read_block`/`write_block`/`write_superblock` — all of which require a state
  guard. (`block::is_initialized` is a momentary probe from `fs::init` during
  single-threaded boot.)

Both are therefore covered transitively; a nested guard would only re-save an already-masked
DAIF. The invariant is documented at the `BLOCK_DEVICE` declaration in `src/block.rs`,
including what a future caller reaching it from outside an ext2 guard must do.

Note `BLOCK_DEVICE` is held across a full virtio round-trip — `read_sectors` busy-polls the
virtqueue and never yields — which is why stranding it matters.

---

## 4. Syscalls converted (Phase 2a — read paths only)

The guard is placed in the narrowest scope that covers the on-disk work, mirroring how
`sys_read`'s `Socket` arm takes `NetBklGuard` *inside* the arm.

| Syscall | Placement | Why |
|---|---|---|
| `sys_read` | inside `File` arm | the `Stdin` arm parks in `schedule_blocking` while holding non-IRQ-masked terminal-state locks — not audited |
| `sys_pread64` | inside `File` arm | |
| `sys_pwrite64` | inside `File` arm | |
| `sys_write` | whole fn, `new_if(is File)` | the `match` is *inside* the per-chunk loop, and the O_APPEND `file_size` probe already hits the VFS |
| `sys_lseek` | `new_if(is File)` | SEEK_END's `file_size` runs *inside* the `update_fd` closure, i.e. already under `with_irqs_disabled` |
| `sys_fstat` | inside `File` arm | every other arm synthesizes a `Stat` from constants |
| `sys_newfstatat` | after the cross-core forward arm | forwarding marshals through the BKL-protected bounce and must keep the lock |
| `sys_statx` | after path resolution | |
| `sys_getdents64` | after the `File` match | cache-miss path reads a whole directory |

Deliberately **not** converted: the `Socket` arms (already `NetBklGuard`), any non-`File`
fd kind, and every cross-core `RemoteFd` forwarding path.

---

## 5. Verification

### Boot self-test

`test_smp_shared_vfs_parallelism` (`src/process_tests.rs`, `cfg(kernel_smp_shared)`) has two
halves:

1. **Correctness (any core count, genuinely fails).** `READERS` threads plus the test thread
   hammer `fs::read_at` on one file, checksumming every read against a single-threaded
   baseline. Run once with the drop OFF and once ON, so a mismatch can be attributed to the
   carve-out rather than to a pre-existing ext2 bug. With the BKL dropped, the ext2 inner
   locks are the *only* thing serializing superblock/BGD state and the block cache, so a
   torn read surfaces here.
2. **Contention A/B (SMP=2 only, measurement).** Reports the `contention_spins` delta.

**SMP=2 result:** `reads OFF=96 ON=96 | bad OFF=0/0/0 ON=0/0/0 (mismatch/short/err)`.
Boot reaches `[SSH Server] Listening`, 0 PANIC.

The contention half measured `BKL-spins OFF=0 ON=0` — the block cache served every read, so
there is **no contention signal yet**. Getting one needs a working set larger than the
cache; see §7.

### Host tests

`crates/akuma-exec/src/sync.rs`: `vfs_bkl_guard_latched_arm_stays_balanced_across_toggle_flip`
and `preempt_guard_constructs_and_nests` (the latter pins the nesting contract —
`disable_preemption` is a per-thread *counter*, so an inner drop must not re-enable
preemption for an outer holder). Full host suite green.

### A gotcha worth remembering: thread-slot reclamation

The first version of the self-test passed but broke `smp_shared_cooperative_wait` two tests
later with `No available user threads for process execution`.

`spawn_user_thread_fn` draws from the **same** fixed pool `spawn_process_with_channel`
needs, and `mark_thread_terminated` only makes a slot *eligible* — the slot stays occupied
until a cleanup pass runs. In deferred mode (`DEFERRED_THREAD_CLEANUP = true`)
`cleanup_terminated_internal` returns early unless called **from thread 0** — and the boot
self-tests run *on* thread 0. So merely yielding in a wait loop starves the only thread that
could recycle anything. Measured: `pool: before=8 after=5 (waited 5000619us)` — the 5 s wait
timed out having reclaimed nothing.

Fix: call `threading::cleanup_terminated()` explicitly in the wait loop, which is what the
other thread-spawning tests in that file already do. After the fix:
`before=8 after=53 (waited 11185us)`.

That number is itself instructive: **one cleanup call reclaimed ~45 slots.** The apparent
"8 of 56 free" was not pool pressure — it was terminated slots sitting uncollected because
nothing had run the pass recently. Raising `MAX_THREADS` would not have helped.

### Builds

`default`, `size`, `extreme-size`, `release`, `release-smp-shared` all build; clippy clean
on default / `smp-shared` / host workspace.

**The plan claimed non-feature builds stay byte-for-byte identical. They do not.** Measured
on `release`: `.text` **shrank by 692 bytes** (0x2845f4 → 0x284340); `.rodata`, `.data`,
`.bss` byte-identical. The cause is changed inlining around the new ext2 guard wrapper, not
a behavior change — but the claim should not be repeated.

---

## 6. Known-pre-existing failures seen in these boots

Not caused by this work; recorded so they aren't re-attributed:

- `fs_error_to_errno_mapping FAILED: PermissionDenied -> EPERM — got -13 expected -1`. The
  mapping (`src/syscall/fs.rs`, `PermissionDenied => EACCES`) is unchanged vs `HEAD`; the
  test expects EPERM. A standing test-vs-code disagreement.
- `stp_xzr_ec15_handler_fires FAILED` — self-describes as a QEMU EC=0x25-instead-of-0x15
  artifact.
- ~15 `[BKL] stuck` + 1 `[BKL] RECOVERED`, all in the NEON/FP preemptive-scheduling test
  region, ~850 log lines *before* the VFS test. Known self-healing SMP spikes.

---

## 7. Remaining work

1. **Phase 2b** — `sys_openat`, `sys_close`, `sys_dup`, `sys_dup3`, `sys_fcntl`.
2. **Phase 2c** — the mutating syscalls (`mkdirat`, `unlinkat`, `renameat2`, `symlinkat`,
   `linkat`, `readlinkat`, `fchmodat`, `fchmod`, `truncate`, `ftruncate`, `fallocate`).
   These take the ext2 **write** guard, so they are the real test of §3. Fresh evidence
   this matters: a single `rm` of a 735 MB file held the BKL for ~40 s and produced 274
   consecutive `[BKL] stuck` warnings on the peer core (all self-healed, no data loss) —
   measured 2026-07-25 during the §9 validation, on a kernel whose read paths were clean.
3. **Phase 2d** — `chdir`, `fchdir`, `getcwd`, `fstatfs`.
4. **Phase 2e** — the eager file-backed `sys_mmap` arm (`src/syscall/mem.rs`), closing the
   asymmetry where the lazy fault path drops the BKL but the eager path does not. Needs an
   `as_lock` audit first. **Verification plan (per user, 2026-07-25):** the existing mmap
   stress test in `userspace/`, plus llama.cpp with mmap model loading as the end-to-end
   consumer check (model loads + serves — the same harness that validated the
   mmap-bigger-than-RAM and mmap_regions-race work).
5. **A contention signal.** The current A/B is uninformative because everything is
   cache-resident. Needs a working set exceeding the block cache.
6. **SMP=4 stress.** The failure mode this hardening targets (AB-BA under nested IRQ) is
   what net hit at SMP=4, not SMP=2. Until that runs, §3 is argued-correct, not
   demonstrated-correct. Note the §9 fix also WIDENS true window concurrency (windows now
   survive IRQs instead of silently re-serializing), so SMP=4 exercises §3 harder than any
   pre-fix run did.
7. ~~**`[BKL] stuck` regression (§8) — highest priority.**~~ **RESOLVED — root-caused and
   fixed, see §9.** The doubling/re-queue hypothesis in §8 was wrong; the real mechanism
   was IRQ-epilogue reconcile converting dropped windows into BKL-held runs.
8. **I/O regimen** — done for the read path (§8), re-run post-fix (§9.4); re-run after
   each of 2b–2e.

---

## 8. Combined net + VFS I/O regimen — RUN, 2026-07-25

Neither carve-out's self-test covers the *interaction*: a large download drops the BKL for
the socket path (`no-bkl-network`) and the file-write path (`no-bkl-vfs`) in the same
workload, interleaving ext2 guards with socket locks under real concurrency. This is the
highest-value validation available, and unlike the in-kernel shell it actually drives the
Phase 2a syscall guards — a real userspace process issuing `sys_read`/`sys_write`.

### Harness

**devbox-smoltcp** (`scripts/build_devbox_smoltcp.sh` + `overlays/devbox/run-smoltcp.sh`),
SMP=2, `devbox.img`, MEMORY=4096. Native smoltcp stack, built-in in-kernel SSH **dropped**
(`userspace-sshd`), so SSH is served by the userspace `/bin/sshd` from herd over a full
busybox userspace — `curl`, `sha256sum`, `cp`, `dd` all real processes.

> The `overlays/devbox-smoltcp/README.md` is **stale**: it describes an older variant using
> `--no-default-features` and the built-in in-kernel SSH. The Cargo `devbox-smoltcp` feature
> (`userspace-sshd` + `smp-shared`) and the two scripts above are current.

Host is `10.0.2.2` under QEMU SLIRP. Payloads are deterministic, non-compressible
(`AKUMA%07d` per 64 KiB block) so a torn write cannot hide in zero-fill. Drive `ssh` from
Python `subprocess` (the CLI is blocked by policy — see CLAUDE.md), and **ignore the exit
code**: this sshd never sends exit-status, so `ssh` always returns 255. Key on stdout.

### Results — shipping config (`no-bkl-vfs` + `no-bkl-network` ON, SMP=2)

| # | Check | Result |
|---|---|---|
| T1 | VM → host HTTP, small | 14 B, sha256 **exact** |
| T2 | VM → host **32 MiB → ext2 disk** | 33554432 B, sha256 **exact**, ~2.5–3.4 MB/s |
| T3a | VM → internet **HTTPS** (DNS+TLS) → disk | 35149 B, sha256 **exact** vs host fetch |
| T3b | VM → internet HTTPS **10.9 MB** + redirect → disk | 10967997 B, sha256 **exact** |
| T5 | in-VM `cp` of 32 MiB (ext2 read+write via syscalls) | sha256 **exact** |
| T6 | host ← VM httpd, 32 MiB egress, ×2 | 33554432 B, sha256 **exact** both, ~26–37 MB/s |
| W | 3× (8 MiB net→disk + disk→disk copy) | **6/6 digests exact** |

T2's digest also survived a **reboot** — re-checksummed intact on the next boot.

### T4 (`ssh root@vm cat /big.bin`) FAILS — pre-existing, not this carve-out

Pulling a large file through the sshd **exec channel** loses data non-deterministically:
7092224 / 6938624 / 6981632 bytes of 33554432, and `scp` dies with `Connection closed`. The
loss is not corruption at the right offset but a **positional shift** — at byte 175105 the
stream carried block ~11's content where block 2 belonged, i.e. chunks were *dropped*
mid-stream.

**Attribution (decisive):** rebuilt the identical image with `no-bkl-vfs` compiled **out**
(`no-bkl-network` still on) — it truncates the same way: 6000640 / 6117376 / 3656704 bytes,
also lossy. So the bug predates this work.

**Localized:** the same 32 MiB file served by the VM's **httpd** over the same smoltcp stack
transfers byte-exact, twice (T6). So the socket send path and the VFS read path are both
fine; the defect is specific to the **sshd exec-channel stdout bridge** dropping data under
a fast producer. Related to — but evidently not covered by — the interactive-shell drain fix
(a reaped child's stdout channel discarded before the bridge drained it).

*Not filed as part of this work.* Use httpd, not `ssh cat`/`scp`, for bulk egress until it
is fixed.

### ⚠ One real regression signal: `[BKL] stuck` appears only with the carve-out on

Controlled A/B — identical workload W, same image, same SMP=2, near-identical log volume:

| build | digests | `[BKL] stuck` | `RECOVERED` | new log lines |
|---|---|---|---|---|
| `no-bkl-vfs` **ON** | 6/6 ✅ | **8** | 15 | 456 |
| `no-bkl-vfs` **OUT** | 6/6 ✅ | **0** | 22 | 444 |

`[BKL] stuck` fires when a waiter spins `SPIN_WARN_THRESHOLD` = 10,000,000 times while the
lock is *genuinely owned* — tens of milliseconds of real hold, not a ticket anomaly (that is
what `RECOVERED` reports, and the baseline has *more* of those). All 8 were
`owner=2 waiter=1`; in a longer run the ratio was 26 `owner=2 waiter=1` to 8 the other way.

Likely mechanism: each guarded syscall does `leave_kernel` + `enter_kernel`, so it **doubles
BKL acquisitions**, and every re-acquire takes a *fresh ticket at the back of the FIFO*. A
core doing bulk fs I/O therefore repeatedly re-queues, and the peer observes long waits.

**Nothing lost data and nothing wedged** (`PANIC=0`, `WILD=0`, `SPURIOUS=0`, every anomaly
self-healed). But the SMP=4 hard wedge that `no-bkl-network` hit presented as exactly this —
`[BKL] stuck` with a frozen owner — so **this must be understood before SMP=4 stress, and
before Phase 2c puts the ext2 *write* guard on this path.** It is the top open item.

> **2026-07-25, same day:** resolved — see §9. The "doubles BKL acquisitions / fresh ticket
> at the back of the FIFO" mechanism above was **wrong**: the lock is a fair FIFO, so with
> two cores a 10M-spin wait cannot come from queueing — only from a genuine tens-of-ms
> *hold*. The real mechanism was found by reading the IRQ path, and it is worse and more
> interesting.

---

## 9. The `[BKL] stuck` regression: root cause and fix — 2026-07-25

### 9.1 Root cause: one IRQ converts a dropped window into a BKL-held run

The reconcile invariant is "BKL held iff EL1", enforced at every `eret`
(`reconcile_for_spsr`, called from the IRQ epilogues in `exceptions.rs`). A guard's dropped
window deliberately violates that invariant — and the violation did not survive the first
interrupt:

1. Core takes a guarded fs syscall, `VfsBklGuard` drops the BKL. So far so good.
2. A **timer IRQ** lands inside the window. The device-IRQ path does `enter_kernel()`
   unconditionally (the handler needs the BKL), and its epilogue reconciles to the
   *interrupted frame's* SPSR — which is **EL1** — so it **keeps the BKL held**.
3. The `eret` resumes the middle of the "BKL-free" window **with the BKL held**. Nothing
   ever notices: the guard's `drop()` re-acquire is the owner-reentrant no-op, and the
   syscall wrapper's `leave_kernel` releases at the end. Balanced, correct — and silently
   serialized.

At the regimen's measured ~2.5 MB/s ext2 write speed, one 64 KiB `sys_write` is a ~20 ms
syscall, so the 10 ms tick landed inside essentially **every** bulk-I/O syscall — each one
then held the BKL for its remainder. Baseline (`no-bkl-vfs` OUT) shows 0 stuck because its
holds get *chopped by preemption*: a tick that context-switches to an EL0 thread releases
the BKL at the eret. The converted windows resisted exactly that chop — the ext2
`PreemptGuard` holds (IRQs masked across virtio busy-polls, §3.1) keep deferring the tick —
so the holds ran long enough to cross the 10M-spin warning. The same conversion applied to
every dropper: `NetBklGuard` (converted on every blocking-recv wake), the execve ELF-read
drop, and the file-fault fill drop.

### 9.2 Fix: a per-thread dropped-window ledger, consulted at every eret

`akuma_exec::bkl` now keeps a **per-thread depth of open dropped-BKL windows**
(`DroppedWindowLedger`, host-tested; `MAX_THREADS` entries of plain atomics):

- All five dropper sites go through `bkl::dropped_window_open()` / `dropped_window_close()`
  instead of bare `leave_kernel`/`enter_kernel`: `VfsBklGuard`, `NetBklGuard`, the execve
  ELF-read drop (`syscall/proc.rs`), and both file-fault fill drops (`exceptions.rs`).
- `reconcile_for_spsr` (and `_no_ticket`) treat "target is EL1 **and** the resumed thread
  has an open window" as *release*: the eret restores the state the interrupted code chose.
  The check is thread-scoped (not core-scoped) because windows survive preemption, blocking
  waits, and cross-core migration; it reads the *incoming* thread, which is authoritative at
  the epilogue because `commit_switch` has already published it.
- `close()` re-acquires only for the **outermost** window, so a fault-drop nested inside a
  vfs window no longer converts the outer window on its way out (a pre-existing lesser form
  of the same bug).
- `idle_halt`'s post-WFI re-take is skipped inside a window (`blocking_relax` from a guarded
  net wait used to convert the window right there).
- Ordering is chosen so a race with an IRQ is benign in both directions: `open` publishes
  the depth *before* releasing; `close` decrements *before* re-acquiring.

Leak containment (a stale depth on a recycled thread slot would make unrelated EL1 code run
BKL-free — catastrophic): `return_to_kernel_from_fault` force-clears the ledger (the EL1
fault-kill path abandons the kernel stack, skipping guard destructors), and
`rust_sync_el0_handler` carries a tripwire — entry from EL0 with nonzero depth is healed and
logged (`[BKL] stale dropped-window depth …`), since no window can legally span an EL0
crossing.

Diagnostics: each preserved window logs with decaying frequency
(`[BKL] dropped window preserved across IRQ xN`, power-of-two sampled), and
`bkl::dropped_windows_preserved()` exposes the counter.

### 9.3 Tests

- **Host:** `bkl::tests::dropped_window_ledger_{nesting_and_isolation, unbalanced_close_saturates,
  reset_and_bounds}` pin the ledger contract. Full workspace suite green.
- **Boot self-test:** `test_smp_shared_dropped_window_survives_irq` (`src/process_tests.rs`)
  opens a window, yields through the scheduler and dwells >3 timer ticks, and asserts the
  BKL stays dropped (deterministically fails pre-fix), plus the nesting contract.
  SMP=2 result: `PASSED (18 eret(s) preserved the window)`. Full suite: 225 PASSED, the only
  failures the two known pre-existing ones (§6), 0 PANIC; the ~16 `[BKL] stuck` in the boot
  log are the known NEON/FP-test-region spikes (§6), unchanged.

### 9.4 Regimen re-run (same harness as §8) — fixed build, `no-bkl-vfs` ON, SMP=2

| check | result |
|---|---|
| W (3× 8 MiB net→disk + disk→disk copy) | **6/6 digests exact** |
| 32 MiB net→disk + 32 MiB `cp`, ×2 (fresh boot) | all **exact** |
| `[BKL] stuck` during W + 32 MiB transfers | **0** (was 8 on W alone) |
| `RECOVERED` | 8–13 (baseline band: 15–22) |
| PANIC / WILD / SPURIOUS | 0 |
| windows preserved across IRQs | >32,768 in one boot — each a conversion the pre-fix kernel suffered |
| throughput | 32 MiB ingress **3.9 MB/s** (pre-fix §8: ~2.5–3.4); in-VM 32 MiB `cp` ~6.5 MB/s r+w |

Two harness gotchas found while validating, so nobody re-burns the time:

- **`devbox.img` accumulates artifacts across sessions.** The 32 MiB test initially "failed"
  with truncation-at-22 MiB then instant 0-byte curls: the 1 GB image still carried a 735 MB
  Debian ISO from an earlier aria2 session, and the disk was simply FULL. `curl -s` swallows
  the ENOSPC (host server sees `BrokenPipeError`), and busybox `df /` can't resolve the
  mount point on this box — check with `ls -l /` before bulk-I/O tests.
- The 274-stuck storm during the cleanup `rm` of that ISO is the Phase 2c data point quoted
  in §7.2 — a real, reproducible motivation for guarding the mutating syscalls, and equally
  a warning that Phase 2c must land the §3 write-guard hardening first.
