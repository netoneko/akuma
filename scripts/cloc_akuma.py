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

The report also splits the tree by PLATFORM. The repo is two kernels — the
AArch64 bin (``src/``) and the amd64 bin (``amd64/``) — over one shared crate
pool. Which pool a crate belongs to is derived from the
``#[cfg(target_arch = ...)]`` attributes the lexer already parsed, never from a
hand-kept table: a crate naming exactly one arch is arch-only, anything else is
shared. Each kernel's "links" footprint is its bin plus its arch-only crates
plus the shared pool; the two footprints deliberately double-count the pool, so
the gap between them is the duplication the shared-crate extraction has not yet
absorbed.

Usage:
    scripts/cloc_akuma.py                      # defaults to src/ crates/ amd64/src/ taoxichuan/src/
    scripts/cloc_akuma.py src crates --by-file
    scripts/cloc_akuma.py --json
    scripts/cloc_akuma.py --all-languages      # also C, asm, shell, TOML, Markdown, ...
    scripts/cloc_akuma.py --rev HEAD~5         # count a revision's tree
    scripts/cloc_akuma.py --vs main            # delta: main -> working tree
    scripts/cloc_akuma.py --rev HEAD --vs HEAD~1   # delta across one commit
"""

from __future__ import annotations

import argparse
import glob
import json
import os
import re
import subprocess
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

#: Rust only by default: shell, TOML and Markdown are not the code being measured, and
#: counting them as code inflates every kernel and crate total. `--all-languages`
#: restores the full table. Set once in `main`, read by the two tree walkers.
RUST_ONLY = True


def lang_for(ext: str):
    """The `LangSpec` for a file extension, or None when it is not counted."""
    spec = EXT_LANGS.get(ext)
    if spec is not None and RUST_ONLY and not spec.rust:
        return None
    return spec


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
    #: This file is a crate ROOT carrying `#![forbid(unsafe_code)]`, so the ban
    #: covers the whole crate.
    #:
    #: The root check is the point. `#![forbid(..)]` is an inner attribute: on a
    #: non-root file it bans `unsafe` for that module tree only, and the crate
    #: around it can still be full of it. Counting those as enforced is what made
    #: this table report `akuma-kernel-glue` as forbidding while it still held
    #: boot assembly and three `unsafe` sites — `console.rs` carried a
    #: module-level ban. A module-level ban is now simply not counted; the tree
    #: has none left (2026-09-01), and if one comes back it understates the
    #: crate rather than overstating it.
    forbids_unsafe: bool = False
    #: Production CODE lines lying inside an `unsafe { .. }` block, plus the
    #: declaration line of an `unsafe fn`/`impl`/`trait`. This is the "how much
    #: of what ships is the compiler NOT checking" number; `unsafe_sites` counts
    #: how many places it happens, which is a different question — one 40-line
    #: block is 1 site and 40 lines.
    unsafe_code: int = 0
    #: The same, for lines the test buckets claimed.
    test_unsafe_code: int = 0
    #: Target architectures named in `#[cfg(target_arch = "...")]` attributes
    #: in this file. Derived from the source, not from a hand-kept table, so a
    #: crate that grows an amd64 backend starts counting as shared without
    #: anyone remembering to update a list. `cfg!(target_arch = ..)` macros are
    #: NOT seen (only `#[cfg(..)]` attributes are parsed); that under-counts
    #: towards "shared", which is the honest direction — a crate classified
    #: arch-only has provably no code for the other arch.
    arches: set = field(default_factory=set)

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
    # Architectures named by `#[cfg(target_arch = "...")]` attributes (see
    # FileCount.arches).
    arches: set = set()

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
            for m in ARCH_RE.finditer(text[start:j]):
                arches.add(m.group(1))
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

                # `r#ident` — a *raw identifier*, not a raw string. This is the
                # escape hatch for naming something after a keyword, so
                # `r#unsafe` is precisely the one spelling of those six letters
                # that is NOT the keyword. Falling through to the identifier
                # scanner below counted it as an unsafe site: the scanner saw
                # `r`, then a bare `#`, then `unsafe`. Nothing in this tree
                # writes it today; it is fixed because the failure is silent and
                # in the direction that inflates the number.
                if hashes and k < n and _is_ident_char(text[k]) and not text[k].isdigit():
                    while k < n and _is_ident_char(text[k]):
                        k += 1
                    last_ident = ""  # a raw ident can never name an asm macro
                    i = k
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
            if spec.rust and last_ident == "unsafe":
                # Reached only from the identifier branch, which strings, comments,
                # char literals and `asm!` bodies never fall through to — so this
                # counts real `unsafe` sites, unlike a grep over the file. The line
                # is kept, not just a tally, so `count_text` can split the sites into
                # production and test using the same rule it buckets lines with.
                #
                # Gated on `spec.rust`: `unsafe` is a Rust keyword, but the
                # non-Rust specs (Markdown, TOML, shell) have no strings or
                # comments, so prose lexes as bare identifiers — the word
                # "unsafe" in a crate README counted as a site and rolled up
                # into that crate's tally, tripping the forbid-contradiction
                # self-check. Found 2026-10-18; the three crates it flagged
                # (pci, usb, net-rtl8169) were never guilty.
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

    return code, comment, test, all_file_test, unsafe_lines, unsafe_span, arches


#: A crate-level `#![forbid(unsafe_code)]`, at the start of a line so a mention
#: inside a doc comment ("carries `#![forbid(unsafe_code)]`") does not match.
FORBID_RE = re.compile(r"^\s*#!\[forbid\([^)]*\bunsafe_code\b", re.M)

#: The arch named by a `#[cfg(target_arch = "...")]` attribute. Matched against
#: the attribute's inner text, which the lexer has already cut out of comments
#: and strings — a doc comment saying `target_arch = "x86_64"` does not match.
ARCH_RE = re.compile(r'\btarget_arch\s*=\s*"([a-z0-9_]+)"')


def count_file(path: str, relpath: str, spec: LangSpec, kernel_gate: bool) -> FileCount:
    with open(path, "r", encoding="utf-8", errors="replace") as fh:
        text = fh.read()
    return count_text(text, relpath, spec, kernel_gate)


def count_text(text: str, relpath: str, spec: LangSpec, kernel_gate: bool) -> FileCount:
    nlines = text.count("\n") + (1 if text and not text.endswith("\n") else 0)
    code, comment, test, inner_test, unsafe_lines, unsafe_span, arches = scan(
        text, spec, kernel_gate
    )

    fc = FileCount(path=relpath, lang=spec.name)
    fc.arches = arches
    fc.whole_file_test = is_test_path(relpath) or inner_test
    fc.forbids_unsafe = FORBID_RE.search(text) is not None and os.path.basename(
        relpath
    ) in ("lib.rs", "main.rs")
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
            spec = lang_for(os.path.splitext(root_arg)[1])
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
                spec = lang_for(os.path.splitext(fn)[1])
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
        spec = lang_for(os.path.splitext(rel)[1])
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
    #: True once this component's crate root carried `#![forbid(unsafe_code)]`.
    #: See `FileCount.forbids_unsafe` for why only the root counts.
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

    def merge(self, other: "Agg"):
        """Add an already-aggregated bucket — footprints sum component Aggs,
        which `add` (file-level) would misread as ten files' worth of counts."""
        self.files += other.files
        self.test_files += other.test_files
        self.unsafe_sites += other.unsafe_sites
        self.test_unsafe_sites += other.test_unsafe_sites
        self.unsafe_code += other.unsafe_code
        self.test_unsafe_code += other.test_unsafe_code
        self.forbids_unsafe = self.forbids_unsafe or other.forbids_unsafe
        for f in ("blank", "comment", "code", "test_blank", "test_comment", "test_code"):
            setattr(self, f, getattr(self, f) + getattr(other, f))

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
        print(f"skipped (not Rust, or unknown ext) .. {ignored} files")
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
    print_platform_report(files, W)

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
        # One row per root that was actually counted. `bins` used to be reported as
        # a single "src/", which silently folded `amd64/src` into the AArch64
        # kernel's number; the two kernels are different programs and the
        # port's `unsafe` is the one being driven down.
        def root_of(comp: str) -> str:
            parts = comp.split(os.sep)
            return os.sep.join(parts[:2] if parts[0] in ("amd64", "taoxichuan") else parts[:1]) + os.sep

        scopes = [("crates/", crates)]
        for root in sorted({root_of(c) for c in bins}, key=lambda r: (r != "src/", r)):
            scopes.append((root, {c: a for c, a in bins.items() if root_of(c) == root}))
        for label, d in scopes:
            prod, test = tot(d, "unsafe_sites"), tot(d, "test_unsafe_sites")
            print(f"{label:<26}{prod + test:>11}{prod:>12}{test:>8}")
        prod = sum(tot(d, "unsafe_sites") for _, d in scopes)
        test = sum(tot(d, "test_unsafe_sites") for _, d in scopes)
        print(rule(W))
        print(f"{'all roots (sum)':<26}{prod + test:>11}{prod:>12}{test:>8}")
    print(rule(W))


