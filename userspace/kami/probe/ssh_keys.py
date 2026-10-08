#!/usr/bin/env python3
"""Run kami on a live Akuma over an ssh pty, feed it keys, read its logs.

    python3 ssh_keys.py HOST [URL] [--keys "j,ENTER,TAB,CTRL-Q"] [--wait 240] [--env NAME=VALUE]...

HOST is the Akuma address (sshd on 2222, root, default key). kami runs as
`kami URL` with a pty for stdin/stdout, so its raw-mode and input thread see a
tty. The script waits for `ready for frames` in /tmp/kami-input.log, sends each
key one second apart, then prints the new part of that log, the kami lines of
/tmp/kami.log, and whether kami exited. Drives the live panel: it paints on
/dev/fb0.
"""
import os, pty, subprocess, sys, time, select

host = sys.argv[1]
url = "http://example.com/"
keys = ["j", "ENTER", "TAB", "ENTER", "CTRL-Q"]
wait = 240
envs = []
args = sys.argv[2:]
while args:
    a = args.pop(0)
    if a == "--keys": keys = args.pop(0).split(",")
    elif a == "--wait": wait = int(args.pop(0))
    elif a == "--env": envs.append(args.pop(0))
    else: url = a
KEYS = {"ENTER": b"\r", "TAB": b"\t", "ESC": b"\x1b", "CTRL-Q": b"\x11", "CTRL-C": b"\x03", "UP": b"\x1b[A", "DOWN": b"\x1b[B"}

O = ["ssh", "-p", "2222", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
     "-o", "LogLevel=ERROR", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15", f"root@{host}"]

def remote(cmd, timeout=60):
    return subprocess.run(O + [cmd], capture_output=True, text=True, timeout=timeout).stdout

before = (remote("cat /tmp/kami-input.log 2>/dev/null | wc -c").split() or ["0"])[-1]
m, s = pty.openpty()
t0 = time.time()
p = subprocess.Popen(O[:1] + ["-tt"] + O[1:] + [" ".join(envs + [f"kami {url}"])], stdin=s, stdout=s, stderr=s, close_fds=True)
os.close(s)
out = b""

def drain(sec):
    global out
    end = time.time() + sec
    while time.time() < end:
        r, _, _ = select.select([m], [], [], 0.2)
        if r:
            try: out += os.read(m, 65536)
            except OSError: return

ready = False
while time.time() - t0 < wait and p.poll() is None:
    drain(3)
    log = remote(f"tail -c +{int(before) + 1} /tmp/kami-input.log 2>/dev/null")
    if "ready for frames" in log:
        ready = True
        print(f"[probe] ready after {time.time() - t0:.0f} s")
        break
print("[probe] kami alive:", p.poll() is None, "ready:", ready)
if ready and p.poll() is None:
    time.sleep(3)
    for k in keys:
        b = KEYS.get(k, k.encode())
        os.write(m, b)
        print(f"[probe] sent {k!r} -> {b!r}")
        drain(2)
        if p.poll() is not None:
            print("[probe] kami exited after", k)
            break
    drain(3)
print("[probe] kami exited:", p.poll() is not None, "rc:", p.poll())
print("---- tty output (tail) ----")
print(out.decode(errors="replace")[-1200:])
print("---- /tmp/kami-input.log (new) ----")
print(remote(f"tail -c +{int(before) + 1} /tmp/kami-input.log 2>/dev/null")[-6000:])
print("---- /tmp/kami.log (kami-daemon lines) ----")
print(remote("grep -a 'kami-daemon' /tmp/kami.log | tail -12"))
if p.poll() is None:
    p.kill()
