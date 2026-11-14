# What the SGI scheduler left behind (2026-11-11)

**Status: closed (2026-11-11).** The dead code is removed. The three behaviours that went
with the SGI scheduler were measured on the current scheduler and accepted for the merge (see
"Measured"). Improving wakeup latency under load is
[`proposals/WAKEUP_LATENCY_UNDER_LOAD.md`](../../proposals/WAKEUP_LATENCY_UNDER_LOAD.md).

## Background

`e85f53dd` made the call-switch ("hooked") scheduler the AArch64 default, and `cd1d8f17`
deleted the SGI scheduler it replaced: the vector table, the handlers, the trampolines and the
per-tick self-SGI. Code that only the SGI path called stayed behind, still compiling, because
it was `pub` (the `dead_code` lint does not check `pub` items) or because it was a `static` that
something still wrote.

The pre-merge sweep found it by listing every `pub` item whose name appears nowhere else in the
tree outside comments, then following the compiler's `dead_code` warnings through 11 build
configurations ([`AKUMA_FEATURE_AUDIT.md`](AKUMA_FEATURE_AUDIT.md) § "Second pass").

## Removed

`akuma-threading`:
- **`ThreadPool::schedule_indices`** (~300 lines), the SGI picker, plus what only it used:
  `commit_switch`, the per-core `VOLUNTARY_SCHEDULE` flags and `request_voluntary_reschedule`,
  `DISPLACE_DEFER`, `SGI_DEBUG_ONCE`/`set_sgi_debug`, and the pool's `current_idx`,
  `network_boost_counter` and `round_robin_idx` fields.
- **The run-next hint**, `PREEMPT_WAKE_TID`, and its three arms: `WAKEUP_LOCALITY_HINT` (off),
  `WAKE_DEADLINE_PREEMPT` and `SIGNAL_WAKE_PREEMPT`. The signal arm still stored into the hint;
  nothing read it.
- SGI-era accessors and stubs with no caller: `adopt_current_as_core_idle` (the call switch uses
  `adopt_core_idle`), `get_saved_kernel_resume`, `test_swap_saved_ctx_ttbr0`,
  `validate_current_sp`, `check_all_stack_canaries`, `check_stack_overlaps`,
  `get_stack_bounds`, `has_ready_threads`, `dump_stack_info`, `reserve_slot` (an old
  Taoxichuan crossing), and four thread-count/state getters.

`akuma-switch`: `thread_start_addr`, `thread_start_closure_addr` and the two panic stubs they
pointed at.

`akuma-exec`: the `process::diag` module. Its borrow tracker counted `lookup_process_shared`
calls and never decremented (no call site released a borrow), so it could only flood the
console; it was compiled off and a boot test asserted it stayed off. Its lock-hold timers had
no callers. Also `with_as_locked`, `kill_process_group` (the console uses
`kill_foreground_group`), `record_mmap_region`, `remove_mmap_region`, `remove_lazy_region`,
`is_in_lazy_region`, `get_stack_bounds`, `get_process_channel`, `set_args`, `take_stdout`,
`fd_table`.

