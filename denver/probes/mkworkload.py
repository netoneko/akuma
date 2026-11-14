#!/usr/bin/env python3
"""mkworkload.py - the PHASE-3-JAVACLASSLIB workload assembler: emits TWO class files
WITHOUT javac (no javac exists anywhere in the lineage - PHASE-3-JAVAWORKLOAD.md D64;
the R9 sweep found no bytecode precedent before mkhello.py).

Emits (the four surfaces of REQUIREMENTS.md G12, ordered as main runs them so a death
leaves the earlier markers already captured):

  Workload.class - main():
      1. exceptions  : thrower() builds + athrows java/lang/Exception from a separate
                       frame; main's try/catch (a real exception_table) unwinds it back;
                       main prints getMessage() then WORKLOAD-EXC-PASS.
      2. file I/O    : FileOutputStream("/work/out.bin").write(data); close();
                       FileInputStream().read([B); close(); Arrays.equals compare;
                       mismatch athrows Exception("WORKLOAD-IO-MISMATCH ...");
                       pass prints WORKLOAD-IO-PASS.
      3. allocation  : Runtime.freeMemory() printed, 1000 x 128 KiB byte[] churn
                       (young-gen pressure), 24 x 16 MiB byte[] churn (humongous-region
                       pressure -> heap expansion -> mmap/mprotect on the kernel side;
                       sized by -Xms64m -Xmx1536m on the java command line),
                       freeMemory() printed again, prints WORKLOAD-GC-PASS.
      4. threads     : new Worker (Runnable), new Thread, start(), join(),
                       getstatic Worker.RESULT, println(I) - the raw number 42 is the
                       pass datum (a thread that never runs leaves the default 0);
                       prints WORKLOAD-THREAD-PASS, then WORKLOAD-ALL-PASS.
  Worker.class - implements java/lang/Runnable (interfaces_count = 1 - the structure
      whose omission killed JAVAWORKLOAD attempt 1): run() prints
      WORKLOAD-THREAD-RAN, churns 200 x 64 KiB, sets static int RESULT = 21*2
      (computed by imul, not constant-folded).

Class file version: major 49 (Java 5), minor 0. Rationale (research D69): code with
branch targets (loops, catch handlers) at major >= 50 REQUIRES StackMapTable frames;
at <= 49 HotSpot verifies with the inferencing verifier instead. Risk named: if this
JDK build rejected pre-50 classes, the failure is readable (UnsupportedClassVersionError
/ verifier error through the pipe arm) and the flip path is major 65 + hand-built
StackMapTable frames. Version 49 still carries the exception_table - its own structure
exercise.

Verify BEFORE use (the D67 discipline): denver/probes/verifyworkload.py must pass on
both files. That verifier is an independent JVMS walker - different code, its own
opcode table - and HotSpot remains the final receiver.

Usage: python3 mkworkload.py <outdir>   (writes Workload.class, Worker.class)
"""
import struct
import sys
import os

MARKERS = {
    "EXC": "WORKLOAD-EXC-PASS",
    "IO": "WORKLOAD-IO-PASS",
    "GC": "WORKLOAD-GC-PASS",
    "THREAD_RAN": "WORKLOAD-THREAD-RAN",
    "THREAD": "WORKLOAD-THREAD-PASS",
    "ALL": "WORKLOAD-ALL-PASS",
    "IO_MISMATCH": "WORKLOAD-IO-MISMATCH",
}

MAJOR = 49  # Java 5 era: inferencing verifier, no StackMapTable needed (see D69)


def u1(x):
    return struct.pack(">B", x)


def u2(x):
    return struct.pack(">H", x)


def u4(x):
    return struct.pack(">I", x)


def i4(x):
    return struct.pack(">i", x)


