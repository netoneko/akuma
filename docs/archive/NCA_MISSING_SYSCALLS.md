# NCA (native-cli-ai) — Missing Kernel Pieces for a Rust AI CLI

**Date:** 2026-08-17. **Scope:** getting `nca` (upstream
`madebyaris/native-cli-ai`) to build and run on Akuma (devbox-smoltcp, nightly
rustc 1.99.0 in the guest).
**Status:** **submodule swapped to upstream 2026-08-17**; host-built upstream
binary **verified against host Ollama from the guest** (two full round trips).
In-guest build still blocked by the spawn EFAULT (§1); the slow-model hang
(§2b) was **root-caused and FIXED 2026-08-17** — four kernel defects, see
`SOCKET_DELAYED_FIRST_BYTE_HANG.md` § Resolution. Patches needed on top of upstream HEAD are in
`userspace/nca/upstream-akuma-patches.patch`.
Later on 2026-08-17: nca driven against **Z.ai GLM-4.7 over real HTTPS** from
the guest (§6b); the stdin-pipe transfer route in §3 was **retested and works**
(the "use HTTP" advice there is superseded); and the exit-time
`tokio-rt-worker` abort was traced to an **upstream tokio shutdown bug**, not a
kernel gap (§7).

> **Deployment trap seen twice this session:** the patches live in the submodule
> working tree, but `bootstrap/bin/nca` is a *build artifact* and does not
> update itself. A stale `bootstrap/bin/nca` (Aug 12, 8 862 136 bytes) was
> shipped into the guest five days after the §0 patches were written, so the
> AF_UNIX bind was still fatal and the §2 fix looked broken. Compare sizes
> before debugging: patched is **10 913 904** bytes. Rebuild with
> `userspace/build.sh --nca-only` (installs straight into `bootstrap/bin/`).

## TL;DR

| Problem | Status |
| --- | --- |
| `cargo build` in guest: `could not execute process rustc ... Bad address (os error 14)` | **Open** — not a kernel syscall EFAULT; see §1 |
| `socket(AF_UNIX)` unimplemented → nca IPC dies with EAFNOSUPPORT | **Patched in-tree** (bind failure degrades to IPC-less mode) |
| Upstream HEAD won't cross-build: `openssl-sys` via `mcpr` + core's own `reqwest` | **Patched in-tree** (two one-liners, §4b) |
| `scp`/SFTP into the guest times out | Use HTTP: host `python3 -m http.server`, guest `wget http://10.0.2.2:<port>/...` (§3) |
| nca↔Ollama verified | qwen3.5:0.8b instant round trips OK; gemma4:e4b hang **FIXED 2026-08-17** (§2b) — it was not prefill time, it was four kernel defects, dominant one a `SynSent` socket reported read-closed |
| `tokio-rt-worker` panics `Option::unwrap()` on `None` at `fs/file.rs:745` on **exit** (session itself fine) | **Not a syscall gap** — upstream tokio state bug on the runtime-shutdown path, §7 |
| GLM-4.7 via Z.ai verified end-to-end from the guest | §6b. Context window mis-detected as 32 000 (real: 204 800) — `model_limits.rs` has no GLM entry |

## 0. Submodule swap (2026-08-17)

`userspace/nca/native-cli-ai` now points at **upstream**
`https://github.com/madebyaris/native-cli-ai.git` (main @ `2268932`), replacing
the `netoneko/native-cli-ai` dev fork. The fork's raison d'être (clipboard
gating, rustls switch — commit `da4d0f7`) was evaluated against upstream HEAD:

- workspace `reqwest` is already rustls-only upstream; **but** `crates/core`
  declares its *own* `reqwest` without `default-features = false` → native-tls
  → `openssl-sys` (the fork had fixed exactly this line, and it never landed
  upstream);
- `mcpr = "0.2.3"` is a **declared-but-never-imported** dependency of
  `nca-core` that pulls reqwest with default features (native-tls) again —
  mcpr 0.2.3 has no rustls feature;
- upstream `ipc.rs` was restructured (Windows TCP fallback, `bind_listener`),
  so the AF_UNIX fallback patch was re-adapted.

Local patches carried in the submodule working tree (saved as
`userspace/nca/upstream-akuma-patches.patch`):

1. `crates/runtime/src/ipc.rs` — `bind_listener` failure ⇒ warn + IPC-less
   mode instead of fatal `Err` (akuma has no AF_UNIX stream sockets).
