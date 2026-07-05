# SSH Terminal Size Fix (TIOCGWINSZ / TIOCSWINSZ)

## Problem

When running full-screen terminal apps (e.g. vi, less) via SSH, the application
would not use the full terminal size. For example, an editor set the scroll region
to rows 1–23 when the actual terminal was taller:

```
[syscall] ioctl(fd=0, cmd=0x5413, arg=0x3ffffcd8)   # TIOCGWINSZ
[syscall] ioctl result=0
[syscall] write(fd=1, count=728) ".[m.[r.[1;23r.[1;1H..."  # DECSTBM sets 23-row region
```

The bug has surfaced twice, once per SSH path:

- **Kernel built-in SSH** (`src/ssh/protocol.rs`, `userspace-sshd` feature off) —
  fixed first; see Gaps 1 & 2 below.
- **Userspace sshd** (`/bin/sshd`, used by the devbox image) — fixed second; see
  Gap 3 below. This path replaces the editor at `/bin/vi` with busybox vi, which
  is what re-exposed it.

## Root Cause

The SSH session stores terminal dimensions from the SSH `pty-req` and `window-change`
channel requests in `SshSession.term_width / term_height`. The kernel's `TerminalState`
struct (shared with processes via `TIOCGWINSZ`) is a separate object that was always
initialized to the hardcoded defaults of 80×24 and never updated.

### Data flow before the fix

```
SSH client → pty-req (e.g. 220×50)
               ↓
         SshSession.term_width/height = 220/50   ← only stored here

         TerminalState.term_width/height = 80/24  ← stuck at defaults
               ↓
         Process calls TIOCGWINSZ
               ↓
         Returns 80×24  ← wrong
```

There were three distinct gaps across the two paths:

**Gap 1 — initial PTY dimensions not copied (kernel built-in SSH):**
`run_shell_session` creates `TerminalState::default()` (80×24) *after* the `pty-req`
has already been processed and stored in `session.term_width/height`. Those values
were never written into the new `TerminalState`.

**Gap 2 — window-change not propagated in the bridge path (kernel built-in SSH):**
When bridging an external shell process (`bridge_process`), SSH packets are read
directly from the TCP stream and dispatched manually. The `SSH_MSG_CHANNEL_REQUEST`
message type (which carries `window-change`) was not handled — it fell through
silently. The stale comment read:

```rust
// TIOCGWINSZ will pick up session.term_width/height next time it's called
```

This was wrong: `TIOCGWINSZ` reads from `TerminalState`, not from `SshSession`.

**Gap 3 — userspace sshd never set the size at all, and couldn't:**
The devbox runs `/bin/sshd` (a userspace binary) instead of the kernel built-in
SSH, so the Gap 1 & 2 fixes did not cover it. The userspace sshd spawned the login
shell via `spawn_pty` (`libakuma`) → SPAWN with `SPAWN_FLAG_PTY`, passing **no
dimensions**, and parsed neither `pty-req` nor `window-change`. Worse, the kernel
had no `TIOCSWINSZ` (`0x5414`) handler, so even if sshd had wanted to set the
size, there was no syscall path to do it — `TIOCGWINSZ` was the only winsize ioctl
implemented. The child therefore read 80×24 from `TIOCGWINSZ` regardless of the
real terminal.

## Fix

### 1. Initialize `TerminalState` from session dimensions (kernel built-in SSH)

In `src/ssh/protocol.rs::run_shell_session`, capture the PTY dimensions before
`session` is borrowed by `SshChannelStream`, then apply them immediately after
creating the `TerminalState`:

```rust
let initial_width = session.term_width;
let initial_height = session.term_height;
// ... create channel_stream, create terminal_state ...
{
    let mut ts = terminal_state.lock();
    ts.term_width = initial_width as u16;
    ts.term_height = initial_height as u16;
}
```

### 2. Propagate `window-change` in the bridge path (kernel built-in SSH)

Pass the `terminal_state` Arc into `src/ssh/protocol.rs::bridge_process` and
handle `SSH_MSG_CHANNEL_REQUEST` in the packet dispatch loop:

```rust
} else if msg_type == SSH_MSG_CHANNEL_REQUEST {
    let mut offset = 0;
    let _recipient = read_u32(&payload, &mut offset);
    if let Some(req_type) = read_string(&payload, &mut offset) {
        if req_type == b"window-change" {
            offset += 1; // skip want_reply
            if let Some(width) = read_u32(&payload, &mut offset) {
                if let Some(height) = read_u32(&payload, &mut offset) {
                    session.term_width = width;
                    session.term_height = height;
                    let mut ts = terminal_state.lock();
                    ts.term_width = width as u16;
                    ts.term_height = height as u16;
                }
            }
        }
    }
}
```

