# Second server after a bound listener: whole-kernel freeze at SMP=1 (2026-08-25)

**Status: mechanism DIAGNOSED by GDB on the frozen VM (§2a); the fix is not yet
designed.** Reproduced 4/4 in QEMU at SMP=1; the identical trigger at SMP=4
(same 1024 MiB RAM) survives. Sibling of
[`HTTPD_STARVATION.md`](HTTPD_STARVATION.md) (same trigger, harder failure) and
probably unrelated to [`NGINX_LOST_WAKEUP.md`](NGINX_LOST_WAKEUP.md) (no epoll,
no backstop rescue — the tick itself stops).

> **The one-line answer.** On a single-core QEMU boot with herd's sshd running
> (one listener bound, in its accept loop), starting a **second server process
> from an SSH session** freezes the whole kernel — no timer ticks, no heartbeats,
> no panic, QEMU drops to ~3 % CPU. nginx alone boots fine; the freeze lands
> *after the first fork+exec+bind from the session shell*, at varying syscalls.

## 1. Exact reproducible conditions

All three freezes reproduced with:

- **Host:** QEMU `11.1.0`, `-machine virt,gic-version=3 -accel hvf -cpu host`
  (the `cargo_runner.sh` default on Apple Silicon), macOS HVF.
- **Kernel:** `cargo build --release --features devbox-smoltcp,no-tests` at
  `2c1eb9d0` + uncommitted instrumentation commits of 2026-08-24/25.
- **Guest:** `devbox.img` (bootstrapped 2026-08-25), mounted **snapshot=on**
  (`INSTANCE>0`), herd auto-started sshd on :22.
- **`SMP=1`, `MEMORY=1024`.** This combination is load-bearing — see §4.
- **Trigger**, from one SSH session (sshd→shell→children):

```
/bin/httpd 8090 >/tmp/h.log 2>&1 & sleep 1; nginx     # froze
/bin/httpd 8080 ... & sleep 1; /bin/httpd 8080 ...     # froze (same port twice)
/bin/httpd 8080 ... & ...; nginx                       # froze
```

## 2. Where it froze — three different syscalls, one common predecessor

| run | last lines before silence | froze inside |
|---|---|---|
| matrix-3 | `execve(httpd 8090)` + `[AS-EXEC] pid=5` + `bind(fd=3, port=8090)` OK, then `execve("/bin/busybox", ["sleep","1"]) PID 6` with **no** `[AS-EXEC] pid=6` | `execve` of the shell's own `sleep 1` |
| matrix-4 | httpd #1 bound :8080, `execve(busybox sleep)` completed, then the **second** `bind(fd=3, port=8080)` (port already bound) | `bind` EADDRINUSE path |
| boot-5 (first hit) | `execve(httpd 8080)` + `[AS-EXEC] pid=5` + `bind(8080)` OK, then `execve("/usr/sbin/nginx") PID 6` with **no** `[AS-EXEC] pid=6` | `execve` of nginx |

Common shape: the session shell forks several children back-to-back; the first
child completes `execve` **and binds a listener**; the freeze lands in the next
fork/exec-adjacent syscall (or a port-conflict bind). Every run shows
`[AS-FREE-DEFER] ... path=owner held_by_ctx tid=12 state=2` shortly before
silence — the deferred address-space free owned by the first child's thread is
in the blast radius but is unproven as the cause.

After the freeze: no `[Heartbeat]`, no `[EXC]`/`[IRQS]` counters (they print
every 30 s), no `[BKL] stuck`, no watchdog, no panic. QEMU falls to ~3 % host
CPU — the core is parked (`wfi`), not spinning.

## 2a. The mechanism — GDB on the frozen VM (2026-08-25, release-debug profile)

Booted the `release-debug` kernel with `GDB=1 SMP=1 INSTANCE=9`, ran the
double-httpd trigger, attached to the gdbstub. Everything below is read off the
frozen core, not inferred:

- **Backtrace of the running thread (t12):** `sys_accept` → `socket_accept` →
  `wait_until` (`socket.rs:740`) → `wait_park` → **`idle_halt`**
  (`threading/mod.rs:3069`). t12 is httpd's `accept`, parked via the
  Promiscuous park arm — `blocking_relax_net`, which under `kernel_smp_shared`
  is `idle_halt` **without** the `yield_now` (the "+27 % req/s" variant).
- **`idle_halt` disables preemption for the whole halt**
  (`threading/mod.rs:3052` … `:3091`) and never marks the thread WAITING. The
  parked waiter therefore *remains the current thread, RUNNING, holding the
  only core inside a preempt-disabled `wfi` loop*.
