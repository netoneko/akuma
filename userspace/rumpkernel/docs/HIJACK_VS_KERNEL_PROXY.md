# Userspace hijack vs. kernel sysproxy — does bypassing the kernel save context switches?

**Question (2026-06-23):** Could we go back to an `LD_PRELOAD` "hijack" library that
talks to `rump_server` **directly**, bypassing the Akuma kernel's syscall
interception, to avoid context switches and the per-syscall latency we see in M2?

**Short answer:** Going back to a hijack lib that *speaks the sysproxy wire to a
shared `rump_server`* (model B below) would **not** meaningfully help latency,
because the bottleneck is **inside `rump_server`** (its ~19 pthread kthreads
spin-contending on one virtual CPU), not in the kernel interception layer or the
channel. The one model that genuinely eliminates the cross-process hop is the
**original M1 in-process hijack** (model C) — but that gives up stack *sharing*
and is blocked on musl-stdio binaries and the one-rump-per-tap limit. So this is a
real trade, not a free win. Details below.

---

## The three models (don't conflate them)

There are **three** distinct architectures in this tree, and "hijack lib" has meant
two different things historically.

### A. Current (M2) — kernel as sysproxy client  *[shipped, shared]*

```
box process          Akuma kernel                         rump_server (separate proc)
  connect()  --svc--> handle_syscall                       ~19 rump kthreads + 1 vCPU
                      rump_proxy::intercept_box_syscall
                      akuma_rump::sysproxy::Client
                      --- pipe pair (kernel-held) --------> rumpuser_sp server
                                                            rump_sys_connect()
                      <-------- pipe response -------------
  <--svc-ret--        copyout to user VA
```

- One `rump_server` per `stack=rump` box; **all** box processes share its NetBSD
  stack. This is the whole point of M2 (sshd + a separate sic on one stack).
- The sysproxy `Client` (`crates/akuma-rump/src/sysproxy.rs`) is generic over a
  `Transport`. Here the transport is `PipeTransport` over a **kernel-held pipe pair**.
- Marshaling (`syscall_translation` + `ProcMem`) runs kernel-side; copyin/copyout
  hit the calling process's VA directly (synchronous-on-calling-thread, "approach 1").

### B. Userspace hijack + `rumpclient` → shared `rump_server`  *[the proposed idea]*

```
box process                                         rump_server (separate proc)
  connect()  (interposed in libc, NO kernel trap for the op)
  rumpclient marshals
  write(channelfd) --svc--> kernel pipe/socket ----> rumpuser_sp server
  read(channelfd)  --svc--> kernel             <----  rump_sys_connect()
```

- The process links a hijack `.so` that embeds **`librumpclient`** (proven working
  in sysproxy Step 3, `sp_client_test.c`) and routes `socket/connect/...` over the
  **same sysproxy wire** — but the *client is the process*, not the kernel.
- Still a **shared** `rump_server`. Still cross-process. This is "move the sysproxy
  client from the kernel into userspace."

### C. Original M1 hijack — in-process rump  *[`rumpuser/hijack.c`, proven in container]*

```
box process (rump stack linked IN)
  connect()  (interposed) --> rump_sys_connect()   [plain function call, same address space]
                              ~19 rump kthreads live INSIDE this process
```

- `rumpuser/hijack.c` (read its header) is explicit: *"No rump server / no sysproxy:
  the rump kernel lives in the same address space, brought up by this library
  constructor."* The whole PIC rump stack is statically embedded in the `.so`.
- Socket ops are **direct function calls** — no IPC, no second process, no channel.
- This is the genuinely-fewer-context-switches option.

---

## Where the M2 latency actually comes from

From `RUMP_SYSPROXY.md` "B-OUTCOME" / `HANDOFF.md` "NEXT TASK", root-caused, not guessed:

- Each forwarded syscall costs ~0.8–4 s; a 74-byte `sendto` is ~0.8 s **with no
  network wait** — pure scheduling cost.
- Cause: `rump_server`'s ~19 rump **pthread** kthreads `STATE running, SYSCALL -` —
  they **busy-spin in userspace** contending for the rump kernel's single virtual
  CPU. A thread that needs the CPU (to read the channel / run the proxied syscall)
  waits behind ~19 spinners → ~270 ms per scheduling hop, ~4 hops/syscall.
- **Evidence it is thread-count-bound, not client-bound:** removing a duplicate
  second `rump_server` **halved** per-op latency (linear in thread count).
- **Two client-side fixes that did NOT help** (this is the key data point for the
  question): (1) making `sys_ppoll` event-driven like epoll, and (2) making the
  kernel proxy's channel read sleep instead of busy-yield. Both touch exactly the
  layer a userspace hijack would replace — and **neither moved the needle.**

So the expensive thing lives **inside `rump_server`** and is independent of *who*
the client is.

---

## Context-switch accounting

Per proxied socket op, "context switch to `rump_server`" is unavoidable in **both**
A and B — `rump_server` is a separate process and must be scheduled in to run
`rump_sys_*`. That scheduling-in is where the ~270 ms × N hops are spent.

| | Model A (kernel client) | Model B (userspace hijack client) | Model C (in-process) |
|---|---|---|---|
| Kernel trap for the logical op | 1 (the original `svc`) | the op is interposed in libc, **but** the channel I/O needs `write`/`read` syscalls → ≥2 traps | 0 extra (just the interposed call; rump runs in-proc) |
| Cross-process hop to `rump_server` | **yes** (must schedule it) | **yes** (must schedule it) | **none** |
| Marshaling location | kernel (`ProcMem`, no extra copy across address spaces) | userspace (rumpclient), then channel copy through kernel | none (same VA) |
| `rump_server` vCPU spin cost (the dominant term) | **paid** | **paid** | paid, but now *in your own process* (still ~19 threads) |

