# `no-bkl-vfs` — Carving VFS out of the Big Kernel Lock

Phase 4 of [BKL_FINE_GRAINED_LOCKING_PLAN.md](BKL_FINE_GRAINED_LOCKING_PLAN.md), implemented
2026-07-25 on branch `another-smp-attempt-0`. Mirrors the `no-bkl-network` carve-out
(Phase 2, that doc's §631) and reuses its hardening discipline.

**Status: Phase 1 (foundation) + Phase 2a (read-path syscalls) + Phase 2c-first-target (`unlinkat`)
+ Phase 2e (eager file `mmap`) + Phase 3.1 (ext2 hardening) shipped and verified at SMP=2. The §8
`[BKL] stuck` regression is ROOT-CAUSED AND FIXED (§9: per-thread dropped-window ledger; 0 stuck in
the re-run regimen). Phase 2c's `unlinkat` conversion is DONE (§12, 2026-07-30): the §11.6
attribution's 72.6% culprit dropped to **absent**, and the SMP=4 `[BKL] stuck` storm collapsed
598–704 → 0. Phase 2b first target `openat` is DONE, contention-confirmed by a controlled A/B
(§13.3, 2026-07-30, SMP=4, `bkl-profile`, identical net4+read4+cp2 workload): `openat`'s cumulative
cross-core BKL share cut **13.5% → 2.1%** (peak per-window 68.1% → 16.0%), total workload spinning
cut 2.4x, 0 stuck / 0 PANIC / 6-of-6 digests exact on both sides. Remaining
2b (`close`/`dup`/`fcntl`, rest of 2c/2d) NOT started — to be evidence-led off the next attribution.
The two pre-existing kernel bugs from §11 stand as before (thread-slot reclaim FIXED §11.7;
`wait`/SIGCHLD still open §11.3).**

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
   **First target `openat` DONE 2026-07-30 — see §13** (the §12.2 36.6% surfaced next target
   now runs BKL-free through `O_CREAT`/`O_TRUNC`/dirfd; a controlled SMP=4 A/B, §13.3, confirmed
   its cumulative cross-core BKL share cut 13.5% → 2.1%). The rest of 2b
   (`close`/`dup`/`dup3`/`fcntl`) is not started — to be evidence-led off the next attribution.
2. **Phase 2c** — the mutating syscalls (`mkdirat`, `unlinkat`, `renameat2`, `symlinkat`,
   `linkat`, `readlinkat`, `fchmodat`, `fchmod`, `truncate`, `ftruncate`, `fallocate`).
   These take the ext2 **write** guard, so they are the real test of §3. Fresh evidence
   this mattered: a single `rm` of a 735 MB file held the BKL for ~40 s and produced 274
   consecutive `[BKL] stuck` warnings on the peer core (all self-healed, no data loss) —
   measured 2026-07-25 during the §9 validation, on a kernel whose read paths were clean.
   **First target `unlinkat` DONE 2026-07-30 — see §12** (the §11.6 72.6% culprit dropped to
   *absent*; SMP=4 stuck 598–704 → 0). The remaining 2c list is now evidence-led: §12's
   attribution names `openat` (Phase 2b, 36.6%), not a 2c syscall, as the next-largest holder.
3. **Phase 2d** — `chdir`, `fchdir`, `getcwd`, `fstatfs`.
4. ~~**Phase 2e** — the eager file-backed `sys_mmap` arm.~~ **DONE — see §10.** Shipped as
   fill-before-install inside a `VfsBklGuard` window; verified with the
   `userspace/forktest/c_stress` mmap tools + llama.cpp mmap model loading end-to-end
   (which exposed and fixed the pre-existing `MADV_WILLNEED` zero-fill corruption, §10.3).
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

---

## 10. Phase 2e (eager file-backed `mmap`) + the llama.cpp end-to-end — 2026-07-25

### 10.1 What shipped

- **Eager file-backed fill restructured to fill-before-install** (`sys_mmap`,
  `src/syscall/mem.rs`): frames are filled from the file while still PRIVATE (unmapped,
  untracked) inside a `VfsBklGuard` window, then installed under `as_lock` with the final
  page flags. The old order (map as `RW_NO_EXEC` → fill → fix up flags) needed the BKL
  across the whole fill precisely because the pages were already visible to the process
  (a sibling could munmap the frames out from under the fill). Fill-before-install is the
  proven demand-fault Pass B shape; the `RW_NO_EXEC` + permission-fix-up dance disappears.
  In practice this arm serves writable `MAP_SHARED` mappings — `MMAP_FILE_BACKED_LAZY`
  routes read-only file mmaps to the demand-paged path on every profile.
- Both mmap-path `resolve_inode` calls (lazy-file arm + eager→lazy fallback) take the
  window for their on-disk metadata read.
- Boot suite SMP=2 green (225 PASSED, only the two §6 pre-existing failures, 0 PANIC),
  including `shared_file_mmap_writeback`, which drives the restructured path directly.

### 10.2 Verification per plan: userspace mmap stress + llama.cpp on devbox-smoltcp (SMP=2, 4 GB)

| check | result |
|---|---|
| `mmap_stress` (18 iters × 5 × 70 MB anon mmap/memset/munmap) | clean |
| `mmap_file` on the 508 MB qwen3.5-0.8B gguf (touch all ~130k pages) | clean, 16 s |
| `mmapsum` read vs mmap ×2 vs madvise-prefaulted vs 2-thread-concurrent | **all byte-exact** vs host reference (post-§10.3 fix) |
| `fpfault` (all 32 Q regs canaried across 130k demand faults; also 2 instances concurrently) | 0 corrupted |
| `neonfault` (130k page-crossing NEON loads into faulting pages) | 0 wrong |
| llama-server, model via `mmap = true`, chat completion | **coherent** (post-§10.3 fix) |

New tools live in `userspace/forktest/c_stress/` (see its README).

### 10.3 Bug found by the llama end-to-end: `madvise(MADV_WILLNEED)` destroyed file-backed lazy pages

llama with mmap produced garbage tokens (`$6;,0#%+-DB,HA/'`) while `--no-mmap` was clean —
**on the pre-Phase-2e baseline too** (controlled A/B), so pre-existing, not this work.
Content probes (mmapsum/fpfault/neonfault) were all clean; the discriminator was that
llama's loader calls `posix_madvise(WILLNEED)` over the whole mapping and no probe did.

Root cause: `sys_madvise(MADV_WILLNEED)` pre-faulted lazy pages by installing **zeroed**
frames, ignoring the region's `LazySource` — correct for anonymous regions (zero-fill IS
their fill), catastrophic for `LazySource::File`: the page is now "present", the demand
fault never runs, and the file content is permanently zeros. Fixed by pre-faulting ONLY
`LazySource::Zero` pages; file-backed pages are left to the (correct, readahead-optimized)
fault path. `mmapsum`'s `madv:` line is the standing regression check.