- **The scheduler is entered on every tick and refuses to switch.**
  Breakpoints in `schedule_indices` fire each timer tick with
  `voluntary=false`; the wake-pass runs (six threads sit READY: t0, t1, t8, t9,
  t10, t13), then `!voluntary && is_preemption_disabled()` (mod.rs:2531)
  returns `None` — t12's preempt count is 1 for as long as it is inside
  `idle_halt`. No involuntary switch can ever displace it.
- **No voluntary SGI can ever be raised.** Voluntary reschedules come only
  from *running* threads (`yield_now`, `schedule_blocking` entry,
  `request_voluntary_reschedule`). At SMP=1 the only running thread is the
  parked waiter; every producer of a voluntary SGI is itself READY-but-never-
  scheduled. Circular starvation of the scheduler itself.
- **Not a lock deadlock.** `KERNEL_LOCK` (BKL): `owner=0` (FREE),
  `next_ticket == now_serving` (balanced). `POOL.raw.locked=0`. No spinlock is
  held anywhere. The IRQ-masked-recursive-lock family is ruled out.
- **The park that closes the loop:** t11 (the session shell, in `wait4`) is
  WAITING with `WAKE_TIMES[11] = 0` — a waker-only park whose wake is its
  child's exit; the child is one of the READY-never-run threads.
- **CPSR at the freeze: `0x80000345`** — IRQs enabled (I-bit clear), EL1.
  That is why the machine looks "parked, ~3 % host CPU" rather than spinning:
  it sleeps between ticks and services each tick's wake-pass without ever
  switching.

So the freeze condition in one sentence: **a non-idle thread parks via
`idle_halt` (the yield-less `blocking_relax_net`) at SMP=1, and with no second
core to raise a voluntary reschedule, the preempt-disabled halt becomes an
uninterruptible monopoly on the only CPU.**

Open sub-question (fix-relevant, not diagnosis-relevant): healthy boots run for
minutes with sshd's `accept` parked in the same `idle_halt` path without
freezing. The likely escape on those boots is the `fruitless_progress` relap —
`wait_until`'s lap only re-parks when `poll()` reports no progress, so a waiter
on a system with *any* background socket traffic keeps re-lapping (preemptible
seams) instead of settling into deep `wfi`; a fully quiet network (localhost-
only double-httpd, nothing else) has no such seams. Unverified.

## 2b. Why SMP=4 survives

The identical trigger (`/bin/httpd 8080 & … /bin/httpd 8080 &`, same 1024 MiB
RAM, same image) at `SMP=4` completes (`SURVIVED`, both httpds in `ps`, SSH
still answers). Peer cores keep executing threads that raise voluntary
reschedules, so a preempt-disabled halt on one core cannot strand the machine.
**The discriminating factor is core count, not RAM.** This also predicts the
Firecracker starvation ([`HTTPD_STARVATION.md`](HTTPD_STARVATION.md), 1 vCPU)
is the same mechanism wearing a milder face — there, the NIC interrupt stream
keeps producing wake-ups that eventually get scheduled; the QEMU freeze has no
such stream.

## 3. Ruled out

- **nginx.** Alone on a fresh boot it starts, binds, serves `200`
  (`ab -n 100` in-guest, 0.55–0.86 ms/req — the fast regime).
- **Port value or port conflict.** matrix-3 froze with distinct ports and no
  conflict; matrix-4 froze *in* the conflict. Both paths die; neither is the
  single cause.