2. `crates/core/Cargo.toml` — `mcpr = { version = "0.2.3", optional = true }`
   (unused dep; nothing enables it, so it is never resolved).
3. `crates/core/Cargo.toml` — `reqwest = { version = "0.12", default-features
   = false, features = ["json", "stream", "rustls-tls"] }`.

All three are upstreamable as-is. With them, upstream HEAD builds
`aarch64-unknown-linux-musl` static (10.9 MB stripped) and runs on akuma.

## 1. Guest build: cargo→rustc spawn fails with EFAULT

**Symptom:** `cargo build --release -p nca-cli --offline` in
`/tmp/native-cli-ai` (fresh upstream clone, `cargo clean`ed) fails on the first
*registry* dependency crates:

```
error: could not compile `libc` (build script)
Caused by:
  could not execute process `rustc --crate-name build_script_build ...` (never executed)
Caused by:
  Bad address (os error 14)
```

**What it is NOT:**

- Not argv length — single args up to 64 KB and 70-arg argvs exec fine
  (`rustc --version <64KB of 'a'>` OK).
- Not envp size — 256 KB of env vars exec fine.
- Not the `env_clear()` path — a std mimic of cargo's spawn (piped stdio,
  env_clear, big argv, from 8 threads) passes 40/40 rounds.
- Not `current_dir` (which forces musl std off posix_spawn onto fork+exec) —
  mimic with `current_dir("/tmp/native-cli-ai")` + 40 large args passes 40/40.
- Not disk/registry — `cargo build` of a minimal crate with one registry dep
  (`cfg-if`) **succeeds** in the guest.
- Not a kernel-returned EFAULT — with `SYSCALL_ERRNO_DIAG_ENABLED` (narrowed to
  EFAULT-only) the kernel console logs **zero** EFAULT returns while cargo
  reports three. A deliberate-EFAULT canary (`write(1, 0x10, 8)`) does log, so
  the hook works. The kernel also logs every `execve(path=...)` at
  `src/syscall/proc.rs` `sys_execve` — **no execve line appears for the failing
  rustc spawns**. So the errno originates *before* execve: in the spawn child's
  pre-exec dance (chdir/dup2/CLOEXEC-pipe read), or is fabricated in userspace
  (stale read off the child-error pipe).

**Correlates that narrow it:**

- Same cargo, same registry cache, building `/tmp/akuma` (first-party crates
  only, fully cached *or* after `cargo clean -p akuma-exec` + rebuild) spawns
  rustc fine.
- The failure is racy per-spawn (a `RUSTC=/bin/sh` shim saw ~8 probe spawns
  succeed, then the first big compile spawn EFAULT).
- Retrying the build in a loop does **not** grind through (80 attempts, zero
  progress; failed crates never cache).

**Debug artifacts left behind (host):** `src/config.rs`
`SYSCALL_ERRNO_DIAG_ENABLED` flipped to `true` with the EINVAL arm commented
out in `src/syscall/mod.rs` (readlinkat EINVAL probes flood the log otherwise).
Revert both when this is solved.

**Next steps if resumed:** strace-less guest means instrument musl std's
`posix_spawn`/fork child path, or add a one-shot debug print in the kernel's
`dup2`/`chdir`/pipe-read paths gated on a new config flag, and diff which
child-side syscall returns -14 for the failing spawn.

**2026-08-18: ruled out "just spawning real rustc with piped stdio,
repeatedly."** `ncaprobe bigspawn N` (`userspace/ncaprobe`) spawns the *exact*
heavy rustc invocation cargo uses for this crate's build script (piped
stdout/stderr, matching cargo's JSON-diagnostics capture) via plain
`std::process::Command`, in a loop. **50/50 succeeded** — well past the ~8
spawns where cargo itself starts failing, and each took the real ~4s a build
of this size actually costs, so it wasn't skipping work. That narrows it
further: it isn't the weight of the real rustc invocation, and it isn't
`std::process::Command`'s basic piped-stdio spawn path in isolation — cargo
itself must be doing something concurrent with the spawn that a sequential
probe doesn't: its jobserver (a pipe + fds set up and passed to children even
at `-j1`), or its own background threads (progress bar / signal handling)
running while the spawn happens. The "next steps" instrumentation above is
still the right next step, but now specifically aimed at *live cargo*, not a
synthetic mimic — a probe that only reproduces cargo's spawn shape without
cargo's actual concurrency has been shown not to trigger this.