> The 6-month-old "llama mmap=true loads+serves at 4048 MB" validation predates this
> llama-server build; whether it verified token *quality* is unknown. On smp-shared it was
> reproducibly garbage until this fix.

Same-family latent bug, documented not fixed: `MADV_DONTNEED` zeroes mapped pages
regardless of backing — correct-ish for anon, wrong for file-backed `MAP_PRIVATE`
(POSIX: dropping the private copy must re-expose FILE content on next touch, not zeros).

### 10.4 Pre-existing llama-on-smp-shared issues found on the way (not fixed)

llama.cpp had never run on the shared-SMP kernel before this validation. With default
arguments it dies before serving:

1. **Default 262k context → ~3 GiB anon KV cache → SIGSEGV**: the demand fault at
   VA ≈ 0x2_36c0e000 (inside the mmap'd KV region, ~9.5 GB) is rejected — smells like a
   user-VA-limit check rather than genuine OOM (3.5 GB PMM free at the time).
   `-c 4096` avoids it.
2. **`clone_thread` first-touch SIGSEGV** (fresh thread faults just above `SP_EL0`,
   `WILD-DA`, `last_sc=MAX`) — same family as the fixed extreme-profile spawn bug, now
   seen on smp-shared under llama's thread spawn.
3. **One (unreproduced) `[BKL] stuck` storm**: after ~20 min of 2-thread no-mmap
   generation following several crashed llama instances, 7.8k consecutive
   `owner=2 waiter=1` warnings — core 0 starved (sshd unresponsive) while heartbeats
   continued; no panic, no data loss. Not seen again across several longer runs on the
   same build (nor with the BKL profiler enabled), and never on baseline (which however
   got fewer opportunities). The dropped-window ledger is the prime suspect *class*;
   `[BKL] stuck` lines now print the holder's profiler tag to attribute the next
   occurrence. **Watch item for SMP=4 stress.**
4. The userspace sshd kills exec channels under load (ssh `ServerAlive` keepalives time
   out when llama pegs both cores → channel teardown kills the session's children).
   Workaround: `nohup … &` for long-running commands; known sshd instability family.

### 10.5 Ledger tripwire proven in anger

The first crashed llama leaked one open dropped-BKL window (destructor-skipping kill
path); the §9 EL0-entry tripwire healed it at slot reuse exactly as designed
(`[BKL] stale dropped-window depth 1 healed at EL0 entry (tid=14)`).

---

## 11. SMP=4 stress campaign + BKL-hold attribution — 2026-07-29 (IN PROGRESS)

The two open items from §7 — "a contention signal" (§7.5) and "SMP=4 stress" (§7.6) — are the
gate on everything downstream: until they run, §3's hardening is argued-correct, not
demonstrated-correct, and there is no evidence that carving *more* subsystems out of the BKL
is where the remaining contention actually is. This section is the campaign that runs both.

**Section status: COMPLETE. Stress run at SMP=4 (§11.2) — no corruption, no wedge, but
hundreds of `[BKL] stuck` where SMP=2 produced none. Attribution collected (§11.6) — and it
names a single culprit: `unlinkat`, at 72.6% of all cross-core BKL wait. Two pre-existing
kernel bugs were found on the way; the thread-slot one is FIXED (§11.7), the missing SIGCHLD
is not (§11.3).**

### 11.1 Harness

Same shape as §8 (devbox-smoltcp, userspace sshd over a full busybox userspace, deterministic
non-compressible payload from a host HTTP server on `10.0.2.2`), with three changes:

- **SMP=4**, MEMORY=4096.
- **`devbox.img` grown 1 GB → 4 GB.** The §9.4 gotcha (image silently full, ENOSPC
  masquerading as a network bug) cost real debugging time; 4 GB removes it. Grown in place on
  the host with Homebrew e2fsprogs — `e2fsck -fp && truncate -s 4G && resize2fs && e2fsck -fp`
  — which preserves the existing rootfs. Akuma's ext2 driver reads and writes the 32-block-group
  image without complaint. **Nothing else may have the image open while this runs**; a stale
  QEMU from an earlier session holds a write lock on it (and would be writing through a stale
  1 GB superblock).
- The workload is driven from a **script fetched into the VM and run detached** (`nohup sh
  /tmp/job.sh &`), polled over short-lived ssh connections. Detached because this sshd tears
  down exec channels when the cores are pegged; `sh /tmp/job.sh` rather than the shebang
  because busybox resolves a bare executable path with no recognised interpreter as an *applet
  name* and fails with `applet not found`.

Phases, each driving a different BKL consumer: `net4` (4 concurrent 32–64 MiB downloads → net
syscalls + ext2 write), `read4` (4 concurrent `sha256sum` → ext2 read with a working set far
larger than the 64-slot block cache, which is what §7.5 asked for), `cp2` (read+write in one
process), `rm` (mutating syscalls — the Phase 2c motivation).

### 11.2 Stress result: SMP=4 reproduces `[BKL] stuck` in bulk, with no corruption

Two runs of the shipping config (`no-bkl-vfs` + `no-bkl-network`, `release-smp-shared`,
`devbox-smoltcp,no-tests`), each moving 256 MiB through 4 concurrent `curl`s:

| signal | run 1 | run 2 | SMP=2 (§9.4) |
|---|---|---|---|
| `[BKL] stuck` | **704** | **598** | 0 |
| `RECOVERED` | 39 | 12 | 8–13 |
| PANIC / WILD / SPURIOUS | 0 / 0 / 0 | 0 / 0 / 0 | 0 / 0 / 0 |
| stale dropped-window heals | 0 | 0 | — |
| download integrity | 4/4 complete, `rc=0` | 4/4 complete, `rc=0` | — |
| **digests** (4 net + 2 in-VM copies) | — (run cut short) | **6/6 exact** | 6/6 exact |

Run 2's digest verification is the correctness half: 4 × 64 MiB downloaded concurrently over
the BKL-free net path onto ext2, then re-read (`sha256sum`, 4 processes, 256 MiB — far past the
64-slot block cache) and copied in-VM, all byte-exact against the host reference. So SMP=4
concurrency does **not** corrupt the carve-out's read, write, or copy paths.

Throughput datapoint for the read phase: the 4 concurrent `sha256sum`s over 256 MiB joined in
15 s (~17 MB/s aggregate), i.e. the cache-missing read path is healthy even while producing
hundreds of stuck warnings.

So the §9 ledger fix did **not** eliminate long BKL holds — it eliminated them *at SMP=2*. At
SMP=4 the same workload produces hundreds of >10M-spin waits. Every one self-healed: no panic,
no wedge, no wild data abort, no truncated transfer. All lines report `tag=511` (unknown)
because the per-tag profiler is off in a shipping build — attributing them is §11.5.

Two independent points worth keeping: (a) the failure mode §3's hardening targets (AB-BA under
a nested IRQ inside a dropped window) did **not** occur — 0 wedges across both runs; (b) long
holds at SMP=4 are nevertheless routine, so Phase 2c (the ext2 *write* guard) must not land
until they are attributed.

### 11.3 Blocker found: the shell's `wait` builtin never returns

`sh -c "sleep 1 & wait; echo OK"` **hangs forever** — at SMP=1 and SMP=4, on an otherwise idle
VM. The child runs, exits, and disappears from `ps`; the parent sits in `wait` indefinitely.

Not a regression from either carve-out (it reproduces at SMP=1, and the mechanism below has
never existed): **the kernel delivers no SIGCHLD.** `grep -r SIGCHLD src/ crates/` finds only
clone-flag *parsing* — no code path ever raises the signal on a child's exit. Foreground waits
are unaffected, because they go through a blocking `wait4` while the child is still alive
(that is how every `ssh <cmd>` returns); it is specifically the background-job path that
depends on the signal.

Impact well beyond this campaign: `&` + `wait` is the standard way to express parallelism in
shell, so *no* parallel shell workload can synchronize — which is exactly what SMP testing
needs. The harness works around it by having each worker touch a sentinel file and having the
parent poll for it.

### 11.4 Blocker found: thread-slot reclamation starves under load

Under sustained load `fork` stalls for **minutes**, then fails outright: userspace sees
`can't fork: Out of memory` and the kernel logs `[sys_spawn] … No available user threads`
while **3.4 GB of RAM is free** (`pmm=905586free/1048576tot`). Measured reclaim latency in one
run: **p50 24 s, p90 176 s, max 192 s** — against a configured
`THREAD_CLEANUP_COOLDOWN_US` of **10 ms** (`src/config.rs:591`). The cooldown is not the
problem; the pass simply is not running.

Mechanism, all of it in `cleanup_terminated_internal`
(`crates/akuma-exec/src/threading/mod.rs:981`) and its callers:

1. With `DEFERRED_THREAD_CLEANUP = true` the pass returns early unless the caller **is thread
   0** — so no other thread can ever reclaim a slot, including the thread that just failed to
   find one.
2. The only steady-state caller is thread 0's **idle** loop (`src/main.rs:1137`, every 10
   iterations). That loop runs only when nothing else is runnable — precisely never under the
   load that exhausts the pool. The async-main poll loop, which *does* run on the BSP under
   load, never calls it.
