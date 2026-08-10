# Cooperative scheduling audit: where (if anywhere) the kernel still cooperates

**Date:** 2026-08-11. **Scope:** kernel scheduler in `src/` and
`crates/akuma-exec/src/threading/`, plus the userspace rump fiber backend under
`userspace/rumpkernel/rumpuser/`. **Status:** survey only — no code changed.
This doc exists to answer "do we still use cooperative scheduling anywhere?"
in one place, with file:line citations.

**One line:** the kernel scheduler is **preemptive** (10 ms timer → SGI,
[`docs/reference/subsystems/scheduler.md`](../reference/subsystems/scheduler.md)).
Cooperative scheduling survives as (1) a 100 ms-bounded cooperative slice on
**thread 0 / the BSP idle thread**, (2) a per-thread `cooperative: bool` flag
plus a `spawn_fn_cooperative` API whose **only callers are tests**, and (3) a
**separate userspace cooperative fiber scheduler** in the rump backend, which
is not the kernel scheduler at all. No production kernel thread other than
thread 0 is cooperative; system threads, user/session threads, and secondary
cores' idle threads are all explicitly marked preemptible. **Thread 0's
cooperative flag protects a removed workload** — the in-kernel SSH server
deleted 2026-08-10 — so the flag, the async executor it shelters (now vestigial:
pins one `memory_monitor` future), and the test-only API are all now removal
candidates (see §6, §7).

## 0. TL;DR table

| Site | Cooperative? | Live in production? | Where |
|---|---|---|---|
| Thread 0 / BSP idle thread | **Yes**, 100 ms slice | **Yes** (load-bearing) | `threading/mod.rs:2248` |
| `spawn_fn_cooperative` API | Optional | **No callers** outside tests | `threading/mod.rs:2872` |
| Secondary cores' idle threads | **No** (explicitly cleared) | n/a | `threading/mod.rs:1838, 1864` |
| System threads (`spawn_system_thread_fn`) | **No** | Yes | `threading/mod.rs:3891` |
| User/session processes | **No** | Yes | `spawn_user_thread_fn_with_options` default |
| `yield_now()` inside `block_on` / async loops | Yield primitive, not a scheduling class | Yes | `threading/mod.rs:2221` |
| Rump fiber scheduler | **Yes** (userspace, separate) | Yes (default backend) | `userspace/rumpkernel/rumpuser/src/fiber.rs` |

## 1. Method

```
rg -n 'cooperative'               src crates userspace/rumpkernel --type rust
rg -n 'coop'                       src crates userspace/rumpkernel --type rust
rg -n 'yield.*sched|sched.*yield|voluntary.*yield'  src crates --type rust
rg -n 'spawn_fn_cooperative|spawn_fn_with_options|spawn_user_thread_fn_with_options'
                                   src crates --type rust
```

then manual read of every hit's enclosing function to check (a) whether the
`cooperative` flag is being *set*, *cleared*, *read*, or merely mentioned in a
comment, and (b) whether the call site is reachable outside the boot self-test
harness. Only `spawn_fn_cooperative` callers were counted as "uses of the
cooperative spawn API"; reads of the flag inside `schedule_indices` are the
mechanism, counted separately.

**Excluded as out of scope** (matched the greps, not the goal):

- Comment-level mentions of "cooperative" in `src/smp_shared.rs`,
  `src/rump_proxy.rs`, `crates/akuma-exec/src/process/{lifecycle,spawn}.rs`,
  `docs/`, etc. — they describe wait patterns or the rump backend, not a kernel
  scheduling class. Cited below where load-bearing.
- `sched_yield` syscall (`NR 124`, `crates/akuma-exec/src/process/stats.rs:154`)
  — a Linux ABI voluntary yield, not a scheduling class. It calls
  `threading::yield_now()` (stays READY) and returns.