**2026-08-18, continued:** this investigation forked into two — a real fd-table
bug in the same family was found and fixed
([`NCA_FD_NONBLOCK_TOCTOU.md`](NCA_FD_NONBLOCK_TOCTOU.md), confirmed by A/B),
but the `EFAULT` failure documented above survives that fix under a real
`cargo build -j4`. Full ruled-out/still-open detail moved to
[`NCA_CARGO_SPAWN_EFAULT.md`](NCA_CARGO_SPAWN_EFAULT.md) rather than growing
further here.

## 2. AF_UNIX `socket()` → EAFNOSUPPORT (nca IPC)

**Symptom:** `nca` runs (`--help`, `models`, config load fine) but any real
session dies with:

```
Error: Connection failed: Address family not supported by protocol (os error 97)
```

**Cause:** `nca-runtime`'s `IpcServer::start` always binds a Unix listener, and
attach/status/connect paths use `UnixStream`. Akuma implements
`socketpair(AF_UNIX)` (syscall 199, two pipes) but **`sys_socket` only accepts
domain 2 (AF_INET)** — `src/syscall/net.rs:115` returns EAFNOSUPPORT for
everything else, including domain 1.

**Workaround (in-tree, upstreamable):** `IpcServer::start` treats bind failure
as *IPC disabled* — returns a valid `IpcHandle` (channels work in-process;
`tracing::warn` announces the degradation). Only external `attach`/`status`/
`cancel` clients lose connectivity. `nca attach` etc. will not work on akuma
until the kernel fix lands.

**Proper fix (kernel, not done):** named AF_UNIX stream sockets with a
filesystem-backed rendezvous. Nontrivial: per-path listener table, `connect`
blocking model, VFS special-file entries.

## 2b. slow first byte on a socket → read hangs — FIXED 2026-08-17

Full write-up moved to **`SOCKET_DELAYED_FIRST_BYTE_HANG.md`** (also
DEVBOX_ISSUES Issue 17); summary retained here.

> **FIXED 2026-08-17.** Filed as "lost wakeup?" — it was not. Four separate
> kernel defects, the dominant one a socket still in `SynSent` being reported
> read-closed (`EPOLLIN`+`EPOLLRDHUP`, `recv() == Ok(0)`), which made nca park
> forever *before sending its request*. The gemma-vs-qwen split was request
> **size** shifting timing into that SYN window, not prefill being slow. Plus an
> undeclared 30 s blocking-read cap (which also killed mid-stream reads, so the
> "first byte" framing below is wrong), a dropped `SO_RCVTIMEO`, and an
> `EPOLLET` write edge that was never re-armed. Resolution, measurements and
> regression tests: `SOCKET_DELAYED_FIRST_BYTE_HANG.md` § Resolution; procedure
> and probes: `docs/runbooks/debug-delayed-first-byte.md`. The summary below is
> the original filing, kept verbatim.

**Symptom:** nca↔Ollama over SLIRP works end-to-end for **instant** responses
(qwen3.5:0.8b: `2+2 → 4`, `3+3 → 6`, full SSE stream, session completed) but
hangs forever when the server's first byte is **delayed** — gemma4:e4b with a
~2900-token system prompt has a multi-second silent prefill window; nca
connects, sends, then blocks forever with zero response bytes.

**Ruled out:**

- Network path guest→host: `wget` GET/POST to `10.0.2.2:11434` works always.
- Keep-alive reuse: a std client doing two sequential requests over **one**
  connection to :11434 gets both answers.
- Non-blocking writes: a std probe (`set_nonblocking(true)` + write + read)
  against a host listener delivered bytes fine.
- Ollama-side: the identical request through a host-side logging proxy
  (`10.0.2.2:18082` → 127.0.0.1:11434) **works from nca** — same guest, same
  binary, same model, full SSE streamed back. The proxy answers the models
  list instantly and pipes chunks as they come; the only difference vs direct
  is response timing.

**Suspicion:** when a socket has been idle (data awaited for N seconds) and
bytes then arrive, the wake of the blocked reader is lost — the reader sleeps
forever. All passing tests got a response within ~1s; every failing one had a
>5s silent window (cold 10 GB model load ≈ minutes; gemma prefill ≈ 10+s).
Alternatively SLIRP-side, but the proxy run also went through SLIRP with
sub-second chunks and worked, pointing at the timing/lost-wakeup shape rather
than the path.