3. So slot exhaustion has no recovery path: `spawn` fails and returns an error instead of
   running a pass and retrying.

`MAX_THREADS` is 64 (`crates/akuma-exec/src/threading/types.rs:11`), so this bites after a few
dozen short-lived processes — which a shell script reaches in seconds. This is very likely the
root of the standing "SMP=4 testing is blocked by pool exhaustion" note and of the §5 gotcha
("one cleanup call reclaimed ~45 slots… raising MAX_THREADS would not have helped").

Remedy directions, cheapest first — none implemented yet:
- **Reclaim on demand.** `spawn_user_thread_fn_internal`
  (`crates/akuma-exec/src/threading/mod.rs:3326`) turns a `claim_free_slot` miss straight into
  `Err("No free user thread slots")` without ever attempting a pass. Run a cleanup pass and
  retry once there (and at the sibling site `:3189`). The pass is already race-safe against
  concurrent spawns: it CASes `TERMINATED → INITIALIZING`, which blocks a spawn from claiming
  the slot mid-pass.
- **Reclaim from the async-main loop**, not just the idle loop, so a busy BSP still collects.
- **Relax the thread-0 restriction.** The `INITIALIZING` interlock is what actually provides
  safety against a concurrent *spawn*; "only thread 0" looks like a conservative leftover. Two
  things must survive the relaxation: the cooldown (which guards against recycling a slot whose
  thread has not yet left its kernel stack), and a "not currently running" check. The latter is
  cheap and already per-core — `get_current_thread_register()` reads `TPIDRRO_EL0`
  (`threading/mod.rs:593`), so under shared-kernel SMP a reclaimer must confirm the candidate
  slot is not the running thread on *any* core, not just its own. Reclaiming from a
  non-thread-0 core without that check is the one way this could get worse instead of better.

