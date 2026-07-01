# Multikernel Networking Experiment

Cross-core syscall forwarding under the one-kernel-per-core multikernel (see `MULTIKERNEL.md`,
branch `smp-attempt-0`). A process pinned to a secondary core has no local VFS or network stack,
so every VFS/socket syscall is *forwarded* to the BSP (core 0), which owns those capabilities.
This doc records why forwarded networking was "extremely slowly," the fixes, the measurements,
and the plan to run a NIC-local network stack (rump) on a secondary instead of forwarding.

All numbers below: QEMU `virt`, HVF, `SMP=3 MEMORY=2048`, virtual timer 24 MHz
(`TIMER_INTERVAL_TICKS = 0x10_0000` ⇒ one tick ≈ 43.7 ms). Reproduce with the in-kernel
self-test (`RUN_FWD_BENCH` in `src/smp.rs`, off by default — needs `SMP>=3` so its bench core
doesn't collide with a herd-pinned service on core 1).

## 1. Symptom

`curl` and `sshd` pinned to a secondary core worked but were extremely slow (commit
`c93e2d7`: "sshd + curl works on multikernel, albeit extremely slowly").

## 2. Root cause — reply latency bounded by the timer tick

One forwarded syscall is a round-trip: the secondary publishes the request to the BSP's inbox
and parks; the BSP's forward-server thread services it and publishes the reply to the
secondary's dedicated mailbox; the secondary observes the reply and returns.

The reply was published promptly. The cost was that the **secondary didn't get scheduled to
observe it**:

- The parked forwarder yields (`yield_now`) to the per-core **idle boot thread**, which `WFI`s.
- The idle boot thread is marked **cooperative** (`crates/akuma-exec/src/threading/mod.rs`,
  `IDLE_THREAD_IDX`). `schedule_indices` refuses to *involuntarily* preempt a cooperative
  RUNNING thread until its 100 ms timeout — so only a timer tick eventually switched back to
  the forwarder, and empirically that took ~3 ticks.
- There was **no doorbell** rung on the reply, so nothing woke the secondary sooner.

Measured per-round-trip: **~136 ms** (≈ 3.1 × the 43.7 ms tick).

## 3. The fix — doorbell wake + voluntary reschedule

Two parts, both required (`src/smp.rs`):

1. **BSP rings the requester's doorbell** the instant it publishes a reply
   (`service_fwd_requests` → `trigger_sgi_core(from, DOORBELL_SGI)`).
2. **The secondary's doorbell handler forces a voluntary reschedule** when a forward is
   outstanding (`FWD_AWAITING_REPLY`): it calls the new
   `akuma_exec::threading::request_voluntary_reschedule()` (sets `VOLUNTARY_SCHEDULE`) then
   rings its own scheduler SGI. A *voluntary* switch bypasses the cooperative-idle guard and
   preempts idle→waiter at once.

An earlier attempt that rang only an *involuntary* scheduler SGI made **no difference** (the
cooperative idle thread ignored it) — that dead end is why part (2) marks the switch voluntary.

## 4. Results

### Per-syscall latency (payload-free `clock_gettime`, 40 round-trips)

| reschedule | µs / round-trip |
|---|---|
| OFF (old, timer-tick bound) | ~136,000 |
| ON (doorbell wake) | **~45** |

≈ **3000× faster**. Verified identical on an idle dedicated core and on a core also running
sshd — so the win is the reschedule, not reduced contention.

### Bulk transfer — forwarded fetch of `/bin/curl` (1,511,904 B), reschedule ON

| `FWD_BOUNCE_CAP` | round-trips | time | throughput |
|---|---|---|---|
| 16 KiB | 93 | 20.2 ms | 74 MB/s |
| **64 KiB** | 24 | 12.6 ms | **119 MB/s** |
| 128 KiB | 12 | 13.4 ms | 113 MB/s |

The doorbell fix alone took this fetch from an extrapolated ~12.6 s (93 × 136 ms) to 20 ms at
16 KiB. Raising the bounce to 64 KiB adds 1.6× more, but 128 KiB is **worse** — we're
**copy-bound** past ~64 KiB (the byte-wise `AtomicU8` bounce copy dominates), and 128 KiB also
overflowed a 256 KiB thread stack via the `[u8; FWD_BOUNCE_CAP]` staging arrays. So **64 KiB is
the knee** and the value kept. The real bulk lever past here is a shared `(offset,len)` arena
that skips the per-chunk copy (`MULTIKERNEL.md` §16), not a bigger control-path buffer.

### End-to-end: `curl -sS https://ifconfig.me` on core 1 (all networking forwarded to core 0)

Real workload — forwarded DNS (UDP), the HTTPS TCP connection + TLS handshake to
`34.160.111.145:443`, and the HTTP GET; curl's own ELF (1.5 MB) is fetched over forwarded
open/read first. Both runs returned the public IP `87.71.63.90`.

| reschedule | spawn → result | speedup |
|---|---|---|
| OFF (old) | ~8.5 s | — |
| ON (fix) | **~1.6 s** | ~5.3× |

The end-to-end speedup is "only" ~5× (not 3000×) because curl's wall-time is dominated by real
internet DNS + TLS RTT, which is fixed regardless of forwarding speed; the 3000× shows in the
syscall-bound phases. (The ELF fetch stays fast even before the fix, because it runs on the boot
thread, which busy-spins — there's no idle thread to hand off to until the process is spawned.)

### curl comparison: native (core 0) vs forwarded (core 1)

Measured the way it's actually used — **interactively over SSH**, with curl's own `-w` phase
timing, fetching Apple's captive-portal page over HTTP and HTTPS (SMP=2, MEMORY=2048, HVF).
`ssh -p 2222` lands a core-0 shell (native smoltcp); `ssh -p 2323` lands a core-1 shell (all
networking forwarded to core 0). Both return HTTP 200 `<TITLE>Success</TITLE>`.

`curl -sS -o /dev/null -w ... http://www.apple.com/library/test/success.html https://…`

| phase | core 0 — native | core 1 — forwarded | overhead |
|---|---|---|---|
| **HTTP total** | **0.100 s** | **1.379 s** | ~14× |
| **HTTPS total** | **0.271 s** | **2.642 s** | ~10× |
| DNS lookup | 0.03–0.09 s | 0.72–0.76 s | ~10–28× |
| TCP connect | 0.04–0.10 s | 1.18–1.29 s | ~13–34× |
| TLS handshake | 0.21 s | 2.31 s | ~11× |

The native stack is fast (100 ms HTTP / 271 ms HTTPS). Forwarding is correct and usable but
carries a real per-syscall tax that accumulates over the handshake-heavy phases (every DNS/TLS/
HTTP round-trip is several forwarded socket syscalls). This is the motivation for a local rump
stack on the secondary (core 2), which avoids forwarding entirely — the Stage 2 comparison
(curl-over-rump) is pending.

**Methodology caveat (important):** native core-0 curl is **not** broken. An earlier apparent
"hang" was purely a test-harness artifact — running curl as a stripped-down herd *boot-time
oneshot* (sshd + other services disabled) tipped the timing-marginal native connect into a
stall. Run interactively (full boot, live session) it's the fast 0.1 s path above. Always
measure the native stack from an interactive shell, not a bare boot oneshot.

## 5. Related changes

- **herd: reject two services pinned to the same core.** A kernel runs one init program per
  core (`core_init` overwrites the pending program), so pinning both `sshd` and `netcheck` to
  core 1 silently clobbered one. herd now tracks pinned cores and logs an error rather than
  clobbering (`userspace/herd/src/main.rs`, `HerdState::pinned_cores`).
- **`THREADING_HEARTBEAT_INTERVAL`** was raised — the BSP idle loop's `[Thread0] loop=` serial
  spam was throttling the guest ~10× under HVF, which distorted timing.

## 6. Reproduction / gotchas

- Build: `cargo build --profile release-smp --features smp,no-tests`. The `no-tests` is because
  an **unrelated pre-existing** boot self-test (`test_mmap_file_oom`, `src/process_tests.rs`)
  panics on the SMP boot — worth a separate look; it is not related to forwarding.
- Set `RUN_FWD_BENCH = true` (with `SMP>=3`) to run the in-kernel forwarding self-test, which
  PASSes iff mean round-trip < `FWD_LATENCY_MAX_US` (5 ms), catching a doorbell-wake regression.
- The `curl` measurement enabled the `netcheck` herd service (`curl -sS https://ifconfig.me`,
  `core = 1`) on the disk and temporarily disabled `sshd` (both pin core 1). Needs
  `/etc/ssl/certs/ca-certificates.crt` staged for HTTPS.
- Grep the serial log with `grep -a` (it carries control chars).

## 7. Next: rump networking on a secondary (Stage 0/1)

Forwarding is now fast, but a secondary still has no *local* network stack. The experiment: run
the NetBSD rump TCP/IP stack on a secondary so its networking is local instead of forwarded.
Blocker + plan are in the memory notes and `MULTIKERNEL.md`: secondaries map **no device MMIO**
(`build_isolated_table` maps only the core's own GIC redistributor), so a secondary can't touch
a NIC as-is. Stage 0/1: give a dedicated NIC (a 3rd `virtio-net` on `virtio-mmio-bus.5`) to the
secondary — map that one virtio-mmio page into its isolated table, register `akuma_net`'s
runtime on that core, bind `rump_tap` to that slot — then run `rumphttp` (self-contained static
binary) pinned there and compare its HTTP-GET latency to the forwarded `curl` above.

### Stage 0/1 implementation status (in progress)

**Done — plumbing works end to end (no crash):**
- **Stage 0:** `CORE2_NIC=1` in `scripts/cargo_runner.sh` adds NIC2 on `virtio-mmio-bus.5` (own
  SLIRP + a host:8081→rump:80 forward). Build `--features smp,rump,no-tests`.
- **Stage 1 (kernel):** `RUMP_NIC_CORE` (=2) gets the virtio-mmio page mapped **twice** — into
  its bringup isolated table (`build_isolated_table`, for the NIC init on the boot thread) AND
  into every user address space's kernel overlay (`build_secondary_user_kernel_view` via the new
  `UserAddressSpace::map_device_page`, for the tap read/write during the pinned process's
  syscalls — Akuma is TTBR0-only, so a syscall runs at EL1 under the *user* table). A missing
  overlay mapping was the first bug: a level-0 translation fault at `DEV_VIRTIO_VA` from
  rumphttp's syscall context. `secondary_init_local_nic` registers `akuma_net`'s runtime on the
  core (no smoltcp) and binds `rump_tap::init_at(bus.5)`. herd pins `/bin/rumphttp` to core 2
  (`rumphttp.conf`).
- **Verified:** core 2 maps the MMIO, binds bus.5 (its own MAC, distinct from the BSP's bus.4),
  fetches the 13.5 MB ELF over forwarded VFS, spawns rumphttp, and its rump kernel boots
  (NetBSD 7.99.34) and creates `virt0` — all with no fault. Boot: `RUMP_NIC=1 CORE2_NIC=1 SMP=3`
  (RUMP_NIC=1 gives the BSP its own bus.4 so its auto-select doesn't grab core 2's bus.5).

**WORKING end to end (2026-07-01):** local rump networking on core 2 does **DHCP + ARP + TCP +
HTTP GET with zero forwarding** to core 0:
```
RUMPHTTP: dhcp_ipv4_oneshot -> 0     (got an IP over bus.5)
RUMPHTTP: connect 10.0.2.2:8000 -> 0
HTTP/1.0 200 OK ... hello-from-host-core2-rump
RUMPHTTP: PASS — fetched 212 bytes over the NetBSD rump stack (DHCP + TCP via /dev/net/tap0)
```
(GET target = a host `python3 -m http.server`; the earlier `connect 10.0.2.2:80 -> -1` was just
no server on the host's port 80, not a bug.)

**The DMA bug that blocked it (and its fix):** `virt_to_phys` was pure identity, but `TapNic`'s
2 KB `rx_buffer` is a field of the `static TAP` → it lives in the kernel's **replicated
`.data`/`.bss`**, which on a secondary is mapped at the kernel VA but backed by a PRIVATE
physical page (R1). So identity `v2p` handed the device the wrong physical address: the device
DMA-wrote the DHCP OFFER to the wrong page and the CPU read an all-zero (stale) buffer. TX worked
only because its buffers come from the identity-mapped partition heap. Fix: register a
secondary-specific `virt_to_phys` (`secondary_dma_virt_to_phys`) that WALKS this core's active
page table for the true PA (identity ranges translate to themselves; the replicated window is
physically contiguous via sequential `PartitionBump` pages, so a page-spanning buffer is safe).
Not a cache-coherency issue — the address was simply wrong.

**Remaining polish (not blockers):** the BSP now binds its rump tap to bus.4 explicitly so
`CORE2_NIC=1` works without `RUMP_NIC=1`; wire NIC→core assignment through herd core-init (see
the memory note) instead of the hardcoded `RUMP_NIC_CORE`.

### Stage 2 — real curl over a LOCAL rump stack on core 2 (mechanism achieved)

Goal: an unmodified `curl` on core 2 whose networking goes through core 2's OWN rump stack (not
forwarded to core 0). Approach: run a **second herd instance pinned to core 2** (herd
`--service <path>` to load its config files directly — a secondary's forwarded VFS does file
reads but not directory listing; see the getdents64 TODO). That herd creates a `stack=rump` box,
spawns `rump_server` in it (over core 2's local NIC / bus.5), and joins a curl + an sshd to the
box; the kernel's `rump_proxy` (all per-core-replicated state) sysproxy-routes the box's socket
syscalls to that rump_server.

**Working end-to-end on core 2:** box marked `stack=rump`; sysproxy channel attached;
`rump_server` created `virt0` and **DHCP'd 10.0.2.15/24** over bus.5; curl's
`socket`/`bind`/`sendto`/`recvmsg` all routed via `[RUMP-SP]` to rump_server; a DNS query went
out and a 161-byte response came back — **all over core 2's local rump stack, zero forwarding**.
`sshd-rump` (interactive SSH-over-rump) also comes up listening (:22 → host :2224).

**IMPROVED — real preemption on the secondary (2026-07-01):** the ~840 ms/round-trip stall was
the secondary running a *cooperative* idle thread with a coarse ~44 ms timer, so a just-woken
sysproxy peer couldn't be scheduled until the idle thread's 100 ms cooperative timeout. Fix (in
`src/smp.rs` + `crates/akuma-exec/src/threading/mod.rs`, "enable real preemption on secondaries"):
- **Make the secondary idle boot thread preemptible** (`threading::make_idle_preemptible()`,
  called from `secondary_steady_state`). On a secondary the boot thread is a pure idle/heartbeat
  loop (not the BSP's async/network runner), so it can take normal *involuntary* timer preemption;
  while it stayed cooperative, `schedule_indices` returned early on every tick and never ran its
  WAITING→READY wake pass, pinning every reschedule to the 100 ms timeout.
- **Drop the steady-state tick to ~10 ms** (matching the BSP): `arm_cntv_timer` now reads a live
  `SECONDARY_TICK_INTERVAL` atomic that `secondary_steady_state` lowers from the coarse ~44 ms
  bringup value (`TIMER_INTERVAL_TICKS`) to `cntfrq/100`. Bounds the reschedule granularity —
  and thus each pipe hop — to ~10 ms. R3b/bringup keep the coarse tick (atomic default).

Measured (`CORE2_NIC=1 RUMP_NIC=1 SMP=3 MEMORY=2048`): `sendto` ~840 ms → **~100–145 ms**;
`recvmsg` ~560 ms → **~96 ms**; a curl-over-rump DNS attempt's wall-time **11.5 s → 1.06 s**.
The residual per-syscall cost is now the *poll-hop count* — each sysproxy round-trip is bounded
by the ~10 ms tick (the spread 96–145 ms ⇒ ~10–14 hops, i.e. it is NOT the old 100 ms floor),
and every `ppoll`/`epoll` readiness check on a rump fd is itself a MSG_PEEK sysproxy round-trip.
The next lever is event-driven pipe wakeup (§8 #3), which replaces the ~10 ms poll granularity
per hop with a µs waker.

**Dispatch wiring — verified + hardened (2026-07-01).** `handle_syscall` (`src/syscall/mod.rs`) is
a single linear funnel: after bookkeeping it calls `rump_proxy::intercept_box_syscall` *before*
any native handler; `Some(r)` short-circuits native smoltcp dispatch, `None` falls through.
Coverage: `op_from_linux_sysno` maps every marshaled socket op (Linux 198–212) plus read/write/
readv/writev/close, and `intercept_box_syscall` routes each to the box's `rump_server` (only
rump-owned fds for read/write/close; never the server's own pid). `poll`/`ppoll`/`epoll` fall
through to the native handlers, which are rump-aware (a rump fd's readiness is a forwarded
MSG_PEEK — `poll.rs:434`).

**Hard isolation guarantee (closed a real leak):** `intercept_box_syscall` was reordered to check
`box_is_rump` FIRST and now enforces that, for a `stack=rump` box, *any socket-family syscall (by
number) or any syscall on a rump-owned fd* is owned by the proxy — routed if marshalable, else a
clean `EOPNOTSUPP`, but NEVER a native-smoltcp fall-through. Previously the op-map's `None` bail
ran before the rump-box check, so the socket syscalls the proxy doesn't marshal yet — `accept4`
(242), `recvmmsg` (243), `sendmmsg` (269) — fell through to native smoltcp *with a rump fd*. The
socket-family superset lives in `akuma_rump::syscall_translation::is_socket_family_sysno` (host
test `socket_family_covers_unmarshaled_ops_for_isolation`). Only truly-unrelated syscalls
(brk/mmap/openat/poll/read-on-a-real-file) still fall through — correct, they aren't networking.
Net: **no socket-family syscall from a rump box can reach native smoltcp with a rump fd.**

The remaining `rumpapple[000]` is a separate *functional* DNS issue (a 161-byte answer comes back
over recvmsg but getaddrinfo still retries), NOT a preemption or dispatch-wiring problem — it
reproduced identically before this fix.

Repro: `RUMP_NIC=1 CORE2_NIC=1 SMP=3 MEMORY=2048 cargo run --profile release-smp --features
smp,rump,no-tests`, with `/etc/herd/enabled/rumphttp.conf` (args `10.0.2.2 <port>`) and a host
HTTP server on that port.

## 8. Handoff — NEXT SESSION (resume here)

**Where we are:** curl-over-rump on core 2 is *functionally proven* (§ Stage 2): a real curl,
its sockets sysproxy-routed to a `rump_server` running on core 2's own kernel over the local NIC
(bus.5), DHCP + DNS observed over rump, `sshd-rump` listening. **Real preemption on secondaries
is now DONE** (see the Stage 2 "IMPROVED" note) — the ~840 ms/round-trip stall dropped to
~100–145 ms and a curl DNS attempt's wall-time fell 11.5 s → 1.06 s. Dispatch wiring was audited
and is correct. Two things remain: (A) the residual poll-hop latency, and (B) a *functional* DNS
bug that keeps curl at `rumpapple[000]`.

**DONE — "enable real preemption on secondaries":**
1. `threading::make_idle_preemptible()` clears the secondary idle boot thread's cooperative flag
   at `secondary_steady_state` entry, so the timer takes normal involuntary preemption (and the
   `schedule_indices` WAITING→READY wake pass runs every tick). *Real* preemption, not the
   voluntary-override hack — chosen because on a secondary the idle thread is a pure heartbeat
   loop, and involuntary preemption still respects any genuinely-cooperative thread if one is
   ever spawned there.
2. Steady-state tick lowered to ~10 ms via a live `SECONDARY_TICK_INTERVAL` atomic that
   `arm_cntv_timer` reads (`cntfrq/100`, clamped ≤ the coarse bringup value). R3b/bringup keep
   the coarse tick (atomic default), so R3b is unchanged.

**NEXT (A) — event-driven sysproxy pipe (the big remaining latency lever):** each pipe hop is now
bounded by the ~10 ms tick, not 100 ms, but it is still a *poll*. Make `pipe_write` fire the
blocked reader's waker + voluntary-reschedule to it (µs latency). The kernel side polls at
`schedule_blocking(now+1ms)` (`KernelPipeIo::yield_now`, `src/rump_proxy.rs:1062`); the
`rump_server` side (blocked `recvfrom` on the pipe) needs the same. `ppoll` already has a waker
path (`current_thread_waker`); the raw pipe/UnixSocket read likely doesn't reschedule on write.
Also: every `ppoll`/`epoll` readiness check on a rump fd is a MSG_PEEK sysproxy round-trip
(`poll.rs:434`) — cheaper once the hops are µs, but still one round-trip per probe.

**NEXT (B) — functional DNS bug (why curl stays `rumpapple[000]`):** the DNS query goes out
(`sendto 31 -> 31`) and a **161-byte answer comes back** (`recvmsg -> 161`), yet curl doesn't
proceed to `connect()` the resolved IP — it closes and re-issues the DNS query. Suspect
`proxy_recvmsg` not populating `msg_name` (source addr) so musl's resolver rejects the reply, or
the answer being SERVFAIL from the core2 SLIRP resolver. This is independent of preemption/
dispatch and reproduced before the preemption fix. Debug `proxy_recvmsg` (`src/rump_proxy.rs:944`)
vs what musl's `__res_msend`/`getaddrinfo` expects.

**How to test:** boot `CORE2_NIC=1 RUMP_NIC=1 SMP=3 MEMORY=2048 cargo run --profile release-smp
--features smp,rump,no-tests > logs/x.log 2>&1`. Watch `logs/x.log` for `[RUMP-SP] sendto -> 31
(Nus)` — N now ~100–145 ms (was ~840 ms). Success target = curl prints `rumpapple[200]:...` (not
`[000]`), which needs BOTH (A) and (B). Then SSH into the rump box: `ssh -p 2224 root@localhost`
(via `CORE2_SSH_PORT`, net2 → core-2 rump sshd:22) and run curl there → interactive
SSH-over-rump; that gives the 3rd number for the §4 table (native 0.1 s / forwarded 1.4 s /
rump-local ?).

**Config wiring in place (committed):** `bootstrap/etc/herd/enabled/core2herd.conf` launches a
second herd pinned to core 2 with `--service /srv/core2/etc/herd/{rumpnet,netcheck-rump,sshd-rump}.conf`
(explicit files — no dir scan, working around the getdents64 gap). herd gained `--service <path>`
(`explicit_service_files()`), `--enabled-dir`. Runner adds a `CORE2_SSH_PORT` (2224) hostfwd on net2.
Deploy after edits: rebuild herd (`userspace/ cargo build --release -p herd`), copy to
`bootstrap/bin/herd`, and docker-mount `disk.img` to place `/bin/herd` + `/srv/core2/etc/herd/*.conf`.

**Also open (separate TODO):** forwarded directory listing (`getdents64`) is classified-but-
unimplemented for secondaries (see the memory note); it's why herd needs `--service`. Proper fix:
marshal dirents through the bounce in `service_forwarded_syscall`.
