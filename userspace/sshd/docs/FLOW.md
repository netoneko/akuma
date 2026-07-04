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
  run_built_in_shell,│    -> poll_fn: WouldBlock => Pending  ────┼──> suspends THIS
  send_packet, ...   │       (yields to the multiplexer)         │    session's future;
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

## `shell` vs `exec` — same pipe, different entry

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
                                      - waitpid(pid) -> drain remaining stdout -> return
```

`ssh host <cmd>` (`ssh -p 2223 root@localhost echo hi`) used to do nothing:
`handle_message` only recognized the `"shell"` channel-request type, so an
`"exec"` request fell through with no reply and no spawned process. It now
parses the command string out of the same `CHANNEL_REQUEST` payload (right
after the `want_reply` byte) and spawns `<shell> -c <cmd>` through the exact
same `bridge_process` pump interactive sessions use — so exit-on-child-exit,
stdin forwarding, and stdout draining behave identically for both.

## Where the wire-format parsing lives

`read_string`/`read_u32`/`write_u32`/packet framing/`SimpleRng`/key
derivation used to be a hand-duplicated copy of
[`crates/akuma-ssh-crypto`](../../../crates/akuma-ssh-crypto) (the same
primitives the in-kernel SSH server uses). `sshd/src/crypto.rs` now
re-exports from that crate instead, so its existing host-run test suite
(`cargo test -p akuma-ssh-crypto`) covers the exact byte-parsing code the
`exec` command-string extraction above depends on.
