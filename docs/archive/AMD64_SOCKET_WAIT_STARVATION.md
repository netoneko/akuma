# Main amd64 kernel: socket waiters that halt starve a READY thread (C4, 2026-11-22)

## Symptom
`scripts/gate.py`'s `tcp_loopback` (accept on one thread, 64 KiB echoed back through a reader thread) timed out on the
main amd64 kernel at SMP=2 in roughly 1 boot in 8, only after the spawn checks had run (`flaky:` in `scripts/gate/xfail.txt`).
Reproduced 2026-11-22 with `scripts/gate.py --no-build --only boot:akuma-amd64-gateprobe` in a loop.

## Cause
Not TCP and not smoltcp. A syscall trace (`strace_nr=44,45,202,...`) of a failing boot shows three user threads (client
writer, echo server, reader) trading 4 KiB reads and writes, then **every syscall stops at T8.40 and resumes at T28.35**: the
instant the probe's own 20 s futex wait times out. Counters in the probe's timeout line (`sent / served / echoed`) were
different on every hit (32768/32768/32768, 61440/36864/32768), so it is a stall at an arbitrary point.

`akuma_net::wait_hooks::blocking_relax` on this kernel pumps and then `halt`s (`allow_tick`: `hlt` until the next tick)
unless `wake_due()` (a sleeper's deadline has passed) or `yield_before_relax()` (only on a wireless link). Ring-0 code is not
preempted on this target, so a thread waiting in `wait_until` never gives up its core. With two cores and three waiting
threads, two of them halt in their wait loops forever and the third, the one whose progress satisfies the other two, is READY
and never picked. It runs again only when the main thread's timeout makes a sleeper due, which turns `wake_due()` true. This is
the same shape as `UDP_REPLY_WAIT_TIMER_STORM.md` (that fix covered a due *sleeper*, not a plain READY thread) and
`TOKIO_KERNELS_LOOPBACK_STARVATION.md` (the same rule on the tokio kernels). It needs more waiting threads than cores, which is
why it showed up only after the spawn checks and never in a lone probe.

## Fix
`crates/akuma-kernel-amd64/src/net.rs`: `SocketWait::yield_before_relax` is now
`wireless_is_link() || akuma_threading::runnable_exists()`, so a waiter yields (a no-op when nothing else can run) before it
halts. No allocation; `runnable_exists` scans the thread-state atomics.

## Verification
Before: `scripts/gate.py --no-build --only boot:akuma-amd64-gateprobe` in a loop timed out on `tcp_loopback` in 2 of 12 boots, and
again within 1, 8 and 5 boots on three later loops. After: 40 consecutive boots, 0 timeouts (about 5 expected at the old rate).
Selftests SMP 1 and 2 and the full akuma/amd64 gateprobe matrix pass. The `flaky: tcp_loopback` line is removed from
`scripts/gate/xfail.txt`; the probe's timeout line keeps the `stage / sent / served / echoed` counters so a recurrence says
where it stopped.

Still open (not this fix): `time` failed in 7 of those 40 boots with `wall clock 5s is before 2020`, i.e. the guest read
seconds-since-boot as wall time although `epoch=<now>` is on the command line; it also appeared before this change.
Recorded as C7 in `proposals/FUTURE_WORK.md`.

Background: [UDP_REPLY_WAIT_TIMER_STORM](UDP_REPLY_WAIT_TIMER_STORM.md),
[TOKIO_KERNELS_LOOPBACK_STARVATION](TOKIO_KERNELS_LOOPBACK_STARVATION.md).
