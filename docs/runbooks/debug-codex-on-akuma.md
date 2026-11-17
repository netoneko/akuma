# Run codex on Akuma, and trace why its shell tool fails (amd64, ryzen)

**Stability: C — the herd service and the fake-model repro are exercised; the guest-side trace (step 5) is not finished.**
Files: `overlays/ryzen-akuma-dev/{codex-code-mode-host.conf,codex-code-mode-host-serve,vm-codex-trace.conf}`,
`scripts/codex/fakemodel.py`. Related: [`kvm-dev-loop-on-akuma.md`](kvm-dev-loop-on-akuma.md),
[`trace-failing-syscalls-amd64.md`](trace-failing-syscalls-amd64.md).

Symptom: `codex` (0.169.0, static musl) starts and talks to its model, but every shell command fails — in the TUI and in
`codex exec` — with `KeepAliveTimedOut` / `exec runtime was replaced`, or, once that is fixed, `exited 143 in 0ms` with
no output. You need to know which layer is broken and to see the syscalls under it.

## 1. Know the layers

codex runs each shell command as `tools.exec_command(...)` inside a **code-mode host**, a separate process
(`/usr/local/bin/codex-code-mode-host`) it talks to over gRPC/HTTP-2. By default it spawns the host itself over
`grpc+stdio://` (pipes). Three separate failures, in the order you meet them:

| layer | symptom | state |
|---|---|---|
| host spawned over stdio | host dies after first use; `gRPC code-mode session closed`, then `KeepAliveTimedOut` after ~36 s | worked around (step 2) |
| sandbox (`bwrap`) | `Can't read /proc/sys/kernel/overflowuid`; no `/proc/self/ns` | worked around (step 3); not fixed |
| the command's child | `exited 143 in 0ms`, even `true`: the child gets SIGTERM before it runs (`[signal] pid=N killed by signal 15` in `dmesg`) | **open** — step 4 reproduces it, step 5 traces it |

Ruled out (each probed on this box): HTTP/2 and V8 in the host (a standalone TCP host answers gRPC in 0.4 ms), plain
pipes (a Python client gets SETTINGS/PING acks), `epoll` on a pty master, plain/`setsid`/process-group/pty/pdeathsig
spawns of `/bin/sh -lc`. The kernel accepts `PR_SET_PDEATHSIG` as a no-op (`crates/akuma-syscalls-glue/src/proc.rs`).

## 2. Run the host as a herd service and point codex at it

```sh
cp overlays/ryzen-akuma-dev/codex-code-mode-host-serve /usr/local/bin/ && chmod 755 /usr/local/bin/codex-code-mode-host-serve
cp overlays/ryzen-akuma-dev/codex-code-mode-host.conf /etc/herd/available/
herd enable codex-code-mode-host && herd start codex-code-mode-host
```

- The service runs a **wrapper script** because herd's `args = --listen grpc://…` loses its first flag at spawn
  (`userspace/herd/README.md` Roadmap). Do not "simplify" it back to `command = …host`.
- **`herd enable` copies** the conf into `enabled/`. After editing `available/`, `herd disable` then `herd enable`, or herd keeps
  restarting the old copy. `herd status` keeps showing `restart-pending … last_exit=2` after a good start; trust `ps`.
- Point codex at it: `export CODE_MODE_HOST=http://127.0.0.1:47200` (`/etc/profile.d/20-codex-code-mode-host.sh`; there is no
  config-file key for the URL) and, in `~/.codex/config.toml`, `[features] code_mode_host = true`. Existing shells need the export by hand.

## 3. Turn codex's sandbox off (only while bwrap cannot run)

`~/.codex/config.toml`: `sandbox_mode = "danger-full-access"` (top level, before any `[table]`). That makes codex run
commands unsandboxed on this box; revert it when `bwrap` works. `codex sandbox linux -- /bin/ls` is the one-line probe.

## 4. Reproduce without a model or network: `scripts/codex/fakemodel.py`

A 40-line Responses-API server that answers codex's first request with one canned `exec` tool call and its second with
"done", logging what codex sent back.

