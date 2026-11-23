# codex-code-mode-host: sessions lost to HTTP/2 keep-alive on the main kernel (2026-11-23)

Status: **open**. One small fix landed on the way (`getrandom`, below); the cause of the lost sessions is not found.

## Symptom

On the ryzen box, codex under herdr with the long-lived `codex-code-mode-host` herd service
(`CODE_MODE_HOST=http://127.0.0.1:47200`) lost its connection to the host several times in one day and
never recovered; `herd stop`/`herd start` of the service brought it back.

## What the logs say

1. **The `unexpected argument 'grpc://127.0.0.1:47200'` flood is not this bug.** All 62 lines in
   `/var/log/herd/codex-code-mode-host.log` date from 2026-11-17, before the `codex-code-mode-host-serve`
   wrapper existed (herd's `args = --listen ...` reaching the host without `--listen`; still unreproduced,
   `userspace/herd/README.md`). Since the wrapper the host logs one `listening on :47200` per boot and never
   restarts. The host process stayed alive; it was the *connection* that died.
2. **The ecosystem demo logs show the same failure on the main kernel and none on Denver.**
   `/var/tmp/akuma-eco/agent-runs/` (codex agents, `code_mode_host = true`, codex owns its host over stdio):

   | run | kernel | `gRPC code-mode session closed` |
   |---|---|---|
   | `20261122-180606` rust agent | main (`akuma`) | 8, between 16:10 and 16:19 |
   | `20261122-180606` scala, python | main | 0 |
   | `20261122-193921`, `20261122-235944`, all agents | Denver | 0 |

   The rust agent's stderr has, at 16:17:39: `gRPC code-mode execution failed: code: 'The service is currently
   unavailable', message: "code-mode host transport failed", source: tonic::transport::Error(Transport,
   hyper::Error(Http2, KeepAliveTimedOut))`. Codex's HTTP/2 client sent a PING and the pong did not arrive in time.
   Each "session closed" is followed 4-10 s later by `The exec runtime was replaced. Stored values and running cells
   were lost`: with its own host codex respawns one and continues. **With `CODE_MODE_HOST` pointing at an external
   service there is nothing to respawn**, which is why the ryzen setup looks unrecoverable (inferred, not tested).
3. The demo's real-model transcripts are mostly plain `exec_command` (`/bin/sh -lc ...`), which does not use the host.
   Only cells that go through code mode (`exec` / `tools.exec_command`) exercise it.

## Local reproduction (amd64 headless kernel under KVM, `scripts/codex/fakemodel.py`)

| setup | result |
|---|---|
| long-lived host on :47200 | the cell yields `Script running ... Wall time 43-49 s`, empty output |
| `code_mode_host = true`, no `CODE_MODE_HOST` (codex spawns a stdio host) | no better: hung until a 150 s timeout, host processes left behind |
| trivial cell `text("hello-from-cell")`, no nested tool call, stdio host | completes, but **25 s** wall; whole `codex exec` 104 s |
| `code_mode_host = false`, fake model emitting a plain `exec_command` call | command ran, exit 0, 3 s |

The host itself is healthy: a non-blocking loopback connect plus an h2 client preface gets a SETTINGS reply in
~2 ms (`/root/probe`, a 40-line C program). Per-syscall costs in the demo's host stats do not blame `mmap`/`munmap`
(about 0.5 s total). Cold start of the 294 MB codex binary alone takes over 20 s the first time.

## Fixed on the way: `getrandom(buf, 0)` returned EFAULT

`strace_err` showed `nr=318 a1=0x1 a2=0x0 -> -14` in the host's runtime process. Rust passes a dangling pointer
(address 1) for an empty slice; `sys_getrandom` ran `validate_user_ptr` first, which rejects anything below 0x1000.
Linux returns 0 for an empty request without looking at `buf`. `sys_getrandom` now returns 0 for `len == 0`
(commit `f5e0071e`). It did **not** cure the stalled cell. Other syscalls that validate before checking `len == 0`
may have the same flaw; not audited.

## Hypotheses, none confirmed

- The pong (or codex's read of it) is late because of a missed epoll wakeup on a loopback socket; hyper/mio use
  `EPOLLET`, and `docs/reference/subsystems/syscalls/poll.md` records several edge-bookkeeping fixes.
- The host is simply unresponsive for longer than the keep-alive timeout while something in code-mode startup or
  cell execution is slow (the 25 s trivial cell).

## Next

1. Measure h2 PING round-trip against a busy host on the main kernel; compare Denver.
2. Find what the 25 s of a trivial code-mode cell is spent on (kernel `strace_nr` on `futex`, `epoll_pwait`, clone/exec).
3. Whether codex's keep-alive interval/timeout can be configured.

**Workaround on ryzen:** `[features] code_mode_host = false` in `~/.codex/config.toml`, drop the `CODE_MODE_HOST`
export, `herd disable codex-code-mode-host`. Codex then uses its plain command tool (tested with a fake model only).

## Background

- `docs/runbooks/debug-codex-on-akuma.md`, `docs/archive/DENVER_CODEX_TOOLS.md`
- `userspace/herd/README.md` (the `args` first-flag report)
