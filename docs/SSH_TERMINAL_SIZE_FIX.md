# SSH Terminal Size Fix (TIOCGWINSZ)

## Problem

When running full-screen terminal apps (e.g. neatvi, less) via SSH, the application
would not use the full terminal height. For example, neatvi set the scroll region to
rows 1–23 when the actual terminal was taller:

```
[syscall] ioctl(fd=0, cmd=0x5413, arg=0x3ffffcd8)   # TIOCGWINSZ
[syscall] ioctl result=0
[syscall] write(fd=1, count=728) ".[m.[r.[1;23r.[1;1H..."  # DECSTBM sets 23-row region
```

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

There were two distinct gaps:

**Gap 1 — initial PTY dimensions not copied:**
`run_shell_session` creates `TerminalState::default()` (80×24) *after* the `pty-req`
has already been processed and stored in `session.term_width/height`. Those values
were never written into the new `TerminalState`.

**Gap 2 — window-change not propagated in the bridge path:**
When bridging an external shell process (`bridge_process`), SSH packets are read
directly from the TCP stream and dispatched manually. The `SSH_MSG_CHANNEL_REQUEST`
message type (which carries `window-change`) was not handled — it fell through
silently. The stale comment read:

```rust
// TIOCGWINSZ will pick up session.term_width/height next time it's called
```

This was wrong: `TIOCGWINSZ` reads from `TerminalState`, not from `SshSession`.

## Fix

### 1. Initialize `TerminalState` from session dimensions (`run_shell_session`)

Capture the PTY dimensions before `session` is borrowed by `SshChannelStream`, then
apply them immediately after creating the `TerminalState`:

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

### 2. Propagate `window-change` in `bridge_process`

Pass the `terminal_state` Arc into `bridge_process` and handle
`SSH_MSG_CHANNEL_REQUEST` in the packet dispatch loop:

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

## Files Changed

| File | Change |
|------|--------|
| `src/ssh/protocol.rs` | `run_shell_session`: capture PTY dims before borrowing session; init `TerminalState` with actual dims. |
| `src/ssh/protocol.rs` | `bridge_process`: add `terminal_state` Arc parameter; handle `SSH_MSG_CHANNEL_REQUEST` / `window-change`; update both `session` and `TerminalState`. |

## Architecture Note

`TerminalState` is an `Arc<Spinlock<…>>` created once per SSH session and inherited
by all child processes via `spawn_process_with_channel` →
`current_terminal_state()`. Updating it in the bridge therefore propagates to all
descendants automatically. No per-process update is needed.
