#!/usr/bin/env python3
"""Estimate how many bugfixes are documented in docs/archive (and, optionally,
the per-package userspace/*/docs/*.md docs).

These are 200+ freeform investigation write-ups, not a structured changelog,
so this is a heuristic count, not ground truth. Each file is classified into
one of three shapes and counted accordingly:

  status-log    File has 2+ "**Status:** ..." entries (e.g. KNOWN_ISSUES.md,
                GOLANG_MISSING_SYSCALLS.md). Each entry whose status starts
                with Fixed/Resolved counts as one bugfix.
  bug-sections  File has "## Bug N: ..." headers (e.g. CONTEXT_SWITCH_BUGS.md).
                Each section counts as a bugfix if it contains a resolution
                signal (fixed/resolved/checkmark) without a later override
                like "under investigation" / "Status: Open".
  whole-file    Neither of the above - most files. Counts as one bugfix if
                either:
                  (a) it has a *strong* resolution marker: a checkmark next
                      to fixed/resolved (✅ **FIXED**), a bold
                      **FIXED**/**RESOLVED**, or "(RESOLVED)" in a heading, or
                  (b) the filename itself signals a bugfix doc (a whole
                      underscore/dot-separated token is "fix", "bug",
                      "crash", "panic", "deadlock", "corrupt(ion)", "hang",
                      "leak", "regression", or "freeze" - matched as whole
                      tokens so "DEBUG" doesn't false-positive on "bug"),
                      the filename doesn't also contain "plan"/"proposal"
                      (a plan for a fix isn't the fix itself), and the body
                      doesn't contain an override phrase like "Status: Open"
                      or "unresolved" suggesting it's still open.
                Bare occurrences of the words "fixed"/"resolved" in body
                prose do NOT count on their own - those words show up
                constantly in non-bug contexts (DNS *resolved*, "prefixed",
                a "fixed virtual address", etc.).

This is a precision-over-recall heuristic: it will still miss real fixes
described only in plain prose with a non-indicative filename, and it may
undercount docs that bundle several distinct fixes into one whole-file doc
(each such doc caps at 1 unless it hits the status-log or bug-sections shape).

Run with --verbose to see the per-file classification.
"""
import argparse
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
ARCHIVE_DIR = REPO_ROOT / "docs" / "archive"

STATUS_RE = re.compile(r"\*\*Status:?\*\*:?\s*✅?\s*\**\s*([A-Za-z][A-Za-z ]*)")
BUG_HEADER_RE = re.compile(r"^#{1,4}\s*Bug\s*\d+\b.*$", re.MULTILINE)
RESOLVED_WORD_RE = re.compile(r"\bfixed\b|\bresolved\b|✅", re.IGNORECASE)
UNRESOLVED_OVERRIDE_RE = re.compile(
    r"under investigation|status:\s*open|\bnot\s+(?:yet\s+)?fixed\b|\bunresolved\b",
    re.IGNORECASE,
)
STRONG_RESOLUTION_RE = re.compile(
    r"✅[^\n]{0,20}?\b(fixed|resolved)\b"
    r"|\b(fixed|resolved)\b[^\n]{0,20}?✅"
    r"|\*\*(fixed|resolved)\*\*"
    r"|^#.*\(resolved\)",
    re.IGNORECASE | re.MULTILINE,
)
FILENAME_SIGNAL_TOKENS = {
    "fix", "fixes", "fixed",
    "bug", "bugs",
    "crash", "crashes",
    "panic", "panics",
    "deadlock", "deadlocks",
    "corrupt", "corruption",
    "hang", "hangs",
    "leak", "leaks",
    "regression", "regressions",
    "freeze", "freezes",
}
FILENAME_VETO_TOKENS = {"plan", "proposal", "proposals", "idea", "ideas"}


def filename_signals_bugfix(filename):
    tokens = set(re.split(r"[^a-zA-Z]+", filename.lower()))
    if tokens & FILENAME_VETO_TOKENS:
        return False
    return bool(tokens & FILENAME_SIGNAL_TOKENS)


def classify_status_log(text):
    entries = STATUS_RE.findall(text)
    fixed = sum(1 for e in entries if re.match(r"fixed|resolved", e.strip(), re.IGNORECASE))
    return fixed, len(entries)


def classify_bug_sections(text):
    headers = list(BUG_HEADER_RE.finditer(text))
    bounds = [h.start() for h in headers] + [len(text)]
    fixed = 0
    for i in range(len(headers)):
        section = text[bounds[i] : bounds[i + 1]]
        if RESOLVED_WORD_RE.search(section) and not UNRESOLVED_OVERRIDE_RE.search(section):
            fixed += 1
    return fixed, len(headers)


def classify_whole_file(text, filename):
    status_entries = STATUS_RE.findall(text)
    if len(status_entries) == 1:
        fixed = 1 if re.match(r"fixed|resolved", status_entries[0].strip(), re.IGNORECASE) else 0
        return fixed, 1
    if STRONG_RESOLUTION_RE.search(text):
        return 1, 1
    if filename_signals_bugfix(filename) and not UNRESOLVED_OVERRIDE_RE.search(text):
        return 1, 1
    return 0, 1


def classify(text, filename):
    if len(STATUS_RE.findall(text)) >= 2:
        fixed, total = classify_status_log(text)
        return "status-log", fixed, total
    if BUG_HEADER_RE.search(text):
        fixed, total = classify_bug_sections(text)
        return "bug-sections", fixed, total
    fixed, total = classify_whole_file(text, filename)
    return "whole-file", fixed, total


def scan(files, verbose, label):
    grand_fixed = 0
    category_counts = {"status-log": 0, "bug-sections": 0, "whole-file": 0}
    rows = []

    for f in files:
        text = f.read_text(errors="replace")
        category, fixed, total = classify(text, f.name)
        category_counts[category] += 1
        grand_fixed += fixed
        if fixed > 0:
            rows.append((str(f.relative_to(REPO_ROOT)), category, fixed, total))

    if verbose and rows:
        print(f"\n--- {label}: files contributing a bugfix ---")
        print(f"{'file':65} {'category':14} {'fixed/total'}")
        print("-" * 95)
        for name, category, fixed, total in rows:
            print(f"{name:65} {category:14} {fixed}/{total}")

    print(f"\n{label}: scanned {len(files)} files")
    print(f"  shape breakdown: " + ", ".join(f"{k}={v}" for k, v in category_counts.items()))
    print(f"  files contributing a bugfix: {len(rows)}")
    print(f"  bugfixes counted: {grand_fixed}")
    return grand_fixed


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("-v", "--verbose", action="store_true", help="print per-file classification")
    parser.add_argument(
        "--include-userspace",
        action="store_true",
        help="also scan userspace/*/docs/*.md (per-package docs; excludes vendored nested source trees)",
    )
    args = parser.parse_args()

    if not ARCHIVE_DIR.is_dir():
        print(f"error: {ARCHIVE_DIR} is not a directory", file=sys.stderr)
        sys.exit(1)

    archive_files = sorted(ARCHIVE_DIR.glob("*.md"))
    total = scan(archive_files, args.verbose, "docs/archive")

    if args.include_userspace:
        userspace_files = sorted((REPO_ROOT / "userspace").glob("*/docs/*.md"))
        total += scan(userspace_files, args.verbose, "userspace/*/docs")

    print(f"\nGrand total estimated bugfixes: {total}")


if __name__ == "__main__":
    main()