class Pool:
    """A real constant-pool allocator. mkhello.py's hand-laid indices do not scale to
    two classes with ~50 symbols - and hand-laying is exactly how index drift happens."""

    def __init__(self):
        self.entries = []
        self.d = {}

    def _new(self, key, entry):
        i = self.d.get(key)
        if i is None:
            i = len(self.entries) + 1
            self.d[key] = i
            self.entries.append(entry)
        return i

    def utf8(self, s):
        i = self.d.get(("u", s))
        if i is None:
            b = s.encode("utf-8")
            assert len(b) <= 0xFFFF, "utf8 too long: %r" % s
            i = self._new(("u", s), bytes([1]) + u2(len(b)) + b)
        return i

    def cls(self, name):
        return self._new(("c", name), bytes([7]) + u2(self.utf8(name)))

    def string(self, s):
        return self._new(("s", s), bytes([8]) + u2(self.utf8(s)))

    def integer(self, v):
        return self._new(("i", v), bytes([3]) + i4(v))

    def nat(self, n, d):
        return self._new(("n", n, d), bytes([12]) + u2(self.utf8(n)) + u2(self.utf8(d)))

    def fieldref(self, c, n, d):
        return self._new(("f", c, n, d), bytes([9]) + u2(self.cls(c)) + u2(self.nat(n, d)))

    def methodref(self, c, n, d):
        return self._new(("m", c, n, d), bytes([10]) + u2(self.cls(c)) + u2(self.nat(n, d)))


class Asm:
    """Bytecode emitter. EVERY instruction goes through these helpers - no ad-hoc byte
    appends anywhere (the JAVAWORKLOAD builder bugs were both hand-laid-width bugs)."""

    def __init__(self, pool):
        self.pool = pool
        self.code = bytearray()
        self.labels = {}
        self.fixups = []  # (operand_pos, target_label)

    # -- single-byte opcodes
    def op(self, opc):
        self.code.append(opc)

    # -- u1-operand opcodes (newarray atype, iinc's two operands come separately)
    def op_u1(self, opc, v):
        assert 0 <= v <= 0xFF
        self.code += bytes([opc, v])

    # -- cp-index opcodes (2-byte operand)
    def op_cp2(self, opc, idx):
        assert 1 <= idx <= 0xFFFF
        self.code += bytes([opc]) + u2(idx)

    def ldc(self, idx):
        assert 1 <= idx <= 255, "ldc index %d overflows the u1 operand" % idx
        self.code += bytes([0x12, idx])

    # -- local variable loads/stores, with the _n single-byte forms where they exist
    def aload(self, n):
        self.op(0x2A + n) if 0 <= n <= 3 else self.op_u1(0x19, n)

    def astore(self, n):
        self.op(0x4B + n) if 0 <= n <= 3 else self.op_u1(0x4A, n)

    def iload(self, n):
        self.op(0x1A + n) if 0 <= n <= 3 else self.op_u1(0x15, n)

    def istore(self, n):
        self.op(0x3B + n) if 0 <= n <= 3 else self.op_u1(0x36, n)

    def bipush(self, v):
        self.code.append(0x10)
        self.code += struct.pack(">b", v)

    def sipush(self, v):
        self.code.append(0x11)
        self.code += struct.pack(">h", v)

    def iinc(self, n, c):
        self.op_u1(0x84, n)
        self.code += struct.pack(">b", c)

    # -- labels and branches
    def label(self, name):
        assert name not in self.labels, "duplicate label %s" % name
        self.labels[name] = len(self.code)

    def branch(self, opc, target):
        self.code.append(opc)
        self.fixups.append((len(self.code), target))
        self.code += u2(0)  # placeholder

    def resolve(self):
        for pos, tgt in self.fixups:
            off = self.labels[tgt] - (pos - 1)  # offset is from the OPCODE byte
            # backward branches (loop edges) are negative - both directions legal
            assert -0x8000 <= off <= 0x7FFF, "branch to %s out of s16 range" % tgt
            struct.pack_into(">h", self.code, pos, off)
        self.fixups = []


ACC_PUBLIC, ACC_STATIC, ACC_SUPER = 0x0001, 0x0008, 0x0020

# newarray atype for byte
ATYPE_BYTE = 8