Takeaway:

- **B does not remove the cross-process hop** — it just relocates the sysproxy
  *client* from kernel to userspace. The expensive scheduling-in of `rump_server`
  is identical. And B *adds* client-side `write`/`read` traps for the channel that
  A folds into the kernel side of the original trap. So B is plausibly **slightly
  worse**, and at best a wash, on latency.
- **C removes the hop entirely** — socket ops become in-process function calls.
  That's the only structural win on context switches. But the ~19 rump kthreads
  now live in *your* process and still spin on one vCPU, so per-op rump scheduling
  cost persists; you save the inter-process scheduling, not the intra-rump spin.

---

## What model C costs you (why we left it for M2 in the first place)

The original M1 hijack (model C) was deliberately superseded by the shared-stack
model. Going back means re-accepting its limits:

1. **No sharing.** Each process embeds its own rump kernel → its own DHCP lease, its
   own ~19 kthreads, its own routing table. sshd and a separately-compiled sic
   **cannot share one stack** — that shared-stack story is the entire reason M2
   exists (`RUMP_SYSPROXY.md` opening).
2. **One rump per `/dev/net/tap0` per boot** (carried workaround #5): the NIC1 RX
   two-phase state machine isn't reset on close, so only one rump owner works per
   boot. N in-process-rump binaries would each want the tap. (M1 ran exactly one
   long-lived payload, which is why this was fine then.)
3. **musl-stdio binaries can't be `LD_PRELOAD`-hijacked** (HANDOFF gotcha, M1): musl
   stdio flushes via *inline* `writev`/`readv` that bypass the PLT, so `busybox
   wget` and anything `FILE*`-based slips past the interposers. curl/nc-class
   (direct `send`/`recv`) work; that's a narrow binary set. The kernel-proxy path
   (A) has no such limit — it intercepts at the syscall boundary, which is why sic
   (stdio!) worked under M2 once `readv`/`writev` were marshaled.
4. **Static-only / per-binary linking.** Every networked binary must be linked or
   `LD_PRELOAD`ed against the embedded stack; the kernel route works on unmodified
   static binaries with zero per-binary work.

---

## The actual levers for latency (none require abandoning the kernel proxy)

Per `HANDOFF.md` "NEXT TASK" — fix the *real* bottleneck instead:

1. **Make idle rump kthreads / the rump vCPU truly block (futex sleep) instead of
   spin** — in our Rust `rumpuser` (`rumpuser/src/lib.rs`: `cv_wait` / scheduler
   CPU-wait / spin-mutex paths). This is "THE fix (B, next)" in the doc and attacks
   the ~270 ms hops directly. **Helps A, B, and C equally** — because it fixes
   `rump_server`/the rump kernel, which all three share.
2. **Lower the rump kernel `hz`** before `rump_init` to cut 100 Hz heartbeat churn
   (untried lever; `thetick` computed once at thread start).
3. **Fiber rumpuser** (one OS thread per kernel, `rumpfiber.c`) collapses the ~19
   threads → ~1, killing the spin contention at the source. **Blocked:**
   `rumpfiber_sp.c` stubs the sysproxy server (`abort()`), so fiber needs a
   from-scratch sp-server port. Note this would *also* be the cleanest enabler for
   a fast model-C, since it removes the per-process thread explosion.

---

## Recommendation

- **Don't switch to model B for performance.** It moves the sysproxy client into
  userspace without removing the cross-process hop or touching the in-`rump_server`
  spin that dominates — so it cannot fix the latency, and it loses the kernel
  route's biggest asset (unmodified static binaries at the syscall boundary). The
  evidence is direct: the two client-side fixes already tried (epoll-style ppoll,
  sleeping channel read) didn't help.
- **Model C (in-process) is the only structural context-switch win**, and it's
  worth keeping in mind for a *single-payload, performance-sensitive* box (e.g. one
  dedicated proxy/relay process) where sharing isn't needed and the binary is
  hijackable (direct `send`/`recv`, not stdio). It is **not** a general replacement
  for M2's shared stack.
- **The high-value work is fixing the rump vCPU spin** (lever 1 above). That speeds
  up every model at once and doesn't require giving up the shared-stack architecture
  we just landed.

---

## Source pointers

- `rumpuser/hijack.c` — model C, in-process (header comment states it explicitly).
- `rumpuser/sp_client_test.c`, `rumpuser/rump_server.c` — model B building blocks
  (`librumpclient` over the sysproxy wire to a shared server), proven in container.
- `crates/akuma-rump/src/sysproxy.rs` — the `Transport`-generic sysproxy `Client`
  (model A uses it with `PipeTransport`; a model-B userspace client would implement
  `Transport` over its own channel fd).
- `src/rump_proxy.rs` — model A kernel-side interception + per-box proxy.
- `docs/RUMP_SYSPROXY.md` "B-OUTCOME" + `docs/HANDOFF.md` "NEXT TASK" — the
  latency root-cause analysis this assessment rests on.

---

# Scope: compiling the rump kernel in "fiber mode"

**Question (2026-06-23):** Could we build the NetBSD rump kernel in *fiber* mode
(`rumpfiber.c` — ucontext green threads, ~1 OS thread instead of ~19 pthreads) to
kill the single-vCPU thundering-herd that dominates M2 latency? "I know we'd have
to add some patches — what's the scope?"

## TL;DR

The latency win is real and structural — fiber collapses the ~19 contending rump
kthreads to one cooperative thread, removing the 100 Hz heartbeat herd at its
source (see `RUMP_LATENCY_SLEEP_FIX.md` Step 3). **But the framing "compile the
NetBSD kernel in fiber mode" doesn't match our tree, and that changes the whole
estimate.** Two facts decide it:

1. **There is no NetBSD kernel to recompile for fiber.** Pthread-vs-fiber is a
   property of **librumpuser**, *not* the rump kernel libraries. We build the
   kernel libs with `-k` (kernel only) and the default **hypercall** curlwp scheme
   — which is exactly what fiber's `Makefile` enforces (`RUMP_CURLWP=hypercall`,
   lines 33–36 of `src-netbsd/lib/librumpuser/Makefile`). So `librumpkern`,
   `librumpnet`, etc. **do not change**. The work is entirely in the hypercall
   layer.

