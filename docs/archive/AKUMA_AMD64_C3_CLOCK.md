# amd64 C3: the clock, and the two anchors that disagreed

**Date:** 2026-09-12
**Scope:** box C3 of `docs/archive/AKUMA_SELF_HOSTING_AMD64.md` — "`clock.rs`
dies; `akuma-syscalls-time` builds here; real clock: re-sync, drift, itimers,
adjtimex". The last box of trunk C.
**Status:** done, on all four rigs. QEMU/TCG `SMP=1` **665/0** (from a measured
656/0 baseline — nine new checks, all in the dispatch smoke test) and `SMP=4`
**675/0**; Firecracker/KVM on the box `SMP=4` **653/0**; **bare metal 665/0**;
AArch64 **315/0** at `SMP=4`; host tests **1377/0**; memory probes **10/10**;
`amd64_ring3_check` **OK**; `/probes/clockprobe` **12/12** on QEMU, **12/12 on
bare metal** and **12/12 on real Linux**; `amd64_ctrlc_probe` KILLED after
3.3 s (QEMU) and 3.4 s (metal).

**The metal's clock is right to the second.** `date -u +%s` against the host at
the same instant: **skew −1 s**, from SNTP through the LAN. And `busybox
sleep 3` takes **3.51 s** of real time there against **0.63 s** under QEMU/TCG
— the same kernel, the same arithmetic, so the ~5× skew is the emulated LAPIC
and not this code. See § 7.

---

## 1. The shape of the problem: two clocks, each internally plausible

The box reads "clock.rs dies", and the thing that had to die was not the file.
It was the **second anchor**.

| | amd64 before C3 | AArch64 |
|---|---|---|
| monotonic uptime | `net::uptime_us` = `lapic::ticks() * 10_000` | `akuma_timer::uptime_us` = CNTVCT/CNTFRQ |
| wall clock | `amd64/src/clock.rs`'s `(ANCHOR_UNIX_US, ANCHOR_UPTIME_US)` | `akuma_timer`'s `UTC_OFFSET_US` |
| who serves `clock_gettime` | an arm in `amd64/src/usermode.rs` | `akuma_syscalls_time::sys_clock_gettime` |

