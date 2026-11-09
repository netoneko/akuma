#!/usr/bin/python3
"""waitid(2) and thread-group reaping probe. Runs ON an Akuma box (needs python3 there); the expected Linux
answer for each line is in its label. Copy it over with  base64 | ssh ... "base64 -d > /tmp/waitid_probe.py".
Results and the bug it caught: docs/archive/CHILD_OF_EXITED_THREAD_UNREAPABLE.md."""
import threading
import os, subprocess, time, ctypes, struct
libc = ctypes.CDLL(None, use_errno=True)
P_ALL, P_PID, P_PIDFD = 0, 1, 3
WNOHANG, WEXITED, WNOWAIT = 1, 4, 0x01000000
def wid(idt, i, opts):
    buf = ctypes.create_string_buffer(128)
    r = libc.waitid(idt, i, buf, opts)
    e = ctypes.get_errno()
    si_signo, si_errno, si_code = struct.unpack_from("iii", buf, 0)
    si_pid, = struct.unpack_from("i", buf, 16); status, = struct.unpack_from("i", buf, 24)
    return ("rc", r, "errno", e if r else 0, "code", si_code, "pid", si_pid, "status", status)
def sp(cmd): return subprocess.Popen(cmd)
p = sp(["/bin/sh", "-c", "exit 7"]); time.sleep(0.5)
print("P_PID WNOWAIT       ", wid(P_PID, p.pid, WEXITED | WNOWAIT), "(child", p.pid, ")")
print("P_PID again (reap)  ", wid(P_PID, p.pid, WEXITED))
print("P_PID after reap    ", wid(P_PID, p.pid, WEXITED | WNOHANG))
q = sp(["/bin/sleep", "2"])
print("P_PID WNOHANG live  ", wid(P_PID, q.pid, WEXITED | WNOHANG))
fd = os.pidfd_open(q.pid)
print("P_PIDFD blocking    ", wid(P_PIDFD, fd, WEXITED), "(waits ~2s)")
r = sp(["/bin/true"]); time.sleep(0.5)
print("P_ALL               ", wid(P_ALL, 0, WEXITED | WNOHANG))
me = os.pidfd_open(os.getpid())
print("P_PIDFD on self     ", wid(P_PIDFD, me, WEXITED | WNOHANG), "(Go's probe needs ECHILD=10)")


# A child forked by a thread that then exits must still be reaped by its process (Linux: yes; Akuma before 2026-11-09: ECHILD).
box = {}
th = threading.Thread(target=lambda: box.setdefault("p", sp(["/bin/true"])))
th.start(); th.join(); time.sleep(0.5)
try:
    print("forker thread exited, main waits", os.waitpid(box["p"].pid, os.WNOHANG), "(expect (pid, 0))")
except ChildProcessError as e:
    print("forker thread exited, main waits EXC", e, "(BUG: expect (pid, 0))")
