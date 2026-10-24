# The Firecracker guest hang: a one-shot lost to a race, and a `poll` that kept stale `revents`

**Date:** 2026-10-24. **Kernel:** `kvm-part-1` at `421f84bf`..`f43dee19`, ryzen metal (Akuma host, Akuma guest under unmodified Firecracker
1.16 on `/dev/kvm`, a tap from `/dev/net/tun`). **Status:** fixed; verification at the end.

This is the "guest wedges ~10 s into network activity" of [`AKUMA_FIRECRACKER_KVM.md`](AKUMA_FIRECRACKER_KVM.md) § *The guest wedge under
Firecracker + `/dev/net/tun`*. That section's seven theories were all wrong: it was not the NIC, the tap, the memfd mapping, the
virtio-net header or an rx edge. Two **guest-kernel** bugs, both on the path of a blocking `recvfrom` / `poll` with a timeout, produced
two different "the guest never reaches its probe lines" pictures.

## Symptoms

Firecracker plus `vm-net.json` (a NIC), guest script ends with `echo ... | nc -u -w 2 10.0.2.2 7`. Before the fixes, about one run in three
and then every run stopped before `udp probe:` was printed:

* **A, dead.** The guest stops printing altogether. Host: `vcpu idle ... rflags=0x...2xx due_in_us=none can_wake=no`, LAPIC
  `lvt_timer=0x20` (one-shot, unmasked) with a nonzero initial count that never changes, PIC fully masked. The vCPU thread is asleep, the
  guest is not.
* **B, alive and useless.** The guest keeps printing its DNS retries, `[kbd] polls=` climbs, the host sees a normal tick, but `nc` never
  exits: `ps` shows it `R` with `utime` climbing and `stime` 0.

## Method (what found it)

* **An event ring in the host** (`amd64/src/kvm/api/vcpu.rs`, `evring`): the last 160 of {entry: guest `rflags`, offered vector, V_INTR,
  time to the next timer event; exit: code, rip, info; each emulated LAPIC store; each accepted vector}, in 5 KiB of statics, dumped
  once by `idle_note` when a guest halts with `due == None && !can_wake` (the lost-interrupt signature). The earlier tracing kept the first 24
  exits and then one in 8192, which cannot show a window a few microseconds wide.
* **`strace_pid=<tgid>` with time and arguments** (`usermode.rs`): the `[sc>]`/`[sc]` lines now carry `a2`, `a3` and `uptime_us`. B was
  one `poll` returning after the right 2.1 s and one `read` returning after the kernel's 10 s cap.
* The guest ELF is not stripped, so `nm` + `objdump -d` on the mac turned the ring's addresses into `arm_deadline+0x257` etc.

## A. The lost one-shot (`lapic::arm_deadline` with `IF` set)

The guest's idle `hlt` is armed with a one-shot at the earliest parked deadline (`arm_deadline`: `restore_periodic()`, read `TIMER_CUR`,
write LVT = one-shot, write the initial count, **then** `ONESHOT.store(true)`), and the tick handler `on_tick` ends with
`restore_periodic()`, which does nothing unless that flag is set. `net_blocking_relax` → `sched::allow_tick` calls it as
`bkl_leave(); arm_deadline(); sti; hlt; cli; restore_periodic()`.

The ring for the stuck cycle:

```
-2928 EXIT  NPF  rip=arm_deadline+0x1e2  gpa=0xfee00390   (read TIMER_CUR; guest rflags 0x46: IF clear)
-2015 ENTRY rflags=0x246                                   (IF now SET)
-2007 LAPIC write 0x320 = 0x20        (LVT: one-shot)
-1913 LAPIC write 0x380 = 0xb984      (initial count: ~0.76 ms)
 -903 ENTRY rflags=0x246 offered=0x20 vintr=0x2001100100 due_in=0   (the shot has already expired)
 -893 EXIT  NPF  rip=on_tick+0x138  gpa=0xfee000b0     (the handler's EOI)
 -893 ACCEPT vector=0x20
   -5 ENTRY ... ; -0 EXIT HLT rip=net_blocking_relax+0x216      ← back at the same hlt, no restore writes in between
```

In every healthy cycle the EOI is followed by `0x3e0`, `0x320 = 0x20020`, `0x380 = …` (`start_timer`: the periodic tick coming back).
Here nothing follows it: `on_tick`'s `xchgb ONESHOT` read **0**.

So the tick landed **between the count write and the flag store**, with interrupts enabled. Nothing is supposed to enable them
there (the comments on `arm_deadline`/`allow_tick` say the caller runs masked), but `bkl_leave()` in `allow_tick` restores the flags the BKL was
taken under, and the guest's blocking-syscall path took it with `IF` set; the ring's `rflags=0x246` is the proof. On bare metal the window
is a handful of instructions wide and a 0.8 ms shot cannot expire in it; under SVM the count write is a `#VMEXIT` (an NPF to the LAPIC
page), the host yields between entries (`run_inner`'s `yield_now`, which is the cooperative kernel's only way for the device thread to run),
and a millisecond passes. The handler found the flag clear and returned without restoring the periodic tick; `arm_deadline` then set the flag,
`sti; hlt` slept on a one-shot that had already fired, and nothing in the guest had a deadline any more. A one-count shot
(`counts.max(1)`, which `net_blocking_relax` itself documents arming "every lap") has the same race on metal.

