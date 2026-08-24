# Debug networking (native smoltcp stack)

Symptom-driven debugging for the **native smoltcp** stack (NIC0). For the rump
stack (devbox), see [`debug-devbox.md`](debug-devbox.md). For architecture, see
[`../reference/subsystems/networking.md`](../reference/subsystems/networking.md).

> **Stability: C (active risk).** Lock-ordering invariants here are subtle
> (`NETWORK` ↔ `SOCKET_TABLE` ↔ `EPOLL_TABLE`). Recent churn (Jul). The
> recurring lesson: **never `yield_now()` or block inside a preemption-disabled
> closure** (`with_socket_handle`, `with_network`, `with_fs`).

The native stack is raw smoltcp 0.12 behind a global
`NETWORK: Spinlock<Option<NetworkState>>` (`crates/akuma-net/src/smoltcp_net.rs`).
The old embassy-net docs are superseded — cite the lesson, not the embassy
mechanism.

## Symptom → cause → fix

| Symptom / signature | Cause | Status | Fix |
|---|---|---|---|
| Whole-system hang 5+ s; watchdog `Preemption disabled ... (critical)` | Priority-inversion: preemptible userspace thread held a VFS/Block spinlock, preempted; preemption-disabled network thread spun on it | FIXED | `with_fs`/`with_device` disable preemption **before** acquiring the spinlock |
| Network starves during SSH/auth disk I/O (e.g. `read_file("authorized_keys")`) | Async tasks held "preemption disabled" across slow sync fs I/O | FIXED, then N/A | `src/async_fs.rs` temporarily **enabled** preemption during sync I/O; the file (and its only callers, the built-in shell/SSH) was deleted 2026-08-10 — see `docs/archive/ASYNC_FS_WRAPPERS.md` |
| `sys_sendto` watchdog trips (5+ s preemption-disabled) | Flush loop with `yield_now()` ran **inside** `with_socket_handle` | FIXED | Poll briefly inside the closure; `yield_now()` **outside**. Rule: never yield/block inside `with_socket_handle`. |
| Panic: `subtract sequence numbers with underflow` (`smoltcp/tcp.rs:81`) during sideband buffers | Out-of-order packet handling / concurrent access state corruption | OPEN (latent) | Mitigated by single `NETWORK` lock + smoltcp migration. Believed largely gone. |
| EL1 `EC=0x25 FAR=0xffffffff00000000` in `TcpStream::read` → `SocketSet::get_mut` | VirtIO RX buffer overflow: `receive()` built slices from `hdr_len+pkt_len` with no bounds check; OOB write corrupted the SocketSet | FIXED | Bounds-check `hdr_len.saturating_add(pkt_len) > rx_buffer.len()`; `TcpStream` caches + validates `handle_index` on every op |
| Panic: `adding a socket to a full SocketSet` after long uptime | No capacity guard; sockets stuck in `pending_removal` (FIN/TIME/LAST-ACK) leaking slots | FIXED | Capacity guard returning `None`; `pending_removal` Vec with 30 s forced GC (`SOCKET_GC_TIMEOUT_US`); `MAX_SOCKETS` 128→256 |
| After smoltcp migration: NO external traffic (loopback OK, SSH/HTTP dead) | Three VirtIO receive bugs: RX buffer never posted; 10 B VirtIO net header not stripped; MTU 1500 vs 1514 | FIXED | Two-phase `receive()`; offset by `hdr_len`; MTU→1514 |
| Download starts fast then collapses to ~2 kbps; window shrinkage | smoltcp 0.12 default **10 ms delayed-ACK** | FIXED | `socket.set_ack_delay(None)` on every new socket in `socket_create()` |
| SSH listener wedges; no new sessions (old code did `None => break`) | `socket_create()` returned `None` under connect-storm → accept loop terminated permanently | FIXED | `recreate_listener_with_retry()` polls+yields until a slot frees |
| EL1 fault loop / SSH drops after EL1 recovery; cascading `EC=0x25 FAR=0x1` | Old handler did `ELR+4` → landed on next instruction using poisoned register → re-faulted → stack overflow | FIXED | Redirect `ELR_EL1` to `el1_fault_recovery_pad` (→ `return_to_kernel(-14)`) |
| Second `bun install` hangs on DNS after a prior crash | `el1_fault_recovery_pad` yielded forever; zombie's socket fds never reaped → `MAX_SOCKETS` exhausted | FIXED | Pad now `return_to_kernel(-14)` (runs full fd cleanup) |
| SMP=1: whole kernel freezes (ticks stop, no panic, QEMU ~3 % CPU) after starting a second server from SSH while a listener is bound; SMP=4 unaffected (same RAM) | **Diagnosed**: waiter parked in `idle_halt` (yield-less `blocking_relax_net`) monopolizes the only core preempt-disabled; ticks' wake-pass runs but `schedule_indices` never switches, no voluntary-SGI producer left — see `docs/archive/SECOND_LISTENER_SMP1_FREEZE.md` §2a | OPEN (mechanism) | Fix directions in §5 of that doc |

