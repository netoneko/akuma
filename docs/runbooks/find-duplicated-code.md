# Find duplicated code

Use this when you want to know where the copy-paste is before starting a
refactor, when reviewing a change that looks like it cloned an existing
function, or to gate CI against someone duplicating something substantial. The
tool is PMD's **CPD** (Copy/Paste Detector), which has a Rust tokenizer.

The current findings and the work list derived from them live in
[`../archive/TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md`](../archive/TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md).
Read that before re-running — most of what a fresh scan reports is already
triaged there.

## 1. Install

```bash
brew install pmd          # 7.26.0 or later
pmd --version
```

CPD ships with PMD; there is no separate package. Confirm the Rust tokenizer is
present:

```bash
pmd cpd --help | grep -A14 -- '-l, --language' | grep rust
```

## 2. Run

```bash
cd /path/to/akuma
pmd cpd --dir src --dir crates --language rust \
        --minimum-tokens 100 --format text
```

**Exit code 4 means "duplications were found" — that is success, not an error.**
Guard any scripting accordingly (`pmd cpd … || [ $? -eq 4 ]`).

Never point CPD at the repo root: it will walk `target/` and take minutes. Always
pass explicit `--dir` arguments.

### Choosing `--minimum-tokens`

| Value | Use for |
|---:|---|
| 150 | CI gate — only substantial clones |
| 100 | The default working threshold; whole cloned functions |
| 75 | Broader sweep before a refactor |
| 50 | Exhaustive; the tail is small clones where a helper costs more than it saves |

**Use 50 for the fault, CoW and page-table paths regardless.** A 13-line clone in
`src/exceptions.rs` is worth more attention than a 60-line one in a test file:
the 2026-08-12 CoW refcount underflow lived in three mutually-cloned break sites
that only appear at `--minimum-tokens 50`, and had to be fixed three times
(`../archive/TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md` §5.6). Small clones in
dangerous code outrank large clones in safe code.

## 3. Aggregate the output

The text report lists one block at a time and double-counts overlapping blocks,
so summing `Found a N line` naively overstates coverage and says nothing about
how much is *removable*. Union the line ranges instead:

```bash
pmd cpd --dir src --dir crates --language rust \
        --minimum-tokens 100 --format text > /tmp/cpd.txt

python3 - <<'PY'
import re, os, collections
blocks=[]
for m in re.finditer(r'^Found a (\d+) line \((\d+) tokens\) duplication in the following files: \n((?:Starting at line \d+ of .*\n)+)',
                     open('/tmp/cpd.txt').read(), re.M):
    n=int(m.group(1))
    sites=[(int(a), os.path.relpath(b)) for a,b in re.findall(r'Starting at line (\d+) of (.*)', m.group(3))]
    blocks.append((n, sites))

def union(iv):
    if not iv: return 0
    iv=sorted(iv); t=0; cs,ce=iv[0]
    for s,e in iv[1:]:
        if s<=ce: ce=max(ce,e)
        else: t+=ce-cs; cs,ce=s,e
    return t+ce-cs

cov=collections.defaultdict(list); keep=collections.defaultdict(list)
for n,sites in blocks:
    for i,(st,f) in enumerate(sites):
        cov[f].append((st,st+n))
        if i==0: keep[f].append((st,st+n))
covered=sum(union(v) for v in cov.values())
removable=covered-sum(union(v) for v in keep.values())
print(f"blocks={len(blocks)} covered={covered} removable={removable}")
for f,v in sorted(cov.items(), key=lambda kv:-union(kv[1]))[:15]:
    print(f"  {union(v):5d}  {f}")
PY
```

`covered` = lines participating in at least one clone. `removable` = what
disappears if every clone group collapses to one copy (before the 10–20% you add
back as parameters).

## 4. Known traps

- **`--ignore-identifiers` silently does nothing for Rust** (verified on PMD
  7.26.0 — output is byte-identical with and without it). Every number CPD gives
  you is Type-1 (exact tokens) only. Clones that survived a variable rename are
  invisible. Worked examples of what this misses are in
  [`../archive/TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md`](../archive/TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md) §6.
- **Treat the numbers as a floor, never an estimate.** The two virtio `Hal`
  impls are ~120 lines of functionally identical code; CPD reports 35, because a
  single substituted call expression breaks the token run.
- **Test files are ~31% of the tree** (`src/tests.rs`, `src/process_tests.rs`,
  and the smaller `*_tests.rs`). Split them out before quoting a figure — they
  turn out to be *less* duplicated than production code, so lumping them in
  understates the real problem.
- CPD has no notion of intent. Hardware-ABI layouts (virtqueue rings, the aio
  ring, `#[repr(C)]` structs mirroring Linux) will show up and must not be
  "deduplicated".

## 5. Type-2 clones: what to use instead

When you suspect a clone that CPD cannot see:

- **`ast-grep`** with an explicit pattern, once you know the shape you are
  hunting. Good for "find every copy of this probe loop".
- **The compiler.** Extract the candidate into a generic function or trait and
  see whether both call sites still build. If they do, it was a duplicate; if
  not, the error tells you exactly how they differ. This is what settled the
  `Hal` question.
- **Grep a magic constant.** Copy-paste renames variables but almost never
  renames literals — `0xe00` found four copies of `VIRTIO_MMIO_ADDRS`.
- **A differential harness**, when the "clone" is two implementations of the
  same contract and you intend to delete one. Reimplement or link both, run them
  over a real corpus, diff the outputs. This is what proved the two ELF parsers
  equivalent before deleting the hand-rolled one
  ([`../archive/TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md`](../archive/TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md) §3):
  2,387 real binaries under `bootstrap/` + `userspace/`, 0 disagreements, plus
  the 2,031 `.o` files as free negative cases and a header-field mutation pass
  for panic-safety. Cheap to write, and it converts "looks equivalent" into
  evidence.

## 6. CI gate (optional)

```bash
pmd cpd --dir src --dir crates --language rust \
        --minimum-tokens 150 --format text > cpd.txt || [ $? -eq 4 ]
BLOCKS=$(grep -c '^Found a ' cpd.txt)
[ "$BLOCKS" -le 38 ] || { echo "new large clone(s): $BLOCKS > 38"; cat cpd.txt; exit 1; }
```

38 is the 2026-08-12 baseline at 150 tokens. Ratchet it down as clones are
fixed; never up without a note saying why.

## Verify

A working run on an unmodified tree at `--minimum-tokens 100` prints blocks in
this form:

```
Found a 61 line (627 tokens) duplication in the following files:
Starting at line 778 of .../crates/akuma-exec/src/elf/mod.rs
Starting at line 1137 of .../crates/akuma-exec/src/elf/mod.rs
```

and the aggregation script reports, as of 2026-08-12:

```
blocks=92 covered=3485 removable=1718
```

If you get `blocks=0`, CPD did not tokenize anything — check that `--language
rust` was passed (without it CPD defaults to Java and silently matches no
files), and that the `--dir` paths exist.

If the run takes more than ~30 seconds, you are almost certainly walking
`target/`.

## Background

- [`../archive/TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md`](../archive/TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md)
  — the 2026-08-12 findings, the per-pattern work list, and what CPD missed.
- [`../archive/UNSAFE_AUDIT.md`](../archive/UNSAFE_AUDIT.md) — the `unsafe`
  census; its tier-A work list is the same virtio-driver consolidation, since
  deleted code carries no `unsafe`.