# ---------------------------------------------------------------------------
# platforms
# ---------------------------------------------------------------------------


_PATH_DEP = re.compile(r'path\s*=\s*"([^"]+)"')


def path_deps(manifest: str) -> set:
    """Repo-relative dirs a manifest names with `path = "..."` in a normal
    `[dependencies]`-style table (target-specific and optional ones included;
    dev- and build-dependencies are not part of what a kernel links)."""
    found: set = set()
    try:
        text = open(manifest, encoding="utf-8").read()
    except OSError:
        return found
    base = os.path.dirname(manifest)
    live = False
    for line in text.splitlines():
        line = line.strip()
        if line.startswith("["):
            live = "dependencies" in line and not (
                "dev-dependencies" in line or "build-dependencies" in line
            )
            continue
        if live:
            m = _PATH_DEP.search(line)
            if m:
                found.add(os.path.normpath(os.path.join(base, m.group(1))))
    return found


_TREE_LINE = re.compile(r"^(\S+) v(\S+)(?: \(([^)]*)\))?")


def cargo_tree(pkg: str, triple: str, features: str = "", default: bool = True):
    """Packages cargo links into `pkg` for `triple`: [(name, version, abs_path_or_None)].

    `cargo tree` resolves features for THIS package alone (`cargo metadata`
    unifies them across the whole workspace, which can switch on optional
    dependencies another member wanted), applies target-specific dependency
    tables, and with `no-proc-macro` drops host-only crates (`syn`, `quote`).
    None if cargo cannot answer.
    """
    cmd = ["cargo", "tree", "-p", pkg, "--target", triple, "-e", "normal,no-proc-macro",
           "--prefix", "none", "--offline"]
    if not default:
        cmd.append("--no-default-features")
    if features:
        cmd += ["--features", features]
    try:
        out = subprocess.run(cmd, capture_output=True, text=True, timeout=120, check=True).stdout
    except (OSError, subprocess.SubprocessError):
        return None
    seen: set = set()
    for line in out.splitlines():
        m = _TREE_LINE.match(line.strip())
        if not m:
            continue
        loc = m.group(3) or ""
        seen.add((m.group(1), m.group(2), loc if loc.startswith("/") else None))
    return sorted(seen)