`akuma-syscalls-time` is shared: `akuma-syscalls-glue` depends on it and glue
builds for `x86_64-unknown-none` (that was B3's gate, passed 2026-09-07). So
the implementation was *already linked into the amd64 kernel* and already
reachable — the numbers were in `akuma-syscalls-abi` for four of the family.
It was not used, and the reason is one line:

```rust
let us = akuma_timer::utc_time_us(akuma_timer::uptime_us()).unwrap_or(0);
```

`akuma_timer::uptime_us` is `akuma_cpu::sysreg::cntvct_el0()` over
`cntfrq_el0()`, and `akuma-cpu`'s x86_64 arms for both are stubs returning `0`.
So on amd64 that crate's every clock read was **zero, forever** — and
`utc_time_us` read an offset nothing on the target ever wrote.

**Folding first and looking later would have been silent.** `clock_gettime`
would have returned `{0, 0}` for `CLOCK_MONOTONIC` and the unset anchor for
`CLOCK_REALTIME`. `nanosleep` would have been worse than either: it computes
`deadline = uptime_us() + interval` on the **frozen** clock and then parks with
`schedule_blocking(deadline)`, which compares against the **registered** clock —
a working one. So the park returns at once, the loop's own exit test reads the
frozen clock and never fires, and the syscall becomes an unkillable spin. A
frozen clock is a hang, not a wrong number.

## 2. The fix: one anchor, one hook, in the crate both kernels already share

`akuma_primitives::clock` already owned the **monotonic** clock as a
boot-registered hook (`set_clock_hook`, called from
`akuma_exec::runtime::register`), and both kernels already registered it:
AArch64 registers `akuma_timer::uptime_us` itself, amd64 registers
`net::uptime_us`. Five of `akuma-syscalls-time`'s sibling modules inside glue —
`poll`, `sync`, `flock`, `timerfd`, `proc` — already read it. It was the odd
one out.

Three moves, in order:

1. **`akuma-syscalls-time` reads `akuma_primitives::clock::uptime_us()`**
   instead of `akuma_timer::uptime_us()`. On AArch64 the registered function
   *is* `akuma_timer::uptime_us`, so the value is identical and the cost is one
   indirect call per read. On amd64 it is a working clock for the first time.
2. **The UTC anchor moves from `akuma-timer` to `akuma_primitives::clock`**,
   beside the monotonic clock it is expressed against. `akuma-timer` is the
   AArch64 generic-timer crate — CNTVCT, CNTV_CVAL, the PL031, the tick policy
   — and the amd64 kernel must not depend on it to hold a wall clock. The
   three AArch64 call sites (`akuma-kernel-core`'s `timer.rs` ×2 and
   `ntp_boot.rs`) name the new home directly rather than going through a
   forwarder: one spelling, so a reader cannot find the offset in two places
   again. `akuma-syscalls-time` no longer depends on `akuma-timer` at all.
3. **`amd64/src/clock.rs` deletes its pair** and anchors there. `now_us()`,
   `is_synced()` and `set_unix_us()` survive as three-line forwarders because
   `fs.rs` and `boot.rs` register them as the `utc_time_us` hook and the
   x86-only `gettimeofday`/`time`/`settimeofday` shims call them.

The signatures kept `akuma-timer`'s two-argument shape —
`set_utc_time_us(unix_epoch_us, boot_uptime_us)` — deliberately. SNTP samples
the anchor uptime at **packet receipt** and hands it over after the parse; a
convenience that read `uptime_us()` itself would silently add the parse to the
clock. The `is_utc_set()` companion is new, for the callers that ask "does this
machine know what time it is" with no timestamp in hand.

## 3. What folded, and what did not

Folded into glue (`amd64/src/usermode.rs`):

| syscall | x86_64 | asm-generic | note |
|---|---|---|---|
| `clock_gettime` | 228 | 113 | gains the large-`clock_id` guard — a pointer-sized id from a Go heap is `EINVAL` on Linux and used to get a timespec written to it |
| `clock_settime` | 227 | 112 | same two checks; no change |
| `adjtimex` | 159 | 171 | gains `ADJ_OFFSET` (this arm honoured only `ADJ_SETOFFSET`, so an `ntpd` asking for a slew got silence), the `ADJ_NANO` unit switch, and `STA_UNSYNC` |
| `nanosleep` | 35 | 101 | **parks** (`schedule_blocking`) where the local arm yield-spun |

New rows in `akuma-syscalls-abi`, each of which was an `ENOSYS` on amd64 and
served on AArch64 — the implementation was in glue all along and there was no
number to reach it by:

| syscall | x86_64 | asm-generic |
|---|---|---|
| `clock_getres` | 229 | 114 |
| `clock_nanosleep` | 230 | 115 — **the spelling `std::thread::sleep` emits** |
| `clock_adjtime` | 305 | 266 |
| `setitimer` | 38 | 103 |
| `times` | 100 | 153 |
| `getrusage` | 98 | 165 |

`clock_adjtime` is the sharpest crossing in the block: **x86_64 305 is
`akuma_syscalls_linux::nr::TIME`**, an Akuma-private number. A table that
forgot to translate it would have answered a `clock_adjtime` with the wall
clock in seconds and no error — the wrong-arm-not-no-arm failure that cost this
target months on `symlink`.

Not folded, by rule 2 of `akuma-syscalls-abi` (never invent an asm-generic
number for an x86-only legacy spelling): `gettimeofday` (96), `settimeofday`
(164) and `time` (201) stay as shims in `usermode.rs`. They read and write the
shared anchor now, so they are the same clock as everything else — which is
what rung 2 of the probe checks.

Two x86-only spellings gained an implementation rather than a shim, because
neither takes user memory and both belong to state the shared crates own:

- **`alarm`** (37) → `akuma_syscalls_glue::sys_alarm`, new in
  `akuma-syscalls-time`. musl spells `alarm(3)` as `SYS_alarm` where the number
  exists and as a `setitimer` pair where it does not, so AArch64 has never
  needed it.
- **`pause`** (34) → `akuma_syscalls_glue::sys_pause`, new in glue's `signal`
  module as `rt_sigsuspend` with the mask the thread already has. See § 5.

## 4. The itimers, and the one thing that had to be made safe first

`setitimer`/`alarm` arm a deadline in `akuma_exec::threading`; something has to
notice it expired. `akuma_exec::runtime::ExecRuntime::check_itimers` was
`|| {}` on amd64 with the comment "that is C3". It is
`akuma_syscalls_glue::check_itimers` now, and `idt::timer_dispatch` calls it.

Two decisions there:

**Called directly, not through `runtime().check_itimers`** — which is the same
function. `runtime()` is `require()` and **panics** when nothing is registered,
and the timer vector is live from `boot::late_init`'s `sti`, which the
self-test path reaches by more than one route. The direct call degrades: with
no clock registered `uptime_us()` is `0`, no deadline is `<= 0`, and the walk is
a few hundred relaxed loads that find nothing. The runtime row is still filled
in — a table that disagrees with what the kernel does is how five `ProcessHooks`
rows kept stale `false`s for months (`AKUMA_AMD64_STALE_FALSE_HOOKS.md`).

**Ungated by `from_user`**, unlike the signal delivery immediately above it in
the same handler. That gate is load-bearing there (the interrupted code
provably holds no BKL); here it would be wrong: an `alarm(5)` is most often set
by a process that then *blocks*, so a check that only ran on ticks interrupting
ring 3 would never fire for the caller it was written for.

Which meant the walk had to be safe from IRQ context, and it was not quite.
`wants_force_interrupt` takes `proc.signal_actions.actions` — a plain
`Spinlock` with no IRQ masking on its holders — so a tick landing on a core
already inside `rt_sigaction` for the same process would spin forever on a lock
that core owns. It is a `try_lock` now, falling back to the same `true` the
function already returns when there is no process context: a contended read
costs an `SA_RESTART` handler one un-restarted syscall, in a window it can only
reach by racing its own `sigaction` call. The AArch64 kernel has driven this
from its tick for months without hitting it; the window is narrow, not absent.

## 5. What the probe found, which is the interesting part

`userspace/forktest/c_stress/clockprobe.c`, twelve rungs, wired into
`scripts/utils/amd64_ring3_check.py`. It is statically linked musl and **passes
12/12 on real Linux**, which is what makes a failure a kernel result rather
than a probe bug (`LINUX_AB_PROBE_TECHNIQUE.md`). It never calls
`clock_settime`/`settimeofday`/`adjtimex`: a probe that steps the clock of the
machine it is being A/B'd on is a probe nobody runs twice, so the write half is
checked from the boot suite instead.

Half its rungs compare **two spellings against each other** rather than against
a known-good value, because the failure C3 exists to catch is two clocks that
are each internally plausible and disagree. Rung 2 is that rung, and it skips
rather than fails on a machine whose clock was never set — that is an offline
machine, not a broken kernel.

It found two defects, and neither was a clock.

### 5.1 `pause(2)` did not exist

Rung 8 is `alarm(1); pause();`. It failed, and the first explanation —
"the alarm is not firing" — was wrong. A five-line probe printing `errno` said:

```
alarm(1) -> 0
pause -> -1 errno=38 (Function not implemented) alarms=0
raw SYS_pause(34) -> -1 errno=38 alarms=0
```

x86_64 34 is `pause`; asm-generic has none, so musl emits the raw syscall here
and `ppoll(0,0,0,0)` there, and the amd64 kernel had answered `ENOSYS` to
**the oldest idiom in POSIX** — `alarm(n); pause();` — for its whole life. The
alarm was armed correctly; nothing ever waited for it.

Fixed by factoring glue's `sys_rt_sigsuspend` wait loop into
`suspend_until_signal(mask)` and adding `sys_pause()` = that loop with the
mask the thread already has.

### 5.2 Every delivered signal cost the *next* syscall a spurious `EINTR`

With `pause` in place, rung 8 passed and rung 9 failed. Measured:

```
delivered=1
getpid=-4 errno=0          <-- -EINTR, from a syscall that cannot block
getuid=0
```

`interrupt_thread` raises a per-thread flag meaning "break whatever blocking
syscall this thread is in". **`is_current_interrupted` consumes it**, and it is
read by two sorts of caller: a blocking wait loop, and the syscall prologue of
both kernels, which answers `EINTR` and returns. That works when the thread is
blocked. When it is *running*, nothing consumes the flag: the signal is
delivered at the next tick or syscall return, the handler runs and returns, and
the next syscall the program makes — any syscall — fails with `EINTR`.

**Pre-existing, on both kernels, and not caused by C3**: `deliver_signal` sets
the same flag for every signal, so `kill`, Ctrl-C and `pthread_kill` all reach
it. It went unnoticed because the obvious victim is a `write` whose result
nobody checks — `sigprobe` announces every rung through `write(2)` and ignores
the return, so twelve rungs of signal tests passed over it.

Fixed by `akuma_exec::process::signal_frame_installed(tid)`, called from the
point each kernel **installs a frame**: `amd64`'s `signal::enter_handler` and
`akuma_exceptions::try_deliver_signal`. Deliberately *not* from the decision
before it — a signal that turns out to be ignored, or whose handler cannot be
entered, has not reached userspace, and a flag raised for a blocking wait that
has not happened yet must survive to break it. Both call sites sit after the
`Ignore`/`Default` arms for that reason, which is also why the default-action
Ctrl-C path is untouched (re-measured: `amd64_ctrlc_probe` KILLED after 3.3 s).

Rung 8b pins it, and `getpid` is the witness because it is the syscall that
most obviously cannot block: an `EINTR` from it cannot be anything but a leaked
flag.

### 5.3 A third, smaller one, closed on the way in

`akuma_syscalls_time::sys_nanosleep` had no argument validation.
`Timespec::to_us` reinterprets — its own doc says so — so `tv_sec = -1` is
`1.8e19` microseconds and the sleep loop parks for ~584 000 years. That is an
unbounded wait reachable from unprivileged ring 3 by one bad argument, where
Linux returns `EINVAL`. The amd64 arm checked it and the shared crate did not,
so folding without this would have *traded an `EINVAL` for a hang*.

The check is `Timespec::is_valid_interval()` in `akuma-syscalls-linux`, host
tested there, applied by `sys_nanosleep` and `sys_clock_nanosleep` and by
nothing else: `to_us`'s own note says validating inside a conversion helper
changes syscall behaviour behind its callers' backs, and `pselect6`/`ppoll`/
`futex` pass *timeouts*, where the tree's saturating answer to an absurd value
is the established one.

## 6. Two measurement traps, both paid for

**The boot suite cannot prove a clock moves.** The first version of the
dispatch smoke-test check read `clock_gettime(CLOCK_MONOTONIC)` twice with four
`allow_tick()`s between and asserted movement. It failed against a working
clock: `allow_tick` opens a one-instruction interrupt window, four of them land
inside the same 10 ms tick, and — more to the point — **the suite runs with the
LAPIC timer stopped** so its own output does not interleave with ticks. The
check became an *identity* check instead, which is deterministic and says more:
the syscall's answer must equal `net::uptime_us()` within one tick, i.e. it is
*this kernel's* clock and not another one. Movement is a ring-3 property and
the probe is where it lives.

**C's argument evaluation order.** A throwaway probe printed
`printf("a(10)=%ld a(0)=%ld", syscall(SYS_alarm,10), syscall(SYS_alarm,0))` and
reported `0` for both, which looked like a broken `alarm`. GCC evaluates
right-to-left; `a(0)` ran first. The kernel was correct and the probe was not,
which cost one boot to work out.

## 7. What C3 did **not** do

- **`clock.rs` does not die.** It is 361 lines and all of them are the SNTP
  client — a UDP socket, DNS, the retry policy and the outcome reporting. That
  is a genuine platform fact (no RTC on Firecracker's device model or QEMU
  `microvm`), not a duplicate of anything. What died is the *clock it kept*.
- **No frequency discipline.** `adjtimex`/`clock_adjtime` step rather than
  slew, on both kernels, pinned in `akuma-syscalls-time`. "Drift" in the box's
  wording is not implemented and was not going to be by a 10 ms tick.
- **`timex.tick` is reported as `0`** where the deleted amd64 arm reported
  `lapic::US_PER_TICK_TARGET`. Not restored: the AArch64 tick is governor-tuned
  and can demote to 1 ms, so a constant in the shared crate would be wrong on
  one of the two kernels, and nothing in the tree reads the field.
- **`clock_getres` still reports 1 µs** for a clock whose tick is 10 ms, on
  both kernels. An old divergence, pinned rather than re-litigated here.
- **`getitimer` (x86_64 36) is still `ENOSYS`.** Glue has no arm for
  asm-generic 102 either, so there was nothing to reach; both kernels lack it
  equally.
- **The QEMU/TCG guest clock still runs ~5× wall-clock.** Both halves measured
  here on the same kernel: `busybox sleep 3` returns in **0.63 s** of host time
  under QEMU/TCG and **3.51 s** on the metal, whose wall clock is within **1 s**
  of the host's. So this is the emulated LAPIC and not the kernel's arithmetic. Every rung of `clockprobe` that measures a sleep does it
  against the *guest's own* monotonic clock for that reason — what must hold is
  that the clock a program steers by and the sleep it asks for agree with each
  other.

  The obvious next step, if the skew starts costing something, is a
  **TSC-derived uptime**: `rdtsc` is calibrated against the same PIT the LAPIC
  already is, reads without an interrupt (so it keeps advancing while the timer
  is masked — which is exactly why the boot suite cannot time anything today),
  and gives sub-microsecond resolution instead of 10 ms. It was not attempted
  here because `net::uptime_us` is the clock the scheduler, the network
  timeouts and `park_until` all steer by, and changing what it *means* is its
  own measured change, not a rider on this one.

## 8. Gates

| gate | result |
|---|---|
| `amd64_trials.py --local-only` | **665 passed, 0 failed** (`SMP=1`) |
| `amd64_trials.py --local-only --smp 4` | **675 passed, 0 failed** |
| `amd64_ring3_check.py -n 8` | **OK** — 8/8 sessions, `free` unmoved, heap +17 kB, `ps` steady, grandfork ALL PASS, sigprobe 12/12, clockprobe 12/12 |
| `/probes/clockprobe` on real Linux (aarch64 musl, Lima) | **12/12, rc=0** |
| `amd64_mem_trials.py --local-only` | **10/10**, 0 unexpected failures |
| `amd64_ctrlc_probe.py` | **KILLED after 3.3 s** |
| `amd64_trials.py --remote-only --smp 4` (Firecracker/KVM on the box) | **653 passed, 0 failed** |
| **bare metal**, RAM image, `init=/bin/sshd` | **665 passed, 0 failed** — same number as QEMU `SMP=1`. Staged with no `root=`, so the USB controller is not touched: this does not re-measure the `root=/dev/sda1` path, which is still item 1 of the walk's YOU-ARE-HERE |
| `/probes/clockprobe` on bare metal | **12/12, rc=0** |
| `amd64_ctrlc_probe.py` against the metal | **KILLED after 3.4 s** |
| the metal's wall clock vs the host's, same instant | **−1 s** |
| `busybox sleep 3` on the metal / under QEMU-TCG | **3.51 s** / 0.63 s |
| AArch64 `MEMORY=2048 SMP=4 cargo run --release` | **315 passed, 0 failed** |
| `cargo test` (host) | **1377 passed, 0 failed** |
| clippy, every crate + both kernels + `extreme-size` | clean at `-D warnings` |

## Background

- `docs/archive/AKUMA_SELF_HOSTING_AMD64.md` — the unlock tree; C3 is its last
  trunk-C box.
- `docs/archive/MISSING_NTP_SYSCALLS.md` — why `akuma-syscalls-time` exists and
  what `clock_settime`/`adjtimex` were missing before it.
- `docs/archive/AKUMA_FIRECRACKER_AMD64.md` § 3.29.5 — the TLS-certificate
  failure that made an unset clock visible in the first place.
- `docs/archive/AKUMA_AMD64_STALE_FALSE_HOOKS.md` — why the `check_itimers` row
  is filled in even though the tick calls glue directly.
- `docs/archive/LINUX_AB_PROBE_TECHNIQUE.md` — the rule that makes
  `clockprobe`'s Linux run the arbiter.
- `docs/archive/PTHREAD_KILL_EINTR_DELIVERY_STARVATION.md` — the other
  delivered-signal record at the same chokepoint § 5.2 edits.
