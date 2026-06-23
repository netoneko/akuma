# Fiber rumpuser — handoff (2026-06-24)

Pick-up notes for continuing the cooperative-fiber rump backend. Deep analysis +
rationale live in `HIJACK_VS_KERNEL_PROXY.md` (the "fiber" sections); this file is
the operational "where we are / what's next / how to run it".

## TL;DR

- We ported NetBSD rump's threading to a **cooperative fiber backend** (one OS
  thread + a userspace scheduler) in our Rust `rumpuser`. It is now the **DEFAULT**
  cargo feature **`threads_fiber`** (2026-06-24; `--no-default-features` for the
  legacy pthread backend). Collapses the ~19 rump kthreads → 1 OS thread, killing
  the single-vCPU futex thundering-herd.
- **WORKS & verified:** the cooperative scheduler; `rump_init`; thread-collapse;
  **full in-process (model-C) networking** (`rumphttp`); AND — as of **2026-06-24**
  — the **sysproxy `rump_server` networked path end-to-end**: a `stack=rump` box
  runs **unmodified `curl` over the NetBSD rump stack** (DHCP + DNS + TCP + HTTP GET
  proxied over the kernel pipe), all on **one OS thread**.
- **🏆 RESULT (the whole point):** `box use rumpnet -i /bin/curl -sS
  http://example.com/` → HTTP 200 in **16.3s on fiber vs 62.8s on the pthread
  baseline (~3.85× faster)**, `rump_server` = **1 OS thread (vs 19)**, PSTATS
  **`clone=0 futex=0` (vs `clone=20 futex=2606`)**. Same workload, same box.
- **DONE (2026-06-24, see "PORT — DONE" section below):** ported our C wrapper
  `rump_server.c` → Rust (`rumpuser/src/rump_server.rs`, feature `rump_server_main`)
  and archived the C harnesses **+ their docker test scripts** to `rumpuser/c_tests/`.
  Verified **perf-neutral**: curl over the Rust-`main` rump_server = **HTTP 200 in
  16.3s** (identical to the C wrapper), `ps` = **1 OS thread**, Rust fiber test PASS.
- **NEXT:** event-driven channel wakeup to shave the residual per-syscall latency
  (~1s/proxied-syscall from poll/yield + ~10ms rump-clock granularity); Phase 5
  herd/box `--net` auto-spawn ergonomics.

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

## What's RESOLVED (2026-06-24): networked sysproxy `rump_server` under fiber

The fiber **`rump_server`** (sysproxy / shared-stack path) now works end-to-end.
Three bugs fixed (all in *our* files; NetBSD source stays unmodified):

1. **`rump_server.c` park loop** — was `for(;;) sleep(3600)`, which under fiber
   blocks the single OS thread (a kernel `nanosleep`, NOT the cooperative
   `rumpuser_clock_sleep`), so the serve-loop fiber never ran and never sent its
   handshake banner → kernel handshake read timed out (`errno 5`). Fixed: under the
   fiber backend the main thread parks via `for(;;) rumpuser_akuma_yield()` (runs
   the scheduler); pthread keeps the cheap `sleep`.
2. **`sp_serve_fd.c` thread/lock redirect** — the sp-server's `pthread_create` AND
   its `pthread_mutex_*`/`pthread_cond_*` are routed to cooperative fiber primitives
   at runtime (via `rumpuser_akuma_cooperative()`). The cond redirect is the key
   one: NetBSD's COPYIN `waitresp` (`rumpuser_sp.c:148`) does a real
   `pthread_cond_wait`, which on the one OS thread blocks it (a futex) and
   DEADLOCKS the scheduler — a worker fiber parked mid-`bind` while the receiver
   fiber that would wake it can never run (a proxied `bind` then stalled to the
   read timeout → DNS failed). The shims (`akfiber_sp_*` in `fiber.rs`) back each
   pthread object with a fiber wait-queue. **This replaced the "blocking-I/O
   offload thread" the doc proposed — the lighter cooperative redirect sufficed.**
