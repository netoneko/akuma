#!/usr/bin/env python3
"""Attribute a kernel image's bytes to subsystems, by ELF symbol size.

Answers "what is subsystem X actually costing us in .text/.rodata?" — the
question `scripts/cloc_akuma.py` cannot answer, because line counts say nothing
about code size (a 30 KB precomputed table is one line of Rust).

The size/extreme-size profiles set `strip = "symbols"`, so build with symbols
retained first:

    scripts/build_size.sh --config 'profile.size.strip=false'
    scripts/symbol_sizes.py target/aarch64-unknown-none/extreme-size/akuma

Caveat that matters for every number this prints: the size profiles use
`lto = true, codegen-units = 1`. Inlined code is attributed to the symbol it was
inlined *into*, so a group's total is a floor, not a ceiling — and a small group
often means "inlined into its caller", not "cheap". Cross-check a surprising
result against the raw listing:

    llvm-nm --print-size --size-sort --demangle <image> | tail -50

llvm-nm ships in the nightly toolchain, not on PATH:
  ~/.rustup/toolchains/nightly-*/lib/rustlib/*/bin/llvm-nm
"""

from __future__ import annotations

import argparse
import collections
import glob
import os
import re
import subprocess
import sys

# Ordered: first match wins, so put the specific groups before the broad ones.
GROUPS = [
    ("ssh: in-kernel server", (r"akuma::ssh", r"\bakuma_ssh\b", r"akuma_ssh_crypto")),
    (
        "crypto (ssh + tls)",
        (
            r"curve25519_dalek", r"ed25519_dalek", r"\bsha2\b", r"\baes\b", r"chacha20",
            r"poly1305", r"ghash", r"\bctr\b", r"\bcipher\b", r"signature::", r"subtle",
            r"\bhmac\b", r"x25519", r"\bdigest\b", r"\brsa\b", r"crypto_bigint",
        ),
    ),
    (
        "tls / x509",
        (r"embedded_tls", r"tls_verifier", r"x509", r"\bder\b", r"\bspki\b", r"pkcs", r"asn1"),
    ),
    ("smoltcp", (r"smoltcp",)),
    ("akuma-net", (r"akuma_net",)),
    ("shell", (r"akuma::shell", r"akuma_shell")),
    ("editor", (r"akuma_editor", r"akuma::editor")),
    ("ext2 / vfs", (r"akuma_ext2", r"akuma_vfs", r"akuma::vfs")),
    ("exec (proc/thread/mmu)", (r"akuma_exec",)),
    ("rump proxy", (r"akuma_rump", r"akuma::rump")),
    ("smp", (r"akuma_smp", r"akuma::smp")),
]


def find_llvm_nm() -> str:
    pats = [
        os.path.expanduser("~/.rustup/toolchains/nightly-*/lib/rustlib/*/bin/llvm-nm"),
        os.path.expanduser("~/.rustup/toolchains/*/lib/rustlib/*/bin/llvm-nm"),
    ]
    for pat in pats:
        hits = sorted(glob.glob(pat))
        if hits:
            return hits[-1]
    raise SystemExit("error: llvm-nm not found in any rustup toolchain")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("image", help="path to an unstripped kernel ELF")
    ap.add_argument("--top", type=int, default=0, help="also list the N largest symbols")
    ap.add_argument("--nm", default=None, help="path to llvm-nm")
    args = ap.parse_args()

    nm = args.nm or find_llvm_nm()
    out = subprocess.run(
        [nm, "--print-size", "--size-sort", "--demangle", args.image],
        capture_output=True,
        text=True,
    )
    if "no symbols" in out.stdout or not out.stdout.strip():
        raise SystemExit(
            f"error: {args.image} has no symbol table — rebuild with "
            "--config 'profile.<name>.strip=false'"
        )

    totals: collections.Counter = collections.Counter()
    counts: collections.Counter = collections.Counter()
    biggest: list = []
    unmatched = grand = 0

    for ln in out.stdout.splitlines():
        parts = ln.split(None, 3)
        if len(parts) < 4:
            continue
        try:
            size = int(parts[1], 16)
        except ValueError:
            continue
        if parts[2].lower() not in ("t", "r", "d", "b"):  # text/rodata/data/bss
            continue
        name = parts[3]
        grand += size
        biggest.append((size, name))
        for label, pats in GROUPS:
            if any(re.search(p, name) for p in pats):
                totals[label] += size
                counts[label] += 1
                break
        else:
            unmatched += size

    print(f"{args.image}  (sized symbols: {grand:,} bytes)")
    print(f"{'group':<26}{'bytes':>11}{'KB':>8}{'share':>8}{'syms':>7}")
    print("-" * 60)
    for label, _ in GROUPS:
        v = totals[label]
        if not v:
            continue
        print(f"{label:<26}{v:>11,}{v / 1024:>8.1f}{100 * v / grand:>7.1f}%{counts[label]:>7}")
    print("-" * 60)
    print(f"{'unattributed / core':<26}{unmatched:>11,}{unmatched / 1024:>8.1f}"
          f"{100 * unmatched / grand:>7.1f}%")

    if args.top:
        print(f"\nTop {args.top} symbols:")
        for size, name in sorted(biggest, reverse=True)[: args.top]:
            print(f"  {size:>8,}  {name[:100]}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
