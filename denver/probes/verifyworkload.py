#!/usr/bin/env python3
"""verifyworkload.py - INDEPENDENT structural + dataflow verifier for the
PHASE-3-JAVACLASSLIB workload classes (JVMS 4.x walk, its own opcode table).

Why: the D67 lesson (PHASE-3-JAVAWORKLOAD attempt 1) - a checker that shares the
author's assumptions verifies nothing; mkhello.py and verifyclass.py both omitted the
same interfaces_count u2 and only the GUEST's HotSpot caught it. So this verifier is
written against JVMS 4.10.1 structure, not against mkworkload.py's internals, and it
makes its own independent statement of what the workload IS (the symbol whitelists and
surface assertions below come from REQUIREMENTS.md G12, not from the builder's code).
The final receiver is still HotSpot - this only lowers the cost of a RED from one
boot (~11 s) to zero.

Checks:
  1. structure: magic/version/pool/fields/methods/attributes with overrun detection;
  2. pool closure: every reference index resolves, tags match the referencing entry
     kind, descriptors parse;
  3. instruction decode: the code stream decodes exactly to code_length, every CP
     reference is in-range and tag-appropriate, newarray atype valid, every branch
     target in-range AND on an instruction boundary, exception_table entries
     in-range/boundary/correct-typed;
  4. dataflow: operand-stack DEPTH propagated over a worklist (fallthrough + branch
     edges, exception handlers seeded at depth 1); a merge point reached with two
     different depths is RED, underflow is RED, and max_stack overflow is RED -
     the exact bug class of a println whose receiver slipped;
  5. intent: per-class method/field/interface sets and the external-symbol whitelist
     (what the workload is ALLOWED to reference), asserted from the gate's text.

Usage: python3 verifyworkload.py   (paths are fixed: denver/probes/build/*.class)
       exit 0 iff every file passes.
"""
import struct
import sys

PATHS = ["denver/probes/build/Workload.class", "denver/probes/build/Worker.class"]

# ------------------------------------------------------------------ opcode table
OPS = {}
for o in (0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A,
          0x2A, 0x2B, 0x2C, 0x2D, 0x2E, 0x2F, 0x32, 0x33,
          0x3B, 0x3C, 0x3D, 0x3E, 0x4B, 0x4C, 0x4D, 0x4E,
          0x57, 0x59, 0x5F, 0x60, 0x64, 0x68,
          0xAC, 0xAD, 0xB0, 0xB1, 0xBE, 0xBF):
    OPS[o] = ("none", o)  # (operand kind, tag-expectation or None)

NAMES = {0x00: "nop", 0x01: "aconst_null", 0x02: "iconst_m1", 0x03: "iconst_0",
         0x04: "iconst_1", 0x05: "iconst_2", 0x06: "iconst_3", 0x07: "iconst_4",
         0x08: "iconst_5", 0x09: "lconst_0", 0x0A: "lconst_1",
         0x10: "bipush", 0x11: "sipush", 0x12: "ldc", 0x13: "ldc_w",
         0x15: "iload", 0x16: "lload", 0x19: "aload",
         0x1A: "iload_0", 0x1B: "iload_1", 0x1C: "iload_2", 0x1D: "iload_3",
         0x2A: "aload_0", 0x2B: "aload_1",
         0x2C: "aload_2", 0x2D: "aload_3", 0x2E: "iaload", 0x2F: "laload",
         0x32: "aaload", 0x33: "baload", 0x34: "caload", 0x35: "saload",
         0x36: "istore", 0x3B: "istore_0",
         0x3C: "istore_1", 0x3D: "istore_2", 0x3E: "istore_3",
         0x4A: "astore", 0x4B: "astore_0", 0x4C: "astore_1", 0x4D: "astore_2",
         0x4E: "astore_3", 0x57: "pop", 0x59: "dup", 0x5F: "swap", 0x60: "iadd",
         0x64: "isub", 0x68: "imul", 0x84: "iinc", 0x99: "ifeq", 0x9A: "ifne",
         0xA1: "if_icmplt", 0xA2: "if_icmpge", 0xA7: "goto", 0xAC: "ireturn",
         0xAD: "lreturn", 0xB0: "areturn", 0xB1: "return",
         0xB2: "getstatic", 0xB3: "putstatic", 0xB4: "getfield", 0xB5: "putfield",
         0xB6: "invokevirtual", 0xB7: "invokespecial", 0xB8: "invokestatic",
         0xBB: "new", 0xBC: "newarray", 0xBD: "anewarray", 0xBE: "arraylength",
         0xBF: "athrow", 0xC0: "checkcast", 0xC1: "instanceof"}

