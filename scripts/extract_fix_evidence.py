#!/usr/bin/env python3
"""Extract resolution-marker evidence from every doc, regardless of filename.

For each .md file under docs/archive and userspace/*/docs, find every line
that looks like a fix/bugfix resolution signal (checkmark+fixed/resolved,
bold FIXED/RESOLVED, "**Status:**" lines, "## Bug N" headers, "Root Cause"
headers) and print it together with its nearest preceding heading, so a
human (or an LLM) can classify each hit as a real distinct bugfix without
reading the entire prose of every file.

This intentionally has NO filename-based filtering - every file is scanned.
"""
import re
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
ARCHIVE_DIR = REPO_ROOT / "docs" / "archive"

HEADING_RE = re.compile(r"^(#{1,4})\s+(.*)$")
EVIDENCE_RE = re.compile(
    r"✅|\bFIXED\b|\bRESOLVED\b|\*\*Status\b|^#{1,4}\s*Bug\s*\d+\b|^#{1,4}.*Root Cause",
    re.IGNORECASE,
)


def nearest_headings(lines, idx):
    """Return the current H1/H2 heading stack above line idx."""
    stack = {}
    for i in range(idx, -1, -1):
        m = HEADING_RE.match(lines[i])
        if m:
            level = len(m.group(1))
            if level not in stack:
                stack[level] = m.group(2).strip()
            if level == 1:
                break
    return " > ".join(stack[k] for k in sorted(stack))


def extract(f):
    lines = f.read_text(errors="replace").splitlines()
    hits = []
    for i, line in enumerate(lines):
        stripped = line.strip()
        if not stripped:
            continue
        if EVIDENCE_RE.search(line):
            heading = nearest_headings(lines, i)
            hits.append((i + 1, heading, stripped))
    return hits


def main():
    files = sorted(ARCHIVE_DIR.glob("*.md")) + sorted((REPO_ROOT / "userspace").glob("*/docs/*.md"))
    for f in files:
        hits = extract(f)
        if not hits:
            continue
        rel = f.relative_to(REPO_ROOT)
        print(f"\n===== {rel} =====")
        last_heading = None
        for lineno, heading, text in hits:
            if heading != last_heading:
                print(f"  [{heading}]")
                last_heading = heading
            text = text[:160]
            print(f"    L{lineno}: {text}")


if __name__ == "__main__":
    main()
