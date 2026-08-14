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
cut 2.4x, 0 stuck / 0 PANIC / 6-of-6 digests exact on both sides. Phase 2c's next target
`renameat`/`renameat2` is DONE, contention-confirmed by a controlled A/B (§14, 2026-07-30):
the base regimen didn't stress `mkdirat`/`renameat`/`fchmodat`/`truncate`/`close` at all, so it
was extended with `meta`/`trunc`/`openclose` phases first — the resulting attribution named
`renameat` (2.8%) narrowly ahead of `mkdirat` (2.6%) and `fchmodat` (1.8%) as the largest
untouched holders; converting `renameat` cut its own share **8.3% → absent** and total workload
spinning **289.4M → 255.0M spins** (~12%), 0 stuck / 0 PANIC / 6-of-6 digests exact on both sides.
Phase 2c's remaining pair `mkdirat`/`fchmodat` is DONE, contention-confirmed by a controlled A/B
(§15, 2026-07-30): both converted together (§14.6 named them the next-largest untouched holders,
5.2%/3.2% of a 285.7M-spin extended-regimen run); post-conversion neither appears in the top 12 —
both drop out entirely, matching `unlinkat`/`renameat` — and total workload spinning cut
285.7M → 246.0M spins (~14%), 0 stuck / 0 PANIC / 6-of-6 digests exact on both sides.
Remaining 2b (`close`/`dup`/`fcntl`) and 2d are NOT started — to be evidence-led off the next
attribution. The two pre-existing kernel bugs from §11 stand as before (thread-slot reclaim FIXED
§11.7; `wait`/SIGCHLD still open §11.3). §16–§18 (2026-07-31) found and fixed a profiler attribution
bug that had inflated `irq/sched` to 88.4%; corrected, it is 23.0%, and the true largest remaining
holder was the async-main smoltcp poll loop (`netpoll`, 59.7%) — not a VFS/net syscall at all. §19
decomposed that into sub-phases (the drain itself: 59.8%, isolated) and audited it as carveable by
the existing `NetBklGuard` precedent. **§20 (2026-08-01) shipped that carve, contention-confirmed:
`netpoll_drain`'s share went 57.2% → absent, and total workload spinning cut 144.6M → 47.3M spins
(−67.3%) — the largest single cut this campaign has measured — 0 stuck / 0 PANIC / 6-of-6 digests
exact on both sides. Default-on: rides the same `no-bkl-network` gate already bundled into
`smp-shared`, no new feature flag.**

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
   *absent*; SMP=4 stuck 598–704 → 0). **Second target `renameat`/`renameat2` DONE 2026-07-30 —
   see §14** (the base regimen never stressed `mkdirat`/`renameat`/`fchmodat`/`truncate`, so it
   was extended first; the resulting attribution named `renameat` at 2.8%, narrowly ahead of
   `mkdirat` 2.6% and `fchmodat` 1.8%; a controlled A/B confirmed its share cut 8.3% → absent).
   The remaining 2c list (`mkdirat`, `symlinkat`, `linkat`, `readlinkat`, `fchmodat`, `fchmod`,
   `truncate`, `ftruncate`, `fallocate`) is now evidence-led: §14's attribution names `mkdirat`
   (6.4%) and `fchmodat` (7.5% ON-side / 4.3% OFF-side) as the next-largest untouched holders,
   both still well behind `irq/sched`/`clone`/`execve` (Phase 3/process-management territory).
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

> **[2026-07-31] "Decisive" overstates it.** This is a cargo-feature toggle, so it is exposed to the
> stale-`.bin` trap (§17.1) — and "both sides fail identically" is precisely that trap's signature,
> so the OFF-build run cannot clear itself. The *conclusion* survives on the independent evidence in
> the next paragraph (T6 moves the same 32 MiB byte-exact through httpd over the same stack). Nothing
> in the campaign depends on this run; do not cite it as an A/B.

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

> **[2026-07-31]** This is one of only two A/Bs in this doc that toggled a *cargo feature* rather
> than source, so it is exposed to the stale-`.bin` trap — see §17.1, which clears it (the sides
> differ, which the trap cannot produce, and §9 root-caused the mechanism in code anyway).

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
- ~~**Phase 2c second target `renameat`/`renameat2`.** The base net4+read4+cp2+rm regimen never
  called `mkdirat`/`renameat`/`fchmodat`/`truncate` more than once or twice, so it couldn't say
  which of the rest of the 2c/2d list mattered.~~ **DONE — see §14 (2026-07-30).** Extended the
  regimen with `meta` (mkdir/rename/chmod)/`trunc`/`openclose` phases first, then attributed:
  `renameat` (2.8%) narrowly ahead of `mkdirat` (2.6%) and `fchmodat` (1.8%). Converted
  `renameat`/`renameat2`; a controlled A/B confirmed the share cut 8.3% → absent and total
  workload spinning dropped ~12%. `mkdirat` and `fchmodat` are now the next-largest untouched
  holders, both still well behind `irq/sched`/`clone`/`execve`.

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
 53.9%, `openat` `tag=56` **36.6%**, everything else <2%. *(§12.2 and §11.6 audited 2026-07-31
against the stale-`.bin` trap: both immune — see §17.1.)* `openat` is the surfaced next target —
**Phase 2b**, not 2c; the rest of the 2c list is now evidence-led rather than principle-led.
**DONE, contention-confirmed by A/B — see §13.3 (2026-07-30):** cumulative share cut 18.4% → 2.9%
(re-derived 2026-07-31; originally reported as 13.5% → 2.1%).

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
with the guard on vs. reverted cuts `openat`'s cumulative cross-core BKL share **18.4% → 2.9%**
(peak per-window 69.1% → 16.8%), and total workload spinning 1.9x, with 0 stuck / 0 PANIC and
6-of-6 exact digests on both sides. *(Those are the 2026-07-31 workload-restricted figures; the
originally-reported 13.5% → 2.1% / 2.4x came from a magnitude-filtered window selection — see the
dated block at the end of §13.3, and §17 for why this A/B is not affected by the stale-`.bin` bug.)*

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

> ### [2026-07-31] Re-derived after the stale-`.bin` fix — conclusion holds, three numbers corrected
>
> Re-audited when `scripts/cargo_runner.sh`'s stale-`.bin` bug was found
> (`BKL_PROCESS_CARVE_OUT.md` §9.8). **This A/B is not exposed to that trap** — both sides used the
> identical feature set (`devbox-smoltcp,no-tests,bkl-profile`) and differed only in *source* (the
> guard reverted), so cargo recompiled and the ELF's mtime moved forward before each boot. The trap
> needs a *cached* artifact uplifted with an mtime older than the `.bin` a different feature set left
> behind, which a source edit can never produce. Full audit of every A/B in this doc: §17.
>
> Both original serial logs survive (`/tmp/akuma_smp4.log` = ON, `/tmp/akuma_smp4_off.log` = OFF) and
> confirm two genuinely different kernels booted: `openat`'s per-window share tracks the `cp2` phase
> at 53.7 / 69.1 / 31.3 / 53.1% on the OFF side vs. 2.9 / 4.1 / 16.8% on the ON side. A stale `.bin`
> boots the *same* image twice, whose signature is no difference — not this. (Both logs also report an
> identical image extent, `1341 KB`, `0x40100000-0x402dfe38`; that is expected, not suspicious —
> `linker.ld` page-aligns `.rodata`/`.data`, so a sub-page `.text` delta is absorbed by padding and
> moves neither `_kernel_phys_end` nor the `.bin` byte count.)
>
> What *was* wrong: the window selection. The table above summed "every window >10M spins" rather
> than "the windows the regimen occupied" — which on the ON side pulled in a pre-regimen probe window
> (t=120s) and dropped the whole `cp2` phase (t=250–270s). Restricting instead to the regimen
> interval read off each log (ON: first `curl p*.bin` T175.2 → final `ls` T268.9, windows t=180–270s;
> OFF: T14.9 → T112.8, windows t=20–120s):
>
> | signal | ON (converted) | OFF (BKL-held) | as reported above |
> |---|---|---|---|
> | `openat` `tag=56` cumulative share | **2.9%** | **18.4%** | 2.1% vs 13.5% |
> | `openat` `tag=56` spins | 4,457,032 | 53,530,971 | 3,190,704 vs 49,291,185 |
> | `openat` peak per-window share | 16.8% | 69.1% | 16.0% vs 68.1% |
> | workload cumulative spins | 157,164,639 | 295,379,111 (**1.9x**) | 152,786,715 vs 364,902,792 (2.4x) |
>
> So the headline gets *stronger* on the share axis (6.3x rather than 6.4x separation, from a higher
> base on both sides) and weaker on the total-spin axis: **1.9x, not 2.4x**. Every conclusion in this
> section stands.
>
> One confound worth stating, since it is visible in the logs and was not noted at the time: the OFF
> boot's regimen started 6 s after boot (staging `rm` at T6.1), so its early windows overlap herd/sshd
> bringup, while the ON boot idled ~175 s first. That inflates OFF's `irq/sched`, not its `openat` —
> dropping the bringup-contaminated window *raises* OFF's `openat` share (13.7% → 18.4%). The
> cleanest phase-matched slice is `cp2` alone (32 MiB `cp` ×2, i.e. `O_CREAT`+`O_TRUNC` on a large
> destination — precisely the hold §13.1 targets), which is bounded identically on both sides:
> **`openat` 10.3% of 11.7M spins (ON) vs 50.5% of 38.0M spins (OFF)**.
>
> Caveat inherited, not introduced: this A/B predates the §16.2 profiler tag-restore fix, so
> `irq/sched` is over-credited on *both* sides. That does not move `openat`, whose windows are far
> shorter than a 10 ms tick.

