#!/usr/bin/env python3
"""Summarise an `strace -f` log of chromium into an Akuma gap list."""
import collections, re, sys

log = open(sys.argv[1], errors="replace").read().splitlines()
call_re = re.compile(r"^(\d+)\s+(?:<\.\.\. )?([a-z0-9_]+)[( ]")
calls = collections.Counter()
fails = collections.Counter()
ioctls = collections.Counter()
paths = collections.Counter()
big_mmaps = []
clones = collections.Counter()
sockets = collections.Counter()
prctls = collections.Counter()
pids = set()

for line in log:
    m = call_re.match(line)
    if not m or "resumed>" in line and "<..." in line:
        continue
    pid, name = m.groups()
    pids.add(pid)
    calls[name] += 1
    em = re.search(r"= -1 (E[A-Z]+)", line)
    if em:
        fails[(name, em.group(1))] += 1
    if name == "ioctl":
        im = re.match(r"\S+\s+ioctl\(\d+[^,]*, ([A-Z0-9_]+|0x[0-9a-f]+)", line)
        if im:
            ioctls[im.group(1)] += 1
    if name in ("openat", "open", "statx", "newfstatat", "access", "faccessat2", "readlink", "readlinkat"):
        pm = re.search(r'"(/(?:proc|sys|dev|run|tmp|etc)[^"]*)"', line)
        if pm:
            p = re.sub(r"/proc/\d+", "/proc/<pid>", pm.group(1))
            p = re.sub(r"/task/\d+", "/task/<tid>", p)
            ok = "ok" if not em else em.group(1)
            paths[(p, ok)] += 1
    if name == "mmap":
        mm = re.match(r"\S+\s+mmap\((\S+), (\d+), ([A-Z_|]+), ([A-Z_|]+)", line)
        if mm and int(mm.group(2)) >= 1 << 30:
            big_mmaps.append((int(mm.group(2)), mm.group(3), mm.group(4)))
    if name in ("clone", "clone3"):
        fm = re.search(r"flags=([A-Z_|]+)", line)
        clones[fm.group(1) if fm else "?"] += 1
    if name in ("socket", "socketpair"):
        sm = re.search(r"\((AF_[A-Z0-9]+), ([A-Z_|]+)", line)
        if sm:
            sockets[(name, sm.group(1), sm.group(2))] += 1
    if name == "prctl":
        pm2 = re.match(r"\S+\s+prctl\(([A-Z_]+)", line)
        if pm2:
            prctls[pm2.group(1)] += 1

scm = sum(1 for l in log if "SCM_RIGHTS" in l)
print(f"processes/threads seen: {len(pids)}   lines: {len(log)}   SCM_RIGHTS msgs: {scm}\n")
print("== syscalls (count) ==")
for k, v in sorted(calls.items(), key=lambda kv: -kv[1]):
    print(f"  {k:24} {v}")
print("\n== failures (syscall, errno) ==")
for (k, e), v in fails.most_common(40):
    print(f"  {k:20} {e:12} {v}")
print("\n== ioctls ==")
for k, v in ioctls.most_common():
    print(f"  {k:28} {v}")
print("\n== clone flags ==")
for k, v in clones.most_common():
    print(f"  {v:5}  {k}")
print("\n== sockets ==")
for k, v in sockets.most_common():
    print(f"  {v:5}  {k}")
print("\n== prctl ==")
for k, v in prctls.most_common():
    print(f"  {k:28} {v}")
print("\n== mmaps >= 1 GiB ==")
for size, prot, flags in sorted(set(big_mmaps), reverse=True):
    print(f"  {size / (1 << 30):10.1f} GiB  {prot:28} {flags}")
print("\n== /proc /sys /dev /run /tmp /etc paths ==")
for (p, ok), v in sorted(paths.items()):
    print(f"  {ok:8} {v:5}  {p}")
