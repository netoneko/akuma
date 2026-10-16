#!/usr/bin/env python3
"""Drive herdr on an Akuma box over an ssh PTY and check that scrollback works.

    python3 scripts/herdr_scrollback.py [--host 192.168.1.159] [--port 2222]
                                        [--key ~/.ssh/id_ed25519] [--show]

What it proves: a program in a herdr pane prints more lines than fit; the
pointer's wheel (SGR mouse reports, ESC [ < 64 ; col ; row M — what any
terminal emits for a wheel or two-finger scroll) scrolls the pane back to the
earlier lines, and wheel-down returns to the live tail. The screen is rendered
by pyte, so what is asserted is what a human would see.

Needs `pyte` (run with an interpreter that has it). Python on the host, not a
shell `ssh` line: see CLAUDE.md, "VM Access".
"""
import argparse
import fcntl
import os
import pty
import re
import select
import struct
import sys
import termios
import time

import pyte

ROWS, COLS = 45, 140


class TermScreen(pyte.Screen):
    """pyte, plus the answers a real terminal gives herdr's startup queries.

    pyte raises on private-mode CSI queries (`CSI ? ... n`); a TUI waiting for
    a reply that never comes can also stall. Replies go back down the PTY.
    """

    reply = None

    def report_device_status(self, mode, **kwargs):
        if self.reply and mode == 5:
            self.reply(b"\x1b[0n")
        elif self.reply and mode == 6:
            self.reply(f"\x1b[{self.cursor.y + 1};{self.cursor.x + 1}R".encode())

    def report_device_attributes(self, mode=0, **kwargs):
        if self.reply and mode == 0 and not kwargs.get("private"):
            self.reply(b"\x1b[?62;c")

    def set_mode(self, *modes, **kwargs):
        super().set_mode(*modes, **{k: v for k, v in kwargs.items() if k == "private"})

    def reset_mode(self, *modes, **kwargs):
        super().reset_mode(*modes, **{k: v for k, v in kwargs.items() if k == "private"})


class Session:
    def __init__(self, argv):
        self.screen = TermScreen(COLS, ROWS)
        self.stream = pyte.ByteStream(self.screen)
        pid, fd = pty.fork()
        if pid == 0:
            os.environ["TERM"] = "xterm-256color"
            os.execvp(argv[0], argv)
        self.pid, self.fd = pid, fd
        self.screen.reply = self.send
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

    def pump(self, secs):
        end = time.time() + secs
        while time.time() < end:
            r, _, _ = select.select([self.fd], [], [], 0.1)
            if r:
                try:
                    data = os.read(self.fd, 65536)
                except OSError:
                    return
                if not data:
                    return
                self.stream.feed(data)

    def send(self, data):
        os.write(self.fd, data if isinstance(data, bytes) else data.encode())

    def text(self):
        return "\n".join(line.rstrip() for line in self.screen.display)

    def close(self):
        try:
            os.kill(self.pid, 9)
        except ProcessLookupError:
            pass


def wheel(s, up, n, col=60, row=20):
    code = 64 if up else 65
    for _ in range(n):
        s.send(f"\x1b[<{code};{col};{row}M")
        s.pump(0.05)


def numbers_on_screen(text):
    # A pane line is `<sidebar>│<number>   ...<scrollbar glyph>`.
    return sorted({int(m) for m in re.findall(r"(?m)│(\d{1,3})\s*[▕▐ ]*$", text)})


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="192.168.1.159")
    ap.add_argument("--port", default="2222")
    ap.add_argument("--key", default=os.path.expanduser("~/.ssh/id_ed25519"))
    ap.add_argument("--show", action="store_true", help="print the screen at each stage")
    a = ap.parse_args()

    s = Session(["ssh", "-tt", "-i", a.key, "-p", a.port,
                 "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
                 "-o", "LogLevel=ERROR", f"root@{a.host}", "herdr"])
    ok = True
    try:
        s.pump(6)
        if a.show:
            print("--- after attach ---\n" + s.text())
        # First-run dialogs (welcome, then settings) swallow keystrokes: Enter
        # to continue, Escape to close whatever is left.
        s.send("\r")
        s.pump(1.5)
        s.send("\x1b")
        s.pump(1.5)
        if a.show:
            print("--- after dismissing dialogs ---\n" + s.text())
        s.send("seq 1 300\r")
        s.pump(3)
        tail = numbers_on_screen(s.text())
        if a.show:
            print("--- after seq 1 300 ---\n" + s.text())
        print(f"live tail shows {tail[:1]}..{tail[-1:]} ({len(tail)} numbers)")
        if not tail or tail[-1] != 300:
            print("FAIL: the pane does not end at 300 (herdr did not start a shell pane?)")
            ok = False
        wheel(s, True, 30)
        s.pump(1)
        back = numbers_on_screen(s.text())
        if a.show:
            print("--- after wheel up x30 ---\n" + s.text())
        print(f"after wheel-up shows {back[:1]}..{back[-1:]} ({len(back)} numbers)")
        if not back or back[-1] >= (tail[-1] if tail else 300) or (tail and back[0] >= tail[0]):
            print("FAIL: wheel-up did not reveal earlier lines")
            ok = False
        wheel(s, False, 60)
        s.pump(1)
        again = numbers_on_screen(s.text())
        print(f"after wheel-down shows {again[:1]}..{again[-1:]}")
        if not again or again[-1] != 300:
            print("FAIL: wheel-down did not return to the live tail")
            ok = False
    finally:
        s.close()
    print("PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