2. **In our tree the hypercall layer is our Rust `rumpuser`, not NetBSD's C.**
   `RUMPUSER_THREADS=fiber` selects NetBSD's `rumpfiber.c` + `rumpfiber_sp.c` —
   which would *replace our entire Rust `rumpuser/src/lib.rs`* (all 59 hypercalls,
   plus the `clock_sleep` / scheduler-wrap fixes we landed 2026-06-22). So "turn on
   fiber" is not a build flag for us; it's an *implementation swap or rewrite* of
   the layer we own.

**And the dominant cost in either path is the same single item:** the **sysproxy
server must be re-architected to *yield* instead of *block*** under one OS thread.
NetBSD never finished this — `rumpfiber_sp.c` is **all stubs** (`rumpuser_sp_init`
returns 0, `rumpuser_sp_copyin/out/anonmmap/raise` all `abort()`). Our entire
shipped M2 (model A: kernel = sysproxy client, `rump_server` = sysproxy server,
shared NetBSD stack) **depends on that server**. Fiber as-shipped-by-NetBSD =
no sp server = no shared stack = M2 broken.

So: a genuine structural latency win, gated behind the one piece of work NetBSD
itself left undone, plus a decision about whether to keep our Rust investment.

## The two real options (not "flip a flag")

### Option 1 — adopt NetBSD's C `rumpfiber.c` wholesale (`RUMPUSER_THREADS=fiber`)

Drop our Rust `rumpuser`, build librumpuser from `rumpfiber.c` + `rumpfiber_bio.c`
+ `rumpfiber_sp.c`.

- **Pro:** `rumpfiber.c` is a complete, upstream-tested cooperative scheduler —
  thread_create/exit/join, mutex/rw/cv on wait-queues, curlwp, clock_sleep, all
  done (≈22 hypercalls, ucontext `swapcontext`, 64 KB mmap'd fiber stacks,
  round-robin with wakeup timers). We'd inherit it for free.
- **Con — we throw away the Rust layer** and every fix encoded in it (the
  `clock_sleep` CPU-release, the contended-mutex/cv unschedule discipline, the
  FUTEX/abs-timeout correctness work). Re-validation from scratch.
- **Con — ucontext dependency.** `rumpfiber.c` switches via
  `getcontext`/`makecontext`/`swapcontext` and allocates stacks with `mmap`. musl
  ships these (`ucontext.h` is in our sysroot) and our kernel has the signal
  syscalls musl's `swapcontext` leans on (`src/syscall/signal.rs`:
  sigaltstack/rt_sigprocmask), **but swapcontext has never been exercised on Akuma
  aarch64** — must be de-risked with a standalone test before trusting it.
- **Con — the sp blocker (below) still applies**, and now in C we don't control.

### Option 2 — make *our* Rust `rumpuser` cooperative (keep the Rust)

Keep `rumpuser/src/lib.rs`; reimplement only the threading/sync primitives from
pthread-backed to single-OS-thread cooperative green threads.

- **Pro:** keeps our investment and our fixes; the `component_*` bridge and
  `rumpkern_unsched`/`rumpkern_sched` upcalls are already backend-abstracted and
  would survive unchanged. Lets us keep linking NetBSD's *working* `rumpuser_sp.c`
  (which we already link against our hypercalls) rather than the stubbed
  `rumpfiber_sp.c`.
- **Con — real rewrite.** `thread_create` → green-thread spawn (ucontext or a Rust
  context-switch); `cv_wait`/`cv_signal`/`cv_broadcast`, `mutex_enter`,
  `rw_enter` → wait-queues + a cooperative scheduler instead of
  `pthread_cond_wait`/`pthread_mutex_*`. This is exactly the logic `rumpfiber.c`
  already contains — so Option 2 is largely "port `rumpfiber.c`'s scheduler to
  Rust," which raises the question of why not just take Option 1's C.

**The honest read:** Option 2's only durable advantage over Option 1 is keeping
NetBSD's *non-stubbed* `rumpuser_sp.c` server — but that server is **threaded and
blocking**, so it can't run as-is on one cooperative OS thread either. Which lands
both options on the same gate:

## The gate (THE scope): a cooperative sysproxy server

Under one OS thread, **any blocking read in the sp server blocks every fiber**,
including the fibers that would produce the reply. The sp server must be turned
inside-out into a yield-on-I/O event loop:

- channel reads/writes (`copyin`/`copyout`/the request dispatch) must **yield to
  the fiber scheduler** when they'd block, and resume when the fd is ready;
- the per-client request handling that NetBSD parallelizes with threads must become
  cooperative tasks driven by that loop.

