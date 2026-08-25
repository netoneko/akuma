# time syscalls

`clock_gettime` / `clock_settime` / `clock_getres` / `adjtimex` /
`clock_adjtime` / `nanosleep` / `clock_nanosleep` / `setitimer` / `times` /
`getrusage` / `time` / `uptime`. Source: `crates/akuma-time/src/lib.rs` —
moved out of the bin crate 2026-08-25 (nothing in it actually needed
bin-crate privilege; see that file's module doc). `src/syscall/mod.rs`
aliases `use akuma_time as time;`, so every `time::sys_*` call site in the
dispatch table is unchanged. The boot-time SNTP fallback
([below](#boot-time-clock-source-and-the-firecracker-fallback)) lives in the
same crate's `sntp`/`boot` submodules, host-tested there; the wiring into
`akuma_net` is `src/ntp_boot.rs`.

> **Stability: A (stable).** The Go forktest clock-corruption bug (Mar–May
> 2026) is resolved and quiet since. The recurring lesson: **a leaked GPR
> looks exactly like a valid syscall argument** — Go's `nanotime1` left
> `x8=113` (clock_gettime's own syscall number) in the register file, and a
> QEMU DC-ZVA misroute replayed it with a heap pointer in `x0` as the
> `clock_id`; only a plausibility bound on the argument caught it.
> `clock_settime`/`adjtimex`/`clock_adjtime` are new (2026-08-25, closing
> `archive/MISSING_NTP_SYSCALLS.md`) and unmeasured in production, so treat
> their grade as provisional until they've seen real churn.

## clock_gettime / clock_getres

`sys_clock_gettime` (`lib.rs:238`, syscall 113) rejects `clock_id_arg >
0x1000_0000` with `EINVAL` **before** touching `tp_ptr` — a genuine Linux
clock_id is a small integer or a compact CPU-clock encoding
(`~(pid << 3) | CPUCLOCK_*`), never pointer-sized. This guard exists because
a pointer-sized value here is corruption (see Background): copying out a
timespec to a "clock_id" that's actually a stray pointer produced a
WILD-DA at `FAR=0x10`. On a large `clock_id`, the handler also decodes the
4 bytes before/after the trap-frame ELR and logs them via `log::warn!`
(`lib.rs:246-259`) to identify which caller left the leaked register — pure
diagnostics, no behavioural effect.

Once past the guard: `clock_id == 0` (`CLOCK_REALTIME`) reads
`akuma_timer::utc_time_us(akuma_timer::uptime_us())`; anything else falls
back to `akuma_timer::uptime_us()` (i.e. every non-zero clock ID is treated
as monotonic). `sys_clock_getres` (`lib.rs:380`) always reports 1 ns
resolution and ignores `clock_id` entirely.

## clock_settime / adjtimex / clock_adjtime

The other half of clock support — until 2026-08-25 there was no way to ever
*set* the clock at all (`archive/MISSING_NTP_SYSCALLS.md`), so a wrong or
unset `CLOCK_REALTIME` had no recovery path: no `date -s`, no `rdate`, no
`ntpd`.

- **`sys_clock_settime`** (`lib.rs:286`, syscall 112): `EINVAL` unless
  `clock_id == CLOCK_REALTIME`. Reads a `timespec`, calls
  `akuma_timer::set_utc_time_us(unix_epoch_us, akuma_timer::uptime_us())`.
  This is the syscall `date -s`/`rdate`/`ntpd`'s final step all bottom out
  on, and the prerequisite for the other two below.
- **`sys_clock_adjtime`** / **`sys_adjtimex`** (`lib.rs:322`/`363`, syscalls
  266/171 — `adjtimex` is `clock_adjtime(CLOCK_REALTIME, buf)`): reads a
  208-byte `struct timex` (`LocalTimex`, `lib.rs:76`, field-for-field aarch64
  layout with the padding after `modes`/`status`/`shift` spelled out — get
  that wrong and every field after the first gap reads the wrong bytes).
  Honors `ADJ_SETOFFSET` and `ADJ_OFFSET` as an **immediate step**, not a
  gradual PLL slew — there is no frequency-discipline state machine here, so
  a "slew" request lands all at once. Good enough for `rdate`/`ntpd -q`/
  `sntp`-style one-shot correction (verified live, see below); not a full
  `ntpd` daemon doing continuous correction. Every other `modes` bit
  (`ADJ_FREQUENCY`, `ADJ_TICK`, …) is accepted and silently ignored rather
  than rejected, so a caller that ORs several bits together for one step
  still gets that step. Query mode (`modes == 0`) reports current state and
  `STA_UNSYNC` unless the caller explicitly cleared it via `ADJ_STATUS`.
  Always returns `TIME_OK` (0) — there is no leap-second state machine to
  report anything else from.

**Verified live** (isolated QEMU boot, 2026-08-25): `date -s '2030-01-02
03:04:05'` correctly sets and reflects; `ntpd -q -n -p pool.ntp.org` (real
network round trip) steps the clock to the correct wall-clock time via this
same `adjtimex` path.

## boot-time clock source and the Firecracker fallback

QEMU `virt` has a PL031 RTC; `kernel_main` reads it
(`timer::init_utc_from_rtc`, `src/timer.rs`) before networking exists, and
that is the end of it there. Firecracker's aarch64 microVM exposes **no
PL031 at all** — `archive/MISSING_NTP_SYSCALLS.md` found the guest
permanently stuck at epoch 0 with no `clock_settime` to even correct it.

The fix is a platform switch with no separate "which board is this" check:
in `run_async_main` (`src/main.rs`), right after network init, an unset
clock at that point in boot **is** the "no RTC" signal —
```rust
if timer::utc_time_us().is_none() && config::ENABLE_NTP_BOOTSTRAP {
    match ntp_boot::try_bootstrap_clock() {
        Ok(()) => safe_print!(96, "[NTP] boot-time clock sync succeeded: {}\n", timer::utc_iso8601()),
        Err(e) => safe_print!(128, "[NTP] boot-time clock sync failed: {}\n", e),
    }
}
```
`try_bootstrap_clock` (`#[cfg(feature = "smoltcp")]`, a stub returning
`Err(...)` otherwise) itself does no logging — it returns
`Result<(), &'static str>` (same shape as `akuma_net::init`) and leaves
reporting to this one call site, which is why it's a single `safe_print!`
per outcome rather than several `console::print` calls in a row: the wait
loop below cooperatively yields while polling, so another ready thread can
genuinely run *between* separate `console::print` calls at this point in
boot and tear the line (reproduced live — see below); a `safe_print!`
formats into one stack buffer and flushes it as a single `emit()`, so it
can't be interleaved mid-message. Steps 1-3 below are all inside
`try_bootstrap_clock`:

1. Resolves `config::NTP_SERVER_HOSTNAME` via `akuma_net::smoltcp_net::
   dns_query`. **This is an IP literal (`216.239.35.0`, Google Public NTP's
   stable anycast address) by design, not a hostname** — `dns_query`
   fast-paths a literal without any DNS lookup, and the one platform this
   fallback exists for has broken DNS: `overlays/devbox-firecracker/
   README.md` documents that `smoltcp_net.rs`'s hardcoded
   `QEMU_DNS_SERVER` (`10.0.2.3`) has nothing listening on it under
   Firecracker. A hostname here would time out on exactly the platform that
   needs this fallback.
2. Opens a UDP socket, sends one SNTP request (`akuma_time::sntp::
   build_request`), and busy-polls (`akuma_net::smoltcp_net::poll()` +
   `yield_now`, no `blocking_relax`/interrupt park — this runs **before**
   `akuma_primitives::irq::unmask_irqs()`) up to
   `config::NTP_BOOTSTRAP_TIMEOUT_US` (3 s) for a response.
3. `akuma_time::sntp::parse_response` validates mode/stratum/origin-echo
   (rejects a stale or off-path-spoofed reply) and computes the estimated
   Unix time from **uptime deltas**, not absolute client time — the
   client's own clock has no absolute epoch yet, which is the entire reason
   this is running, so the classic four-timestamp offset formula is applied
   over `(t4_up - t1_up)` and `(T3_srv - T2_srv)` instead. See the doc
   comment on `akuma_time::sntp` for the derivation.
4. On success, `akuma_timer::set_utc_time_us(result.unix_epoch_us,
   result.anchor_uptime_us)` and `Ok(())`. On any failure (DNS failure, no
   socket, bind failure, send failure, timeout, or a malformed/spoofed
   reply), `Err(&'static str)` describing which — never a panic, and boot
   continues with the clock unset if the caller's log shows a failure.

**Verified live** on the Lima Firecracker host (`overlays/devbox-firecracker`,
genuinely no PL031), 2026-08-25:
```
Warning: RTC not available, UTC time not set
[NTP] boot-time clock sync succeeded: 2026-08-25T12:45:17.621299Z
```
Boot suite still 302/0/0 (confirmed via `herd`/`httpd` starting normally
afterward). An earlier revision of this call site used three sequential
`console::print` calls instead of one `safe_print!`, and one live boot
reproduced exactly the predicted tear: the label and timestamp came out
fine but the trailing newline was pushed out by another thread's
`[AS-FREE]` line landing in the gap between calls, all three findable in
`archive/UART_SMP_INTERLEAVE_FIX.md`'s class of bug despite this being a
single-vCPU boot — the interleaving source there is cross-*core*, here it's
a cooperative `yield_now()` inside the wait loop letting another thread run
between two otherwise-unrelated `console::print` calls.

## nanosleep

`sys_nanosleep` (`lib.rs:390`) accepts **two ABIs** on the same syscall
number: Linux/musl's `struct timespec *` in `a0`, or libakuma's raw
`(seconds, nanoseconds)` pair in `a0`/`a1`. It disambiguates by treating
`a0 >= PAGE_SIZE` as "looks like a user pointer" and attempting
`copy_from_user_safe`; a copy failure falls back to treating `a0`/`a1` as
raw values instead of faulting. Sleeping itself is the standard blocking
pattern: compute a deadline, then loop `schedule_blocking(deadline)` until
the deadline passes or `is_current_interrupted()` returns `EINTR`.

`sys_clock_nanosleep` (`lib.rs:430`, syscall 115) is the same loop with
`clockid`/`TIMER_ABSTIME` handling layered on — `CLOCK_REALTIME` absolute
deadlines convert through `akuma_timer::utc_time_us()` the same way
`sys_futex`'s `FUTEX_CLOCK_REALTIME` does (`src/syscall/sync.rs`).
`std::thread::sleep` on `target_os = "linux"` calls this syscall
specifically, not plain `nanosleep`.

## setitimer / itimers

`sys_setitimer` (`lib.rs:179`, `ITIMER_REAL` only) arms/disarms the
per-thread-slot deadline `alarm()`/`setitimer` deliver SIGALRM from; the
deadline itself lives in `akuma_exec::threading` (`get_itimer`/`set_itimer`)
so slot recycling resets it like every other per-slot register. `
check_itimers` (`lib.rs:140`, called from the timer tick) fires expired
timers and force-interrupts the target's blocking syscall only when the
installed SIGALRM handler wants that (`SignalAction::
wants_itimer_force_interrupt`) — an `SA_RESTART` heartbeat handler must not
have its own blocking syscalls broken every tick. See
`archive/GIT_CLONE_STALE_ITIMER_SIGALRM.md` for the bug this guards against.

## times / getrusage

`sys_times` (`lib.rs:470`) and `sys_getrusage` (`lib.rs:481`) both zero-fill
their output struct (`struct tms` 32 B, `struct rusage` 144 B) rather than
tracking real per-process CPU accounting — good enough for callers that only
check the syscall succeeds. `sys_times` return value (`uptime_us / 10_000`,
i.e. clock ticks) is otherwise real.

## time / uptime

`sys_time()` and `sys_uptime()` (`lib.rs:490-493`) are one-line wrappers
around `akuma_timer::utc_time_us()` / `akuma_timer::uptime_us()` — no
argument validation needed since neither takes a pointer.

## Background

- `archive/GO_FORKTEST_DEBUG.md` — the `clock_gettime` leaked-x8 /
  errno-as-pointer investigation (`nr=113` crash signatures, the
  `FAR=0xfffffffffffffffa` family).
- `archive/MISSING_NTP_SYSCALLS.md` — the Firecracker epoch-0/TLS
  investigation that motivated `clock_settime`/`adjtimex`/`clock_adjtime`
  and the boot-time SNTP fallback. **Closed 2026-08-25**, verified on both
  QEMU (regression) and the actual Firecracker platform (the fallback
  firing).
