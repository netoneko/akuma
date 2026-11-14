#!/usr/bin/env python3
"""Fail when a component gains untyped address parameters it has not been budgeted for.

`docs/archive/NEWTYPE_AUDIT.md` (2026-11-05) found 248 `fn` parameters named like an address and typed
`usize`/`u64` in the mm-domain, where `PhysAddr`/`VirtAddr`/`Pfn` (`akuma_primitives::addr`) now exist. This is the
ratchet that audit asked for: the same shape as `check_unsafe_budget.py` - a component may not gain such a
parameter without the budget file's diff carrying it into review, and a decrease ratchets down with `--update`.

What counts: a parameter of a production `fn` whose name is an address word (`va`, `vaddr`, `pa`, `paddr`, `pfn`,
`phys`, `addr`, `base`, also with a `_suffix` or `prefix_`) and whose type is exactly `usize` or `u64`. Test files,
`tests.rs`, `tests/` directories and inline `#[cfg(test)] mod name { .. }` blocks are excluded. Greps, not a
parser: it matches the audit's own method (its precision is "context-read, not exact").

    scripts/check_addr_params.py            # check (exit 1 on a violation)
    scripts/check_addr_params.py --update   # rewrite the budget from the tree
    scripts/check_addr_params.py --list [COMPONENT]   # the sites, for conversion sweeps
    scripts/check_addr_params.py --self-test
"""
import json
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BUDGET = os.path.join(ROOT, "docs", "reference", "addr-params-budget.json")
PATHS = ["src", "crates", os.path.join("taoxichuan", "src")]

NAME = re.compile(r"^(?:\w+_)?(?:va|vaddr|pa|paddr|pfn|phys|addr|base)(?:_\w+)?$")
FN = re.compile(r"\bfn\s+(\w+)\s*(?:<[^>(]*>)?\s*\(")


def component_of(rel):
    parts = rel.split(os.sep)
    if parts[0] == "crates" and len(parts) > 1:
        return "/".join(parts[:2])
    if parts[0] == "taoxichuan":
        return "taoxichuan/src"
    return parts[0] if parts[0] != "src" else "src"


def is_test_path(rel):
    base = os.path.basename(rel)
    return base in ("tests.rs", "test_support.rs") or base.endswith("_tests.rs") or f"{os.sep}tests{os.sep}" in rel


def split_params(sig):
    """Top-level comma split of a parameter list (depth over (), [], <>, {})."""
    out, depth, cur = [], 0, []
    for ch in sig:
        if ch in "([<{":
            depth += 1
        elif ch in ")]>}":
            depth -= 1
        if ch == "," and depth == 0:
            out.append("".join(cur))
            cur = []
        else:
            cur.append(ch)
    if "".join(cur).strip():
        out.append("".join(cur))
    return out


def sites_in(text):
    """`[(line, fn, param)]` of untyped-address parameters in the production part of `text`."""
    # Blank out every inline `#[cfg(test)] mod name { ... }` block (brace-matched), keeping line numbers. Test modules
    # can sit between production items (akuma-pmm has five), so truncating at the first one would hide the rest.
    for m in list(re.finditer(r"#\[cfg\(test\)\]\s*(?:pub\s+)?mod\s+\w+\s*\{", text))[::-1]:
        k, depth = m.end(), 1
        while k < len(text) and depth:
            depth += {"{": 1, "}": -1}.get(text[k], 0)
            k += 1
        block = text[m.start():k]
        text = text[: m.start()] + re.sub(r"[^\n]", " ", block) + text[k:]
    found = []
    for m in FN.finditer(text):
        i, depth = m.end(), 1
        while i < len(text) and depth:
            depth += {"(": 1, ")": -1}.get(text[i], 0)
            i += 1
        sig = text[m.end(): i - 1]
        for p in split_params(sig):
            if ":" not in p:
                continue
            name, ty = p.split(":", 1)
            name = name.strip().lstrip("&").removeprefix("mut ").strip()
            if NAME.match(name) and ty.strip() in ("usize", "u64"):
                found.append((text.count("\n", 0, m.start()) + 1, m.group(1), name))
    return found


def walk():
    """`{component: [(relpath, line, fn, param)]}`."""
    res = {}
    for top in PATHS:
        for dp, dn, fn in os.walk(os.path.join(ROOT, top)):
            dn[:] = [d for d in dn if d not in ("target", ".git", "node_modules")]
            for f in fn:
                if not f.endswith(".rs"):
                    continue
                full = os.path.join(dp, f)
                rel = os.path.relpath(full, ROOT)
                if is_test_path(rel):
                    continue
                try:
                    text = open(full, encoding="utf-8").read()
                except (OSError, UnicodeDecodeError):
                    continue
                for line, func, param in sites_in(text):
                    res.setdefault(component_of(rel), []).append((rel, line, func, param))
    return res


def compare(now, budget):
    violations, notes = [], []
    for name in sorted(now):
        n, allowed = now[name], budget.get(name, 0)
        if n > allowed:
            violations.append(f"{name}: {n} untyped address params, budget {allowed} (+{n - allowed})")
        elif n < allowed:
            notes.append(f"{name}: {n} untyped address params, budget {allowed} - run --update to ratchet it down")
    return violations, notes


def self_test():
    src = "pub fn a(va: usize, len: usize) {}\nfn b(&self, pa: u64, mut base_pa: usize) -> u8 { 0 }\nfn c(v: PhysAddr, n: usize) {}\n#[cfg(test)]\nfn t1(va: usize) {}\n#[cfg(test)]\nmod tests { fn d(va: usize) { if true { } } }\nfn after(pa: u64) {}"
    got = [(f, p) for _, f, p in sites_in(src)]
    assert got == [("a", "va"), ("b", "pa"), ("b", "base_pa"), ("t1", "va"), ("after", "pa")], got
    assert compare({"x": 3}, {"x": 3}) == ([], [])
    assert compare({"x": 4}, {"x": 3})[0] == ["x: 4 untyped address params, budget 3 (+1)"]
    assert component_of(os.path.join("crates", "akuma-pmm", "src", "lib.rs")) == "crates/akuma-pmm"
    print("self-test ok")


def main(argv):
    if "--self-test" in argv:
        return self_test() or 0
    res = walk()
    now = {k: len(v) for k, v in res.items()}
    if "--list" in argv:
        want = [a for a in argv if not a.startswith("--")]
        for comp in sorted(res):
            if want and comp not in want:
                continue
            for rel, line, func, param in res[comp]:
                print(f"{rel}:{line}: {func}({param})")
        return 0
    if "--update" in argv:
        with open(BUDGET, "w") as f:
            json.dump({"untyped_address_params": dict(sorted((k, v) for k, v in now.items() if v))}, f, indent=2)
            f.write("\n")
        print(f"check_addr_params: wrote {os.path.relpath(BUDGET, ROOT)} ({sum(now.values())} sites in {len(now)} components)")
        return 0
    budget = json.load(open(BUDGET)).get("untyped_address_params", {}) if os.path.exists(BUDGET) else {}
    violations, notes = compare(now, budget)
    for n in notes:
        print("note:", n)
    if violations:
        for v in violations:
            print("ERROR:", v)
        print("An untyped address parameter needs a reason in review: use PhysAddr/VirtAddr/Pfn, or run --update.")
        return 1
    print(f"check_addr_params: ok ({sum(now.values())} untyped address params, within budget)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
