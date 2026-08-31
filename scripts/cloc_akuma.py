#!/usr/bin/env python3
"""cloc-style line counter that knows the difference between kernel code and tests.

Unlike `cloc`, this walks Rust source with a real lexer (string literals, raw
strings, char-vs-lifetime, nested block comments) and then attributes every line
to one of two buckets:

  production  — code that ships in the kernel/crates
  test        — code that only exists to test it

It also reports two different `unsafe` safety numbers per crate, because they
answer different questions and only quoting one of them misleads:

  enforced    — the crate carries `#![forbid(unsafe_code)]`, so the compiler
                refuses to let anyone write `unsafe` in it at all. A guarantee.
  safe %      — the share of production CODE lines that do not sit inside an
                `unsafe { .. }` block. A measurement.

A crate with one 3-line block in 3,000 lines scores 0% on the first and 99.9%
on the second. Both are true; the gap between them is the interesting part.
Run `--self-test` to check the unsafe-line counter against the cases a plain
`grep -c unsafe` gets wrong in both directions.

A line is test code when any of these hold:

  1. Its file is a test file: ``tests.rs`` / ``*_tests.rs`` / ``*_test.rs`` /
     ``test_*.rs`` / ``test_support.rs``, or it lives under a ``tests/`` or
     ``benches/`` directory.
  2. It is inside an item annotated ``#[test]`` / ``#[bench]``.
  3. It is inside an item whose ``#[cfg(...)]`` is *only* compiled for tests.
     The cfg predicate is parsed and evaluated with three-valued logic against
     two worlds — "tests on" and "tests off" — and the item counts as test code
     only if it cannot exist in the second one. That gets the interesting cases
     right:

       #[cfg(test)]                                        -> test
       #[cfg(all(test, feature = "kernel-tls"))]           -> test
       #[cfg(not(any(feature = "no-tests", ...)))]         -> test  (see below)
       #[cfg(any(ext2_fs_cache, test))]                    -> production
       #[cfg(not(test))]                                   -> production

Akuma's in-kernel boot suite is not gated on rustc's ``test`` cfg (it runs on
bare metal, not under ``cargo test``); it is gated on the absence of the
``no-tests`` feature. That gate is therefore treated as a test gate too. Pass
``--no-kernel-test-gate`` to count those lines as production instead.

``--rev`` counts a git revision's tree instead of the working tree, and ``--vs``
prints a production/test delta between two of them. That pair exists because the
obvious way to ask "did the refactor cut code?" — eyeball two numbers, or count
`.rs` lines with grep — gets the answer *backwards* on this repo. Extracting a
subsystem into a host-testable crate moves its `#[cfg(test)] mod tests` from a
`*_tests.rs` file into `crates/<name>/src/lib.rs`, so any counter that splits
prod from test **by filename** re-labels hundreds of test lines as production.
The `akuma-pmm` extraction (`eb19f23`) reads as **+501 production lines** that
way and **+221** once the inline test module is attributed correctly — the same
commit, off by more than 2x. This script's scanner already gets that right; the
only thing missing was being able to point it at two commits.

The published numbers this feeds live in ``docs/archive/LINE_COUNT_ANALYSIS.md``,
which is a *living* document — re-measure and rewrite it in place rather than
appending a new snapshot.

Usage:
    scripts/cloc_akuma.py                      # defaults to src/ crates/
    scripts/cloc_akuma.py src crates --by-file
    scripts/cloc_akuma.py --json
    scripts/cloc_akuma.py --rev HEAD~5         # count a revision's tree
    scripts/cloc_akuma.py --vs main            # delta: main -> working tree
    scripts/cloc_akuma.py --rev HEAD --vs HEAD~1   # delta across one commit
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from dataclasses import dataclass, field

# ---------------------------------------------------------------------------
# language table
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class LangSpec:
    name: str
    line_comments: tuple = ("//",)
    blocks: tuple = (("/*", "*/"),)
    nested_blocks: bool = False
    strings: tuple = ('"',)
    rust: bool = False


RUST = LangSpec("Rust", rust=True, nested_blocks=True)
C_LIKE = LangSpec("C")
HEADER = LangSpec("C/C++ Header")
ASM = LangSpec("Assembly", line_comments=("//", ";", "#"), blocks=(("/*", "*/"),))
HASH = lambda name: LangSpec(name, line_comments=("#",), blocks=())  # noqa: E731
PLAIN = lambda name: LangSpec(name, line_comments=(), blocks=())  # noqa: E731

EXT_LANGS = {
    ".rs": RUST,
    ".c": C_LIKE,
    ".h": HEADER,
    ".s": ASM,
    ".S": ASM,
    ".ld": LangSpec("Linker Script", line_comments=(), blocks=(("/*", "*/"),)),
    ".toml": HASH("TOML"),
    ".py": HASH("Python"),
    ".sh": HASH("Shell"),
    ".md": PLAIN("Markdown"),
    ".json": PLAIN("JSON"),
}

SKIP_DIRS = {".git", "target", "__pycache__", "node_modules", ".idea", ".vscode"}
TEST_DIRS = {"tests", "benches"}
TEST_BASENAMES = {"tests.rs", "test.rs", "test_support.rs"}


def is_test_path(relpath: str) -> bool:
    parts = relpath.split(os.sep)
    if any(p in TEST_DIRS for p in parts[:-1]):
        return True
    base = parts[-1]
    if base in TEST_BASENAMES:
        return True
    stem, ext = os.path.splitext(base)
    if ext != ".rs":
        return False
    return stem.endswith(("_tests", "_test")) or stem.startswith("test_")


# ---------------------------------------------------------------------------
# cfg(...) predicate parsing + three-valued evaluation
# ---------------------------------------------------------------------------


def _tokenize_cfg(s: str) -> list:
    toks: list = []
    i, n = 0, len(s)
    while i < n:
        c = s[i]
        if c.isspace():
            i += 1
        elif c in "(),=":
            toks.append(c)
            i += 1
        elif c == '"':
            j = i + 1
            while j < n and s[j] != '"':
                j += 2 if s[j] == "\\" else 1
            toks.append(("str", s[i + 1 : j]))
            i = j + 1
        else:
            j = i
            while j < n and (s[j].isalnum() or s[j] in "_-"):
                j += 1
            if j == i:  # stray punctuation
                i += 1
                continue
            toks.append(("id", s[i:j]))
            i = j
    return toks


def _parse_cfg(toks: list, pos: int):
    """Return (node, next_pos). Nodes: ('all'|'any'|'not', [children]) | ('atom', key, value)."""
    if pos >= len(toks) or not isinstance(toks[pos], tuple):
        return ("atom", "?", None), pos + 1
    name = toks[pos][1]
    pos += 1
    if pos < len(toks) and toks[pos] == "(":
        pos += 1
        children = []
        while pos < len(toks) and toks[pos] != ")":
            if toks[pos] == ",":
                pos += 1
                continue
            child, pos = _parse_cfg(toks, pos)
            children.append(child)
        return (name.lower(), children), pos + 1
    if pos < len(toks) and toks[pos] == "=":
        pos += 1
        value = toks[pos][1] if pos < len(toks) and isinstance(toks[pos], tuple) else ""
        return ("atom", name, value), pos + 1
    return ("atom", name, None), pos


def _eval_cfg(node, env: dict):
    """Three-valued eval: True / False / None (unknown)."""
    kind = node[0]
    if kind == "atom":
        return env.get((node[1], node[2]))
    kids = [_eval_cfg(k, env) for k in node[1]]
    if kind == "all":
        if any(v is False for v in kids):
            return False
        return None if any(v is None for v in kids) else True
    if kind == "any":
        if any(v is True for v in kids):
            return True
        return None if any(v is None for v in kids) else False
    if kind == "not":
        if not kids:
            return None
        return None if kids[0] is None else (not kids[0])
    return None  # unrecognised cfg function


def cfg_is_test_only(inner: str, kernel_gate: bool) -> bool:
    """True if this cfg predicate can only hold in a tests-enabled build."""
    toks = _tokenize_cfg(inner)
    if not toks:
        return False
    node, _ = _parse_cfg(toks, 0)
    tests_on = {("test", None): True}
    tests_off = {("test", None): False}
    if kernel_gate:
        tests_on[("feature", "no-tests")] = False
        tests_off[("feature", "no-tests")] = True
    return _eval_cfg(node, tests_on) is not False and _eval_cfg(node, tests_off) is False


def attr_is_test_gate(inner: str, kernel_gate: bool) -> bool:
    """`inner` is the text between `#[` and `]`."""
    inner = inner.strip()
    bare = inner.rsplit("::", 1)[-1]
    if bare in ("test", "bench"):
        return True
    if inner.startswith("cfg(") and inner.endswith(")"):
        return cfg_is_test_only(inner[4:-1], kernel_gate)
    return False


# ---------------------------------------------------------------------------
# the scanner
# ---------------------------------------------------------------------------


@dataclass
class FileCount:
    path: str
    lang: str
    blank: int = 0
    comment: int = 0
    code: int = 0
    test_blank: int = 0
    test_comment: int = 0
    test_code: int = 0
    whole_file_test: bool = False
    #: `unsafe` keyword sites lexed in CODE context — not in comments, string
    #: literals or `asm!` bodies, which a grep cannot tell apart. `unsafe_code`
    #: (as in `#![forbid(unsafe_code)]`) lexes as one identifier and is not
    #: counted.
    #:
    #: `#[unsafe(no_mangle)]` (edition 2024) is NOT counted either. This said it
    #: *was* until 2026-08-30, which was wrong — the lexer consumes `#[..]`
    #: wholesale, so the keyword inside never reaches the identifier branch. The
    #: behaviour is right and the claim was stale: the attribute marks a
    #: declaration, not an operation the compiler stopped checking, and the tree
    #: has a dozen of them. Pinned by `--self-test`.
    unsafe_sites: int = 0
    #: `unsafe` sites inside test code — a `#[test]` body, a test-gated item, or a
    #: whole test file. Split out because a kernel test that pokes a page table is
    #: not the same liability as one on a live syscall path.
    test_unsafe_sites: int = 0
    #: This file carries a crate-level `#![forbid(unsafe_code)]`.
    forbids_unsafe: bool = False
    #: Production CODE lines lying inside an `unsafe { .. }` block, plus the
    #: declaration line of an `unsafe fn`/`impl`/`trait`. This is the "how much
    #: of what ships is the compiler NOT checking" number; `unsafe_sites` counts
    #: how many places it happens, which is a different question — one 40-line
    #: block is 1 site and 40 lines.
    unsafe_code: int = 0
    #: The same, for lines the test buckets claimed.
    test_unsafe_code: int = 0

    @property
    def lines(self) -> int:
        return (
            self.blank
            + self.comment
            + self.code
            + self.test_blank
            + self.test_comment
            + self.test_code
        )


def _is_ident_char(c: str) -> bool:
    return c.isalnum() or c == "_"


ASM_MACROS = {"asm", "global_asm", "naked_asm"}


def classify_asm_lines(body: str, first_line: int, code: set, comment: set) -> None:
    """Classify the interior of an `asm!`/`global_asm!` raw string as assembly.

    `body` is the string contents; `first_line` is the line its opening quote sits
    on. The opening and closing lines are the caller's problem (they hold Rust
    tokens); everything between gets AArch64 comment rules so that the vector
    table's `//` annotations count as comments rather than as string payload.
    """
    segs = body.split("\n")
    in_block = False
    for offset, seg in enumerate(segs[1:-1], start=1):
        ln = first_line + offset
        rest = seg.strip()
        if in_block:
            comment.add(ln)
            if "*/" in rest:
                in_block = False
                tail = rest.split("*/", 1)[1].strip()
                if tail and not tail.startswith("//"):
                    code.add(ln)
            continue
        if not rest:
            continue  # blank
        if rest.startswith("//"):
            comment.add(ln)
            continue
        if rest.startswith("/*"):
            comment.add(ln)
            in_block = "*/" not in rest
            continue
        code.add(ln)
        if "/*" in rest and "*/" not in rest.split("/*", 1)[1]:
            in_block = True


def scan(text: str, spec: LangSpec, kernel_gate: bool = True):
    """Classify every line of `text`. Returns (code, comment, test) line-number sets."""
    code: set = set()
    comment: set = set()
    test: set = set()
    all_file_test = False

    i, n = 0, len(text)
    line = 1
    depth = 0
    # Open test spans: dicts of start line / brace depth / whether `{` was seen.
    spans: list = []
    pending_test = False  # a test attribute is waiting for its item
    pending_line = 0  # line the gating attribute started on
    awaiting_item = False  # currently inside a test-gated item's header
    last_ident = ""  # most recent identifier token
    unsafe_lines: list = []  # line of each `unsafe` token lexed in code context
    # Lines lying inside an `unsafe { .. }` block, plus the declaration line of an
    # `unsafe fn`/`impl`/`trait`. A set, so nested blocks overlap harmlessly.
    unsafe_span: set = set()
    # An `unsafe` token is waiting to find out what it introduces. A following `{`
    # makes it a block (span it); any other identifier makes it a declaration
    # (`unsafe fn`, `unsafe impl`, `unsafe extern`) and any `(` makes it the
    # edition-2024 `#[unsafe(..)]` attribute — neither of which opens an unsafe
    # *context*, so those count one line and nothing more.
    unsafe_pending = False
    unsafe_spans: list = []  # open blocks: dicts of start line / brace depth

    def close_span(sp):
        for ln in range(sp["start"], line + 1):
            test.add(ln)

    def close_unsafe(sp):
        for ln in range(sp["start"], line + 1):
            unsafe_span.add(ln)

    def start_item():
        # The gating attribute belongs to the test, so the span starts there.
        nonlocal pending_test, pending_line, awaiting_item
        spans.append({"start": pending_line or line, "depth": depth, "brace": False})
        pending_test = False
        pending_line = 0
        awaiting_item = True

    while i < n:
        c = text[i]

        if c == "\n":
            line += 1
            i += 1
            continue

        if c.isspace():
            i += 1
            continue

        # -- line comment ---------------------------------------------------
        lc = next((p for p in spec.line_comments if text.startswith(p, i)), None)
        if lc is not None:
            comment.add(line)
            while i < n and text[i] != "\n":
                i += 1
            continue

        # -- block comment --------------------------------------------------
        blk = next((b for b in spec.blocks if text.startswith(b[0], i)), None)
        if blk is not None:
            opener, closer = blk
            comment.add(line)
            i += len(opener)
            bdepth = 1
            while i < n and bdepth:
                if text[i] == "\n":
                    line += 1
                    comment.add(line)
                    i += 1
                    continue
                if spec.nested_blocks and text.startswith(opener, i):
                    bdepth += 1
                    i += len(opener)
                    continue
                if text.startswith(closer, i):
                    bdepth -= 1
                    i += len(closer)
                    continue
                i += 1
            continue

        # Everything below is code.
        # -- attributes (Rust) ----------------------------------------------
        if spec.rust and c == "#" and text.startswith(("#[", "#!["), i):
            inner_attr = text.startswith("#![", i)
            attr_line = line
            start = i + (3 if inner_attr else 2)
            j, bdepth = start, 1
            while j < n and bdepth:
                ch = text[j]
                if ch == "\n":
                    code.add(line)
                    line += 1
                elif ch == '"':
                    j += 1
                    while j < n and text[j] != '"':
                        if text[j] == "\\":
                            j += 1
                            if j < n and text[j] == "\n":
                                code.add(line)
                                line += 1
                        elif text[j] == "\n":
                            code.add(line)
                            line += 1
                        j += 1
                elif ch == "[":
                    bdepth += 1
                elif ch == "]":
                    bdepth -= 1
                    if bdepth == 0:
                        break
                j += 1
            code.add(line)
            if attr_is_test_gate(text[start:j], kernel_gate):
                if inner_attr:
                    all_file_test = True
                else:
                    if not pending_test:
                        pending_line = attr_line
                    pending_test = True
            i = j + 1
            continue

        code.add(line)

        if pending_test:
            start_item()

        # -- string literals -------------------------------------------------
        if spec.rust and (c in "rb") and not (i and _is_ident_char(text[i - 1])):
            j = i
            if text[j] == "b" and j + 1 < n and text[j + 1] == "r":
                j += 1
            if text[j] == "r":
                k = j + 1
                hashes = 0
                while k < n and text[k] == "#":
                    hashes += 1
                    k += 1
                if k < n and text[k] == '"':
                    terminator = '"' + "#" * hashes
                    open_line = line
                    body_start = k + 1
                    k = body_start
                    while k < n and not text.startswith(terminator, k):
                        if text[k] == "\n":
                            line += 1
                            code.add(line)
                        k += 1
                    if last_ident in ASM_MACROS and "\n" in text[body_start:k]:
                        # Reclassify the interior with assembly comment rules.
                        for ln in range(open_line + 1, line):
                            code.discard(ln)
                        classify_asm_lines(text[body_start:k], open_line, code, comment)
                        code.add(line)  # the line holding the closing `"#`
                    i = k + len(terminator)
                    continue

        if c in spec.strings:
            i += 1
            while i < n and text[i] != c:
                if text[i] == "\\":
                    i += 1  # skip the escaped char, but not its line break
                    if i < n and text[i] == "\n":
                        line += 1
                        code.add(line)
                elif text[i] == "\n":
                    line += 1
                    code.add(line)
                i += 1
            i += 1
            continue

        if spec.rust and c == "'":
            # char literal, or a lifetime we should just walk past
            if i + 1 < n and text[i + 1] == "\\":
                j = i + 2
                while j < n and text[j] != "'":
                    j += 1
                i = j + 1
                continue
            if i + 2 < n and text[i + 2] == "'":
                i += 3
                continue
            i += 1
            continue

        # -- identifiers (remembered so `asm!` raw strings can be recognised) --
        if _is_ident_char(c) and not c.isdigit():
            j = i
            while j < n and _is_ident_char(text[j]):
                j += 1
            last_ident = text[i:j]
            if last_ident != "unsafe" and unsafe_pending:
                # `unsafe fn` / `unsafe impl` / `unsafe trait` / `unsafe extern`.
                # In edition 2024 an `unsafe fn` body is NOT itself an unsafe
                # context (`unsafe_op_in_unsafe_fn`), and this tree writes the
                # inner `unsafe { .. }` explicitly — so spanning the body here
                # would double-count it against those inner blocks.
                unsafe_pending = False
            if last_ident == "unsafe":
                # Reached only from the identifier branch, which strings, comments,
                # char literals and `asm!` bodies never fall through to — so this
                # counts real `unsafe` sites, unlike a grep over the file. The line
                # is kept, not just a tally, so `count_text` can split the sites into
                # production and test using the same rule it buckets lines with.
                unsafe_lines.append(line)
                unsafe_span.add(line)
                unsafe_pending = True
            i = j
            continue

        # -- braces / item termination ---------------------------------------
        if c == "{":
            if spans and awaiting_item and not spans[-1]["brace"] and spans[-1]["depth"] == depth:
                spans[-1]["brace"] = True
                awaiting_item = False
            if unsafe_pending:
                unsafe_spans.append({"start": line, "depth": depth})
                unsafe_pending = False
            depth += 1
            i += 1
            continue

        if c == "}":
            depth = max(0, depth - 1)
            while spans and spans[-1]["brace"] and spans[-1]["depth"] == depth:
                close_span(spans.pop())
            while unsafe_spans and unsafe_spans[-1]["depth"] == depth:
                close_unsafe(unsafe_spans.pop())
            awaiting_item = any(not sp["brace"] for sp in spans)
            i += 1
            continue

        if c == "(":
            # `#[unsafe(no_mangle)]` — an attribute, not a context.
            unsafe_pending = False
            i += 1
            continue

        if c == ";":
            unsafe_pending = False
            if spans and awaiting_item and not spans[-1]["brace"] and spans[-1]["depth"] == depth:
                close_span(spans.pop())
                awaiting_item = any(not sp["brace"] for sp in spans)
            i += 1
            continue

        i += 1

    for sp in spans:  # unterminated (truncated file) — count what we saw
        for ln in range(sp["start"], line + 1):
            test.add(ln)
    for sp in unsafe_spans:
        for ln in range(sp["start"], line + 1):
            unsafe_span.add(ln)

    return code, comment, test, all_file_test, unsafe_lines, unsafe_span


#: A crate-level `#![forbid(unsafe_code)]`, at the start of a line so a mention
#: inside a doc comment ("carries `#![forbid(unsafe_code)]`") does not match.
FORBID_RE = re.compile(r"^\s*#!\[forbid\([^)]*\bunsafe_code\b", re.M)


def count_file(path: str, relpath: str, spec: LangSpec, kernel_gate: bool) -> FileCount:
    with open(path, "r", encoding="utf-8", errors="replace") as fh:
        text = fh.read()
    return count_text(text, relpath, spec, kernel_gate)


def count_text(text: str, relpath: str, spec: LangSpec, kernel_gate: bool) -> FileCount:
    nlines = text.count("\n") + (1 if text and not text.endswith("\n") else 0)
    code, comment, test, inner_test, unsafe_lines, unsafe_span = scan(text, spec, kernel_gate)

    fc = FileCount(path=relpath, lang=spec.name)
    fc.whole_file_test = is_test_path(relpath) or inner_test
    fc.forbids_unsafe = FORBID_RE.search(text) is not None
    for ln in unsafe_lines:
        if fc.whole_file_test or ln in test:
            fc.test_unsafe_sites += 1
        else:
            fc.unsafe_sites += 1

    for ln in range(1, nlines + 1):
        is_test = fc.whole_file_test or ln in test
        if ln in code:
            # Only CODE lines can be unsafe: a comment or blank inside an
            # `unsafe` block is not code the compiler stopped checking, and
            # counting it would make a well-commented block look worse than a
            # bare one.
            if ln in unsafe_span:
                if is_test:
                    fc.test_unsafe_code += 1
                else:
                    fc.unsafe_code += 1
            bucket = "test_code" if is_test else "code"
        elif ln in comment:
            bucket = "test_comment" if is_test else "comment"
        else:
            bucket = "test_blank" if is_test else "blank"
        setattr(fc, bucket, getattr(fc, bucket) + 1)
    return fc


# ---------------------------------------------------------------------------
# walking + reporting
# ---------------------------------------------------------------------------


def walk(paths: list, kernel_gate: bool):
    """Count every source file under `paths`, each file exactly once.

    **The dedup is load-bearing, not tidiness.** Roots that overlap
    (`src crates src/syscall`) or simply repeat (`src src crates`) used to count
    the shared files twice, and every column doubled in lockstep — `src/syscall`
    read 46 files / 22,886 code / 9,698 comment against a true 23 / 11,443 /
    4,849. A clean 2x is exactly what a plausible-looking number looks like, and
    this script is what `CLAUDE.md` tells people to run to regenerate the figures
    in `docs/reference/crate-safety.md`, so a silent doubling lands in the docs
    as authoritative. Found 2026-08-31 by a reader who thought the *script* was
    miscounting; it was, but only when handed overlapping arguments.

    `walk_rev` has never had this bug and needs no equivalent: it filters one
    `git ls-tree` listing, which names each path once by construction.
    """
    files: list = []
    ignored = 0
    seen: set = set()
    duplicates = 0

    def take(full: str, spec) -> None:
        """Count `full` unless some earlier root already claimed it."""
        nonlocal duplicates
        key = os.path.realpath(full)
        if key in seen:
            duplicates += 1
            return
        seen.add(key)
        files.append(count_file(full, full, spec, kernel_gate))

    for root_arg in paths:
        root_arg = root_arg.rstrip(os.sep)
        if os.path.isfile(root_arg):
            spec = EXT_LANGS.get(os.path.splitext(root_arg)[1])
            if spec:
                take(root_arg, spec)
            else:
                ignored += 1
            continue
        for dirpath, dirnames, filenames in os.walk(root_arg):
            dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS and not d.startswith(".")]
            for fn in sorted(filenames):
                if fn.startswith("."):
                    continue
                full = os.path.join(dirpath, fn)
                spec = EXT_LANGS.get(os.path.splitext(fn)[1])
                if spec is None:
                    ignored += 1
                    continue
                take(full, spec)

    if duplicates:
        print(f"note: {duplicates} file(s) matched more than one of {paths} and "
              f"were counted once. Overlapping roots are fine; the totals below "
              f"are deduplicated.", file=sys.stderr)
    return files, ignored


def walk_rev(paths: list, kernel_gate: bool, rev: str):
    """Same as `walk`, but reads a git revision's tree instead of the checkout.

    Uses `git ls-tree`/`git show` rather than a temporary checkout so it never
    touches the working tree — the whole point is to compare "before the refactor"
    against uncommitted work in progress.
    """
    import subprocess

    listing = subprocess.run(
        ["git", "ls-tree", "-r", "--name-only", rev],
        capture_output=True, text=True,
    )
    if listing.returncode != 0:
        raise SystemExit(f"error: cannot read revision {rev!r}: "
                         f"{listing.stderr.strip()}")

    prefixes = tuple(p.rstrip(os.sep) for p in paths)
    files: list = []
    ignored = 0
    for rel in listing.stdout.splitlines():
        if not rel.startswith(prefixes) and rel not in prefixes:
            continue
        parts = rel.split(os.sep)
        if any(p in SKIP_DIRS or p.startswith(".") for p in parts[:-1]):
            continue
        spec = EXT_LANGS.get(os.path.splitext(rel)[1])
        if spec is None:
            ignored += 1
            continue
        blob = subprocess.run(["git", "show", f"{rev}:{rel}"],
                              capture_output=True, text=True, errors="replace")
        if blob.returncode != 0:
            continue
        files.append(count_text(blob.stdout, rel, spec, kernel_gate))
    return files, ignored


def print_delta(old_files: list, new_files: list, old_label: str, new_label: str) -> None:
    """Production/test deltas, whole-tree and per component.

    Production code is the headline because it is the number a refactor claims to
    move; test code and comments are shown beside it because on this repo they are
    usually where the growth actually went, and reporting prod alone invites the
    "we cut nothing" misreading.
    """
    def agg_by_comp(files):
        out: dict = {}
        for fc in files:
            out.setdefault(component_of(fc.path), Agg()).add(fc)
        return out

    old_total, new_total = Agg(), Agg()
    for fc in old_files:
        old_total.add(fc)
    for fc in new_files:
        new_total.add(fc)
    old_comp, new_comp = agg_by_comp(old_files), agg_by_comp(new_files)

    W = 79
    print(rule(W))
    print(f"Delta: {old_label}  ->  {new_label}")
    print(rule(W))
    print(f"{'Bucket':<24}{old_label[:11]:>13}{new_label[:11]:>13}{'delta':>11}")
    print(rule(W))
    for label, attr in (("Production code", "code"),
                        ("Test code", "test_code"),
                        ("Comments", "total_comment"),
                        ("Files", "files")):
        o, n = getattr(old_total, attr), getattr(new_total, attr)
        print(f"{label:<24}{o:>13}{n:>13}{n - o:>+11}")
    print(rule(W))

    print()
    print(rule(W))
    print(f"{'Component':<32}{'prod code':>13}{'delta':>10}{'test delta':>12}")
    print(rule(W))
    rows = []
    for comp in sorted(set(old_comp) | set(new_comp)):
        o = old_comp.get(comp, Agg())
        n = new_comp.get(comp, Agg())
        if o.code == n.code and o.test_code == n.test_code:
            continue
        rows.append((n.code - o.code, comp, n.code, n.test_code - o.test_code))
    for d, comp, now, td in sorted(rows):
        print(f"{comp:<32}{now:>13}{d:>+10}{td:>+12}")
    print(rule(W))
    print("A component that grew in `prod code` while shrinking overall usually "
          "gained\nan inline #[cfg(test)] module — check `test delta` beside it "
          "before concluding\nthe refactor added production code.")
    print(rule(W))


def component_of(relpath: str) -> str:
    parts = relpath.split(os.sep)
    if len(parts) > 2:
        return os.sep.join(parts[:2])
    return parts[0]


@dataclass
class Agg:
    files: int = 0
    blank: int = 0
    comment: int = 0
    code: int = 0
    test_blank: int = 0
    test_comment: int = 0
    test_code: int = 0
    test_files: int = 0
    unsafe_sites: int = 0
    test_unsafe_sites: int = 0
    unsafe_code: int = 0
    test_unsafe_code: int = 0
    #: True once ANY file in this component carried `#![forbid(unsafe_code)]` —
    #: in practice its `lib.rs`, since the attribute is crate-level.
    forbids_unsafe: bool = False

    def add(self, fc: FileCount):
        self.files += 1
        self.test_files += 1 if fc.whole_file_test else 0
        self.unsafe_sites += fc.unsafe_sites
        self.test_unsafe_sites += fc.test_unsafe_sites
        self.unsafe_code += fc.unsafe_code
        self.test_unsafe_code += fc.test_unsafe_code
        self.forbids_unsafe = self.forbids_unsafe or fc.forbids_unsafe
        for f in ("blank", "comment", "code", "test_blank", "test_comment", "test_code"):
            setattr(self, f, getattr(self, f) + getattr(fc, f))

    @property
    def total_code(self) -> int:
        return self.code + self.test_code

    @property
    def safe_code(self) -> int:
        """Production code lines the compiler is checking."""
        return self.code - self.unsafe_code

    @property
    def total_comment(self) -> int:
        return self.comment + self.test_comment

    @property
    def total_blank(self) -> int:
        return self.blank + self.test_blank


def rule(width: int) -> str:
    return "-" * width


def pct(part: int, whole: int) -> str:
    return f"{(100.0 * part / whole):5.1f}%" if whole else "    - "


def print_report(files: list, paths: list, ignored: int, by_file: bool, top: int) -> None:
    by_lang: dict = {}
    by_comp: dict = {}
    total = Agg()
    for fc in files:
        by_lang.setdefault(fc.lang, Agg()).add(fc)
        by_comp.setdefault(component_of(fc.path), Agg()).add(fc)
        total.add(fc)

    W = 79
    print(rule(W))
    print(f"Akuma line count — {', '.join(paths)}")
    print(rule(W))
    print(f"{'Language':<24}{'files':>8}{'blank':>10}{'comment':>10}{'code':>10}{'% test':>10}")
    print(rule(W))
    for lang, agg in sorted(by_lang.items(), key=lambda kv: -kv[1].total_code):
        print(
            f"{lang:<24}{agg.files:>8}{agg.total_blank:>10}{agg.total_comment:>10}"
            f"{agg.total_code:>10}{pct(agg.test_code, agg.total_code):>10}"
        )
    print(rule(W))
    print(
        f"{'SUM':<24}{total.files:>8}{total.total_blank:>10}{total.total_comment:>10}"
        f"{total.total_code:>10}{pct(total.test_code, total.total_code):>10}"
    )
    print(rule(W))

    print()
    print(rule(W))
    print(f"{'Bucket':<24}{'files':>8}{'blank':>10}{'comment':>10}{'code':>10}{'% code':>10}")
    print(rule(W))
    print(
        f"{'Production':<24}{total.files - total.test_files:>8}{total.blank:>10}"
        f"{total.comment:>10}{total.code:>10}{pct(total.code, total.total_code):>10}"
    )
    print(
        f"{'Tests':<24}{total.test_files:>8}{total.test_blank:>10}"
        f"{total.test_comment:>10}{total.test_code:>10}"
        f"{pct(total.test_code, total.total_code):>10}"
    )
    print(rule(W))
    ratio = (total.test_code / total.code) if total.code else 0.0
    print(f"comment / code ......... {pct(total.total_comment, total.total_code).strip()}")
    print(f"test code / prod code .. {ratio:.2f}x")
    print(f"physical lines ......... {total.total_blank + total.total_comment + total.total_code}")
    if ignored:
        print(f"skipped (unknown ext) .. {ignored} files")
    print(rule(W))

    print()
    print(rule(W))
    print(
        f"{'Component':<28}{'files':>7}{'code':>9}{'comment':>9}"
        f"{'prod code':>11}{'test code':>11}"
    )
    print(rule(W))
    for comp, agg in sorted(by_comp.items(), key=lambda kv: -kv[1].total_code):
        print(
            f"{comp:<28}{agg.files:>7}{agg.total_code:>9}{agg.total_comment:>9}"
            f"{agg.code:>11}{agg.test_code:>11}"
        )
    print(rule(W))

    print_unsafe_report(by_comp, W)

    if top:
        print()
        print(rule(W))
        print(f"Top {top} files by code lines")
        print(rule(W))
        print(f"{'File':<48}{'code':>9}{'comment':>10}{'test':>10}")
        print(rule(W))
        for fc in sorted(files, key=lambda f: -(f.code + f.test_code))[:top]:
            name = fc.path if len(fc.path) <= 47 else "…" + fc.path[-46:]
            print(
                f"{name:<48}{fc.code + fc.test_code:>9}"
                f"{fc.comment + fc.test_comment:>10}{fc.test_code:>10}"
            )
        print(rule(W))

    if by_file:
        print()
        print(rule(W))
        print(f"{'File':<44}{'blank':>8}{'comment':>9}{'code':>8}{'test code':>11}")
        print(rule(W))
        for fc in sorted(files, key=lambda f: f.path):
            name = fc.path if len(fc.path) <= 43 else "…" + fc.path[-42:]
            print(
                f"{name:<44}{fc.blank + fc.test_blank:>8}"
                f"{fc.comment + fc.test_comment:>9}{fc.code:>8}{fc.test_code:>11}"
            )
        print(rule(W))


def print_unsafe_report(by_comp: dict, W: int) -> None:
    """`unsafe` per crate: how many sites, how many LINES, and how much is enforced.

    Two different safety numbers, deliberately both reported — see the comment on
    the summary block for why quoting only the enforced one misleads.

    Exists because the same two numbers were being hand-maintained in
    `docs/reference/crate-safety.md` and went stale: its prose said "Ten of the
    eighteen" while its own tables listed 12 and 9. Numbers a document cannot
    regenerate are numbers that drift, so regenerate them.

    Only `crates/*` rows can be enforced-safe — `src` is the bin crate and carries
    no crate-level ban. **`src/` rows are still listed**, marked `bin` in the
    enforced column: leaving them out was how the biggest concentration of
    `unsafe` in the tree stayed off the one table that measures `unsafe`, which is
    the failure mode this whole report exists to prevent. `src/` cannot be
    *enforced* safe; that is a fact worth showing, not a reason to hide the rows.
    The `enforced unsafe-free` ratio below still counts `crates/*` only, because
    a bin crate was never a candidate.
    """
    crates = {c: a for c, a in by_comp.items() if c.startswith("crates" + os.sep)}
    bins = {c: a for c, a in by_comp.items() if not c.startswith("crates" + os.sep)}
    if not crates and not bins:
        return

    def rows(d):
        return sorted(d.items(), key=lambda kv: (kv[1].unsafe_code, -kv[1].code, kv[0]))

    print()
    print(rule(W))
    print(
        f"{'Unsafe by crate':<26}{'prod code':>11}{'sites':>7}{'unsafe':>8}"
        f"{'safe':>8}{'enforced':>10}"
    )
    print(rule(W))
    for comp, agg in rows(crates):
        mark = "forbid" if agg.forbids_unsafe else ""
        print(
            f"{comp:<26}{agg.code:>11}{agg.unsafe_sites:>7}{agg.unsafe_code:>8}"
            f"{pct(agg.safe_code, agg.code):>8}{mark:>10}"
        )
    if bins:
        print(rule(W))
        for comp, agg in rows(bins):
            print(
                f"{comp:<26}{agg.code:>11}{agg.unsafe_sites:>7}{agg.unsafe_code:>8}"
                f"{pct(agg.safe_code, agg.code):>8}{'bin':>10}"
            )
    print(rule(W))

    safe = [a for a in crates.values() if a.forbids_unsafe]
    safe_code = sum(a.total_code for a in safe)
    all_code = sum(a.total_code for a in crates.values())
    all_unsafe = sum(a.unsafe_sites for a in crates.values())
    all_test_unsafe = sum(a.test_unsafe_sites for a in crates.values())
    leaked = sum(a.unsafe_sites + a.test_unsafe_sites for a in safe)
    prod_code = sum(a.code for a in crates.values())
    prod_unsafe_code = sum(a.unsafe_code for a in crates.values())
    prod_safe_code = prod_code - prod_unsafe_code

    print(f"enforced unsafe-free ... {len(safe)} of {len(crates)} crates")
    print(f"code in those crates ... {safe_code} of {all_code} ({pct(safe_code, all_code).strip()})")
    # The two percentages answer different questions and the gap between them is
    # the point, so both are printed. The first is a GUARANTEE: code in a crate
    # the compiler refuses to let anyone write `unsafe` in. The second is a
    # MEASUREMENT: lines that happen not to sit in an `unsafe` block, in crates
    # where one still could. A crate with a single 3-line block in 3,000 lines
    # scores 0% on the first and 99.9% on the second; neither number is wrong,
    # and quoting only the first badly understates how much of the tree is
    # actually compiler-checked.
    print(
        f"safe production code ... {prod_safe_code} of {prod_code} "
        f"({pct(prod_safe_code, prod_code).strip()}) — lines outside any `unsafe` block"
    )
    print(
        f"unsafe sites ........... {all_unsafe + all_test_unsafe} across crates/ "
        f"({all_unsafe} production, {all_test_unsafe} test), {leaked} inside enforced crates"
    )
    if leaked:
        # `forbid` makes this a hard compile error, so a non-zero count means the
        # counter is wrong (or an `#[unsafe(...)]` attribute was counted), not that
        # the ban was bypassed.
        print("  !! non-zero inside a `forbid(unsafe_code)` crate — check the counter")

    # Production vs test `unsafe`, per scope and for the tree. `src/` is the bulk
    # of the tree's test `unsafe` (the in-kernel boot suite forges trap frames and
    # builds page tables by hand, which is the job) and quoting a crates/-only
    # figure as "the tree" overstates how clean things are by a wide margin.
    if bins:
        def tot(d, attr):
            return sum(getattr(a, attr) for a in d.values())

        print(rule(W))
        print(f"{'unsafe sites by scope':<26}{'total':>11}{'production':>12}{'test':>8}")
        print(rule(W))
        scopes = [("crates/", crates), ("src/", bins)]
        for label, d in scopes:
            prod, test = tot(d, "unsafe_sites"), tot(d, "test_unsafe_sites")
            print(f"{label:<26}{prod + test:>11}{prod:>12}{test:>8}")
        prod = sum(tot(d, "unsafe_sites") for _, d in scopes)
        test = sum(tot(d, "test_unsafe_sites") for _, d in scopes)
        print(f"{'tree':<26}{prod + test:>11}{prod:>12}{test:>8}")
    print(rule(W))


# ---------------------------------------------------------------------------
# self-test
# ---------------------------------------------------------------------------

#: (name, source, expected production `unsafe` sites, expected `unsafe` CODE lines).
#:
#: These exist because the unsafe-line counter is the one number here a reader
#: cannot check by eye against a grep — `grep -c unsafe` gets every one of them
#: wrong in a different direction, which is the whole reason this file lexes.
UNSAFE_CASES = [
    ("one-line block", "fn f() {\n    unsafe { g() };\n}\n", 1, 1),
    (
        "multi-line block counts its body",
        "fn f() {\n    unsafe {\n        a();\n        b();\n    }\n}\n",
        1,
        4,
    ),
    (
        "blank and comment lines inside a block are not code",
        "fn f() {\n    unsafe {\n        // why\n\n        a();\n    }\n}\n",
        1,
        3,
    ),
    (
        "unsafe fn body is NOT an unsafe context (edition 2024)",
        "unsafe fn f() {\n    a();\n    b();\n}\n",
        1,
        1,
    ),
    (
        # The `unsafe fn` line, plus the three lines of the inner block.
        "but an inner block inside an unsafe fn is",
        "unsafe fn f() {\n    unsafe {\n        a();\n    }\n}\n",
        2,
        4,
    ),
    ("unsafe impl is one line", "unsafe impl Send for X {}\n", 1, 1),
    ("unsafe trait is one line", "unsafe trait T {\n    fn a();\n}\n", 1, 1),
    (
        "unsafe extern block declares, it does not execute",
        'unsafe extern "C" {\n    fn a();\n}\n',
        1,
        1,
    ),
    (
        # NOT counted, and that is deliberate. The lexer consumes `#[..]`
        # wholesale, so the keyword inside never reaches the identifier branch —
        # and it should not: `#[unsafe(no_mangle)]` marks a declaration, it is
        # not an operation the compiler stopped checking. There are a dozen of
        # these in the tree; scoring them as unsafe *lines* would inflate the
        # number this report exists to give.
        "#[unsafe(..)] attribute is a declaration marker, not a site",
        "#[unsafe(no_mangle)]\nfn f() {\n    a();\n}\n",
        0,
        0,
    ),
    (
        "nested blocks do not double-count lines",
        "fn f() {\n    unsafe {\n        unsafe { a() };\n    }\n}\n",
        2,
        3,
    ),
    ("the word in a line comment is not a site", "fn f() {\n    // unsafe { }\n}\n", 0, 0),
    ("the word in a block comment is not a site", "fn f() {\n    /* unsafe */\n}\n", 0, 0),
    ('the word in a string is not a site', 'fn f() {\n    p("unsafe { }");\n}\n', 0, 0),
    (
        "the word in a raw string is not a site",
        'fn f() {\n    p(r#"unsafe { }"#);\n}\n',
        0,
        0,
    ),
    ("unsafe_code is one identifier, not `unsafe`", "#![forbid(unsafe_code)]\nfn f() {}\n", 0, 0),
    (
        "a test-gated block is charged to test, not production",
        "#[cfg(test)]\nmod t {\n    fn f() {\n        unsafe { a() };\n    }\n}\n",
        0,
        0,
    ),
]


def self_test() -> int:
    """Check the unsafe counter against cases a grep gets wrong. Returns exit code."""
    spec = EXT_LANGS[".rs"]
    failures = 0
    for name, src, want_sites, want_lines in UNSAFE_CASES:
        fc = count_text(src, "probe.rs", spec, True)
        got = (fc.unsafe_sites, fc.unsafe_code)
        ok = got == (want_sites, want_lines)
        failures += 0 if ok else 1
        status = "ok  " if ok else "FAIL"
        print(f"  {status} {name}")
        if not ok:
            print(f"       want sites={want_sites} lines={want_lines}, got sites={got[0]} lines={got[1]}")

    # An invariant rather than a case: a line can only be unsafe if it is code,
    # so the unsafe count can never exceed the code count for any real file.
    over = []
    for root in ("src", "crates"):
        if not os.path.isdir(root):
            continue
        for dirpath, _dirs, names in os.walk(root):
            for nm in names:
                if not nm.endswith(".rs"):
                    continue
                rel = os.path.join(dirpath, nm)
                fc = count_file(rel, rel, spec, True)
                if fc.unsafe_code > fc.code or fc.test_unsafe_code > fc.test_code:
                    over.append(rel)
    if over:
        failures += 1
        print(f"  FAIL unsafe lines exceed code lines in {len(over)} file(s): {over[:3]}")
    else:
        print("  ok   unsafe lines <= code lines in every file")

    # Overlapping roots must not double-count. This shipped broken: `walk`
    # appended every file under every root arg with no dedup, so
    # `cloc_akuma.py src crates src/syscall` doubled every column for the shared
    # files in lockstep (`src/syscall` read 46 files / 22,886 code against a true
    # 23 / 11,443). A clean 2x reads as a plausible number, and this script is
    # what regenerates the figures in docs/reference/crate-safety.md, so the
    # doubling would have landed there as authoritative. Fixed 2026-08-31; pinned
    # here because nothing else would notice it coming back.
    if os.path.isdir("src") and os.path.isdir(os.path.join("src", "syscall")):
        plain, _ = walk(["src"], True)
        overlapped, _ = walk(["src", os.path.join("src", "syscall"), "src"], True)
        same = (len(plain) == len(overlapped)
                and sum(f.code for f in plain) == sum(f.code for f in overlapped))
        failures += 0 if same else 1
        if same:
            print(f"  ok   overlapping roots deduplicate ({len(plain)} files either way)")
        else:
            print(f"  FAIL overlapping roots double-count: {len(plain)} files / "
                  f"{sum(f.code for f in plain)} code alone vs {len(overlapped)} / "
                  f"{sum(f.code for f in overlapped)} with an overlapping root")

    print()
    print("self-test: " + ("PASS" if failures == 0 else f"{failures} FAILURE(S)"))
    return 0 if failures == 0 else 1


def emit_json(files: list, paths: list) -> None:
    total = Agg()
    by_lang: dict = {}
    by_comp: dict = {}
    for fc in files:
        total.add(fc)
        by_lang.setdefault(fc.lang, Agg()).add(fc)
        by_comp.setdefault(component_of(fc.path), Agg()).add(fc)

    def dump(agg: Agg) -> dict:
        return {
            "files": agg.files,
            "test_files": agg.test_files,
            "blank": agg.total_blank,
            "comment": agg.total_comment,
            "code": agg.total_code,
            "prod_code": agg.code,
            "test_code": agg.test_code,
            "prod_comment": agg.comment,
            "test_comment": agg.test_comment,
            "unsafe_sites": agg.unsafe_sites,
            "unsafe_code": agg.unsafe_code,
            "test_unsafe_code": agg.test_unsafe_code,
            "safe_code": agg.safe_code,
            "test_unsafe_sites": agg.test_unsafe_sites,
            "forbids_unsafe": agg.forbids_unsafe,
        }

    print(
        json.dumps(
            {
                "paths": paths,
                "total": dump(total),
                "by_language": {k: dump(v) for k, v in by_lang.items()},
                "by_component": {k: dump(v) for k, v in by_comp.items()},
                "by_file": [
                    {
                        "path": fc.path,
                        "language": fc.lang,
                        "blank": fc.blank + fc.test_blank,
                        "comment": fc.comment + fc.test_comment,
                        "prod_code": fc.code,
                        "test_code": fc.test_code,
                        "test_file": fc.whole_file_test,
                        "unsafe_sites": fc.unsafe_sites,
                        "unsafe_code": fc.unsafe_code,
                        "test_unsafe_code": fc.test_unsafe_code,
                        "test_unsafe_sites": fc.test_unsafe_sites,
                        "forbids_unsafe": fc.forbids_unsafe,
                    }
                    for fc in sorted(files, key=lambda f: f.path)
                ],
            },
            indent=2,
        )
    )


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("paths", nargs="*", default=["src", "crates"], help="files/dirs (default: src crates)")
    ap.add_argument("--by-file", action="store_true", help="per-file table")
    ap.add_argument("--top", type=int, default=15, help="show N largest files (0 = off)")
    ap.add_argument("--json", action="store_true", help="machine-readable output")
    ap.add_argument(
        "--self-test",
        action="store_true",
        help="check the unsafe counter against cases a grep gets wrong",
    )
    ap.add_argument(
        "--no-kernel-test-gate",
        action="store_true",
        help='count #[cfg(not(any(feature = "no-tests", ...)))] items as production code',
    )
    ap.add_argument("--rev", help="count this git revision's tree instead of the working tree")
    ap.add_argument("--vs", metavar="REV",
                    help="also count REV and print the delta REV -> (--rev or working tree)")
    args = ap.parse_args()
    if args.self_test:
        return self_test()

    paths = args.paths or ["src", "crates"]
    kernel_gate = not args.no_kernel_test_gate

    if args.rev:
        files, ignored = walk_rev(paths, kernel_gate, args.rev)
        label = args.rev
    else:
        missing = [p for p in paths if not os.path.exists(p)]
        if missing:
            print(f"error: no such path: {', '.join(missing)}", file=sys.stderr)
            return 1
        files, ignored = walk(paths, kernel_gate)
        label = "working tree"

    if not files:
        print("error: no recognised source files found", file=sys.stderr)
        return 1

    if args.vs:
        old_files, _ = walk_rev(paths, kernel_gate, args.vs)
        if not old_files:
            print(f"error: no recognised source files at {args.vs}", file=sys.stderr)
            return 1
        print_delta(old_files, files, args.vs, label)
        print()

    if args.json:
        emit_json(files, paths)
    else:
        print_report(files, paths, ignored, args.by_file, args.top)
    return 0


if __name__ == "__main__":
    sys.exit(main())