### 11.5 Attribution build (`bkl-profile`) — NEW, result pending

`akuma_exec::sync` has carried a per-tag BKL-hold profiler for a while (a waiter samples what
the *owning* core is doing when it first observes contention, and credits its spins to that
tag on acquiring), but its only consumer was a boot self-test — so it had never been read under
a real userspace workload, and the Phase 0 claim that the scheduler/IRQ path holds ~70% of
contended time remained an estimate.

New `bkl-profile` cargo feature → `cfg(kernel_bkl_profile)` (`build.rs`), plus
`src/bkl_profile.rs`: turns the profiler on for the whole boot and prints a **delta** histogram
every 10 s from the async-main loop —

```
[BKLPROF] w12 t=340s spins=1843221 attributed=1840012 windows_preserved=4211
[BKLPROF]   irq/sched tag=501 44.1% spins=811... 
[BKLPROF]   write tag=64 31.7% spins=583...
```

Deltas, not totals, so a window is attributable to the workload that ran during it rather than
to boot noise. Measurement-only: with profiling on, every kernel entry stores to a shared
per-core tag line, which perturbs timing — hence the explicit feature rather than inclusion in
`smp-shared`. Build:

```
cargo build --profile release-smp-shared --features devbox-smoltcp,no-tests,bkl-profile
```

This is what turns §11.2's 700 anonymous `tag=511` stuck episodes into an answer about which
subsystem to carve out next — and therefore whether Phase 2b/2c/2d are worth doing before
Phase 3 (process management), which the Phase 0 estimate says is the bigger lever.

### 11.6 RESULT: `unlinkat` is 72.6% of all cross-core BKL wait