- **The 2026-06 HVF `isv` assertion returning.** Four "instant assert, no
  kernel output" boots today were operator error: `FC_FEATURES=...
  overlays/devbox-firecracker/build.sh` had overwritten
  `target/.../release/akuma` with the Firecracker-linked image (base
  0x80300000); QEMU loads at 0x40000000 → ISV=0 data abort on the first
  absolute access. `cargo build` uplifted the cached devbox artifact back
  (runner comment in `scripts/cargo_runner.sh` warns about exactly this).
  Real freezes boot normally and run for 60–145 s first.
- **The listener-queue data-structure change** (`4a3c2b76` "use fixed type for
  socket table"). That commit changed `SOCKET_TABLE` (Vec → fixed array), *not*
  the listener backlog, which is still a `VecDeque<SocketHandle>`
  (`crates/akuma-net/src/socket.rs:162`). Not exonerated by evidence, but the
  description and the freeze sites (execve/bind, not accept) don't line up.
- **A recursive / re-entered networking spinlock** (the previous-branch
  incident class). §2a: `KERNEL_LOCK` free with balanced tickets, `POOL`
  unlocked, and the frozen thread's backtrace contains no lock acquire at all
  — it is in `idle_halt`'s `wfi` wake loop.
- **An IRQ-masked park ("someone forgot to turn IRQs back on").** §2a: CPSR
  `0x80000345` at the freeze — I-bit clear. Ticks deliver and the wake-pass
  runs every tick; it is the *preemption* count (not DAIF) that suppresses the
  switch.

## 4. What makes it fire — the single-core condition

- **SMP=4, same image, same day, same 1024 MiB RAM, identical trigger:**
  survives (§2b). Earlier SMP=4 runs with four listeners at 4096 MiB also
  survived.
- **SMP=1, QEMU:** 4/4 freezes (§2, plus the release-debug repro).
- **Firecracker, 1 vCPU** ([`HTTPD_STARVATION.md`](HTTPD_STARVATION.md)): the
  milder sibling — second herd service starves sshd's accept, first service
  keeps serving, system alive.

Single-core is the common factor across both hosts; §2a is the mechanism, and
severity differences reduce to whether anything on the machine still produces
wake-ups/voluntary reschedules.

Anomaly worth recording: at SMP=1 the netpoll thread (tid=1) accumulated
~100 % of one core from early boot (113.6 s CPU at T120 uptime, zero workload)
— the loop never reached its halt even before anything went wrong. Unknown
whether related.

## 5. Fix directions (not yet implemented)

The mechanism suggests three candidate fixes, in ascending invasiveness:

1. **Make `idle_halt` displacement-safe for non-idle callers.** The preempt
   disable exists to keep halt residency out of the halter's CPU-time billing
   (`threading/mod.rs:3040-3052`). For a *non-idle* thread (a socket waiter via
   `blocking_relax_net`), being switched out mid-halt is acceptable — the
   billing argument can be preserved by gating the disable on
   `IS_IDLE_THREAD[tid]`, or by re-enabling preemption just before `wfi` and
   re-disabling after (the halt itself is then preemptible; a tick landing in
   it can displace the waiter, which is exactly what SMP=1 needs).
2. **Park socket waiters via `schedule_blocking` (WAITING + backstop deadline)
   instead of `idle_halt` when `core_count() == 1`.** Deadline-driven parks are
   rescued by the wake-pass itself (it CASes WAITING→READY on every scheduler
   entry, even preempt-disabled ones), and the park's entry raises a voluntary
   SGI that immediately hands the core away. This is a policy split inside
   `blocking_relax_net`, mirroring the `net-waker-park`/promiscuous split
   already modelled in `akuma-net-yarn` — and measurable there first.
3. **Scheduler-side: let the wake-pass escalate.** If a tick's wake-pass readied
   a thread but the current thread is preempt-disabled-and-parked (in
   `idle_halt`), treat the *next* scheduler entry as voluntary. Broad, risky,
   and probably the wrong first move — listed for completeness.

Whichever is chosen, the acceptance test is §6's trigger at SMP=1 plus the
SMP=4 four-listener soak, and the healthy-boot sshd-park case (§2a's open
sub-question) should be measured before/after — the fix changes the park path
every socket waiter takes.

Remaining diagnosis follow-ups: (a) verify the traffic-seam hypothesis for
healthy sshd parks (§2a open sub-question); (b) run §1's trigger on the
Firecracker/Lima setup to confirm the starvation is the same mechanism.

## 6. Reproducing the measurement

```bash
cargo build --release --features devbox-smoltcp,no-tests
ELF=target/aarch64-unknown-none/release/akuma
INSTANCE=1 MEMORY=1024 SMP=1 DISK=devbox.img scripts/cargo_runner.sh "$ELF" &
# wait for "[herd] Starting service: sshd" in the log, then:
ssh -o StrictHostKeyChecking=no -p 2322 root@localhost \
  '/bin/httpd 8090 >/tmp/h.log 2>&1 & sleep 1; nginx'
# expect: log goes silent mid-trigger, ssh dead, QEMU ~3% CPU
```

Beware the artifact trap from §3: after any `overlays/devbox-firecracker/build.sh`,
rebuild the QEMU kernel before booting it (`cargo build --release --features
devbox-smoltcp,no-tests` restores the cached artifact).

## Background

- [`HTTPD_STARVATION.md`](HTTPD_STARVATION.md) — the Firecracker 1-vCPU
  sibling: second listener starves sshd without freezing the machine.
- [`HTTPD_ACCEPT_HANG.md`](HTTPD_ACCEPT_HANG.md) — blocking-`accept` family;
  episodes where httpd stopped answering at SMP=4. Possibly a third sibling.
- [`NGINX_LOST_WAKEUP.md`](NGINX_LOST_WAKEUP.md) — epoll lost-wake with 10 ms
  backstop rescue; distinct (ticks continue there).
- [`QEMU_HVF_ISV_BUG.md`](QEMU_HVF_ISV_BUG.md) — the 2026-06 `isv` fix this is
  NOT (and the artifact confusion that mimicked it).