- The `cooperative` field of `ProcessSnapshot` / `ThreadInfo`
  (`threading/mod.rs:4499,4513,4579,4597`) — read-only diagnostic surface for
  `list_kernel_threads`; reflects the flag, doesn't set policy.

## 2. The mechanism (still wired in)

The cooperative scheduling class is real and load-bearing as a mechanism, even
though only one production thread is a member.

- **Per-slot flag** `ThreadSlot.cooperative: bool`
  (`crates/akuma-exec/src/threading/types.rs:194`), default `false`
  (`:207`).
- **Slice length** `COOPERATIVE_TIMEOUT_US: u64 = 100_000`
  (`crates/akuma-exec/src/threading/types.rs:31-32`) — 100 ms. Set into
  `slot.timeout_us` whenever a slot is marked cooperative
  (`threading/mod.rs:1319, 2249, 4089`).
- **Honored by `schedule_indices`** at `threading/mod.rs:2513-2526`:

  ```rust
  // For timer-triggered preemption, check if the current thread is cooperative.
  let current_state = THREAD_STATES[current_idx].load(Ordering::SeqCst);
  if !voluntary && current_cooperative && current_state == thread_state::RUNNING {
      let timeout = current_timeout_us;
      if timeout > 0 && current_start_time_us > 0 {
          let elapsed = now.saturating_sub(current_start_time_us);
          if elapsed < timeout { return None; }   // skip this tick
      } else {
          return None;                            // no timeout → never preempt
      }
  }
  ```

  I.e. an **involuntary** (timer-tick) preemption is suppressed while a
  cooperative RUNNING thread is inside its 100 ms slice. **Voluntary**
  `yield_now()` and `schedule_blocking()` always switch — `voluntary=true`
  bypasses the block above (`threading/mod.rs:2439, 2509`). The wake-pass that
  flips WAITING→READY runs unconditionally on every scheduler entry
  (`:2473-2505`), so cooperative threads do not delay wakeups, only the context
  switch off themselves.

- **Public spawn API**:
  - `spawn_fn_cooperative` (`threading/mod.rs:2872`) → `spawn_fn_with_options(f, true)`
  - `spawn_fn` (`:2864`) → `spawn_fn_with_options(f, false)` (the preemptible default)
  - `spawn_fn_with_options` (`:2882`) → `spawn_user_thread_fn_with_options`
  - `spawn_user_thread_fn_with_options` (`:3950`) → `spawn_user_thread_fn_internal(f, cooperative, ...)`

  The `cooperative` parameter is plumbed all the way down to
  `pool.slots[slot_idx].cooperative` and `timeout_us`
  (`threading/mod.rs:4087-4089`).

## 3. Production use (exactly one site)

### 3.1 Thread 0 / BSP idle thread — `cooperative = true`

`ThreadPool::init` (`crates/akuma-exec/src/threading/mod.rs:2238`) marks slot 0
cooperative and stamps the 100 ms timeout:

```rust
// Slot 0 is the idle/boot thread (uses boot stack, never terminated)
// It runs the async executor and network runner, so mark it cooperative
// to avoid preemption during critical I/O operations. It still gets
// preempted after the timeout to allow other threads to run.
THREAD_STATES[IDLE_THREAD_IDX].store(thread_state::RUNNING, Ordering::SeqCst);
self.slots[IDLE_THREAD_IDX].cooperative = true;                      // :2248
self.slots[IDLE_THREAD_IDX].timeout_us = COOPERATIVE_TIMEOUT_US;     // :2249
```

This is the **only** production path that leaves a slot cooperative. The
rationale in the code comment (line 2244): slot 0 runs the async executor +
network runner and must not be preempted mid-poll. The 100 ms ceiling means it
is not pure cooperation — the timer will eventually force a switch — but
inside its slice it is unpreemptible by involuntary ticks.

