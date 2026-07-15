# Debug SSH latency / echo / terminal

Symptom-driven debugging for SSH slowness, echo lag, staggering, terminal
sizing, and command chaining.

> **Stability: A (stable).** SSH has many docs but ≤4 commits each; the echo
> path is now sub-ms after the waker + poll-on-progress fixes. The recurring
> lesson: `block_on` must use `yield_now()` (not `schedule_blocking()`), and
> must re-poll immediately when `smoltcp_net::poll()` reports progress.

For the SSH architecture (built-in vs userspace sshd), see
[`../reference/subsystems/ssh.md`](../reference/subsystems/ssh.md). For
network-level connection issues, see [`debug-network.md`](debug-network.md).

## Symptom → cause → fix

| Symptom | Cause | Status | Fix |
|---|---|---|---|
| Keystroke echo delayed; typing again makes both appear ("stagger"). `[SSH-ECHO] read gap=800ms–1.8s` | Network-thread boost was hardcoded to **slot 0** (idle); real poller competed round-robin → ~80 ms between poller slots | FIXED | `NETWORK_THREAD_ID: AtomicUsize` registered by `run_async_main`; boost targets the registered thread |
| 800 ms–1.4 s spikes after the above | `block_on`'s no-op waker meant smoltcp's `register_recv_waker` did nothing; SSH thread only noticed data on its next slot (~100 ms) | FIXED | Real `current_thread_waker()`; `block_on` re-polls immediately when `poll()` returns true |
| `block_on` deadlock on single-core | `schedule_blocking()` flips to WAITING; wake fires an SGI; if SGI fires while network thread holds `NETWORK` spinlock → SSH thread re-acquires → deadlock | FIXED (design constraint) | `block_on` **must** use `yield_now()` (stays READY, skips SGI). Regression guard: `test_block_on_uses_yield_now` |
| Input lag in interactive sessions | `SshChannelStream::write` didn't auto-flush; `execute_external_interactive` had artificial `for _ in 0..20 { yield_now() }` throttling | FIXED | Auto-flush in `write` (10 ms timeout); remove the 20× yield loop |
| Stagger got *worse* after write-path fix | `flush()` waited for `send_queue()==0` (remote ACK) with a cooperative 10 ms timeout that stretched to 36 ms, blocking the next read | FIXED | (1) `block_on` only yields when poll reports idle; (2) remove redundant yield from flush; (3) writes ≤128 B (keystrokes) skip ACK-flush, just `poll()` once. Echo path <1 ms. |
| `ssh host "cmd"` output arrives ~1 s late in one burst | Only thread 0 transmits; `block_on` used no-op waker; TCP writes just buffer | PARTIALLY FIXED | Mitigated by real waker + poll-on-progress. True real-time streaming would need sessions on the network-thread executor. |
| Delete key inserts `~` (Mac fn+backspace `\x1b[3~`) | `EscapeState::Bracket` reset to Normal on `b'3'` with empty arm; trailing `~` printed literal | FIXED | `BracketNum(u8)` variant holds the digit, waits for `~`; handles Delete/Home/End |
| Full-screen apps (vi, less) use 80×24 regardless of real terminal; `ioctl(TIOCGWINSZ)` returns defaults | 3 gaps: kernel `TerminalState::default()` created after `pty-req` without copying dims; `bridge_process` dropped `window-change`; no `TIOCSWINSZ` (`0x5414`) handler | FIXED | Copy dims on pty-req; handle window-change; kernel `src/syscall/term.rs` implements `TIOCSWINSZ`; `userspace/libakuma` `set_terminal_size` |
| Command chaining: `nonexistent; echo x` prints only the error; `ls && pwd` → "pwd not found" | (a) Exec breaks chain on any failure, ignoring `;` vs `&&`; (b) missing `pwd`/`cd` builtins; (c) chaining only in SSH exec mode, not interactive/console | OPEN | Track `prev_operator`; only break on `&&` failure; add `pwd`/`cd`; call `parse_command_chain()` in interactive + console |
| SSH exit code always 255 | Separate known issue | OPEN | — |

## Measuring SSH latency

From the host:
```bash
ssh -o StrictHostKeyChecking=no -p 2222 root@localhost "hello" | \
  while read line; do echo "$(date +%S.%3N): $line"; done
```
Identical timestamps on all lines ⇒ batched transmission (the streaming
limitation above).

Built-in instrumentation (build-gated): `[SSH-TX-DROP]` (VirtIO TX failures),
`[SSH-ECHO]` (gap between reads), `[SSH-ECHO-SLOW]` (echo write >5 ms),
`[SSH-FLUSH-TIMEOUT]` (auto-flush >10 ms).

## SSH debug knobs & counters

Live counters in `src/ssh/server.rs:91` `stats()`: `opened` vs `closed`
divergence = handle/session leak; `handshake_fail`/`auth_fail`; `panicked`;
`last_step` (pinned at `PRE_WITH_NETWORK`(2) ⇒ NETWORK contention; at
`POLL`(6) ⇒ poll stuck); `listener_valid`. The heartbeat in `src/main.rs`
prints these.

Timeouts (kernel SSH, `src/ssh/protocol.rs:40-43`): handshake 30 s, idle 300 s,
read 60 s, interactive read 10 ms. **Do not raise the 10 ms interactive poll.**

## Built-in SSH vs userspace sshd — which is failing?

| | Built-in (kernel, smoltcp) | Userspace sshd (devbox) |
|---|---|---|
| Failure modes | (a) `NETWORK` spinlock contention deadlocking `block_on` if SGI fires mid-`iface.poll()`; (b) socket-pool exhaustion wedging accept loop; (c) async state machine pointer corruption | (a) `sleep_ms` inside a session future **monopolizes the whole thread** (use `yield_now()` helper); (b) `noop_waker` fine for config load, fatal in a session; (c) no in-kernel wakers — relies on `WouldBlock`→Pending polling |
| Port | 22 → host 2222 | `--port` > config > default 2222 |

## Background

- `archive/SSH_STAGGERING.md`, `archive/SSH_ECHO_LATENCY_FIX.md` — the waker/poll fixes.
- `archive/SSH_PERFORMANCE_FIX_2026.md`, `archive/SSH_STREAMING_ARCHITECTURE.md`.
- `archive/SSH_TERMINAL_KEY_TRANSLATION_FIX.md`, `archive/SSH_TERMINAL_SIZE_FIX.md`.
- `archive/COMMAND_CHAINING_SSH_BUGS.md` (open chaining issues).
