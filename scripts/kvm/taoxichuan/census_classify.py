#!/usr/bin/env python3 -I
"""census_classify.py OUT UNISTD_64_H - classify B10's census by what serves each call on Taoxichuan.

OUT is a copy of `census_pop.sh`'s out/ directory; UNISTD_64_H is Linux's
`asm/unistd_64.h` (names to x86_64 numbers). A run with a `.full.gz` trace counts
only what its own program called: calls made after a spawned child's `execve`
belong to that child (Pop's glibc `uname`, `bash`), while the window between a
`clone` and the `execve` is Rust std's spawn path (`dup2`, `chdir`) and counts.

Prints each call's server - a group of `akuma-runtime-tokio`'s allow-list, the
fd family of `akuma-syscalls-glue-amd64`, the legacy fold, Taoxichuan's own arms -
or UNSERVED, then the allow-listed calls no census program made. Run from the
repository root: the groups are read from the sources, not restated here.
"""
import collections
import gzip
import os
import re
import sys

ROOT = os.getcwd()


def src(path):
    with open(os.path.join(ROOT, path)) as f:
        return f.read()


def abi_rows():
    text = src("crates/akuma-syscalls-abi/src/lib.rs")
    return {m.group(1): int(m.group(2)) for m in re.finditer(r"^\s+(\w+)\s+=>\s+\w+\s*=\s*(\d+),\s*nr::", text, re.M)}


def fn_body(text, start):
    body = text[text.index(start):]
    return body[:body.index("\n}\n")]


def tokio_groups(row):
    text = src("crates/akuma-runtime-tokio/src/lib.rs")
    groups = {}
    for name in ("runtime", "net", "process"):
        names = re.findall(r"S(?:yscall)?::(\w+)", fn_body(text, f"pub const fn {name}(call"))
        groups[name] = [n for n in names if n in row]
    return groups


def glue_amd64(row):
    body = fn_body(src("crates/akuma-syscalls-glue-amd64/src/lib.rs"), "pub fn dispatch")
    legacy = {int(n) for n in re.findall(r"^\s+(\d+) => Some", body, re.M)}
    return {row[n] for n in re.findall(r"Syscall::(\w+)", body) if n in row} | legacy


def legacy_fold():
    m = re.search(r"pub const NUMBERS: \[u64; \d+\] = \[([^\]]*)\]", src("crates/akuma-syscalls-abi/src/lib.rs"))
    return {int(n) for n in m.group(1).split(",")}


def local_arms(row):
    text = src("taoxichuan/src/main.rs")
    body = fn_body(text, "fn dispatch_inner(")
    consts = set(re.findall(r"akuma_syscalls_abi::x86_64::([A-Z0-9_]+)", body))
    by_const = {re.sub(r"(?<!^)(?=[A-Z])", "_", n).upper(): v for n, v in row.items()}
    nums = {by_const[c] for c in consts if c in by_const}
    ring3 = src("crates/akuma-ring3-amd64/src/lib.rs")
    nr_mod = fn_body(ring3, "pub mod nr {")
    for c in set(re.findall(r"ring3::nr::([A-Z0-9_]+)", body)):
        m = re.search(rf"pub const {c}: u64 = (\d+);", nr_mod)
        if m:
            nums.add(int(m.group(1)))
    return nums


def own_calls(path):
    """Syscall names a `strace -f` trace's first program made, children after their execve excluded."""
    img, first, calls = {}, None, collections.defaultdict(set)
    with gzip.open(path, "rt", errors="replace") as f:
        for line in f:
            m = re.match(r"(\d+)\s+(<\.\.\. )?(\w+)", line)
            if not m:
                continue
            pid, resumed, name = int(m.group(1)), bool(m.group(2)), m.group(3)
            if first is None:
                first = pid
                img[pid] = "main"
            cur = img.get(pid, "main")
            if name in ("clone", "clone3", "fork", "vfork"):
                r = re.search(r"= (\d+)\s*$", line)
                if r:
                    img[int(r.group(1))] = cur
            if name == "execve" and pid != first and re.search(r"= 0\s*$", line):
                img[pid] = "child"
            if not resumed:
                calls[img.get(pid, cur)].add(name)
    return calls["main"]


def is_summary(path):
    with open(path, errors="replace") as f:
        return any(line.startswith("% time") for line, _ in zip(f, range(5)))


def summary_calls(path):
    names = set()
    with open(path) as f:
        for line in f:
            fields = line.split()
            if len(fields) >= 5 and fields[0][0].isdigit() and fields[-1] != "total":
                names.add(fields[-1])
    return names


def main(argv):
    if len(argv) != 2:
        print(__doc__)
        return 2
    out, header = argv
    numbers = {}
    with open(header) as f:
        for line in f:
            m = re.match(r"#define __NR_(\w+)\s+(\d+)", line)
            if m:
                numbers[m.group(1)] = int(m.group(2))
    runs = {}
    for name in sorted(os.listdir(out)):
        if name.endswith(".txt") and is_summary(os.path.join(out, name)):
            run = name[:-4]
            full = os.path.join(out, run + ".full.gz")
            runs[run] = own_calls(full) if os.path.exists(full) else summary_calls(os.path.join(out, name))
    row = abi_rows()
    groups = tokio_groups(row)
    servers = [(f"tokio:{g}", {row[n] for n in names}) for g, names in groups.items()]
    servers[-1][1].update({434, 424})  # `process`: the shared-number calls
    servers += [("glue-amd64 (fd family)", glue_amd64(row)), ("legacy fold -> glue-amd64", legacy_fold()),
                ("taoxichuan local", local_arms(row))]
    union = set().union(*runs.values())
    table = collections.defaultdict(list)
    for name in sorted(union, key=lambda n: numbers.get(n, 9999)):
        nr = numbers.get(name, -1)
        server = next((label for label, nums in servers if nr in nums), "UNSERVED")
        table[server].append((nr, name, [r for r in runs if name in runs[r]]))
    print(f"census: {len(runs)} runs, {len(union)} distinct calls")
    for server in sorted(table):
        print(f"\n## {server} ({len(table[server])})")
        for nr, name, users in table[server]:
            print(f"  {nr:>4} {name:20} {','.join(users)}")
    used = {numbers[n] for n in union if n in numbers}
    print("\n## allow-listed, made by no census program")
    for g, names in groups.items():
        print(f"  {g}: {', '.join(n for n in names if row[n] not in used) or '-'}")
    return 1 if "UNSERVED" in table else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
