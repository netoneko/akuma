#!/usr/bin/env python3
"""Fail when a component gains untyped user-pointer parameters it has not been budgeted for.

`docs/archive/TYPE_SYSTEM_SAFETY_REPORT.md` §8 (stage B of the user boundary): syscall handlers take their user pointers as
bare `u64`/`usize` (`*_ptr`), validate and copy in separate calls, and offset them with plain arithmetic. `UserAddr`
(`akuma_user_access`) is the type that has a checked `offset_of`/`add` and a `slice` that validates; this is the ratchet
that stops the raw form spreading while handlers are converted crate by crate, the same shape as `check_addr_params.py`
(it reuses that script's parser): a component may not gain such a parameter without the budget file's diff carrying it
into review, and a decrease ratchets down with `--update`.

What counts: a parameter of a production `fn` whose name is `ptr` or ends in `_ptr` and whose type is exactly `u64` or
`usize`. Tests and `#[cfg(test)] mod` blocks are excluded.

    scripts/check_user_ptr_params.py            # check (exit 1 on a violation)
    scripts/check_user_ptr_params.py --update   # rewrite the budget from the tree
    scripts/check_user_ptr_params.py --list [COMPONENT]
"""
import importlib.util
import json
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("check_addr_params", os.path.join(HERE, "check_addr_params.py"))
base = importlib.util.module_from_spec(spec)
spec.loader.exec_module(base)

base.NAME = re.compile(r"^(?:\w+_)?ptr$|^\w+_ptr$")

# Not user pointers, and the boundary primitives that are *where* a raw address becomes a checked one. Classified by hand on
# 2026-11-16 (docs/reference/safety-types.md "Untyped pointer classification"); everything else that matches is a real user
# pointer that should become `UserAddr`.
NOT_USER_COMPONENTS = {
    "crates/akuma-alloc",       # `ptr` is a kernel heap pointer (allocation tracking)
    "crates/akuma-switch",      # `closure_ptr`: a kernel pointer to a boxed closure
    "crates/akuma-kernel",      # `dtb_ptr`: the device-tree blob the bootloader handed over
    "crates/akuma-kernel-glue", # `dtb_ptr`
    "crates/akuma-threading",   # a test helper's `ptr`
}
BOUNDARY_FILES = {
    "crates/akuma-user-access/src/lib.rs",           # the validators and the cstr walk: raw in, proof out
    "crates/akuma-kernel-amd64/src/uaccess.rs",      # amd64's range checks and byte copies, the same layer
}

# Boundary helpers inside otherwise-converted files: glue's own `validate_user_ptr` and `copy_from_user_str` sit beside the
# `akuma_user_access` copy family and take the raw address like it does; everything above them passes a `UserAddr`.
BOUNDARY_FNS = {
    ("crates/akuma-syscalls-glue/src/lib.rs", "validate_user_ptr"),
    ("crates/akuma-syscalls-glue/src/lib.rs", "copy_from_user_str"),
}

_walk = base.walk


def walk():
    res = _walk()
    out = {}
    for comp, rows in res.items():
        if comp in NOT_USER_COMPONENTS:
            continue
        rows = [r for r in rows if r[0] not in BOUNDARY_FILES and (r[0], r[2]) not in BOUNDARY_FNS]
        if rows:
            out[comp] = rows
    return out
BUDGET = os.path.join(base.ROOT, "docs", "reference", "user-ptr-params-budget.json")
KEY = "untyped_user_ptr_params"


def main(argv):
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
            json.dump({KEY: dict(sorted((k, v) for k, v in now.items() if v))}, f, indent=2)
            f.write("\n")
        print(f"check_user_ptr_params: wrote {os.path.relpath(BUDGET, base.ROOT)} ({sum(now.values())} sites in {len(now)} components)")
        return 0
    budget = json.load(open(BUDGET)).get(KEY, {}) if os.path.exists(BUDGET) else {}
    violations, notes = [], []
    for name in sorted(now):
        n, allowed = now[name], budget.get(name, 0)
        if n > allowed:
            violations.append(f"{name}: {n} untyped user-pointer params, budget {allowed} (+{n - allowed})")
        elif n < allowed:
            notes.append(f"{name}: {n} untyped user-pointer params, budget {allowed} - run --update to ratchet it down")
    for n in notes:
        print("note:", n)
    if violations:
        for v in violations:
            print("ERROR:", v)
        print("A new user pointer takes `akuma_user_access::UserAddr`, not a bare u64; or run --update and explain why in review.")
        return 1
    print(f"check_user_ptr_params: ok ({sum(now.values())} untyped user-pointer params, within budget)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
