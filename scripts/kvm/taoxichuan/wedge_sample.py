#!/usr/bin/env python3 -I
"""wedge_sample.py N SYMS [SAMPLES]: run ON THE RYZEN against a wedged build_run.py guest (slot N, monitor /root/mon<N>).

SYMS is `llvm-nm -C -n KERNEL` output copied to the ryzen. Prints, per sample: the BKL ticket counters (BKL_SERVING / BKL_NEXT), the owner core, every core's RIP
symbolized, and the owner's stack symbolized. SERVING advancing means a convoy, not a deadlock (docs/archive/TAOXICHUAN_J4_BKL_CONVOY.md).
Symbol addresses for the three statics are looked up in SYMS (they are `b`/`d` symbols, so pass nm output unfiltered)."""
import bisect, re, socket, sys, time

N, SYMF = sys.argv[1], sys.argv[2]
SAMPLES = int(sys.argv[3]) if len(sys.argv) > 3 else 5
text, data = [], {}
for l in open(SYMF):
    p = l.split(None, 2)
    if len(p) != 3:
        continue
    try:
        a = int(p[0], 16)
    except ValueError:
        continue
    if p[1] in "tTwW":
        text.append((a, p[2].strip()))
    for k in ("BKL_SERVING", "BKL_NEXT", "BKL_OWNER"):
        if p[2].strip().endswith("smp::" + k) or p[2].strip().startswith("taoxichuan::smp::" + k):
            data[k] = a
text.sort()
addrs = [a for a, _ in text]


def sym(a):
    if not text or a < addrs[0] or a > addrs[-1] + 0x10000:
        return None
    i = bisect.bisect_right(addrs, a) - 1
    return f"{text[i][1][:70]}+{hex(a - text[i][0])}"


s = socket.socket(socket.AF_UNIX)
s.connect(f"/root/mon{N}")
s.settimeout(1.5)


def cmd(c, w=1.5):
    s.sendall((c + "\n").encode())
    time.sleep(w)
    out = b""
    try:
        while True:
            d = s.recv(65536)
            if not d:
                break
            out += d
    except OSError:
        pass
    return re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", out.decode(errors="replace")).replace("\r", "")


def peek(addr, fmt):
    m = re.findall(r"^[0-9a-f]+:\s+0x([0-9a-f]+)", cmd(f"x/{fmt} 0x{addr:x}"), re.M)
    return int(m[-1], 16) if m else None


cmd("")
for k in range(SAMPLES):
    serving, nxt = peek(data["BKL_SERVING"], "1wx"), peek(data["BKL_NEXT"], "1wx")
    owner = peek(data["BKL_OWNER"], "1gx")
    print(f"--- sample {k} t={int(time.time())} serving={serving} next={nxt} owner={owner}", flush=True)
    regs = cmd("info registers -a", 3)
    for blk in regs.split("CPU#")[1:]:
        rip = re.search(r"RIP=([0-9a-f]+)", blk)
        print(f"  cpu{blk.split()[0]}: {sym(int(rip.group(1), 16)) if rip else '?'}")
    if owner is not None and 0 <= owner < 16:
        cmd(f"cpu {owner}")
        r = cmd("info registers")
        rsp = re.search(r"RSP=([0-9a-f]+)", r)
        if rsp:
            seen = []
            for v in re.findall(r"0x([0-9a-f]{16})", cmd(f"x/160gx 0x{rsp.group(1)}", 3)):
                x = sym(int(v, 16))
                if x and (not seen or seen[-1] != x):
                    seen.append(x)
            print("  owner stack (stale words included):")
            for x in seen[:20]:
                print("    ", x)
    time.sleep(5)
