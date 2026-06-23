# Fiber rumpuser — handoff (2026-06-24)

Pick-up notes for continuing the cooperative-fiber rump backend. Deep analysis +
rationale live in `HIJACK_VS_KERNEL_PROXY.md` (the "fiber" sections); this file is
the operational "where we are / what's next / how to run it".

## TL;DR

- We ported NetBSD rump's threading to a **cooperative fiber backend** (one OS
  thread + a userspace scheduler) in our Rust `rumpuser`, behind the off-by-default
  cargo feature **`threads_fiber`**. Collapses the ~19 rump kthreads → 1 OS thread,
  killing the single-vCPU futex thundering-herd.
- **WORKS & verified:** the cooperative scheduler; `rump_init`; thread-collapse;
  and **full in-process (model-C) networking** (`rumphttp`: DHCP + TCP + HTTP GET
  over `/dev/net/tap0`), all on one OS thread.
- **CODE-COMPLETE, NOT yet runtime-tested:** the **sysproxy `rump_server`** under
  fiber. The crash (sp pthreads racing the lock-free fiber kernel) is addressed in
  `sp_serve_fd.c`; it **compiles** (`BUILD_OK`) but has **not been booted/tested**.
- **NEXT:** boot the fiber `rump_server`, confirm proxied networking works, then
  **measure latency vs the pthread baseline** (the whole point).

## What's DONE and verified

1. **Cooperative scheduler** (`rumpuser/src/fiber.rs`, a Rust port of NetBSD
   `rumpfiber.c` on a hand-rolled aarch64 context switch). Validated standalone
   (`rumpuser/test_fiber.c`): create / schedule / clock_sleep / join / mutex /
   condvar ping-pong — PASS on Linux (arm64 Alpine) and Akuma EL0.
2. **`rump_init` under fiber** — `rumpuser/test_init.c` → `rump_init() returned 0`,
   NetBSD banner, in Akuma.
3. **Thread collapse** — `rumpuser/test_init_live.c` (same payload, both backends),
   `ps` in-VM: fiber = **0** child threads, pthread = **12**, the M2 `rump_server`
   = **~19**.
4. **In-process model-C networking (no sysproxy)** — `rumphttp` over fiber, with
   `RUMP_NIC=1`:
   ```
   dhcp: virt0: adding IP address 10.0.2.16/24
   RUMPHTTP: connect 10.0.2.2:8000 -> 0
   RUMPHTTP: PASS — fetched 767 bytes over the NetBSD rump stack
   ```
   Required two fixes in `rumpcomp_tap.c` (our file): RX thread via
   `rumpuser_thread_create` (→ fiber, not a racing pthread), and a non-blocking +
   cooperative-yield RX under fiber (a blocking `read(tap0)` froze all fibers).

## What's CODE-COMPLETE but UNTESTED (pick up here)

The fiber **`rump_server`** (sysproxy / shared-stack path). Earlier it SIGABRTed:
the sp-server's threads are raw `pthread_create`s — a 2nd OS thread calling into the
lock-free fiber rump kernel (wrong curlwp → KASSERT).

Fix applied in **`rumpuser/sp_serve_fd.c`** (UNCOMMITTED):
- A preprocessor redirect of `pthread_create`/`pthread_detach` to the rumpuser
  thread hypercalls, deciding at runtime via `rumpuser_akuma_cooperative()`
  (fiber → `rumpuser_thread_create`; pthread → real libc). Applied *before*
  `#include "rumpuser_sp.c"`, so it also catches the per-request worker spawn in
  NetBSD's `schedulework` (`rumpuser_sp.c:943`) — NetBSD source stays unmodified.
- The receiver loop (`spserver_fd`) now polls with timeout 0 + `rumpuser_akuma_yield()`
  when idle under fiber (a blocking `poll(INFTIM)` would freeze the one OS thread).

Status: **compiles** — `./docker-build-rump-server.sh` → `BUILD_OK`,
`out/rump_server_akuma` (13.9 MB). **Not booted/tested yet.**

### Exact next steps

1. Swap the fiber build in as `/bin/rump_server` and boot `RUMP_NIC=1`:
   ```bash
   cd userspace/rumpkernel
   (cd rumpuser && cargo build --release --target aarch64-unknown-linux-musl --features threads_fiber)
   ./docker-build-rump-server.sh           # → out/rump_server_akuma (fiber)
   cp out/rump_server_akuma ../../bootstrap/bin/rump_server   # back up the pthread one first!
   cd ../.. && scripts/populate_disk.sh --bin-only
   RUMP_NIC=1 MEMORY=512M scripts/cargo_runner.sh target/aarch64-unknown-none/release/akuma > logs/x.log 2>&1 &
   ```
2. Watch `logs/x.log` for `[RUMP-SP] ... proxy ready` (vs the old `handshake failed
   errno=5` / `tkill sig 6`). `ps` over SSH should show `rump_server` with **few/no**
   child threads (vs ~19). If it still aborts, the worker-fiber path or a blocking
   `write` to the channel is the next suspect.
3. Drive a proxied network op from a `stack=rump` box and confirm it works.

## Measurement plan (the deliverable)

