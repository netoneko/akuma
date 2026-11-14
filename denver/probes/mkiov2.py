#!/usr/bin/env python3
"""mkiov2.py - PHASE-3-JAVACLASSLIB bisect round 2 + the design-fix candidate.

Round 1 verdicts: NO variant reproduced attempt 1's VerifyError; the branch+compare
machinery verifies and RUNS (IoV1/3/6 threw IOV*-MISMATCH because a 64-byte zero
buffer is never Arrays.equals-equal to a 19-byte expected array - a design bug, not a
verifier datum). The VerifyError delta is therefore: ioCheck is a SEPARATE STATIC
METHOD that uses the indexed local forms (astore 0x4A+l4 / aload 0x19+l4).

Round 2 (one boot, one java run each):
  IoV7  = exact static-method replica of Workload.ioCheck (params in locals 0-1,
          indexed l4 forms)  -> reproduces the VerifyError?
  IoV8  = same logic, all _n forms, expected-bytes computed inline on the stack
          (no local 4)     -> isolation of the indexed forms
  IoV9  = minimal indexed-l4 static method (sipush/newarray/astore l4/aload l4/pop)
  IoV10 = the same minimal shape inline in MAIN (isolate method-ness vs index-ness)
  IoV11 = the DESIGN FIX: buf sized at runtime via data.length()I, full compare,
          ifeq + new/athrow mismatch path (static method)

Usage: python3 mkiov2.py <outdir>
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from mkworkload import (Pool, Asm, code_attr, method, emit_class,
                        ACC_PUBLIC, ACC_STATIC, ATYPE_BYTE)
from mkiov import base, p_str, exc_throw

PATH_S = "/work/out.bin"
DATA_S = "WORKLOAD-IO-DATA-42"


def streams_open(pool, a):
    fis_init = pool.methodref("java/io/FileInputStream", "<init>", "(Ljava/lang/String;)V")
    pool.cls("java/io/FileInputStream")
    a.op_cp2(0xBB, pool.d[("c", "java/io/FileInputStream")])
    a.op(0x59)
    a.ldc(a.pool.string(PATH_S))
    a.op_cp2(0xB7, fis_init)


def write_streams(pool, a, t):
    fos_init = pool.methodref("java/io/FileOutputStream", "<init>", "(Ljava/lang/String;)V")
    fos_write = pool.methodref("java/io/FileOutputStream", "write", "([B)V")
    fos_close = pool.methodref("java/io/FileOutputStream", "close", "()V")
    pool.cls("java/io/FileOutputStream")
    a.op_cp2(0xBB, pool.d[("c", "java/io/FileOutputStream")])
    a.op(0x59)
    a.ldc(a.pool.string(PATH_S))
    a.op_cp2(0xB7, fos_init)
    a.astore(2)
    a.aload(2)
    a.ldc(a.pool.string(DATA_S))
    a.op_cp2(0xB6, t["s_bytes"])
    a.op_cp2(0xB6, fos_write)
    a.aload(2)
    a.op_cp2(0xB6, fos_close)


def check_v7(p, a, t):
    """EXACT replica of Workload.ioCheck: static (String,String)V, indexed l4 forms"""
    streams_open(p, a)
    a.astore(2)
    a.sipush(64)
    a.op_u1(0xBC, ATYPE_BYTE)
    a.astore(3)
    a.aload(2)
    a.aload(3)
    a.op_cp2(0xB6, p.methodref("java/io/FileInputStream", "read", "([B)I"))
    a.op(0x57)
    a.aload(2)
    a.op_cp2(0xB6, p.methodref("java/io/FileInputStream", "close", "()V"))
    a.aload(1)
    a.op_cp2(0xB6, t["s_bytes"])
    a.op_u1(0x4A, 4)          # astore l4 (indexed)
    a.aload(3)
    a.op_u1(0x19, 4)          # aload l4 (indexed)
    a.op_cp2(0xB8, t["arr_eq"])
    a.branch(0x99, "mis")
    a.op(0xB1)
    a.label("mis")
    exc_throw(p, a, "IOV7-MISMATCH", t["exc_init"])


def check_v8(p, a, t):
    """same logic, NO indexed forms, expected stays on the stack"""
    streams_open(p, a)
    a.astore(2)
    a.sipush(64)
    a.op_u1(0xBC, ATYPE_BYTE)
    a.astore(3)
    a.aload(2)
    a.aload(3)
    a.op_cp2(0xB6, p.methodref("java/io/FileInputStream", "read", "([B)I"))
    a.op(0x57)
    a.aload(2)
    a.op_cp2(0xB6, p.methodref("java/io/FileInputStream", "close", "()V"))
    a.aload(3)
    a.aload(1)
    a.op_cp2(0xB6, t["s_bytes"])
    a.op_cp2(0xB8, t["arr_eq"])
    a.branch(0x99, "mis")
    a.op(0xB1)
    a.label("mis")
    exc_throw(p, a, "IOV8-MISMATCH", t["exc_init"])


def minimal_indexed(p, a, t):
    a.sipush(64)
    a.op_u1(0xBC, ATYPE_BYTE)
    a.op_u1(0x4A, 4)          # astore l4
    a.op_u1(0x19, 4)          # aload l4
    a.op(0x57)                # pop
    a.op(0xB1)                # return


def check_v11(p, a, t):
    """THE DESIGN FIX: buf sized at runtime to the expected length; full compare"""
    streams_open(p, a)
    a.astore(2)
    a.aload(1)                                 # data (the expected string)
    a.op_cp2(0xB6, p.methodref("java/lang/String", "length", "()I"))
    a.op_u1(0xBC, ATYPE_BYTE)                  # newarray byte[data.length()]
    a.astore(3)                                # buf - exactly expected-sized
    a.aload(2)
    a.aload(3)
    a.op_cp2(0xB6, p.methodref("java/io/FileInputStream", "read", "([B)I"))
    a.op(0x57)
    a.aload(2)
    a.op_cp2(0xB6, p.methodref("java/io/FileInputStream", "close", "()V"))
    a.aload(3)
    a.aload(1)
    a.op_cp2(0xB6, t["s_bytes"])
    a.op_cp2(0xB8, t["arr_eq"])
    a.branch(0x99, "mis")
    a.op(0xB1)
    a.label("mis")
    exc_throw(p, a, "IOV11-MISMATCH", t["exc_init"])


def build(name, main_body, extra_methods=()):
    p = Pool()
    t = {}
    t["ps_str"], t["s_bytes"], t["arr_eq"], t["exc_init"], t["out_field"] = base(p)
    methods = []
    if main_body is not None:
        a = Asm(p)
        p_str(a, t["out_field"], t["ps_str"], name + "-START")
        main_body(p, a, t)
        p_str(a, t["out_field"], t["ps_str"], name + "-END")
        a.op(0xB1)
        mattr = code_attr(p, a, 6, 7, [])
        methods.append(method(p, ACC_PUBLIC | ACC_STATIC, "main", "([Ljava/lang/String;)V", mattr))
    for mname, mdesc, mbody in extra_methods:
        am = Asm(p)
        mbody(p, am, t)
        am.op(0xB1)
        attr = code_attr(p, am, 6, 6, [])
        methods.append(method(p, ACC_PUBLIC | ACC_STATIC, mname, mdesc, attr))
    ai = Asm(p)
    ai.aload(0)
    ai.op_cp2(0xB7, p.methodref("java/lang/Object", "<init>", "()V"))
    ai.op(0xB1)
    iattr = code_attr(p, ai, 1, 1, [])
    methods.append(method(p, ACC_PUBLIC, "<init>", "()V", iattr))
    return emit_class(p, name, "java/lang/Object", [], [], methods)


def main():
    if len(sys.argv) != 2:
        print("usage: python3 mkiov2.py <outdir>", file=sys.stderr)
        return 2
    outdir = sys.argv[1]
    os.makedirs(outdir, exist_ok=True)

    def call_check(name, desc):
        def body(p, a, t):
            a.ldc(a.pool.string(PATH_S))
            a.ldc(a.pool.string(DATA_S))
            a.op_cp2(0xB8, a.pool.methodref(name, "check", desc))
        return body

    jobs = [
        # IoV7: main -> invokestatic check (exact ioCheck replica, indexed l4)
        ("IoV7", build("IoV7", call_check("IoV7", "(Ljava/lang/String;Ljava/lang/String;)V"),
                       [("check", "(Ljava/lang/String;Ljava/lang/String;)V", check_v7)])),
        # IoV8: main -> invokestatic check (no indexed forms)
        ("IoV8", build("IoV8", call_check("IoV8", "(Ljava/lang/String;Ljava/lang/String;)V"),
                       [("check", "(Ljava/lang/String;Ljava/lang/String;)V", check_v8)])),
        # IoV9: main -> invokestatic m9 (minimal indexed l4 in a METHOD)
        ("IoV9", build("IoV9", call_check("IoV9", "(Ljava/lang/String;)V"),
                       [("m9", "(Ljava/lang/String;)V", minimal_indexed)])),
        # IoV10: minimal indexed l4 inline in MAIN
        ("IoV10", build("IoV10", minimal_indexed)),
        # IoV11: the design fix as a static check method
        ("IoV11", build("IoV11", call_check("IoV11", "(Ljava/lang/String;Ljava/lang/String;)V"),
                        [("check", "(Ljava/lang/String;Ljava/lang/String;)V", check_v11)])),
    ]
    for name, blob in jobs:
        path = os.path.join(outdir, name + ".class")
        with open(path, "wb") as f:
            f.write(blob)
        print("WROTE %s %d bytes" % (path, len(blob)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