Elsewhere, no caller anywhere: six `akuma-net` socket accessors, `akuma-virtio`'s
`transmit_begin`/`transmit_complete` and two helpers (the NIC transmits through
`akuma-net-nic`'s arena path), `akuma-mmu`'s `map_kernel_block_2mb` and
`translate_current_user_va`, `akuma-exceptions`'s `default_exception`, and single helpers in
`akuma-dma`, `akuma-kernel-core`, `akuma-locks-rw`, `akuma-net-unix`, `akuma-pipe`,
`akuma-pmm`, `akuma-primitives`, `akuma-syscalls-net` and `akuma-vfs`.

Kept on purpose: hardware register vocabulary with no caller (EHCI/xHCI/HID/NVMe/RTL8169/
I2C/HDA/SVM fields and constants, `AT_*`, `MAIR_*`, PSCI ids, `akuma_cpu::set_fpcr`). Each
models a spec or an instruction, costs nothing, and is what a future driver reaches for.

## Removed from the lock and its configuration (second commit)

The SGI path was also the only caller of the lock's eret-time reconcile:
- `akuma_bkl::bkl::{reconcile_for_spsr, reconcile_for_spsr_no_ticket}` and the
  "window preserved" counter they kept (`dropped_windows_preserved`, printed by `[BKLPROF]`
  as `windows_preserved=` and therefore always 0 since);
- `KernelLock::{reconcile, reconcile_no_ticket, acquire_no_ticket}` and the per-core
  `barged` flags `release` consulted. The ticket-free "barge" acquire existed only for the
  BKL-free EL0-preempt scheduler path; with it gone, every hold is ticketed and `release`
  always advances `now_serving`. The three host tests of the barge went with it;
- two `akuma_bkl::policy` toggles whose only reader was the SGI IRQ handler:
  `irq_bkl_drop_enabled` (`no-bkl-irq`) and `sched_bklfree_el0_enabled`. Boot tests still
  flipped them, testing nothing. Five phase toggles remain, each with a production reader;
- the scheduler config fields `network_thread_ratio` and `prioritize_never_scheduled`
  (and `config::{NETWORK_THREAD_RATIO, PRIORITIZE_NEVER_SCHEDULED}`), which every kernel,
  Taoxichuan's included, filled in and nothing read.

The interrupt path today takes the lock only if the interrupted code did not hold it and
drops only what it took, so a dropped window survives an interrupt without any eret-time
logic. The dropped-window ledger stays; the idle loop and the EL0-entry tripwire read it.

## Three behaviours that went with the SGI picker

All three lived in `schedule_indices`. The current picker, `pick_next`, is plain round-robin
with no hint and no boost, so a thread that becomes runnable waits its turn.

| fix | what it did | what is left |
|---|---|---|
| `WAKE_DEADLINE_PREEMPT`, 2026-08-18 ([`SCHEDULING_INVESTIGATION.md`](SCHEDULING_INVESTIGATION.md)) | an expired sleeper ran on the next switch, removing a ~tick × runnable-threads floor on `nanosleep`/`poll` | the faster tick (3 ms today) |
| `SIGNAL_WAKE_PREEMPT`, 2026-08-28 ([`PTHREAD_KILL_EINTR_DELIVERY_STARVATION.md`](PTHREAD_KILL_EINTR_DELIVERY_STARVATION.md)) | a signal-woken thread ran on the next switch | `SIGFRAME_ACTIVE`, the half that took the probe from 9/10 to 15/15 |
| `NETWORK_THREAD_RATIO` boost, 2026-07-18 ([`RUMP_SYSPROXY_LATENCY_FIX.md`](RUMP_SYSPROXY_LATENCY_FIX.md) Phase 2a) | the registered network thread (netpoll, or `rump_server` under `rump-default`) was preferred every 4th tick | `set_network_thread_id` still registers it, which now only names it in thread dumps |

## Measured (2026-11-11)

`ncaprobe sleepbench` (1 ms request, median of 40) and `pthread_kill_eintr`, on `da505df7`.
The full table and its caveats are in
[`proposals/WAKEUP_LATENCY_UNDER_LOAD.md`](../../proposals/WAKEUP_LATENCY_UNDER_LOAD.md).

| | idle | 3 CPU hogs | 7 hogs | `pthread_kill_eintr` |
|---|---|---|---|---|
| AArch64 HVF SMP=1 | 1.27 ms | 4.29 ms | — | 20/20 |
| AArch64 HVF SMP=4 | 1.31 ms | 1.00 ms | 2.02 ms | 20/20 |
| amd64 trashcan (metal, 4 cores) | 1.01 ms | 10.0 ms | 11.5 ms | flaky on this kernel and on the one before it |

- **`WAKE_DEADLINE_PREEMPT`:** AArch64 SMP=1 under load went from the fix's ~1.06 ms to
  4.3 ms. That is round robin at a 1 ms tick. It is far from the original ~35 ms floor, and
  SMP≥2 is unaffected. amd64 never had this fix: its picker was always plain round robin, and
  a kernel trashcan ran before this branch measured 6–24 ms under the same load.
- **`SIGNAL_WAKE_PREEMPT`:** no regression on AArch64. `SIGFRAME_ACTIVE` carries the probe.
  amd64's flakiness predates this branch and is a separate signal-delivery problem, listed
  in the proposal.
- **`NETWORK_THREAD_RATIO`:** not measured (rump devbox, deferred). The scheduler no longer
  reads `set_network_thread_id`. It remains only to name the thread `network` in dumps, and
  the source-shape tests guarding its registration went with this closure.

Accepted for the merge. Restoring sub-tick wakeups under load, on both architectures, is the
proposal's job: a deadline timer on busy cores, a kick for halted cores, and a per-core
run-next hint in `pick_next`.

## Verification

- `cargo check` with `-W dead_code` in the 11 configurations listed in the audit: no
  warnings.
- clippy (9 targets), the per-crate host clippy loop, host tests.
- AArch64 boot suite under HVF at SMP=1/2/4: 0 `[Test]` failures, the same PASSED set as
  before minus the deleted borrow-tracker test.
- amd64 TCG self-test: SMP=1 829/0, SMP=2 839/0.