This is the "from-scratch sp-server port" called out in `HANDOFF.md` NEXT TASK #1,
`RUMP_LATENCY_SLEEP_FIX.md` Step 3, and `RUMP_SYSPROXY.md` "Future optimizations"
— and it's precisely what the `abort()` stubs in `rumpfiber_sp.c` represent:
upstream rump shipped fiber *without* sysproxy on purpose, because the two models
fight. There is no existing code to copy for this; it's the genuine net-new work.

> Escape hatch worth noting: fiber + **model C (in-process rump)** sidesteps the sp
> server entirely — no cross-process hop, so no sp wire to make cooperative (this
> doc's earlier point that fiber "is also the cleanest enabler for a fast model-C").
> But model C gives up the shared stack (per-process DHCP lease, the one-rump-per-tap
> limit, the musl-stdio hijack gap) — i.e. it abandons M2's whole reason to exist.
> If a fiber sp-server is too costly, "fiber + single-payload model-C box" is the
> fallback that still banks the thread-collapse win for a *dedicated* relay/proxy
> process.

## Effort buckets (rough)

| Piece | Option 1 (C rumpfiber) | Option 2 (Rust cooperative) |
|---|---|---|
| Threading/sync primitives | free (upstream) | **rewrite** (≈ port `rumpfiber.c` to Rust) |
| ucontext on Akuma aarch64 | de-risk + likely small fixes | de-risk (or avoid via a Rust ctx-switch) |
| Keep our shipped fixes | **lost** — revalidate | kept |
| sp server | **must port** `rumpfiber_sp.c` (all stubs) | **must port** (`rumpuser_sp.c` blocks) |
| Build glue | `-V RUMPUSER_THREADS=fiber -V RUMP_CURLWP=hypercall`, drop Rust link | unchanged build, swap primitives |
| Rump kernel libs | **no change** | **no change** |

The **sp-server port is the long pole in both columns** and is the only piece with
no prior art. Everything else is either free (Option 1 threading) or a known
quantity (Option 2 threading = port of code we can read).

## Recommendation

1. **Do Steps 1 + 2 first — they target the same latency for a fraction of the
   cost and touch neither the threading model nor the sp server.** Lower the rump
   `hz` from 100 → 10–25 before `rump_init` (kills the heartbeat herd cadence
   directly; expected multiple-× cut) and change the scheduler's CPU-release
   `cv_broadcast` → `cv_signal` (`scheduler.c:492`, fewer wakers per hop). Both are
   small, both stay inside the architecture we shipped. `RUMP_LATENCY_SLEEP_FIX.md`
   explicitly says fiber (Step 3) should only be revisited if 1+2 don't get latency
   into a usable range.
2. **If we do pursue fiber, scope it as: (a) the context-switch primitive —
   ALREADY de-risked: a hand-rolled aarch64 cooperative switch, validated on Linux
   + Akuma EL0 (see "VALIDATED prototype" below; musl ships no ucontext so FFI
   `swapcontext` is out); (b) a cooperative rewrite of the sp server — the one true
   cost; (c) the threading layer, which is "adopt `rumpfiber.c`" (Option 1) or
   "port it to Rust to keep our fixes" (Option 2).** Decide 1-vs-2 on how
   much we value the Rust `rumpuser` and its landed fixes versus a clean upstream
   baseline. The kernel libs and the curlwp scheme do **not** move either way.
3. **Don't frame it as a compile flag.** `RUMPUSER_THREADS=fiber` is a one-line
   build change *only* if you also accept deleting our Rust layer **and** living
   with a dead (stubbed) sysproxy server. The "some patches" in the question are,
   concretely, the sp-server port — which is most of the work.

## Cost of porting `rumpfiber.c` to Rust (Option 2, threading layer only)

Read the whole of `rumpfiber.c` (1035 lines, but most is license header + blank
lines — the logic is compact). Porting the **threading layer** into our Rust
`rumpuser` is **~600 lines of straightforward Rust**, and most of it is *simpler*
than the pthread code we already shipped, because single-vCPU cooperative
scheduling deletes the concurrency machinery.

| Piece | LOC (C) | Porting difficulty |
|---|---|---|
| `struct thread` + 3 intrusive lists (run / exited / join) | ~40 | trivial — no locks needed |
| `schedule()` round-robin + wakeup-timer scan | ~40 | mechanical |
| `create_thread`/`create_ctx`, `exit_thread`, `join_thread` | ~90 | mechanical |
| `wait`/`wakeup_one`/`wakeup_all` + `waiter` queue | ~50 | mechanical |
| mutex (7) / rw (8) / cv (10) on wait-queues | ~290 | mechanical, **shorter than our pthread versions** |
| curlwp set/clear/get | ~30 | trivial — just `current_thread.lwp` |
| `switch_threads` → the context switch | ~10 + primitive | **the only hard part** |

**What deletes vs. our current code:** no `pthread_mutex`/`cond`/`rwlock` FFI, no
futexes, no atomics / memory-ordering reasoning, no pthread TLS key for curlwp, and
the contended/uncontended fast-path dance in `mutex_enter`/`cv_wait` collapses
(spin mutexes just `assert` success — there is no preemption). Cooperative
single-thread is a *strictly simpler* model than what we already debugged.

**What carries over unchanged:** `rumpuser_init`, `clock_gettime`, `clock_sleep`
(the body is nearly identical: `unsched → msleep/abssleep → sched`), `getparam`,
console/`putchar`, `getrandom`, `seterrno`, `kill`, `dprintf`, the `component_*`
backend bridge, and the `rumpuser__hyp` export. `rumpkern_unsched`/`rumpkern_sched`
are called identically. So this is **swapping the bodies of ~25 functions in
`lib.rs`, not rewriting the crate**.

**Estimate (threading layer to "`rump_init` boots + DHCPs under fiber"):**

- Mechanical port (scheduler + lifecycle + sync primitives + integration): ~3–5 days.
- Context-switch primitive (asm, or de-risk + wire ucontext): ~1–3 days.
- **Debugging the cooperative scheduler running a *real* kernel** — ~19 kthreads
  (heartbeat, softints, pagedaemon) yielding correctly, no missed-wakeup hangs,
  `cv_unsched`/`cv_resched` ordering exact: ~2–4 days. *This is where the time goes;
  the line count lies.*
- **Net: ~1.5–2.5 weeks** focused. Code is small; risk is concentrated in the
  context switch and first-real-boot debugging.

Coupling caveat (unchanged): the threading port alone does **not** keep M2 working
— once the kernel runs on one cooperative OS thread, the sysproxy server can no
longer do a blocking channel read on that thread (it would block every fiber). So
fiber forces the sp-server rework too: either make its channel I/O yield to the
fiber scheduler, or run it on a *separate* host pthread that hands work into the
fiber world (one synchronization boundary). That is the real long pole, not the
threading port.

## Why a context switch is needed at all — and why "just use Rust threads" doesn't help

**"Can't we just use our Rust/OS threads — we already have them running?"** The
threading *primitive* is not the problem; the thread *count* is. Our Rust
`rumpuser` already spawns the ~19 rump kthreads via `pthread_create`. Spawning them
via Rust `std::thread`, or any other OS-thread API, yields the **same ~19 real OS
threads, 1:1 with kthreads**, all contending on the rump kernel's single virtual
CPU, all re-woken 100×/sec by the heartbeat `cv_broadcast`. That herd *is* the
latency. Re-spelling the spawn is a rename, not a fix.

**Why a context switch is fundamentally required for the fix.** Each rump kthread
is straight-line **blocking** C: it runs on its own stack, calls `rumpuser_cv_wait`
from arbitrary call depth, blocks, and later resumes *in place*. The only way to
collapse N such independent blocking control-flows onto **one** OS thread is to
save the suspending thread's stack pointer + callee-saved registers and restore the
next one's — i.e. a context switch. There is no way around it *for code written as
blocking straight-line C*, which the entire NetBSD kernel is (you cannot rewrite
TCP/IP into callbacks).

So the real question is only **who** performs the switch:

- **OS threads (today):** the Akuma kernel scheduler does the switch — preemptively,
  kernel-mediated, once per thread swap, with all ~19 schedulable at once → the
  expensive herd. We are **not** avoiding context switches today; we have *more* of
  them, done by the kernel, and they are the slow ones.
- **Fibers:** *we* do the switch in userspace — cooperative, cheap, and only between
  the handful of fibers that are actually runnable. Fiber doesn't *add*
  context-switching to a switch-free system; it **replaces many expensive kernel
  switches with few cheap userspace ones**. The price of taking the OS scheduler out
  of the loop is that we must now write the save/restore ourselves.

**Why Rust can't make this disappear:** `std::thread` *is* OS threads (no win, and
we're `no_std` anyway). `async`/await can't help either — a future can only suspend
at an `.await` point that the *callee* cooperates with; it cannot suspend a plain
blocking C call mid-stack. To yield out of `rumpuser_cv_wait` you need a real stack
switch. So "in Rust" is fine, but it must be **stackful-coroutine Rust**: either
~40–80 lines of aarch64 inline asm (save x19–x30, sp, fp; build the initial frame),
or a `no_std` stackful-coroutine crate (e.g. `corosensei` / `generator` — verify
`no_std` + our custom target first). Both *are* "using Rust" — they're just not our
existing OS-thread machinery, which is precisely the thing that's slow.

## The context-switch primitive: option ladder, the musl dead-end, and a VALIDATED prototype (2026-06-23)

The threading port needs exactly one non-mechanical primitive: a stackful
context switch. We evaluated the ladder empirically rather than on memory.

### musl ships NO ucontext implementation — "FFI `swapcontext`" is dead

`<ucontext.h>` in musl **declares** `getcontext`/`makecontext`/`swapcontext`, but
`libc.a` **defines none of them**. Confirmed against *both* the Homebrew
`aarch64-linux-musl` toolchain and **Akuma's own musl** sysroot
(`userspace/target/.../musl-*/.../libc.a`): `nm` shows zero `*context` text
symbols (only unrelated `__malloc_context`). A `-static` link of a ucontext
ping-pong fails with `undefined reference to swapcontext`. This is a deliberate,
long-standing musl stance. Upstream `rumpfiber.c` only links because it's paired
with NetBSD libc / frankenlibc, which *do* implement ucontext.

→ The cheapest option (FFI libc `swapcontext`) is **off the table on musl**.
   No symbol to call; nothing to smoke-test on Akuma. Answered at link time.

### The implementable primitive: a hand-rolled aarch64 cooperative switch

Modeled directly on **Akuma's own** `switch_context`
(`crates/akuma-exec/src/threading/mod.rs:1102`, `global_asm!`) — the authoritative
reference for this target. That is an **EL1, preemptive** kernel-thread switch, so
it saves x19–x30, sp, **and** DAIF / ELR_EL1 / SPSR_EL1 / TTBR0_EL1 / `tpidr_el0`,
plus an `x30==0` guard and a `dsb ish`. A **cooperative EL0** fiber switch is a
strict subset — it runs as a plain function call inside one userspace process, so
it needs only the AAPCS callee-saved set:

- **x19–x28, x29(fp), x30(lr), sp** — same as `switch_context`.
- **`d8–d15`** — the callee-saved low halves of v8–v15. `switch_context` *omits*
  FP (kernel code is effectively FP-free); a fiber runs arbitrary C (printf, the
  rump kernel) so it **must** save these.
- **`tpidr_el0`** — so each fiber keeps its own TLS/`errno`. A cheap `mrs`/`msr`;
  `exceptions.rs` already uses `tpidr_el0` as the EL0 user-TLS base.
- **NOT needed at EL0:** DAIF, ELR/SPSR, TTBR0, the x30 guard — those exist only
  because `switch_context` can fire from IRQ context across address spaces.

Crucially this is a pure register/stack swap — **no syscall, no signal-mask
touch** — so unlike glibc/musl `swapcontext` it has zero dependency on Akuma's
`rt_sigprocmask` (the risk #3 we'd flagged simply doesn't exist for this path).

### Validated on BOTH targets

A ~110-line prototype (`akctx_switch` + a tiny entry trampoline + `akctx_make`,
mirroring `rumpfiber.c`'s usage: 64 KB mmap stack, `makecontext`-style pointer
arg) doing an A/B fiber ping-pong:

- **Linux baseline** (static `aarch64-linux-musl`, run in a native arm64 Alpine
  container): full 10-hop ping-pong, clean unwind, exit 0. ✅
- **Akuma EL0** (same static binary on `disk.img`, run over SSH as
  `/bin/asmctx_smoke`): identical 10-hop ping-pong, "main resumed … OK". ✅ —
  including the production-shape build that exercises `msr tpidr_el0` from EL0 and
  the `d8–d15` save/restore. So Akuma userspace honors the full switch.

Prototype source: `rumpuser/akctx_smoke.c` (landed next to the existing
`test_*.c`). It is the ready EL0-adapted template for `switch_threads` when the
Rust rumpfiber port lands. Build/run: `aarch64-linux-musl-gcc -static -O2 -o
akctx_smoke akctx_smoke.c`; run on disk via SSH as `/bin/akctx_smoke`.

→ **Updated recommendation for the primitive:** skip the FFI-`swapcontext` rung
   (musl can't provide it) and skip the crate hunt unless desired — the
   hand-rolled aarch64 switch is **already proven on Linux + Akuma**, is ~80 lines
   we fully control, derives directly from `switch_context`, and avoids the
   signal-mask syscall path entirely. A `no_std` coroutine crate (`corosensei` /
   `context`) remains a fine alternative for the primitive, but is no longer the
   *only* de-risked route — and would still have to clear `no_std` + our
   offline/no-deps build + symmetric-switch + C-stack-interop.

## Implementation status — threading layer ported + validated (2026-06-23)

Path chosen: **Option 2 (Rust port, feature-gated)** — keep our Rust `rumpuser`,
add the cooperative backend behind a cargo feature so the shipped pthread/M2 path
is untouched.

Done:

- **`rumpuser/Cargo.toml`**: new `threads_fiber` feature (off by default).
- **`rumpuser/src/lib.rs`**: the pthread threading/sync/curlwp block is wrapped in
  `#[cfg(not(feature="threads_fiber"))] mod pthread_backend` (default build is
  byte-identical — M2 intact); `clock_sleep`, `CURLWP_KEY`, and the `rumpuser_init`
  curlwp path are individually gated; `rumpkern_{un,}sched` are reused by the fiber
  module via descendant-module privacy.
- **`rumpuser/src/fiber.rs`** (~580 lines): a faithful Rust port of NetBSD's
  `rumpfiber.c` on top of the validated `akctx`-style switch — intrusive
  run/exited/join lists, round-robin `schedule()` with wakeup timers, thread
  lifecycle, wait-queues, and the full hypercall set (thread_create/exit/join,
  curlwp, clock_sleep, mutex×7, rw×8, cv×9). Single OS thread, cooperative.
- **Builds clean both ways**: `cargo build/clippy` for `aarch64-unknown-linux-musl`
  with and without `--features threads_fiber` — zero warnings, zero clippy findings.
- **Validated end-to-end** with `rumpuser/test_fiber.c` (stubs the two hyp
  scheduler upcalls, drives the hypercalls directly): Test A = 3 fibers
  cooperatively `clock_sleep` + exit + join; Test B = two fibers mutex+condvar
  ping-pong (perfect P/Q alternation, 10 rounds). **PASS on both** the Linux
  baseline (static aarch64-musl in arm64 Alpine) and **Akuma EL0** (`/bin/fiber_test`
  over SSH). So create/schedule/sleep/exit/join/mutex/cv all work on the
  hand-rolled switch, in Akuma.

**`rump_init` boots under fiber — VERIFIED in Akuma (2026-06-24).** A minimal
`test_init.c` (rump_init only) linked against the fiber `rumpuser` + `librump`
prints the NetBSD boot banner and `rump_init() returned 0 … PASS` over SSH in
Akuma EL0. The full `rump_server` also **rebuilds and links** with the fiber
backend (`docker-build-rump-server.sh`, 13.9 MB binary).

**Thread collapse — VERIFIED, apples-to-apples (`test_init_live.c`, same payload,
only the rumpuser backend differs), `ps` in a fresh VM:**

| binary | rump_init | OS threads (kthreads) |
|---|---|---|
| `rump_live_fiber` | ✅ booted | **0 child threads** (all kthreads are fibers on 1 OS thread) |
| `rump_live_pthread` | ✅ booted | **12 child threads** |
| `/bin/rump_server` (pthread, M2 autostart) | ✅ | **~19 child threads** (the herd) |

So the fiber backend collapses every rump kthread onto one OS thread in the real
NetBSD rump kernel. Core fiber work (tasks 1–5) is **done and verified**.

### Full fiber `rump_server` — built, run, and the coupling CONFIRMED empirically

The fiber `rump_server` was swapped in as `/bin/rump_server` and run two ways:

- **Without a tap (`RUMP_NIC=0`)**: `rump_init` runs, kthread herd collapses
  (`ps`: PID + **1** thread = the sp pthread, vs ~19 pthread), sysproxy reaches
  `[RUMP-SP] proxy ready`, PSTATS `clone=1`, **no `futex` line**,
  `in_kernel=131ms` (vs the pthread server's `283050ms`). The herd/futer storm is
  gone.
- **With the tap (`RUMP_NIC=1`, the real networked path)**: `rump_init` runs and
  the herd still collapses, but during the sysproxy handshake the boot log shows
  **`[signal] tkill(tid=86, sig=6)`** (SIGABRT on tid 86 — the sp-server's lone
  pthread) → `[RUMP-SP] handshake failed errno=5` → `rumpnet exited`. **No
  networking.** Root cause = the predicted coupling: the sp pthread is a *second*
  OS thread calling into the **lock-free** fiber rump kernel (wrong `curlwp` /
  raced lock state → a rump `KASSERT` → `abort()`).

So: the thread-collapse + futex-storm-elimination are real and verified, but a
*networked* fiber `rump_server` is blocked on the coupling below — confirmed, not
theoretical.

> **UPDATE (2026-06-24): RESOLVED — networked fiber `rump_server` works.** The
> coupling below was fixed *without* the blocking-I/O offload thread. Two bugs:
> (1) `rump_server.c`'s `for(;;) sleep(3600)` park blocked the one OS thread so the
> serve fiber never sent its banner (handshake `errno 5`); (2) the real deadlock —
> NetBSD's COPYIN `waitresp` does `pthread_cond_wait`, blocking the OS thread on a
> futex so the receiver fiber can't wake the parked worker. Fix: `sp_serve_fd.c`
> now redirects `pthread_mutex_*`/`pthread_cond_*` (not just `pthread_create`) to
> cooperative fiber primitives (`akfiber_sp_*` in `fiber.rs`), runtime-gated on
> `rumpuser_akuma_cooperative()`. **Result:** `curl http://example.com/` over the
> proxied rump stack = **16.3 s on fiber vs 62.8 s pthread (~3.85×)**, `rump_server`
> = **1 OS thread, PSTATS `clone=0 futex=0`**. The offload-thread design below is
> no longer needed for correctness (it remains a possible latency optimization).
> See `FIBER_HANDOFF.md` for the operational write-up + the Rust regression test.

### In-process (model C) networking under fiber — WORKING end-to-end (2026-06-24)

The model-C path (`rumphttp`: rump linked in-process, the box's own backend over
`/dev/net/tap0`, **no sysproxy**) now runs fully under the fiber backend. Two fixes
in `rumpcomp_tap.c` (our file, not NetBSD source):

1. **RX thread via `rumpuser_thread_create`, not `pthread_create`** — so it's a
   *fiber* under the fiber backend (a pthread under the default). Fixes the
   cross-thread race (a raw 2nd OS thread calling `VIF_DELIVERPKT` into the
   lock-free fiber kernel → KASSERT/abort). With just this, `rump_init` + `virt0`
   create cleanly, but the stack then *froze*: the RX fiber's **blocking**
   `read(tap0)` parked the one OS thread before the DHCP DISCOVER could go out.
2. **Cooperative non-blocking RX under fiber** — new backend hooks
   `rumpuser_akuma_cooperative()` / `rumpuser_akuma_yield()` (in `src/lib.rs` +
   `src/fiber.rs`): the fiber build opens the tap `O_NONBLOCK` and yields to the
   scheduler on `EAGAIN` (the pthread build keeps its blocking read — no M2
   regression). This lets the rest of the rump kernel run on the one OS thread.

Result, `RUMP_NIC=1`, `/bin/rumphttp 10.0.2.2 8000` over SSH:
```
dhcp: virt0: adding IP address 10.0.2.16/24
dhcp: virt0: adding default route via 10.0.2.2
RUMPHTTP: connect 10.0.2.2:8000 -> 0
RUMPHTTP: sent 56-byte GET -> 56
HTTP/1.0 200 OK ... (full response)
RUMPHTTP: PASS — fetched 767 bytes over the NetBSD rump stack (DHCP + TCP via /dev/net/tap0)
[VIRTIF STATS] tx=77 pkts rx=8 pkts
```
**DHCP + TCP connect + HTTP GET all work on the single-OS-thread cooperative fiber
stack.** So fiber networking is proven for the model-C / no-sysproxy path — the one
the "do we even need sysproxy" pondering favors.

Still open: the **sysproxy** `rump_server` path (the SIGABRT above) — its sp-server
uses `pthread_create` in NetBSD source we don't own, so it needs the offload-thread
treatment (below) rather than the in-file fix that worked for the tap backend.

### Next (the real long pole, for the sysproxy path): blocking-I/O offload thread

The blocker for a *working networked* fiber `rump_server`: the sysproxy
server calls **`pthread_create` directly in 3 sites** (`sp_serve_fd.c:131`,
`rumpuser_sp.c:943,1374`) and does **blocking channel reads**; the tap RX
(`rumpcomp_tap.c`) does a **blocking `read` on `/dev/net/tap0`**. On the single
cooperative OS thread, (a) a blocking read freezes *all* fibers, and (b) an
sp-spawned pthread is a *second* OS thread that calls into the rump kernel and
races the **lock-free** fiber scheduler globals → the SIGABRT above.

**Design (decided 2026-06-24): a dedicated blocking-I/O OS thread.** Quarantine
all blocking host I/O (tap0 read/write, the sp channel) onto one extra OS thread.
The hard rule that preserves the win: **that thread never touches fiber internals**
(run/wait queues, cv/mutex state stay single-thread-owned and lock-free). It only
- runs the blocking syscalls,
- exchanges raw buffers with the fiber world over a thread-safe SPSC queue, and
- wakes the fiber scheduler via a dedicated primitive (self-pipe / eventfd / futex);
  the scheduler's idle `nanosleep` becomes a wait on that primitive (+ timeout).
A fiber on the core thread then drains the queue and does the rump packet-input /
proxied-syscall execution cooperatively (holding the one rump CPU as usual).

Net effect: removes BOTH the blocking-freeze and the ~19-thread herd; adds exactly
ONE I/O thread whose only shared state is a queue + a wakeup fd. A separate
*process* is the wrong unit (forces IPC copies + re-raises tap-fd ownership); a
*thread* shares the address space for zero-copy buffer handoff. This is the
standard "cooperative core + blocking-I/O offload" pattern and is what unblocks
M2's shared stack (and the tap RX) under fiber.

### Open ponderings (deferred — decide after model-C networking works)

- **Do we still need sysproxy at all?** Sysproxy's job is *sharing* (many box
  processes, one stack + one identity) and unmodified static binaries serviced at
  the syscall boundary — NOT performance. Fiber doesn't replace that need; it
  removes what made sysproxy *slow* (the herd), so it *rescues* sysproxy. What
  fiber newly makes cheap is **in-process model C** (rump linked into the box,
  ~1 thread instead of ~19) — which lets a *dedicated single-payload box* skip
  sysproxy entirely. So: keep sysproxy where you need >1 process on one stack;
  use model C where a box is one payload. Fiber widens the menu, doesn't collapse
  it. (Model C's other limits — one-rump-per-tap, LD_PRELOAD on musl-stdio — are
  unchanged by fiber; see next.)
- **`rump_server` as the box's PID 1 + dynamic loading (idea, 2026-06-24) — could
  remove the need for sysproxy entirely (for dynamic payloads).** Make the (fiber,
  cheap) `rump_server` the box's PID 1 / rump host, and have it **dynamically load
  the box payload into its own address space** (it acts as the loader; the dynamic
  linker / an `LD_PRELOAD` hijack wires the payload's `socket`/`connect`/… straight
  to the *in-process* rump stack). Because payload and rump now share one address
  space, socket ops are **direct function calls** — this is model C, so there is
  **no sysproxy channel at all** (no separate process, no remote-syscall proxy).
  Fiber is what makes the pid-1 rump host cheap (~1 thread). Clean lifecycle too:
  pid 1 dies → the box's networking dies.
  - **Caveat (the catch):** this needs **dynamic loading**. It does **not** work
    for unmodified **static** musl binaries — there's no loader to inject into, and
    musl-static stdio bypasses the PLT (the original M1 hijack gotcha). So the split
    is: *dynamic* payloads → pid-1 host + in-process rump, **no sysproxy**;
    *unmodified static* binaries → still need sysproxy (kernel syscall
    interception at the trap boundary, the one path that needs no preload).
  - Worth exploring: a box that standardizes on dynamic payloads could drop
    sysproxy completely and run everything in-process against a pid-1 fiber rump.

## Source pointers (fiber)

- `rumpuser/src/fiber.rs` — the cooperative backend (Rust port of `rumpfiber.c`).
- `rumpuser/test_fiber.c` — standalone validation harness (Linux + Akuma).
- `crates/akuma-exec/src/threading/mod.rs:1102` — **Akuma's own `switch_context`
  `global_asm!`**; the reference our EL0 cooperative switch is adapted from.
- `src/exceptions.rs:108–237` — the EL1 trap-frame save/restore (full q0–q31 +
  `sp_el0` + `tpidr_el0`); shows the target's FP/TLS handling.
- `src-netbsd/lib/librumpuser/rumpfiber.c` / `.h` — the cooperative scheduler
  (ucontext `swapcontext`, 64 KB mmap stacks, round-robin + wakeup timers);
  ~22 hypercalls fully implemented.
- `src-netbsd/lib/librumpuser/rumpfiber_sp.c` — **the blocker**: all sp hypercalls
  stubbed (`abort()` / no-op). This is the net-new work.
- `src-netbsd/lib/librumpuser/Makefile` lines 11–42 — `RUMPUSER_THREADS` select;
  fiber requires `RUMP_CURLWP=hypercall` (which our `-k` kernel build already uses).
- `rumpuser/src/lib.rs` — our 59-hypercall Rust layer that Option 1 would replace /
  Option 2 would partially rewrite (pthread threading, `clock_sleep` CPU-release,
  contended-mutex/cv unschedule discipline).
- `docs/RUMP_LATENCY_SLEEP_FIX.md` Steps 1–3 — the cheaper levers to try before
  fiber, and the fiber blocker statement.