**Repro:** guest `/tmp/nca-upstream -p "..."` with `model = "gemma4:e4b"`
against host `:11434`; works with `qwen3.5:0.8b`. Minimal kernel-side repro to
build next: std client that connects, sends, sleeps 10 s **server-side** before
responding, then reads — against a host `nc` with a delayed reply.

## 3. Getting a binary into the guest

- `scp`/SFTP: times out ("Timeout, server localhost not responding") — the
  SFTP subsystem chokes on Akuma's sshd. Don't fight it.
- **Working path:** `python3 -m http.server 18080` in the dir containing the
  binary on the host, then in the guest
  `wget -O /tmp/nca-new http://10.0.2.2:18080/<path>/nca`. 8.8 MB in ~seconds.
- `ssh ... 'cat > /tmp/file' < file` (stdin pipe) also stalled — likely the
  same flow-control issue. Use HTTP.

> **Correction 2026-08-17:** the stdin-pipe route **does** work; it just needs
> keepalives so the client does not give up during a quiet stretch. 10 913 904
> bytes landed in 25 s, sha256 identical to the host copy:
>
> ```python
> opts = ["-o","StrictHostKeyChecking=no","-o","ServerAliveInterval=15",
>         "-o","ServerAliveCountMax=40"]
> subprocess.run(["ssh",*opts,"-p","2222","root@localhost",
>                 "cat > /bin/nca.new && wc -c /bin/nca.new && sha256sum /bin/nca.new"],
>                input=open("bootstrap/bin/nca","rb").read(), timeout=900)
> ```
>
> `scp` remains broken (rc 255, "Timeout, server localhost not responding") —
> that is the SFTP subsystem, not the transport. Always stage to a temp name and
> compare `sha256sum` before `mv`-ing over a live binary: a stalled stream
> truncates and a truncated ELF looks exactly like a successful copy.
> Throughput is erratic (a 1 MB warm-up probe measured 72 KB/s, the 10.9 MB
> transfer averaged ~440 KB/s), so size the timeout generously rather than
> extrapolating from a small probe.

## 4. `userspace/nca` wrapper (in-guest self-build) — still blocked

1. **build.rs hardcodes `aarch64-linux-musl-gcc`** (`CC_aarch64_...`,
   linker). The Alpine guest has `aarch64-alpine-linux-musl-gcc` and plain
   `cc` — host==target musl, so plain `cc` is correct. Fix: try `cc` when
   `aarch64-linux-musl-gcc` is absent.
2. **Kernel `.cargo/config.toml` bleeds in**: building under the akuma tree
   makes cargo walk up to the repo-root config and add
   `--target aarch64-unknown-none -C link-arg=-Tlinker.ld` to the *inner*
   nca build (seen in the shim log). The wrapper scrubs
   `CARGO_ENCODED_RUSTFLAGS` but not target/config discovery. Building
   outside the tree (e.g. `/tmp/native-cli-ai`) avoids it.
3. Everything else is gated on §1 (the spawn EFAULT) anyway.

## 4b. Upstream cross-build fixes (in-tree patches)

- `mcpr = "0.2.3"` in `crates/core/Cargo.toml`: declared but **never imported
  by any .rs file**; pulls reqwest default-features (native-tls → openssl-sys)
  and breaks musl-static cross. Patched to `optional = true`.
- `crates/core/Cargo.toml` reqwest line lacked `default-features = false`,
  re-enabling native-tls despite the rustls-only workspace declaration.
  Patched to `default-features = false, features = ["json", "stream",
  "rustls-tls"]` — the same one-liner the old fork carried (`da4d0f7`).
- Note: `arboard = "3"` (tui) compiles fine on musl-static — x11rb is pure
  Rust; the wayland path dlopens at runtime, so clipboard is dead code on
  akuma but harmless. No gating needed for the build.

## 5. Working host-side cross-build (the currently-used path)

Exact parameters mirror `userspace/nca/build.rs`:

```bash
cd userspace/nca/native-cli-ai
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc  # brew musl-cross
export CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc
export AR_aarch64_unknown_linux_musl=aarch64-linux-musl-ar
export RUSTFLAGS="-C opt-level=3 -C lto=fat -C codegen-units=1 -C panic=abort \
  -C overflow-checks=off -C target-feature=+neon,+fp16,+dotprod -C link-arg=-static"
cargo build --release --no-default-features --target aarch64-unknown-linux-musl -p nca-cli
```

Result: 10.9 MB static stripped ELF; runs on Akuma. Verified twice against
host Ollama (§6): `2+2 → 4` and `3+3 → 6` with qwen3.5:0.8b, full session
lifecycle (`Session ended (Completed)`).

