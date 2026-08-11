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

Connection-refused is almost always socket-pool/accept-loop (above) or the
network thread not yet ready.

## Loopback (127.0.0.1) issues

`LoopbackAwareDevice` wraps VirtIO + an internal `loopback_queue`. TX frames to
127.x short-circuit into the queue; `receive()` drains it first, then VirtIO.

| Symptom | Cause | Status | Fix |
|---|---|---|---|
| Loopback connection timeout: `Client: SynSent, Server: Listen` (SYN never transmitted) | Global ARP rate limiter (1 s `silent_until`) set by an external SSH SYN; or DHCP `update_ip_addrs()` flushing neighbor cache | OPEN (planned) | Pre-seed neighbor cache for local IPs after every `update_ip_addrs()`; wall-clock test timeout ≥2 s |
| Loopback test crashes when external SSH SYN arrives mid-boot | 3-way interaction: DHCP `flush_neighbor_cache` + gateway ARP `limit_rate` (1 s global) + loopback SYN hits `RateLimited` → no ARP for 127.0.0.1 | OPEN | Pre-seed neighbor cache for each local IP with interface MAC after `update_ip_addrs()` |

## epoll issues

| Symptom | Cause | Status | Fix |
|---|---|---|---|
| Hang during `spawn_process_with_channel` | Lock inversion #1: `EPOLL_TABLE` ↔ `PROCESS_TABLE` | FIXED | Snapshot interest list into 128-FD stack array, release `EPOLL_TABLE` before readiness checks |
| Intermittent whole-kernel hang under bun event loop + SSH | Lock inversion #2: `NETWORK` ↔ `SOCKET_TABLE` | FIXED | `poll()` drops `NETWORK` **before** `with_table(wake_all)`; returns a `socket_state_changed` flag |
| `test_epoll_multi_poller_pipe` reports `woken=0` | Poller threads not registered via `register_thread_pid` → `current_process()` None → EBADF | FIXED | Each thread calls `register_thread_pid` on entry, `unregister` before parking |
| EL1 crash / SIGSEGV / DNS hang under bun (`EC=0x25 FAR=0x50004000`) | 6 causes: no EL1 recovery→stack overflow; lazy stack missing; bad kernel-VA exclusion; ELR+4; `epoll_destroy` on child-shared fd; `EPOLL_CLOEXEC` ignored | FIXED | `el1_fault_recovery_pad`; `copy_to_user_safe`; 32 MB lazy stack; strip EpollFd on fork; honor EPOLL_CLOEXEC |
| Sharing an epoll fd via `dup` across fork | `epoll_destroy` is **not refcounted** | OPEN | Don't share epoll fds across fork via dup |

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
