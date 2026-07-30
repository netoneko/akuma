# sshd Connection Flow (Cooperative Multiplexer)

How `userspace/sshd` went from "one connection at a time" to concurrent
sessions, and how `ssh host <cmd>` (exec) fits into the same pipe as an
interactive shell. See [`docs/OPTIONAL_SMOLTCP.md`](../../../docs/OPTIONAL_SMOLTCP.md)
for the rump-networking backstory that made this possible.

## Before: one `block_on` per connection

```
main()
  loop {
      (stream, _) = listener.accept()        // BLOCKS in-kernel until a client connects
      block_on(handle_connection(stream))    // BLOCKS until this session ends entirely
  }
```

`accept()` and every socket read were blocking, in-kernel calls. A second
client connecting while the first session was alive just piled up in the
kernel's TCP backlog — `accept()` never returned for it until the first
`handle_connection` returned. Interactive sessions ran fine one at a time;
a second simultaneous SSH connection hung until the first one exited.

## After: one future per connection, polled cooperatively

```
main()
  listener.set_nonblocking(true)
  sessions: Vec<Pin<Box<dyn Future<Output = ()>>>> = []

  loop {
      match listener.try_accept() {          // one-shot, returns WouldBlock immediately
          Ok((stream, _)) => {
              set_nonblocking(stream.fd, true)
              sessions.push(Box::pin(protocol::handle_connection(SshStream::new(stream), config)))
          }
          Err(WouldBlock) => {}
      }

      for session in &mut sessions {         // poll every live session once per tick
          if session.poll(cx) == Ready { remove it }
      }

      if nothing happened this tick { sleep_ms(1) }
  }
```

```
 tick N:     [accept?]  [poll A]  [poll B]  [poll C]  -> sleep_ms(1) if all idle
 tick N+1:   [accept?]  [poll A]  [poll B]  [poll C]  -> ...
```

Every connection's whole lifecycle — version exchange, key exchange, auth,
channel open, shell/exec — is one `async fn handle_connection` state machine.
`main()` never awaits any *one* of them to completion; it round-robins a
single poll of each, same idea as a tiny hand-rolled `select!` loop. While
session A's spawned shell sits idle waiting on the client to type something,
its future returns `Pending` and the loop moves straight on to poll B and C.

## Why sockets had to actually support non-blocking I/O

The multiplexer only works if `try_accept()` and a session's socket
read/write can truly return "nothing yet" instead of parking the one OS
thread `sshd` runs on. Two layers had to agree on that:

1. **Kernel (`src/rump_proxy.rs`)** — the devbox's default network stack is
   NetBSD rump (see `OPTIONAL_SMOLTCP.md`), and rump sockets used to hard-fail
   `fcntl(F_SETFL, O_NONBLOCK)` with `EOPNOTSUPP` (a deliberate anti-footgun
   from an earlier bug). That made `listener.set_nonblocking(true)` fail and
   crash-looped `sshd` before this fix — `fcntl` on a rump fd now actually
   flips `Process::is_nonblock`, which `accept`/`recvfrom`/`sendto` already
   read on every call.
2. **`libakuma::net`** — `TcpListener` gained `try_accept()` (one-shot, no
   internal EAGAIN-retry loop) and `set_nonblocking()`.

## Two read paths in `SshStream` — and why there are two

```
                     ┌─────────────────────────────────────────┐
                     │            SshStream (main.rs)           │
                     │                                           │
  handshake loop,    │  Read::read()/Write::write()              │
  send_packet, ...   │    -> poll_fn: WouldBlock => Pending  ────┼──> suspends THIS
                     │       (yields to the multiplexer)         │    session's future;
                     │                                           │    other sessions
                     │  try_read()                                │    keep polling.
  bridge_process's   │    -> WouldBlock => Err, returns now  ────┼──> does NOT suspend;
  own ssh-input read │       (never awaits/suspends)              │    same poll tick
                     └─────────────────────────────────────────┘    still drains the
                                                                      child's stdout.
```

`bridge_process` (the interactive/exec I/O pump) already manually
interleaves two things in one loop iteration: draining the spawned child's
stdout and forwarding SSH input to its stdin. If reading SSH input suspended
the whole `handle_connection` future (like `Read::read` does everywhere
else), the loop would never get back to draining stdout while waiting for
the next keystroke — a session-local stall, independent of the multiplexer.
So `bridge_process` calls the non-suspending `try_read()` instead.

## Two more blockers only a *live* second connection turned up

The multiplexer above looked correct and passed automated concurrency tests
(two `ssh` processes started back-to-back and driven programmatically), but
a *manual* test — open one connection, sit at its prompt, then open a
second — still hung: the second connection got zero bytes back, not even
the SSH version banner, until the first was closed. Two bugs, both about
things that "being inside an `async fn`" does **not** automatically fix:

**1. A blocking `sleep_ms` inside a loop with no `.await` on it never
suspends the future.** `bridge_process`'s idle branch was
`if !did_io { sleep_ms(10); }` — `sleep_ms` is a raw blocking `NANOSLEEP`
syscall. Rust only yields an `async fn` at an explicit `.await` point; a
loop that never hits one just runs synchronously forever once polled. So
the *first* session to reach that idle branch (e.g. sitting at an
interactive shell prompt, waiting for a keystroke) never returned control to
`main()`'s executor — its `poll()` call simply never came back, and
`try_accept()` for every other connection starved for as long as that
session's shell stayed open. Fixed with a proper one-shot async yield:

```rust
pub async fn yield_now() {
    let mut yielded = false;
    poll_fn(|cx| {
        if yielded { Poll::Ready(()) }
        else { yielded = true; cx.waker().wake_by_ref(); Poll::Pending }
    }).await
}
```
`bridge_process` (and every other spawned-command loop that used to call
`sleep_ms` in the same pattern) now does `crate::yield_now().await` instead
— a real suspend, not a blocking sleep dressed up as async.

**2. `sshd`'s own `TerminalState` was shared by every `spawn_pty` child.**
`spawn_process_with_channel_ext` (`crates/akuma-exec/src/process/spawn.rs`)
inherits `current_terminal_state()` from the *caller* by default — correct
for a real shell forking a subcommand (they should share one controlling
terminal), wrong for a multiplexing daemon: `sshd` is one OS process with
one `TerminalState` of its own, so every session's spawned shell used to
inherit **the same one**, including its single `input_waker` slot. A stdin
wakeup meant for session B's parked reader could get delivered to session
A's instead (and vice versa), so a session's shell could permanently miss
its wakeup while a sibling session kept using "the" terminal. Fixed by *not*
inheriting for a `pty` spawn — it now keeps the fresh, independent
`TerminalState` every process already gets by default (`Process::new`),
matching real Unix semantics: allocating a new pty starts a new session, it
doesn't borrow the allocator's.

## `shell` vs `exec` — busybox is the only shell now

```
CHANNEL_REQUEST -----------------+
                                 |
             "shell" ------------------> run_shell_session
                                 |            spawn_pty(shell, [])         (pty: cooked tty)
             "exec", cmd --------------> run_exec_session
                                 |            spawn(shell, ["-c", cmd])    (pipe: no tty)
                                              |
                                              v
                                    bridge_process(pid, stdout_fd)
                                      - poll child stdout  (read_fd, non-blocking)
                                      - poll SSH input     (try_read, non-blocking)
                                      - waitpid_status(pid) -> drain stdout
                                                     -> close fds
                                                     -> send_exit_report(status):
                                                          exit-status OR exit-signal
                                                          + EOF + CLOSE
                                                     -> wait for client's CLOSE

  spawn failure (either path) -> fail_spawn(): error message to client,
  exit status 127, session ends. No built-in shell to fall back to.
```

The RFC 4254 §6.10 exit report is not optional bookkeeping — it is the only way
the client learns how the command ended, and omitting it made every command
report 255. `waitpid_status` (not `waitpid`) because a signal death has
`WEXITSTATUS` 0 and would otherwise be reported as a success. The trailing wait
for the client's `CHANNEL_CLOSE` is what stops the socket close from racing the
flush of those final packets. See [`EXIT_STATUS_FIX.md`](EXIT_STATUS_FIX.md).

`ssh host <cmd>` (`ssh -p 2223 root@localhost echo hi`) used to do nothing:
`handle_message` only recognized the `"shell"` channel-request type, so an
`"exec"` request fell through with no reply and no spawned process. It now
parses the command string out of the same `CHANNEL_REQUEST` payload (right
after the `want_reply` byte) and spawns `<shell> -c <cmd>` through the exact
same `bridge_process` pump interactive sessions use — so exit-on-child-exit,
stdin forwarding, and stdout draining behave identically for both.

`sshd` originally had a hand-rolled fallback shell (`userspace/sshd/src/shell/`)
for when no external shell was configured or a configured one failed to
spawn — a small untested command interpreter duplicating what a real shell
already does. It's been removed entirely: `session.config.shell` is now a
plain `String` (not `Option<String>`), defaulting to `config::DEFAULT_SHELL`
(busybox's `/bin/sh` — present on every bootstrap/devbox image). A spawn
failure now ends the session with an error message via `fail_spawn` instead
of degrading to a fallback.

## Where the wire-format parsing lives

`read_string`/`read_u32`/`write_u32`/packet framing/`SimpleRng`/key
derivation used to be a hand-duplicated copy of
[`crates/akuma-ssh-crypto`](../../../crates/akuma-ssh-crypto) (the same
primitives the in-kernel SSH server uses). `sshd/src/crypto.rs` now
re-exports from that crate instead, so its existing host-run test suite
(`cargo test -p akuma-ssh-crypto`) covers the exact byte-parsing code the
`exec` command-string extraction above depends on.

Byte layout that is specific to *this* server — the channel teardown messages —
lives in `sshd/src/wire.rs`, a lib target with its own host unit tests. Note that
`crypto.rs` cannot host-test: it calls `libakuma::getrandom`. Pure code must
import from `akuma-ssh-crypto` directly rather than through that re-export, or it
drags `libakuma` (and its `#[panic_handler]`) into the build. See
[`../src/lib.rs`](../src/lib.rs).