def code_attr(pool, asm, max_stack, max_locals, exc_table):
    asm.resolve()
    code = bytes(asm.code)
    body = (u2(max_stack) + u2(max_locals) + u4(len(code)) + code
            + u2(len(exc_table)) + b"".join(
                u2(s) + u2(e) + u2(h) + u2(c) for (s, e, h, c) in exc_table)
            + u2(0))  # no attributes inside Code
    return u2(pool.utf8("Code")) + u4(len(body)) + body


def method(pool, flags, name, desc, attr):
    return u2(flags) + u2(pool.utf8(name)) + u2(pool.utf8(desc)) + u2(1) + attr


def field_entry(pool, flags, name, desc):
    return u2(flags) + u2(pool.utf8(name)) + u2(pool.utf8(desc)) + u2(0)


def emit_class(pool, this, super_, interfaces, fields, methods):
    ti, si = pool.cls(this), pool.cls(super_)
    iface_idx = [pool.cls(i) for i in interfaces]
    out = b"\xca\xfe\xba\xbe" + u2(0) + u2(MAJOR) + u2(len(pool.entries) + 1)
    out += b"".join(pool.entries)
    out += u2(ACC_PUBLIC | ACC_SUPER) + u2(ti) + u2(si) + u2(len(iface_idx))
    out += b"".join(u2(i) for i in iface_idx)
    out += u2(len(fields)) + b"".join(fields)
    out += u2(len(methods)) + b"".join(methods)
    out += u2(0)  # no class attributes (no SourceFile - traces say "Unknown Source")
    return out


# ---------------------------------------------------------------- Workload.class

