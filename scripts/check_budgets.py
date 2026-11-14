#!/usr/bin/env python3
"""Fail when a component gains copying calls or `mut` it has not been budgeted for.

The same gate as `scripts/check_unsafe_budget.py`, for two more measures that each
mark code worth a second look in review:

  * `copy` - calls that copy their receiver: `.clone()`, `.cloned()`, `.to_vec()`,
    `.to_owned()`, `.to_string()`, and this tree's whole-`FileDescriptor` copiers
    `get_fd(` / `table_get(` (`COPY_METHODS` / `FD_COPIERS` in `cloc_akuma.py`).
    `.clone()` alone undercounts: the costliest copies an audit found were
    `.cloned()` of a whole fd on the syscall path, not `.clone()`.
  * `mut` - `mut` keywords (`let mut`, `&mut`, `mut self`, `static mut`).

Both are counted by `scripts/cloc_akuma.py`'s lexer in production code only - not
in comments, strings, `#[cfg(test)]` items or test files - per component (`src`,
`amd64`, `taoxichuan`, `crates/<name>`). Budgets: `docs/reference/copy-budget.json`
and `docs/reference/mut-budget.json`. Background:
`docs/archive/CLONE_AND_MUT_BUDGETS.md`.

A change fails the gate if a component's count exceeds its budget (a component with
no entry has a budget of 0 - a new crate starts at zero). The way through is
deliberate: `--update`, and the budget file's diff carries the increase into review
beside the code that needed it. A decrease is reported and never fails; `--update`
ratchets it down so it cannot creep back.

    scripts/check_budgets.py                 # check both (exit 1 on a violation)
    scripts/check_budgets.py --update        # rewrite both budgets from the tree
    scripts/check_budgets.py copy            # one measure
    scripts/check_budgets.py --self-test
"""
import json
import os
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PATHS = ["src", "crates", os.path.join("taoxichuan", "src")]
#: measure -> (JSON field from `cloc_akuma.py --json`, budget file, what it counts)
MEASURES = {
    "copy": ("copy_sites", os.path.join(ROOT, "docs", "reference", "copy-budget.json"), "copying calls"),
    "mut": ("mut_sites", os.path.join(ROOT, "docs", "reference", "mut-budget.json"), "`mut` keywords"),
}


def measure():
    """`{component: {measure: production sites}}` for the tree."""
    out = subprocess.run(
        [sys.executable, os.path.join(ROOT, "scripts", "cloc_akuma.py"), "--json", *PATHS],
        capture_output=True, text=True, cwd=ROOT, check=True,
    ).stdout
    comps = json.loads(out)["by_component"]
    return {name: {m: c[field] for m, (field, _, _) in MEASURES.items()} for name, c in comps.items()}


def compare(now, budget, what):
    """`(violations, notes)` of `now` (`{component: n}`) against `budget` (`{component: n}`)."""
    violations, notes = [], []
    for name in sorted(now):
        n, allowed = now[name], budget.get(name, 0)
        if n > allowed:
            violations.append(f"{name}: {n} {what}, budget {allowed} (+{n - allowed})")
        elif n < allowed:
            notes.append(f"{name}: {n} {what}, budget {allowed} - run --update to ratchet it down")
    return violations, notes


def to_budget(now):
    return {name: n for name, n in sorted(now.items()) if n > 0}


def self_test():
    budget = {"a": 4}
    assert compare({"a": 4, "c": 0}, budget, "x") == ([], [])
    v, _ = compare({"a": 5}, budget, "x")
    assert v == ["a: 5 x, budget 4 (+1)"], v
    v, _ = compare({"a": 4, "c": 1}, budget, "x")
    assert v == ["c: 1 x, budget 0 (+1)"], "a new component starts at zero"
    v, n = compare({"a": 3}, budget, "x")
    assert v == [] and len(n) == 1, "a decrease is a note, never a failure"
    assert to_budget({"a": 4, "b": 0}) == {"a": 4}
    print("check_budgets: self-test ok")


def main(argv):
    if "--self-test" in argv:
        self_test()
        return 0
    chosen = [a for a in argv if not a.startswith("-")] or list(MEASURES)
    unknown = [m for m in chosen if m not in MEASURES]
    if unknown:
        print(f"check_budgets: unknown measure {unknown}; have {list(MEASURES)}", file=sys.stderr)
        return 2
    tree = measure()
    failed = False
    for m in chosen:
        _, path, what = MEASURES[m]
        now = {name: counts[m] for name, counts in tree.items()}
        rel = os.path.relpath(path, ROOT)
        if "--update" in argv:
            new = to_budget(now)
            with open(path, "w") as fh:
                json.dump(new, fh, indent=2, sort_keys=True)
                fh.write("\n")
            print(f"check_budgets: wrote {rel} ({sum(new.values())} {what} in {len(new)} components)")
            continue
        try:
            with open(path) as fh:
                budget = json.load(fh)
        except FileNotFoundError:
            print(f"check_budgets: no {rel}; run with --update", file=sys.stderr)
            failed = True
            continue
        violations, notes = compare(now, budget, what)
        for line in notes:
            print(f"note: {line}")
        for line in violations:
            print(f"ERROR: {line}", file=sys.stderr)
        if violations:
            failed = True
        else:
            print(f"check_budgets: {m} ok ({sum(now.values())} production {what}, within budget)")
    if failed:
        print("An increase needs a reason in review: run `scripts/check_budgets.py --update` "
              "and the budget diff carries it.", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
