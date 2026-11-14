#!/usr/bin/env python3
"""mkhello.py - emit a minimal, valid Java 21 class file WITHOUT javac.

Why: denver/docs/research/PHASE-3-JAVAWORKLOAD.md D64 - no javac exists anywhere in
the lineage (host and guest are both JRE-only; JEP 330 source mode measured dead).
This emits the bytecode for:

    public class Hello {
        public Hello() { super(); }
        public static void main(String[] a) {
            System.out.println("HELLO-DENVER-WORKLOAD-PASS");
        }
    }

The <init> is kept even though static main never runs it: javac always emits one and a
missing <init> is a variable we do not need on the guest's first compiled-class run.

Verify BEFORE use (parse-time verdicts-lie discipline): the host JRE must run the
result - `java -cp <dir> Hello` must print the marker. A host that cannot run the
bytes has no business booting them.

Usage: python3 mkhello.py /path/to/Hello.class
"""
import struct
import sys

MARKER = "HELLO-DENVER-WORKLOAD-PASS"


def u1(x):
    return struct.pack(">B", x)


def u2(x):
    return struct.pack(">H", x)


def u4(x):
    return struct.pack(">I", x)


def utf8(s):
    b = s.encode()
    assert len(b) <= 0xFFFF
    return u1(1) + u2(len(b)) + b


def cls(name_idx):
    return u1(7) + u2(name_idx)


def string_idx(utf8_idx):
    return u1(8) + u2(utf8_idx)


def nat(name_idx, desc_idx):
    return u1(12) + u2(name_idx) + u2(desc_idx)


def fieldref(class_idx, nt_idx):
    return u1(9) + u2(class_idx) + u2(nt_idx)


def methodref(class_idx, nt_idx):
    return u1(10) + u2(class_idx) + u2(nt_idx)


# Constant pool, indices hand-laid and asserted at the end (the pool is the one place
# a silent off-by-one produces a class that loads nowhere).
cp = [None]
cp.append(methodref(2, 3))                       # 1  Object.<init>:()V
cp.append(cls(4))                                # 2  java/lang/Object
cp.append(nat(5, 6))                             # 3  <init>:()V
cp.append(utf8("java/lang/Object"))              # 4
cp.append(utf8("<init>"))                        # 5
cp.append(utf8("()V"))                           # 6
cp.append(fieldref(8, 9))                        # 7  System.out:Ljava/io/PrintStream;
cp.append(cls(10))                               # 8  java/lang/System
cp.append(nat(11, 12))                           # 9  out:Ljava/io/PrintStream;
cp.append(utf8("java/lang/System"))              # 10
cp.append(utf8("out"))                           # 11
cp.append(utf8("Ljava/io/PrintStream;"))         # 12
cp.append(string_idx(14))                        # 13 "HELLO-DENVER-WORKLOAD-PASS"
cp.append(utf8(MARKER))                          # 14
cp.append(methodref(16, 17))                     # 15 PrintStream.println:(Ljava/lang/String;)V
cp.append(cls(18))                               # 16 java/io/PrintStream
cp.append(nat(19, 20))                           # 17 println:(Ljava/lang/String;)V
cp.append(utf8("java/io/PrintStream"))           # 18
cp.append(utf8("println"))                       # 19
cp.append(utf8("(Ljava/lang/String;)V"))         # 20
cp.append(cls(22))                               # 21 Hello
cp.append(utf8("Hello"))                         # 22
cp.append(utf8("[Ljava/lang/String;"))           # 23
cp.append(utf8("main"))                          # 24
cp.append(utf8("([Ljava/lang/String;)V"))        # 25
cp.append(utf8("Code"))                          # 26
assert len(cp) == 27, "constant pool layout drifted: %d slots" % (len(cp) - 1)

ACC_PUBLIC, ACC_STATIC, ACC_SUPER = 0x0001, 0x0008, 0x0020


def code_attr(max_stack, max_locals, code):
    # Code_attribute: max_stack u2, max_locals u2, code_length u4, code,
    # exception_table_length u2, attributes_count u2 (JVMS 4.7.3)
    body = u2(max_stack) + u2(max_locals) + u4(len(code)) + code + u2(0) + u2(0)
    return u2(26) + u4(len(body)) + body


def method(flags, name_idx, desc_idx, attr):
    return u2(flags) + u2(name_idx) + u2(desc_idx) + u2(1) + attr


def main():
    if len(sys.argv) != 2:
        print("usage: python3 mkhello.py Hello.class", file=sys.stderr)
        return 2
    out = u4(0xCAFEBABE) + u2(0) + u2(65) + u2(len(cp))  # Java 21 = major 65
    out += b"".join(cp[1:])
    out += u2(ACC_PUBLIC | ACC_SUPER) + u2(21) + u2(2)  # this=Hello, super=Object
    out += u2(0)                             # interfaces_count (JVMS 4.1 - OMITTING
    # this u2 is fatal: HotSpot reads fields_count as interfaces_count, then parses the
    # methods table as fields -> ClassFormatError: Field "<init>" illegal signature
    # "()V" (attempt 1's measured datum). The interfaces table must be present, empty.
    out += u2(0)  # fields
    out += u2(2)  # methods
    out += method(ACC_PUBLIC, 5, 6, code_attr(1, 1, u1(0x2A) + u1(0xB7) + u2(1) + u1(0xB1)))
    # main: getstatic #7; ldc #13; invokevirtual #15; return
    # (ldc's operand is ONE byte - CP index 13 fits; a u2 here shifts the stream and
    # decodes as ldc #0, the bug verifyclass.py caught on first emission)
    out += method(ACC_PUBLIC | ACC_STATIC, 24, 25,
                  code_attr(2, 1, u1(0xB2) + u2(7) + u1(0x12) + u1(13)
                            + u1(0xB6) + u2(15) + u1(0xB1)))
    out += u2(0)  # class attributes
    with open(sys.argv[1], "wb") as f:
        f.write(out)
    print("WROTE %s %d bytes" % (sys.argv[1], len(out)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
