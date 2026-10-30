#!/usr/bin/env python3
"""Audit Akuma's syscall-number constants against Linux's own tables (report only).

    scripts/audit_syscall_constants.py TABLES_DIR

TABLES_DIR holds four inputs, taken from a Linux tree and the musl cross toolchains:

    linux_x86_64.tbl   <linux>/arch/x86/entry/syscalls/syscall_64.tbl
    linux_generic.tbl  <linux>/scripts/syscall.tbl   (asm-generic, which AArch64 uses)
    musl_x86_64.txt    echo | x86_64-linux-musl-gcc  -E -dM -include bits/syscall.h - | grep __NR_
    musl_aarch64.txt   echo | aarch64-linux-musl-gcc -E -dM -include bits/syscall.h - | grep __NR_

Checks: every constant in akuma-syscalls-linux::nr, akuma-syscalls-abi's rows (both halves),
akuma-ring3-amd64::nr and libakuma's two per-arch tables (sshd's syscall layer); literal dispatch
arms in amd64/src/usermode.rs and taoxichuan/src against the call their comment names; and
explicit "name = N" / "name (N)" / "x86_64 N" claims in comments. The arm and comment checks
are heuristics and report false positives - read each hit in context. Findings and their review:
docs/archive/SYSCALL_CONSTANT_AUDIT_2026_10_30.md.
"""
import os, re, sys, collections

REPO, T = os.path.dirname(os.path.dirname(os.path.abspath(__file__))), sys.argv[1]

def linux_tbl(path, abis):
    by_name, by_nr = {}, collections.defaultdict(set)
    for ln in open(path):
        p = ln.split()
        if not p or p[0].startswith("#") or len(p) < 3 or p[1] not in abis:
            continue
        n, name = int(p[0]), p[2]
        by_name[name] = n
        by_nr[n].add(name)
    return by_name, by_nr

X86, X86N = linux_tbl(f"{T}/linux_x86_64.tbl", {"common", "64"})
GEN, GENN = linux_tbl(f"{T}/linux_generic.tbl", {"common", "64", "renameat", "rlimit", "memfd_secret"})
for arch, tab in (("x86_64", X86), ("aarch64", GEN)):
    for m in re.finditer(r"#define __NR_(\w+)\s+(\d+)", open(f"{T}/musl_{arch}.txt").read()):
        name, n = m.group(1), int(m.group(2))
        if name in tab and tab[name] != n:
            print(f"!! musl and Linux disagree on {arch} {name}: musl {n}, linux {tab[name]}")

# Constant-name spellings that are not the kernel's table name.
ALIAS = {"fstatat": "newfstatat", "newfstat": "fstat", "fstatat64": "newfstatat", "pread": "pread64",
         "pwrite": "pwrite64", "getdents": "getdents", "wait4": "wait4", "uname": "uname",
         "sigaction": "rt_sigaction", "sigprocmask": "rt_sigprocmask", "sigreturn": "rt_sigreturn",
         "exit": "exit", "mmap2": "mmap", "statx": "statx", "fadvise": "fadvise64"}

def canon(const):
    n = const.lower()
    for pre in ("sys_", "nr_", "__nr_"):
        if n.startswith(pre):
            n = n[len(pre):]
    return ALIAS.get(n, n)

findings = collections.defaultdict(list)
unresolved = collections.defaultdict(list)
checked = collections.Counter()

def check_const(table, arch, const, value, where):
    truth, truth_n = (X86, X86N) if arch == "x86_64" else (GEN, GENN)
    name = canon(const)
    checked[table] += 1
    if name in truth:
        if truth[name] != value:
            findings["A wrong number"].append(f"{where}: {table} {const} = {value}, but {arch} {name} is {truth[name]}"
                                              + (f" ({value} is `{'/'.join(sorted(truth_n[value]))}`)" if value in truth_n else ""))
    else:
        if value in truth_n:
            findings["B private/unknown name on a real number"].append(
                f"{where}: {table} {const} = {value} is not a Linux name, and {arch} {value} is `{'/'.join(sorted(truth_n[value]))}`")
        else:
            unresolved[table].append(f"{const}={value}")

def block(text, opener):
    i = text.find(opener)
    if i < 0:
        return "", 0
    j, depth = text.index("{", i), 0
    for k in range(j, len(text)):
        depth += {"{": 1, "}": -1}.get(text[k], 0)
        if depth == 0:
            return text[j:k], text[:j].count("\n") + 1
    return text[j:], 0

CONST = re.compile(r"pub const ([A-Z][A-Z0-9_]*): *u(?:64|size) *= *(0x[0-9a-fA-F]+|\d+);")

def scan_consts(path, opener, table, arch):
    text = open(os.path.join(REPO, path)).read()
    body, base = block(text, opener) if opener else (text, 1)
    vals = {}
    for m in CONST.finditer(body):
        v = int(m.group(2), 0)
        vals[m.group(1)] = v
        check_const(table, arch, m.group(1), v, f"{path}:{base + body[:m.start()].count(chr(10))}")
    return vals

nr_generic = scan_consts("crates/akuma-syscalls-linux/src/nr.rs", None, "akuma-syscalls-linux::nr", "aarch64")
scan_consts("userspace/libakuma/src/lib.rs", "mod aarch64_nr {", "libakuma aarch64_nr", "aarch64")
scan_consts("userspace/libakuma/src/lib.rs", "mod x86_64_nr {", "libakuma x86_64_nr", "x86_64")
scan_consts("crates/akuma-ring3-amd64/src/lib.rs", "pub mod nr {", "akuma-ring3-amd64::nr", "x86_64")