Connection-refused is almost always socket-pool/accept-loop (above) or the
network thread not yet ready.

## Loopback (127.0.0.1) issues

`LoopbackAwareDevice` wraps VirtIO + an internal `loopback_queue`. TX frames to
127.x short-circuit into the queue; `receive()` drains it first, then VirtIO.

| Symptom | Cause | Status | Fix |
|---|---|---|---|
| Loopback connection timeout: `Client: SynSent, Server: Listen` (SYN never transmitted) | Global ARP rate limiter (1 s `silent_until`) set by an external SSH SYN; or DHCP `update_ip_addrs()` flushing neighbor cache | OPEN (planned) | Pre-seed neighbor cache for local IPs after every `update_ip_addrs()`; wall-clock test timeout ≥2 s |
| Loopback test crashes when external SSH SYN arrives mid-boot | 3-way interaction: DHCP `flush_neighbor_cache` + gateway ARP `limit_rate` (1 s global) + loopback SYN hits `RateLimited` → no ARP for 127.0.0.1 | OPEN | Pre-seed neighbor cache for each local IP with interface MAC after `update_ip_addrs()` |

| `httpd` stops answering after a load run — **process still alive** and its log still reads `Listening for connections...`, but every request returns `000` from **both** the host and in-guest | Blocking `accept` loop wedged. `httpd`/`libakuma` use **no epoll** — `TcpListener::accept` calls the raw `accept` syscall and parks in `akuma_net::socket::wait_until`. Reproduced twice: once after serving 4 requests, once immediately after a 200-request `ab` run | **OPEN** | Not diagnosed. Note the pairing: nginx loses wakes on the **epoll** family, `httpd` wedges on the **`wait_until`** family — the two wait families `akuma-net-yarn` documents as differing in six policy fields. See [`../archive/NGINX_LOST_WAKEUP.md`](../archive/NGINX_LOST_WAKEUP.md) |
| Benchmark client (`ab`, curl) hangs forever in the guest and never exits | Its target server was killed mid-run (e.g. by a `pkill` between benchmark arms). The client blocks on a socket that will never complete and is never reaped | OPEN (harness) | `pkill -f /usr/bin/ab` between arms. Hung clients accumulate and silently contaminate later measurements — check `ps` before trusting a number |

## epoll issues

