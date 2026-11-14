#!/usr/bin/env python3
"""dumpworkload.py - readable disassembly of the built classes for the gate record.
Structurally INDEPENDENT of both mkworkload.py and verifyworkload.py (raw struct
walk, its own tables) - a third pair of eyes on the emitted bytes before the boot."""
import struct
import sys

MNEMONIC = {0x00: "nop", 0x01: "aconst_null", 0x03: "iconst_0", 0x10: "bipush",
            0x11: "sipush", 0x12: "ldc", 0x15: "iload(w)", 0x19: "aload(w)",
            0x1A: "iload_0", 0x1B: "iload_1", 0x1C: "iload_2", 0x1D: "iload_3",
            0x2A: "aload_0", 0x2B: "aload_1", 0x2C: "aload_2", 0x2D: "aload_3",
            0x33: "baload", 0x36: "istore(w)", 0x3B: "istore_0", 0x3C: "istore_1",
            0x3D: "istore_2", 0x3E: "istore_3", 0x4A: "astore(w)", 0x4B: "astore_0",
            0x4C: "astore_1", 0x4D: "astore_2", 0x4E: "astore_3", 0x57: "pop",
            0x59: "dup", 0x68: "imul", 0x84: "iinc", 0x99: "ifeq",
            0xA1: "if_icmplt", 0xA7: "goto", 0xB1: "return", 0xB2: "getstatic",
            0xB3: "putstatic", 0xB6: "invokevirtual", 0xB7: "invokespecial",
            0xB8: "invokestatic", 0xBB: "new", 0xBC: "newarray-byte", 0xBE: "arraylength",
            0xBF: "athrow"}
SZ = {0x10: 1, 0x11: 2, 0x12: 1, 0x15: 1, 0x19: 1, 0x36: 1, 0x4A: 1, 0x84: 2,
      0x99: 2, 0xA1: 2, 0xA7: 2, 0xB2: 2, 0xB3: 2, 0xB4: 2, 0xB5: 2, 0xB6: 2,
      0xB7: 2, 0xB8: 2, 0xBB: 2, 0xBC: 1}


def dump(path):
    b = open(path, "rb").read()
    o = 10
    count = struct.unpack_from(">H", b, 8)[0]
    cp = {}
    i = 1
    while i < count:
        tag = b[o]; o += 1
        if tag == 1:
            n = struct.unpack_from(">H", b, o)[0]
            cp[i] = ("u", b[o + 2:o + 2 + n].decode("utf-8", "replace")); o += 2 + n
        elif tag == 3:
            cp[i] = ("i", struct.unpack_from(">i", b, o)[0]); o += 4
        elif tag == 8:
            cp[i] = ("s", struct.unpack_from(">H", b, o)[0]); o += 2
        elif tag == 7:
            cp[i] = ("c", struct.unpack_from(">H", b, o)[0]); o += 2
        elif tag in (9, 10, 12):
            cp[i] = (tag,) + struct.unpack_from(">HH", b, o); o += 4
        else:
            print("unhandled tag %d" % tag); return
        i += 1

    def S(idx, depth=0):
        v = cp.get(idx, (0, "?%d" % idx))
        t = v[0]
        if depth > 4:
            return "..."
        if t == "u":
            return v[1]
        if t == "i":
            return str(v[1])
        if t == "s":
            return '"%s"' % S(v[1], depth + 1)
        if t == "c":
            return S(v[1], depth + 1)
        if t == 12:
            return "%s:%s" % (S(v[1], depth + 1), S(v[2], depth + 1))
        if t in (9, 10):
            return "%s.%s" % (S(v[1], depth + 1), S(v[2], depth + 1))
        return "?"

    major = struct.unpack_from(">H", b, 6)[0]
    print("== %s major=%d pool=%d" % (path, major, count - 1))
    flags, this, sup, nif = struct.unpack_from(">HHHH", b, o)
    ifaces = [struct.unpack_from(">H", b, o + 8 + 2 * k)[0] for k in range(nif)]
    o += 8 + 2 * nif
    print("   class 0x%04x this=%s super=%s ifaces=[%s]"
          % (flags, S(this), S(sup), ", ".join(S(i) for i in ifaces)))
    nf = struct.unpack_from(">H", b, o)[0]; o += 2
    for _ in range(nf):
        ff, fn, fd, na = struct.unpack_from(">HHHH", b, o); o += 8
        print("   field 0x%04x %s : %s" % (ff, S(fn), S(fd)))
    nm = struct.unpack_from(">H", b, o)[0]; o += 2
    for _ in range(nm):
        mf, mn, md, na = struct.unpack_from(">HHHH", b, o); o += 8
        print("== method 0x%04x %s %s" % (mf, S(mn), S(md)))
        for _ in range(na):
            ani, alen = struct.unpack_from(">HI", b, o); o += 6
            end = o + alen
            if S(ani) == "Code":
                ms, ml, cl = struct.unpack_from(">HHI", b, o); o += 8
                code = b[o:o + cl]; o += cl
                ec = struct.unpack_from(">H", b, o)[0]; o += 2
                for _ in range(ec):
                    s, e, h, ct = struct.unpack_from(">HHHH", b, o); o += 8
                    print("   handler [%d,%d) -> %d catch %s" % (s, e, h, S(ct) if ct else "<any>"))
                pc = 0
                while pc < len(code):
                    op = code[pc]
                    m = MNEMONIC.get(op, hex(op))
                    if op in (0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xBB):
                        print("  %4d %-13s %s" % (pc, m, S(struct.unpack_from(">H", code, pc + 1)[0])))
                        pc += 3
                    elif op == 0x12:
                        print("  %4d %-13s %s" % (pc, "ldc", S(code[pc + 1])))
                        pc += 2
                    elif op in (0x99, 0xA1, 0xA7):
                        off = struct.unpack_from(">h", code, pc + 1)[0]
                        print("  %4d %-13s -> %d" % (pc, m, pc + off))
                        pc += 3
                    elif op == 0x84:
                        print("  %4d iinc l%d,%+d" % (pc, code[pc + 1], struct.unpack_from(">b", code, pc + 2)[0]))
                        pc += 3
                    elif op in (0x10, 0x11):
                        fmt = ">b" if op == 0x10 else ">h"
                        print("  %4d %-13s %d" % (pc, m, struct.unpack_from(fmt, code, pc + 1)[0]))
                        pc += 2 if op == 0x10 else 3
                    elif op in (0x15, 0x19, 0x36, 0x4A):
                        print("  %4d %-13s l%d" % (pc, m, code[pc + 1]))
                        pc += 2
                    else:
                        print("  %4d %s" % (pc, m))
                        pc += 1
            o = end
    print("   (%d bytes)" % len(b))


for p in sys.argv[1:]:
    dump(p)
