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

---

## 4. `reattach` fails to wake target process

**Status:** Open
**Component:** `src/process.rs`, `src/threading.rs`

The `sys_reattach` syscall successfully delegates I/O channels, but the target
process (often `paws` or `meow`) remains in a `WAITING` state and does not
respond to input, even when the kernel logs show explicit wake-up calls.

**Observation:**
Kernel logs show `Writing X bytes to PID Y stdin` followed by `Waking PID Y`,
but the target process thread does not transition to `READY` or resume
execution to consume the buffer. This occurs despite the implementation of
"Sticky Wake" logic.

---

## 5. `/proc/boxes` appears empty in userspace

**Status:** Open
**Component:** `src/vfs/proc.rs`

While kernel logs indicate that boxes are being registered (e.g., `[ProcFS] Reading boxes (count=2)`), 
the userspace `box ps` utility frequently reports "No active boxes found."

**Reproduction:**
```
akuma:/> box ps
No active boxes found.
```

This may indicate a discrepancy in how `read_at` or `read_dir` handles virtual
ProcFS files, or a synchronization issue between the global `BOX_REGISTRY`
and the VFS view.

---

## 6. bun HTTPS fetch hangs — epoll_pwait sleeps forever on large positive timeout

**Status:** Fixed (2026-03-14) in `src/syscall/poll.rs`
**Component:** `src/syscall/poll.rs` — `sys_epoll_pwait`

### Symptom

Running a bun script that performs an HTTPS `fetch()` hangs indefinitely after
the TLS handshake completes.  The HTTP request is never sent and the process
loops on 4-second timerfd callbacks forever.

### Root cause

bun's internal network/resolver thread calls
`epoll_pwait(epfd=12, ..., timeout=1827387391ms)` — roughly a 21-day timeout.

`epoll_wait_deadline()` for positive timeouts returned `start_time + timeout_us`
(the absolute deadline for the whole wait, not the per-iteration sleep).
`schedule_blocking(deadline)` was then called with that huge deadline.

`schedule_blocking` parks the kernel thread until the deadline is reached.
The only early-exit path was an explicit `wake()` call, which only happens when
a thread directly `read()`s a blocking eventfd — not when an eventfd's epoll
registration fires.  So the thread slept until the 21-day deadline expired.

Meanwhile bun's main event loop (epfd=19, timeout=-1) correctly polled every
10 ms because the infinite-wait case always used `now + 10ms` as the deadline.
This asymmetry meant only the `-1` case worked.

### Diagnostic trace

After TLS handshake completion (two-level signaling in bun):

1. TLS worker writes `eventfd fd=14 (id=1)` → signals the network thread.
2. Network thread is in `epoll_pwait(epfd=12, timeout=1827387391ms)` watching
   `fd=14`.  Due to the bug it is sleeping until the year 2047 and never polls.
3. DNS had correctly used `fd=21 (id=2)` to signal the main loop (epfd=19),
   which polled every 10 ms — those events fired fine.
4. The main loop (epfd=19) only sees 4-second timerfd ticks; the HTTP request
   is never sent.

### Fix

In `sys_epoll_pwait`, cap the per-iteration `schedule_blocking` deadline to
`now + BLOCKING_POLL_INTERVAL_US (10ms)` regardless of the total timeout:

```rust
let abs_deadline = epoll_wait_deadline(timeout, start_time, timeout_us, now);
let deadline = abs_deadline.min(now + BLOCKING_POLL_INTERVAL_US);
schedule_blocking(deadline);
```

The absolute deadline is still checked at the top of every loop iteration to
correctly return 0 when the caller's timeout expires.

### Related bugs found during investigation

- **EPOLLET drain reset** (`epoll_on_fd_drained`): After a successful TCP
  `recvfrom`/`recvmsg`, the EPOLLET edge was not reset, so new data arriving
  before the socket drained to EAGAIN would not re-fire EPOLLIN.  Fixed by
  calling `epoll_on_fd_drained(fd)` after every successful read.
- **eventfd id vs fd confusion**: bun uses two eventfds for a two-level
  notification scheme (DNS/completion → main loop via `fd=21 id=2`;
  TLS/work-done → network thread via `fd=14 id=1`).  The level-triggered
  EPOLLIN for the network thread's eventfd was masked by the sleep bug above.
