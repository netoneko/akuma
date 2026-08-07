# `std::thread::sleep()` panicked on every call — missing `clock_nanosleep` syscall — fixed 2026-08-07

## Summary

`std::thread::sleep()` — on any thread, any duration, unconditionally —
panicked instead of sleeping, on every build targeting
`aarch64-unknown-linux-musl`. Root cause: Akuma never implemented the
`clock_nanosleep` syscall (Linux aarch64 syscall #115), and every
unimplemented syscall falls through to a generic `ENOSYS` handler. Rust's
`std::thread::sleep` on any `target_os = "linux"` build calls
`clock_nanosleep` specifically — not the plain `nanosleep` syscall most
people assume it uses — and its own internal retry loop only ever expects
`0` or `EINTR` back from that call, so `ENOSYS` (a value it never
anticipated) tripped an `assert_eq!` inside `std` itself.

**Smallest possible reproduction**, no threads even needed:

```rust
fn main() { std::thread::sleep(std::time::Duration::from_millis(1)); }
```

```
thread 'main' (18) panicked at .../library/std/src/sys/thread/unix.rs:581:17:
assertion `left == right` failed
  left: 38
 right: 4
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
```
Exit code 101, every time.

## How it was found

Not by looking for it — found as a side effect of debugging a *different*
bug (the Failure D lost-wake investigation,
[`J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md`](J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md)
§7.17). A watchdog thread built to poll and log state every second went
completely silent — its log file was created (so the thread definitely ran)
but stayed at 0 bytes for over a minute. The same session had already logged
an unrelated-looking panic (`` assertion `left == right` failed: left: 38,
right: 4 ``) appearing once in *every* probe run all session, dismissed at
the time as a separate, low-priority curiosity. Connecting the two —
`thread::sleep` was the *only* call in the watchdog's poll loop that wasn't a
futex-backed primitive — led directly here.

## Root cause, read from Rust's own source

Fetched `library/std/src/sys/thread/unix.rs` from `rust-lang/rust` at the
exact commit this build's `rustc` reports (`rustc --version --verbose` →
`31fca3adb283cc9dfd56b49cdee9a96eb9c96ffd`, `rustc 1.96.1`) via `gh api
repos/rust-lang/rust/contents/<path>?ref=<commit>`. `sleep()`'s
implementation branches by target:

```rust
cfg_select! {
    // Any unix that has clock_nanosleep
    any(target_os = "freebsd", target_os = "netbsd", target_os = "linux", ...) => {
        unsafe fn nanosleep(rqtp: *const libc::timespec, rmtp: *mut libc::timespec) -> libc::c_int {
            unsafe { libc::clock_nanosleep(crate::sys::time::Instant::CLOCK_ID, 0, rqtp, rmtp) }
        }
    }
    _ => {
        unsafe fn nanosleep(rqtp: *const libc::timespec, rmtp: *mut libc::timespec) -> libc::c_int {
            let r = unsafe { libc::nanosleep(rqtp, rmtp) };
            // `clock_nanosleep` returns the error number directly, so mimic
            // that behaviour to make the shared code below simpler.
            if r == 0 { 0 } else { sys::io::errno() }
        }
    }
}
...
let r = nanosleep(ts_ptr, ts_ptr);
if r != 0 {
    assert_eq!(r, libc::EINTR);
    ...
}
```

`target_os = "linux"` is in the first arm's list, so this build calls
`clock_nanosleep` — never plain `nanosleep` — regardless of duration or
which thread calls it. The comment on the fallback arm is the key: POSIX's
`clock_nanosleep()` C library function has an unusual contract among libc
functions — *"these functions return the error number directly as the
function result"* (its own man page's wording), not the usual "-1, check
`errno`" pattern. musl's wrapper implements that correctly: it makes the raw
syscall, and if that syscall returned a negative Linux-ABI `-errno`, negates
it back to a positive value and returns *that* — 0 on success, or a positive
errno on failure, never -1.

