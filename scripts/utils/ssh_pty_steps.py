#!/usr/bin/env python3
"""Drive a TUI on an Akuma box over an ssh PTY (120x40) as a scripted series of steps.

    HOSTIP=192.168.1.159 scripts/utils/ssh_pty_steps.py 2222 'cd /tmp; /usr/local/bin/claude' 90 \\
        '@Run/login' '/login' '@Sign in' '\\r' '@Claude account' '\\r' '@Paste code' '~45' \\
        '\\x1b[200~CODE#STATE\\x1b[201~' '@STATE' '\\r'

Each step is one of:
  @text   wait until the output since the previous step contains `text` (ANSI and
          spaces stripped — Ink draws with cursor moves, not spaces);
  ~N      sleep N seconds (a person reading, or switching to a browser);
  keys    anything else, written to the PTY (Python escapes: \\r, \\x1b[27u, ...).

Waiting for screen text instead of fixed delays is the point: claude's start time
varies by seconds, and fixed-delay key scripts typed into the wrong screen. The
`~45` step is how the `/login` paste bug was reproduced (a person takes that long
in the browser; an instant scripted paste never showed it).

Env: HOSTIP (default 127.0.0.1, e.g. a Firecracker guest behind `ssh -L 2223:...`),
XTERM (TERM to export, default xterm-256color). Prints each step with its time, then
the output since the last completed step."""
import os, pty, re, select, sys, time, fcntl, termios, struct
port, cmd, secs = sys.argv[1], sys.argv[2], float(sys.argv[3])
steps = [s if s[:1] in "@~" else s.encode().decode("unicode_escape") for s in sys.argv[4:]]
pid, fd = pty.fork()
if pid == 0:
    os.execvp("ssh", ["ssh", "-tt", "-F", "/dev/null", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
                      "-o", "LogLevel=ERROR", "-i", os.path.expanduser("~/.ssh/id_ed25519"), "-p", port,
                      "root@" + os.environ.get("HOSTIP", "127.0.0.1"),
                      "stty cols 120 rows 40; export TERM=" + os.environ.get("XTERM", "xterm-256color") + "; " + cmd])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
ANSI = re.compile(rb"\x1b\[[0-9;?<>=]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]|\x1b[()][A-Za-z0-9]")
def plain(b): return ANSI.sub(b"", b).decode("utf-8", "replace")
buf = b""; t0 = time.time(); i = 0; mark = 0; log = []
while time.time() - t0 < secs:
    r, _, _ = select.select([fd], [], [], 0.1)
    if r:
        try:
            d = os.read(fd, 65536)
        except OSError:
            break
        if not d:
            break
        buf += d
    if i < len(steps):
        s = steps[i]
        if s.startswith("~"):
            if not hasattr(sys, "_slp"): sys._slp = time.time()
            if time.time() - sys._slp >= float(s[1:]): log.append(f"{time.time()-t0:5.1f}s slept {s[1:]}"); i += 1; del sys._slp
        elif s.startswith("@"):
            if s[1:].replace(" ", "") in plain(buf[mark:]).replace(" ", ""):
                log.append(f"{time.time()-t0:5.1f}s saw {s[1:]!r}"); i += 1; mark = len(buf)
        else:
            time.sleep(0.5)
            os.write(fd, s.encode()); log.append(f"{time.time()-t0:5.1f}s sent {s!r}"); i += 1; mark = len(buf)
print("\n".join(log))
print(f"--- stopped at step {i}/{len(steps)}; output since last step:")
print(plain(buf[mark:])[-600:])