Full regimen, SMP=4, `bkl-profile` build, 16.85 billion attributed spin-iterations:

| share | holder tag | what it is |
|---|---|---|
| **72.6%** | `unlinkat` (35) | **file deletion — a Phase 2c mutating syscall** |
| **26.9%** | `irq/sched` (501) | the scheduler / IRQ path — Phase 3's target |
| 0.3% | `openat` (56) | Phase 2b |
| <0.2% total | everything else | `read`, `write`, `clone`, `mmap`, `ppoll`, … |

The `[BKL] stuck` lines agree independently: of 1583 tagged episodes, **1156 were `tag=35`**
and 427 `tag=501` — nothing else appears at all.

Three conclusions, in order of how much they change the plan:

1. **Phase 2a worked, and the read path is done.** `read` and `write` — the syscalls this
   carve-out converted — together account for well under 0.1% of contended wait. There is no
   remaining contention to win on the read path, at any core count.
2. **Phase 2c is the whole game, and it is narrower than "the mutating syscalls".** It is
   specifically `unlinkat`. §7.2 already flagged it anecdotally (one `rm` of a 735 MB file =
   ~40 s hold, 274 stuck warns) and §9.4 filed that as a large-file curiosity. It is not: in
   this regimen the `rm` phase deletes 192 MB in a workload that also moves 128 MB over the
   network, re-reads 128 MB past the block cache, and copies 64 MB — and the deletes still
   outweigh *everything else combined* by nearly 3:1. Converting `unlinkat` alone should
   recover the large majority of the available win; the rest of the 2c list can follow on
   evidence rather than on principle.

   **Confirmed — §12 (2026-07-30):** converting `unlinkat` alone dropped it from 72.6% to
   *absent* and took the SMP=4 `[BKL] stuck` count from 598–704 to 0. The prediction
   under-recovers slightly: the win this workload could surface was recovered *entirely* by
   this one conversion.
3. **The Phase 0 "scheduler/IRQ holds ~70% of contended time" estimate is wrong for this
   workload — it is 27%.** Still the second-largest consumer, and still what Phase 3 targets,
   so Phase 3 keeps its place in the plan. But it is not the reason this workload contends,
   and "do Phase 3 before finishing Phase 2" is not supported.

**Caveats, stated so the number is not over-read.** The profiler perturbs timing (that is why
it is a separate feature). This is one workload, and it deliberately ends with a bulk delete —
a workload that never unlinks would obviously not attribute 72.6% there. What the ratio does
establish is the *rate*: a handful of `unlinkat` calls out-contend hundreds of megabytes of
read, write, and socket traffic, which is a statement about `unlinkat`'s hold length, not
about how often the regimen calls it. The run also carried the §11.4 reclaim fix, so it had
genuinely more cross-core concurrency than the §11.2 runs (whole regimen: 152 s vs. >1800 s
and unfinished) — which raises absolute stuck counts (1594) without changing the shares.

Correctness held throughout: **6/6 digests exact** on this build too.

### 11.7 Thread-slot reclaim: FIXED

The §11.4 starvation is fixed, and the effect on this campaign is not subtle — **the identical
regimen went from "did not finish in 1800 s" to "finished in 152 s."**

- `crates/akuma-exec/src/threading/mod.rs`: `cleanup_terminated_internal` now takes its two
  gates independently (`any_caller`, `ignore_cooldown`) instead of one `force` flag that
  dropped both. New `reclaim_terminated_slots()` = drop the caller gate, **keep the cooldown**.
  The cooldown is the property that actually matters (a thread marks itself TERMINATED while
  still on its kernel stack); the caller-identity gate never provided safety — the
  `TERMINATED → INITIALIZING` CAS does, and it serializes two concurrent reclaimers exactly as
  it serializes a reclaimer against a spawn.
- Both spawn paths (`spawn_user_thread_fn_internal`, `spawn_system_thread_fn`) now collect and
  retry once on a `claim_free_slot` miss instead of returning "No free … slots" outright. Slot
  exhaustion is a recoverable condition, not an error to hand to userspace as ENOMEM.
- `src/main.rs`: the async-main loop reclaims every 100 ms. This is the steady-state collector
  that was missing — it runs on a system thread (not thread 0) and keeps running under load,
  where thread 0's idle loop by definition does not.

Boot self-test `test_thread_slot_reclaim_on_spawn` (`src/process_tests.rs`) drives the exact
failing shape — fill the pool, terminate everything, let the cooldown elapse, spawn again with
no explicit cleanup — and additionally pins that a slot **inside** its cooldown is not
recycled, and that the gated `cleanup_terminated()` still declines from a non-thread-0 caller
(asserted from a spawned thread, since boot tests themselves run on thread 0). SMP=2 suite:
`PASSED (filled 56 slots, hot-reclaim 0, respawn ok)`, 230 passed, 0 panics, the only 2
failures the two known pre-existing ones (§6).

`No available user threads` occurrences during the post-fix regimen: **0**.

