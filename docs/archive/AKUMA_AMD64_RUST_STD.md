# amd64: a real Rust `std` binary in ring 3, and what actually stopped it

**Date:** 2026-09-06
**Scope:** `userspace/amd64/ruststd`, `userspace/amd64/threadprobe`,
`amd64/src/{thread,futex}.rs`, the `a6` syscall-argument change, and the
`clone`/`futex`/`gettid`/`exit` dispatch arms.
**Status:** done. A `x86_64-unknown-linux-musl` Rust `std` binary runs all six
of its stages on this kernel, spawns and joins a thread, and exits with the
status a real Linux gives it. Boot suite 245/245.

The instruction this work was given predicted futex would be the wall and said
to measure first anyway. The measurement disagreed with the prediction, and
following the trace instead of the prediction is most of what made this cheap.

---

## 1. The probe, and why it is a `std` binary

`userspace/amd64/hello` and `fdprobe` are `#![no_std]` programs against
`x86_64-unknown-none` that make raw syscalls. They are good loader tests and
they cannot answer this question at all, because everything interesting about
running `rustc` happens **before `main`**: musl's `__libc_start_main`, the
static-PIE self-relocation, `__init_tp`, and `std::rt::init`. No hand-rolled
probe reproduces that prologue; the only way to test it is to link a real one.

So `userspace/amd64/ruststd` is an ordinary Rust binary — `println!`, `Vec`,
`std::fs`, `std::thread` — built for `x86_64-unknown-linux-musl`, static-PIE,
600 KiB stripped of DWARF. It is staged onto the disk by `amd64/mkdisk.sh`
rather than `include_bytes!`d, and it is best-effort in that script: the build
host is Apple Silicon, so `cc` is Apple clang and cannot emit ELF. Homebrew
`musl-cross` supplies `x86_64-linux-musl-gcc`, which can. A tree without it
still gets a bootable image, minus this probe.

It reports by **printing before each stage**, not only by exiting, because the
question it answers is *which stage stops it* and a program that dies at stage 6
has no exit status to report. Stages are ordered by how early Rust needs them.

## 2. The measurement

`INIT=/bin/ruststd STRACE=1`. The whole program, 20 distinct syscalls:

```
158 arch_prctl(ARCH_SET_FS)   0     218 set_tid_address           1
  7 poll(fds 0-2)             0      13 rt_sigaction × 4          0
131 sigaltstack             -38      14 rt_sigprocmask × n        0
 12 brk × 2                 -38       9 mmap × n                 ok
 10 mprotect                  0       1 write                    ok
228 clock_gettime             0       2 open / 5 fstat / 0 read / 3 close / 11 munmap
 56 clone(0x7d0f00)         -38     186 gettid                  -38
231 exit_group(101)
```

Stages 1 through 5 — `println!`, the heap, `env::args`, `Instant::now`,
`fs::read` — **all passed on the unmodified kernel**. Four syscalls answered
`ENOSYS`, and exactly one of them was fatal:

| syscall | result | consequence |
|---|---|---|
| `brk` (12) | `ENOSYS` | none. musl's mallocng falls back to `mmap`. |
| `sigaltstack` (131) | `ENOSYS` | none. musl carries on without an alt stack. |
| `gettid` (186) | `ENOSYS` | cosmetic: `thread 'main' (18446744073709551615)`. |
| **`clone` (56)** | **`ENOSYS`** | **fatal.** musl reports `EAGAIN`; `std` panics. |

```
failed to spawn thread: Os { code: 11, kind: WouldBlock, ... }
```

### 2a. futex was never called. Not once.

`grep -c 'nr=202'` over the whole trace: **0**.

This is the finding that changed the plan. The prediction was reasonable — Rust
`std` does touch futex on lazy-init paths — but musl only *syscalls* into a
futex on contention, and a single-threaded program has nothing to contend with.
`std`'s `Once`, `Mutex` and `Stdout` lock all resolved uncontended, in userspace,
every time.

So futex was not the wall and could not have been: **it is unreachable until
`clone` works.** Implementing it first would have been implementing something
with no way to test it. The order in `AKUMA_AMD64_STREAMLINING.md` §11.2 — "the
`clone` side … then wire `akuma-syscalls-sync`" — was right, and the reason is
stronger than the one given there.

### 2b. The A/B against Linux

The identical binary (`sha256` compared, not "the same source"):

```
docker run --platform linux/amd64 -v …:/probe:ro alpine:3.20 /probe/ruststd
  → all 6 stages, exit=6
```

So the divergence is the kernel, not the probe. `strace` inside that container
is useless — Rosetta scrambles its syscall decoding into
`syscall_0xffffc00000000000(…)` — so the reference trace came from qemu-user's
own decoder on a native arm64 container (`qemu-x86_64 -strace`). That is worth
recording as technique: **on Apple Silicon, `docker --platform linux/amd64` runs
the binary correctly and traces it wrongly.**

The reference trace is what supplied the target shape for a working thread:

```
clone(CLONE_VM|CLONE_FS|CLONE_FILES|CLONE_SIGHAND|CLONE_THREAD|CLONE_SYSVSEM|
      CLONE_SETTLS|CLONE_PARENT_SETTID|CLONE_CHILD_CLEARTID|CLONE_DETACHED,
      child_stack=…, parent_tidptr=…, tls=…, child_tidptr=…) = 12
futex(<child_tidptr>, FUTEX_PRIVATE|FUTEX_WAIT, 2, NULL)
futex(<child_tidptr>, FUTEX_PRIVATE|FUTEX_WAKE, 1, …) = 1
exit(0)
```

**Two futex calls for a whole thread lifecycle.** That is the entire surface a
`spawn` + `join` needs.

## 3. What was already done, and was listed as work

§11.2 named "per-task FS/GS base save/restore on switch (currently *not*
saved)". It **is** saved: `UserCtx::fs_base` and `gs_base` are per-task and the
scheduler `wrmsr`s both on every switch (`sched.rs`, and `sys_arch_prctl`'s own
comment). The CoW `fork` work did it, and the survey predates it. Half of §11.2
was already closed and the list did not know.

`Task::space_root` has been per-task since Stage I, so two tasks sharing one
address space needed **no scheduler change at all**. That is the whole reason
this was a day and not a week.

## 4. What was built

### 4a. `amd64/src/thread.rs` — `clone(CLONE_VM|CLONE_THREAD)`

A thread is a task, not a process, and the module is mostly *subtraction* from
`fork`:

| `fork` gives the child | a thread gets |
|---|---|
| a new `AddressSpace`, CoW-shared | the parent's `space_root`, verbatim |
| its own `FrameSet` | none — it owns no frames |
| a `PROCS` slot + `Spawn` record | the parent's, shared |
| its own fd routing | the parent's, shared |
| `%fs` copied from the parent | its own, from `CLONE_SETTLS` |
| a `waitpid`-visible exit status | a futex wake on `clear_child_tid` |

**`CLONE_VM` never touches the CoW share pass.** `Process::fork_from` demotes
the parent's live PTEs so the next write faults and copies; a thread that went
through that would get a private copy of every page it touched and the two sides
would silently diverge. `sys_clone_thread` never constructs a `Process` and
never calls `fork_from` — it treats `space_root` as an opaque number. That also
means threads add nothing to the target's `invlpg`-has-no-shootdown problem:
nothing here demotes a PTE. **CoW `fork` remains SMP=1 only**
(`AKUMA_AMD64_COW.md`); threads neither help nor hurt that.

One entry function serves every thread, unlike `proc_entry_for`'s sixteen
hand-written trampolines, because `UserCtx` gained a `thread_slot` field —
there is somewhere to put the index now.

### 4b. `amd64/src/futex.rs` — the effects half

Not a new crate. `crates/akuma-syscalls-sync` is the crate, it is the one the
AArch64 kernel uses, and the dep diff between `Cargo.toml` and `amd64/Cargo.toml`
showed it was simply absent from the amd64 manifest. It owns the op decode, the
`(tgid, uaddr)` key namespace, the waiter table, the deadline algebra and the
`WAKE_OP` opcode — all host-tested.

**Why the AArch64 kernel half was not reused.** It is
`akuma-syscalls-glue::sync`, 947 lines, and 22 of its references are
`akuma_exec::threading` — that crate's park/wake, thread states and
`tpidr_el1`. Measured, not assumed:

```
cargo check -p akuma-syscalls-glue --target x86_64-unknown-none
  → UserAddressSpace::ttbr0, ::is_shared, ::invalidate_icache_for_page_va,
    map_user_page_tracked, trait UserPages … (the aarch64 page-table walker)
```

Reusing it means porting `akuma-mmu` first, which is §11.4's prerequisite, not
§11.2's. That split is the crate's own stated design — "the kernel performs
every effect" — and here the effect is *parking a thread on a scheduler the two
targets do not share*.

The wait is a **poll, not a park**, because this target has no per-thread waker
(`net::park_until` says so; `wait4` already spins the same way). `FUTEX_WAKE`
does one thing: it takes the waiter off the table. The waiter notices on its
next poll. That cannot lose a wake — the removal is durable state, not an edge —
and it costs one round of the round-robin per waiter per tick. The poll loop
**allocates nothing**: membership is checked through `WaiterTable::iter`, which
borrows, where the obvious spellings (`queue()`, `locate_and_take()`) each
allocate or churn a `BTreeMap` entry per tick.

### 4c. `a6`: the sixth syscall argument

`syscall_entry` dropped Linux's `a6` because nothing needed one. `futex` takes
six and the sixth is not optional — Rust `std` emits `FUTEX_WAIT_BITSET` for
every timed wait and `val3` *is* the bitset, which a zero would make `EINVAL` by
the decode's own rule.

The fix is one instruction. `sub rsp, 8` (an alignment pad) became `push r9`:
the same 8 bytes, in the same place, now holding System V's seventh argument at
`[rsp]`. `r9` still holds the user's `a6` at that point — the register shuffle
below is what clobbers it, and moving the push after `mov r9, r8` would push
`a5` twice. `mmap`'s offset is the other user, still unreached.