> **Stale rationale (2026-08-10).** The comment dates from when slot 0 drove
> the **in-kernel SSH server** (`src/ssh/server.rs`, removed 2026-08-10 — see
> `docs/archive/BUILTIN_SSH_REMOVAL.md`): the SSH thread ran a `block_on`
> future that called `iface.poll()` under the `NETWORK` spinlock, and a timer
> preemption mid-poll could SGI-switch to a peer that re-acquired `NETWORK` on
> the same core → single-CPU spinlock deadlock (`docs/archive/SSH_STAGGERING.md`,
> `docs/runbooks/debug-ssh-latency.md` — both describe `src/ssh/server.rs`,
> not the userspace sshd). With in-kernel SSH gone, the async executor pins
> exactly **one** future (`memory_monitor`, a periodic stats printer — see
> `src/main.rs:1377,1648`), and the network poll services userspace sockets
> via the normal blocking socket layer. The flag now protects no live
> workload; see §6.

### 3.2 Everything else is explicitly cleared

| Path | What it does | Line |
|---|---|---|
| `make_idle_preemptible` | Clears `cooperative` + `timeout_us` on a secondary's idle thread, so the timer tick round-robins off idle | `threading/mod.rs:1835-1839` |
| `adopt_current_as_core_idle` | A secondary's adopted idle starts preemptible: `cooperative = false`, `timeout_us = 0` | `threading/mod.rs:1864-1865` |
| `spawn_system_thread_fn` (interior) | System threads: `pool.slots[slot_idx].cooperative = false` | `threading/mod.rs:3891` |
| `cleanup_terminated_internal` | On slot recycle: `pool.slots[i].cooperative = false` | `threading/mod.rs:1619` |
| `spawn_user_thread_fn_internal` | User/session threads take `cooperative` from the caller; the only production caller path (`spawn_fn` → `spawn_fn_with_options(f, false)`) passes `false` | `threading/mod.rs:4014, 4087` |

The docstring at `threading/mod.rs:2442-2443` confirms the intent: *"Cooperative
threads (thread 0): Only switch after timeout elapses. Non-cooperative threads
(sessions, user processes): Always preemptible."*

## 4. Test-only use of the cooperative spawn API

`spawn_fn_cooperative` has **three call sites, all in boot self-test code**:

| File:line | Test | What it checks |
|---|---|---|
| `src/tests.rs:2338` | `test_spawn_cooperative` | A cooperative thread can be spawned and runs to completion |
| `src/tests.rs:2477` | `test_mixed_cooperative_preemptible` | One cooperative + one preemptible thread both finish; round-robin still serves the preemptible one |
| `src/process_tests.rs:802` | `test_top_cpu_stats_column` | Spawns a cooperative kernel thread to drive CPU stats sampling for the `top` CORE column |

`test_cooperative_timeout` (`src/tests.rs:2001`, registered at `:358`) exercises
the 100 ms slice directly but does not use `spawn_fn_cooperative` — it mutates
the slot flag by hand. The SMP test `test_smp_shared_cooperative_wait`
(`src/process_tests.rs:1930`) is named for the *cooperative wait* pattern
(yielding the CPU while holding the BKL across `exec_with_io`), not the
cooperative scheduling class; it does not spawn a `cooperative=true` thread.

**Conclusion:** the `spawn_fn_cooperative` API is exercised only by tests. No
production caller exists outside slot 0's hard-coded initialization.

## 5. Related but distinct cooperative machinery (not the kernel scheduler)

These are mentioned for completeness because the grep hits them; none of them
is a kernel scheduling class.

### 5.1 `yield_now()` as a cooperative yield primitive

`threading::yield_now()` (`threading/mod.rs:2221`) is a **voluntary** yield:
the thread stays `READY` and is switched back in promptly, with no SGI fired.
Per `scheduler.md:208-210`, it is the primitive used inside `block_on` and any
cooperative async loop. It is a *yield primitive*, not a scheduling class —
any thread (cooperative or not) may call it, and the recipient is scheduled
under the normal preemptive rules.