# operand sizes beyond the opcode byte
SIZE = {0x10: 1, 0x11: 2, 0x12: 1, 0x13: 2, 0x15: 1, 0x19: 1, 0x36: 1, 0x4A: 1,
        0x84: 2, 0x99: 2, 0x9A: 2, 0xA1: 2, 0xA2: 2, 0xA7: 2,
        0xB2: 2, 0xB3: 2, 0xB4: 2, 0xB5: 2, 0xB6: 2, 0xB7: 2, 0xB8: 2,
        0xBB: 2, 0xBC: 1, 0xBD: 2, 0xC0: 2, 0xC1: 2}

# tag expectations for cp-carrying opcodes
CPTAG = {0x12: (3, 4, 7, 8, 16, 17), 0x13: (3, 4, 5, 6, 7, 8, 16, 17, 19, 20),
         0xB2: (9,), 0xB3: (9,), 0xB4: (9,), 0xB5: (9,),
         0xB6: (10, 11), 0xB7: (10, 11), 0xB8: (10, 11),
         0xBB: (7,), 0xBD: (7,), 0xC0: (7,), 0xC1: (7,)}

# branch opcode -> also-jump-to-target flag; terminal = no fallthrough
TERMINAL = {0xA7, 0xAC, 0xAD, 0xB0, 0xB1, 0xBF}

# (pops, pushes) in SLOTS; invokes resolved via the callee descriptor at check time
EFFECT = {"aconst_null": (0, 1), "iconst_m1": (0, 1), "iconst_0": (0, 1),
          "iconst_1": (0, 1), "iconst_2": (0, 1), "iconst_3": (0, 1),
          "iconst_4": (0, 1), "iconst_5": (0, 1), "lconst_0": (0, 2), "lconst_1": (0, 2),
          "bipush": (0, 1), "sipush": (0, 1), "ldc": (0, 1), "ldc_w": (0, 1),
          "iload": (0, 1), "iload_0": (0, 1), "iload_1": (0, 1), "iload_2": (0, 1),
          "iload_3": (0, 1), "lload": (0, 2), "aload": (0, 1), "aload_0": (0, 1),
          "aload_1": (0, 1), "aload_2": (0, 1), "aload_3": (0, 1),
          "iaload": (2, 1), "laload": (2, 2),
          "aaload": (2, 1), "baload": (2, 1), "caload": (2, 1), "saload": (2, 1),
          "istore": (1, 0), "astore": (1, 0), "istore_0": (1, 0), "istore_1": (1, 0),
          "istore_2": (1, 0), "istore_3": (1, 0), "astore_0": (1, 0), "astore_1": (1, 0),
          "astore_2": (1, 0), "astore_3": (1, 0),
          "pop": (1, 0), "dup": (1, 2), "swap": (2, 2), "iadd": (2, 1), "isub": (2, 1),
          "imul": (2, 1), "iinc": (0, 0),
          "getstatic": (0, 1), "putstatic": (1, 0), "getfield": (1, 1), "putfield": (2, 0),
          "new": (0, 1), "newarray": (1, 1), "anewarray": (1, 1), "arraylength": (1, 1),
          "athrow": (1, 0), "return": (0, 0), "ireturn": (1, 0), "lreturn": (2, 0),
          "areturn": (1, 0), "ifeq": (1, 0), "ifne": (1, 0), "if_icmplt": (2, 0),
          "if_icmpge": (2, 0), "goto": (0, 0)}