def build_workload():
    p = Pool()
    p.utf8("Code")
    p.cls("java/lang/Object")
    p.cls("Workload")
    p.cls("java/lang/Exception")
    exc_init = p.methodref("java/lang/Exception", "<init>", "(Ljava/lang/String;)V")
    get_msg = p.methodref("java/lang/Exception", "getMessage", "()Ljava/lang/String;")
    out_field = p.fieldref("java/lang/System", "out", "Ljava/io/PrintStream;")
    p.cls("java/io/PrintStream")
    ps_str = p.methodref("java/io/PrintStream", "println", "(Ljava/lang/String;)V")
    ps_long = p.methodref("java/io/PrintStream", "println", "(J)V")
    ps_int = p.methodref("java/io/PrintStream", "println", "(I)V")
    p.cls("java/io/FileOutputStream")
    fos_init = p.methodref("java/io/FileOutputStream", "<init>", "(Ljava/lang/String;)V")
    fos_write = p.methodref("java/io/FileOutputStream", "write", "([B)V")
    fos_close = p.methodref("java/io/FileOutputStream", "close", "()V")
    p.cls("java/io/FileInputStream")
    fis_init = p.methodref("java/io/FileInputStream", "<init>", "(Ljava/lang/String;)V")
    fis_read = p.methodref("java/io/FileInputStream", "read", "([B)I")
    fis_close = p.methodref("java/io/FileInputStream", "close", "()V")
    p.cls("java/lang/String")
    s_bytes = p.methodref("java/lang/String", "getBytes", "()[B")
    p.cls("java/util/Arrays")
    arr_eq = p.methodref("java/util/Arrays", "equals", "([B[B)Z")
    p.cls("java/lang/Runtime")
    rt_get = p.methodref("java/lang/Runtime", "getRuntime", "()Ljava/lang/Runtime;")
    rt_free = p.methodref("java/lang/Runtime", "freeMemory", "()J")
    p.cls("java/lang/Thread")
    thr_init = p.methodref("java/lang/Thread", "<init>", "(Ljava/lang/Runnable;)V")
    thr_start = p.methodref("java/lang/Thread", "start", "()V")
    thr_join = p.methodref("java/lang/Thread", "join", "()V")
    p.cls("java/lang/Runnable")
    p.cls("Worker")
    w_result = p.fieldref("Worker", "RESULT", "I")
    worker_init = p.methodref("Worker", "<init>", "()V")
    wf_throw = p.methodref("Workload", "thrower", "(Ljava/lang/String;)V")
    wf_iow = p.methodref("Workload", "ioWrite", "(Ljava/lang/String;Ljava/lang/String;)V")
    wf_ioc = p.methodref("Workload", "ioCheck", "(Ljava/lang/String;Ljava/lang/String;)V")

    s_path = p.string("/work/out.bin")
    s_data = p.string("WORKLOAD-IO-DATA-42")
    s_excmsg = p.string("denver-exc-datum")
    s_iomis = p.string(MARKERS["IO_MISMATCH"] + " read-back differs from written")
    s_exc = p.string(MARKERS["EXC"])
    s_io = p.string(MARKERS["IO"])
    s_gc = p.string(MARKERS["GC"])
    s_thr = p.string(MARKERS["THREAD"])
    s_all = p.string(MARKERS["ALL"])
    i_young = p.integer(131072)   # 128 KiB per young-gen churn step
    i_big = p.integer(16777216)   # 16 MiB per humongous churn step

    # shared snippets -------------------------------------------------
    def p_str(a, sidx):
        a.op_cp2(0xB2, out_field)   # getstatic out
        a.ldc(sidx)                 # ldc
        a.op_cp2(0xB6, ps_str)      # println(String)

    def p_free_mem(a):
        a.op_cp2(0xB2, out_field)   # getstatic out
        a.op_cp2(0xB8, rt_get)      # invokestatic getRuntime
        a.op_cp2(0xB6, rt_free)     # invokevirtual freeMemory -> J
        a.op_cp2(0xB6, ps_long)     # println(J)V

    def churn(a, loop, int_idx, bound, bound_small, i_local, a_local):
        a.op(0x03)                  # iconst_0
        a.istore(i_local)
        a.label(loop)
        a.ldc(int_idx)              # ldc <element count>
        a.op_u1(0xBC, ATYPE_BYTE)   # newarray byte
        a.astore(a_local)
        a.iinc(i_local, 1)
        a.iload(i_local)
        a.bipush(bound) if bound_small else a.sipush(bound)
        a.branch(0xA1, loop)        # if_icmplt loop

    def new_exc(a, cls_key, msg_idx):
        a.op_cp2(0xBB, cls_key)     # new
        a.op(0x59)                  # dup
        a.ldc(msg_idx)
        a.op_cp2(0xB7, exc_init)    # invokespecial Exception.<init>(String)
        a.op(0xBF)                  # athrow

    # ---- main([Ljava/lang/String;)V --------------------------------------------
    a = Asm(p)
    # 1. exceptions
    a.label("T_start")
    a.ldc(s_excmsg)
    a.op_cp2(0xB8, wf_throw)
    a.label("T_end")
    a.branch(0xA7, "after_exc")
    a.label("catch")
    a.astore(1)                                 # e = the unwound exception
    a.op_cp2(0xB2, out_field)                   # getstatic out
    a.aload(1)                                  # aload_1 (e)
    a.op_cp2(0xB6, get_msg)                     # getMessage -> String (stack: out,msg)
    a.op_cp2(0xB6, ps_str)                      # println(msg): receiver out, arg msg
    a.op_cp2(0xB2, out_field)                   # getstatic out (fresh - the receiver
    #   must sit directly under the argument for every println; one getstatic cannot
    #   serve two println calls with an ldc between them)
    a.ldc(s_exc)
    a.op_cp2(0xB6, ps_str)                      # println(marker)
    a.label("after_exc")
    exc_table = [(a.labels["T_start"], a.labels["T_end"],
                  a.labels["catch"], p.cls("java/lang/Exception"))]

    # 2. file I/O
    a.ldc(s_path)
    a.ldc(s_data)
    a.op_cp2(0xB8, wf_iow)
    a.ldc(s_path)
    a.ldc(s_data)
    a.op_cp2(0xB8, wf_ioc)
    p_str(a, s_io)

    # 3. allocation pressure
    p_free_mem(a)
    churn(a, "Y", i_young, 1000, False, 4, 5)
    churn(a, "B", i_big, 24, False, 4, 5)
    p_free_mem(a)
    p_str(a, s_gc)

    # 4. threads
    a.op_cp2(0xBB, p.d[("c", "Worker")])        # new Worker
    a.op(0x59)                                  # dup
    a.op_cp2(0xB7, worker_init)                 # invokespecial Worker.<init>()V
    a.astore(2)                                 # worker
    a.op_cp2(0xBB, p.d[("c", "java/lang/Thread")])
    a.op(0x59)                                  # dup
    a.aload(2)
    a.op_cp2(0xB7, thr_init)                    # Thread.<init>(Runnable)
    a.astore(3)                                 # t
    a.aload(3)
    a.op_cp2(0xB6, thr_start)                   # start()
    a.aload(3)
    a.op_cp2(0xB6, thr_join)                    # join()
    a.op_cp2(0xB2, out_field)                   # getstatic out
    a.ldc(p.string("WORKLOAD-THREAD-RESULT-FOLLOWS"))
    a.op_cp2(0xB6, ps_str)                      # println(FOLLOWS line)
    a.op_cp2(0xB2, out_field)                   # getstatic out (println(I)'s receiver)
    a.op_cp2(0xB2, w_result)                    # getstatic Worker.RESULT
    a.op_cp2(0xB6, ps_int)                      # println(I) -> the raw 42 line
    p_str(a, s_thr)
    p_str(a, s_all)
    a.op(0xB1)                                  # return
    main_attr = code_attr(p, a, 4, 6, exc_table)

    # ---- thrower(Ljava/lang/String;)V : static; new Exception(aload_0 msg); athrow
    at = Asm(p)
    at.op_cp2(0xBB, p.d[("c", "java/lang/Exception")])  # new Exception
    at.op(0x59)                                         # dup
    at.aload(0)                                         # aload_0 (the message argument)
    at.op_cp2(0xB7, exc_init)                           # invokespecial <init>(String)
    at.op(0xBF)                                         # athrow
    thrower_attr = code_attr(p, at, 3, 1, [])

    # ---- ioWrite(path, data) : fos = new FileOutputStream(path);
    #                           fos.write(data.getBytes()); fos.close()
    aw = Asm(p)
    aw.op_cp2(0xBB, p.d[("c", "java/io/FileOutputStream")])
    aw.op(0x59)
    aw.aload(0)
    aw.op_cp2(0xB7, fos_init)
    aw.astore(2)
    aw.aload(2)
    aw.aload(1)
    aw.op_cp2(0xB6, s_bytes)                    # data.getBytes() -> [B
    aw.op_cp2(0xB6, fos_write)                  # write([B)
    aw.aload(2)
    aw.op_cp2(0xB6, fos_close)                  # close()
    aw.op(0xB1)
    iow_attr = code_attr(p, aw, 3, 3, [])

    # ---- ioCheck(path, expected) : fis; buf=new byte[64]; n=read(buf); close();
    #      exp=expected.getBytes(); Arrays.equals(buf, exp) or athrow(mismatch)
    ac = Asm(p)
    ac.op_cp2(0xBB, p.d[("c", "java/io/FileInputStream")])
    ac.op(0x59)
    ac.aload(0)
    ac.op_cp2(0xB7, fis_init)
    ac.astore(2)
    ac.sipush(64)
    ac.op_u1(0xBC, ATYPE_BYTE)
    ac.astore(3)                                # buf
    ac.aload(2)
    ac.aload(3)
    ac.op_cp2(0xB6, fis_read)                   # n = read(buf)
    ac.op(0x57)                                 # pop n (a short read is a mismatch anyway)
    ac.aload(2)
    ac.op_cp2(0xB6, fis_close)
    ac.aload(1)
    ac.op_cp2(0xB6, s_bytes)                    # exp = expected.getBytes()
    ac.astore(4)
    ac.aload(3)                                 # buf
    ac.aload(4)                                 # exp
    ac.op_cp2(0xB8, arr_eq)                     # invokestatic Arrays.equals -> Z
    ac.branch(0x99, "mis")                      # ifeq mis
    ac.op(0xB1)                                 # return (equal)
    ac.label("mis")
    new_exc(ac, p.d[("c", "java/lang/Exception")], s_iomis)
    ioc_attr = code_attr(p, ac, 3, 5, [])

    # ---- <init> -----------------------------------------------------------------
    ai = Asm(p)
    ai.aload(0)
    ai.op_cp2(0xB7, p.methodref("java/lang/Object", "<init>", "()V"))
    ai.op(0xB1)
    init_attr = code_attr(p, ai, 1, 1, [])

    methods = [
        method(p, ACC_PUBLIC | ACC_STATIC, "main", "([Ljava/lang/String;)V", main_attr),
        method(p, ACC_PUBLIC | ACC_STATIC, "thrower", "(Ljava/lang/String;)V", thrower_attr),
        method(p, ACC_PUBLIC | ACC_STATIC, "ioWrite", "(Ljava/lang/String;Ljava/lang/String;)V", iow_attr),
        method(p, ACC_PUBLIC | ACC_STATIC, "ioCheck", "(Ljava/lang/String;Ljava/lang/String;)V", ioc_attr),
        method(p, ACC_PUBLIC, "<init>", "()V", init_attr),
    ]
    return emit_class(p, "Workload", "java/lang/Object", [], [], methods)