```sh
python3 scripts/codex/fakemodel.py 47300 /tmp/fake.log 'ls /tmp | head -3' &
CODEX_HOME=/tmp/chome   # config.toml: model="gpt-6.1-terra", model_provider="fake", sandbox_mode="danger-full-access",
                        # [features] code_mode_host=true, [model_providers.fake] base_url="http://127.0.0.1:47300"
                        # wire_api="responses", requires_openai_auth=false, supports_websockets=false
cp ~/.codex/models_cache.json ~/.codex/version.json $CODEX_HOME/   # a model codex does not know has no `exec` tool
CODE_MODE_HOST=http://127.0.0.1:47200 codex exec --skip-git-repo-check -C /tmp go
grep TOOL_OUTPUT /tmp/fake.log      # ... "exit_code":143 ...
```

`CODEX_HOME` keeps your real `~/.codex` (auth, history) out of it. Every command, including `true`, exits 143 — so the child is
killed at spawn, not by anything the command does.

## 5. Trace the syscalls in a throwaway KVM guest

The kernel's own trace is a **command-line flag**, so a metal box needs a reboot; a guest does not. `vm-codex-trace.conf` boots
the running kernel as a guest of `/dev/kvm0` on the RAM-copied `vm-dev` root (nothing persists), with
`strace_err strace_nr=62,200,234` (every failing call, plus every `kill`/`tkill`/`tgkill` with its target and signal).

```sh
cp overlays/ryzen-akuma-dev/vm-codex-trace.conf /etc/herd/available/ && herd enable vm-codex-trace && herd start vm-codex-trace
herd log vm-codex-trace | tail          # the trace; `nr=61 … -> -10` spam is sshd's wait4, filter it out
/bin/ssh -p 2224 root@localhost ...     # Akuma's ssh takes no -o; accept the guest's host key by hand (it is new every boot)
```

Then copy `codex`, `codex-code-mode-host`, `rg`, the `CODEX_HOME` above and `fakemodel.py` into the guest, and run step 4 **inside** it
(the guest has no route to the model API — `vm_outbound` is only a relay to the host's LAN IP — which is why step 4's fake
server exists; start it in the guest, or on the host and dial `192.168.1.159`). Find the `nr=62/200/234` line whose `a2=0xf` (15):
its `pid=` is the sender, its `a1` the victim. One kvm0 guest at a time; `herd disable vm-codex-trace` when done.

Not yet done: the copy-in and the run. The first attempt stopped at the guest's ssh host-key prompt.

## Verify

- `ps` shows `/usr/local/bin/codex-code-mode-host --listen grpc://127.0.0.1:47200`, and
  `curl --http2-prior-knowledge -H 'content-type: application/grpc' http://127.0.0.1:47200/codex.code_mode.v1.CodeModeHost/GetProcessMetrics` prints `200`.
- With `CODE_MODE_HOST` set, `codex exec …` spawns **no** `grpc+stdio://` host (`ps | grep code-mode`).
- Step 4 prints `exit_code":143` until the kernel bug is fixed, then the command's output.

## Traps

| symptom | cause |
|---|---|
| host service exits 2, `unexpected argument 'grpc://…'` | `args =` conf instead of the wrapper (herd loses the first flag), or a stale `enabled/` copy |
| `unsupported custom tool call: exec` | the model name is not in `models_cache.json`; codex gives unknown models no code-mode tool |
| `code_mode_host is ignored` warning | the URL is not a config key; use `CODE_MODE_HOST` |
| `dmesg` shows `[signal] pid=N killed by signal 15` but no sender | the default log names the victim only; that is what step 5 adds |
| `herd stop` of the host service during a trace | codex then falls back to spawning its own stdio host and fails differently |

## Background

Findings and dead ends are in the session notes; the herd bugs are in `userspace/herd/README.md` (Roadmap).
`docs/archive/CLOCK_GETTIME_EINTR_ON_SIGNAL.md` is the earlier codex-on-Akuma incident (a TUI panic from `EINTR`).