#: manifest -> (cargo package, target triple) for the kernels `linked_crates` can ask cargo about.
KERNEL_PACKAGES = {
    "Cargo.toml": ("akuma", "aarch64-unknown-none"),
    os.path.join("amd64", "Cargo.toml"): ("akuma-amd64", "x86_64-unknown-none"),
    os.path.join("taoxichuan", "Cargo.toml"): ("taoxichuan", "x86_64-unknown-none"),
}


def linked_crates(root_manifest: str) -> set:
    """Workspace crate directories a kernel links, as repo-relative paths (the
    kernel's own dir excluded).

    Asks `cargo tree` (feature- and target-aware) for the kernels in
    KERNEL_PACKAGES. The fallback, and the walk for any other manifest, reads
    `path = ".."` lines out of Cargo.toml text, which cannot see features or
    that a crate is only needed for another architecture — it over-counts
    (taoxichuan: 29 crates by text, 27 by cargo) — so it is a fallback only.
    """
    kp = KERNEL_PACKAGES.get(root_manifest)
    if kp:
        tree = cargo_tree(*kp)
        if tree:
            root = os.path.abspath(".")
            own = os.path.abspath(os.path.dirname(root_manifest))
            dirs = set()
            for _name, _ver, path in tree:
                if path and os.path.abspath(path) not in (own, root):
                    dirs.add(os.path.relpath(path, root))
            return dirs
    seen: set = set()
    todo = list(path_deps(root_manifest))
    while todo:
        d = todo.pop()
        if d in seen:
            continue
        seen.add(d)
        todo.extend(path_deps(os.path.join(d, "Cargo.toml")))
    return seen


def platform_buckets(files: list) -> dict:
    """Group every component into a platform bucket, derived from the source.

    The tree is two kernels over one shared crate pool: `src/` is the AArch64
    bin (root `Cargo.toml`), `amd64/` is the second bin, `crates/` serve
    whichever kernels link them. Which kernel links a crate is read from the
    Cargo dependency graph (path dependencies, transitively):

      linked by both kernels — shared
      linked by only amd64   — amd64-only
      linked by only AArch64 — aarch64-only

    A crate neither kernel reaches (host tools, userspace) falls back to its
    `#[cfg(target_arch = ...)]` attributes: exactly one arch named picks that
    arch's bucket, anything else is shared.

    The cfg scan alone is NOT trustworthy for crates that are reached: a crate
    whose only `target_arch` mention is an `x86_64` gate beside an unconditional
    AArch64 path (akuma-bkl, akuma-syscalls-glue, akuma-net-nic) names one arch
    yet serves both, and was reported amd64-only.
    """
    comp_arches: dict = {}
    for fc in files:
        comp_arches.setdefault(component_of(fc.path), set()).update(fc.arches)

    a64 = linked_crates("Cargo.toml")
    x64 = linked_crates(os.path.join("amd64", "Cargo.toml"))
    have_graph = bool(a64 or x64)

    buckets: dict = {}
    for comp, arches in comp_arches.items():
        if comp == "src" or comp.startswith("src" + os.sep):
            buckets.setdefault("aarch64 bin", []).append(comp)
        elif comp == "amd64" or comp.startswith("amd64" + os.sep):
            buckets.setdefault("amd64 bin", []).append(comp)
        elif comp == "taoxichuan" or comp.startswith("taoxichuan" + os.sep):
            buckets.setdefault("taoxichuan bin", []).append(comp)
        elif have_graph and comp in a64 and comp in x64:
            buckets.setdefault("shared crates", []).append(comp)
        elif have_graph and comp in x64:
            buckets.setdefault("amd64-only crates", []).append(comp)
        elif have_graph and comp in a64:
            buckets.setdefault("aarch64-only crates", []).append(comp)
        elif arches == {"x86_64"}:
            buckets.setdefault("amd64-only crates", []).append(comp)
        elif arches == {"aarch64"}:
            buckets.setdefault("aarch64-only crates", []).append(comp)
        else:
            buckets.setdefault("shared crates", []).append(comp)
    return buckets


