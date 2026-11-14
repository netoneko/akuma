#!/usr/bin/env python3
"""Linked production Rust lines of a kernel package: the workspace crates in its dependency closure.

usage: python3 -I scripts/benchmarks/linked_loc.py <package> [--features F] [--no-default-features]
                                                   [--target x86_64-unknown-none] [--vs <package> [--vs-features F]]

Counts non-blank, non-comment lines of `src/**/*.rs` per workspace crate, skipping `tests.rs`, `*_tests.rs`,
`tests/`, `benches/` and `#[cfg(test)]` items (brace matched). `///` and `//!` lines are reported separately.
It is the CRATE closure, not a symbol-level link: feature-gated or unreachable code inside a linked crate
counts, third-party registry crates do not. Heuristic comment stripping; good to a percent, not to a line.
First used for docs/denver archive: denver/docs/archive/AUTORESEARCH_EXPERIMENT_RESULTS.md.
"""
import argparse, os, re, subprocess, sys

ROOT = subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip()


def closure(pkg, features, no_default, target):
    cmd = ["cargo", "tree", "--offline", "-p", pkg, "--target", target, "-e", "normal", "--prefix", "none", "--format", "{p}"]
    if no_default: cmd.append("--no-default-features")
    if features: cmd += ["--features", features]
    out = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True, check=True).stdout
    d = {}
    for l in out.splitlines():
        m = re.match(r"(\S+) v\S+ \((/[^)]+)\)", l.strip())
        if m: d[m.group(1)] = m.group(2)
    return d


def strip(src):
    out, i, n = [], 0, len(src)
    pat = re.compile(r"#\[cfg\(test\)\]")
    while True:
        m = pat.search(src, i)
        if not m: out.append(src[i:]); break
        out.append(src[i:m.start()]); j = m.end()
        while j < n and src[j] not in "{;": j += 1
        if j < n and src[j] == "{":
            depth = 0
            while j < n:
                depth += (src[j] == "{") - (src[j] == "}"); j += 1
                if depth == 0: break
        else: j += 1
        i = j
    s = re.sub(r"/\*.*?\*/", "", "".join(out), flags=re.S)
    code = doc = 0
    for line in s.split("\n"):
        t = line.strip()
        if not t: continue
        if t.startswith(("///", "//!")): doc += 1
        elif not t.startswith("//"): code += 1
    return code, doc


def crate_loc(path):
    code = doc = 0
    for dp, dn, fn in os.walk(os.path.join(path, "src")):
        dn[:] = [d for d in dn if d not in ("tests", "benches")]
        for f in fn:
            if not f.endswith(".rs") or f == "tests.rs" or f.endswith("_tests.rs") or f.startswith("test_"): continue
            s = open(os.path.join(dp, f), errors="replace").read()
            if re.match(r"\s*(//[^\n]*\n\s*)*#!\[cfg\(test\)\]", s): continue
            c, d = strip(s); code += c; doc += d
    return code, doc


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("package"); ap.add_argument("--features", default="")
    ap.add_argument("--no-default-features", action="store_true")
    ap.add_argument("--target", default="x86_64-unknown-none")
    ap.add_argument("--vs"); ap.add_argument("--vs-features", default="")
    a = ap.parse_args()
    one = {n: crate_loc(p) for n, p in closure(a.package, a.features, a.no_default_features, a.target).items()}
    print(f"{a.package}: {len(one)} workspace crates, code={sum(v[0] for v in one.values())} doc={sum(v[1] for v in one.values())}")
    if a.vs:
        two = {n: crate_loc(p) for n, p in closure(a.vs, a.vs_features, False, a.target).items()}
        print(f"{a.vs}: {len(two)} workspace crates, code={sum(v[0] for v in two.values())} doc={sum(v[1] for v in two.values())}")
        sh = set(one) & set(two)
        print(f"shared: {len(sh)} crates, code={sum(one[n][0] for n in sh)}")
        for label, names, d in ((f"{a.package}-only", set(one) - sh, one), (f"{a.vs}-only", set(two) - sh, two)):
            print(f"{label}: " + ", ".join(f"{n}={d[n][0]}" for n in sorted(names)) + f"  (total {sum(d[n][0] for n in names)})")


main()