The load-bearing constraint around it (`scheduler.md:200-204`,
`runbooks/debug-ssh-latency.md`): `block_on` **must** use `yield_now()` and not
`schedule_blocking()`, because `schedule_blocking` flips the thread to WAITING
and fires an SGI; if that SGI lands while the network thread holds the
`NETWORK` spinlock, the re-acquire deadlocks. That constraint is unrelated to
the `cooperative` flag — it applies to the network thread, which is
preemptible.

### 5.2 Cooperative wait loops (the BKL pattern)

Several call sites and docs refer to a "cooperative wait"
(`src/smp_shared.rs:385-419`, `src/process_tests.rs:179, 1918-2045`,
`crates/akuma-exec/src/process/lifecycle.rs:29`, `src/syscall/fs.rs:687`). This
is a *pattern* — a kernel thread holds the BKL and yields in a loop waiting
for a child/peer — not the scheduling class. The M5c step-2 fix
(`docs/reference/subsystems/smp-shared.md:86`,
`docs/runbooks/debug-smp.md:54`) specifically prohibits holding the BKL across
such a wait: a BKL-free secondary would claim a thread RUNNING without the BKL
while the BSP spins in the cooperative wait. The `cooperative` flag is not
read on this path.

### 5.3 Rump fiber scheduler — a real userspace cooperative scheduler

`userspace/rumpkernel/rumpuser/src/fiber.rs` (~580 lines) is a Rust port of
NetBSD's `rumpfiber.c`. It collapses rump's ~19 pthread kthreads onto **one OS
thread** and switches between them cooperatively. It is the default backend
(`threads_fiber` cargo feature, on by default; `--no-default-features` selects
the legacy pthread backend). Per
[`docs/reference/subsystems/rump-stack.md:31`](../reference/subsystems/rump-stack.md),
it is "Fiber (cooperative) backend" and is what made the rump stack stable.

This is a **userspace cooperative scheduler**, running in the `rumpkernel`
process. It calls into the kernel scheduler only via the normal blocking
primitives (`schedule_blocking`, `yield_now`) when it needs to wait on I/O. It
is not the kernel scheduler and is unaffected by anything in §2-§4.

The `rumpuser_akuma_yield` shim (`fiber.rs:828, 1159, 1163`) is the bridge: a
cooperative fiber that needs to wait parks itself on the kernel scheduler
(via `schedule_blocking`/`yield_now`), which schedules other kernel threads
preemptively while the fiber is parked.

## 6. Could the `cooperative` flag be removed?

**Yes.** The flag's only production user is thread 0, and thread 0's
invariant — don't preempt the async executor + network runner mid-poll — was
load-bearing only for the **in-kernel SSH server** removed 2026-08-10
(`docs/archive/BUILTIN_SSH_REMOVAL.md`). The two docs that documented the
SSH-staggering regression (`docs/archive/SSH_STAGGERING.md`,
`docs/runbooks/debug-ssh-latency.md`) both describe `src/ssh/server.rs`,
which no longer exists; the userspace sshd (`userspace/sshd`) is a normal
scheduled process and never rode the kernel's async executor.

What slot 0 actually carries today, post-removal:

- **The async executor** (`src/main.rs:1392`) pins exactly **one** future,
  `memory_monitor` (`src/main.rs:1648`) — a `loop` that prints heap/RAM
  stats every `MEM_MONITOR_PERIOD_SECONDS` via `Timer::after().await`. It is
  vestigial: `config.rs:239`'s "pins 6 complex futures (SSH, HTTP, network)"
  comment is stale, left over from when SSH/HTTP ran in-kernel. Preempting
  this mid-poll delays a stats print by ~10 ms; nobody notices.
