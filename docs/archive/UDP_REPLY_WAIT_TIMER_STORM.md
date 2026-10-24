# `nc -u -w 2` hangs after a UDP reply: a halting wait loop never runs the wake pass

2026-10-22. amd64 guest kernel (plain QEMU, TCG; also seen under the SVM hypervisor and on the metal).

## Symptom

busybox `nc -u -w 2 <ip> <port>` against a peer that answers with a datagram never exits. strace
ends at `poll(2000 ms) -> 0`; `ps` shows `nc` in state `R` with `utime` climbing and `stime` 0; QEMU's
monitor and gdbstub stop answering. A silent port, a TCP echo and slirp's DNS (10.0.2.3:53) do not
stall. Repro: `scripts/kvm/udp_stall_repro.sh LOG` (5 of 5 stalls before the fix, 0 of 34 after).

## Cause

Not user mode. `-d int` on QEMU showed ring-0 timer interrupts all landing in
`net::net_blocking_relax` (and `sched::allow_tick`). After its poll timeout `nc` issues a second,
blocking `recvfrom` (`socket_recv_udp`, 10 s `wait_until`), which parks by `net_blocking_relax` ->
`allow_tick`: drop the BKL, `sti; hlt`, `restore_periodic`.

`allow_tick` arms the halt with `lapic::arm_deadline`, which reads
`x86_earliest_wake_time()` — the earliest deadline of *any* `WAITING` thread. The wake pass that turns
a due deadline into `READY` (`x86_wake_pass`) runs only inside `x86_yield_now`, and this loop never
yields. So a sleeper whose deadline had passed (the netpoll daemon and friends) stayed `WAITING`
with a past deadline; `arm_deadline` saw "already due" and armed a 1-count one-shot every lap; the
halt returned at once; the loop spun at interrupt rate and the sleeper never ran. Kernel code is
not preempted on this target, so nothing else could break the cycle. It needed a reply because that
is what puts `nc` into the second blocking `recvfrom` while the daemon's deadline is expiring.

## Fix

`net_blocking_relax` asks `akuma_threading::wake_is_due(x86_earliest_wake_time(), uptime_us())` first
and, if so, `yield_now()`s (which runs the wake pass and lets the sleeper run) instead of halting.
The predicate is a pure function with a host test in `akuma-threading`.

## Verification

* `scripts/kvm/udp_stall_repro.sh`: `no stall: udp host echo: akuma-probe` 34 of 34 runs.
* `cargo test -p akuma-threading wake_due`, `scripts/check_unsafe_budget.py`: green.

## Correction 2026-10-24

The "second, blocking `recvfrom`" in § Cause was not `nc` design: `nc` only reads an fd whose `revents` is set, and the timed-out `poll` left the previous call's `POLLIN` in the array. With `poll(2)` zeroing `revents` on timeout (`FIRECRACKER_GUEST_HANG.md` § B) `nc -u -w 2` exits at the timeout and never reaches that `recvfrom`. The wake-pass fix above stands: the blocking wait is still reachable by other callers.

## Still open

Other halting loops (`allow_tick` callers outside `schedule_blocking`) have the same shape if
they ever skip the yield path; `allow_tick` itself could run the wake pass. Not changed here.
