#!/usr/bin/env python3
"""Ratchet on feature drift between the two full kernels' manifests.

The AArch64 kernel (root `Cargo.toml`) and the amd64 kernel (`amd64/Cargo.toml`) each carry a
hand-written feature table, and 16 names appear in both (`smp-shared`, the seven `sc-*`,
...). Each must forward the same *crate* features, or a flag turned on for one kernel is
silently not turned on for the other (`docs/archive/AKUMA_FEATURE_AUDIT.md`, finding 1).

Not every difference is a bug: the root kernel forwards through `akuma-kernel-{core,glue}`, which
amd64 does not use, and amd64 forwards `no-bkl-*` to crates directly. So this does not demand
equality. It records today's differences in `docs/reference/feature-parity-baseline.json` and
fails only when a **new** one appears (or when a recorded one disappears and the file is stale),
exactly like `check_unsafe_budget.py`: the way through is `--update`, so the baseline's diff
carries the change into review.

For each common feature the compared set is its forward targets (`crate/feature`), minus targets
into crates only one manifest depends on - those cannot be forwarded by the other.

    scripts/check_feature_parity.py            # check (exit 1 on new drift)
    scripts/check_feature_parity.py --update   # rewrite the baseline from the tree
    scripts/check_feature_parity.py --list     # print every difference
"""
import json
import os
import sys
import tomllib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BASELINE = os.path.join(ROOT, "docs", "reference", "feature-parity-baseline.json")
A64, AMD64 = "Cargo.toml", os.path.join("amd64", "Cargo.toml")


def load(path):
    with open(os.path.join(ROOT, path), "rb") as f:
        d = tomllib.load(f)
    deps = set(d.get("dependencies", {}))
    return d.get("features", {}), deps


def targets(values, other_deps):
    """Forward targets that the *other* manifest could also forward to."""
    out = set()
    for v in values:
        if v.startswith("dep:"):
            continue
        crate = v.split("/", 1)[0]
        if "/" in v and crate in other_deps:
            out.add(v)
        elif "/" not in v:
            out.add(v)  # a same-manifest feature (e.g. `no-bkl-network`): compared by name
    return out


def measure():
    (fa, da), (fx, dx) = load(A64), load(AMD64)
    common = sorted((set(fa) & set(fx)) - {"default"})
    drift = {}
    for name in common:
        a = targets(fa[name], dx)
        x = targets(fx[name], da)
        only_a64, only_amd64 = sorted(a - x), sorted(x - a)
        if only_a64 or only_amd64:
            drift[name] = {"only_aarch64": only_a64, "only_amd64": only_amd64}
    return {"common": common, "drift": drift}


def main(argv):
    m = measure()
    if "--list" in argv:
        for k, v in m["drift"].items():
            print(f"{k}\n  only aarch64: {v['only_aarch64']}\n  only amd64:   {v['only_amd64']}")
        print(f"{len(m['common'])} common features, {len(m['drift'])} with drift")
        return 0
    if "--update" in argv:
        with open(BASELINE, "w") as f:
            json.dump(m, f, indent=2, sort_keys=True)
            f.write("\n")
        print(f"check_feature_parity: baseline written ({len(m['drift'])} drifting of {len(m['common'])})")
        return 0
    with open(BASELINE) as f:
        base = json.load(f)
    bad = []
    for name, v in m["drift"].items():
        b = base["drift"].get(name, {"only_aarch64": [], "only_amd64": []})
        for side in ("only_aarch64", "only_amd64"):
            new = sorted(set(v[side]) - set(b[side]))
            if new:
                bad.append(f"{name}: new {side} forward(s) {new}")
    gone = sorted(set(base["common"]) - set(m["common"]))
    if gone:
        bad.append(f"common feature(s) no longer in both manifests: {gone}")
    if bad:
        print("check_feature_parity: FAIL")
        for line in bad:
            print("  " + line)
        print("A new difference needs a reason in review: fix the other manifest, or run --update.")
        return 1
    fixed = sum(
        1
        for name, b in base["drift"].items()
        if len(m["drift"].get(name, {}).get("only_aarch64", [])) + len(m["drift"].get(name, {}).get("only_amd64", []))
        < len(b["only_aarch64"]) + len(b["only_amd64"])
    )
    print(f"check_feature_parity: ok ({len(m['drift'])} drifting of {len(m['common'])} common; {fixed} improved - run --update to ratchet)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