def print_platform_report(files: list, W: int) -> None:
    """Per-platform view: what each kernel is actually made of.

    The component table above answers "where does the code live"; this answers
    "what does each kernel link", which is a different question — the shared
    pool belongs to both, so the two footprints deliberately double-count it
    and their sum is larger than the tree. A bin's footprint is its own source
    plus its arch-only crates plus the shared pool; that is the number to
    compare across the two kernels, and the gap between the footprints is the
    duplication + arch asymmetry the shared-crate extraction has not yet
    absorbed.
    """
    buckets = platform_buckets(files)
    if not buckets:
        return
    by_bucket: dict = {}
    members: dict = {}
    for name, comps in buckets.items():
        agg = by_bucket.setdefault(name, Agg())
        for fc in files:
            if component_of(fc.path) in comps:
                agg.add(fc)
                members.setdefault(name, set()).add(fc.path.split(os.sep)[1])

    print()
    print(rule(W))
    print("By platform")
    print(rule(W))
    print(
        f"{'Bucket':<24}{'files':>8}{'prod code':>11}{'test code':>11}"
        f"{'unsafe':>8}{'enforced':>10}"
    )
    print(rule(W))
    order = ("aarch64 bin", "amd64 bin", "taoxichuan bin", "aarch64-only crates",
             "amd64-only crates", "shared crates")
    for name in order:
        if name not in by_bucket:
            continue
        agg = by_bucket[name]
        mark = "forbid" if agg.forbids_unsafe else ""
        print(
            f"{name:<24}{agg.files:>8}{agg.code:>11}{agg.test_code:>11}"
            f"{agg.unsafe_sites:>8}{mark:>10}"
        )
    print(rule(W))

    for label, bin_key, only_key in (
        ("aarch64 kernel links", "aarch64 bin", "aarch64-only crates"),
        ("amd64 kernel links", "amd64 bin", "amd64-only crates"),
    ):
        parts = [by_bucket[k] for k in (bin_key, only_key, "shared crates") if k in by_bucket]
        if not parts:
            continue
        tot = Agg()
        for agg in parts:
            tot.merge(agg)
        print(f"{label:<24}{tot.files:>8}{tot.code:>11}{tot.test_code:>11}{tot.unsafe_sites:>8}")

    # The minimal self-hosting kernel (`taoxichuan/`, docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md):
    # its footprint is its own source plus the closure of its path dependencies, which sit in
    # whichever bucket above they belong to, so it cannot be a sum of buckets.
    if "taoxichuan bin" in by_bucket:
        linked = linked_crates(os.path.join("taoxichuan", "Cargo.toml"))
        tx = Agg()
        tx.merge(by_bucket["taoxichuan bin"])
        n_crates = 0
        seen_comps: set = set()
        for fc in files:
            comp = component_of(fc.path)
            if comp in linked:
                tx.add(fc)
                seen_comps.add(comp)
        n_crates = len(seen_comps)
        print(f"{'taoxichuan kernel links':<24}{tx.files:>8}{tx.code:>11}{tx.test_code:>11}{tx.unsafe_sites:>8}")
        print(f"  ({n_crates} of {len(linked)} linked crates counted, plus taoxichuan/src itself)")

    for name in ("aarch64-only crates", "amd64-only crates"):
        if name in members:
            crates = sorted(members[name])
            print(f"{name}: {', '.join(crates)}")
    print(rule(W))
    print("Shared crates are counted in both footprints, so the two `links` rows")
    print("sum to more than the tree. Attribution comes from each kernel's")
    print("Cargo path-dependency closure; crates neither kernel reaches fall")
    print("back to #[cfg(target_arch = ..)] attributes (cfg!() not parsed).")
    print(rule(W))