TAGS = {1: "Utf8", 3: "Integer", 4: "Float", 5: "Long", 6: "Double", 7: "Class",
        8: "String", 9: "Fieldref", 10: "Methodref", 11: "InterfaceMethodref",
        12: "NameAndType", 15: "MethodHandle", 16: "MethodType", 17: "Dynamic",
        18: "InvokeDynamic", 19: "Module", 20: "Package"}


class VErr(Exception):
    pass


class Reader:
    def __init__(self, b):
        self.b, self.o = b, 0

    def u1(self):
        v = self.b[self.o]; self.o += 1; return v

    def u2(self):
        v = struct.unpack_from(">H", self.b, self.o)[0]; self.o += 2; return v

    def u4(self):
        v = struct.unpack_from(">I", self.b, self.o)[0]; self.o += 4; return v

    def raw(self, n):
        if self.o + n > len(self.b):
            raise VErr("overrun at %d: need %d bytes, file is %d" % (self.o, n, len(self.b)))
        v = self.b[self.o:self.o + n]; self.o += n; return v


def parse_desc(desc):
    """(arg slots with J/D doubled, True) for method descriptors, (slots, False) for
    field descriptors."""
    if desc.startswith("("):
        i, args = 1, 0
        while i < len(desc) and desc[i] != ")":
            c = desc[i]
            if c in "JD":
                args += 2; i += 1
            elif c == "[":
                while desc[i] == "[":
                    i += 1
                if i < len(desc) and desc[i] == "L":
                    while i < len(desc) and desc[i] != ";":
                        i += 1
                args += 1; i += 1
            elif c == "L":
                while i < len(desc) and desc[i] != ";":
                    i += 1
                args += 1; i += 1
            elif c in "BCFISZ":
                args += 1; i += 1
            else:
                raise VErr("bad param %r in %r" % (c, desc))
        if i >= len(desc) or desc[i] != ")":
            raise VErr("unterminated method desc %r" % desc)
        ret = desc[i + 1:]
        if not (ret == "V" or (len(ret) == 1 and ret in "BCFISZJD")
                or (ret.startswith("L") and ret.endswith(";")) or ret.startswith("[")):
            raise VErr("bad return %r in %r" % (ret, desc))
        return args, True
    slots, i = 0, 0
    while i < len(desc):
        c = desc[i]
        if c in "JD":
            slots += 2 if c == "J" else 1; i += 1
        elif c == "[":
            while i < len(desc) and desc[i] == "[":
                i += 1
            if i < len(desc) and desc[i] == "L":
                while i < len(desc) and desc[i] != ";":
                    i += 1
            slots += 1; i += 1
        elif c == "L":
            while i < len(desc) and desc[i] != ";":
                i += 1
            slots += 1; i += 1
        elif c in "BCFISZ":
            slots += 1; i += 1
        else:
            raise VErr("bad field desc %r" % desc)
    return slots, False


class Ctx:
    def __init__(self, cp, count):
        self.cp, self.count = cp, count

    def utf8(self, i):
        t, v = self.cp[i]
        if t != 1:
            raise VErr("cp[%d] not Utf8" % i)
        return v

    def cls_name(self, i):
        t, v = self.cp[i]
        if t != 7:
            raise VErr("cp[%d] is %s want Class" % (i, TAGS.get(t, t)))
        return self.utf8(v)

    def ref(self, i):
        """(class, name, desc) for a Fieldref/Methodref."""
        t, v = self.cp[i]
        if t not in (9, 10, 11):
            raise VErr("cp[%d] is %s want ref" % (i, TAGS.get(t, t)))
        c = self.cls_name(v[0])
        t2, v2 = self.cp[v[1]]
        if t2 != 12:
            raise VErr("cp[%d] NameAndType bad" % v[1])
        return c, self.utf8(v2[0]), self.utf8(v2[1])


