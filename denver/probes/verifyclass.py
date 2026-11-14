#!/usr/bin/env python3
"""verifyclass.py - strict read-back parser for mkhello.py's output (JVMS 4.x walk).

Why an independent parser: the host JRE SIGSEGVs during VM init (vsyscall death,
measured 2026-11-14 - the L1 datum applies to this Linux host too), so the class file
cannot be run-verified on the host. The REAL verifier is the guest's HotSpot, and a
malformed class yields a precise ClassFormatError there - but that costs a boot. This
parser catches builder bugs (wrong indices, truncated structures, trailing bytes)
before the guest ever boots. It verifies structure, not semantics: wrong-but-wellformed
opcodes are HotSpot's to name.

Usage: python3 verifyclass.py Hello.class   (exit 0 = structurally valid)
"""
import struct
import sys

TAG_NAMES = {1: "Utf8", 3: "Integer", 4: "Float", 5: "Long", 6: "Double",
             7: "Class", 8: "String", 9: "Fieldref", 10: "Methodref",
             11: "InterfaceMethodref", 12: "NameAndType", 15: "MethodHandle",
             16: "MethodType", 17: "Dynamic", 18: "InvokeDynamic", 19: "Module",
             20: "Package"}

errors = []


def err(what):
    errors.append(what)


class R:
    def __init__(self, b):
        self.b, self.o = b, 0

    def u1(self):
        v = self.b[self.o]
        self.o += 1
        return v

    def u2(self):
        v = struct.unpack_from(">H", self.b, self.o)[0]
        self.o += 2
        return v

    def u4(self):
        v = struct.unpack_from(">I", self.b, self.o)[0]
        self.o += 4
        return v

    def raw(self, n):
        if self.o + n > len(self.b):
            err("overrun: need %d bytes at offset %d, file is %d" % (n, self.o, len(self.b)))
            raise EOFError("overrun")
        v = self.b[self.o:self.o + n]
        self.o += n
        return v


def utf8_at(cp, i):
    tag, v = cp[i]
    return v if tag == 1 else None


def expect(cond, what):
    if not cond:
        err("MISMATCH: " + what)


def main():
    if len(sys.argv) != 2:
        print("usage: python3 verifyclass.py Hello.class", file=sys.stderr)
        return 2
    b = open(sys.argv[1], "rb").read()
    r = R(b)
    expect(r.u4() == 0xCAFEBABE, "magic")
    minor, major = r.u2(), r.u2()
    expect(major == 65, "major=%d want 65" % major)
    count = r.u2()
    cp = {0: (0, None)}
    i = 1
    while i < count:
        tag = r.u1()
        if tag == 1:
            n = r.u2()
            cp[i] = (tag, r.raw(n).decode("utf-8", "replace"))
        elif tag in (3, 4):
            cp[i] = (tag, r.raw(4))
        elif tag in (5, 6):
            cp[i] = (tag, r.raw(8))
            i += 1  # long/double take two slots
            cp[i] = (0, None)
        elif tag in (7, 8, 16, 19, 20):
            cp[i] = (tag, r.u2())
        elif tag in (9, 10, 11, 12, 17, 18):
            cp[i] = (tag, (r.u2(), r.u2()))
        elif tag == 15:
            cp[i] = (tag, (r.u1(), r.u2()))
        else:
            err("unknown tag %d at index %d" % (tag, i))
            return finish(b, r)
        i += 1
    expect(len(cp) - 1 == count - 1, "pool slot count")

    def cls_name(idx):
        tag, v = cp.get(idx, (0, None))
        expect(tag == 7, "index %d is %s want Class" % (idx, TAG_NAMES.get(tag, tag)))
        return utf8_at(cp, v[0] if isinstance(v, tuple) else v) if tag == 7 else None

    expect(cls_name(21) == "Hello", "this_class name")
    expect(cls_name(2) == "java/lang/Object", "super_class name")

    access, this, sup = r.u2(), r.u2(), r.u2()
    expect(access == 0x0021, "class access=0x%04x want 0x0021" % access)
    expect(this == 21 and sup == 2, "this/super indices")
    expect(r.u2() == 0, "fields count")

    nmethods = r.u2()
    expect(nmethods == 2, "methods count=%d want 2" % nmethods)
    seen = set()
    for _ in range(nmethods):
        fl, ni, di, na = r.u2(), r.u2(), r.u2(), r.u2()
        name, desc = utf8_at(cp, ni), utf8_at(cp, di)
        seen.add((name, desc))
        expect(na == 1, "method %s attr count=%d" % (name, na))
        for _ in range(na):
            ani, alen = r.u2(), r.u4()
            expect(utf8_at(cp, ani) == "Code", "attr name")
            ms, ml = r.u2(), r.u2()
            clen = r.u4()
            code = r.raw(clen)
            expect(r.u2() == 0, "exception table empty")
            expect(r.u2() == 0, "code attributes empty")
        if name == "<init>":
            expect(fl == 1 and desc == "()V", "<init> shape")
            expect(code == bytes([0x2A, 0xB7, 0, 1, 0xB1]), "<init> code %s" % code.hex())
            expect((ms, ml) == (1, 1), "<init> stack/locals")
        if name == "main":
            expect(fl == 0x0009, "main flags")
            expect(desc == "([Ljava/lang/String;)V", "main descriptor")
            expect(code == bytes([0xB2, 0, 7, 0x12, 13, 0xB6, 0, 15, 0xB1]),
                   "main code %s" % code.hex())
            expect((ms, ml) == (2, 1), "main stack/locals")
    expect(seen == {("<init>", "()V"), ("main", "([Ljava/lang/String;)V")},
           "methods present: %s" % seen)
    expect(r.u2() == 0, "class attributes empty")
    expect(r.o == len(b), "trailing bytes: %d of %d" % (r.o, len(b)))
    return finish(b, r)


def finish(b, r):
    if errors:
        print("VERIFY RED %s" % sys.argv[1])
        for e in errors:
            print("  " + e)
        return 1
    print("VERIFY PASS %s %d bytes, major %d, pool %d slots" % (
        sys.argv[1], len(b), 65, 26))
    return 0


if __name__ == "__main__":
    sys.exit(main())
