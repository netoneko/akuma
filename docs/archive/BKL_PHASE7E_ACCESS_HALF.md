# Phase 7e ("Access" half): retire `lookup_process`/`current_process`

**Status**: Landed 2026-08-01 (uncommitted at time of writing). No feature flag,
no runtime toggle, no carve-out guard — behaviour-preserving per site, with the
BKL still in place. This is the second half of 7e in
[`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md) §5; the "Free" half
([`BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md`](BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md))
landed the same day.

## 1. What changed

`lookup_process(pid) -> Option<&'static mut Process>` and
`current_process() -> Option<&'static mut Process>` are **deleted**, along with
the dead `diag::lookup_process_tracked`/`BorrowGuard`. Every one of the ~250
production call sites (plus ~140 in the boot-test files) now uses one of:

| accessor | for | notes |
|---|---|---|
| `lookup_process_shared(pid)` / **new** `current_process_shared()` | reads, `&self` methods (fd table, `vm_*`, `with_as_locked`, `Arc` fields) | the M5b fault-path precedent (`children.rs`), extended everywhere |
| `table::with_process(pid, f)` / **new** `with_current_process(f)` | short plain-field writes and scalar copies | IRQ-masked closure; MUST NOT allocate (moving a pre-built value in, or letting the replaced value drop, is fine) |
| **new** `Process::with_address_space(&self, f: FnOnce(&mut UserAddressSpace) -> R)` | the `&mut self` page-table mutators (`unmap_page*`, `update_page_flags*`, `map_page`/`map_and_track`, `try_evict_ro_page`, …) | holds `as_lock` + IRQ mask on `smp-shared` (plain call elsewhere), interior mutability per `vm_with_regions`'s discipline; replaces every open-coded `#[cfg(kernel_smp_shared)] let _asg = AsLockHold::new(&proc.as_lock)` + `proc.address_space.…` pair |
| **new** `table::with_process_exclusive(pid, f)` — `unsafe` | the enumerated lifecycle windows only | no lock, no IRQ mask; exclusivity is structural (own process, BKL-held path). Exactly two call sites: `sys_execve`'s `replace_image*` tail and `spawn::run_registered_process`'s first entry to user mode — the execve/clone-class destructive windows Phase 7f owns. Do not add call sites casually. |

Also `&self`-ified because their bodies only touch interior-locked state:
`Process::{read_stdin, write_stdout, take_stdout}` (Arc'd `Spinlock<StdioBuffer>`),
`Process::set_brk` (page installs via `with_address_space`, the `brk` scalar
store under `vm_lock`).

Why: two cores could each materialize `&'static mut` to the same `Process`
(aliasing UB — `BKL_PHASE7_AUDIT.md` §2.1.2), and the `'static` lifetime
structurally outlives the RETIRED→FREE deferred reclamation the Free half
introduced. Cross-core exclusion for plain fields still comes from the BKL —
this phase is the prerequisite for 7f, not a carve-out.

The migration was done subsystem-at-a-time with a full SMP=2 boot-suite +
live-SSH verification against a same-day HEAD baseline after each batch
(`vfs/proc` → `syscall/mem` → `syscall/fs` → `syscall/proc`+small files →
`akuma-exec process/*` → `exceptions.rs`+rest → tests), so any regression
would have been pinned to one batch. Every batch matched the baseline exactly.

## 2. Incidental gap closed: unlocked PTE edits on the signal paths

Three sites edited live PTEs through `&mut Process` with **no `as_lock` hold at
all** (the same gap class the `no-bkl-mm` audit closed for `sys_mmap`'s reclaim
sweep): `ensure_cow_page_writable` and `try_resolve_el1_cow_fault`
(EL1-write CoW breaks: map_page + retrack), and `try_deliver_signal`'s
handler/restorer RX fix-up (`update_page_flags`). All three now go through
`with_address_space`, so they exclude a concurrent BKL-free fault on the same
address space like every other PTE mutator.

## 3. The fd-release defect (§3b's class, rediscovered for kill paths) — FIXED

`cleanup_process_fds` gated the shared-table close on
`Arc::strong_count(&proc.fds) == 1`. That test was correct while
`unregister_process` freed a `Process` synchronously — a dead CLONE_THREAD
sibling's `Arc` clone disappeared with it. The Free half deferred that drop
(RETIRED slots wait ≥10ms for `reclaim_retired_processes`, which during the
synchronous boot self-test phase never runs at all), so an **externally killed
multithreaded group** (`kill -9` → `kill_thread_group` +
`kill_process_with_signal`) never saw the count reach 1: its pipes/sockets were
released only by the deferred collector. A peer blocked on such a pipe hangs
exactly like §3b's `yes | head`. (Single-threaded kills were unaffected —
count 1 — and `sys_exit_group` closes its own table unconditionally, which is
why the ordinary exit path never showed this.)

Fix: `cleanup_process_fds` now counts **live sharers** — not-yet-`exited`,
still-ACTIVE processes sharing the same `Arc<SharedFdTable>`, via
`for_each_process` + `Arc::ptr_eq` (RETIRED slots are invisible to the scan by
design; kill paths mark `exited`/retire each member before its group's last
cleanup runs). Only `Process` structs hold long-lived clones of these tables
(fork deep-copies; only CLONE_FILES shares), so the scan is authoritative, and
`close_all` stays idempotent so the later unconditional closes are harmless.
Honors the Free half's §3 lesson: this is still the killer's own
`LifecycleGuard`-serialized context calling `close_all` — no `Process::drop`
runs anywhere new.

Boot self-test: `test_external_kill_closes_shared_fds`
(`src/process_tests.rs`) — synthetic leader + CLONE_THREAD sibling sharing one
table holding a pipe read end, killed through the real
`kill_thread_group` + `kill_process_with_signal` flow; asserts the table is
empty and `pipe_write` fails (EPIPE) immediately after the kill, while the
test itself deliberately holds an extra `Arc` clone to pin count-independence.
Under the old gate this fails by construction (the RETIRED sibling's clone
alone keeps the count ≥ 2).

`return_to_kernel`'s `already_terminated` skip of `cleanup_process_fds` is
sound after this: every marker of TERMINATED (the `kill_*` family,
`unregister_process` after a normal exit) either runs the corrected cleanup
itself or follows the victim's own unconditional `close_all`.

## 4. Verification

- **Boot suite** (`release-smp-shared --features devbox-smoltcp`,
  `DISK=devbox.img MEMORY=4096 INSTANCE=60`):
  - SMP=1: 341 PASS / 2 known FAIL / 0 `[BKL] stuck` / 0 PANIC.
  - SMP=2: 349 PASS (348 + the new test) / 2 known FAIL / ~22 stuck / 0 PANIC —
    **byte-identical counts to a same-day HEAD baseline boot** (348/2/22).
  - SMP=4: 346 PASS + 2 by-design SKIPs (`smp_shared_{exec,fault}_parallelism`
    run only at SMP=2) / 2 known FAIL / ~65 stuck / 0 PANIC — HEAD baseline:
    346/2/67. One run of three flaked `test_epoll_multi_poller_pipe`
    (`woken=1 expected 2`, passed on re-run) — SMP=4 timing flake, watch item.
  - The 2 standing FAILs are the pre-existing `PermissionDenied -> EPERM` and
    `stp_xzr_ec15_handler_fires` (environmental).
  - The `[BKL] stuck` lines are **pre-existing on HEAD** at both SMP=2 (22) and
    SMP=4 (67), measured by baseline boots during this session — not from this
    phase.
- **Live SSH** (userspace sshd, port 8222 for INSTANCE=60): file I/O, procfs
  (`ps` parses `/proc/<pid>/stat`), `cd`/`pwd` (the `with_current_process` cwd
  write), `yes | head -n 1` (pipe+SIGPIPE), `kill -9` of a background job,
  fork/exec — all correct at SMP=2 and SMP=4.
- **Host tests**: full workspace `cargo test` green (478+ tests, 0 failed).
- **Clippy**: clean on default and `release-smp-shared --features devbox-smoltcp`.
- **Builds**: `release`, `release-smp --features smp` (multikernel),
  `release-smp-shared` all compile. `scripts/build_size.sh` /
  `build_extreme_size.sh` fail on HEAD too (pre-existing dead
  `pipe_write_all_blocking` under `--no-default-features`, from the committed
  Free-half pipe work) — not this phase.

## 5. What this unblocks, and what it doesn't

Both 7e halves are now landed, which is the audit's precondition for 7f
("Nothing about removing the BKL from syscall entry should be attempted before
both halves land"). It does **not** move any syscall off the BKL, and it does
not change that same-core field races on a live ACTIVE process are still
BKL-serialized — `with_process`'s IRQ mask is single-core exclusion; the
per-syscall opt-in traversal (7f) and the `execve`/`clone` conversions remain,
with `with_process_exclusive`'s two call sites as their explicit worklist.

## Background

- [`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md) §2.1/§2.1.2/§5 — scoping; the
  migration shape this phase executed.
- [`BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md`](BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md)
  — the "Free" half; §3/§3b are the two deadlocks whose lessons shaped §3 here.
- [`BKL_PROCESS_CARVE_OUT.md`](BKL_PROCESS_CARVE_OUT.md) §9.2 — why there is no
  `PROCESS_TABLE_LOCK` in this design either.
- [`../reference/subsystems/locking.md`](../reference/subsystems/locking.md) —
  current-state reference; updated alongside this doc.