- **The network poll** (`smoltcp_net::poll()` at `src/main.rs:1552`) still
  services the userspace sshd's sockets, but preempting it mid-poll is a
  **latency** question (packets drain on the next thread-0 slot, ~10 ms out),
  not the old single-CPU `NETWORK`-spinlock deadlock (which needed the SSH
  thread to re-acquire `NETWORK` from the same core). The
  `NETWORK_THREAD_RATIO=4` proportional scheduler boost
  (`threading/mod.rs:2528-2544`) keeps the poll frequent enough for userspace
  sshd latency, and that boost is **independent of the `cooperative` flag** —
  it keys off `NETWORK_THREAD_ID`, registered by `run_async_main`
  (`src/main.rs:1192`).

So the cooperative flag on thread 0 is protecting a workload that no longer
exists. The path to full removal is open; the only remaining consideration is
empirical (does userspace sshd echo latency tolerate thread-0 poll at
timer-tick granularity rather than the 100 ms slice?) — and since the
proportional boost already schedules the poll every 4th tick regardless of the
flag, the answer is very likely yes. A single boot with
`slots[IDLE_THREAD_IDX].cooperative = false` and an SSH echo-latency
measurement would confirm.

Plausible removals:

- **Make slot 0 preemptible** (`cooperative = false` in `ThreadPool::init`)
  and measure userspace sshd echo latency. If it holds, the flag is dead.
- **Inline `memory_monitor`** as a timestamp-gated block in the existing main
  loop (the loop already has `LAST_HEARTBEAT_US`/`LAST_PSTATS_US`/
  `LAST_RECLAIM_US` patterns), deleting the async executor, the `Timer`
  Future impl (`src/kernel_timer.rs:248-272`), the `schedule_wake` it's the
  sole caller of, and the waker/`Context` setup (`src/main.rs:1380-1386`).
  This removes the last `async` from the kernel and makes the §3.1 rationale
  moot.
- **Keep the flag but delete `spawn_fn_cooperative`.** The API has no
  production callers; removing it drops one code path without changing slot 0.

The mechanism is small (~10 lines of scheduler logic), the API is one trivial
wrapper, and the current behavior is tested (`test_cooperative_timeout`,
`test_mixed_cooperative_preemptible`). The risk bar for full removal is now
"measure sshd echo latency," not "rewrite the boot/async path."

## 7. Removal cost (2026-08-11)

"How much code can we cut if we remove cooperative scheduling?" Two scopes,
tallied from the citations in §2-§5. Compiled impact is dominated by BSS
(struct fields × 64 slots), not text — the removed functions are thin generic
wrappers the compiler already inlines away.

### Scope A — delete the unused API surface (no behavior change)

Safe because `spawn_fn_cooperative` has no production callers (§4) and
`make_idle_preemptible` has **zero callers anywhere** (grep confirms; dead
code today). Thread 0 stays cooperative; the flag is untouched.

| What | File:lines | LOC |
|---|---|---|
| `spawn_fn_cooperative` (whole fn) | `threading/mod.rs:2871-2877` | 7 |
| `spawn_fn_with_options` (whole fn; `spawn_fn` calls down one level) | `threading/mod.rs:2879-2886` | 8 |
| `spawn_user_thread_fn_with_options` (self-described "legacy wrapper") | `threading/mod.rs:3949-3955` | 7 |
| `make_idle_preemptible` (whole fn + 12-line docstring) | `threading/mod.rs:1823-1840` | 18 |
| `test_cooperative_timeout` + registration | `src/tests.rs:2001-2018, 358` | 19 |
| `test_spawn_cooperative` + registration | `src/tests.rs:2332-2371, 365` | 41 |
| `test_mixed_cooperative_preemptible` + `COOP_THREAD_DONE` + `set/get_coop_done` helpers + registration | `src/tests.rs:2443-2559, 367` | 119 |
| **Scope A total** | | **~219 LOC** |

`test_thread_last_core_tracked` (`src/process_tests.rs:799-845`) is *edited*
(s/spawn_fn_cooperative/spawn_fn/ at `:802`), not removed — it tests
last-core tracking, not cooperation. Net 0 lines.

