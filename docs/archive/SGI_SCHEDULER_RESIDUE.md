# What the SGI scheduler left behind (2026-11-11)

**Status:** the dead code is removed. Two behaviours that went with the SGI scheduler are
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

## Open: two fixes that went with the SGI picker

Both were run-next hints read only by `schedule_indices`. The current picker, `pick_next`, is
plain round-robin with no hint, so a thread that becomes runnable waits its turn.

| fix | what it did | what is left |
|---|---|---|
| `WAKE_DEADLINE_PREEMPT`, 2026-08-18 ([`SCHEDULING_INVESTIGATION.md`](SCHEDULING_INVESTIGATION.md)) | an expired sleeper ran on the next switch, removing a ~tick × runnable-threads floor on `nanosleep`/`poll` | the faster tick (3 ms today) |
| `SIGNAL_WAKE_PREEMPT`, 2026-08-28 ([`PTHREAD_KILL_EINTR_DELIVERY_STARVATION.md`](PTHREAD_KILL_EINTR_DELIVERY_STARVATION.md)) | a signal-woken thread ran on the next switch | `SIGFRAME_ACTIVE`, the half that took the probe from 9/10 to 15/15 |

Neither was re-measured when the call switch became the default. What to run:
- the `nanosleep`/`sleep_ms(1)` floor from `SCHEDULING_INVESTIGATION.md` § Evidence, at
  SMP=1 with three runnable threads;
- `smp1.ex.pthread_kill_eintr` from `scripts/verify_trim.py`.

If either regressed, the fix belongs in `pick_next`. A hint there has to work with the
`ON_CPU` claim and with several cores scanning at once, which a single global slot did not
need to under the SGI picker.

## Verification

- `cargo check` with `-W dead_code` in the 11 configurations listed in the audit: no
  warnings.
- clippy (9 targets), the per-crate host clippy loop, host tests.
- AArch64 boot suite under HVF at SMP=1/2/4: 0 `[Test]` failures, the same PASSED set as
  before minus the deleted borrow-tracker test.
- amd64 TCG self-test: SMP=1 829/0, SMP=2 839/0.
