# amd64 "freeze" under QEMU = LAPIC periodic switch livelocking QEMU's main loop

2026-11-11, branch `akuma-multiarch-attempt-0`.

## Symptom

Every amd64 herd guest under QEMU TCG (`-M microvm`, `SMP=2`, virtio-mmio NIC, slirp `hostfwd`)
eventually stopped answering ssh: after 30-600 s, idle or busy, on every kernel tried, including the
one from before the socket and futex slices. The TCP connect reached slirp, and no banner came back.
QEMU sat at ~100 % of one host core, and its HMP monitor stopped accepting connections. Logged as a
guest freeze in `AKUMA_MULTIARCH_ATTEMPT_0.md` ("Slice 2: futex").

## Investigation, including the wrong turn

Read live through the gdbstub (lldb, `gdb-remote`):

- Both vCPUs sat in `hlt`: CPU 0 in `allow_tick` under `schedule_blocking`'s fallback, CPU 1 in
  `idle_loop`. Two of the first samples caught a vCPU in `akuma_lapic_amd64::start_timer`.
- The guest was **not** stuck. The thread table, dumped twice, showed every `WAITING` slot's
  deadline move from ~246.7 s to ~498.7 s; `idle_loop`'s `LAST_PSTATS_US` advanced; `[PSTATS]` kept
  printing; the netpoll daemon lapped ~95/s; sshd's accept loop kept counting. Only its
  `recvfrom`/`sendto` counts had stopped.
- The NIC probe line (`PROBE_ON` flipped from the debugger) read `rx` flat while `laps` climbed, and
  the receive virtqueue held one posted buffer with `used.flags = NO_NOTIFY`.

**Wrong turn:** that ring state was read as a lost receive notify, and a hand-written `QUEUE_NOTIFY`
from lldb was followed by a working ssh banner. A driver change that re-rang the doorbell after 64
empty polls was written and soaked; the guest hung at ~70 s anyway with thousands of re-rings. On
that guest, ssh answered *before* any manual notify. One buffer posted with `NO_NOTIFY` set is simply
an idle receive queue. The revivals most likely came from the debugger stopping and resuming the VM,
which this doc did not test on its own. The change was
dropped before commit.

**What QEMU was doing** (`sample <qemu pid>` on macOS, during a hang):

```
qemu_main:   main_loop_wait -> qemu_clock_run_all_timers -> timerlist_run_timers
               -> apic_timer / aio_bh_enqueue ...            (100 % of the samples)
CPU 0/TCG:   cpu_tb_exec -> do_st_mmio_leN -> qemu_mutex_lock (BQL)   (100 %)
CPU 1/TCG:   cpu_tb_exec -> do_st_mmio_leN -> qemu_mutex_lock (BQL)   (100 %)
```

QEMU's main loop was stuck running timers with the BQL held. Both vCPUs were blocked on the BQL in
the middle of an MMIO store. slirp, the monitor and the gdbstub all run in that main loop, which is
why ssh went dead while the guest's own bookkeeping, between stalls, looked healthy.

## Cause

`akuma_lapic_amd64::start_timer` wrote the periodic LVT **before** the new initial count:

```
TIMER_DIV  = /16
LVT_TIMER  = vector | PERIODIC     <- periodic, with whatever count is still loaded
TIMER_INIT = TIMER_COUNT
```

Its usual caller is `restore_periodic`, after `arm_oneshot` has loaded a one-shot count, which
`arm_deadline` floors at **1**. Between the two stores the LAPIC is periodic with a count of 1. QEMU
recomputes the APIC timer on the LVT write: (1 + 1) << 4, a 32 ns period of the virtual clock. Its
timer list runs from the main loop, which holds the BQL, and re-arms 32 ns later. It can never catch
up. The store that would fix it, `TIMER_INIT`, is the vCPU's next MMIO access, and it waits on the BQL
forever. On hardware the same window would only have cost a burst of interrupts. Under QEMU it is a
livelock that the guest cannot end.

Per-boot timing varies because it needs the LVT write to land while the leftover count is tiny,
which depends on the deadline `arm_deadline` happened to compute.

## Fix

`start_timer` writes `TIMER_INIT = 0` first. A zero initial count stops the timer, so no
intermediate state has a period, and the LVT switch and count reload happen with no timer running.
`arm_oneshot`'s order (one-shot LVT, then count) has no such window: its intermediate state is a
one-shot of the periodic count. `start_timer` is the only writer that enters periodic mode.

## Verified

- `akuma-lapic-amd64` host tests and clippy on the amd64 kernel are clean.
- Soak on the same rig (`SMP=2`, TCG, idle herd guest, ssh pinged every 10 s): **25 minutes, passed**
  (finished just after the fix was committed). Every ping answered except one at 111 s, taken during
  heavy host load, and QEMU's monitor still answered at that moment (during the hangs it did not).
  sshd was still serving at 1524 s of uptime. QEMU's main thread had used ~5 s of CPU at 2.5 min,
  against 134 s on a hung guest of the same age. The three kernels before the fix hung at ~70 s,
  ~160 s and ~270 s on this rig. On battery power, not AC.

## Still open

- **QEMU-only.** The trashcan and ryzen run KVM or bare metal, where this would only have been a burst.
  It was not measured there.
- Firecracker (KVM) should be unaffected for the same reason (the APIC timer is not run under QEMU's
  BQL there); not tested.

## Tooling that found it

- `sample <pid>` on the QEMU process. One look at the host threads separated "the guest is wedged"
  from "QEMU is wedged", which no amount of guest-side reading could.
- A thread-table dump through the gdbstub (`THREAD_STATES`, `WAKE_TIMES`, `ON_CPU`), taken twice. A
  single dump made the guest look frozen; two dumps showed it was running.
- The probe line can be switched on in a running guest by writing `1` to `net::PROBE_ON`.