3. The receiver loop polls with timeout 0 + `rumpuser_akuma_yield()` when idle
   under fiber (a blocking `poll(INFTIM)` would freeze the one OS thread).

### Results (same workload, same `stack=rump` box, MEMORY=512M)

| metric | pthread baseline | **fiber** |
| --- | --- | --- |
| `curl -sS http://example.com/` (HTTP 200) | **62.8 s** | **16.3 s** (3/3 stable) |
| `rump_server` OS threads (`ps`) | 19 | **1** |
| PSTATS `clone` / `futex` | 20 / 2606 (236 s) | **0 / 0** |

DHCP (`10.0.2.15`), DNS-over-rump, TCP, and HTTP GET all proxied over the kernel
pipe to a single-OS-thread cooperative `rump_server`. ~3.85× faster end-to-end AND
the 19→1 thread collapse / futex-storm elimination.

### Tests

- **Rust unit test** (`rumpuser/src/fiber.rs` `mod tests`): a mutex+condvar
  ping-pong on the actual `akfiber_sp_*` shims — the deadlock regression test (if
  `cond_wait` blocked the OS thread it would never reach `2*ROUNDS`). Run with
  `userspace/rumpkernel/test-fiber.sh` (cross-builds with rust-lld, runs in a
  Docker linux/arm64 alpine; `--test-threads=1`). PASS.
- **C harness** `rumpuser/test_fiber.c` gained Test C (same ping-pong via the
  `akfiber_sp_*` ABI), alongside Tests A/B.

### How to run the fiber networked path

```bash
cd userspace/rumpkernel
(cd rumpuser && cargo build --release --target aarch64-unknown-linux-musl --features threads_fiber)
./docker-build-rump-server.sh           # → out/rump_server_akuma (fiber)
cp out/rump_server_akuma ../../bootstrap/bin/rump_server   # back up the pthread one first!
cd ../.. && scripts/populate_disk.sh            # FULL populate: puts etc/herd/enabled/rumpnet.conf on disk
RUMP_NIC=1 MEMORY=512M scripts/cargo_runner.sh target/aarch64-unknown-none/release/akuma > logs/x.log 2>&1 &
# wait for "[RUMP-SP] box=... proxy ready", then over SSH:
#   box use rumpnet -i /bin/curl -sS http://example.com/
```

### Open / next