| Symptom | Cause | Status | Fix |
|---|---|---|---|
| Hang during `spawn_process_with_channel` | Lock inversion #1: `EPOLL_TABLE` ↔ `PROCESS_TABLE` | FIXED | Snapshot interest list into 128-FD stack array, release `EPOLL_TABLE` before readiness checks |
| Intermittent whole-kernel hang under bun event loop + SSH | Lock inversion #2: `NETWORK` ↔ `SOCKET_TABLE` | FIXED | `poll()` drops `NETWORK` **before** `with_table(wake_all)`; returns a `socket_state_changed` flag |
| `test_epoll_multi_poller_pipe` reports `woken=0` | Poller threads not registered via `register_thread_pid` → `current_process()` None → EBADF | FIXED | Each thread calls `register_thread_pid` on entry, `unregister` before parking |
| EL1 crash / SIGSEGV / DNS hang under bun (`EC=0x25 FAR=0x50004000`) | 6 causes: no EL1 recovery→stack overflow; lazy stack missing; bad kernel-VA exclusion; ELR+4; `epoll_destroy` on child-shared fd; `EPOLL_CLOEXEC` ignored | FIXED | `el1_fault_recovery_pad`; `copy_to_user_safe`; 32 MB lazy stack; strip EpollFd on fork; honor EPOLL_CLOEXEC |
| Sharing an epoll fd via `dup` across fork | `epoll_destroy` is **not refcounted** | OPEN | Don't share epoll fds across fork via dup |
| nginx (or any epoll server) serves at **~17 ms/request** on some boots and ~0.1 ms on others; `ab -k` halves it; **0 failed requests**; tight within a boot | **Lost epoll wakeup** — the wait ends on the 10 ms `backstop_us` timer instead of the waker. Akuma's blocking `httpd` (no epoll at all) is 16x faster on the same boot; redis never shows it | **OPEN** | Not fixed. **Do not tune `backstop_us`** — that shortens the rescue, not the miss. [`../archive/NGINX_LOST_WAKEUP.md`](../archive/NGINX_LOST_WAKEUP.md) |
| Every benchmark through an epoll server reads 3-7x slow; guest console emits ~250k lines per run | The `[epoll] ctl` trace (`poll.rs:462`) was **ungated** — it fired on every `epoll_ctl` ADD/MOD, and an event loop re-arms per request. ~40 B out the emulated 16550, **one MMIO trap per byte, on the request path**. It was 99.3 % of all console output | FIXED 2026-08-24 | Gated behind `SYSCALL_DEBUG_NET_ENABLED`; 14 further per-operation traces gated with it. [`../archive/LONG_ROAD_TO_REDIS_PART_2.md`](../archive/LONG_ROAD_TO_REDIS_PART_2.md) §9 |
| `select()` is never woken by a fast peer — it only ever returns on the 10 ms tick (every cargo/libcurl network wait) | `sys_pselect6` passed `None` for its waker, alone among the three poll syscalls | FIXED 2026-08-24 | Registers the waker; boot test `run_pselect6_registers_waker_test`, verified to fail on the unfixed kernel |
| A process blocked in `select()` cannot be interrupted by Ctrl-C or `kill`; `alarm()` + `select()` sleeps through its own signal | `sys_pselect6` had no `should_interrupt_blocking_syscall()` check; `epoll_pwait` and `ppoll` both did | FIXED 2026-08-24 | Interrupt check added; boot test `run_pselect6_eintr_test` (unfixed kernel slept the full 300 ms and returned 0 instead of `EINTR`) |
| `dup(eventfd)` or `dup(rump socket)`: the first `close()` destroys the object under the surviving fd | `dup`/`dup3`/`fcntl(F_DUPFD)` matched only 4 of the 6 `FileDescriptor` variants, then `_ => {}` — so two families were aliased without a refcount | FIXED 2026-08-24 | Three drifted copies deleted for one `akuma_exec::process::clone_fd_refs`, shared with `clone_deep_for_fork`. **No runtime test yet** |

## TLS / HTTPS issues