## 6. Ollama wiring (guest → host model server)

Host Ollama listens on :11434; the guest reaches it at `10.0.2.2:11434`
(SLIRP). nca has no native Ollama provider — use the Custom provider with
OpenAI compatibility:

```toml
# ~/.nca/config.toml  (in the guest)
[provider]
default = "custom"

[provider.custom]
base_url = "http://10.0.2.2:11434"   # NO trailing /v1 — nca appends /v1/... itself;
                                     # a /v1 suffix produces /v1/v1/... → "404 page not found"
model = "gemma4:e4b"                  # or qwen3.5:0.8b etc.
compatibility = "openai"
api_key = "ollama"                    # non-empty placeholder; Ollama ignores it but nca requires one
```

Without `api_key` nca exits `provider configuration error: missing Custom
provider API key` even though Ollama needs no auth. `nca models` should show
`Custom [selected] -> <model> (http://10.0.2.2:11434)`.

IPv4 TCP to 10.0.2.2 works from the guest; IPv6 sockets EAFNOSUPPORT on
Akuma — keep URLs literal IPv4 (never `localhost`). Caveat: pick a model with
fast prefill until §2b is fixed; slow first byte hangs the session.

## 6b. GLM-4.7 over the public internet (Z.ai) — verified 2026-08-17

Unlike §6 (host Ollama over SLIRP) this leaves the box entirely: real DNS, real
TLS, real HTTPS from the guest. Z.ai speaks Anthropic's wire format, so the
Custom provider with `compatibility = "anthropic"` drives it — the adapter
appends `/v1/messages` (`crates/core/src/provider/custom.rs:91`), giving
`https://api.z.ai/api/anthropic/v1/messages`.

**Config path moved.** The `~/.nca/config.toml` in §6 is now only a legacy read
fallback. Upstream resolves `$NCA_HOME` → `$XDG_DATA_HOME/ncacli` →
`$HOME/.local/share/ncacli` (`nca_product_home`, `crates/common/src/config.rs`).
The guest runs with `HOME=/`, so the live file is
**`/.local/share/ncacli/config.toml`**.

```toml
[provider]
default = "custom"

[provider.custom]
base_url = "https://api.z.ai/api/anthropic"   # no /v1 suffix — adapter appends it
model = "glm-4.7"
compatibility = "anthropic"
temperature = 1.0
api_key = "<z.ai key>"
api_key_env = "ZAI_API_KEY"                    # consulted only if api_key is absent

[model]
default_model = "glm-4.7"

[ui]
onboarding_completed = true
```

Verified from the guest: `SessionStarted … "model":"glm-4.7"`, then a genuine
JSON `401 token expired or incorrect` from Z.ai on a placeholder key — which is
the useful negative result, since reaching a provider-issued 401 proves DNS,
the rustls handshake (§4b) and the endpoint URL are all correct.

**Two gotchas:**

- **`[ui] onboarding_completed = true` alone does not skip the first-run modal.**
  `needs_onboarding()` is `!onboarding_completed || !any_api_key_present()`, so
  without a resolvable key the second half keeps it true and the connect TUI
  still opens. There is no config-level `--no-tui`; the flag is CLI-only.
- **Context window is mis-detected as 32 000 tokens.** `model_limits.rs` has no
  GLM entry (zero hits for `glm` in `crates/runtime/`), so detection falls
  through to the 32 000 default; GLM-4.7 is really **204 800**. At the default
  75 % `auto_summarize_threshold` nca starts compacting near ~24 k tokens and
  discards context the model could have held. `context_window_target` is read
  **only when auto-detect is off** (`supervisor.rs:412-420`), so both lines are
  required:

  ```toml
  [memory.context]
  auto_detect_context_window = false
  context_window_target = 204800
  ```

  The alternative — a GLM row in `crates/runtime/src/model_limits.rs` — is an
  upstream change and belongs in `upstream-akuma-patches.patch`.

## 7. Exit-time `tokio-rt-worker` panic — NOT a syscall gap (2026-08-17)

**Symptom:** the session runs fine start to finish; the panic lands only as the
process exits.

```
2026-08-17T13:56:18 INFO nca: nca starting
[session] Resuming last session session-1786974897551249-0
2026-08-17T13:56:18 WARN nca_runtime::ipc: IPC disabled: socket bind failed: ... (os error 97)
2026-08-17T13:56:19 INFO nca_runtime::supervisor: Context window target for glm-4.7: 32000 tokens

thread 'tokio-rt-worker' (17) panicked at tokio-1.50.0/src/fs/file.rs:745:51:
called `Option::unwrap()` on a `None` value
Aborted
```