# ------------------------------------------------------------------ Worker.class

def build_worker():
    p = Pool()
    p.utf8("Code")
    p.cls("Worker")
    p.cls("java/lang/Object")
    p.cls("java/lang/Runnable")
    out_field = p.fieldref("java/lang/System", "out", "Ljava/io/PrintStream;")
    p.cls("java/io/PrintStream")
    ps_str = p.methodref("java/io/PrintStream", "println", "(Ljava/lang/String;)V")
    w_result = p.fieldref("Worker", "RESULT", "I")
    obj_init = p.methodref("java/lang/Object", "<init>", "()V")
    s_ran = p.string(MARKERS["THREAD_RAN"])
    i_arr = p.integer(65536)

    # ---- run()V -----------------------------------------------------------------
    a = Asm(p)
    a.op_cp2(0xB2, out_field)
    a.ldc(s_ran)
    a.op_cp2(0xB6, ps_str)                      # println(THREAD-RAN)
    a.op(0x03)                                  # iconst_0
    a.istore(1)                                 # i = 0
    a.label("L")
    a.ldc(i_arr)                                # ldc 65536
    a.op_u1(0xBC, ATYPE_BYTE)                   # newarray byte
    a.astore(2)                                 # arr (previous becomes garbage)
    a.iinc(1, 1)
    a.iload(1)
    a.sipush(200)
    a.branch(0xA1, "L")                         # if_icmplt L
    a.bipush(21)
    a.bipush(2)
    a.op(0x68)                                  # imul
    a.op_cp2(0xB3, w_result)                    # putstatic RESULT (42)
    a.op(0xB1)                                  # return
    run_attr = code_attr(p, a, 3, 3, [])

    ai = Asm(p)
    ai.aload(0)
    ai.op_cp2(0xB7, obj_init)
    ai.op(0xB1)
    init_attr = code_attr(p, ai, 1, 1, [])

    fields = [field_entry(p, ACC_PUBLIC | ACC_STATIC, "RESULT", "I")]
    methods = [
        method(p, ACC_PUBLIC, "<init>", "()V", init_attr),
        method(p, ACC_PUBLIC, "run", "()V", run_attr),
    ]
    return emit_class(p, "Worker", "java/lang/Object", ["java/lang/Runnable"],
                      fields, methods)


def main():
    if len(sys.argv) != 2:
        print("usage: python3 mkworkload.py <outdir>", file=sys.stderr)
        return 2
    outdir = sys.argv[1]
    os.makedirs(outdir, exist_ok=True)
    for name, blob in (("Workload.class", build_workload()),
                       ("Worker.class", build_worker())):
        path = os.path.join(outdir, name)
        with open(path, "wb") as f:
            f.write(blob)
        print("WROTE %s %d bytes major=%d" % (path, len(blob), MAJOR))
    return 0


if __name__ == "__main__":
    sys.exit(main())
