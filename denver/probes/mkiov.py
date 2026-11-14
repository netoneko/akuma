#!/usr/bin/env python3
"""mkiov.py - PHASE-3-JAVACLASSLIB attempt-1 bisect generator.

Attempt 1's datum: HotSpot's inferencing verifier (major 49 accepted!) rejected
ioCheck with `Expecting to find double on stack` while main/thrower/ioWrite verified
clean. These six variants separate ioCheck's shape into its pieces; all six are staged
into the image at once and run in ONE boot (one java run each, ~10 s per run). Each
variant prints IOVn-START (and END when it verifies+runs).

  IoV1 = exact replica of ioCheck's shape as main (streams + read + compare +
         ifeq + new/athrow mismatch path) - RED expected, replicates the datum
  IoV2 = IoV1 minus the branch: equals, pop, return
  IoV3 = no java.io: hand arrays only (sipush/newarray/astore + getBytes +
         equals + ifeq + new/athrow path)
  IoV4 = streams only: open/read/pop/close, no compare, no branch
  IoV5 = streams + equals + ifeq whose target is a bare return (merge-only test)
  IoV6 = IoV1 with ifne + swapped bodies (equals; ifne OK; <mis>; OK: return)

Usage: python3 mkiov.py <outdir>
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from mkworkload import (Pool, Asm, code_attr, method, emit_class,
                        ACC_PUBLIC, ACC_STATIC, ATYPE_BYTE)


def base(pool):
    """common symbols every variant needs; returns (ps_str, get_bytes, arr_eq, exc_init, out_field)"""
    pool.utf8("Code")
    pool.cls("java/lang/Object")
    out_field = pool.fieldref("java/lang/System", "out", "Ljava/io/PrintStream;")
    pool.cls("java/io/PrintStream")
    ps_str = pool.methodref("java/io/PrintStream", "println", "(Ljava/lang/String;)V")
    s_bytes = pool.methodref("java/lang/String", "getBytes", "()[B")
    pool.cls("java/lang/String")
    arr_eq = pool.methodref("java/util/Arrays", "equals", "([B[B)Z")
    pool.cls("java/util/Arrays")
    exc_init = pool.methodref("java/lang/Exception", "<init>", "(Ljava/lang/String;)V")
    pool.cls("java/lang/Exception")
    return ps_str, s_bytes, arr_eq, exc_init, out_field


def p_str(a, out_field, ps_str, s):
    a.op_cp2(0xB2, out_field)
    a.ldc(a.pool.string(s))
    a.op_cp2(0xB6, ps_str)


def streams_open(pool, a, path_s):
    fis_init = pool.methodref("java/io/FileInputStream", "<init>", "(Ljava/lang/String;)V")
    pool.cls("java/io/FileInputStream")
    a.op_cp2(0xBB, pool.d[("c", "java/io/FileInputStream")])
    a.op(0x59)
    a.ldc(a.pool.string(path_s))
    a.op_cp2(0xB7, fis_init)
    a.astore(1)


def streams_write(pool, a, path_s, data_s):
    fos_init = pool.methodref("java/io/FileOutputStream", "<init>", "(Ljava/lang/String;)V")
    fos_write = pool.methodref("java/io/FileOutputStream", "write", "([B)V")
    fos_close = pool.methodref("java/io/FileOutputStream", "close", "()V")
    pool.cls("java/io/FileOutputStream")
    a.op_cp2(0xBB, pool.d[("c", "java/io/FileOutputStream")])
    a.op(0x59)
    a.ldc(a.pool.string(path_s))
    a.op_cp2(0xB7, fos_init)
    a.astore(2)
    a.aload(2)
    a.ldc(a.pool.string(data_s))
    a.op_cp2(0xB6, s_bytes(pool))
    a.op_cp2(0xB6, fos_write)
    a.aload(2)
    a.op_cp2(0xB6, fos_close)


def s_bytes(pool):
    return pool.d[("m", "java/lang/String", "getBytes", "()[B")]


def exc_throw(pool, a, msg_s, exc_init):
    a.op_cp2(0xBB, pool.d[("c", "java/lang/Exception")])
    a.op(0x59)
    a.ldc(a.pool.string(msg_s))
    a.op_cp2(0xB7, exc_init)
    a.op(0xBF)


def build(name, body, extra_pool=None):
    p = Pool()
    ps_str, sb, arr_eq, exc_init, out_field = base(p)
    a = Asm(p)
    p_str(a, out_field, ps_str, name + "-START")
    body(p, a, dict(ps_str=ps_str, s_bytes=sb, arr_eq=arr_eq, exc_init=exc_init,
                    out_field=out_field))
    p_str(a, out_field, ps_str, name + "-END")
    a.op(0xB1)
    mattr = code_attr(p, a, 5, 6, [])
    ai = Asm(p)
    ai.aload(0)
    ai.op_cp2(0xB7, p.methodref("java/lang/Object", "<init>", "()V"))
    ai.op(0xB1)
    iattr = code_attr(p, ai, 1, 1, [])
    methods = [
        method(p, ACC_PUBLIC | ACC_STATIC, "main", "([Ljava/lang/String;)V", mattr),
        method(p, ACC_PUBLIC, "<init>", "()V", iattr),
    ]
    return emit_class(p, name, "java/lang/Object", [], [], methods)


def body_v1(p, a, t):
    """full ioCheck shape as main: write, then read-back + compare + branch + athrow"""
    streams_write(p, a, "/work/out.bin", "WORKLOAD-IO-DATA-42")
    streams_open(p, a, "/work/out.bin")
    a.sipush(64)
    a.op_u1(0xBC, ATYPE_BYTE)
    a.astore(2)
    a.aload(1)
    a.aload(2)
    a.op_cp2(0xB6, p.methodref("java/io/FileInputStream", "read", "([B)I"))
    a.op(0x57)
    a.aload(1)
    a.op_cp2(0xB6, p.methodref("java/io/FileInputStream", "close", "()V"))
    a.ldc(a.pool.string("WORKLOAD-IO-DATA-42"))
    a.op_cp2(0xB6, t["s_bytes"])
    a.astore(3)
    a.aload(2)
    a.aload(3)
    a.op_cp2(0xB8, t["arr_eq"])
    a.branch(0x99, "mis")
    a.op(0xB1)
    a.label("mis")
    exc_throw(p, a, "IOV1-MISMATCH", t["exc_init"])


def body_v2(p, a, t):
    """V1 minus the branch: compare then pop the boolean, return"""
    streams_write(p, a, "/work/out.bin", "WORKLOAD-IO-DATA-42")
    streams_open(p, a, "/work/out.bin")
    a.sipush(64)
    a.op_u1(0xBC, ATYPE_BYTE)
    a.astore(2)
    a.aload(1)
    a.aload(2)
    a.op_cp2(0xB6, p.methodref("java/io/FileInputStream", "read", "([B)I"))
    a.op(0x57)
    a.aload(1)
    a.op_cp2(0xB6, p.methodref("java/io/FileInputStream", "close", "()V"))
    a.ldc(a.pool.string("WORKLOAD-IO-DATA-42"))
    a.op_cp2(0xB6, t["s_bytes"])
    a.astore(3)
    a.aload(2)
    a.aload(3)
    a.op_cp2(0xB8, t["arr_eq"])
    a.op(0x57)  # pop the boolean - no branch anywhere


def body_v3(p, a, t):
    """no java.io at all: sipush/newarray + getBytes + compare + branch + athrow"""
    a.ldc(a.pool.string("WORKLOAD-IO-DATA-42"))
    a.op_cp2(0xB6, t["s_bytes"])
    a.astore(3)
    a.sipush(64)
    a.op_u1(0xBC, ATYPE_BYTE)
    a.astore(2)
    a.aload(2)
    a.aload(3)
    a.op_cp2(0xB8, t["arr_eq"])
    a.branch(0x99, "mis")
    a.op(0xB1)
    a.label("mis")
    exc_throw(p, a, "IOV3-MISMATCH", t["exc_init"])


def body_v4(p, a, t):
    """streams only: write, open, read, pop, close - no compare, no branch, no athrow"""
    streams_write(p, a, "/work/out.bin", "WORKLOAD-IO-DATA-42")
    streams_open(p, a, "/work/out.bin")
    a.sipush(64)
    a.op_u1(0xBC, ATYPE_BYTE)
    a.astore(2)
    a.aload(1)
    a.aload(2)
    a.op_cp2(0xB6, p.methodref("java/io/FileInputStream", "read", "([B)I"))
    a.op(0x57)
    a.aload(1)
    a.op_cp2(0xB6, p.methodref("java/io/FileInputStream", "close", "()V"))


def body_v5(p, a, t):
    """streams + compare + ifeq whose target is a bare return (merge-only)"""
    streams_write(p, a, "/work/out.bin", "WORKLOAD-IO-DATA-42")
    streams_open(p, a, "/work/out.bin")
    a.sipush(64)
    a.op_u1(0xBC, ATYPE_BYTE)
    a.astore(2)
    a.aload(1)
    a.aload(2)
    a.op_cp2(0xB6, p.methodref("java/io/FileInputStream", "read", "([B)I"))
    a.op(0x57)
    a.aload(1)
    a.op_cp2(0xB6, p.methodref("java/io/FileInputStream", "close", "()V"))
    a.ldc(a.pool.string("WORKLOAD-IO-DATA-42"))
    a.op_cp2(0xB6, t["s_bytes"])
    a.astore(3)
    a.aload(2)
    a.aload(3)
    a.op_cp2(0xB8, t["arr_eq"])
    a.branch(0x99, "t")   # ifeq -> bare return
    a.op(0xB1)
    a.label("t")
    a.op(0xB1)


def body_v6(p, a, t):
    """V1 with ifne + swapped bodies"""
    streams_write(p, a, "/work/out.bin", "WORKLOAD-IO-DATA-42")
    streams_open(p, a, "/work/out.bin")
    a.sipush(64)
    a.op_u1(0xBC, ATYPE_BYTE)
    a.astore(2)
    a.aload(1)
    a.aload(2)
    a.op_cp2(0xB6, p.methodref("java/io/FileInputStream", "read", "([B)I"))
    a.op(0x57)
    a.aload(1)
    a.op_cp2(0xB6, p.methodref("java/io/FileInputStream", "close", "()V"))
    a.ldc(a.pool.string("WORKLOAD-IO-DATA-42"))
    a.op_cp2(0xB6, t["s_bytes"])
    a.astore(3)
    a.aload(2)
    a.aload(3)
    a.op_cp2(0xB8, t["arr_eq"])
    a.branch(0x9A, "ok")  # ifne -> ok
    exc_throw(p, a, "IOV6-MISMATCH", t["exc_init"])
    a.label("ok")
    a.op(0xB1)


def main():
    if len(sys.argv) != 2:
        print("usage: python3 mkiov.py <outdir>", file=sys.stderr)
        return 2
    outdir = sys.argv[1]
    os.makedirs(outdir, exist_ok=True)
    variants = [("IoV1", body_v1), ("IoV2", body_v2), ("IoV3", body_v3),
                ("IoV4", body_v4), ("IoV5", body_v5), ("IoV6", body_v6)]
    for name, body in variants:
        blob = build(name, body)
        path = os.path.join(outdir, name + ".class")
        with open(path, "wb") as f:
            f.write(blob)
        print("WROTE %s %d bytes" % (path, len(blob)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
