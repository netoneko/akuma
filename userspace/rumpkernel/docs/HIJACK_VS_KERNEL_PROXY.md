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
