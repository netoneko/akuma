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

**Status:** Open
**Component:** SSH server / PTY layer

When connected via SSH, programs that repaint the terminal (e.g. `top`, `meow`,
any fullscreen TUI) do not clear/reposition the cursor properly. Instead of
redrawing in place, output appears to scroll continuously in a single moving
line, making interactive programs unusable over SSH.

**Symptoms:**

- Screen content is appended rather than overwritten.
- ANSI escape sequences for cursor positioning (`\x1b[H`, `\x1b[2J`, etc.)
  appear to be ignored or not transmitted correctly.
- Works fine over the direct UART/telnet console.

Likely causes:

- The SSH channel is not advertising terminal capabilities correctly (missing
  `TERM` environment variable or pty allocation).
- Escape sequences are being filtered or corrupted in the SSH data path.
- The SSH server's PTY emulation does not handle cursor-addressing escapes.
- Window-size (`SIGWINCH` equivalent) is not being forwarded, causing programs
  to assume a 0×0 or 1-column terminal.
