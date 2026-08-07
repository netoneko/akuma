#!/usr/bin/env python3
"""cloc-style line counter that knows the difference between kernel code and tests.

Unlike `cloc`, this walks Rust source with a real lexer (string literals, raw
strings, char-vs-lifetime, nested block comments) and then attributes every line
to one of two buckets:

  production  — code that ships in the kernel/crates
  test        — code that only exists to test it

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

Usage:
    scripts/cloc_akuma.py                      # defaults to src/ crates/
    scripts/cloc_akuma.py src crates --by-file
    scripts/cloc_akuma.py --json
"""

from __future__ import annotations

import argparse
import json
import os
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

    def close_span(sp):
        for ln in range(sp["start"], line + 1):
            test.add(ln)

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
            i = j
            continue

        # -- braces / item termination ---------------------------------------
        if c == "{":
            if spans and awaiting_item and not spans[-1]["brace"] and spans[-1]["depth"] == depth:
                spans[-1]["brace"] = True
                awaiting_item = False
            depth += 1
            i += 1
            continue

        if c == "}":
            depth = max(0, depth - 1)
            while spans and spans[-1]["brace"] and spans[-1]["depth"] == depth:
                close_span(spans.pop())
            awaiting_item = any(not sp["brace"] for sp in spans)
            i += 1
            continue

        if c == ";":
            if spans and awaiting_item and not spans[-1]["brace"] and spans[-1]["depth"] == depth:
                close_span(spans.pop())
                awaiting_item = any(not sp["brace"] for sp in spans)
            i += 1
            continue

        i += 1

    for sp in spans:  # unterminated (truncated file) — count what we saw
        for ln in range(sp["start"], line + 1):
            test.add(ln)

    return code, comment, test, all_file_test


def count_file(path: str, relpath: str, spec: LangSpec, kernel_gate: bool) -> FileCount:
    with open(path, "r", encoding="utf-8", errors="replace") as fh:
        text = fh.read()

    nlines = text.count("\n") + (1 if text and not text.endswith("\n") else 0)
    code, comment, test, inner_test = scan(text, spec, kernel_gate)

    fc = FileCount(path=relpath, lang=spec.name)
    fc.whole_file_test = is_test_path(relpath) or inner_test

    for ln in range(1, nlines + 1):
        is_test = fc.whole_file_test or ln in test
        if ln in code:
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
    files: list = []
    ignored = 0
    for root_arg in paths:
        root_arg = root_arg.rstrip(os.sep)
        if os.path.isfile(root_arg):
            spec = EXT_LANGS.get(os.path.splitext(root_arg)[1])
            if spec:
                files.append(count_file(root_arg, root_arg, spec, kernel_gate))
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
                files.append(count_file(full, full, spec, kernel_gate))
    return files, ignored


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

    def add(self, fc: FileCount):
        self.files += 1
        self.test_files += 1 if fc.whole_file_test else 0
        for f in ("blank", "comment", "code", "test_blank", "test_comment", "test_code"):
            setattr(self, f, getattr(self, f) + getattr(fc, f))

    @property
    def total_code(self) -> int:
        return self.code + self.test_code

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
        "--no-kernel-test-gate",
        action="store_true",
        help='count #[cfg(not(any(feature = "no-tests", ...)))] items as production code',
    )
    args = ap.parse_args()

    paths = args.paths or ["src", "crates"]
    missing = [p for p in paths if not os.path.exists(p)]
    if missing:
        print(f"error: no such path: {', '.join(missing)}", file=sys.stderr)
        return 1

    files, ignored = walk(paths, kernel_gate=not args.no_kernel_test_gate)
    if not files:
        print("error: no recognised source files found", file=sys.stderr)
        return 1

    if args.json:
        emit_json(files, paths)
    else:
        print_report(files, paths, ignored, args.by_file, args.top)
    return 0


if __name__ == "__main__":
    sys.exit(main())
