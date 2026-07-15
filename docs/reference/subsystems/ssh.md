# SSH

Current-state architecture for both SSH servers: the built-in in-kernel sshd
(smoltcp) and the userspace sshd (devbox).

> **Stability: A (stable).** Low per-doc churn; the echo path is sub-ms after
> the waker/poll fixes. Open items are minor: command chaining, exit code 255,
> true real-time streaming. The load-bearing invariant: `block_on` uses
> `yield_now()` (not `schedule_blocking()`) and re-polls on progress.

For debugging, see [`../../runbooks/debug-ssh-latency.md`](../../runbooks/debug-ssh-latency.md).

## Algorithm suite

KEX `curve25519-sha256`; host key `ssh-ed25519`; encryption `aes128-ctr`; MAC
`hmac-sha2-256`; compression `none`; auth **publickey (Ed25519 only — RSA
rejected)**. Up to 4 concurrent sessions (`MAX_CONNECTIONS`).

## Built-in in-kernel SSH server

`src/ssh/`:
- `server.rs` — accept loop on a system thread; counter bookkeeping; `block_on`;
  `SessionGuard` RAII.
- `protocol.rs` — kernel-coupled orchestration: `handle_connection`,
  `SshChannelStream`, `run_shell_session`, `bridge_process`, timeouts.
- `crypto.rs`, `keys.rs`, `config.rs`, `auth.rs`.
- Protocol *logic* (state machine, kex, packet processing) lives in the
  **`akuma_ssh` crate**; `protocol.rs` is the kernel integration layer.

**Session lifecycle:** `AwaitingVersion → AwaitingKexInit → AwaitingKexEcdhInit
→ AwaitingNewKeys → AwaitingServiceRequest → AwaitingUserAuth → Authenticated →
ShellSession → Disconnected`.

### Accept + threading model (`src/ssh/server.rs`)

1. `run()` creates a listening socket; loops: `with_network` to check for
   `Established`, hand the socket to a fresh system thread running
   `run_session`, **recreate** the listener (`recreate_listener_with_retry` —
   retries forever on pool exhaustion, driving `poll()` to advance GC).
2. `run_session` wraps work in a `SessionGuard` so on normal return **or**
   panic-unwind, `socket_close` runs, `ACTIVE_SESSIONS` decrements. (Guard is
   block-scoped because the fn is `-> !`.)
3. `block_on` drives the async session future with `current_thread_waker()`
   (real waker); on `Pending` it calls `smoltcp_net::poll()` and **only**
   `yield_now()`s when poll reports no progress. **Must use `yield_now()` not
   `schedule_blocking()`** (SGI-during-poll deadlock — see debug runbook).

### Async exec / streaming

- `exec_streaming()` spawns the ELF via `spawn_process_with_channel` and loops:
  drain `ProcessChannel` → `output.write_all().await` → `flush().await` → yield.
- `SshChannelStream` (`src/ssh/protocol.rs:113`) is the `embedded_io_async`
  bridge: `write` does `send_channel_data` + auto-flush (writes ≤128 B skip
  ACK-flush, just `poll()` once); implements `InteractiveRead` (10 ms
  `try_read_interactive`).
- `execute_external_interactive` is a poll loop enabling apps like `meow` to
  read stdin while streaming output.
- Gated by `config::ENABLE_SSH_ASYNC_EXEC = true`; buffered fallback only for
  builtins/unresolvable commands.

## Userspace sshd (`userspace/sshd`)

- Entry `main()` (`userspace/sshd/src/main.rs`): load config + host key, parse
  `--shell`/`--shell-arg`/`--port` (precedence CLI > config > 2222), bind
  `0.0.0.0:port`, listener non-blocking.
- **Cooperative multiplexer:** a `Vec<Pin<Box<dyn Future>>>` holds one future
  per accepted session. Each tick: `listener.try_accept()`, then poll every
  session future once in turn; `Poll::Pending` stays, `Ready` swap-removed. If
  no progress, `sleep_ms(1)`. This lets a second connection progress while the
  first is idle.
- `SshStream` wraps `libakuma::net::TcpStream`: maps `WouldBlock` →
  `Poll::Pending` (re-arming via `cx.waker().wake_by_ref()`).
- **`yield_now()` helper** — **must** be used instead of `sleep_ms` inside
  session futures (`sleep_ms` parks the entire OS thread, starving all other
  sessions).
- Used by the **devbox** image.

## Auth model

- **Publickey only**, Ed25519 only; `password`/`none` rejected.
- Flow: client `none` → server `FAILURE(methods=publickey)` → client
  `publickey(no sig)` → server checks `authorized_keys` → `PK_OK` → client
  `publickey(sig)` → server verifies against `session_id` → `SUCCESS`.
- `disable_key_verification` in `SshdConfig` returns `Success` unconditionally
  (**test/local-dev only**, e.g. the devbox).
- **Host key:** load 32-byte private key from `/etc/sshd/host_key`; if
  missing/wrong-length, generate via hardware RNG and best-effort persist.
  Public key auto-added to `/etc/sshd/authorized_keys`.

## Terminal handling

- **Rich terminal syscalls 307–313:** `set/get_terminal_attributes` (raw/cooked),
  cursor, clear_screen, `poll_input_event` (blocking/non-blocking/timed). Raw
  mode forwards SSH channel bytes unmodified; cooked mode does line editing +
  echo in the shell.
- **PTY / winsize:** `TerminalState` is `Arc<Spinlock<…>>` (`term_width`,
  `term_height`, `input_waker`). `TIOCGWINSZ` (`0x5413`) reads; `TIOCSWINSZ`
  (`0x5414`) writes (kernel `src/syscall/term.rs`). **pty spawn**
  (`SPAWN_FLAG_PTY`): child gets a **fresh** Arc so multiplexed daemons (sshd)
  don't alias `input_waker` slots across sessions — this is why sshd reaches the
  child's state via `TIOCSWINSZ` on the `ChildStdout(pid)` fd rather than its own.
- **Key translation** (`akuma_ssh::util::translate_input_keys`): the
  `EscapeState` machine (Normal→Escape→Bracket→`BracketNum(u8)`→Normal) turns
  xterm sequences into actions: Delete `\x1b[3~`, Home `\x1b[1~`, End `\x1b[4~`,
  arrows `\x1b[A/B/C/D`. Window-change requests signal a resize byte to the
  foreground process.

## Background

- `archive/SSH.md`, `archive/SSH_STREAMING_ARCHITECTURE.md`.
- `archive/RICH_TERMINAL_INTERFACE_OVER_SSH.md`, `archive/INTERACTIVE_IO.md`.
- `userspace/sshd/docs/FLOW.md`, `userspace/sshd/docs/LIMITATIONS.md`.