Post-fix run on the **shipping** config (no profiler, `devbox-smoltcp,no-tests`, SMP=4, the
lean 4 × 32 MiB regimen): completed in **136 s**, **6/6 digests exact**, 455 `[BKL] stuck`,
30 `RECOVERED`, 0 PANIC / WILD / SPURIOUS, 0 pool exhaustion. So the fix changes throughput and
removes the ENOMEM failure mode without disturbing the stability picture — the stuck episodes
are `unlinkat`'s, and they wait for Phase 2c.

`e2fsck -fp devbox.img` after the whole campaign (three SMP=4 runs, ~1 GB written and deleted):
clean. The only complaint is the standing cosmetic one — `HTREE … invalid root node / HTREE
INDEX CLEARED` on `/tmp` — which is present before the campaign too, because Akuma's ext2
writes don't maintain htree indexes.

### 11.8 Still open

- **`wait` still hangs** (§11.3). Unfixed — it needs SIGCHLD delivery, which is a larger piece
  of work than the reclaim fix and touches the signal path rather than the scheduler.
- ~~**Phase 2b first target `openat` — CONVERTED, regimen re-run pending.** §12.2's attribution
  named `openat` (`tag=56`, 36.6%) the next-largest holder after `unlinkat` vanished; §13
  (2026-07-30) wraps its on-disk work in a `VfsBklGuard` window and boot-verifies it.~~
  **DONE — see §13.3 (2026-07-30).** A controlled A/B (same net4+read4+cp2 workload, SMP=4,
  `bkl-profile`, `openat` guard on vs. reverted) confirmed the share cut, not just moved:
  cumulative cross-core BKL share 13.5% → 2.1%, peak per-window 68.1% → 16.0%, total workload
  spinning cut 2.4x. `openat` drops from the #2 holder (behind `irq/sched`) to a minor one.
- ~~**Phase 2c**, first target `unlinkat`. Land the §3 write-guard hardening with it, and
  re-run this regimen to confirm the 72.6% moves.~~ **DONE — see §12 (2026-07-30).** The 72.6%
  did not just move, it vanished; the SMP=4 `[BKL] stuck` storm went with it. The remaining 2c
  list is now evidence-led — §12's attribution names `openat` (Phase 2b, 36.6%) as the
  next-largest holder, not a 2c syscall.
- ~~**The stuck episodes themselves.** Attributed, but not eliminated: the fix for them *is*
  Phase 2c, since that is where they live.~~ **Resolved by the `unlinkat` conversion (§12):**
  the ~600–700/run that were almost all `tag=35` are now 0.

---

## 12. Phase 2c first target (`unlinkat`) — DONE, 2026-07-30

§11.6 named a single culprit — `unlinkat` (syscall 35, `tag=35`), 72.6% of all cross-core BKL
wait — and §11.8 made it Phase 2c's first target. Converted; re-ran the identical §11.1 regimen
to confirm the share moves. It did not move — it vanished.

### 12.1 The change

`sys_unlinkat` (`src/syscall/fs.rs`) now takes a `VfsBklGuard` window across its on-disk work,
mirroring the Phase 2a read-path placement (§4): the user-string copy and the dirfd base-path
lookup stay outside the window (early `EBADF` returns must not pay for a BKL drop), and the
window covers the directory walk (`canonicalize_path`/`resolve_path`) plus the deletion itself
(`remove_dir`/`remove_file`/`remove_symlink`). No new locks — the ext2 **write** guard the
deletion takes is the §3-hardened one. Zero-cost no-op off `smp-shared` + `no-bkl-vfs`.

### 12.2 Result — same §11.1 regimen, `bkl-profile` build, SMP=4, 4 GB

| signal | §11 pre-fix | §12 post-fix |
|---|---|---|
| `[BKL] stuck` | 598–704 (§11.2) / 1156 `tag=35` episodes (§11.6) | **0** |
| `unlinkat` `tag=35` share of attributed spins | **72.6%** | **absent** (not in top 12) |
| PANIC / WILD / SPURIOUS | 0 / 0 / 0 | 0 / 0 / 0 |
| pool exhaustion (`No available user threads`) | 0 (post §11.7) | 0 |
| digests (4 net + 2 in-VM copy) | 6/6 exact | **6/6 exact** |
| regimen wall-clock | 136 s (§11.7 lean) | 136 s |

Post-fix cumulative attributed spins (185.9 M total, same profiler): `irq/sched` `tag=501`
 53.9%, `openat` `tag=56` **36.6%**, everything else <2%. `openat` is the surfaced next target —
**Phase 2b**, not 2c; the rest of the 2c list is now evidence-led rather than principle-led.
**DONE, contention-confirmed by A/B — see §13.3 (2026-07-30):** cumulative share cut 13.5% → 2.1%.

### 12.3 Correctness was verified, not assumed