**This is an upstream tokio bug, not a missing Akuma syscall.** No kernel work
is implied. Mechanism, all inside tokio's `impl AsyncWrite for File::poll_write`
(`tokio-1.50.0/src/fs/file.rs`):

| line | what happens |
| --- | --- |
| 744 | `State::Idle(ref mut buf_cell) =>` |
| 745 | `let mut buf = buf_cell.take().unwrap();` — buffer moved **out** of the cell |
| 756 | `spawn_mandatory_blocking(…)` returns `None` once the runtime is shutting down |
| 765-767 | `.ok_or_else(…)?` — the `?` **returns early** |
| 769 | `inner.state = State::Busy(handle);` — *never reached* |

The early return leaves the file in `State::Idle(None)`: the buffer was taken at
745 and never put back. The write is *poisoned*, not merely failed. The **next**
`poll_write` re-enters 745, `take()` yields `None`, and `unwrap()` aborts.

**Why it fires every time here rather than intermittently.** The event-log
writer does two writes per event and discards both results
(`crates/runtime/src/supervisor.rs:964`, twin at `crates/cli/src/stream.rs:193`,
TUI twin at `crates/tui/src/tui/bridge.rs:40`):

```rust
let _ = file.write_all(line.as_bytes()).await;   // poisons the cell, Err swallowed by `let _ =`
let _ = file.write_all(b"\n").await;             // hits Idle(None) -> unwrap panic
```

The first call is exactly the one that returns the swallowed
`"background task failed"` error and strands the state; the trailing newline
write is the one that panics. The two-call-per-event shape converts a silent
error into an abort within a single event.

**Why Akuma surfaces it and a dev laptop mostly does not.** The writer is a
detached `tokio::spawn` task appending JSONL to the event log, racing runtime
teardown at exit. Guest ext2 writes are slow (same cost centre as
`RUSTC_COMPILE_EXT2_MMAP`), so a write is far likelier to still be in flight
when the runtime drops. Nothing about it is Akuma-specific beyond timing.

`Aborted` rather than a normal panic exit is `-C panic=abort` from the §5
RUSTFLAGS — the abort is the panic, not a second fault.

**Impact:** cosmetic for the session (all model traffic has completed by then),
but the **tail of the event-log JSONL can be truncated** — the last one or two
records may be missing or partial. Session state itself is safe: `session_store`
uses one-shot `tokio::fs::write` on the blocking pool
(`crates/runtime/src/session_store.rs:29`), not `File`/`poll_write`, so it is
not on this path.

**The panic is the loud form; silent truncation is the common one.** Observed
on the same box minutes later, in a run that did *not* panic: a one-shot
`-p "reply with exactly: pong"` streamed four ndjson events, stopped dead after
`Checkpoint … "Starting model turn 1"`, and exited 0 with no assistant output on
stdout — while the session JSON had persisted `{"role":"assistant","content":
"pong"}` and billed 1 output token. The `.events.jsonl` was 629 bytes holding
only those four events. Same shutdown window, same lost tail; whether it aborts
or merely truncates depends on how far the second `write_all` got. **A run that
appears to produce no answer is therefore not evidence the provider failed** —
check the session JSON before chasing it as a network or model problem.

**Fix, if it becomes worth doing** (all userspace, all upstreamable):

1. Stop discarding the results — `let _ =` on `write_all` is what hides the
   first error and lets execution reach the panicking second call.
2. Write `line + "\n"` in a **single** `write_all`, so a poisoned cell surfaces
   as an error and never gets a second poll.
3. Flush/await the event-log task before dropping the runtime, so no write is
   in flight at shutdown.

Upstream-tokio-side the real defect is that the `?` at 767 should restore the
buffer to `buf_cell` before returning.

## Background

- Companion syscall-gap doc: `docs/archive/APK_MISSING_SYSCALLS.md` (same
  pattern: static-PIE + syscall gaps for a static Alpine binary).
- Fork wrapper: `userspace/nca/build.rs`, `userspace/nca/README.md`.
- The spawn-EFAULT debug session also produced the console-capture trick:
  relaunch QEMU with `-serial mon:stdio > logfile` (the repo's runner already
  does this) and grep `[syscall] execve` lines — absence of the line for a
  spawn cargo reports as failed is the key clue.