**Fix.** `arm_deadline`, `arm_oneshot_in_us` and `arm_idle_shot` run their whole check-and-program sequence inside
`akuma_primitives::irq::with_irqs_disabled` (`pushfq`/`cli`/`popfq`), so the flag and the registers change together whatever the caller's `IF`
was; the interrupt that was pending is taken at the `popfq`, after the flag is set, and restores the periodic tick. Not done: finding out why
`allow_tick` is entered with `IF` set (it leaves `IF` clear after the `cli`, so only the arm is exposed) — recorded as open below.

## B. `poll(2)` on timeout left the previous `revents` in the caller's array

The strace of the stuck `nc` (guest time in seconds):

```
25.07 connect(3)                      27.23 read(3) -> 16 (the echo reply)     27.52 read(0) -> 0 (stdin EOF)
25.98 poll(fds, 2, 2000) -> 1         27.85 shutdown(3)                         28.17 write(1, 16)
26.91 poll(fds, 2, 2000) -> 2         28.49 poll(fds, 2, 2000) -> 0   at 30.61 (the 2000 ms timeout, correct)
30.81 read(3) .......................................................  -> -ETIMEDOUT at 40.92 (the kernel's 10 s UDP cap)
41.14 read(0) -> 0 ; shutdown(3) ; poll(…, 2000) -> 0 at 44.0 ; read(3) ... and again, forever
```

busybox `nc` reads whichever of its two fds has a `revents` bit set. After a **timeout** it should have seen none, and exited. The glue
`ppoll` (`akuma-syscalls-glue/src/poll.rs`; `sys_poll` and `sys_ppoll` on both kernels end there) returned `Ok(0)` on `TimedOut`
**without copying its scan back**, so the user's array still held the previous call's `POLLIN`, which `nc` took at face value: a blocking `read`
on a UDP socket with nothing to say, ten seconds, `ETIMEDOUT`, loop. Linux writes `revents` for every entry on every call. (This is
the family of [`UDP_REPLY_WAIT_TIMER_STORM.md`](UDP_REPLY_WAIT_TIMER_STORM.md), whose `nc -u -w 2` repro it is: that fix made the second
`recvfrom` park correctly, and the second `recvfrom` should not have happened.)

**Fix.** On `TimedOut` the kernel copies `kernel_fds` (every `revents` zero, from the last lap) back before returning 0.
Regression: `epollops` probe 7b `poll_timeout_clears_revents` (poll a pipe holding a byte, drain it, poll the same array with a 50 ms
timeout, expect `r == 0 && revents == 0`; it fails on the old kernel with "left the previous call's revents in the array" and is
expected `PASS` on Linux).

## Not the cause

* The tap, the gateway, DHCP/ARP/UDP echo: the reply reached `nc` (`read(3) -> 16`).
* `num_buffers` in the virtio-net header: `tun::read_frame` now writes 1 (a `MRG_RXBUF` VMM treats 0 as invalid; theory 4), a correct but
  unrelated change.
* memfd aliasing, virtio feature negotiation, an rx edge (theories 2, 3, 5): the guest was demonstrably receiving.
* The control that misled: "the same guest with no NIC sleeps 20 s fine" — no NIC means no blocking socket wait, so neither bug's path ran.

## Verification

* Reproduced and caught: with the unfixed kernel as the guest the ring dump fired on 1 run in 6 (A); the other five stopped at the probe (B).
* With both fixes (`f43dee19` as the guest, host `1182a007` unchanged, Firecracker rig `/root/fc/fcnet.sh`, 90 s cap per run):
* `epollops` on the ryzen's unfixed kernel: `FAIL poll_timeout_clears_revents`, the only failure.

**Result: 42 of 42 clean** (12, then a 30-run soak; each run ≈ 28 s to `AKUMA-GUEST-DONE` with `udp probe: akuma-net-probe` and `tcp echo: akuma-tcp-probe`), no ring dump, no stuck probe. Before: 6 runs on the unfixed kernel gave 1 dump (A) and 5 stuck probes (B); the older guest kernel (`ff4695f`) gave 6 stuck probes, and a 12-run loop with only fix A gave 12 stuck probes (B), which is how B was separated from A.

## Open

* Why `allow_tick` reaches `arm_deadline` with `IF` set (`bkl_leave` restoring the flags the lock was taken under). Harmless now; the
  comments that say "callers run masked" are wrong for this caller.
* `UDP_REPLY_WAIT_TIMER_STORM.md` §Still open: `allow_tick` itself could run the wake pass.
* Passing runs before the fix existed (about one in three): they did not take the stale-`revents` path, for a reason not pinned down.
  Both fixes are independent of it.

## Background

[`AKUMA_FIRECRACKER_KVM.md`](AKUMA_FIRECRACKER_KVM.md) (the wedge section this resolves), [`UDP_REPLY_WAIT_TIMER_STORM.md`](UDP_REPLY_WAIT_TIMER_STORM.md),
[`AKUMA_AMD64_LAPIC_ONESHOT_STALE.md`](AKUMA_AMD64_LAPIC_ONESHOT_STALE.md) (the earlier one-shot-left-armed bug in the same
functions), [`../reference/subsystems/kvm.md`](../reference/subsystems/kvm.md).