#: kernel name -> (manifest, target triple). The triple is what `cargo metadata
#: --filter-platform` resolves target-specific dependencies against.
EXTERNAL_KERNELS = {
    "aarch64": ("Cargo.toml", "aarch64-unknown-none"),
    "amd64": (os.path.join("amd64", "Cargo.toml"), "x86_64-unknown-none"),
    "taoxichuan": (os.path.join("taoxichuan", "Cargo.toml"), "x86_64-unknown-none"),
}


def external_closure(manifest: str, triple: str):
    """crates.io / git packages a kernel links, as [(name, version, src_dir)].

    Asks `cargo tree` (offline: the registry is already unpacked for the
    build) for the kernel package's NORMAL dependency edges, so
    build- and dev-dependencies are out, features are resolved for that package
    alone, and proc-macro crates go with everything only they pull in (syn,
    quote, ...): they run on the host and none of their code is in the image.
    Returns None if cargo cannot answer.
    """
    kp = KERNEL_PACKAGES.get(manifest)
    tree = cargo_tree(kp[0], triple) if kp else None
    if tree is None:
        return None
    try:
        meta = json.loads(subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--offline", "--filter-platform", triple],
            capture_output=True, text=True, timeout=120, check=True,
        ).stdout)
    except (OSError, subprocess.SubprocessError, ValueError):
        return None
    where = {(p["name"], p["version"]): os.path.dirname(p["manifest_path"])
             for p in meta["packages"] if p["source"]}
    # `cargo tree` says WHICH crates link (feature-correct); `metadata` only says where their source is.
    return sorted(
        (n, v, os.path.join(where[(n, v)], "src"))
        for n, v, path in tree if path is None and (n, v) in where
    )


def print_external_report(kernel_gate: bool, W: int) -> None:
    """Code linked from external libraries, per kernel.

    Each external crate's `src/` is counted whole: this is an UPPER bound on
    what is compiled in, because cargo features and `#[cfg]`s prune code this
    counter does not evaluate (virtio-drivers' optional transports, zerocopy's
    derive support). It is the amount of third-party source a reviewer would
    have to trust, which is the number the experiment wants.
    """
    print()
    print(rule(W))
    print("External libraries linked (normal deps, proc-macros excluded; crate src/ counted whole)")
    print(rule(W))
    for kernel, (manifest, triple) in EXTERNAL_KERNELS.items():
        if not os.path.exists(manifest):
            continue
        deps = external_closure(manifest, triple)
        if deps is None:
            print(f"{kernel}: cargo metadata unavailable (offline registry missing?)")
            continue
        tot = Agg()
        rows_ = []
        for name, ver, src in deps:
            if not os.path.isdir(src):
                continue
            files, _ = walk([src], kernel_gate)
            a = Agg()
            for fc in files:
                a.add(fc)
            tot.merge(a)
            rows_.append((a.code, f"{name} {ver}", a))
        print(f"{kernel} ({triple}): {len(rows_)} crates")
        print(f"  {'crate':<30}{'files':>8}{'prod code':>11}{'test code':>11}{'unsafe':>8}")
        for _, label, a in sorted(rows_, key=lambda r: -r[0]):
            print(f"  {label:<30}{a.files:>8}{a.code:>11}{a.test_code:>11}{a.unsafe_sites:>8}")
        print(f"  {'TOTAL':<30}{tot.files:>8}{tot.code:>11}{tot.test_code:>11}{tot.unsafe_sites:>8}")
        print(rule(W))


def elf_symbol_bytes(elf: str):
    """{crate: bytes} of defined symbols in an UNSTRIPPED ELF, via `rust-nm`.

    A symbol is attributed to the crate named first in its demangled path
    (`<smoltcp::X as T>::f` -> smoltcp). Code inlined into a caller is the
    caller's, so this UNDER-counts small helper crates and generics; a crate
    with zero bytes is genuinely absent or fully inlined. The extreme-size
    profile strips symbols, so build with CARGO_PROFILE_EXTREME_SIZE_STRIP=none.
    """
    try:
        out = subprocess.run(["rust-nm", "-S", "--demangle", "--defined-only", elf],
                             capture_output=True, text=True, timeout=120, check=True).stdout
    except (OSError, subprocess.SubprocessError):
        return None
    ident = re.compile(r"([A-Za-z_][A-Za-z0-9_]*)::")
    sz: dict = {}
    for line in out.splitlines():
        parts = line.split(None, 3)
        if len(parts) < 4:
            continue
        try:
            size = int(parts[1], 16)
        except ValueError:
            continue
        name = re.sub(r"^[<&*\[\(\)\]\s]*(mut |dyn |const )?", "", parts[3].strip())
        m = ident.match(name)
        sz[m.group(1) if m else "(unattributed)"] = sz.get(m.group(1) if m else "(unattributed)", 0) + size
    return sz