`rm -f` swallows errors, so a *broken* `unlinkat` (early error return, no I/O) would also show
no BKL contention — indistinguishable from "works and is fast" via profiling alone. Ruled out
directly: downloaded `p64.bin` (64 MiB, sha matched the reference) → `rm` → `ls` reports
"No such file or directory"; `mkdir -p`/`rm -rf` exercised the `AT_REMOVEDIR` (`remove_dir`)
path the same way → gone. Both deletion paths remove real files, and the 64 MiB delete produced
0 stuck — the class of hold §7.2 measured at ~40 s pre-fix.

### 12.4 `e2fsck` finding: pre-existing durability bug, NOT a carve-out regression

Post-campaign `e2fsck -fp devbox.img` flagged two directory inodes as *"in use, but has dtime
set"* + *"zero-length directory"* (beyond the standing cosmetic HTREE warning). These were the
`/tmp/delme` + `/tmp/delme/sub` the verify script `rm -rf`'d. Looked serious, so it was
attributed decisively before being filed:

- `remove_dir` (`crates/akuma-ext2/src/ext2.rs:2162`) sets `inode.deletion_time` + `write_inode`
  and then `free_inode` (clears the bitmap bit, `:1105`) **all under one `write_state` guard** —
  the ext2 write lock §3 hardened serializes them, so a concurrency tear between the dtime write
  and the bitmap clear is not possible from dropping the BKL.