This closes the "argued-correct, not contention-demonstrated" gap this section previously flagged
before the A/B was run. Remaining 2b (`close`/`dup`/`dup3`/`fcntl`) is not started; the next attribution run should say
whether any of them is now large enough to be worth converting on its own, or whether the
remaining share is dominated by `irq/sched`/`execve` (i.e. scheduler and process-creation paths
outside this carve-out's scope).

---

## 14. Phase 2c second target (`renameat`/`renameat2`) — DONE, contention-confirmed, 2026-07-30

§13.3 closed out `openat` and left the attribution ambiguous for the rest of 2b/2c/2d: §12.2's
post-`unlinkat` numbers named `openat` (36.6%) as the next holder, but nothing else in the 2b/2c/2d
list had ever been measured, because the standing net4+read4+cp2+rm regimen (§11.1) simply never
calls `mkdirat`/`renameat`/`fchmodat`/`truncate` more than once or twice, and never calls
`symlinkat`/`linkat`/`readlinkat`/`fchmod`/`ftruncate`/`fallocate`/`chdir`/`fchdir`/`getcwd`/
`fstatfs` at all. A first `bkl-profile` run with `openat` converted confirmed this directly: with
`irq/sched` (83.7%) and process-management syscalls (`nanosleep`/`ppoll`/`clone`/`execve`/`mmap`/
`read`) accounting for the rest, not one 2b/2c/2d syscall appeared in the top 12 — not because
they're cheap, but because the regimen doesn't exercise them.

### 14.1 Extending the regimen to get real signal

Added three phases to the payload script (kept as a scratch variant of
`scripts/bkl_smp_regimen/payload/job.sh`, run before the final `rm`):

- **`meta`** — two workers, each 60× `mkdir` + 60× `mv` (rename) + 60× `chmod`, run concurrently.
- **`trunc`** — two workers, each 15 iterations of shrinking a 32 MiB file to ~1 MB then growing
  it back (`truncate -s`), exercising the same class of block alloc/free `unlinkat` hit.
- **`openclose`** — two workers, each 200× `cat` of a 4 KiB file (open+read+close cycles), to get
  signal on `close` (Phase 2b) under repetition rather than the single close-per-download the base
  regimen gives it.

Re-run at SMP=4 with `bkl-profile` (same devbox-smoltcp harness, private disk clone + unique
ports so it didn't collide with another session's VM on the same host — see
`docs/reference/subsystems/locking.md` for the general pattern). Result, cumulative attributed
spins (499.2M total, `openat` already converted from §13):

| share | holder tag | what it is |
|---|---|---|
| 65.2% | `irq/sched` (501) | scheduler / IRQ path — Phase 3's target |
| 12.3% | `clone` (220) | process creation — outside this carve-out |
| 7.6% | `execve` (221) | process creation — outside this carve-out |
| **2.8%** | `renameat` (nr 38) | **directory-entry rewrite — Phase 2c** |
| **2.6%** | `mkdirat` (34) | **directory creation — Phase 2c** |
| 2.3% | `nanosleep` (101) | sshd's accept-poll loop |
| **1.8%** | `fchmodat` (nr 53) | **Phase 2c** |
| 1.7% | `ppoll` (73) | |
| 1.5% | `openat` (56) | residual, already converted (§13) |
| 0.8% | `read` (63) | already converted (§4) |

`trunc` and `openclose` produced no measurable signal (`truncate`/`close` do not appear in the top
12) — consistent with §11.6's finding for `read`/`write`: simple ops with no expensive block
work behind them stay cheap regardless of BKL state. `renameat` edges out `mkdirat` by a margin
well inside the profiler's own perturbation noise (§11.5), but it's the top of the list, so per
§7's evidence-led rule it's the next conversion.

### 14.2 The change

Both `sys_renameat` and `sys_renameat2` (`src/syscall/fs.rs`) now take a `VfsBklGuard` window
immediately after copying the two path strings out of user memory, covering: `resolve_path_at`
for both paths (fd-table + string-normalization work only, no disk I/O — same as the equivalent
call in `sys_unlinkat`/`sys_openat`), the `RENAME_NOREPLACE` existence probe in `sys_renameat2`
(a real lookup), and `crate::fs::rename` (the ext2-write-guarded directory-entry rewrite —
`akuma-ext2`'s `rename` reads both parent directories, optionally frees a replaced destination
inode, then rewrites both directory entries, all under one `write_state` guard). No new locks;
mirrors the §12/§13 placement exactly. Zero-cost no-op off `smp-shared` + `no-bkl-vfs`.

### 14.3 Verification — boot self-test (correctness)

New `test_renameat` (`src/process_tests.rs`), structurally identical to §12/§13's tests: drives
`handle_syscall(RENAMEAT, …)` and `RENAMEAT2`, pinning —

| # | case | pins |
|---|---|---|
| 1 | absolute paths both sides | source gone, destination holds the content |
| 2 | `AT_FDCWD`-relative (`cwd=/tmp`) both sides | cwd-relative resolution |
| 3 | dirfd-relative (`fd 7 → …/sub`) both sides | dirfd must NOT be ignored (the rm-recursion family) |
| 4 | `renameat2` `RENAME_NOREPLACE` onto an existing destination | `EEXIST`, source untouched — the `exists` probe now runs *inside* the window |
| 5 | dirfd `999` (unopen) | exercises the error path, see below |
| 6 | missing source, no `O_CREAT`-equivalent | `ENOENT` through the dropped window |

Case 5 surfaced a real (pre-existing, not a regression) divergence while writing the test: unlike
`sys_unlinkat`, which explicitly checks the dirfd and returns `EBADF`, `sys_renameat` has no such
check at all — `resolve_path_at` falls through to base `"/"` for an unresolvable dirfd (same
family as `sys_openat`'s documented `/proc/self/exe`-ordering divergence, §13.2), so
`renameat(999, "anything", …)` resolves to `/anything` and returns `ENOENT`, not `EBADF`. The
first version of the test asserted `EBADF` and failed (panicking, which is by design — the test
`panic!()`s on any failed case); fixed by asserting the actual pre-existing behavior instead of
changing it, matching how this carve-out treats every other such divergence (preserve behavior,
don't fix unrelated bugs in the same commit).

**SMP=2 boot, `smp-shared` + `no-bkl-vfs`:** `[Test] renameat PASSED (6 cases)`, plus `unlinkat`
and `openat` still pass and `no_spurious_svc_traps` reports 0 phantom SVCs. Full suite: 0 `FAILED`
lines this run (the two standing §6 pre-existing failures are flaky/non-deterministic and simply
didn't fire this boot), 0 PANIC/WILD/stale-dropped-window heals. 19 `[BKL] stuck` lines, all in
the known NEON/FP-test-region noise band (§6/§9.3's ~15–16 baseline).

### 14.4 Verification — controlled A/B (contention), SMP=4, `bkl-profile`, 2026-07-30

Same-binary A/B per the §13.3 template: the extended regimen (net4+read4+cp2+**meta+trunc+
openclose**+rm) run twice at SMP=4 on private disk clones with the `renameat`/`renameat2` guard
toggled — ON (the §14.2 change in place) vs. OFF (`git show HEAD:src/syscall/fs.rs` swapped in
to get the pre-conversion source, **not** `git stash`, since another agent had uncommitted work
in the same working tree — restored afterward, confirmed byte-identical to the intended landing
state via `git diff`).

| signal | ON (converted) | OFF (reverted) |
|---|---|---|
| `renameat` `nr=38` cumulative share | **absent** (not in top 12) | **8.3%** (23.9M spins) |
| workload cumulative attributed spins | 255,013,999 | 289,377,294 (1.13x) |
| `[BKL] stuck` during workload | 0 | 0 |
| PANIC / WILD | 0 / 0 | 0 / 0 |
| digests (4 net + 2 cp) | 6/6 exact | 6/6 exact |
| regimen wall-clock | 136 s | 135 s |

Top holders on the OFF side: `irq/sched` 42.0%, `clone` 19.3%, `execve` 13.6%, `renameat` 8.3%,
`mkdirat` 5.0%, `fchmodat` 4.3%, `openat` 2.8%. On the ON side: `irq/sched` 43.9%, `clone` 19.8%,
`execve` 13.8%, `fchmodat` 7.5%, `mkdirat` 6.4%, `openat` 2.0% — `renameat` drops out of the list
entirely rather than just shrinking, matching the `unlinkat` pattern (§12) more than the `openat`
pattern (§13, which left a small residual). `mkdirat`'s and `fchmodat`'s shares shift up slightly
between the two runs (profiler perturbation + normal run-to-run variance, §11.5/§12.5's standing
caveat), not a sign either was affected by the `renameat` guard.

*[2026-07-31] Audited against the stale-`.bin` trap: **immune** — the `git show HEAD:…` swap is a
source edit, which always forces a recompile and a fresh ELF mtime (§17.1). No re-run needed.*

### 14.5 Data integrity

Both `e2fsck -fn` (read-only) passes on the ON- and OFF-side disk images post-run reported the
*same* "unattached zero-length inode" / inode-bitmap-difference symptom on the *same* two
inodes — the §12.4 write-back-cache-with-no-sync-on-raw-kill artifact, reproduced identically
regardless of the guard, i.e. not a `renameat`-specific regression. The decisive check is the
digest table above: content correctness was verified via `sha256sum` on both the renamed-into
destination path (self-test, §14.3 case 1) and the full regimen's read-back/copy phases (§14.4),
not just via `ls`/exit-code as `unlinkat`'s original §12.3 did — a `rename` that silently failed
or scrambled content would show up as a digest mismatch, not just a missing file.

### 14.6 Next

`mkdirat` (2.6%/6.4%) and `fchmodat` (1.8%/7.5%) are now the largest untouched holders in the 2c
list, both still an order of magnitude behind `irq/sched`/`clone`/`execve`. Whether either is
worth converting on its own — versus the remaining win being in Phase 3 (scheduler/IRQ) or
process-creation paths outside this carve-out's scope — is exactly the question the next
attribution run should answer, per §7's evidence-led rule.

## 15. Phase 2c remaining pair (`mkdirat`/`fchmodat`) — DONE, contention-confirmed, 2026-07-30

§14.6 left `mkdirat` and `fchmodat` as the largest untouched Phase 2c holders, both an order of
magnitude behind `irq/sched`/`clone`/`execve` — small enough that whether converting them was
worth it at all was an open question. Converting both together (they share the exact same
dirfd-resolution shape as `unlinkat`) and re-measuring answers that question directly rather than
guessing.

### 15.1 The change

`sys_mkdirat` and `sys_fchmodat` (`src/syscall/fs.rs`) are restructured to the `sys_unlinkat`
shape rather than the `sys_renameat` shape: both originally interleaved the dirfd/EBADF resolution
with the path-string building in one `if/else` chain, so a straight "add a guard before the disk
call" edit would have left early-return `EBADF` paths inside the window. Instead the dirfd
resolution is pulled out into a `base: Option<String>` computed *before* the guard opens (fd-table
lookup + early `EBADF` returns only, no disk I/O — identical shape to `unlinkat`'s `base`), and the
`VfsBklGuard` opens immediately after, covering: `resolve_path`/`canonicalize_path` (string work),
`crate::fs::create_dir` (ext2 write guard, inode alloc + dirent write) for `mkdirat`; and for
`fchmodat`, `resolve_symlinks` (a real on-disk symlink-target lookup — same class of real lookup as
`renameat2`'s `RENAME_NOREPLACE` `exists` probe, §14.2) plus `crate::vfs::chmod` (ext2 write guard,
mode-bits rewrite). The `/dev/null`/`/dev/zero` fast path in `fchmodat` stays inside the window
(a branch within the window, not a different fd *kind* that needs routing around it — see
`locking.md`'s "route special fd kinds before the guard opens" rule, which is about pipe/socket fds
specifically). No new locks; zero-cost no-op off `smp-shared` + `no-bkl-vfs`.

### 15.2 Verification — boot self-test (correctness)

New `test_mkdirat` and `test_fchmodat` (`src/process_tests.rs`), structurally identical to
§12/§14's tests, drive `handle_syscall(MKDIRAT, …)` / `handle_syscall(FCHMODAT, …)`:

| syscall | # | case | pins |
|---|---|---|---|
| mkdirat | 1 | absolute path | directory created |
| mkdirat | 2 | `AT_FDCWD`-relative (`cwd=/tmp`) | cwd-relative resolution |
| mkdirat | 3 | dirfd-relative (`fd 7 → …/sub`) | dirfd must NOT be ignored |
| mkdirat | 4 | dirfd `999` (unopen) | `EBADF` — unlike `renameat` (§14.3 case 5), `mkdirat` explicitly checks `proc.get_fd` before the window opens |
| mkdirat | 5 | target already exists | `EEXIST` through the dropped window |
| mkdirat | 6 | missing parent | `ENOENT` through the dropped window |
| fchmodat | 1 | absolute path | mode bits actually changed (`metadata().mode`, not just exit code) |
| fchmodat | 2 | `AT_FDCWD`-relative | cwd-relative resolution |
| fchmodat | 3 | dirfd-relative (`fd 7 → …/sub`) | dirfd must NOT be ignored |
| fchmodat | 4 | dirfd `999` (unopen) | `EBADF`, same shape as `mkdirat` case 4 |
| fchmodat | 5 | missing target | `ENOENT` through the dropped window (`resolve_symlinks` + `chmod`'s lookup both run first) |
| fchmodat | 6 | `/dev/null` | fast-path short-circuit to `0`, now living inside the window |

Unlike `renameat`'s divergence discovery (§14.3 case 5), `mkdirat`/`fchmodat` had no surprise here
— both already had an explicit `EBADF` dirfd check pre-conversion, and restructuring preserved it
exactly (case 4 on both pins this).

**SMP=2 boot, `smp-shared` + `no-bkl-vfs`** (private disk clone + unique ports, isolated from a
concurrent agent's own VM on the same host — see `locking.md`'s isolated-verification pattern):
`[Test] mkdirat PASSED (6 cases)`, `[Test] fchmodat PASSED (6 cases)`, plus `unlinkat`/`openat`/
`renameat` still pass and `no_spurious_svc_traps` reports 0 phantom SVCs. Full suite: 2 `FAILED`
lines, both the §6 pre-existing flaky failures (`fs_error_to_errno_mapping` EPERM/EACCES,
`stp_xzr_ec15_handler_fires`), 0 PANIC/WILD/stale-dropped-window heals, 18 `[BKL] stuck` lines
(within the §6/§9.3 NEON/FP-test-region noise band).

### 15.3 Verification — controlled A/B (contention), SMP=4, `bkl-profile`, 2026-07-30

Same-binary A/B per the §13.3/§14.4 template, on a `devbox-smoltcp` VM: the regimen extended with
§14.1's `meta` phase (two workers, each 60× `mkdir` + 60× `mv` + 60× `chmod`, concurrently — kept
as a scratch edit to `scripts/bkl_smp_regimen/payload/job.sh`, restored via `git checkout`
afterward, not landed in the standing regimen) run twice at SMP=4 on private `devbox.img` clones —
ON (the §15.1 change in place) vs. OFF (`git show HEAD:src/syscall/fs.rs` swapped in for the
pre-conversion source, restored afterward, `git diff --stat` confirmed identical to the intended
landing state both before the swap and after restoring it).

| signal | ON (converted) | OFF (reverted) |
|---|---|---|
| `mkdirat` `nr=34` cumulative share | **absent** (not in top 12) | **5.2%** (14.8M spins) |
| `fchmodat` `nr=53` cumulative share | **absent** (not in top 12) | **3.2%** (9.2M spins) |
| workload cumulative attributed spins | 246,018,905 | 285,698,468 (1.16x) |
| `[BKL] stuck` during workload | 0 | 0 |
| PANIC / WILD | 0 / 0 | 0 / 0 |
| digests (4 net + 2 cp) | 6/6 exact | 6/6 exact |
| regimen wall-clock | 136 s | 136 s |

Top holders on the OFF side: `irq/sched` 66.3%, `clone` 12.9%, `execve` 5.5%, `mkdirat` 5.2%,
`nr53`(`fchmodat`) 3.2%, `rt_sigprocmask` 2.0%, `read` 1.3%. On the ON side: `irq/sched` 73.2%,
`clone` 12.5%, `execve` 6.5%, `nanosleep` 2.2%, `read` 1.5%, `openat` 1.3% — both `mkdirat` and
`fchmodat` drop out of the list entirely rather than shrinking, matching the `unlinkat`/`renameat`
pattern (§12/§14) more than `openat`'s (§13, which left a small residual). Note `fchmodat`
(syscall 53) prints as `nr53` in the OFF-side table — `src/bkl_profile.rs`'s tag-label table
doesn't yet have a friendly name for tag 53; cosmetic only, not fixed here (out of scope for a BKL
conversion, and every other reading — the syscall-number cross-reference, the self-test names — is
unambiguous).

*[2026-07-31] Audited against the stale-`.bin` trap: **immune**, same reasoning as §14.4 — the two
sides differ in source, not in cargo features (§17.1). No re-run needed.*

### 15.4 Data integrity

Both disk clones were `e2fsck -fy`'d before boot and showed the identical "unattached zero-length
inode" symptom on the same two inodes (the §12.4/§14.5 write-back-cache-with-no-sync-on-raw-kill
artifact, present on both clones *before* either VM even booted — confirms it predates this
change and isn't `mkdirat`/`fchmodat`-specific). Both VMs ran with `snapshot=on` (guest writes
discarded at exit), so a post-run `e2fsck` comparison would just re-show the identical pre-run
state and carries no signal this round — the decisive check is the digest table in §15.3:
`sha256sum` on all 4 downloaded + 2 copied files, read live from inside each VM over SSH before
teardown, 6/6 exact on both sides. A `mkdir`/`mv`/`chmod` that silently corrupted an unrelated
inode during the concurrent `meta` phase would show up as a digest mismatch in the untouched
net4/cp2 files, not just as a `meta`-phase exit code.

### 15.5 Next

With `mkdirat`/`fchmodat` converted, every Phase 2c syscall this campaign's attribution ever named
(`unlinkat`, `renameat`/`renameat2`, `mkdirat`, `fchmodat`) is now BKL-free. Remaining untouched
work in this carve-out: Phase 2b's `close`/`dup`/`fcntl` (never yet named by an attribution run —
§14.1's `openclose` phase produced no signal on `close`, consistent with §11.6's "simple ops with
no expensive block work behind them stay cheap regardless of BKL state" finding) and Phase 2d
(unexercised entirely: `symlinkat`/`linkat`/`readlinkat`/`fchdir`/`getcwd`/`fstatfs`/`truncate`/
`ftruncate`/`fallocate`). Both OFF-side runs in this campaign (§14.4, §15.3) now show `irq/sched`
alone accounting for two-thirds-plus of all attributed spin (66.3–73.2%, climbing as more VFS
syscalls convert) — per §7's evidence-led rule, the next attribution run should check whether
Phase 3 (scheduler/IRQ) now dominates the remaining opportunity enough to redirect effort there
instead of chasing the rest of 2b/2d's now much smaller shares.

## 16. Fresh Phase 2c baseline + a profiler attribution bug found and fixed — 2026-07-30

§15.5 asked the obvious next question: is `irq/sched` genuinely dominant now, and if so what does
a first Phase 3 step look like? Re-running the attribution regimen to answer it surfaced a bug in
the attribution tool itself, which had to be fixed before the question could be answered honestly.

### 16.1 Fresh baseline confirms Phase 2c is fully closed

Same-shape run as §14.4/§15.3 (private disk clone + `INSTANCE=1` ports, isolated from another
agent's own VM on this host — `devbox.img` was already held open by a concurrent SMP=2 instance),
`net4+read4+cp2+meta+rm` regimen (§14.1's `meta` phase re-added as the same scratch edit to
`payload/job.sh`, restored via `git checkout` afterward), SMP=4, `bkl-profile`, current tree
(`unlinkat`/`renameat`/`renameat2`/`mkdirat`/`fchmodat` all converted):

| signal | result |
|---|---|
| regimen wall-clock | 136 s |
| `[BKL] stuck` | 0 |
| PANIC / WILD | 0 / 0 |
| digests (4 net + 2 cp) | 6/6 exact |
| VFS syscalls in top-12 attribution | **none** |

No VFS syscall this campaign ever converted shows up in the attribution table at all — the Phase
2c conversions hold under a workload that specifically drives `mkdir`/`mv`/`chmod` concurrently.
`irq/sched` 66.3%, `clone` 16.5%, `execve` 7.0% round out the top three (raw, pre-fix numbers —
see §16.3 for the corrected version).

### 16.2 The attribution tool over-credits `irq/sched`: found while sanity-checking the numbers

`irq/sched`'s dominance was suspicious on its own terms: the M5c step-2 scheduler-BKL-free-on-
EL0-preemption optimization (`sched_bklfree_el0`, `src/smp_shared.rs`) has been shipping
**default ON since commit 80262af, 2026-07-24** — six days before this campaign's §14/§15 runs —
so every prior "irq/sched ~66-73%" number already had that optimization applied and still showed
irq/sched as dominant. That's consistent with a real cost, but the *code* said otherwise once
read closely.

`src/bkl_profile.rs`'s attribution model: `akuma_exec::sync::set_holder_tag(core, tag)` stamps a
per-core "what am I doing while I hold the BKL" tag at every kernel-entry site — syscall number
at syscall entry (`src/exceptions.rs:2257`), `HOLD_TAG_FAULT` at fault entry, `HOLD_TAG_IRQ` at
IRQ/scheduler-SGI entry (`src/exceptions.rs:1581`, pre-fix). A waiting core samples the *current*
holder tag once, the first time it notices contention (`KernelLock::acquire`'s `wait_tag`,
`crates/akuma-exec/src/sync.rs`), and credits every spin of that wait to whatever tag it sampled.

The bug: `rust_irq_handler_with_sp` (`src/exceptions.rs`) stamps `HOLD_TAG_IRQ` **unconditionally**
whenever a device IRQ or a scheduler SGI that preempted EL1 fires — including a timer tick landing
mid-syscall, where the BKL stays held and the *same* excursion resumes right after the brief IRQ
dispatch. Nothing ever restored the interrupted excursion's own tag afterward; the core stayed
labeled `irq/sched` for the rest of that syscall, no matter how long it ran. Any BKL-holding
operation that survives one or more 10 ms timer ticks — which under SMP=4 load is exactly the
long tail (`clone`, `execve`, big ext2 writes) — bleeds its later contention into the `irq/sched`
bucket. Short, already-carved-out operations (a `VfsBklGuard` window is typically <<10 ms) never
cross a tick boundary, so they don't get mislabeled — which is also exactly why converted VFS
syscalls "drop out entirely" rather than shrink gradually (§15.3's own observation): once an
operation's *remaining* BKL-held portion is short enough to outrun the tick, it stops feeding this
artifact, not because the underlying cost fully vanished.

This is a genuine analysis of the *profiler's* code path, not conjecture: `set_holder_tag` in
`crates/akuma-exec/src/sync.rs` has no restore-on-return; `HOLDER_TAG` is a flat per-core array
with no save/restore stack.

### 16.2.1 The fix

Minimal, profiler-only, zero behavioral risk (gated the same way the rest of `bkl-profile` is —
a no-op unless the runtime `PROFILE_ENABLED` flag is on, itself gated behind `cfg(kernel_smp_shared)`):

- `crates/akuma-exec/src/sync.rs`: new `holder_tag(core_id) -> u64` reader (mirrors
  `set_holder_tag`'s off-is-no-op behavior, returning `HOLD_TAG_UNKNOWN` when profiling is off).
- `src/exceptions.rs` (`rust_irq_handler_with_sp`): save the core's tag before stamping
  `HOLD_TAG_IRQ` for the dispatch, restore it after — but **only when `new_sp == 0`** (no context
  switch happened, i.e. the same excursion resumes on this core). If the scheduler picked a
  *different* thread to run (`new_sp != 0`), the saved tag belongs to the thread being switched
  away from, not the one about to run — reapplying it would misattribute in the other direction,
  so that case is left as `HOLD_TAG_IRQ` (honest "just scheduled, not yet re-profiled") rather than
  guessed at.

Verified compiling clean on `--release` (default, tag calls fully absent), `release-smp-shared`
with `smp-shared` only (tag calls present, profiler off, no warnings), and `release-smp-shared`
with `devbox-smoltcp,no-tests,bkl-profile` (the attribution build).

### 16.3 Corrected attribution — same regimen, same tree, only the profiler's tag-restore fixed

Fresh disk clone (the §16.1 clone had 120 leftover `meta`-phase directories and dirty e2fsck
inodes from that run), same isolated VM setup, identical regimen:

| holder | pre-fix (§16.1) | post-fix | delta |
|---|---|---|---|
| `irq/sched` | 66.3% | **58.4%** | **−7.9pp** |
| `clone` | 16.5% | **22.5%** | **+6.0pp** |
| `execve` | 7.0% | **8.3%** | +1.3pp |
| `nanosleep` | 2.8% | 3.1% | ~flat |
| `read` | 1.2% | 2.0% | ~flat |

`[BKL] stuck` 0, PANIC/WILD 0/0, 6/6 digests exact — the fix changes attribution only, not
behavior, exactly as intended. *(Audited 2026-07-31 against the stale-`.bin` trap: **immune** — pre-
and post-fix differ in source, same feature set, §17.1. But both are whole-boot cumulative views;
see §17.2 on why that dilutes.)* The correction moved ~8 points from `irq/sched` into `clone`/
`execve`, matching the mechanism in §16.2: those are the two syscalls in this workload most
likely to span a tick boundary. `irq/sched` remains the single largest bucket even corrected
(58.4%, still bigger than `clone`+`execve` combined at 30.8%) — so Phase 3's premise (§15.5)
survives the correction, just at a smaller, more trustworthy magnitude. (Also observed, both
runs: 68–75 `[BKL] RECOVERED (reticket-skipped)` lines — the still-open, self-healing ticket-leak
family described in `smp-shared.md`'s 2026-07-21 update, not new and not this session's doing;
7–8 `[WATCHDOG] Preemption disabled for >1.7s at sync.rs:126` warnings during the `meta` phase in
*both* runs, equally present pre- and post-fix — also pre-existing, not investigated further here,
out of scope for a profiler-attribution fix.)

### 16.4 Tangential finding: stale doc comments on an already-shipped fix

While tracing `sched_bklfree_el0`'s default (needed to know whether §16.1's baseline already had
M5c step-2 applied), found that `src/smp_shared.rs`'s doc comment on `SCHED_BKLFREE_EL0_ENABLED`
and `docs/reference/subsystems/smp-shared.md`'s M5c status row both still described the flag as
"defaults OFF, not yet safe under load, leaks a ticket" — but the code has read
`AtomicBool::new(true)` since commit 80262af ("more smp fixes", 2026-07-24), which added
`reconcile_for_spsr_no_ticket`/`KernelLock::acquire_no_ticket` specifically to fix that leak
(`crates/akuma-exec/src/bkl.rs`, `crates/akuma-exec/src/sync.rs`) and flipped the default the
same commit. `docs/runbooks/debug-smp.md` was updated correctly in that commit; the other two
were not. Fixed both to match the code (git-blame-verified against 80262af before editing) —
worth flagging since this is exactly the "stability grade" trust the doc header promises being
briefly violated by a same-day partial doc update.

### 16.5 Next: `clone`/`fork_process` is the concrete Phase 3 candidate — flagged, not started

Per §7's rule, the corrected numbers (§16.3) name the next target directly: `clone` (22.5%) is now
larger than any VFS syscall this campaign ever measured (`unlinkat`'s peak was 72.6%, but that was
against a workload where nothing else was converted yet; every other VFS syscall topped out at
≤5.2%), and unlike `execve` (8.3%, already partially covered — `set_exec_bkl_drop_enabled`,
default on, drops the BKL around the ELF-read portions of `do_execve` and the dynamic-linker load,
per `debug-smp.md`), `fork_process` (`crates/akuma-exec/src/process/mod.rs:1487`) has **no BKL-drop
treatment at all** — grepped for `VfsBklGuard`/`NetBklGuard`/`PreemptGuard`/`BklGuard` in that
function and its call sites in `src/syscall/proc.rs`; none found. The whole fork body (CoW
page-table walk/copy, `ProcessInfo` write, context capture, child-thread registration) runs BKL-
held start to finish.

This is a legitimate "first concrete Phase 3 step" candidate by the same evidence-led rule that
picked every VFS target so far — but **it is a materially different risk profile than any VFS
conversion in this doc**, flagged rather than started this session:

- Every VFS carve-out target (§4, §12–§15) touches state with an *already-existing* fine-grained
  lock one level down (fd table, ext2 superblock/BGD, block cache) — the work was proven to be "a
  BKL-drop guard plus inner-lock hardening," never a new lock (§ "Headline" in the Background
  section / `BKL_FINE_GRAINED_LOCKING_PLAN.md`'s Phase 4 progress note). Whether `fork_process`'s
  state has the same property is unaudited — CoW page-table sharing is live, cross-core-visible
  state the instant the child thread is marked `READY`, not a self-contained per-fd or per-inode
  structure.
- `smp-shared.md`'s "Separate issues surfaced 2026-07-20" section documents an **unresolved**
  SMP=4 memory-corruption family in exactly this code path: forked children crash with
  heterogeneous signatures (zeroed GPRs, kernel-address user PC, empty mmap-region lists) during
  the bringup window, root-caused to fork/exec/exit not being atomic across preemption. A BKL-
  scoping change to `fork_process` would be edited directly adjacent to that live, open bug — any
  new symptom during verification would need to be triaged against "did the carve-out cause this"
  vs. "this is the pre-existing corruption," which the VFS carve-out never had to contend with.

Recommendation: before touching `fork_process`'s locking, first audit which of its steps
(`FORK-DBG step1`..`step8` in the debug logging) touch state that already has its own lock vs.
state that's genuinely BKL-dependent for cross-core CoW correctness — the same audit VFS got for
free (every subsystem already had a lock; process/CoW state might not). That audit, plus reading
`archive/SMP_FORK_EXEC_CORRUPTION_FIX.md` in full first, is the right-sized next task — not a
guard-and-measure cycle straight off this doc's playbook.

**[2026-07-31] That audit is now complete: see
[`BKL_PROCESS_CARVE_OUT.md`](BKL_PROCESS_CARVE_OUT.md).** Result: no carve-out implemented.
Every significant-BKL-time step touches state with no inner lock; the BKL is the lock, not a
redundant wrapper. A carve-out requires either fixing the fork-corruption bug first or building
the process-table lock the original Phase 3 plan sketched.

---

## 17. Stale-`.bin` audit: do this doc's A/B numbers survive? — 2026-07-31

On 2026-07-31 `scripts/cargo_runner.sh` was found to silently boot the wrong kernel
(`BKL_PROCESS_CARVE_OUT.md` §9.8, the "stale-`.bin` trap" block). The ELF→flat-binary step was
guarded by `[ ! -f "$BIN" ] || [ "$ELF" -nt "$BIN" ]`; cargo **uplifts** a cached artifact into
`target/` when you switch back to a previously-built feature set, and the uplifted ELF can carry an
mtime *older* than the `.bin` the feature set you built in between left at the same path (`$BIN` is
just `$ELF.bin`, and every feature set under one profile shares it). objcopy was then skipped and
QEMU booted the other feature set's kernel. The buggy guard was in the tree from **2026-05-28
(e771939) to 2026-07-31 (738ff52)** — i.e. for this campaign's entire duration, so every A/B here
had to be classified rather than dismissed on dates.

### 17.1 What is and isn't exposed

The trap fires **only** when cargo does not recompile, because only then can the ELF's mtime be
older than the `.bin`. Any A/B whose two sides differ in *source* forces a recompile and stamps a
fresh mtime on the ELF before every boot — structurally immune. Any A/B whose two sides differ only
in *cargo features/profile*, alternated, can hit it.

Its failure mode is also worth naming: it boots the **same** image twice, so its signature is *no
difference between the two sides*. A large, direction-correct difference is positive evidence the
trap did not fire.

| § | A/B | how the two sides were produced | verdict |
|---|---|---|---|
| §8 "⚠ One real regression signal" | `no-bkl-vfs` ON vs OUT, 8 vs 0 `[BKL] stuck` | **cargo feature toggled** | **exposed — but cleared**: the two sides differ (8 vs 0), which the trap cannot produce; and §9 root-caused the mechanism in the IRQ-epilogue code and fixed it, so the finding never rested on this A/B alone |
| §8 T4 (`ssh cat` truncation) | `no-bkl-vfs` compiled out, still lossy | **cargo feature toggled** | **exposed, not clearable by this A/B**: both sides failing is exactly the trap's signature. The conclusion ("pre-existing, not ours") still holds on independent evidence — T6 moves the same 32 MiB byte-exact through httpd over the same stack, localizing the defect to the sshd exec-channel bridge. Treat the OFF-build *run* as unverified; nothing in the campaign depends on it |
| §12.2 | `unlinkat` 72.6% → absent | before/after *commit*, same feature set | immune (source differs) |
| §13.3 | `openat` 13.5% → 2.1% | same feature set, guard **reverted in source** | immune — and re-derived from the original logs, see the 2026-07-31 block in §13.3 |
| §14.4 | `renameat` 8.3% → absent | `git show HEAD:src/syscall/fs.rs` swapped in | immune (source differs) |
| §15.3 | `mkdirat` 5.2%, `fchmodat` 3.2% → absent | `git show HEAD:src/syscall/fs.rs` swapped in | immune (source differs) |
| §16.1 / §16.3 | pre-fix vs post-fix attribution | profiler source fix, same feature set | immune (source differs) |
| §11.6 | `unlinkat` 72.6% | single run, first build of the then-new `bkl-profile` feature | immune (no cached artifact to uplift) |

**Nothing in §§12–16 needs retracting.** The only feature-toggled A/Bs in this doc are §8's two, both
from 2026-07-25, and neither carries a contention number the campaign later built on.

### 17.2 The other half: restrict attribution to the workload windows

Re-deriving §13.3 from its saved logs surfaced a methodology bug that is *not* the stale-`.bin` bug
and that every attribution table in this doc should be read against: `analyze.py`'s default view is
whole-boot, and the ad-hoc substitute used in §13.3 ("every window >10M spins") is a magnitude
filter, not a time filter. On a boot whose regimen starts 6 s in, it silently counts service bringup
as workload; on a boot that idles first, it drops real workload phases that happened to spin less
than the threshold.

Read `drive.py`'s REGIMEN START/DONE timestamps — or, for a hand-driven run, the first and last
regimen `execve` in the serial log — and sum `[BKLPROF]` per-tag spins only over the `t=` windows
spanning that interval. Doing so changed §13.3's total-spin ratio from 2.4x to 1.9x (both
`openat` shares moved up, and the conclusion strengthened); it changed Phase 3's `clone` number from
25.2% whole-boot to 19.5% workload (`BKL_PROCESS_CARVE_OUT.md` §9.8). Same conclusions both times —
but only the workload-restricted numbers are defensible.

---

## 18. `irq/sched` was an artifact: thread-scoped attribution — 2026-07-31

§16.2 found the profiler over-credits `irq/sched` and §16.2.1 fixed half of it, deliberately
leaving the other half in place. This section closes it, and the answer changes the campaign's
direction: **`irq/sched` is not ~88% of remaining cross-core BKL wait. It is ~23%.**

### 18.1 Why the remaining half mattered

`BKL_PROCESS_CARVE_OUT.md` §9.8's workload-restricted A/B put `irq/sched` at **88.4%** on the
carved side, and by the campaign's evidence-led rule that number picked the next target. But it
was also the campaign's least trustworthy number, for a mechanism the code stated plainly
(`src/exceptions.rs`, pre-fix):

```rust
if new_sp == 0 {
    set_holder_tag(current_core_id(), prev_holder_tag);   // no switch: restore
}
// new_sp != 0: left as HOLD_TAG_IRQ — "honest, not guessed"
```

`HOLDER_TAG` was a flat per-**core** array stamped only at kernel-*entry* sites (syscall number,
`HOLD_TAG_FAULT`, `HOLD_TAG_IRQ`). So a timer tick that context-switched handed the incoming
thread the `irq/sched` label, and a thread preempted mid-EL1 inside a long BKL-held syscall never
re-enters the kernel — it ran the whole remainder of that syscall labelled `irq/sched`.

Which excursions get preempted? The long ones — exactly the ones worth finding. The bucket that
systematically absorbed them was ~88% of the pie.

### 18.2 The fix: attribution follows the thread

A kernel excursion belongs to a **thread**. It survives preemption and can resume on another core,
so core-scoped state cannot represent it — the same reason `DroppedWindowLedger`
(`crates/akuma-exec/src/bkl.rs`) is thread-scoped.

Two shapes were considered. **(b)** a per-thread array with a core→thread indirection at the
sampling point was rejected: the waiter knows only the owner's *core* (`owner` encodes core
`cur - 1`), and nothing maps core → current thread — `LAST_CORE` runs the other way — so (b) would
have had to add a core→tid mirror **anyway**, plus an extra dependent load on the sampling path.
**(a)** was taken: keep the cache-line-padded per-core `HOLDER_TAG` as the sampling point,
unchanged, and make it a *cache* of an authoritative per-thread table.

`crates/akuma-exec/src/sync.rs` gains `ThreadTagTable<N>` (pure, host-testable, out-of-range tids
inert — `DroppedWindowLedger`'s shape) as `THREAD_TAG`, and three operations that together give the
invariant `HOLDER_TAG[c] == THREAD_TAG[thread running on c]`:

| operation | writes | called from |
|---|---|---|
| `set_holder_tag(core, tag)` | **both** tables | kernel entry: syscall nr, `HOLD_TAG_FAULT` |
| `set_core_tag_transient(core, tag)` | core cache **only** | the IRQ dispatch stamp |
| `load_thread_tag_to_core(core, tid)` | core cache, from the table | `set_current_thread_register` + IRQ epilogue |

The transient stamp is the key: because the IRQ dispatch never touches `THREAD_TAG`, the
interrupted thread's own tag is never lost, and the epilogue does not need to remember it.

**§16.2.1's `new_sp == 0` special case is gone, not kept alongside.** The epilogue is now
unconditional:

```rust
load_thread_tag_to_core(current_core_id(), current_thread_id());
```

One rule covers both outcomes. After a switch, `current_thread_id()` is the *incoming* thread and
we install its own tag — the case §16.2.1 declined to guess at is now a lookup, not a guess. With
no switch it is the interrupted thread and we reinstall the tag it never lost.

`set_current_thread_register` (`threading/mod.rs`) is the choke point every switch path funnels
through — `commit_switch`, the network-thread boost in `schedule_indices`, per-core idle adoption,
boot — so hooking it there also fixed a case the old code never even reached: the
`sched_bklfree_el0` EL0-preemption path returns from `rust_irq_handler_with_sp` early and touched
no tags at all, leaving an incoming EL1 thread wearing the outgoing EL0 thread's label.

Recycled thread slots are reset in `claim_free_slot`, so a fresh thread cannot lend its
predecessor's tag to a waiter.

Profiler-only and gated exactly as the rest of `bkl-profile` is: every accessor is an early-return
no-op unless `PROFILE_ENABLED`, and every call site is `#[cfg(kernel_smp_shared)]`. Verified
compiling clean (clippy included) on `--release`, `release-smp-shared` with `smp-shared` only, and
`release-smp-shared` with `devbox-smoltcp,no-tests,bkl-profile`.

### 18.3 Naming the two in-kernel holders

The first post-fix run answered the headline question and immediately raised another: `irq/sched`
collapsed to **12.1%** — and **66.0%** landed in `unknown`.

That is honest, and it is a real answer rather than the "profiler is off" placeholder: `unknown`
means *the BKL was held by a thread that never passed a tagging site*. Two such threads exist
structurally, both long-lived kernel threads with no syscall or fault entry point:

- the **async-main smoltcp poll loop** (`src/main.rs`), which holds the BKL across its whole drain;
- the **idle loops** (`idle_halt`'s post-WFI bookkeeping plus the `yield_now()` that follows, and
  the secondary bootstrap loop in `src/smp_shared.rs`).

Pre-fix these inherited whatever their core last did, which is precisely how they fed the
`irq/sched` bucket. Two new tags name them (`HOLD_TAG_IDLE` 502, `HOLD_TAG_NETPOLL` 503) — the
minimum needed to make the measurement *actionable* rather than trading one uninformative bucket
for another. Also named: syscall 301 (Akuma's `SPAWN`), which read as a bare `nr301` and shows up
on `[BKL] stuck` lines under this regimen.

### 18.4 Result — matched A/B, SMP=4, 2026-07-31

Both sides: `SNAPSHOT=1 DISK=devbox.img MEMORY=4096 SMP=4`,
`--profile release-smp-shared --features devbox-smoltcp,no-tests,bkl-profile`, the unmodified
`net4 → read4 → cp2 → rm` regimen (`scripts/bkl_smp_regimen/`), driven by `drive.py`. `SNAPSHOT=1`
so both boots start from **byte-identical disk state** (writes discarded) — §16.3 had to take a
fresh clone by hand for this reason. The only difference between the two sides is profiler source.

Attribution restricted to the workload windows per §17.2, via the new
`scripts/bkl_smp_regimen/analyze_workload.py --auto`, which derives the interval from the
regimen's own `execve` footprint in the serial log and rounds outward to window boundaries. It is
reproducible and self-adjusting — the two boots reached the regimen at different uptimes (T=8 s
and T=34 s), which a hand-picked `t=40..100` would have silently mis-sliced.

| holder | pre-fix (HEAD profiler) | post-fix | delta |
|---|---|---|---|
| `irq/sched` | **88.8%** | **23.0%** | **−65.8pp** |
| `netpoll` (async-main) | *not expressible* | **59.7%** | new |
| `execve` | 2.5% | 6.0% | +3.5pp |
| `clone` | 2.1% | 3.5% | +1.4pp |
| `nanosleep` | 1.0% | 2.7% | +1.7pp |
| `read` | 0.1% | 1.5% | +1.4pp |
| `mmap` | 1.5% | 1.1% | −0.4pp |
| `ppoll` | 1.5% | 1.1% | −0.4pp |
| `idle` | *not expressible* | 0.6% | new |
| `openat` | 2.1% | 0.6% | −1.5pp |

| | pre-fix | post-fix |
|---|---|---|
| workload windows | 9 (t=10–90 s) | 9 (t=40–120 s) |
| attributed spins | 163,353,234 | 136,902,871 |
| regimen wall-clock | 90 s | 92 s |
| digests (4 net + 2 cp) | **6/6 exact** | **6/6 exact** |
| PANIC / WILD / SPURIOUS | 0 / 0 / 0 | 0 / 0 / 0 |
| stale dropped-window heals | 0 | 0 |
| `[BKL] stuck` | 0 | 18 (all in the final window — see below) |

The pre-fix side independently reproduces `BKL_PROCESS_CARVE_OUT.md` §9.8's **88.4%** at
**88.8%**, on a different boot with a different harness path — good evidence that the window
selection here matches the one that produced the number being audited.

**The decomposition is separable.** A third run (thread-scoping only, before §18.3's two tags)
read `irq/sched` **12.1%** with `unknown` at **66.0%**; adding the tags resolved that 66% into
`netpoll` 59.7% + `idle` 0.6%. So the two changes can be read independently: thread-scoping is what
moved the mass off `irq/sched`, and naming the in-kernel holders is what made the destination
legible. (`irq/sched` reads 12.1% on that boot and 23.0% on the matched one — run-to-run variance
under a profiler that perturbs by design. Both are far from 88%.)

**Prediction stated before measuring, and how it fared.** The prediction was: if the residual bleed
is real, `irq/sched` falls and the long-tail holders (`execve`, `clone`, bulk ext2 writes) rise.
Direction confirmed on every count — `irq/sched` −65.8pp, `execve` +3.5pp, `clone` +1.4pp, `read`
+1.4pp. But the *magnitude* went somewhere the prediction did not anticipate, because it could not:
the dominant recipient is a holder the old profiler had no bucket for at all.

Two honest caveats on the post-fix numbers:

- The 18 `[BKL] stuck` all land in the **t=120 s window**, the regimen tail; every other workload
  window is 0, and the other post-fix boot logged 0 across the entire boot. Same shape as §9.8's
  own caveat. This is measurement-build perturbation at the tail (the new code adds two relaxed
  stores per context switch *while profiling is on*), not a behavioural change — the shipping
  builds compile all of it out, and `--release` is byte-unaffected.
- `netpoll` is a **thread-granularity** label. `THREAD_TAG` persists on the async-main thread, so
  everything that thread does BKL-held is credited to it — the smoltcp drain, but also the herd
  output/heartbeat polls and `bkl_profile::maybe_dump` itself. The dump runs once per 10 s and
  costs microseconds, so it cannot account for 60%; the drain plausibly can, on a regimen that
  moves 128 MiB over the NIC. But "netpoll" names the thread, not exclusively the poll.

### 18.5 Does Phase 3's `irq/sched` premise survive? No.

**It does not.** `irq/sched` is **23.0%** of workload cross-core BKL wait, not 88.4%. The scheduler
and IRQ path are not the dominant remaining cost on this regimen, and a Phase 3 aimed at them on
the strength of the 88% figure would have been spent on the wrong subsystem — which is exactly what
this audit existed to check.

What the corrected numbers say instead, by the same evidence-led rule that picked every target so
far: the single largest holder is the **async-main kernel thread at 59.7%**, holding the BKL across
its poll drain on the BSP while four cores run a network- and I/O-heavy workload. That is a
different kind of target from anything in this campaign — not a syscall wrapper to guard, but a
long-lived kernel loop's hold discipline. Note it already drops the BKL and `WFI`s once per
iteration (`src/main.rs`, added precisely because holding it starved peers); the question the
numbers now pose is whether the *drain itself* should run BKL-free, which is a `NetBklGuard`-shaped
question about smoltcp's own locking, not a scheduler question.

Nothing was carved this session, per scope. In particular the device-IRQ path's unconditional
`enter_kernel()` (§9.1) was left alone: it is the mechanism the whole dropped-window-ledger story
rests on, and it should be decided on trustworthy numbers — which now exist.

Also worth re-stating before any future scheduler work: M5c steps 1 and 2 already landed
(`smp-shared.md` §Status) — the run-queue lock is split out and the scheduler runs BKL-free on EL0
preemption (`sched_bklfree_el0`, default ON since 2026-07-24). `irq/sched` was never greenfield,
and at 23% it is no longer the headline either.

### 18.6 Tests

- **Host** (`crates/akuma-exec/src/sync.rs`, `thread_tag_tests`): six tests over a replay model of
  the three operations against a private core cache. `tag_survives_preemption_and_resume` is the
  bug itself as an assertion; `switch_installs_incoming_threads_own_tag` covers the half §16.2.1
  declined to guess; `irq_without_switch_restores_interrupted_tag` covers the half it fixed, now
  falling out of the same single rule. Plus isolation, and the storage contract (defaults, bucket
  clamping, reset for recycled slots, out-of-range tids inert). 155 akuma-exec tests green.
- **Boot self-test** (`test_smp_shared_holder_tag_follows_thread`, SMP=4, `smp-shared`): stamps a
  sentinel tag, crosses the scheduler 8 times and dwells ~3.5 timer ticks, and requires the tag to
  survive in both the thread's entry and whatever core it resumes on. PASSED. Note the transient
  IRQ stamp is *not* asserted on — while the dispatch runs, the asserting loop is not executing, so
  it is structurally unobservable from the same thread.

### 18.7 Reproducing

```bash
./venv/bin/python scripts/bkl_smp_regimen/gen_payload.py /tmp/bklpay
( cd /tmp/bklpay && python3 -m http.server 8899 --bind 127.0.0.1 & )
SNAPSHOT=1 DISK=devbox.img MEMORY=4096 SMP=4 cargo run --profile release-smp-shared \
    --features devbox-smoltcp,no-tests,bkl-profile > run.log
# once port 2222 answers:
( cd scripts/bkl_smp_regimen && ../../venv/bin/python -u drive.py 1500 )
python3 scripts/bkl_smp_regimen/analyze_workload.py --auto run.log
```

Both kernels were hash-verified distinct before booting, and the booted `.bin` was byte-searched
for a string only the post-fix image contains (`netpoll`) — the §17 stale-`.bin` discipline. This
A/B differs in *source*, so it is structurally immune to that trap anyway (§17.1).

---

## 19. Decomposing `netpoll` (59.7%): the drain, isolated — and why it's a real carve candidate — 2026-07-31

§18.4 flagged its own biggest caveat before this section closed it: `netpoll` is a
**thread-granularity** label. `THREAD_TAG` persists on the async-main thread for its whole
iteration, so the smoltcp burst-drain, the top-of-loop heartbeat/pstats/reclaim housekeeping, and
the herd output/exit polling were all credited to one bucket. §18.5 called the drain "plausibly"
the bulk of it, on the strength of one independent, non-circular hint — the `PreemptGuard` doc
comment's "async-main poller ... spins on `NETWORK` near-constantly" — and explicitly declined to
act on "plausibly." This section resolves it to a measurement.

### 19.1 Splitting the label

Four sub-tags were added to the `HOLD_TAG_NETPOLL` family in `crates/akuma-exec/src/sync.rs`
(free buckets, per §18.3's numbering: 500 fault, 501 irq, 502 idle, 503 netpoll, 511 unknown):

| tag | value | covers |
|---|---|---|
| `HOLD_TAG_NETPOLL_MAINT` | 504 | top-of-iteration: heartbeat/pstats logging, `reclaim_terminated_slots`, `bkl_profile::maybe_dump` |
| `HOLD_TAG_NETPOLL_DRAIN` | 505 | the `while smoltcp_net::poll() {}` burst-drain itself |
| `HOLD_TAG_NETPOLL_MEMMON` | 506 | the (default-disabled) mem-monitor future's poll |
| `HOLD_TAG_NETPOLL_HERD` | 507 | herd supervisor output/exit-code polling |

`HOLD_TAG_NETPOLL` (503) stays as the family's generic fallback — it now only ever labels the
sliver between re-acquiring the BKL post-WFI and the next loop-top `MAINT` call, a few
instructions.

Each sub-tag is installed with **`set_holder_tag`, not `set_core_tag_transient`** — every one of
these phases genuinely belongs to the async-main thread (unlike the IRQ dispatch stamp, which
belongs to no thread and must not clobber the interrupted one). Using the transient form here
would have been the exact mistake §18.2 built the two-table split to prevent, just introduced
fresh. `src/bkl_profile.rs`'s `tag_label` gained the four matching labels
(`netpoll_maint`/`netpoll_drain`/`netpoll_memmon`/`netpoll_herd`).

Same discipline as §18: every call site `#[cfg(all(kernel_smp_shared, feature = "smoltcp"))]` (or
narrower, matching the block it's in), every accessor an early-return no-op unless
`PROFILE_ENABLED`, zero behavioural change on any build. Verified compiling clean (clippy
included) on `--release`, `release-smp-shared --features smp-shared`, and `release-smp-shared
--features devbox-smoltcp,no-tests,bkl-profile`. `crates/akuma-exec`'s host `sync::` suite (27
tests, including all six `thread_tag_tests`) stays green — the sub-tags reuse `set_holder_tag`
unchanged, they don't touch its contract.

### 19.2 Result — single instrumented run, SMP=4, 2026-07-31

`SNAPSHOT=1 DISK=devbox.img MEMORY=4096 SMP=4`, `--profile release-smp-shared --features
devbox-smoltcp,no-tests,bkl-profile`, the unmodified `net4 → read4 → cp2 → rm` regimen, attribution
restricted to the workload windows via `analyze_workload.py --auto` (§17.2/§18.4's method). This is
a single boot, not a matched A/B: the question here is "how does an existing bucket split," not
"did a profiler change perturb the numbers," so §18.4's two-boot discipline doesn't apply — same
reasoning as why §18's own thread-scoping fix needed a matched pair and this doesn't.

```
selection: auto: regimen execve T=214..295s -> windows t=220..300s
windows: 9  (w21 t=220s .. w29 t=300s)
total contended spins: 138521182   attributed: 138521182

   share  holder            tag  spins
   59.8%  netpoll_drain     505  77421466
   23.4%  irq/sched         501  30285803
    4.5%  execve            221  5881295
    2.9%  clone             220  3692354
    2.5%  openat             56  3209854
    2.4%  mmap              222  3151739
    1.6%  nanosleep         101  2055512
    1.1%  netpoll_maint     504  1469708
    0.9%  ppoll              73  1140984
    0.3%  writev             66  340382
    0.2%  netpoll_herd      507  262808
    0.2%  idle              502  218627
```

`netpoll` (503, the residual fallback) doesn't place in the top 12 at all — confirming the sliver
it now covers really is negligible. `irq/sched` reads 23.4%, matching §18.4's matched-pair 23.0%
closely enough to confirm this run is measuring the same thing the earlier one did.

**The decomposition is not close.** `netpoll_drain` alone is 59.8% — essentially the *entire*
prior 59.7% `netpoll` figure — while `netpoll_maint` (1.1%) and `netpoll_herd` (0.2%) are both
noise. §18.5's "plausibly" is now a measurement: the drain isn't part of the story, it *is* the
story, to within run-to-run variance.

Correctness: 6/6 digests exact (4 net + 2 cp), 0 PANIC/WILD/SPURIOUS/stale-dropped-window, regimen
completed in 92s wall-clock. 14 `[BKL] stuck` and 73 `RECOVERED` lines appear in the raw log, but
both are pre-workload boot noise, not workload signal — see §19.4.

### 19.3 Auditing the drain: does it already have its own lock?

Yes. `crates/akuma-net/src/smoltcp_net.rs::poll()` — the function the `while` loop in
`src/main.rs` calls up to 64 times per iteration — already wraps its entire body in exactly the
pattern the VFS and net carve-outs rely on:

```rust
let _pg = PreemptGuard::new();
let mut guard = NETWORK.lock();
```

Every piece of state `poll()` touches lives behind that one lock: `net.iface` (the smoltcp
interface), `net.device` (the `VirtIONetRaw` wrapper — no separate device lock is needed because
the whole `NetworkState` struct, device included, sits behind the single `NETWORK` spinlock), the
DHCP handle, and `pending_removal`. The one thing `poll()` touches *outside* that lock —
`socket::with_table(...)`'s wake-up pass — is deliberately done **after** `guard` drops, per the
comment already in that function: taking `SOCKET_TABLE` while holding `NETWORK` would AB-BA
against `socket_can_recv_tcp` et al., which take the reverse order. `SOCKET_TABLE` is itself
`PreemptGuard`-protected (same table in §1's precedent list). No new lock would be needed to carve
this — it is *already* the same shape §1 describes for VFS: "every piece of state the [subsystem]
touches already carries its own fine-grained lock."

There is also no separate device-interrupt path to worry about: `grep` for an IRQ handler
registration touching net device state outside `poll()` finds none — the only `irq::register_handler`
call in the tree is the timer (IRQ 27, `src/main.rs:945`). The virtio-net IRQ line has no handler
of its own; it exists only to make the `wfi` in the main loop return promptly (the comment at the
WFI call site is explicit about this). All net-device state changes happen synchronously inside
`poll()`'s `NETWORK`-locked section — there is nothing for a carve-out to race against.

**The AB-BA risk the `PreemptGuard` doc warns about does not apply to this carve, and here is why,
concretely rather than by assertion.** The doc's warning describes a core running a `no-bkl-*`
critical section (holding `NETWORK`) that takes a nested IRQ, whose `enter_kernel()` hard-spins for
the BKL while `NETWORK` is still held — deadlocking against whichever core owns the BKL if *that*
core is itself spinning to acquire `NETWORK`. Today the async-main poller cannot be the *victim* of
that shape (it holds the BKL throughout, so its own nested IRQs take the reentrant fast path,
never spin) — but it is explicitly named as the likely *culprit*, spinning on `NETWORK` while
holding the BKL, wedging any BKL-free-and-`NETWORK`-holding syscall core that gets interrupted.

If the drain is carved with `NetBklGuard`'s own mechanism (`akuma_exec::bkl::dropped_window_open`/
`close` around the `while` loop, `#[cfg(all(kernel_smp_shared, kernel_no_bkl_network))]` — the
exact gate every `src/syscall/net.rs` syscall already uses), the poller stops being a BKL-holding
`NETWORK`-waiter altogether: it becomes one more `NETWORK` contender running under the identical
protocol every net syscall already runs under, `PreemptGuard` included. **The load-bearing detail
is that `PreemptGuard`'s IRQ-masking arm is gated on `no-bkl-network`/`no-bkl-vfs`, not on
`smp-shared` alone** (`crates/akuma-exec/src/sync.rs`, the `saved_daif` field and `irq_save_mask()`
call are both `#[cfg(... any(feature = "no-bkl-network", feature = "no-bkl-vfs"))]`). That is what
makes a `NETWORK`-holder's own nested IRQ a non-event today for every existing BKL-free syscall,
and it is what would make the drain's carve safe by the same mechanism, *provided the carve is
gated on `kernel_no_bkl_network` specifically* (not merely `kernel_smp_shared`, which happens to
imply it today only because `Cargo.toml`'s `smp-shared` feature bundles `no-bkl-network` in —
`smp-shared = [..., "no-bkl-network", "no-bkl-vfs", "no-bkl-process"]`). Gate on the coincidence
and a future feature split silently reintroduces the deadlock; gate on the actual dependency (the
same way `NetBklGuard` already does) and it can't.

### 19.4 Tangential finding: `[BKL] stuck`/`RECOVERED` in this run are pre-workload, not workload

Not investigated further (out of scope — these are the two "standing signals" the task explicitly
carried forward, not assigned to this session), but worth recording precisely since the numbers are
easy to misread. All 14 `[BKL] stuck` lines in this run's log carry the same `[TMR] t=93000`
timestamp, tagged `openat` (56) and Akuma's `SPAWN` (301) — i.e. they cluster in one moment during
boot/service bringup, at uptime t=93s, well *before* the auto-selected workload window (t=220–300s).
None involve `netpoll`/`netpoll_drain`. Likewise `RECOVERED` (73 this run, vs. the 31–57/run range
noted previously) is counted over the whole log by `analyze_workload.py --auto` — its "stability
inside the workload only" comment is aspirational for the `--auto` path: `region` is bound to the
*entire* log text there, not sliced to `lo..hi`, so these two counts are whole-boot, not
workload-window numbers, despite the neighboring print implying otherwise. Manually checking against
`[TMR]` timestamps (as done here) is currently the only way to tell the two apart. This is a
pre-existing characteristic of the analysis script, not something this session's change touched;
flagged for whoever next relies on those two counts.

### 19.5 Recommendation

**Carve, following `NetBklGuard`'s existing shape exactly.** The dominant sub-phase (`netpoll_drain`,
59.8%, effectively all of the prior `netpoll` figure) already has every piece of state it touches
behind its own `PreemptGuard`-protected lock (`NETWORK`, transitively `SOCKET_TABLE`), matching the
VFS precedent's carving condition precisely: nothing new to lock, only a BKL window to stop holding.
The specific deadlock the `PreemptGuard` doc warns about is closed by gating the carve on
`kernel_no_bkl_network` (not `kernel_smp_shared`), the same way every existing net syscall already
is — that is not a new mitigation invented for this carve, it is reusing the one already proven in
production for `sys_recv`/`sys_send`/`sys_accept`/etc.

The two smaller sub-phases should **not** be carved, on the opposite finding from §1's condition:
- `netpoll_maint` (1.1%) touches `reclaim_terminated_slots`, `dump_running_process_stats`, and
  `dump_thread_resume_points` — process/thread-table code in the same family
  `BKL_PROCESS_CARVE_OUT.md` already audited and left alone (`fork_process`, §9 there) because it
  relies on the BKL itself for exclusivity, not a fine-grained lock underneath it. Small *and*
  unsafe to carve without first giving that state its own lock — a materially bigger job than this
  one, and not evidence-led at 1.1%.
- `netpoll_herd` (0.2%) touches `ProcessChannel`, which — checked while auditing, not assumed — is
  already fully self-contained (`Spinlock`-protected buffer/poller set, `AtomicBool`/`AtomicI32`
  exit state; `crates/akuma-exec/src/process/channel.rs`), so it is *not* unsafe to carve, just not
  worth it: 0.2% is below the noise floor this campaign has been treating as "done" since §15.

Nothing was carved this session, per scope — this is a recommendation, not a change. If Phase 4
picks this up, the concrete unit of work is: wrap the `while smoltcp_net::poll() {}` loop in
`src/main.rs` with `akuma_exec::bkl::dropped_window_open()`/`dropped_window_close()` (or a thin
`NetBklGuard`-alike reusing that pair, mirroring `src/syscall/net.rs`'s struct), gated
`#[cfg(all(kernel_smp_shared, kernel_no_bkl_network))]`, and verify with the same three-part
regimen this campaign has used throughout: boot self-test correctness, a controlled A/B for
contention (this section's 59.8% is the number that A/B should move), and the 6/6 digest check this
section already ran once.

### 19.6 Reproducing

Same as §18.7 — the sub-tags are unconditionally present in the tree now, no separate flag needed:

```bash
./venv/bin/python scripts/bkl_smp_regimen/gen_payload.py /tmp/bklpay
( cd /tmp/bklpay && python3 -m http.server 8899 --bind 127.0.0.1 & )
SNAPSHOT=1 DISK=devbox.img MEMORY=4096 SMP=4 cargo run --profile release-smp-shared \
    --features devbox-smoltcp,no-tests,bkl-profile > run.log
# once port 2222 answers:
( cd scripts/bkl_smp_regimen && ../../venv/bin/python -u drive.py 1500 )
python3 scripts/bkl_smp_regimen/analyze_workload.py --auto run.log
```

---

## 20. Carving the drain — DONE, contention-confirmed, 2026-08-01

§19.5's recommendation, carried out: the `while smoltcp_net::poll() {}` block in `src/main.rs`
now runs BKL-free, using `NetBklGuard`'s own mechanism directly.

### 20.1 The change

```rust
#[cfg(all(kernel_smp_shared, kernel_no_bkl_network))]
akuma_exec::bkl::dropped_window_open();
let mut polls = 0u32;
while akuma_net::smoltcp_net::poll() {
    polls += 1;
    if polls >= 64 {
        break;
    }
}
#[cfg(all(kernel_smp_shared, kernel_no_bkl_network))]
akuma_exec::bkl::dropped_window_close();
```

Gated on `kernel_no_bkl_network` specifically, not `kernel_smp_shared` alone — §19.3 explained why:
that is the cfg that makes `PreemptGuard::new()` mask IRQs for the inner `NETWORK`/`SOCKET_TABLE`
holds inside `poll()`, which is what keeps a nested IRQ from ever observing this core "holding
`NETWORK`, wanting the BKL." No new lock was introduced — every piece of state the drain touches
was already behind one (§19.3). **Already default-on**: `Cargo.toml`'s `smp-shared` feature bundles
`no-bkl-network` in (`smp-shared = [..., "no-bkl-network", "no-bkl-vfs", "no-bkl-process"]`, made
default per the `enable fixes by default` commit), so any `smp-shared` build gets this carve for
free — no new feature flag was added or needed.

Zero-cost when off: `dropped_window_open`/`close` are no-ops outside
`cfg(all(kernel_smp_shared, target_os = "none"))` (`crates/akuma-exec/src/bkl.rs`), and the whole
block compiles out entirely on non-`smoltcp` builds.

### 20.2 Verification — build + host tests

Compiles clean (clippy included) on `--release`, `release-smp-shared --features smp-shared`, and
`release-smp-shared --features devbox-smoltcp,no-tests,bkl-profile`. `crates/akuma-exec`'s full
host suite — 155 tests, including `bkl::` (the `DroppedWindowLedger` contract this carve depends
on) and all `sync::thread_tag_tests` — stays green; nothing in the carve touches ledger internals,
it only calls the existing public `dropped_window_open`/`close` pair.

### 20.3 Verification — boot self-test (correctness), SMP=4

Booted `release-smp-shared --features devbox-smoltcp` (self-tests **on**, `no-tests` off,
`SNAPSHOT=1 DISK=devbox.img MEMORY=4096 SMP=4`). The two tests that exercise the exact mechanism
this carve now relies on both passed:

- `smp_shared_dropped_window_survives_irq` **PASSED** (18 eret(s) preserved the window) — the
  ledger correctly keeps the BKL released across a nested IRQ landing mid-window, which is the
  scenario a device IRQ hitting the drain between `poll()` calls now exercises for real (previously
  only `NetBklGuard`'s syscall callers exercised it).
- `smp_shared_holder_tag_follows_thread` **PASSED** — unaffected; confirms the carve didn't disturb
  attribution.
- `smp_shared_cooperative_wait` and `smp_shared_blocking_wait_peer_progress` both **PASSED**,
  explicitly reporting "no BKL deadlock" / "no BKL freeze" under deliberate cross-core BKL pressure
  — these run *before* `run_async_main` starts (confirmed against call order in `kernel_main`), so
  their large `[BKL] stuck` cluster (owner=1, tag=511/unknown, dozens of lines) is that test's own
  by-design contention stress, not the carve; it is identical in kind to stuck clusters seen in
  every boot this session and is unrelated to `netpoll_drain`.
- One unrelated pre-existing failure: `fs_error_to_errno_mapping`'s `PermissionDenied -> EPERM`
  case expects `-1`, but `src/syscall/fs.rs` deliberately maps `PermissionDenied` to `EACCES` (-13)
  — a documented, intentional choice ("Linux uses EACCES for filesystem permission errors... EPERM
  is reserved for capability-style 'operation not permitted'"); the test is stale, not a regression.
  Also `stp_xzr_ec15_handler_fires` failed with a QEMU EC-generation quirk the test itself already
  flags. Neither touches networking/BKL code and both predate this session's changes.
- Post-self-test steady state (45 s observed, sshd `accept` climbing steadily, TMR ticks
  continuous): 0 `[BKL] stuck`, 0 WATCHDOG, 0 panics.

### 20.4 Verification — controlled A/B (contention), SMP=4, `bkl-profile`, 2026-08-01

Same unmodified `net4 → read4 → cp2 → rm` regimen, `SNAPSHOT=1 DISK=devbox.img MEMORY=4096 SMP=4`,
`release-smp-shared --features devbox-smoltcp,no-tests,bkl-profile`, two back-to-back boots (not
reusing §19.2's numbers, to control for host-machine variance across the day boundary): BEFORE ran
with the two `dropped_window_open`/`close` calls compiled out (`cfg(feature =
"TEMP_AB_BASELINE_DISABLE")`, an undeclared feature — always false, warns but doesn't error), AFTER
with them restored. Only that one diff between sides.

| | BEFORE | AFTER | delta |
|---|---|---|---|
| `netpoll_drain` share | 57.2% | **absent from top 15** | **−57.2pp** |
| `netpoll_drain` spins | 78,776,143 | 0 (doesn't place) | — |
| total contended spins | 144,575,777 | **47,280,739** | **−67.3%** |
| `irq/sched` | 25.5% (35.15M) | 29.6% (12.38M) | share up (smaller pie), absolute spins down 65% |
| digests (4 net + 2 cp) | 6/6 exact | 6/6 exact | — |
| `[BKL] stuck` | 12 | 12 | unchanged |
| `RECOVERED` | 52 | 78 | both pre-workload (see below) |
| PANIC / WILD / SPURIOUS | 0 / 0 / 0 | 0 / 0 / 0 | — |

`netpoll_drain` doesn't place in the top 15 tags at all post-carve — the same "drops out entirely"
signature every successful carve in this campaign has produced (`unlinkat`, `renameat`,
`mkdirat`/`fchmodat`). The **67.3% cut in total workload spinning is the largest single change this
campaign has measured** — larger than any prior carve, because the thing being removed was, per
§19, roughly 3/5 of all contended BKL time on this regimen.

Both sides' `[BKL] stuck` (12/12) and `RECOVERED` (52/78) lines were checked against `[TMR]`
timestamps the same way as §19.4: every stuck/recovered line on both sides clusters at a single
pre-workload timestamp (t≈93s before, t≈446s after — the AFTER boot idled longer before the
regimen started, hence the later absolute time), well outside the auto-selected workload window on
either side. Not workload signal, not a regression — same caveat as §19.4, now confirmed to hold on
both sides of this specific A/B.

### 20.5 Status

**Shipped and default-on.** No Cargo.toml change was needed — the carve rides the same
`no-bkl-network` gate `NetBklGuard` already uses, and that gate has been part of the default
`smp-shared` bundle since the `enable fixes by default` commit. Non-`smp-shared` and non-`smoltcp`
builds are byte-for-byte unaffected (both cfg arms compile to nothing).

Nothing else in the `netpoll` family is being carved: `netpoll_maint` (process/thread-table code,
no fine-grained lock underneath — §19.5) and `netpoll_herd` (safe but below the noise floor) stand
as recommended against in §19.5.