def print_elf_report(elf: str, pkg: str, triple: str, features: str, default: bool,
                     kernel_gate: bool, W: int) -> None:
    """Source lines vs bytes actually in the image, per linked crate.

    The line-count footprint above is "what the dependency graph links"; this
    is "what the linker kept". They differ a lot: a crate can be in the graph
    and contribute nothing once LTO drops what nothing calls.
    """
    sz = elf_symbol_bytes(elf)
    tree = cargo_tree(pkg, triple, features, default)
    if sz is None or tree is None:
        print(f"--elf: need `rust-nm` and `cargo tree` ({elf})")
        return
    print()
    print(rule(W))
    print(f"Image check: {elf}")
    print(f"  graph = cargo tree -p {pkg} --target {triple}"
          + ("" if default else " --no-default-features") + (f" --features {features}" if features else ""))
    print(rule(W))
    print(f"{'crate':<30}{'kind':>6}{'prod lines':>12}{'image bytes':>13}")
    rows_ = []
    tot = {"ws": [0, 0, 0, 0], "ext": [0, 0, 0, 0]}  # lines, bytes, crates, crates-with-bytes
    root = os.path.abspath(".")
    for name, ver, path in tree:
        if path and os.path.abspath(path) in (root, os.path.abspath(os.path.join(root, pkg))):
            continue
        kind = "ws" if path else "ext"
        src = os.path.join(path, "src") if path else None
        if src is None:
            m = next((d for d in glob.glob(os.path.expanduser(f"~/.cargo/registry/src/*/{name}-{ver}/src"))), None)
            src = m
        lines = 0
        if src and os.path.isdir(src):
            files, _ = walk([src], kernel_gate)
            lines = sum(fc.code for fc in files) if files and hasattr(files[0], "code") else 0
            if not lines:
                a = Agg()
                for fc in files:
                    a.add(fc)
                lines = a.code
        b = sz.get(name.replace("-", "_"), 0)
        rows_.append((name, kind, lines, b))
        t = tot[kind]
        t[0] += lines
        t[1] += b
        t[2] += 1
        t[3] += 1 if b else 0
    for name, kind, lines, b in sorted(rows_, key=lambda r: (-r[3], -r[2])):
        print(f"{name:<30}{kind:>6}{lines:>12}{b:>13}" + ("   <- 0 B in image" if not b else ""))
    print(rule(W))
    for label, key in (("workspace crates", "ws"), ("external crates", "ext")):
        l, b, n, nb = tot[key]
        print(f"{label:<22}{n:>4} in graph, {nb:>3} with bytes in image; {l:>7} lines counted, {b:>8} B in image")
    print(f"{'rest of image':<22}core+alloc {sz.get('core', 0) + sz.get('alloc', 0)} B, "
          f"unattributed {sz.get('(unattributed)', 0)} B")
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
    # ---- lexer traps -------------------------------------------------------
    # Everything below is a construct that desynchronises a lexer written the
    # obvious way, leaving it inside a "string" or "comment" for the rest of the
    # file. The failure mode is silent and one-directional: the counter reports
    # *fewer* sites than there are, which reads as progress. Every case here is
    # something the tree actually contains.
    (
        # `'"'` is the classic. Treat `'` as "char literal, skip 3 chars" and you
        # are fine; treat `"` as a string opener wherever you see it and the rest
        # of the file is one string.
        "a quote inside a char literal does not open a string",
        "fn f() {\n    let q = '\"';\n    unsafe { a() };\n}\n",
        1,
        1,
    ),
    (
        # And the reverse trap: a lifetime is an apostrophe with no closing one.
        # A lexer that opens a char literal at `'a` swallows everything up to the
        # next apostrophe, which may be many lines away.
        "a lifetime is not an unterminated char literal",
        "fn f<'a>(x: &'a u8) {\n    unsafe { a(x) };\n}\n",
        1,
        1,
    ),
    (
        "an escaped quote does not end the string early",
        'fn f() {\n    p("say \\"unsafe\\" loudly");\n}\n',
        0,
        0,
    ),
    ("the word in a byte string is not a site", 'fn f() {\n    p(b"unsafe");\n}\n', 0, 0),
    (
        "a hashed raw string is not a site",
        'fn f() {\n    p(r##"unsafe { \"x\" }"##);\n}\n',
        0,
        0,
    ),
    (
        "block comments nest, so the inner close does not reopen code",
        "fn f() {\n    /* /* unsafe */ */\n    a();\n}\n",
        0,
        0,
    ),
    ("the word in an inner doc comment is not a site", "//! see `unsafe { }`\nfn f() {}\n", 0, 0),
    (
        "unsafe as part of a longer identifier is not the keyword",
        "fn f() {\n    let my_unsafe = 1;\n    let unsafely = 2;\n    let r#unsafe = 3;\n}\n",
        0,
        0,
    ),
    # ---- cfg gating --------------------------------------------------------
    (
        "a compound test cfg still charges to test",
        '#[cfg(all(test, feature = "x"))]\nfn f() {\n    unsafe { a() };\n}\n',
        0,
        0,
    ),
    (
        "cfg(not(test)) is production, not test",
        "#[cfg(not(test))]\nfn f() {\n    unsafe { a() };\n}\n",
        1,
        1,
    ),
    (
        # A plain feature gate says nothing about tests; charging it to test
        # would hide real shipped `unsafe` behind any optional feature.
        "a non-test feature gate is production",
        '#[cfg(feature = "smp-shared")]\nfn f() {\n    unsafe { a() };\n}\n',
        1,
        1,
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

    # Every crate in `crates/` must appear in the report. This is the failure
    # this script has actually had, twice: `akuma-gic` and `akuma-psci` "were
    # missing entirely" from the per-crate unsafe table until someone noticed
    # (docs/reference/crate-safety.md). A crate that is silently absent does not
    # read as an error — it reads as a crate with nothing to report, and its
    # `unsafe` simply stops being counted. New crates are added to this tree
    # every few days, so nothing about "it worked last time" carries over.
    if os.path.isdir("crates"):
        on_disk = {
            d for d in os.listdir("crates")
            if os.path.isfile(os.path.join("crates", d, "Cargo.toml"))
        }
        files, _ = walk(["crates"], True)
        reported = {component_of(f.path).split(os.sep, 1)[1] for f in files}
        missing = sorted(on_disk - reported)
        if missing:
            failures += 1
            print(f"  FAIL {len(missing)} crate(s) absent from the report: {missing}")
        else:
            print(f"  ok   every crate under crates/ is reported ({len(on_disk)})")

    # `forbids_unsafe` and `unsafe_sites` must agree, in both directions. The
    # attribute is what the compiler enforces; the site count is what this script
    # measures. They are derived independently — one by regex over the file head,
    # one by the lexer — so a disagreement means exactly one of them is wrong,
    # and the report's headline ratio ("N of M crates forbid") is built on both.
    if os.path.isdir("crates"):
        files, _ = walk(["crates"], True)
        by_crate: dict = {}
        for f in files:
            agg = by_crate.setdefault(component_of(f.path), [False, 0])
            agg[0] = agg[0] or f.forbids_unsafe
            agg[1] += f.unsafe_sites + f.test_unsafe_sites
        contradictions = [c for c, (forbids, sites) in by_crate.items() if forbids and sites]
        if contradictions:
            failures += 1
            print(f"  FAIL crate(s) claim #![forbid(unsafe_code)] yet report sites: "
                  f"{sorted(contradictions)}")
        else:
            n = sum(1 for forbids, _ in by_crate.values() if forbids)
            print(f"  ok   every forbidding crate reports zero unsafe sites ({n})")

    # A file the walker calls a test file must charge nothing to production, and
    # vice versa. The two buckets are filled at different places in the scanner
    # (whole-file classification vs. per-item `#[cfg(test)]` gating), and a file
    # leaking into both is how test `unsafe` gets counted as shipped `unsafe` —
    # the number `src/` is judged on.
    leaked = []
    for root in ("src", "crates"):
        if not os.path.isdir(root):
            continue
        for dirpath, _dirs, names in os.walk(root):
            for nm in names:
                if not nm.endswith(".rs"):
                    continue
                rel = os.path.join(dirpath, nm)
                fc = count_file(rel, rel, spec, True)
                if fc.whole_file_test and (fc.unsafe_sites or fc.code):
                    leaked.append(rel)
    if leaked:
        failures += 1
        print(f"  FAIL {len(leaked)} whole-file test(s) charged to production: {leaked[:3]}")
    else:
        print("  ok   whole-file tests charge nothing to production")

    # Total occurrences of the substring are a hard upper bound on sites: the
    # lexer only ever *removes* matches (comments, strings, attributes, longer
    # identifiers), never invents one. This is the cheap direction-of-error check
    # that would have caught the overlapping-roots doubling above while knowing
    # nothing about roots.
    #
    # Occurrences, NOT lines containing one — that version of this check failed
    # on `crates/akuma-alloc/src/lib.rs`, which opens two `GlobalAlloc` methods
    # as `unsafe fn talc_alloc(..) -> *mut u8 { unsafe {`. Two keywords, one
    # line, and the *check* was wrong rather than the script.
    word = re.compile(r"unsafe")
    exceeded = []
    for root in ("src", "crates"):
        if not os.path.isdir(root):
            continue
        for dirpath, _dirs, names in os.walk(root):
            for nm in names:
                if not nm.endswith(".rs"):
                    continue
                rel = os.path.join(dirpath, nm)
                text = open(rel, encoding="utf-8", errors="replace").read()
                fc = count_file(rel, rel, spec, True)
                if fc.unsafe_sites + fc.test_unsafe_sites > len(word.findall(text)):
                    exceeded.append(rel)
    if exceeded:
        failures += 1
        print(f"  FAIL sites exceed the substring upper bound in {len(exceeded)} file(s): "
              f"{exceeded[:3]}")
    else:
        print("  ok   site count stays under the naive substring upper bound")

    # Arch attribution: the platform buckets are only as honest as the
    # extraction underneath them. Pinned: a real cfg names its arch; a mention
    # in a doc comment or string does not; an arch mentioned only inside a
    # `cfg(test)`-gated attribute still counts (it is in an attribute, and the
    # alternative — silently reclassifying a crate as shared because its test
    # pokes the other arch — is exactly the flattering direction).
    arch_cases = [
        ('#[cfg(target_arch = "x86_64")]\nfn f() {}\n', {"x86_64"}),
        ('#[cfg(all(target_arch = "aarch64", feature = "x"))]\nfn f() {}\n', {"aarch64"}),
        ('#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]\nfn f() {}\n',
         {"x86_64", "aarch64"}),
        ('//! docs say target_arch = "x86_64"\nfn f() {}\n', set()),
        ('fn f() {\n    p("target_arch = \\"x86_64\\"");\n}\n', set()),
    ]
    for src, want in arch_cases:
        fc = count_text(src, "probe.rs", spec, True)
        ok = fc.arches == want
        failures += 0 if ok else 1
        print(f"  {'ok  ' if ok else 'FAIL'} arch extraction {src.splitlines()[0][:50]!r}")
        if not ok:
            print(f"       want {want}, got {fc.arches}")

    # ...and the failure this whole platform section indirectly flushed out:
    # `unsafe` in a Markdown README must not count as a site (it rolled up into
    # the crate's tally and tripped the forbid-contradiction check below).
    md = count_text("# unsafe\n\nthe `unsafe` keyword\n", "r.md", EXT_LANGS[".md"], True)
    ok = md.unsafe_sites == 0
    failures += 0 if ok else 1
    print(f"  {'ok  ' if ok else 'FAIL'} prose 'unsafe' in Markdown is not a site")
    if not ok:
        print(f"       got {md.unsafe_sites} sites")

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

    by_platform: dict = {}
    for name, comps in platform_buckets(files).items():
        agg = Agg()
        for fc in files:
            if component_of(fc.path) in comps:
                agg.add(fc)
        by_platform[name] = dict(dump(agg), components=sorted(comps))

    print(
        json.dumps(
            {
                "paths": paths,
                "total": dump(total),
                "by_language": {k: dump(v) for k, v in by_lang.items()},
                "by_component": {k: dump(v) for k, v in by_comp.items()},
                "by_platform": by_platform,
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


# `amd64/src`, not `amd64`: the bin's directory also holds `mkdisk.sh`, `Cargo.toml`,
# a README and linker/boot scaffolding, which this counter would report as code.
# The platform split keys on the `amd64/` prefix, so the narrower root changes nothing there.
DEFAULT_PATHS = ["src", "crates", os.path.join("amd64", "src"), os.path.join("taoxichuan", "src")]


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("paths", nargs="*", default=DEFAULT_PATHS, help="files/dirs (default: src crates amd64/src taoxichuan/src)")
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
    ap.add_argument(
        "--all-languages",
        action="store_true",
        help="also count C, assembly, shell, TOML, Markdown, JSON, ... (default: Rust only)",
    )
    ap.add_argument(
        "--external",
        action="store_true",
        help="also count the crates.io/git libraries each kernel links (via `cargo metadata`, offline)",
    )
    ap.add_argument("--elf", metavar="PATH",
                    help="check the linked-crate set against an UNSTRIPPED kernel ELF (symbol bytes per crate)")
    ap.add_argument("--elf-package", default="akuma", help="cargo package the ELF was built from (default akuma)")
    ap.add_argument("--elf-triple", default="aarch64-unknown-none")
    ap.add_argument("--elf-features", default="", help="features the ELF was built with")
    ap.add_argument("--elf-no-default-features", action="store_true")
    ap.add_argument("--rev", help="count this git revision's tree instead of the working tree")
    ap.add_argument("--vs", metavar="REV",
                    help="also count REV and print the delta REV -> (--rev or working tree)")
    args = ap.parse_args()
    global RUST_ONLY
    RUST_ONLY = not args.all_languages
    if args.self_test:
        return self_test()

    paths = args.paths or DEFAULT_PATHS
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
        if args.external:
            print_external_report(kernel_gate, 78)
        if args.elf:
            print_elf_report(args.elf, args.elf_package, args.elf_triple, args.elf_features,
                             not args.elf_no_default_features, kernel_gate, 78)
    return 0


if __name__ == "__main__":
    sys.exit(main())
