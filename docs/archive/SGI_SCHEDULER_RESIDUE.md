# What the SGI scheduler left behind (2026-11-11)

**Status:** the dead code is removed. Three behaviours that went with the SGI scheduler are
recorded as **open**, unmeasured on the current scheduler.

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

## Open: three behaviours that went with the SGI picker

All three lived in `schedule_indices`. The current picker, `pick_next`, is plain round-robin
with no hint and no boost, so a thread that becomes runnable waits its turn.

| fix | what it did | what is left |
|---|---|---|
| `WAKE_DEADLINE_PREEMPT`, 2026-08-18 ([`SCHEDULING_INVESTIGATION.md`](SCHEDULING_INVESTIGATION.md)) | an expired sleeper ran on the next switch, removing a ~tick × runnable-threads floor on `nanosleep`/`poll` | the faster tick (3 ms today) |
| `SIGNAL_WAKE_PREEMPT`, 2026-08-28 ([`PTHREAD_KILL_EINTR_DELIVERY_STARVATION.md`](PTHREAD_KILL_EINTR_DELIVERY_STARVATION.md)) | a signal-woken thread ran on the next switch | `SIGFRAME_ACTIVE`, the half that took the probe from 9/10 to 15/15 |
| `NETWORK_THREAD_RATIO` boost, 2026-07-18 ([`RUMP_SYSPROXY_LATENCY_FIX.md`](RUMP_SYSPROXY_LATENCY_FIX.md) Phase 2a) | the registered network thread (netpoll, or `rump_server` under `rump-default`) was preferred every 4th tick | `set_network_thread_id` still registers it, which now only names it in thread dumps |

None was re-measured when the call switch became the default. What to run:
- the `nanosleep`/`sleep_ms(1)` floor from `SCHEDULING_INVESTIGATION.md` § Evidence, at
  SMP=1 with three runnable threads;
- `smp1.ex.pthread_kill_eintr` from `scripts/verify_trim.py`;
- the rump sysproxy round trip from `RUMP_SYSPROXY_LATENCY_FIX.md`, on the rump devbox
  (deferred; needs `RUMP_NIC=1`).

If any regressed, the fix belongs in `pick_next`. A hint there has to work with the `ON_CPU`
claim and with several cores scanning at once, which a single global slot did not need to
under the SGI picker.

## Verification

- `cargo check` with `-W dead_code` in the 11 configurations listed in the audit: no
  warnings.
- clippy (9 targets), the per-crate host clippy loop, host tests.
- AArch64 boot suite under HVF at SMP=1/2/4: 0 `[Test]` failures, the same PASSED set as
  before minus the deleted borrow-tracker test.
- amd64 TCG self-test: SMP=1 829/0, SMP=2 839/0.
