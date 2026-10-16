#!/usr/bin/env python3
"""Drive an interactive TUI on the Akuma box over an ssh PTY and report whether it froze.

Written for "claude-kimi / claude-glm hang when used interactively over ssh" (2026-10-16):
the process looked healthy and was in fact spinning at ~2 M syscalls/s in a nested epoll
(`docs/reference/subsystems/syscalls/poll.md` § Nested epoll), starving the UI of input.
A one-shot `-p` run never showed it; only a real terminal session did. So this starts the
program the way a person would (`ssh -tt`, a PTY, TERM set), then:

  1. waits for the first screen and answers the folder-trust prompt if there is one;
  2. sends a prompt and waits for the model's answer to appear;
  3. types characters *while idle* and checks each is echoed back within a deadline
     (the symptom of the spin is input that is accepted and never drawn);
  4. sends a second prompt (a freeze is often the second turn, not the first);
  5. samples the child's syscall rate from /proc/<pid>/syscalls over a second ssh session
     (a healthy idle TUI does tens per second, the spin did millions);
  6. quits and kills anything the closed PTY left behind (Akuma does not deliver the
     hangup to the foreground child: leaked `claude` holds ~/.claude.json.lock).

Exit status 0 only if every check passed.

    scripts/utils/ssh_tui_probe.py --host 192.168.1.159 --cmd /usr/local/bin/claude-kimi
"""
import argparse
import os
import pty
import re
import select
import subprocess
import sys
import time

ANSI = re.compile(rb"\x1b\[[0-9;?<>=]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]|\x1b[()][A-Za-z0-9]")


def plain(b: bytes) -> str:
    return ANSI.sub(b"", b).decode("utf-8", "replace")


class Session:
    def __init__(self, a):
        self.a = a
        self.ssh_base = [
            "ssh", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
            "-o", "LogLevel=ERROR", "-o", "ConnectTimeout=15", "-i", os.path.expanduser(a.key),
            "-p", str(a.port), f"root@{a.host}",
        ]
        self.buf = b""
        pid, fd = pty.fork()
        if pid == 0:
            os.environ["TERM"] = "xterm-256color"
            os.execvp("ssh", self.ssh_base[:1] + ["-tt"] + self.ssh_base[1:] + [a.cmd_line()])
        self.pid, self.fd = pid, fd

    def remote(self, cmd, timeout=30):
        r = subprocess.run(self.ssh_base + [cmd], capture_output=True, timeout=timeout)
        return r.stdout.decode("utf-8", "replace")

    def pump(self, seconds):
        """Read whatever arrives for `seconds`; returns the new bytes."""
        end, got = time.time() + seconds, b""
        while time.time() < end:
            r, _, _ = select.select([self.fd], [], [], 0.2)
            if r:
                try:
                    d = os.read(self.fd, 65536)
                except OSError:
                    break
                if not d:
                    break
                got += d
        self.buf += got
        return got

    def wait_for(self, pattern, timeout, since=None):
        """Wait until `pattern` (regex over the de-ANSI'd text after `since`) shows. Returns seconds or None."""
        t0 = time.time()
        start = len(self.buf) if since is None else since
        while time.time() - t0 < timeout:
            self.pump(0.5)
            # Whitespace-insensitive: a renderer that positions the cursor between characters
            # leaves the de-ANSI'd text chopped into pieces ("w / o / r / d").
            if re.search(pattern, re.sub(r"\s+", "", plain(self.buf[start:])), re.S | re.I):
                return time.time() - t0
        return None

    def send(self, data):
        os.write(self.fd, data.encode() if isinstance(data, str) else data)

    def child_pid(self):
        """The newest live (not zombie) process named like `--proc`: a killed run can leave a
        zombie behind (its parent never reaps it) and sampling that one reads a dead ring."""
        out = self.remote(
            f"for p in $(ps | grep -E '{self.a.proc}$' | grep -v grep | grep -v '{{' | awk '{{print $1}}'); do "
            "grep -q 'State:.Z' /proc/$p/status 2>/dev/null || echo $p; done")
        pids = [int(x) for x in out.split() if x.isdigit()]
        return max(pids) if pids else None

    def syscall_rate(self, pid, window=3.0):
        """Syscalls per second of `pid`, from the kernel's /proc/<pid>/syscalls ring (timestamps in us)."""
        out = self.remote(f"cat /proc/{pid}/syscalls 2>/dev/null | tail -n 4000")
        ts = [int(m.group(1)) for m in re.finditer(r"^\s*(\d{6,})\s+\d+\s+\d+\s+-?\d+\s*$", out, re.M)]
        if len(ts) < 20:
            return 0.0
        span = (ts[-1] - ts[0]) / 1e6
        return len(ts) / span if span > 0 else float("inf")


def type_text(s, text, burst):
    if burst:
        s.send(text)
    else:
        for ch in text:
            s.send(ch)
            s.pump(0.04)


def cmd_line(self):
    return f"cd {self.cwd} && {self.cmd}"