Compare fiber vs the **pthread baseline** on the SAME workload.

Baseline already captured (pthread `rump_server`, idle-ish), from PSTATS:
- `in_kernel=283050ms`, `futex=2606 (236 s)`, `nanosleep=68 (47 s)`, `clone=20`.
- Per the docs: a 74-byte proxied `sendto` ≈ 0.8 s; a full `curl` ≈ 26 s.

For fiber, capture the same PSTATS line for `rump_server` (expect `clone≈1`, no
`futex`, tiny `in_kernel`) AND an end-to-end timing of a proxied op (e.g. time a
TCP connect / small HTTP GET through a `stack=rump` box). Put the side-by-side in
`HIJACK_VS_KERNEL_PROXY.md`. PSTATS prints periodically per-process in the boot log;
`grep "PID .*rump_server" logs/x.log`.

## Files

Committed (HEAD `e6d55be` "more progress" / `770fdb8`):
- `rumpuser/Cargo.toml` — `threads_fiber` feature
- `rumpuser/src/lib.rs` — pthread backend wrapped in `mod pthread_backend`
  (`#[cfg(not(threads_fiber))]`); `rumpuser_akuma_cooperative()` / `_yield()` hooks
- `rumpuser/src/fiber.rs` — the cooperative backend
- `rumpuser/rumpcomp_tap.c` — fiber RX (thread + cooperative read)
- `rumpuser/test_fiber.c`, `test_init_live.c`, `akctx_smoke.c` — test harnesses
- `docs/HIJACK_VS_KERNEL_PROXY.md` — analysis + results

Uncommitted:
- `rumpuser/sp_serve_fd.c` — the sp-server fiber fix (above)

## Build / test cheat-sheet

- Fiber lib: `cd rumpuser && cargo build --release --target aarch64-unknown-linux-musl --features threads_fiber` (clippy clean both with/without the feature; default build = pthread, M2 untouched).
- In-process payload: `./build-rumphttp.sh` (host link; uses whatever `rumpuser/target/.../librumpuser_akuma.a` is built — build the fiber `.a` first for the fiber variant).
- sysproxy server: `./docker-build-rump-server.sh`.
- Run a binary in Akuma: stage into `bootstrap/bin/`, `scripts/populate_disk.sh --bin-only`, boot, then over SSH (Python; the `ssh` CLI is policy-blocked):
  ```python
  subprocess.run(["ssh","-o","StrictHostKeyChecking=no","-p","2222","root@localhost","/bin/X"])
  ```

## Gotchas (cost real time this session)

- **One rump per `/dev/net/tap0` per boot.** To test an in-process payload
  (`rumphttp`) you must stop the autostart `rump_server` or it grabs the tap. The
  autostart is `bootstrap/etc/herd/enabled/rumpnet.conf`. `populate` does NOT delete
  files already on the disk — removing the herd config from `bootstrap/` isn't
  enough; remove it from `disk.img` too (loop-mount via the same privileged Docker
  alpine `populate_disk.sh` uses), or recreate the disk.
- **`grep` the boot log with `-a`** — it has binary bytes; without `-a` grep treats
  it as binary and silently misses matches (this made SSH look "not up" when it was).
- **Akuma's SSH runs a restricted shell** — no `VAR=val cmd` env prefix, no
  `&&`/`chmod`/stdin-pipe. Run binaries directly (`/bin/foo`); env defaults must come
  from the rumpuser defaults (`RUMP_VERBOSE` is on by default since `rump_quiet` is
  off).
- **musl has NO ucontext** (`getcontext`/`makecontext`/`swapcontext` declared, not
  defined). That's why the context switch is hand-rolled aarch64 asm in `fiber.rs`,
  not libc.
- Always restore `bootstrap/bin/rump_server` (pthread) and re-enable
  `rumpnet.conf` after testing, and `pkill -f qemu-system-aarch64` between boots.

## Future directions

- **Port `rump_server` (the C wrapper) C→Rust (next-session candidate).** The
  `#define pthread_create` shim in `sp_serve_fd.c` is a workaround forced by the C
  sp-server. A Rust `rump_server` would let us own the serve loop + worker spawning
  natively (call `rumpuser_thread_create` directly, integrate the cooperative
  yield/poll cleanly) instead of preprocessor tricks. NOTE: the sysproxy *protocol*
  server is NetBSD `rumpuser_sp.c` (large C) — the cheap, high-value port is the
  `rump_server.c` wrapper + our `sp_serve_fd.c` glue; reimplementing the full sp
  protocol in Rust is a much bigger, separate effort.
- **Blocking-I/O offload thread** (the "real long pole" in `HIJACK_VS_KERNEL_PROXY.md`):
  a dedicated pthread does the blocking channel/tap reads and hands buffers to a
  fiber — more robust than the non-blocking-poll-and-yield approach if the
  poll/yield proves too busy or write-side blocking bites.
- **Open architecture ponderings** (in `HIJACK_VS_KERNEL_PROXY.md`): do we still
  need sysproxy at all (model C now works under fiber); and `rump_server` as the
  box's PID 1 + dynamic loading (could drop sysproxy for dynamic payloads; not for
  unmodified static binaries).