def decode_step(code, pc):
    """-> (opcode, next_pcs, pops, push_kind) or raises VErr on unknown opcode.
    push_kind is an int slot count, 'J', or ('invoke', cp_idx)."""
    op = code[pc]
    if op not in NAMES:
        raise VErr("unknown opcode 0x%02x at %d" % (op, pc))
    n = SIZE.get(op, 0)
    nxt = pc + 1 + n
    nexts = [nxt] if op not in TERMINAL else []
    if op in (0x99, 0x9A, 0xA1, 0xA2):           # conditionals also branch
        off = struct.unpack_from(">h", code, pc + 1)[0]
        nexts.append(pc + off)
    elif op == 0xA7:                             # goto: target only
        off = struct.unpack_from(">h", code, pc + 1)[0]
        nexts.append(pc + off)
    name = NAMES[op]
    if name.startswith("invoke"):
        idx = struct.unpack_from(">H", code, pc + 1)[0]
        return op, nexts, None, ("invoke", idx)
    pops, push = EFFECT[name]
    return op, nexts, pops, push


def verify(path, intent):
    errs = []

    def bad(msg):
        errs.append(msg)

    b = open(path, "rb").read()
    r = Reader(b)
    cp = {}
    methods = {}
    fields = {}
    try:
        if r.u4() != 0xCAFEBABE:
            bad("magic")
        minor, major = r.u2(), r.u2()
        if (minor, major) != (0, intent["major"]):
            bad("version %d.%d want %d.0" % (major, minor, intent["major"]))
        count = r.u2()
        i = 1
        while i < count:
            tag = r.u1()
            if tag == 1:
                cp[i] = (tag, r.raw(r.u2()).decode("utf-8", "replace"))
            elif tag in (3, 4):
                cp[i] = (tag, r.raw(4))
            elif tag in (5, 6):
                cp[i] = (tag, r.raw(8)); i += 1; cp[i] = (0, None)
            elif tag in (7, 8, 16, 19, 20):
                cp[i] = (tag, r.u2())
            elif tag in (9, 10, 11, 12, 17, 18):
                cp[i] = (tag, (r.u2(), r.u2()))
            elif tag == 15:
                cp[i] = (tag, (r.u1(), r.u2()))
            else:
                raise VErr("unknown tag %d at index %d" % (tag, i))
            i += 1
        if len(cp) != count - 1:
            bad("pool slot count %d want %d" % (len(cp), count - 1))
        ctx = Ctx(cp, count)

        # --- pool closure
        for idx, (t, v) in cp.items():
            if idx == 0:
                continue
            if t in (7, 8, 16, 19, 20):
                if not (1 <= v < count) or cp[v][0] != 1:
                    bad("cp[%d] %s -> bad Utf8 idx %d" % (idx, TAGS[t], v))
            elif t in (9, 10, 11, 12):
                a, n = v
                if not (1 <= a < count and 1 <= n < count):
                    bad("cp[%d] %s -> out-of-range" % (idx, TAGS[t]))
                else:
                    if t == 12:
                        if cp[a][0] != 1 or cp[n][0] != 1:
                            bad("cp[%d] NameAndType -> not Utf8 pair" % idx)
                        else:
                            parse_desc(ctx.utf8(n))
                    else:
                        if cp[a][0] != 7:
                            bad("cp[%d] ref -> class not Class" % idx)
                        elif cp[n][0] != 12:
                            bad("cp[%d] ref -> nat not NameAndType" % idx)
                        else:
                            parse_desc(ctx.utf8(cp[n][1][1]))

        # --- header (JVMS 4.1 order: access_flags, this_class, super_class, ifaces)
        flags = r.u2()
        this_i = r.u2()
        sup_i = r.u2()
        nif = r.u2()
        ifaces = [ctx.cls_name(r.u2()) for _ in range(nif)]
        if this_i not in cp or ctx.cls_name(this_i) != intent["this"]:
            bad("this_class wrong")
        if sup_i not in cp:
            bad("super_class out of range")
        if (flags & 0x0021) != 0x0021:
            bad("class access 0x%04x lacks PUBLIC|SUPER" % flags)
        if (flags & 0x0210) and not intent["interfaces"]:
            bad("class access 0x%04x says interface/abstract but interfaces empty" % flags)
        if sorted(ifaces) != sorted(intent["interfaces"]):
            bad("interfaces %s want %s" % (sorted(ifaces), sorted(intent["interfaces"])))

        for _ in range(r.u2()):
            fflags, fn, fd, na = r.u2(), r.u2(), r.u2(), r.u2()
            fname = ctx.utf8(fn)
            fdesc = ctx.utf8(fd)
            parse_desc(fdesc)
            fields[fname] = (fflags, fdesc)
            if na != 0:
                bad("field %s has attributes (none expected)" % fname)
                for _ in range(na):
                    r.o += 2          # attribute_name_index
                    alen = r.u4()
                    r.o += alen       # skip the attribute body

        for _ in range(r.u2()):
            mflags, mn, md, na = r.u2(), r.u2(), r.u2(), r.u2()
            name, desc = ctx.utf8(mn), ctx.utf8(md)
            parse_desc(desc)
            key = (name, desc)
            if key in methods:
                bad("duplicate method %s%s" % key)
            methods[key] = mflags
            code = b""
            exct = []
            max_stack = max_locals = None
            for _ in range(na):
                ani = r.u2()
                alen = r.u4()
                end = r.o + alen
                if ctx.utf8(ani) == "Code":
                    max_stack, max_locals = r.u2(), r.u2()
                    clen = r.u4()
                    code = r.raw(clen)
                    exct = [(r.u2(), r.u2(), r.u2(), r.u2()) for _ in range(r.u2())]
                    if r.u2() != 0:
                        bad("method %s: Code carries attributes (none expected)" % name)
                if r.o != end:
                    bad("method %s: attribute %s length mismatch" % (name, ctx.utf8(ani)))
                r.o = end
            if not (0 < max_locals < 64 and 0 < max_stack < 64):
                bad("method %s: implausible max_stack/max_locals %s/%s"
                    % (name, max_stack, max_locals))

            # --- pass 1: linear decode -> boundaries + CP checks
            offs = set()
            pc = 0
            while pc < len(code):
                op, nexts, pops, pushk = decode_step(code, pc)
                offs.add(pc)
                if op in CPTAG:
                    idx = struct.unpack_from(">H", code, pc + 1)[0] if op != 0x12 \
                        else code[pc + 1]
                    if not (1 <= idx < count) or cp[idx][0] not in CPTAG[op]:
                        got = TAGS.get(cp.get(idx, (0, None))[0], cp.get(idx, (0, None))[0])
                        bad("%s at %d: cp[%d] tag %s unexpected" % (NAMES[op], pc, idx, got))
                if op == 0xBC and code[pc + 1] not in range(4, 12):
                    bad("newarray atype %d at %d" % (code[pc + 1], pc))
                if op in (0x15, 0x19, 0x36, 0x4A) and code[pc + 1] >= max_locals:
                    bad("%s local %d >= max_locals at %d" % (NAMES[op], code[pc + 1], pc))
                if op == 0x84 and code[pc + 1] >= max_locals:
                    bad("iinc local %d >= max_locals at %d" % (code[pc + 1], pc))
                for t in nexts:
                    if not (0 <= t < len(code)):
                        bad("%s at %d: target %d outside code" % (NAMES[op], pc, t))
                pc += 1 + SIZE.get(op, 0)
            if pc != len(code):
                bad("decode ended at %d of %d" % (pc, len(code)))

            # --- pass 2: depth dataflow over a worklist
            # entry stack is EMPTY for every method: the receiver lives in local 0,
            # it is not on the operand stack (that is why javac's plain <init> says
            # stack=1: entry [] -> aload_0 -> 1 -> invokespecial pops it -> 0)
            start = 0
            todo = [(0, start)]
            for (_, _, h, _) in exct:
                todo.append((h, 1))                   # handler entries: [exception]
            depth_at = {}
            maxdepth = 0
            while todo:
                at, d = todo.pop()
                if not (0 <= at < len(code)):
                    continue                          # out-of-range already reported
                if at in depth_at:
                    if depth_at[at] != d:
                        bad("method %s: merge at %d sees depth %d and %d"
                            % (name, at, depth_at[at], d))
                    continue
                depth_at[at] = d
                if at not in offs:
                    bad("method %s: dataflow reached non-boundary %d" % (name, at))
                    continue
                op, nexts, pops, pushk = decode_step(code, at)
                if pushk and isinstance(pushk, tuple) and pushk[0] == "invoke":
                    _, _, callee = ctx.ref(pushk[1])
                    argslots, _ = parse_desc(callee)
                    ret = callee.split(")")[-1]
                    pops = argslots + (0 if op == 0xB8 else 1)
                    push = 2 if ret in ("J", "D") else (0 if ret == "V" else 1)
                else:
                    pops, push = pops, pushk
                d2 = d - pops
                if d2 < 0:
                    bad("method %s: stack underflow at %d (%s)" % (name, at, NAMES[op]))
                    continue
                d2 += push
                maxdepth = max(maxdepth, d2)
                for t in nexts:
                    if 0 <= t < len(code):
                        todo.append((t, d2))
            if maxdepth > max_stack:
                bad("method %s: max depth %d > max_stack %d" % (name, maxdepth, max_stack))

            # --- exception table
            for (s, e, h, ct) in exct:
                if not (0 <= s <= e <= len(code)) or not (0 <= h < len(code)):
                    bad("method %s: exception range %d..%d handler %d out of code"
                        % (name, s, e, h))
                    continue
                if s not in offs or (e not in offs and e != len(code)) or h not in offs:
                    bad("method %s: exception entry point off an instruction boundary"
                        % name)
                if ct != 0:
                    try:
                        ctx.cls_name(ct)
                    except VErr:
                        bad("method %s: catch_type is not a Class" % name)

            # --- intent: required/forbidden symbol references per method
            want = intent["methods"].get((name, desc), {})
            for sym in want.get("contains", []):
                if not code_refs(ctx, code, sym):
                    bad("method %s: missing required reference %s" % (name, sym))
            for sym in want.get("excludes", []):
                if code_refs(ctx, code, sym):
                    bad("method %s: must NOT reference %s" % (name, sym))
    except VErr as e:
        bad(str(e))
    except IndexError:
        bad("file truncated mid-structure")
    # class-level attributes (builder emits none; a non-zero count is a finding)
    ncls_attrs = r.u2()
    if ncls_attrs != 0:
        bad("class attributes present: %d" % ncls_attrs)

    # --- intent: method/field/interface sets + external class surface
    want_methods = set(intent["methods"]) | {("<init>", "()V")}
    for k in want_methods:
        if k not in methods:
            bad("missing method %s%s" % k)
    for k in methods:
        if k not in want_methods:
            bad("unexpected method %s%s" % k)
    if set(fields) != set(intent["fields"]):
        bad("fields %s want %s" % (sorted(fields), sorted(intent["fields"])))
    for fn, wf in intent["fields"].items():
        if fn in fields:
            fl, fd = fields[fn]
            if fd != wf[1] or (fl & wf[0]) != wf[0]:
                bad("field %s flags/desc mismatch: 0x%04x %s" % (fn, fl, fd))
    try:
        ext = {ctx.cls_name(v) for i, (t, v) in cp.items() if t == 7}
        ext.discard(intent["this"])
        extra = ext - set(intent["classes"])
        if extra:
            bad("references outside the declared surface: %s" % sorted(extra))
    except VErr:
        pass
    if r.o != len(b):
        bad("trailing bytes: %d of %d" % (r.o, len(b)))

    if errs:
        print("VERIFY RED %s" % path)
        for e in errs:
            print("  " + e)
        return False
    print("VERIFY PASS %s %d bytes" % (path, len(b)))
    return True