- **Residual latency:** ~1 s per proxied syscall (the poll/yield + ~10 ms rump
  clock granularity), so curl is 16 s not sub-second. Event-driven channel wakeup
  (wake the server's poll on a kernel pipe-write instead of ~tick polling) is the
  next lever.
- Phase 5 herd/box `--net` auto-spawn ergonomics.

## PORT — DONE (2026-06-24): C wrapper → Rust; C tests + scripts archived

Goal (user, 2026-06-24): get rid of our hand-written C in `rumpuser/` by porting it
to Rust, **keeping NetBSD's `rumpuser_sp.c` unmodified**, and moving our C test
harnesses to `rumpuser/c_tests/`. A **cleanliness refactor, NOT a perf change**.

**Result — all three steps done and verified:**
- **Step 1 ✓** archived the 7 C harnesses **and the 5 docker `*-test.sh` scripts**
  that drive them into `rumpuser/c_tests/` (user asked to move the scripts too).
  The moved scripts' `HERE` now resolves via `$(dirname "$0")/../..` so the Docker
  mount + relative paths still point at the rumpkernel root. The 5 in-script test
  paths were repointed to `rumpuser/c_tests/...`.
- **Step 2 ✓** ported `rump_server.c` → `rumpuser/src/rump_server.rs`, a
  `#[no_mangle] pub extern "C" fn main` gated behind the new `rump_server_main`
  cargo feature (off by default — avoids a duplicate-`main` collision with the
  other consumers of the shared `.a`). `docker-build-rump-server.sh` rebuilds the
  `.a` `--features rump_server_main` right before its link and **drops
  `rump_server.c` from the gcc line** (crt0 → the Rust `main`, force-included via
  `--whole-archive`). `rump_server.c` archived to `c_tests/`. clippy clean both
  with and without the feature.
- **Step 3 ✓** rebuilt + full-populate + `RUMP_NIC=1` boot:
  `box use rumpnet -i /bin/curl -sS http://example.com/` → **HTTP 200 in 16.3s**
  (same as C), box log shows the Rust `main`'s byte-identical `RUMP_SERVER:` output
  (DHCP `10.0.2.15`, `rumpuser_sp_init_fd(3) -> 0`, `SERVING ... (net=up)`), `ps`
  shows `rump_server` as **1 OS thread**. `./test-fiber.sh` (Rust unit test) PASS.

Historical bring-up notes for the port (kept for reference):

### What ports vs. what MUST stay C (know this before starting)

| file | action | why |
| --- | --- | --- |
| `rump_server.c` (our wrapper/main) | **PORT → Rust** | calls only public/extern APIs (`rump_init`, `rump_pub_netconfig_*`, `rumpuser_sp_init_fd`, `rumpuser_akuma_*`, libc) — clean to port. |
| `sp_serve_fd.c` | **KEEP C** | it `#include`s NetBSD `rumpuser_sp.c` to (a) host the `#define pthread_*`→fiber redirects *into* it and (b) call its `static` fns (`readframe`/`handlereq`/`banner`/`spclist`…). Rust can't call C statics or do that preprocessor redirect. This file **is** the bridge that "keeps NetBSD's rump_server" — leave it. |
| `csupport.c` | KEEP C (for now) | libkern byte-loop overrides via `-Wl,--allow-multiple-definition` + a C-variadic `rumpuser_dprintf` + `rust_eh_personality` stub. Awkward in Rust; revisit later. |
| `rumpcomp_tap.c` | KEEP C (for now) | the `/dev/net/tap0` virtif backend; portable in principle but it's the rump virtif contract + fiber RX, not "the wrapper". Separate task. |

### Step 1 — archive C test harnesses → `rumpuser/c_tests/`

Pure dev harnesses (not linked into any shipped binary):
`test_fiber.c` (superseded by the Rust `mod tests`), `test_init.c`, `test_init_live.c`,
`test_net.c`, `akctx_smoke.c`, `sp_fd_test.c`, `sp_client_test.c`.
Update the 4 scripts that reference them to the new path:
`docker-rumpuser-test.sh` + `docker-sysproxy-spike.sh` (`test_init.c`),
`docker-net-test.sh` (`test_net.c`), `docker-sp-fd-test.sh` (`sp_fd_test.c`),
`docker-sysproxy-client-test.sh` (`sp_client_test.c`).
LEAVE in place (demos/payloads, not tests, used by other scripts): `hijack.c`,
`rumphttp.c`, `rumpserver.c`, `virtif_user_instr.c`.

### Step 2 — port `rump_server.c` → Rust

Replicate its `main` in Rust: arg parse (`--fd N`, `--net`, `--if`, `--log`),
`redirect_log` (open/dup2/mkdir), `rump_init()`, `--net` → `rump_pub_netconfig_ifcreate`
+ `rump_pub_netconfig_dhcp_ipv4_oneshot`, then `rumpuser_sp_init_fd(fd,"NetBSD",
"7.99.34","evbarm64")`, then the **cooperative park loop already proven in
`rump_server.c`**: `if rumpuser_akuma_cooperative()!=0 { loop { rumpuser_akuma_yield() } }
else { loop { sleep(3600) } }`. Declare the rump/libc entry points `extern "C"`
(several libc externs already live in `lib.rs`).

**Recommended build mechanic (least disruptive — keeps the working gcc final link):**
add a **feature-gated** entry to the rumpuser crate:
`#[cfg(feature="rump_server_main")] #[no_mangle] pub extern "C" fn main(argc: c_int,
argv: *const *const c_char) -> c_int { … }`. Then in `docker-build-rump-server.sh`:
build the staticlib with `--features rump_server_main` (threads_fiber is now default)
and **drop `rump_server.c` from the gcc link line** — crt0 calls the Rust `main`; keep
linking `sp_serve_fd.o`, `rumpuser_errtrans.o`, `rumpcomp_tap.c`, `csupport.c` + the
rump `.a`s.
- **GOTCHA — don't put `main` in the default `.a`:** rumphttp/sic/tests define their
  own `main`; an unconditional `main` in the shared `librumpuser_akuma.a` → duplicate
  symbol. Hence the `rump_server_main` feature. Note the `.a` path is shared
  (`target/aarch64-unknown-linux-musl/release/`), so the rump_server build must
  rebuild the `.a` WITH the feature right before its link; other consumers rebuild
  the default `.a`. (Alternative, cleaner but more plumbing: a dedicated `[[bin]]`
  target that pulls the C objects + rump whole-archive libs via `build.rs`
  `cargo:rustc-link-arg`; must link in Docker — native musl gcc, the macOS linker
  rejects GNU `-Wl` flags, as `test-fiber.sh` documents.)

Then archive `rump_server.c` itself to `c_tests/` as reference.

### Step 3 — verify (refactor, so perf must be UNCHANGED)

Rebuild lib + `rump_server`, full `populate_disk.sh`, `RUMP_NIC=1` boot, then
`box use rumpnet -i /bin/curl -sS http://example.com/` → HTTP 200 in **~16.3s**
(same as the C wrapper) and `ps` still shows `rump_server` as **1 OS thread**.
Re-run `./test-fiber.sh` (Rust tests) — still PASS.

## Files

Key files (the 2026-06-24 networked-sysproxy fix touches the ★ ones — UNCOMMITTED):
- `rumpuser/src/fiber.rs` ★ — the cooperative backend; now also the `akfiber_sp_*`
  cooperative pthread-compat shims + a Rust `mod tests` (the deadlock regression).
- `rumpuser/sp_serve_fd.c` ★ — sp-server fiber glue: `pthread_create`/`detach` AND
  `pthread_mutex_*`/`pthread_cond_*` runtime redirect; the timeout-0 poll+yield loop.
- `rumpuser/src/rump_server.rs` ☆ — **NEW (2026-06-24 port)**: the Rust `rump_server`
  wrapper `main` (was `rump_server.c`); feature `rump_server_main`. Same arg parse /
  redirect_log / rump_init / `--net` / sp_init_fd / cooperative park loop.
- `rumpuser/src/lib.rs` ★ — `#![cfg_attr(not(test), no_std)]` + gated panic handler
  so the crate's Rust tests build; `rumpuser_akuma_cooperative()`/`_yield()` hooks;
  now also `#[cfg(feature="rump_server_main")] mod rump_server;`.
- `rumpuser/Cargo.toml` ★ — `threads_fiber` DEFAULT; new `rump_server_main` feature.
- `docker-build-rump-server.sh` ☆ — rebuilds the `.a` `--features rump_server_main`
  and drops `rump_server.c` from the gcc link (crt0 → Rust `main`).
- `rumpuser/c_tests/` ☆ — **NEW**: archived dev C harnesses (`rump_server.c`,
  `test_*.c`, `sp_*_test.c`, `akctx_smoke.c`) + their 5 docker `*-test.sh` drivers.
- `rumpuser/test-fiber.sh` ★ — Rust-test runner (rust-lld cross-build + Docker arm64).
- `rumpuser/sp_serve_fd.c` — KEPT C (it `#include`s NetBSD's unmodified `rumpuser_sp.c`).
- `rumpuser/rumpcomp_tap.c` — fiber RX (thread + cooperative read).
- `docs/HIJACK_VS_KERNEL_PROXY.md` — analysis + results.

NOTE (user drives commits): nothing here is committed by the agent.

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
