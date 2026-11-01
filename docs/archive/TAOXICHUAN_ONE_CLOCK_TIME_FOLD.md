# Taoxichuan: one clock, the time family on glue, and the timed park that never expired (2026-11-01)

**Stability: B**: verified on the Mac TCG rig; not yet booted on the ryzen (KVM) or in a fixed point.

## What changed

Taoxichuan had two clocks. The scheduler and `akuma-exec` read `exec_twin::uptime_us`, the LAPIC tick count times
10 ms. The time syscalls (`clock_gettime`, `gettimeofday`, `nanosleep`) were local arms that read the TSC
(`net::uptime_us`). Glue's time family reads `akuma_primitives::clock::uptime_us`, which `akuma_exec::runtime::register`
points at the `ExecRuntime` clock, so folding those arms would have handed programs the 10 ms tick clock
(`TAOXICHUAN_CLEANUP_PLAN.md`, the note on `56b69679`).

Now there is one clock:

- `net::calibrate()` runs in `kernel_main` right after `lapic_init` (whose TSC measurement is the fallback when there is
  no PIT) and **before** `exec_twin::init`. Registered first, the clock would read `rdtsc / 1` until calibration and
  then jump backwards under every deadline already computed against it.
- `exec_twin` hands `net::uptime_us` to both `ExecRuntime` and `ThreadRuntime`; `exec_twin::uptime_us` is deleted.
- `served_by_glue` gains `clock_gettime`, `clock_getres` (one of the tokio gaps in
  `AKUMA_SYSCALLS_TOKIO_DEP_ANALYSIS.md`), `clock_settime`, `adjtimex`, `clock_adjtime`, `nanosleep`,
  `clock_nanosleep`, `prlimit64` and `getrlimit`. `ExecConfig::user_stack_size` is the 8 MiB stack ceiling
  (`STACK_MAX`), which is what glue's `prlimit64(RLIMIT_STACK)` reports; nothing else reads that field on this kernel.
- The x86-only `gettimeofday` (96) and `time` (201) moved into `akuma-syscalls-glue-amd64`, over the same
  `akuma_primitives::clock` reads. amd64 has its own copies of those two arms in `usermode.rs` and does not call the
  crate yet (cleanup plan step 2c-B2).
- `akuma-syscalls-abi` has a `Getrlimit` row (x86_64 97, asm-generic 163).
- Deleted: the four local arms, `child::sleep_us`, `net::sleep_until`, `struct_bytes` (one `unsafe` site; budget
  35 -> 34).

Behaviour that changed: `nanosleep` parks through the scheduler instead of spinning with a NIC pump per lap, has no
60 s cap, and a `tkill` ends it with `EINTR`. `gettimeofday` without an `epoch=` anchor answers boot-relative time
instead of zeros (glue's `CLOCK_REALTIME` answer). `RLIMIT_NOFILE` reports 1024 instead of unlimited, the other
kernels' answer.

## The bug the fold exposed

The first boot never answered ssh. sshd's accept loop sleeps 1 ms between non-blocking `accept`s, and it sat in
`nanosleep` forever (`[hang] s1 IN nr=35`).

Cause: on x86_64, a `WAITING` thread whose deadline has passed is made `READY` only by `x86_wake_pass`, which runs
inside `x86_yield`. Taoxichuan's idle loop yielded only when `x86_runnable_exists()`, which a due sleeper is not. So a
timed park on an otherwise idle core never expired. The timer tick yields only when it interrupts ring 3, so the bug
hid whenever anything else was running. The old local `nanosleep` spun instead of parking, which is why it never
showed. Every glue timed wait on this kernel had the same exposure (a `poll`/`epoll_pwait` timeout with nothing else
runnable).

Fix (`exec_twin::idle_main`): yield when `akuma_threading::wake_is_due(x86_earliest_wake_time(), net::uptime_us())`,
too. That is the question amd64's wait loops already ask (`amd64/src/net.rs`). A parked deadline is now served at the
next tick, so its resolution is 10 ms on an idle core.

## Verified

Mac TCG rig (`scripts/kvm/taoxichuan/tcg_rig.py`): battery 10/10 + goose PONG, `battery toolchain` (rustc, cargo
build with a build script), 0 console faults. In the guest: `sleep 2` measured 2.09 s of `/proc/uptime`, including
the fork and exec; `ulimit -s` 8192. Host tests of `akuma-syscalls-abi` and `akuma-syscalls-glue-amd64`; amd64
(`no-tests`) and the AArch64 default build compile.

Open: not run on the ryzen under KVM, where the TSC rate comes from the LAPIC calibration rather than the PIT. The
fixed point was not rerun; this change does not touch exec, fork or mm.

## Background

- [`TAOXICHUAN_CLEANUP_PLAN.md`](TAOXICHUAN_CLEANUP_PLAN.md): the X2 clock note on `56b69679` and the status row.
- [`AKUMA_AMD64_C3_CLOCK.md`](AKUMA_AMD64_C3_CLOCK.md): amd64's own move of the time family onto glue.
- [`UDP_REPLY_WAIT_TIMER_STORM.md`](UDP_REPLY_WAIT_TIMER_STORM.md): why `wake_is_due` exists.