Akuma never implemented syscall #115 (`clock_nanosleep`). It falls through
to `syscall/mod.rs`'s final catch-all match arm, which returns `ENOSYS`
unconditionally for anything it doesn't recognize. That raw `-ENOSYS`
(`-38`) becomes `+38` through musl's wrapper as described above. Rust's own
code only ever expects `nanosleep()` (its local name for this platform
function, not to be confused with the actual `nanosleep` syscall) to return
`0` or exactly `libc::EINTR` (`4`) — a signal interrupted the sleep, retry
the remaining duration — and asserts that directly: `assert_eq!(r,
libc::EINTR)`. `38 != 4`. The assertion fires immediately, unconditionally,
on the very first `thread::sleep` call any thread ever makes.

The panic's own reported numbers already name the two constants without
needing the source at all, in hindsight: `38` is `ENOSYS`, `4` is `EINTR`.

## The fix

Implemented `sys_clock_nanosleep` (`src/syscall/time.rs`; dispatched at
`src/syscall/mod.rs` as `nr::CLOCK_NANOSLEEP = 115`), modeled directly on
the existing `sys_nanosleep` (same park-in-a-loop-until-deadline shape,
`schedule_blocking` + `should_interrupt_blocking_syscall`), plus proper
`clockid`/`TIMER_ABSTIME` handling for the full POSIX contract rather than
just std's relative-sleep case:

- Relative (the common case, `flags == 0`): same math as plain `nanosleep`
  — `deadline = uptime_us() + requested_us`.
- Absolute (`flags & TIMER_ABSTIME`), `clock_id == 0` (`CLOCK_REALTIME`):
  converts the absolute wall-clock request into an uptime-based deadline via
  `crate::timer::utc_time_us()`, mirroring the exact conversion
  `sys_futex`'s `FUTEX_CLOCK_REALTIME` arm already does in
  `src/syscall/sync.rs`, and the `clock_id == 0` split
  `sys_clock_gettime` already uses.
- Absolute, any other `clock_id`: treated as an uptime-based deadline
  directly (same simplification `sys_clock_gettime` makes for every
  non-`CLOCK_REALTIME` id).

Returns the ordinary Linux **syscall** convention — `0` on success,
`-errno` (e.g. `EINTR = (-4i64) as u64`, matching the existing constant used
throughout this file) on failure. The "return the positive error number"
behavior is entirely a userspace libc detail (musl's wrapper does that
translation); the syscall itself needs no special-casing for it.

## Verification

The minimal one-liner reproduction above: now exits `0`, no panic.

A spawned-thread variant (`thread::spawn(|| { println!("about to sleep");
thread::sleep(Duration::from_millis(100)); println!("woke up fine"); })`)
now prints both lines and joins cleanly, where it previously panicked
between them every time.

Host unit tests (`cargo test --target <host-triple>`, whole workspace): all
suites pass, 0 failures — this syscall has no host-side unit test of its own
(it's inherently a target-only code path; nothing in `sys_clock_nanosleep`
is host-testable the way `crates/akuma-exec`'s pure logic is), but nothing
else regressed. `cargo build --release` and `cargo build --profile
release-smp-shared --features devbox-smoltcp,no-tests` both build clean.

## Why this matters beyond the one bug it was found through

`thread::sleep()` is about as common as Rust code gets — retry/backoff
loops, health checks, rate limiting, polling, watchdogs, test harnesses.
Every one of those has been silently broken on this kernel, for every Rust
binary that has ever run on it, for as long as the kernel has existed. It
almost certainly explains other "mysteriously silent" background threads in
past investigations that were never traced to a root cause, not just the
one watchdog that led here this session — see the Failure-D-specific
consequences of this discovery in
[`J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md`](J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md)
§7.17, including a clean A/B showing that a working `thread::sleep`-based
watchdog changes whether an *unrelated* SMP hang reproduces at all — a fix
in one place quietly changing a repro rate somewhere else, which is exactly
the kind of thing worth knowing before it causes confusion later.

## Background

- [`J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md`](J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md)
  §7.17 — the investigation this was found during, and the Failure D A/B
  this bug's fix accidentally perturbed.
- `docs/reference/subsystems/syscalls/*` — per-family syscall reference;
  `time`/`sync` cover the neighboring `sys_nanosleep`/`sys_futex` clock
  handling this fix's `clockid`/`TIMER_ABSTIME` logic mirrors.