### 4d. `exit` vs `exit_group`

They were one arm. For a single-threaded process they are the same call; for a
threaded one they could not be more different — `exit` ends the calling thread,
`exit_group` ends the group. musl's `pthread_exit` uses the first.

## 5. The bug the design had, and how it was caught

A thread parked in an **untimed** `FUTEX_WAIT` has already passed syscall entry,
so the "your group is exiting" check there can never fire for it again. With
`exit_group` on one side and `thread::drain` waiting for it on the other, both
loops make progress and neither terminates. Nothing looks stuck, which is the
worst shape a deadlock can take.

Two things close it, and the second is the more valuable:

1. The futex wait loop checks `should_leave_now` itself and returns `EINTR`.
2. `drain` is **bounded** (100,000 rounds) and prints
   `[thread] DRAIN INCOMPLETE: N thread(s) still live`. An unbounded wait turns
   any future bug in the leave path into a boot that hangs with no output.

`threadprobe` leaves a second thread parked forever and then `exit_group`s, so
the path is exercised on every boot. Falsified by disabling fix (1):

```
  [thread] DRAIN INCOMPLETE: 1 thread(s) still live in proc slot 5
  thread: no thread outlived the process   [FAIL] got 0x1 want 0x0
  Akuma/amd64 self-test: 244 passed, 1 FAILED
```

— a named failure instead of a silent hang, which is what the bound bought.

## 6. Result

```
INIT=/bin/ruststd STRACE=1
  [rs] 1 println … [rs] 6 thread
  clone(...) flags=VM|FS|FILES|SIGHAND|THREAD|SYSVSEM|SETTLS|
              PARENT_SETTID|CHILD_CLEARTID|0x400000  -> 7
  task=3 gettid -> 7
  task=2 futex(0x100217b70, op=0|PRIV, val=2)      [blocks]
  task=3 futex(0x100217b70, op=1|PRIV, val=1) -> 1
  task=2                                       -> 0
  task=2 futex(0x100773f8, op=0, val=7)            [blocks — pthread_join]
  task=3 exit(0)                                   [kernel clears + wakes]
  task=2                                       -> 0
  [rs]   thread returned 36
  [rs] all 6 stages complete
  exit_group(6)
```

The two waits are the real contract. The first is musl's thread-start
handshake, woken by the child. **The second is `pthread_join`, and nothing in
userspace wakes it** — the kernel does, by zeroing the `CLONE_CHILD_CLEARTID`
word and waking one waiter on it. Getting that order backwards (wake before
clear) is the classic join-hangs-forever bug; `thread::teardown` states the
ordering as its contract.

Exit status 6 on Akuma, exit status 6 on Linux, from the same bytes.

- Boot suite: **245 passed, 0 failed** (was 240; `thread_test` adds 5).
- Host crate tests: green.
- `busybox ls -l /bin` unchanged — the `a6` asm change touches every syscall,
  so this was the regression that mattered.

## 7. Remaining divergences, deliberately not fixed

| what | status | why not now |
|---|---|---|
| `sigaltstack` → `ENOSYS` | tolerated by musl | signals are §11.7. An honest `ENOSYS` beats another believed stub. |
| `rt_sigaction` → `0` | **a stub that lies** | Rust installs a `SIGSEGV` handler for stack-overflow detection and believes it worked. A guard-page overflow will not be reported as one. §11.7. |
| `brk` → `ENOSYS` | tolerated | musl uses `mmap`. Real cost: it feeds the global monotonic bump, §11.4. |
| `CLOCK_MONOTONIC` granularity | 10 ms | it is `lapic::ticks() * US_PER_TICK`. `Instant::elapsed` over a 100 k-iteration loop reads **0 ns** here and 11 µs on Linux. Correct and monotonic, just coarse. |
| `FUTEX_REQUEUE` / `WAKE_OP` | implemented, unexercised | nothing in the trace reaches them. They cost ~30 lines over the crate's own algebra and would otherwise be `ENOSYS` at the moment a `Condvar` first broadcasts. |
| a thread in a syscall-free ring-3 loop | unreachable at `exit_group` | no signals, so nothing can interrupt it. `drain`'s bound is the containment. |

## Background

- `docs/archive/AKUMA_AMD64_STREAMLINING.md` §11 — the blocker list this closes
  one item of, and updates.
- `docs/archive/AKUMA_AMD64_COW.md` — §11.1, closed 2026-09-06, and the SMP=1
  constraint threads inherit without adding to.
- `docs/archive/AKUMA_AMD64_DYNAMIC_LINKING.md` — §11.6, closed the same day;
  the `PT_INTERP` path this probe's static-PIE variant does not exercise.
- `crates/akuma-syscalls-sync` — the decisions, and the incident table
  explaining why each one is a host test.
- `docs/reference/subsystems/syscalls/sync.md` — the AArch64 side's futex
  contract, which this target now shares the algebra of and not the effects.
