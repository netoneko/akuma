#!/usr/bin/env python3
"""Fail when a component gains `unsafe` it has not been budgeted for.

`docs/archive/POST_AMD64_CODE_AUDIT.md` §2.3 observed that `unsafe` stopped
growing in September 2026 "by discipline while the amd64 port had full
attention", and asked for a gate so that survives the next port. This is it.

The budget is `docs/reference/unsafe-budget.json`: for every component
(`src`, `amd64`, `crates/<name>`) the number of production `unsafe` *sites* it is
allowed, and the list of components that carry `#![forbid(unsafe_code)]`.
Sites, not lines: sites measure blast radius, lines measure volume
(`scripts/cloc_akuma.py` counts both, with a lexer that is right about
attributes, raw strings and `unsafe fn` bodies where `grep -c unsafe` is not).

A change fails the gate if

  * a component's production `unsafe` sites exceed its budget (a component with
    no entry has a budget of 0 - a new crate starts at zero), or
  * a component that forbade `unsafe` no longer does.

The way through is deliberate: run `--update`, and the *budget file's diff*
carries the increase into review next to the code that needed it. That is the
whole mechanism; there is no allowlist to forget to edit. A decrease is reported
and never fails - `--update` ratchets it down so it cannot creep back.

    scripts/check_unsafe_budget.py            # check (exit 1 on a violation)
    scripts/check_unsafe_budget.py --update   # rewrite the budget from the tree
    scripts/check_unsafe_budget.py --self-test
"""
import json
import os
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BUDGET = os.path.join(ROOT, "docs", "reference", "unsafe-budget.json")
PATHS = ["src", "crates", "amd64"]


def measure():
    """`{component: (production unsafe sites, forbids unsafe)}` for the tree."""
    out = subprocess.run(
        [sys.executable, os.path.join(ROOT, "scripts", "cloc_akuma.py"), "--json", *PATHS],
        capture_output=True, text=True, cwd=ROOT, check=True,
    ).stdout
    comps = json.loads(out)["by_component"]
    return {name: (c["unsafe_sites"], bool(c["forbids_unsafe"])) for name, c in comps.items()}


def compare(now, budget):
    """`(violations, notes)` of the measured tree against the budget."""
    sites = budget.get("unsafe_sites", {})
    forbids = set(budget.get("forbids", []))
    violations, notes = [], []
    for name in sorted(now):
        n, forbid = now[name]
        allowed = sites.get(name, 0)
        if n > allowed:
            violations.append(f"{name}: {n} unsafe sites, budget {allowed} (+{n - allowed})")
        elif n < allowed:
            notes.append(f"{name}: {n} unsafe sites, budget {allowed} - run --update to ratchet it down")
        if name in forbids and not forbid:
            violations.append(f"{name}: no longer forbids unsafe_code")
    return violations, notes


def to_budget(now):
    return {
        "unsafe_sites": {name: n for name, (n, _) in sorted(now.items()) if n > 0},
        "forbids": sorted(name for name, (_, forbid) in now.items() if forbid),
    }


def self_test():
    budget = {"unsafe_sites": {"a": 4}, "forbids": ["b"]}
    ok = {"a": (4, False), "b": (0, True), "c": (0, False)}
    assert compare(ok, budget) == ([], [])
    v, _ = compare({**ok, "a": (5, False)}, budget)
    assert v == ["a: 5 unsafe sites, budget 4 (+1)"], v
    v, _ = compare({**ok, "c": (1, False)}, budget)
    assert v == ["c: 1 unsafe sites, budget 0 (+1)"], "a new component starts at zero"
    v, _ = compare({**ok, "b": (0, False)}, budget)
    assert v == ["b: no longer forbids unsafe_code"], v
    v, n = compare({**ok, "a": (3, False)}, budget)
    assert v == [] and len(n) == 1, "a decrease is a note, never a failure"
    assert to_budget(ok) == {"unsafe_sites": {"a": 4}, "forbids": ["b"]}
    print("check_unsafe_budget: self-test ok")


def main(argv):
    if "--self-test" in argv:
        self_test()
        return 0
    now = measure()
    if "--update" in argv:
        new = to_budget(now)
        with open(BUDGET, "w") as fh:
            json.dump(new, fh, indent=2, sort_keys=True)
            fh.write("\n")
        total = sum(new["unsafe_sites"].values())
        print(f"check_unsafe_budget: wrote {os.path.relpath(BUDGET, ROOT)} ({total} sites in "
              f"{len(new['unsafe_sites'])} components; {len(new['forbids'])} forbid)")
        return 0
    try:
        with open(BUDGET) as fh:
            budget = json.load(fh)
    except FileNotFoundError:
        print(f"check_unsafe_budget: no {os.path.relpath(BUDGET, ROOT)}; run with --update", file=sys.stderr)
        return 1
    violations, notes = compare(now, budget)
    for line in notes:
        print(f"note: {line}")
    if violations:
        for line in violations:
            print(f"ERROR: {line}", file=sys.stderr)
        print("An `unsafe` increase needs a reason in review: run "
              "`scripts/check_unsafe_budget.py --update` and the budget diff carries it.",
              file=sys.stderr)
        return 1
    total = sum(n for n, _ in now.values())
    print(f"check_unsafe_budget: ok ({total} production unsafe sites, within budget)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