# akuma-syscalls-abi rows: `Variant => CONST = x86, nr::PATH;`
abi_path = "crates/akuma-syscalls-abi/src/lib.rs"
abi = open(os.path.join(REPO, abi_path)).read()
for m in re.finditer(r"^\s+([A-Z][A-Za-z0-9]*)\s+=>\s+([A-Z0-9_]+)\s+=\s+(\d+),\s*nr::([A-Z0-9_]+)\s*;", abi, re.M):
    var, const, x86, path = m.group(1), m.group(2), int(m.group(3)), m.group(4)
    line = abi[:m.start()].count("\n") + 1
    check_const("akuma-syscalls-abi (x86 half)", "x86_64", const, x86, f"{abi_path}:{line}")
    if path in nr_generic and canon(path) != canon(const) and canon(path) in GEN and canon(const) in GEN:
        findings["C row pairs two different calls"].append(f"{abi_path}:{line}: {var}: x86 half `{canon(const)}`, aarch64 half nr::{path} (`{canon(path)}`)")

# Userspace: only sshd matters (it calls through libakuma, checked above).
for d, _, fs in __import__("os").walk(os.path.join(REPO, "userspace/sshd/src")):
    for f in fs:
        if not f.endswith(".rs"):
            continue
        rel = os.path.relpath(os.path.join(d, f), REPO)
        for i, ln in enumerate(open(os.path.join(REPO, rel)).read().split("\n")):
            m = re.search(r"const (SYS_[A-Z0-9_]+): *[ui](?:64|size|32) *= *(\d+);", ln)
            if m:
                check_const("sshd SYS_*", "aarch64", m.group(1), int(m.group(2)), f"{rel}:{i + 1}")

# Literal dispatch arms and comment claims in the x86_64 kernels.
NAMES_X86 = set(X86)
def names_in(s):
    return {w for w in re.findall(r"`?([a-z][a-z0-9_]{2,})`?", s) if w in NAMES_X86}

for rel in ["amd64/src/usermode.rs"] + sorted("taoxichuan/src/" + f for f in os.listdir(os.path.join(REPO, "taoxichuan/src")) if f.endswith(".rs")):
    lines = open(os.path.join(REPO, rel)).read().split("\n")
    for i, ln in enumerate(lines):
        code = ln.split("//")[0]
        nums = []
        m = re.match(r"\s*((?:\d+\s*\|\s*)*\d+)\s*=>", code)
        if m:
            nums = [int(x) for x in re.findall(r"\d+", m.group(1))]
        for x in re.findall(r"\bnr\s*==\s*(\d+)\b|\brax\s*==\s*(\d+)\b", code):
            nums += [int(v) for v in x if v]
        nums = [n for n in nums if n in X86N or 300 <= n < 500]
        if not nums:
            continue
        # The comment directly above (contiguous // lines) and on the same line.
        j, cmt = i - 1, [ln.split("//", 1)[1] if "//" in ln else ""]
        while j >= 0 and lines[j].strip().startswith("//") and i - j <= 8:
            cmt.append(lines[j].strip()[2:]); j -= 1
        said = names_in(" ".join(cmt))
        if not said:
            continue
        real = set().union(*(X86N.get(n, set()) for n in nums))
        if not (said & real) and len(nums) == 1:
            findings["D arm comment names another call"].append(
                f"{rel}:{i + 1}: arm {nums} is `{'/'.join(sorted(real)) or '?'}`, comment names {', '.join(sorted(said))}")

CLAIM = [re.compile(r"`?([a-z][a-z0-9_]{2,})(?:\(2\))?`?\s*=\s*(\d{1,3})\b"),
         re.compile(r"`?([a-z][a-z0-9_]{2,})(?:\(2\))?`?\s*\((\d{2,3})\)"),
         re.compile(r"`?([a-z][a-z0-9_]{2,})(?:\(2\))?`?[^.;`]{0,40}?\bx86_64\s+(?:nr\s+)?(\d{1,3})\b"),
         re.compile(r"\bx86_64\s+(?:nr\s+)?(\d{1,3})\b[^.;]{0,12}?`([a-z][a-z0-9_]{2,})`", re.I)]
claim_files = ["amd64/src/usermode.rs", "crates/akuma-ring3-amd64/src/lib.rs", "crates/akuma-syscalls-abi/src/lib.rs"] + \
    sorted("taoxichuan/src/" + f for f in os.listdir(os.path.join(REPO, "taoxichuan/src")) if f.endswith(".rs"))
for rel in claim_files:
    for i, ln in enumerate(open(os.path.join(REPO, rel)).read().split("\n")):
        if "//" not in ln:
            continue
        c = ln.split("//", 1)[1]
        for k, rx in enumerate(CLAIM):
            for a, b in rx.findall(c):
                name, n = (b, int(a)) if k == 3 else (a, int(b))
                if name in X86 and X86[name] != n and GEN.get(name) != n and n in X86N:
                    findings["E comment states a wrong x86 number"].append(
                        f"{rel}:{i + 1}: says {name} {n}; x86_64 {name} is {X86[name]}, {n} is `{'/'.join(sorted(X86N[n]))}`")

# The claim patterns must see a plain "`getpid(2)` - x86_64 38" (a check that once ran blind).
assert any(rx.search("`getpid(2)` - x86_64 38") for rx in CLAIM), "claim patterns are blind"

# Private aarch64 block must not shadow real asm-generic numbers.
for const, v in nr_generic.items():
    if canon(const) not in GEN and v in GENN:
        pass  # already reported in B

print("checked constants per table:", dict(checked))
for k in sorted(findings):
    print(f"\n== {k} ({len(findings[k])})")
    for f in findings[k]:
        print("  " + f)
print("\n== names that are not Linux syscalls (private calls or spellings to review)")
for t, xs in unresolved.items():
    print(f"  {t}: {', '.join(xs)}")
