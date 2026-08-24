#!/usr/bin/env python3
"""Minimal ping-flood client that logs actual send()/recv() sizes and timing,
to see directly whether writes to Akuma's forwarded redis port go partial
under host load -- ground truth without needing tcpdump/dtrace (no sudo)."""
import socket, time, sys, threading

HOST, PORT = "127.0.0.1", int(sys.argv[1]) if len(sys.argv) > 1 else 4444
N_CLIENTS = 20
REQS_PER_CLIENT = 2000
PING = b"*1\r\n$4\r\nPING\r\n"

partial_writes = 0
total_writes = 0
errors = []
lock = threading.Lock()

def worker(idx):
    global partial_writes, total_writes
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.connect((HOST, PORT))
    s.setblocking(True)
    s.settimeout(10)
    try:
        for i in range(REQS_PER_CLIENT):
            n = s.send(PING)
            with lock:
                total_writes += 1
                if n != len(PING):
                    partial_writes += 1
            buf = b""
            while not buf.endswith(b"\r\n"):
                chunk = s.recv(4096)
                if not chunk:
                    raise RuntimeError("closed")
                buf += chunk
    except Exception as e:
        with lock:
            errors.append((idx, str(e)))
    finally:
        s.close()

t0 = time.time()
threads = [threading.Thread(target=worker, args=(i,)) for i in range(N_CLIENTS)]
for t in threads: t.start()
for t in threads: t.join()
elapsed = time.time() - t0

print(f"target={HOST}:{PORT} elapsed={elapsed:.2f}s total_writes={total_writes} "
      f"partial_writes={partial_writes} rps={total_writes/elapsed:.1f} errors={len(errors)}")
if errors[:5]:
    print("sample errors:", errors[:5])