### Scope B — full removal (slot 0 made preemptible)

Everything in Scope A, **plus** the `cooperative` flag, the `timeout_us` field
(orphaned once the cooperative-skip block in `schedule_indices` is gone — it is
the field's only reader), the constant, the diagnostic surface, and thread 0's
`cooperative = true` in `ThreadPool::init`. Per §6, the blocker that used to
gate this (the in-kernel SSH server's mid-poll `NETWORK`-spinlock deadlock) was
removed 2026-08-10; the remaining consideration is empirical — measure
userspace sshd echo latency with slot 0 set `cooperative = false`, and if it
holds, the flag is dead.

The async executor itself is vestigial and removable in the same pass or a
preceding one: inline `memory_monitor` (`src/main.rs:1648-1780`) as a
timestamp-gated block in the existing main loop (the loop already has the
`LAST_HEARTBEAT_US`/`LAST_PSTATS_US`/`LAST_RECLAIM_US` pattern at
`src/main.rs:1408,1419,1503`), deleting the `Timer` Future impl
(`src/kernel_timer.rs:248-272`), `schedule_wake` (`src/kernel_timer.rs:123`,
sole caller is `Timer::poll`), and the waker/`Context` setup
(`src/main.rs:1377-1386`). That removes the last `async` from the kernel
entirely. Not counted in the table below — it's a separate ~170 LOC win in
`src/main.rs` + `src/kernel_timer.rs`.

| What | File:lines | LOC |
|---|---|---|
| `COOPERATIVE_TIMEOUT_US` const + doc | `threading/types.rs:31-32` | 2 |
| `ThreadSlot.cooperative` + `timeout_us` + their `empty()` inits | `threading/types.rs:194,196,207,209` | 4 |
| `KernelThreadInfo.cooperative` | `threading/types.rs:230` | 1 |
| `ThreadPoolSnapshot.cooperative` | `threading/types.rs:241` | 1 |
| `schedule_indices` cooperative-skip block + the two local reads it needs | `threading/mod.rs:2457-2458, 2513-2526` | 16 |
| `spawn_user_thread_initializing` `cooperative` param + 3 call-site `false` args | `threading/mod.rs:1154`; `process/mod.rs:2783,2944,3212` | 4 |
| `spawn_user_closure_initializing` param + body writes | `threading/mod.rs:1262,1317,1319` | 3 |
| `cleanup_terminated_internal` `cooperative`/`timeout_us` clears | `threading/mod.rs:1619-1620` | 2 |
| `adopt_current_as_core_idle` clears | `threading/mod.rs:1864-1865` | 2 |
| `ThreadPool::init` slot-0 set + comment | `threading/mod.rs:2244-2249` | 6 |
| `spawn_system_thread_fn` `timeout_us = 0` | `threading/mod.rs:3893` | 1 |
| `spawn_user_thread_fn_internal` param + body + comment | `threading/mod.rs:4012,4014,4087,4089` | 4 |
| Diagnostic surface: `list_kernel_threads` + `dump_stack_info` | `threading/mod.rs:4499,4513,4520,4572,4579,4597` | 6 |
| **Scope B extra** (on top of Scope A) | | **~52 LOC** |
| **Scope B total** | | **~271 LOC** |

`start_time_us` (also on `ThreadSlot`) is **not** removed — it stays because
`commit_switch` (`threading/mod.rs:2677,2704`), the network-boost path
(`:2554`), and `idle_halt`'s halted-quantum correction (`:2997-2998`) all read
it for CPU accounting unrelated to the cooperative class.

### Compiled / runtime impact (estimated; not measured)

| Category | Impact |
|---|---|
| **Text (code)** | Negligible. The four removed functions in Scope A are one-line tail calls into `spawn_user_thread_fn_internal`; the compiler already inlines them. The `schedule_indices` block (Scope B) is ~14 lines of branch logic on the timer-tick hot path — removing it saves a branch + two field loads per involuntary preemption, roughly 20-40 bytes of instructions. |
| **BSS (statics)** | `ThreadPool.slots: [ThreadSlot; 64]`. Removing `cooperative: bool` (1B) + `timeout_us: u64` (8B) per slot saves **64 × 9 = 576 bytes** of BSS. `ThreadPoolSnapshot.cooperative: [bool; 64]` is stack-allocated transiently in `list_kernel_threads`; removing it shrinks that frame by 64 bytes (no BSS). `KernelThreadInfo.cooperative` is heap-allocated per entry in a `Vec` only when `list_kernel_threads` is called; transient. |
| **Total** | **~600 bytes BSS + ~30 bytes text.** Against the extreme-size 4.0 MB floor this is ~0.015%. Not measurable on a single build; the floor fluctuates more than this between toolchain versions. |
| **Hot path** | One branch removed from every involuntary timer-tick preemption (`schedule_indices`). The branch is predicted-taken (cooperative threads are rare — only thread 0), so the runtime win is on the order of a single mispredict-free instruction per tick. Not perceptible. |

**Honest summary:** the win is **source clarity**, not size. ~220 lines of test
+ dead-wrapper code can go today (Scope A, zero risk). The remaining ~50 lines
of flag plumbing (Scope B) used to be gated on the in-kernel SSH server's
mid-poll deadlock; that server was removed 2026-08-10, so Scope B is now gated
only on "measure userspace sshd echo latency once." A further ~170 LOC of
vestigial async machinery (the executor + `Timer` + `schedule_wake`) can go in
the same pass or a preceding one. If the goal is binary size, none of this is
the target (sub-KB combined); if the goal is removing dormant concepts, both
cooperative scheduling and the last kernel `async` are now cheap to delete.

## 8. Does this let us drop `embedded-io-async`?

Asked because the executor removal deletes the last `async fn` poll site; the
natural follow-up is "can the async trait crate go too?" Answer: **partly** —
it splits kernel vs userspace, and the executor removal itself does not touch
the crate (the executor polls `memory_monitor`, which does not use these
traits).

### Method

```
rg -n 'embedded-async-io|embedded_async_io'            # none — name was slightly off
rg -n 'embedded-io-async|embedded_io_async' --type toml --type rust
rg -n 'tcp_connect|exec_streaming'                       --type rust
rg -n 'smoltcp_net::TcpStream|TcpStream::new'  src       # kernel consumers of the async TcpStream
```

### Where the crate is declared

| Crate | Line | Notes |
|---|---|---|
| root `Cargo.toml` | `:426` | kernel workspace dep |
| `crates/akuma-exec/Cargo.toml` | `:51` | for `exec_streaming_cwd`'s `W: embedded_io_async::Write` bound |
| `crates/akuma-net/Cargo.toml` | `:61` | for the async `TcpStream` impls |
| `userspace/sshd/Cargo.toml` | `:51` | live — sshd socket I/O |
| `userspace/libakuma/Cargo.toml` | `:23` | optional, gated on `net-async` feature |
| `selfhost_vendor/embedded-tls/Cargo.toml` | `:82` | vendored TLS lib |

### Kernel consumers — all dead (in-kernel-SSH leftovers)

| Surface | Where | Callers |
|---|---|---|
| `tcp_connect` (async connect returning a `TcpStream`) | `crates/akuma-net/src/smoltcp_net.rs:813-851` | **0** (grep finds only the definition) |
| `TcpStream` + `embedded_io_async::{ErrorType, Read, Write}` impls | `crates/akuma-net/src/smoltcp_net.rs:999-1107` | **0** live; `TcpStream::new` is called only by `tcp_connect` above |
| `exec_streaming` / `exec_streaming_cwd<W: embedded_io_async::Write>` | `crates/akuma-exec/src/process/exec.rs:159-204` | **0** beyond their own definitions |
| `exec_async` / `exec_async_cwd` | `crates/akuma-exec/src/process/exec.rs:93-163` | **1**: `src/tests.rs:430` (a boot self-test) |

The docstring at `exec.rs:15` ("Use exec_async() for non-blocking execution")
and `tcp_connect`'s ("Suitable for use from async shell commands running in
`block_on` contexts") both date from the in-kernel shell/SSH era removed
2026-08-10. Deleting the above (~150 LOC, all `#[allow(dead_code)]` candidates)
lets the **kernel crates** — root `Cargo.toml`, `akuma-exec`, `akuma-net` —
drop `embedded-io-async`. It likely also lets smoltcp drop its `"async"`
feature (`Cargo.toml:423`), **but** that needs a separate check on whether the
blocking socket layer (`crates/akuma-net/src/socket.rs`'s `wait_until`) still
touches `register_recv_waker`; not asserted here.

This cleanup is **independent of** the executor removal in §6-§7 — it can be
done before, after, or without it. The two are related only in that both
remove in-kernel-SSH-era `async` surface.

### Userspace consumers — live, keep the crate in the workspace

| Surface | Where | What it does |
|---|---|---|
| sshd socket wrapper | `userspace/sshd/src/main.rs:22,31-39`; `userspace/sshd/src/protocol.rs:10` | Wraps `libakuma::net::TcpStream` in an `embedded_io_async::{Read, Write}` adapter; the SSH state machine reads/writes the socket through these traits |
| libakuma `net-async` feature | `userspace/libakuma/src/net.rs:82-103` | Impls `embedded_io_async::Error` for its net `Error` so sshd's adapter compiles |
| vendored `embedded-tls` | `selfhost_vendor/embedded-tls/src/{asperd,record_reader,connection}.rs` | TLS record I/O over the traits |

So as long as sshd's async state machine uses `embedded_io_async` as its socket
trait bound, the workspace as a whole keeps the dep. Dropping it from sshd
means rewriting sshd's read/write path off the trait abstraction — a bigger
change than the executor removal, and out of scope for this audit.

### Summary

- **Kernel crates can drop `embedded-io-async`** after a separate ~150 LOC
  dead-async-TCP + streaming-exec cleanup (§8 table above). Independent of the
  §6-§7 cooperative/executor removal.
- **Workspace keeps it** for userspace sshd + libakuma + embedded-tls.
- **smoltcp `"async"` feature** is a candidate to drop with the kernel cleanup,
  pending a check on the blocking-socket layer's waker use.

## Background

- [`docs/reference/subsystems/scheduler.md`](../reference/subsystems/scheduler.md)
  — current-state architecture: preemptive threading, the `POOL` gate, the
  proportional scheduler, `yield_now` vs `schedule_blocking`.
- [`docs/reference/subsystems/smp-shared.md`](../reference/subsystems/smp-shared.md)
  — M5c step-2 and the "don't hold the BKL across a cooperative wait" fix.
- [`docs/reference/subsystems/rump-stack.md`](../reference/subsystems/rump-stack.md)
  — the cooperative-fiber `rumpuser` backend (§5.3 above).
- `crates/akuma-exec/src/threading/types.rs:31-32, 194, 207` —
  `COOPERATIVE_TIMEOUT_US`, the `cooperative` field, its default.
- `crates/akuma-exec/src/threading/mod.rs:2238-2249` — slot 0 marked
  cooperative at init.
- `crates/akuma-exec/src/threading/mod.rs:2436-2526` — `schedule_indices`
  preemption rules and the cooperative-skip block.
- `crates/akuma-exec/src/threading/mod.rs:2864-2886, 3949-3954,
  4012-4089` — the spawn API (`spawn_fn`, `spawn_fn_cooperative`,
  `spawn_fn_with_options`, `spawn_user_thread_fn_with_options`).
- `userspace/rumpkernel/rumpuser/src/fiber.rs` — the userspace cooperative
  fiber scheduler (default rump backend).