def code_refs(ctx, code, sym):
    """sym=(class,name[,desc]): does any cp2 opcode in code reference it?"""
    for i in range(len(code) - 2):
        op = code[i]
        if op in (0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xBB):
            idx = struct.unpack_from(">H", code, i + 1)[0]
            try:
                if op == 0xBB:
                    if len(sym) == 3 and sym[2] == "CLASS" and ctx.cls_name(idx) == sym[0]:
                        return True
                    continue
                c, n, d = ctx.ref(idx)
            except VErr:
                continue
            if c == sym[0] and n == sym[1] and (len(sym) < 3 or d == sym[2]):
                return True
    return False


def main():
    I_Workload = {
        "major": 49, "this": "Workload",
        "interfaces": [],
        "fields": {},
        "classes": ["java/lang/Object", "java/lang/Exception", "java/lang/System",
                    "java/io/PrintStream", "java/io/FileOutputStream",
                    "java/io/FileInputStream", "java/lang/String", "java/util/Arrays",
                    "java/lang/Runtime", "java/lang/Thread", "java/lang/Runnable",
                    "Worker"],
        "methods": {
            ("main", "([Ljava/lang/String;)V"): {
                "contains": [("java/lang/Thread", "start", "()V"),
                             ("java/lang/Thread", "join", "()V"),
                             ("Worker", "RESULT", "I"),
                             ("Workload", "thrower", "(Ljava/lang/String;)V"),
                             ("Workload", "ioWrite", "(Ljava/lang/String;Ljava/lang/String;)V"),
                             ("Workload", "ioCheck", "(Ljava/lang/String;Ljava/lang/String;)V"),
                             ("java/lang/Runtime", "freeMemory", "()J")]},
            ("thrower", "(Ljava/lang/String;)V"): {
                "contains": [("java/lang/Exception", "<init>", "(Ljava/lang/String;)V")]},
            ("ioWrite", "(Ljava/lang/String;Ljava/lang/String;)V"): {
                "contains": [("java/io/FileOutputStream", "write", "([B)V"),
                             ("java/io/FileOutputStream", "close", "()V")]},
            ("ioCheck", "(Ljava/lang/String;Ljava/lang/String;)V"): {
                "contains": [("java/io/FileInputStream", "read", "([B)I"),
                             ("java/util/Arrays", "equals", "([B[B)Z"),
                             ("java/lang/Exception", "<init>", "(Ljava/lang/String;)V")]},
        },
    }
    I_Worker = {
        "major": 49, "this": "Worker",
        "interfaces": ["java/lang/Runnable"],
        "fields": {"RESULT": (0x0009, "I")},  # public static
        "classes": ["java/lang/Object", "java/lang/Runnable", "java/lang/System",
                    "java/io/PrintStream"],
        "methods": {
            ("run", "()V"): {
                "contains": [("Worker", "RESULT", "I"),
                             ("java/io/PrintStream", "println", "(Ljava/lang/String;)V")]},
        },
    }
    ok = True
    ok &= verify(PATHS[0], I_Workload)
    ok &= verify(PATHS[1], I_Worker)
    print("VERIFYWORKLOAD %s" % ("PASS" if ok else "RED"))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
