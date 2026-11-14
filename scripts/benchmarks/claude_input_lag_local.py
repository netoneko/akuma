#!/usr/bin/python3
"""In-box variant of claude_input_lag.py: runs ON the Akuma box, so no ssh/sshd/wifi sits between the
probe and the TUI. The command runs under a local pty; a probe character counts as shown when its byte
reappears in the raw output after it was written (no screen model: python3 on the box has no pyte).

    python3 claude_input_lag_local.py 'cd /root; /usr/local/bin/claude-glm' 20 45 10
        # start, wait 20 s (trust prompt etc. is the caller's job: pass keys as KEYS env, see below),
        # probe 5 keys, idle 45 s, probe 10 keys

Env KEYS="@trust|~1|\\x1b[B|~1|\\r|@auto" drives the first-run screens (same steps as the
ssh harness: '@text' waits for text in the raw stream (Ink splits phrases with cursor codes: use one word), '~N' sleeps, anything else is written).
"""
import os, pty, select, sys, time, fcntl, termios, struct

PROBES = "QXJKZVWQXJKZVWQXJKZVW"
cmd = sys.argv[1]
plan = [int(x) for x in sys.argv[2:]]  # alternating: probes, idle, probes, idle, ...

pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm-256color"
    os.execvp("/bin/sh", ["sh", "-c", cmd])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
buf = bytearray()


def pump(secs):
    end = time.time() + secs
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.005)
        if r:
            try:
                d = os.read(fd, 65536)
            except OSError:
                return False
            if not d:
                return False
            buf.extend(d)
    return True


def wait_for(text, timeout):
    t0 = time.time()
    while time.time() - t0 < timeout:
        if text.encode() in buf:
            return time.time() - t0
        pump(0.05)
    return None


for step in os.environ.get("KEYS", "").split("|"):
    if not step:
        continue
    if step.startswith("@"):
        w = wait_for(step[1:], 120)
        print("wait %r: %s" % (step[1:], "%.1fs" % w if w is not None else "TIMEOUT"))
        buf.clear()
    elif step.startswith("~"):
        pump(float(step[1:]))
    else:
        pump(0.3)
        os.write(fd, step.encode().decode("unicode_escape").encode("latin1"))
        pump(0.2)

def flags(tag):
    try:
        a = termios.tcgetattr(fd)
        print("%s termios: iflag=0x%x lflag=0x%x  ICANON=%d ECHO=%d ICRNL=%d" % (
            tag, a[0], a[3], bool(a[3] & termios.ICANON), bool(a[3] & termios.ECHO), bool(a[0] & termios.ICRNL)))
    except Exception as ex:  # noqa: BLE001
        print(tag, "tcgetattr failed:", ex)


flags("start")
n = 0
for i, v in enumerate(plan):
    if i % 2 == 0:
        xs = []
        for _ in range(v):
            c = PROBES[n % len(PROBES)]
            n += 1
            pump(0.3)
            buf.clear()
            t0 = time.time()
            os.write(fd, c.encode())
            got = None
            while time.time() - t0 < 10:
                pump(0.002)
                if c.encode() in buf:
                    got = time.time() - t0
                    break
            xs.append(got)
            print("  key %r: %s" % (c, "%.1f ms" % (got * 1000) if got is not None else "NOT SEEN in 10s"))
        ok = sorted(x for x in xs if x is not None)
        if ok:
            print("lag: n=%d lost=%d min=%.1f median=%.1f max=%.1f ms" % (len(xs), len(xs) - len(ok), ok[0] * 1000, ok[len(ok) // 2] * 1000, ok[-1] * 1000))
        else:
            print("lag: n=%d lost=%d nothing showed" % (len(xs), len(xs)))
    else:
        print("idle %ds" % v)
        pump(v)
        flags("after idle")
if os.environ.get("NL"):
    # canonical-buffer test: does a bare newline release the keys that never showed?
    buf.clear()
    os.write(fd, b"\n")
    pump(5)
    print("after \\n: %d bytes of output; probe chars seen: %s" % (len(buf), "".join(c for c in set(PROBES) if c.encode() in buf)))
    flags("after nl")
open("/root/lag.raw", "wb").write(bytes(buf[-3000:]))
os.kill(pid, 9)