argparse.Namespace.cmd_line = cmd_line


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--host", default="192.168.1.159")
    ap.add_argument("--port", type=int, default=2222)
    ap.add_argument("--key", default="~/.ssh/id_ed25519")
    ap.add_argument("--cmd", default="/usr/local/bin/claude-kimi")
    ap.add_argument("--cwd", default="/tmp")
    ap.add_argument("--proc", default="/usr/local/bin/claude", help="ps name of the process to sample")
    # The answers must not appear in the questions, or the echo of what was typed passes the check.
    ap.add_argument("--prompt1", default="what is 17 times 23? reply with just the number")
    ap.add_argument("--expect1", default="391")
    ap.add_argument("--prompt2", default="what is 19 times 29? reply with just the number")
    ap.add_argument("--expect2", default="551")
    ap.add_argument("--burst", action="store_true", help="send each prompt in ONE write (a paste) instead of per character")
    ap.add_argument("--answer-timeout", type=int, default=120)
    ap.add_argument("--echo-deadline", type=float, default=3.0)
    ap.add_argument("--max-idle-syscalls", type=float, default=20000.0,
                    help="an idle TUI above this many syscalls/s is spinning (the nested-epoll bug did ~2,000,000)")
    a = ap.parse_args()
    results = []

    def check(name, ok, detail=""):
        results.append(ok)
        print(f"{'PASS' if ok else 'FAIL'} {name}{' — ' + detail if detail else ''}", flush=True)

    s = Session(a)
    try:
        # 1. first screen, trust prompt
        t = s.wait_for(r"trust|yes, proceed|esc to|❯|>\s", 60)
        check("first screen drew", t is not None, f"{t:.1f}s" if t else "nothing in 60 s")
        if t is None:
            return 1
        # The folder-trust dialog highlights "No, exit" first. Move to "Yes, I trust this
        # folder" and confirm — as TWO separate keypresses with a pause, the way a person
        # types: Claude Code's input parser takes "\x1b[B\r" arriving in one read as one
        # unknown escape sequence and drops the Enter (this probe once reported a hang
        # that was only its own burst).
        if re.search(r"trust this folder|Quick safety check", plain(s.buf[-4000:]), re.I):
            s.send("\x1b[B")
            s.pump(1.5)
            s.send("\r")
            s.pump(3)
        s.wait_for(r"\?\s*for shortcuts|bypass|try \"|esc to|›|❯", 30)
        pid = s.child_pid()
        check("the program is running", pid is not None, f"pid {pid}")

        # 2. first prompt
        mark = len(s.buf)
        type_text(s, a.prompt1, a.burst)
        s.pump(1)
        mark = len(s.buf)  # only what the program draws after Enter counts as its answer
        s.send("\r")
        t = s.wait_for(a.expect1, a.answer_timeout, since=mark)
        check("first answer appeared", t is not None, f"{t:.1f}s" if t else f"no '{a.expect1}' in {a.answer_timeout} s")
        s.pump(3)

        # 4. second prompt
        type_text(s, a.prompt2, a.burst)
        s.pump(1)
        mark = len(s.buf)
        s.send("\r")
        t = s.wait_for(a.expect2, a.answer_timeout, since=mark)
        check("second answer appeared", t is not None, f"{t:.1f}s" if t else f"no '{a.expect2}' in {a.answer_timeout} s")
        s.pump(3)

        # 5a. type while idle (after both turns, so the junk cannot become part of a prompt): every character must be echoed back promptly
        slow = []
        for ch in "QZXKJWVY":
            before = len(s.buf)
            s.send(ch)
            t0 = time.time()
            ok = False
            while time.time() - t0 < a.echo_deadline:
                s.pump(0.1)
                if ch in plain(s.buf[before:]):
                    ok = True
                    break
            if not ok:
                slow.append(ch)
        check("typed characters echo promptly", not slow, f"not echoed within {a.echo_deadline}s: {slow}" if slow else "8 of 8")
        s.send("\x7f" * 8)
        s.pump(1)

        # 5. is it spinning while idle?
        pid = s.child_pid() or pid
        if pid:
            rate = s.syscall_rate(pid)
            check("idle syscall rate is sane", rate < a.max_idle_syscalls, f"{rate:,.0f} syscalls/s (limit {a.max_idle_syscalls:,.0f})")
    finally:
        # 6. quit, then clean up what a closed PTY leaves behind
        try:
            s.send("\x03")
            s.pump(1)
            s.send("\x03")
            s.pump(2)
        except OSError:
            pass
        try:
            os.close(s.fd)
        except OSError:
            pass
        time.sleep(2)
        # Live processes only. NEVER `kill -9` a pid that has exited: on this kernel a dead
        # process's scheduler tid can already belong to another process, and the kill lands on
        # that one ("[kill] pid=1468 stale tid=20 now owned by pid=1619"). Measured 2026-10-16:
        # that took down the next test's claude and its child, set off a TRAMP-MISMATCH storm,
        # and the box wedged until the hardware watchdog reset it (FIRED=1).
        left = s.remote(
            f"for p in $(ps | grep -E '{a.proc}$' | grep -v grep | grep -v '{{' | awk '{{print $1}}'); do "
            "grep -q 'State:.Z' /proc/$p/status 2>/dev/null || echo $p; done")
        pids = [p for p in left.split() if p.isdigit()]
        if pids:
            print(f"cleanup: killing leaked {pids}", flush=True)
            s.remote("kill -9 " + " ".join(pids) + "; rmdir /root/.claude.json.lock 2>/dev/null; true")
    ok = all(results)
    if not ok or os.environ.get("TUI_PROBE_DUMP"):
        tail = [l.rstrip() for l in plain(s.buf).replace("\r", "\n").split("\n") if l.strip()][-40:]
        print("\n--- last screen text ---\n" + "\n".join(tail) + "\n--- end ---")
    print(f"\n{'ALL PASSED' if ok else 'FAILED'} ({sum(results)}/{len(results)})")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
