# Phase 7c: `sys_openat`'s BKL-held residual — a guard opened too late

**Status**: Landed 2026-08-01. No new feature flag — rides the existing
`no-bkl-vfs`/`kernel_no_bkl_vfs` gate `VfsBklGuard` has used since Phase 2b.
**Toggle**: `crate::smp_shared::vfs_bkl_drop_enabled()` (already existed; this
phase only moves where the same guard opens).

This is 7c of the decomposition in [`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md)
§5: "re-audit `sys_openat`'s guard placement specifically: 10.5% for a
converted syscall's prologue/epilogue is high enough that either the window
starts too late or the re-acquire is costing more than expected. Measurement
first, not code." Executed per
[`../runbooks/bkl-phase7-workplan.md`](../runbooks/bkl-phase7-workplan.md).

## 1. The measurement, before touching any code

`sys_openat` (`src/syscall/fs.rs`) already carries a `VfsBklGuard` — Phase 2b's
own carve-out — but the guard was constructed at line 1442, well into the
function, past several things that run under the BKL for every call:

1. `raw_path`/dirfd resolution (fd-table read, self-process only — cheap).
2. `crate::vfs::resolve_symlinks(&path)` — up to 8 iterations of
   `read_symlink`, and `read_symlink` calls `with_fs(path, |fs, rel|
   fs.read_symlink(rel))`: a **real ext2 lookup** (mount-table resolve, inode
   walk) for any path, symlink or not, because `is_symlink`/`read_symlink` have
   to consult the on-disk inode to find out.
3. The `/dev/null`/`/dev/urandom`/`/dev/zero`/`/dev/dsp`/`/dev/net/tap0`
   fast-path checks and the `/proc/self/exe` rewrite (self-process only,
   cheap).
4. The `#[cfg(kernel_smp)]` multikernel cross-core forward arm (never compiled
   into a binary where this guard does anything — see §2).

Item 2 is the one that matters: it is real ext2 I/O, done under the BKL,
before the guard that exists specifically to make ext2 I/O BKL-free ever
opens. The comment that used to sit above the old guard call claimed
`resolve_symlinks` on "simple absolute paths" did "none [I/O] worth
unserializing" — that claim is the thing the audit asked to be re-checked, and
it does not hold: `read_symlink`'s on-disk lookup runs for every path openat
resolves, not just ones that turn out to be symlinks.

The fix was checked against precedent before being written, not assumed: two
sibling syscalls in the same file already open their `VfsBklGuard` **before**
calling `resolve_symlinks`, and are unmodified by this phase:

- `sys_fchmodat` (fs.rs:1906 guard, :1912 `resolve_symlinks`) — comment at
  :1900 already names this exact hazard ("`resolve_symlinks` does a real
  on-disk symlink-target lookup").
- `sys_newfstatat` (fs.rs:1777 guard, :1807 conditional `resolve_symlinks`,
  plus `is_symlink`/`read_symlink`/`metadata` calls inside the same window at
  :1793-1841).

So the pattern this phase applies to `openat` was already running in
production for two other syscalls with no reported issue — this is bringing
`openat` into line with its siblings, not inventing a new carve shape.

## 2. What changed

`src/syscall/fs.rs`, `sys_openat`: `let _vfs_bkl = VfsBklGuard::new();` moved
from after `resolve_symlinks`/the dev-node checks/the multikernel forward arm
to immediately **before** `resolve_symlinks` — i.e., right after path
resolution completes. Being a single local RAII binding, every return path
after that point (including all of the dev-node fast-path early returns, which
previously ran before the guard existed) now closes the guard automatically on
drop; no per-return-site bookkeeping was needed.

One ordering question had to be checked, not assumed: the `#[cfg(kernel_smp)]`
multikernel forward arm sits between `resolve_symlinks` and the old guard
position, and its own comment insists the guard must open *after* it ("marshals
through the BKL-protected bounce and must keep the lock"). `VfsBklGuard`'s body
only exists under `cfg(all(kernel_smp_shared, kernel_no_bkl_vfs))`, and
`build.rs` asserts `smp` (which gates `kernel_smp`) and `smp-shared` (which
gates `kernel_smp_shared`) are mutually exclusive — so in every binary where
this guard's `new()`/`drop()` compile to anything but a no-op, the multikernel
forward arm is not compiled in at all. The two code paths never coexist in one
binary, so moving the guard's source position ahead of that arm cannot wrap
live forwarding code with a live guard in any buildable configuration.

No new lock, no new guard type — this is a placement fix to an existing
carve-out, exactly the shape the audit called for.

## 3. Boot self-test

`test_openat` (`src/process_tests.rs`) gained an 8th case: create a real
on-disk symlink via `crate::vfs::create_symlink`, `openat()` it through the
real `handle_syscall(nr::OPENAT, …)` entry point, and verify the returned fd's
content matches the symlink target — pinning that `resolve_symlinks` running
inside the (now earlier) dropped-BKL window still resolves and opens the
target correctly, not just that the guard stays balanced.

## 4. Verification

- **Clippy**, all three configs, clean: `--release`; `--profile
  release-smp-shared --features smp-shared`; `--profile release-smp-shared
  --features devbox-smoltcp,no-tests,bkl-profile[,no-bkl-irq]`.
- **Host tests**: `cargo test -p akuma-exec` — 156 passed, 0 failed (unchanged;
  `fs.rs`/`process_tests.rs` are bin-crate-only, so this phase's regression
  coverage is the boot self-test above).
- **Boot self-test suite**, `release-smp-shared --features smp-shared`:
  - **SMP=2**: 0 PANIC/WILD/SPURIOUS, `openat` PASSED (8 cases, including the
    new symlink case), the same 2 pre-existing unrelated failures as every
    prior phase (`PermissionDenied -> EPERM` errno mapping,
    `stp_xzr_ec15_handler_fires` — QEMU-dependent, self-documenting). 19
    whole-boot `[BKL] stuck` lines, 0 RECOVERED/stale heals.
  - **SMP=4** (2 boots): both 0 PANIC/WILD/SPURIOUS, `openat`/`unlinkat`/
    `fchmodat` all PASSED, same 2 pre-existing failures, 0 RECOVERED/stale
    heals on either boot.
- **Same-binary A/B**, SMP=4, `release-smp-shared --features
  devbox-smoltcp,no-tests,bkl-profile`, `MEMORY=4096`, unmodified `net4 →
  read4 → cp2 → rm` regimen (`scripts/bkl_smp_regimen/`), source-toggled per
  playbook rule 5 (the guard's old position vs. its new one, byte-identical
  feature set — built by temporarily reverting just this phase's diff in
  `src/syscall/fs.rs` for the "before" binary):

  | | before (guard after `resolve_symlinks`) | after (guard before it) |
  |---|---|---|
  | `openat` share (workload window) | 13.1% (3.64M spins) | **not in top 12** (≤0.1%) |
  | `openat` share (whole boot) | 1.9% (3.77M spins) | **not in top 12** |
  | regimen wall-clock | 90s | 90s |
  | digests (4 net + 2 cp) | 6/6 exact | 6/6 exact |
  | `[BKL] stuck` | 4 (all tag=511/unknown, boot-time, pre-workload) | 0 |
  | RECOVERED | 2 | 0 |
  | PANIC / WILD / SPURIOUS / stale dropped-window heals | 0 | 0 |

  `openat` drops out of the attribution top-12 entirely — the same "collapses
  toward the noise floor" signature every successful carve in this campaign
  has produced (`no-bkl-network`'s `netpoll_drain`, 7a's `irq/sched`, 7b's
  `ppoll`). The before-side's 13.1% (measured fresh, this session) is
  consistent with — in fact somewhat higher than — the audit's original 10.5%
  estimate, and per the campaign's standing rule this is the number that
  matters (same-session share, not a cross-session absolute count). The
  before-side's 4 `[BKL] stuck` / 2 `RECOVERED` events are boot-time noise
  (`tag=511` = profiler-off/unknown, all before the workload window opens) —
  the same pre-workload pattern every prior phase has documented — not a
  regression; the after-side simply didn't happen to hit that noise this run.

## 5. What's next (7d–7f)

Per `BKL_PHASE7_AUDIT.md` §5: **7d** (`THREAD_CONTEXTS` ownership proof —
either prove `POOL`'s state machine already guarantees "not running on any
CPU," or add per-slot ownership; cheap if the former, and a prerequisite for
anything touching `clone`), then **7e** (process-table locking — the real
blocker: `Process`'s ~40 fields need grouping into locks or single-writer
proofs before the ~274 call sites convert, and the free path needs epoch/RCU
or a cooldown scheme for peer-core teardown), then **7f** (invert the BKL's
default per `BKL_FINE_GRAINED_LOCKING_PLAN.md` §7.3, rather than deleting it).
`execve`/`clone` go last, after 7e, per the audit's ordering rationale.

7c's own residual — `read`'s 0.9-3.7% and `accept`'s 0.1-0.5% (§3 of the
audit) — was not re-examined this session; `openat` was audit-named as the one
"high enough" to warrant a second look, and this phase's evidence (13.1%→noise
floor) already validates the audit's premise that guard placement, not the
carve-out's existence, was the residual's cause here. `read`/`accept` may or
may not have the same class of misplacement; that's for whoever picks up 7c's
remainder to measure before assuming.

---

## Background

- [`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md) — §3 measured the 11.9%
  already-carved residual and named `openat` specifically; §5 is the 7a–7f
  decomposition.
- [`BKL_PHASE7A_TIMER_IRQ_CARVE_OUT.md`](BKL_PHASE7A_TIMER_IRQ_CARVE_OUT.md),
  [`BKL_PHASE7B_PPOLL_CARVE_OUT.md`](BKL_PHASE7B_PPOLL_CARVE_OUT.md) — 7a/7b,
  same playbook, same verification bar.
- [`BKL_VFS_CARVE_OUT.md`](BKL_VFS_CARVE_OUT.md) §12-14 — Phase 2b/2c, where
  `VfsBklGuard` and the `openat`/`fchmodat`/`newfstatat` conversions this
  phase touches originally landed.
- [`../reference/subsystems/locking.md`](../reference/subsystems/locking.md)
  — the playbook and the load-bearing inventory.
- [`../runbooks/bkl-phase7-workplan.md`](../runbooks/bkl-phase7-workplan.md) —
  the work plan this session executed, resuming at 7c.