### 3. `TIOCSWINSZ` + userspace sshd `pty-req` / `window-change`

This closes Gap 3 for the devbox (and any other userspace-sshd user). Two parts:

**Kernel — implement `TIOCSWINSZ` (`src/syscall/term.rs`):** add a handler that,
unlike `TIOCGWINSZ` (gated to fd 0-2 and the caller's own state), also works on a
`ChildStdout(child_pid)` fd. When invoked on such an fd it updates the **child's**
`TerminalState` (looked up by pid), falling back to `current_terminal_state()` for
fd 0-2:

```rust
TIOCSWINSZ => {
    // read struct winsize { u16 ws_row, ws_col, ws_xpixel, ws_ypixel }
    let child_pid = match proc.get_fd(fd) {
        Some(FileDescriptor::ChildStdout(pid)) => Some(pid),
        _ => None,
    };
    let ts = match child_pid {
        Some(pid) => lookup_process(pid).map(|p| p.terminal_state.clone()),
        None => current_terminal_state(),
    };
    if let Some(state) = ts {
        let mut s = state.lock();
        s.term_width = width;
        s.term_height = height;
        return 0;
    }
    return ENXIO;
}
```

**Userspace sshd — parse dims and call the ioctl:**
- `SshSession` carries `term_width` / `term_height` (default 80×24).
- `handle_message` parses `pty-req` → stores width/height.
- `run_shell_session` calls `set_terminal_size(stdout_fd, w, h)` right after
  `spawn_pty`, before the first redraw.
- `bridge_process` handles `SSH_MSG_CHANNEL_REQUEST` / `window-change` → calls
  `set_terminal_size` so live resizes reflow full-screen apps.

`set_terminal_size` is a thin wrapper in `libakuma` over `ioctl(TIOCSWINSZ)`.

### Why TIOCSWINSZ targets the child, not sshd

A `pty` spawn (`SPAWN_FLAG_PTY`) deliberately gives the child a **fresh**
`TerminalState` instead of inheriting the spawner's (`crates/akuma-exec/...
/process/spawn.rs`): a multiplexing daemon like sshd has exactly one
`terminal_state`; if every concurrent login shell inherited it, their `input_waker`
slots would alias and sessions would steal each other's stdin wakeups. So sshd
cannot update its own state and expect the shell to see it — it must reach the
**child's** state. The `ChildStdout(pid)` fd is the handle sshd already holds for
the spawned shell, so the kernel resolves `TIOCSWINSZ` on that fd to the child's
state. That state is an Arc shared down to the shell's descendants (shell → vi),
so a single update reaches any full-screen app under the session.

## Files Changed

| File | Change |
|------|--------|
| `src/ssh/protocol.rs` | `run_shell_session`: capture PTY dims before borrowing session; init `TerminalState` with actual dims. *(Gap 1)* |
| `src/ssh/protocol.rs` | `bridge_process`: add `terminal_state` Arc parameter; handle `SSH_MSG_CHANNEL_REQUEST` / `window-change`; update both `session` and `TerminalState`. *(Gap 2)* |
| `src/syscall/term.rs` | Implement `TIOCSWINSZ` (`0x5414`); route to child's `TerminalState` via `ChildStdout(pid)` fd, else caller's own. Placed before the `fd > 2` ENOTTY guard so it applies to child fds. *(Gap 3, kernel)* |
| `userspace/libakuma/src/lib.rs` | Add `set_terminal_size(fd, w, h)` — `ioctl(TIOCSWINSZ)` wrapper. *(Gap 3, lib)* |
| `userspace/sshd/src/protocol.rs` | `SshSession` gains `term_width`/`term_height`; `handle_message` parses `pty-req`; `run_shell_session` applies initial dims; `bridge_process` handles `window-change`. *(Gap 3, sshd)* |

## Architecture Note

`TerminalState` is an `Arc<Spinlock<…>>` inherited by child processes via
`spawn_process_with_channel` → `current_terminal_state()`. For a **non-pty**
spawn the child clones the parent's Arc (shared, updates propagate); for a
**pty** spawn the child gets a fresh Arc (deliberate isolation for multiplexed
daemons — see "Why TIOCSWINSZ targets the child" above). The two SSH paths each
have their own update site:

- Kernel built-in SSH updates the Arc it created for the session directly (in-process).
- Userspace sshd updates the child's Arc via `TIOCSWINSZ` on the `ChildStdout` fd
  (cross-process), since it cannot reach the child's `TerminalState` directly.