One impl now: userspace blocking (`userspace/libakuma-tls/`), `embedded-tls
0.17`, `Aes128GcmSha256` (ECDHE P-256), TLS 1.3 only. The in-kernel async impl
(`src/tls.rs`, `src/tls_verifier.rs`, the `kernel-tls`/`tls-rsa` features) was
deleted entirely (commit `bade6ab`, "remove unnecessary profiles and all
crypto") — there is no profile that still has it, so there is no full X.509
verification anywhere in this tree anymore.

| Symptom | Cause | Status | Fix |
|---|---|---|---|
| HTTPS downloads measured in minutes | `libakuma-tls/transport.rs` slept **10 ms** on every `WouldBlock` | FIXED | 10 ms → 1 ms; timeout iterations 500→5000 (~5 s idle preserved) |
| Residual 10 ms sleep | Some flows may still have `sleep_ms(10)` | VERIFY | Check current `transport.rs` before assuming deployed |
| `curl https://...` fails in-kernel | There is no in-kernel HTTPS client (the kernel has no shell at all now — SSH is the userspace `/bin/sshd`) | BY DESIGN | Use a userspace HTTPS tool over the userspace shell |
| Userspace TLS skips cert verification | Phase-1 libakuma-tls does NoVerify (MITM-vulnerable) | OPEN | No kernel fallback exists anymore to do full X.509; would need fixing in `libakuma-tls` itself if this matters |

## Latency: is the NIC interrupt alive?

Until 2026-08-19 the timer (PPI 27) was the only device IRQ this kernel
registered, so the whole stack was tick-driven and a round trip cost
milliseconds. That is fixed — but the fix is one MMIO configuration away from
silently reverting, so check it first for any latency complaint:

```bash
grep -a "virtio-net IRQ" logs/your.log     # expect: slot 0 -> INTID 48
```

With `--features net-profile` the `[NICSTAT]` dump carries the live count:

```
[NICSTAT] w=3 nic_irq=179
```

`nic_irq=0` under real traffic means the SPI never reached the CPU (group,
priority or route not programmed — see `gic_v3::enable_irq`), and every wait is
back on the 3 ms scheduler tick. The other tell is `relax=N/Mms` in the same
dump: a per-park average near the tick interval is the same symptom.

Full investigation and the measured before/after:
[`../archive/AKUMA_NET_ISSUES.md`](../archive/AKUMA_NET_ISSUES.md).

## Latency: do NOT reach for `net-noalloc` to fix it

`net-noalloc` (static RX/TX rings + async transmit,
`crates/akuma-net/src/virtio_rings.rs`) looks like the obvious next latency
lever and **is off for a reason**. Measured 2026-08-19 on `devbox-smoltcp`, same
`httpd` binary both arms:

| | single buffer | `net-noalloc` |
|---|---:|---:|
| poll time held per 5 s window | 472 ms | **211 ms** |
| tx blocking wait | 27.8 us/pkt | **9.2 us/pkt** |
| httpd `read` syscall | 44-78 us | **7-9 us** |
| HTTP p90 | **1,172 us** | 3,433 us |
| HTTP req/s | **1,071** | 855 |

It halves the time the stack holds `NETWORK` and still loses, because the cost
moved into the wake: httpd's `accept` phase went from 66-77 % of the request to
81-88 %. Turn it on to work *that* problem, or to measure a pipelined /
long-lived-connection workload where the lock-hold win should dominate and the
per-connection wake should not — it has not been measured against redis yet.
Reasoning and the untested hypothesis:
[`../archive/AKUMA_NET_ISSUES.md`](../archive/AKUMA_NET_ISSUES.md) §7.

When it is on, `[NICSTAT]` gains two cumulative counters, both of which must
stay at zero:

```
[NICSTAT] w=7 nic_irq=169 orphan=0 tx_stall=0
```

`tx_stall` counts frames that found every TX slot in flight — the ring is too
shallow and those frames took a spin or were dropped. `orphan` counts device
completions whose token no slot claims, which should be impossible and means
the token map has desynchronised from the used ring.

## Latency: `net-waker-park` is off for the same kind of reason

The other tempting lever. Blocking socket ops (`accept`/`recv`/`send`/`connect`)
park in `blocking_relax` — yield + WFI — which leaves the thread READY, so
nothing can target it and the socket's `wake_all()` walks an empty waker list.
Every other blocking path in the kernel (pipes, fs, msgqueue, epoll) registers
and parks properly. Fixing that outlier **measured worse**:

| | default | `net-waker-park` |
|---|---:|---:|
| req/s | **1,071** | 944 |
| p90 | **1,172 us** | 2,169 us |

`blocking_relax` wakes on *any* interrupt, and under load the NIC raises ~6,300
per 5 s window — so the imprecise wake is plentiful, while the directed one is
lossy (`wake_all` drains the list, so a waiter must re-register every lap).
Full reasoning: [`../archive/AKUMA_NET_ISSUES.md`](../archive/AKUMA_NET_ISSUES.md) §8.

**Diagnostic that stays useful either way:** `KernelSocket::waker_count()` reads
zero for any blocking socket waiter on the default build. If you are debugging a
socket that never wakes, that zero is expected, not the bug.

## The doorbell must be re-armed BEFORE the drain

`src/main.rs`'s netpoll loop clears `NIC_WAKE_PENDING` *before* `while poll()`,
not after. That ordering is load-bearing, not cosmetic: re-arming afterwards
leaves a window where a packet arriving after the last `poll()` is missed by the
drain, raises no broadcast (the doorbell is still set), and is then erased by the
re-arm — so every core sleeps to the 3 ms tick. Fixed 2026-08-20, worth:

| | re-arm after (old) | re-arm before |
|---|---:|---:|
| req/s | 673 | **1,108** |
| p50 | 630 us | **583 us** |
| p99 | 10,913 us | **4,085 us** |
| p90 spread across runs | 1,143-5,048 us | **1,977-2,233 us** |

If a latency regression appears here, check that ordering first. The tell in
`[NICSTAT]` is `relax`: the fixed build has MORE parks that are each SHORTER
(5,328-6,151 @ ~800 us vs 3,918 @ 1,172 us). Fewer, longer parks means wakes are
being swallowed again.

**Measure the tail with `-n 2000`, not the default.** At n=400 the baseline's p90
ranged 1,143-5,048 us across runs — enough noise to hide a 2.3x change or invent
one. Full reasoning:
[`../archive/AKUMA_NET_ISSUES.md`](../archive/AKUMA_NET_ISSUES.md) §9.

## Before blaming the NIC path: check whether the tail is bimodal

Akuma's *minimum* HTTP round trip (378 us) is **faster than the Linux control**
(519 us); the whole remaining gap is tail (p99 4,091 vs 882 us). A uniform
slowdown and a wake cliff need opposite fixes, so measure the shape first:

```bash
scripts/benchmarks/bench_nic_rtt.py --mode http --target localhost:8080 -n 400
```

p99/p50 near 1.5 means a genuinely slow path. Near 6 — what Akuma still shows —
means most requests are fine and a minority are landing on the 3 ms scheduler
tick. Chase the missed wake, not the per-packet cost. That reading is what found
the doorbell race above.

Arithmetic for the residue, so the next person does not re-derive it: the tick is
3,000 us exactly (`[Timer] host WFI probe`), p50 is 583 us, so one swallowed wake
costs `3,000 + 583 = 3,583 us` and the measured p99 of 4,085 us leaves ~500 us
unaccounted. p90 (2,107 us) is only 1,524 us above p50 — LESS than a tick — so
the remaining tail is a continuum, not another clean bimodal step. The lead for
it is `poll max = 3.6-3.7 ms` in the same dump: a single `poll()` blocking a full
tick means a `NETWORK` holder is sleeping while holding it.

## Network debug knobs

| Knob | Default | Effect |
|---|---|---|
| `SYSCALL_DEBUG_NET_ENABLED` (`src/config.rs:526`) | false | Per-call tracing in recv/sendto/epoll/socket. **Best first toggle** for "is the syscall reaching the kernel?" |
| `NETWORK_THREAD_RATIO` (`src/config.rs:222`) | 4 | Network thread boosted every N ticks. 2=50% (aggressive), 8=12.5%. Boost targets the **registered** network thread (`set_network_thread_id`), not slot 0. |
| `MAX_SOCKETS` (`smoltcp_net.rs:30`) | 256 (32 on `small-sockets`/`size`) | TCP buffers 16 KB RX + 16 KB TX each |
| `ENABLE_PREEMPTION_WATCHDOG` | true | Catches preemption-stall hangs |
| Live counters | — | `is_ready()`, `is_dhcp_configured()`, `poll_count()`, `tx_drop_count()` — check first for "is the stack alive?" |

## Background

- `archive/NETWORKING_DEADLOCK_INVESTIGATION.md`, `archive/SENDTO_PREEMPTION_FIX.md`.
- `archive/TCPSTREAM_CORRUPTION_FIX.md`, `archive/SOCKETSET_EXHAUSTION_FIX.md`.
- `archive/VIRTIO_RECEIVE_FIX.md`, `archive/NETWORKING_POLLING_AND_ACK_FIXES.md`.
- `archive/LOOPBACK_ARP_RATE_LIMIT_BUG.md`, `archive/LOOPBACK_TIMEOUT_FIX_PLAN.md`.
- `archive/EPOLL_PERFORMANCE.md`, `archive/EPOLL_EL1_CRASH_FIX.md`.
- `archive/TLS_DOWNLOAD_PERFORMANCE.md`, `archive/TLS_INFRASTRUCTURE.md`.
