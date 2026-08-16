# NCA (native-cli-ai) — Missing Kernel Pieces for a Rust AI CLI

**Date:** 2026-08-17. **Scope:** getting `nca` (upstream
`madebyaris/native-cli-ai`) to build and run on Akuma (devbox-smoltcp, nightly
rustc 1.99.0 in the guest).
**Status:** **submodule swapped to upstream 2026-08-17**; host-built upstream
binary **verified against host Ollama from the guest** (two full round trips).
In-guest build still blocked by the spawn EFAULT (§1); slow-model hang is a
new open kernel suspicion (§2b). Patches needed on top of upstream HEAD are in
`userspace/nca/upstream-akuma-patches.patch`.

## TL;DR

| Problem | Status |
| --- | --- |
| `cargo build` in guest: `could not execute process rustc ... Bad address (os error 14)` | **Open** — not a kernel syscall EFAULT; see §1 |
| `socket(AF_UNIX)` unimplemented → nca IPC dies with EAFNOSUPPORT | **Patched in-tree** (bind failure degrades to IPC-less mode) |
| Upstream HEAD won't cross-build: `openssl-sys` via `mcpr` + core's own `reqwest` | **Patched in-tree** (two one-liners, §4b) |
| `scp`/SFTP into the guest times out | Use HTTP: host `python3 -m http.server`, guest `wget http://10.0.2.2:<port>/...` (§3) |
| nca↔Ollama verified | qwen3.5:0.8b instant round trips OK; gemma4:e4b (slow prefill) hangs — kernel suspicion §2b |

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

## 2b. NEW OPEN: slow first byte on a socket → read hangs (lost wakeup?)

Full write-up moved to **`SOCKET_DELAYED_FIRST_BYTE_HANG.md`** (also
DEVBOX_ISSUES Issue 17); summary retained here.

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

## Background

- Companion syscall-gap doc: `docs/archive/APK_MISSING_SYSCALLS.md` (same
  pattern: static-PIE + syscall gaps for a static Alpine binary).
- Fork wrapper: `userspace/nca/build.rs`, `userspace/nca/README.md`.
- The spawn-EFAULT debug session also produced the console-capture trick:
  relaunch QEMU with `-serial mon:stdio > logfile` (the repo's runner already
  does this) and grep `[syscall] execve` lines — absence of the line for a
  spawn cargo reports as failed is the key clue.
