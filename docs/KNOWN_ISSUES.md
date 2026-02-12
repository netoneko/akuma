# Known Issues

Tracked bugs and glitches observed in the running system.

---

## 1. httpd CGI scripts produce empty output

**Status:** Open
**Component:** `userspace/httpd`

The HTTP server's CGI handler invokes scripts but captures 0 bytes of output,
so the response body is always empty.

**Reproduction:**

```
akuma:/> httpd
httpd: Starting HTTP server on port 8080
httpd: Listening for connections...
[Thu, 12 Feb 2026 00:48:32 GMT] CGI GET /cgi-bin/akuma.js
httpd: Raw CGI output captured: 0 bytes
httpd: CGI output len=0, body len=0
httpd: === CGI BODY START ===

httpd: === CGI BODY END ===
```

The CGI request is received and dispatched, but the script's stdout is never
captured. Likely causes:

- The child process's stdout file descriptor is not being redirected/read
  correctly.
- The script interpreter (QuickJS) may exit before its output is flushed or
  collected.
- A pipe/fd plumbing issue in the `exec` + `read` syscall path.

---

## 2. `top` reports impossible CPU percentages

**Status:** Open
**Component:** `userspace/top`, kernel scheduling/accounting

`top --once` shows individual threads well above 100% CPU and the total far
exceeding any reasonable value, indicating broken time accounting.

**Reproduction:**

```
akuma:/> top --once
Akuma OS - CPU Stats (press 'q' to quit)
TID  PID  STATE       CPU%   TIME(ms)  NAME
--------------------------------------------------
  0    0  READY    756.3%      2847
  1    0  READY    294.4%      1368
  2    0  READY    189.6%      1080
  3    0  READY    010.3%       356
  8    0  WAITING  395.2%       426
  9    0  RUNNING  406.3%       105
```

Cross-referenced with `kthreads`, there are only ~5 threads in the system.
CPU% values summing to >2000% are clearly wrong.

Likely causes:

- The timer tick counter or per-thread CPU-time accumulator overflows or wraps.
- The sampling interval used to compute the percentage is too short or
  incorrectly calculated (e.g. using the wrong clock source / frequency).
- Wall-clock elapsed time is near zero, making the ratio blow up.

---

## 3. SSH terminal rewriting is broken (no proper cursor movement)

**Status:** Open — root cause identified
**Component:** `src/syscall.rs` (terminal syscalls), `src/shell/mod.rs` (streaming loop)

When connected via SSH, programs that repaint the terminal (e.g. `top`, `meow`,
any fullscreen TUI) do not clear/reposition the cursor properly. Instead of
redrawing in place, output appears to scroll continuously in a single moving
line, making interactive programs unusable over SSH.

**Example output (meow):**

Each render cycle's footer is appended instead of drawn in place, producing an
endlessly growing stream of repeated status bars:

```
...━━━━━━━━  [Provider: ollama] [Model: gemma3:4b]  [2k/128k|24K|Hist: 2K] (=^･ω･^=) >
  [MEOW] awaiting user input.....━━━━━━━━  [Provider: ollama] [Model: gemma3:4b] ...
```

### Root cause: terminal control syscalls are all stubs

All six terminal syscalls (307-312) in `src/syscall.rs:108-113` return `0`
without doing anything:

```rust
nr::SET_TERMINAL_ATTRIBUTES => 0,  // 307 — should set raw mode
nr::GET_TERMINAL_ATTRIBUTES => 0,  // 308 — should return current mode
nr::SET_CURSOR_POSITION => 0,      // 309 — should write \x1b[{row};{col}H
nr::HIDE_CURSOR => 0,              // 310 — should write \x1b[?25l
nr::SHOW_CURSOR => 0,              // 311 — should write \x1b[?25h
nr::CLEAR_SCREEN => 0,             // 312 — should write \x1b[2J\x1b[H
```

Userspace programs (meow, top, etc.) call these through `libakuma` wrappers
(`set_cursor_position()`, `clear_screen()`, `hide_cursor()`, `show_cursor()`).
Since the kernel never emits the corresponding ANSI escape sequences to the
process's stdout `ProcessChannel`, the SSH client never sees any cursor
movement. All text is simply appended.

For example, `meow`'s `render_footer` calls `set_cursor_position()` ~15 times
per render cycle. None of these actually move the cursor.

### Secondary issue: raw mode is never activated

Because `SET_TERMINAL_ATTRIBUTES` (307) is a stub:

1. When meow calls `set_terminal_attributes(STDIN, 0, RAW_MODE_ENABLE)`, it's
   a no-op.
2. `ProcessChannel.raw_mode` stays `false` (initialized as `false`, `set_raw_mode(true)` is never called anywhere in the kernel).
3. The SSH input handler (`src/ssh/protocol.rs:798-821`) always takes the
   cooked-mode branch — line editing, echo, Enter-to-submit — instead of
   passing raw keystrokes to the process.

This means TUI apps cannot receive individual keystrokes, arrow keys, etc.

### Tertiary issue: unconditional `\n` → `\r\n` conversion

The shell streaming loop (`src/shell/mod.rs:541-546`) converts every `\n` byte
to `\r\n` in process output, regardless of raw/cooked mode. This is correct for
normal line-oriented output but:

- TUI apps that emit `\r\n` themselves would get double-CR (`\r\r\n`).
- Binary protocols or raw escape sequences containing `0x0A` would be corrupted
  (though common ANSI CSI sequences don't contain `\n`).

### Fix plan

The syscalls need to write the corresponding escape sequences into the
calling process's `ProcessChannel` stdout buffer (the same buffer that
`sys_write` uses). Specifically:

| Syscall                  | Should emit                             |
|--------------------------|-----------------------------------------|
| `SET_CURSOR_POSITION(c,r)` | `\x1b[{r+1};{c+1}H`                 |
| `HIDE_CURSOR`            | `\x1b[?25l`                             |
| `SHOW_CURSOR`            | `\x1b[?25h`                             |
| `CLEAR_SCREEN`           | `\x1b[2J\x1b[H`                        |
| `SET_TERMINAL_ATTRIBUTES`| Call `channel.set_raw_mode(true/false)` |
| `GET_TERMINAL_ATTRIBUTES`| Return current `raw_mode` flag          |

Additionally:
- The `\n` → `\r\n` conversion should be skipped when the channel is in
  raw mode (TUI apps handle their own line endings).
- The SSH `pty-req` TERM variable (currently discarded at
  `src/ssh/protocol.rs:1270`) should be stored and made available to
  processes.