- **Decisive test: reproduced identically at SMP=1** (one core, zero concurrency, where the
  `VfsBklGuard`'s BKL-drop is a correctness no-op). Same two directory inodes, same pattern.

The real cause: **Akuma's ext2 block cache is write-back with no sync-on-shutdown.** When QEMU
is raw-`kill`'d, the inode-record write (dtime set) has landed on disk but the inode-bitmap
clear is still dirty in the 64-slot cache and is discarded. §11.7 reported "clean" because those
runs did more I/O after deleting (evicting/flushing the dirty bitmap blocks through the cache);
here the deletion was the literal last op before kill. Pre-existing, SMP-independent, and
unaffected by whether the BKL is held around `remove_dir`. Filed as a separate item; the image
`e2fsck -fp`'d clean afterward (read-only `-n` check passes, exit 0).

### 12.5 Caveat: profiler perturbation, and what the number establishes

The `bkl-profile` feature perturbs timing (§11.5), so absolute spin counts are not comparable
across sessions. The two decisive, profiler-independent signals: the `[BKL] stuck` *count*
(0 vs §11.2's 598–704 — a threshold counter present in every build), and `unlinkat`'s
*presence* in the attribution (absent, under the same profiler on the same workload where it was
72.6%). §11.6's prediction — *"converting `unlinkat` alone should recover the large majority of
the available win"* — is confirmed; for this workload it recovered all of it.

---

## 13. Phase 2b first target (`openat`) — DONE, contention-confirmed, 2026-07-30

§12.2's attribution named `openat` (syscall 56, `tag=56`) the next-largest holder once `unlinkat`
vanished — 36.6% of attributed cross-core BKL wait, second only to `irq/sched`. §11.8 made it
Phase 2b's first target. Converted, boot-verified (§13.2), and then contention-confirmed by a
controlled A/B (§13.3): running the identical net4+read4+cp2 workload at SMP=4 under `bkl-profile`
with the guard on vs. reverted cuts `openat`'s cumulative cross-core BKL share **13.5% → 2.1%**
(peak per-window 68.1% → 16.0%), and total workload spinning 2.4x, with 0 stuck / 0 PANIC and
6-of-6 exact digests on both sides.

### 13.1 The change

`sys_openat` (`src/syscall/fs.rs`) now takes a `VfsBklGuard` window across its on-disk work,
mirroring the placement §4 established for the path-based read syscalls and §12 used for
`unlinkat`. The window opens **after** the cross-core forward arm — exactly like `sys_newfstatat`
(§4) — because that arm marshals through the BKL-protected bounce and must keep the lock. It covers
the existence probes, `O_CREAT` create / `O_TRUNC` truncate (both `crate::fs::write_file`; truncating
a large file frees its blocks under the ext2 write guard, the same class of long hold §7.2 measured
at ~40 s for an `unlink`), `chmod`, and the fd allocation.

Deliberately **outside** the window (i.e. still BKL-held): the user-string copy, the dirfd base-path
lookup (fd-table only, no disk I/O — early `EBADF` returns must not pay for a BKL drop), the
`/dev/null`/`/dev/zero`/`/dev/urandom`/`/dev/dsp`/`/dev/net/tap0` device-node fast paths (allocate
a synthetic fd, do no ext2 I/O), `/proc/self/exe`, and `resolve_symlinks` (runs before the device
checks; for the regimen's simple absolute paths it touches no symlinks, and reordering it would
change the `/proc/self/exe` resolution order). The forward arm and an active guard never coexist in
one binary anyway — `build.rs` asserts `kernel_smp` (multikernel, where the forward arm compiles)
and `kernel_smp_shared` (where the guard's body is non-empty) are mutually exclusive — but the
"forward arm outside the window" placement keeps the §4 invariant consistent under any future cfg
combination. Zero-cost no-op off `smp-shared` + `no-bkl-vfs`.

### 13.2 Verification — boot self-test (correctness)

New `test_openat` (`src/process_tests.rs`), structurally identical to §12's `test_unlinkat`: drives
the real entry point via `handle_syscall(OPENAT, …)`, pinning what the conversion must preserve —

| # | case | pins |
|---|---|---|
| 1 | `O_CREAT\|O_WRONLY` on absent file | file appears, empty — create happens inside the window |
| 2 | `O_TRUNC\|O_WRONLY` on non-empty file | file emptied — the long truncate hold runs BKL-free |
| 3 | dirfd-relative (`fd 7 → …/sub`) | openat must NOT ignore dirfd (the rm-recursion family) |
| 4 | `AT_FDCWD`-relative (`cwd=/tmp`) | cwd-relative resolution |
| 5 | `/dev/null` (`O_RDWR`) | device fast path still allocates a usable fd, pre-window |
| 6 | dirfd `999` (unopen) | `EBADF` early-return path stays balanced |
| 7 | missing target, no `O_CREAT` | `ENOENT` error path *through* the dropped window |

**SMP=2 boot, `smp-shared` + `no-bkl-vfs`:** `[Test] openat PASSED (7 cases)`. Full suite 221 PASSED,
the only 2 failures the standing §6 pre-existing ones, 0 PANIC / WILD / stale dropped-window heals,
0 `[BKL] stuck` lines attributed to `tag=56`. Default / `release-smp-shared` / `bkl-profile` builds
all build; clippy clean; host workspace suite green.

A note on what the test deliberately does **not** assert: a negative non-`AT_FDCWD` dirfd. `sys_openat`
(unlike `sys_unlinkat:2227`, which returns `EBADF`) falls through to base `"/"` (`fs.rs:1237`) — i.e.
it resolves the path against root rather than rejecting the dirfd. That is a pre-existing divergence
from POSIX/unlinkat, it runs before the guard opens, and it is out of scope for this carve-out (which
preserves behavior, not changes it). Filed here so it is not re-discovered as a regression.

### 13.3 Verification — controlled A/B (contention), SMP=4, `bkl-profile`, 2026-07-30

§12 marked `unlinkat` DONE only because the §11.6 regimen confirmed the 72.6% dropped to *absent*
and the stuck storm went 598–704 → 0. `openat` needed the same *contention* half, not just the
*correctness* half §13.2 gave it. Unlike §12.2 (which compared two independent regimen runs before
and after the conversion landed), this is a same-binary A/B: the identical net4+read4+cp2 workload
(4 concurrent downloads driving `openat` `O_CREAT`/`O_TRUNC`, a re-sha read pass, and a 2-file
in-VM copy — a smaller, faster variant of the §11.1 regimen) run twice on the same `bkl-profile`
build with only the `openat` guard
toggled — once with the §13.1 `VfsBklGuard` window in place ("ON"), once with it reverted to
BKL-held ("OFF") — with the guard restored to ON afterward for this landing.

| signal | ON (converted) | OFF (BKL-held) |
|---|---|---|
| `openat` `tag=56` cumulative share | **2.1%** | **13.5%** |
| `openat` `tag=56` peak per-window share | 16.0% | 68.1% |
| workload cumulative attributed spins | 152,786,715 | 364,902,792 (2.4x) |
| `[BKL] stuck` during workload | 0 | 0 |
| PANIC / WILD | 0 / 0 | 0 / 0 |
| digests (4 net + 2 cp) | 6/6 exact | 6/6 exact |

Top holders shift accordingly: OFF is `irq/sched` 65.6%, `openat` 13.5%, `execve` 8.5%, `nr301`
4.2%; ON is `irq/sched` 43.4%, `execve` 32.1%, `nr301` 11.1%, `clone` 4.2%, `read` 3.9%, `openat`
2.1%. `openat` moves from the #2 cross-core holder (behind only `irq/sched`) to a minor one behind
`read`, matching §12.2's expectation and confirming its prediction from the 36.6%-in-isolation
figure: most of that share really was `openat`'s own hold, not measurement noise from the
`unlinkat` conversion settling. `openat`'s holds were sub-threshold even BKL-held (0 stuck on both
sides), so the `[BKLPROF]` attribution — not the `[BKL] stuck` counter — is the discriminator here,
same as it was for the correctness/contention split in §12.5.

This closes the "argued-correct, not contention-demonstrated" gap this section previously flagged
before the A/B was run. Remaining 2b (`close`/`dup`/`dup3`/`fcntl`) is not started; the next attribution run should say
whether any of them is now large enough to be worth converting on its own, or whether the
remaining share is dominated by `irq/sched`/`execve` (i.e. scheduler and process-creation paths
outside this carve-out's scope).
