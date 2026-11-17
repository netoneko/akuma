#!/usr/bin/env python3
"""Which static spinlocks can an interrupt handler reach? A name-based call graph, so an OVER-approximation.

For each `static NAME: Spinlock<..>` (see `spinlock_audit.py`), report whether some interrupt entry point can reach a function
that calls `NAME.lock()`/`try_lock()`, with one example path. Edges are "function A's body contains the identifier `b(`" for
any function named `b` anywhere in the tree, so two unrelated functions sharing a name are merged: a hit is a lead to read, not
a proof; a lock reported **unreachable** is the useful answer (no name-based path exists at all).

    scripts/spinlock_irq_reach.py            # summary and the reachable locks with a path
    scripts/spinlock_irq_reach.py --all      # every lock
    scripts/spinlock_irq_reach.py --loose    # follow duplicated names too (everything becomes reachable; for comparison)
"""
import collections
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "scripts"))
SKIP = {"target", ".git", "overlays", "userspace", "node_modules"}

# Interrupt entry points: the registered handlers and the per-architecture tick/IPI/trap entries.
ROOTS = {
    "dispatch_irq", "timer_irq_handler", "on_timer_interrupt", "nic_irq_handler", "probe_irq_nop",
    "el0_irq", "el1_irq",                                   # aarch64 vector entries (akuma-entry/hooked.rs)
    "timer_trap", "timer_interrupt_entry", "on_tick",       # amd64 timer
}
# amd64's shootdown handler is called `handler` inside shootdown.rs: handled by file below.
EXTRA_ROOT_FILES = {("crates/akuma-kernel-amd64/src/shootdown.rs", "handler"), ("crates/akuma-kernel-amd64/src/shootdown.rs", "service_pending")}


def is_test(rel):
    b = os.path.basename(rel)
    return b in ("tests.rs", "test_support.rs") or b.endswith("_tests.rs") or "/tests/" in rel or "kernel-tests" in rel


FN = re.compile(r"\bfn\s+(\w+)\s*(?:<[^>(]*>)?\(")


def body_after(t, i):
    j = t.find("{", i)
    sem = t.find(";", i)
    if j < 0 or (0 <= sem < j):
        return None
    d, k = 0, j
    while k < len(t):
        c = t[k]
        if c == "{":
            d += 1
        elif c == "}":
            d -= 1
            if d == 0:
                return t[j:k + 1]
        k += 1
    return t[j:]


def load():
    fns = collections.defaultdict(list)       # name -> [(rel, body)]
    for dp, dn, fn in os.walk(ROOT):
        dn[:] = [d for d in dn if d not in SKIP]
        for f in fn:
            if not f.endswith(".rs"):
                continue
            p = os.path.join(dp, f)
            rel = os.path.relpath(p, ROOT)
            if is_test(rel):
                continue
            t = open(p, encoding="utf-8", errors="ignore").read()
            for m in FN.finditer(t):
                b = body_after(t, m.end())
                if b is not None:
                    fns[m.group(1)].append((rel, b))
    return fns


DECL = re.compile(r"^\s*(?:pub(?:\([a-z]+\))?\s+)?static\s+([A-Z][A-Z0-9_]*)\s*:\s*((?:[\w:]+::)?(?:Irq)?Spinlock)\s*<", re.M)


def statics():
    out = []
    for dp, dn, fn in os.walk(ROOT):
        dn[:] = [d for d in dn if d not in SKIP]
        for f in fn:
            if f.endswith(".rs"):
                p = os.path.join(dp, f)
                rel = os.path.relpath(p, ROOT)
                if is_test(rel):
                    continue
                t = open(p, encoding="utf-8", errors="ignore").read()
                for m in DECL.finditer(t):
                    out.append((m.group(1), m.group(2), rel))
    return out


def main(argv):
    fns = load()
    # Follow an edge only to a name defined in exactly one place: `new`, `read`, `init`, `drop`... exist dozens of times, and
    # merging them makes everything reachable from everything. The cost: a path through a duplicated name (a trait method)
    # is missed, so "not reachable" here means "no unique-name path", not "provably unreachable".
    names = {n for n, b in fns.items() if len(b) == 1 or "--loose" in argv}
    calls = {}
    for name, bodies in fns.items():
        if name not in names:
            continue
        s = set()
        for _, b in bodies:
            for m in re.finditer(r"\b(\w+)\s*\(", b):
                if m.group(1) in names and m.group(1) != name:
                    s.add(m.group(1))
        calls[name] = s
    roots = {r for r in ROOTS if r in fns}
    parent = {r: None for r in roots}
    q = list(roots)
    while q:
        a = q.pop(0)
        for b in calls.get(a, ()):
            if b not in parent:
                parent[b] = a
                q.append(b)

    def path(n):
        p = []
        while n is not None:
            p.append(n)
            n = parent[n]
        return " <- ".join(p)

    res = []
    for name, ty, rel in statics():
        pat = re.compile(r"\b" + name + r"\s*\.\s*(?:lock|try_lock)\s*\(")
        hits = sorted({fn for fn, bodies in fns.items() for _, b in bodies if pat.search(b)})
        reach = [h for h in hits if h in parent]
        res.append((name, ty, rel, hits, reach))
    reachable = [r for r in res if r[4]]
    print(f"{len(res)} static locks; {len(roots)} interrupt roots found ({', '.join(sorted(roots))})")
    print(f"{len(reachable)} reachable from an interrupt entry by name; {len(res) - len(reachable)} not reachable")
    for name, ty, rel, hits, reach in sorted(res, key=lambda r: (not r[4], r[2])):
        if not reach and "--all" not in argv:
            continue
        tag = "IrqSpinlock" if ty.endswith("IrqSpinlock") else "Spinlock"
        print(f"  {'REACH' if reach else 'no   '} {name:26} {tag:12} {rel}")
        if reach:
            print(f"        via {path(reach[0])}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
