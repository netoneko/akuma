#!/usr/bin/env python3
"""Count individual bugfixes documented anywhere under docs/archive and
userspace/*/docs, regardless of filename, at "one heading per distinct fix"
granularity - the natural unit these docs already use (## Bug N, ### 1. foo
- FIXED, ## Signal Delivery (RESOLVED), etc).

Method: parse every heading in a file. Skip boilerplate sub-headings that
are part of ONE fix's writeup, not a fix of their own (Root Cause, Fix,
Status, Verification, Summary, ...). Every remaining heading is a fix
candidate; its full subtree (until the next heading of <= its level) is
checked for a resolution signal (FIXED/RESOLVED/checkmark) without a later,
more specific override (not yet fixed / still open / under investigation)
within that subtree, ignoring nested candidate headings' own text (checked
independently, not double-counted into the parent).

Files with no fix-shaped headings at all fall back to whole-file classification
identical to count_archive_bugfixes.py's approach.

Output: a flat list of (file, fix title) plus per-file and grand totals.
"""
import argparse
import re
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
ARCHIVE_DIR = REPO_ROOT / "docs" / "archive"

HEADING_RE = re.compile(r"^(#{1,4})\s+(.*)$")

BOILERPLATE_HEADINGS = {
    "root cause", "root causes", "root cause analysis", "fix", "fixes",
    "the fix", "status", "verification", "summary", "background", "impact",
    "files changed", "files modified", "conclusion", "overview", "tl;dr",
    "next steps", "symptom", "symptoms", "problem", "problem statement",
    "solution", "testing", "test plan", "changes made", "diagnosis",
    "design", "analysis", "context", "motivation", "reproduction",
    "current status", "current state", "notes", "references", "appendix",
    "known limitation", "known limitations", "remaining work",
    "remaining issues", "files touched", "verified", "related",
    "recommendation", "recommendations",
}

ISSUE_NUMBER_RE = re.compile(r"^(bug|issue|problem|regression|cause)\s*\d+\b", re.IGNORECASE)
NUMBERED_RE = re.compile(r"^\d+[\.\)]\s")
FIXED_IN_HEADING_RE = re.compile(r"\bfixed\b|\bresolved\b", re.IGNORECASE)

STRONG_RESOLUTION_RE = re.compile(
    r"✅[^\n]{0,20}?\b(fixed|resolved)\b"
    r"|\b(fixed|resolved)\b[^\n]{0,20}?✅"
    r"|\*\*(fixed|resolved)\*\*"
    r"|\bstatus\b[^\n]{0,10}:?\s*✅?\s*\**\s*(fixed|resolved)",
    re.IGNORECASE,
)
OVERRIDE_RE = re.compile(
    r"not yet fixed|not fixed|still open|under investigation|status:\s*open|"
    r"\bunresolved\b|open\s*[-—]|not been fixed|remains? (open|unfixed)",
    re.IGNORECASE,
)

FILENAME_SIGNAL_TOKENS = {
    "fix", "fixes", "fixed", "bug", "bugs", "crash", "crashes", "panic",
    "panics", "deadlock", "deadlocks", "corrupt", "corruption", "hang",
    "hangs", "leak", "leaks", "regression", "regressions", "freeze", "freezes",
}
FILENAME_VETO_TOKENS = {"plan", "proposal", "proposals", "idea", "ideas"}


def is_boilerplate(text):
    norm = re.sub(r"^[\d\.\)\s]+", "", text)
    norm = re.sub(r"[:\-—(].*$", "", norm).strip().lower()
    return norm in BOILERPLATE_HEADINGS


def is_fix_candidate(text):
    if is_boilerplate(text):
        return False
    stripped = text.strip()
    if ISSUE_NUMBER_RE.match(stripped) or NUMBERED_RE.match(stripped):
        return True
    if FIXED_IN_HEADING_RE.search(stripped):
        return True
    return False


STEP_PARENT_NAMES = {
    "fix", "fixes", "the fix", "resolution", "changes made", "changes",
    "implementation", "verification", "files changed", "files modified",
    "implementation steps", "solution",
}


def parent_heading(headings, idx):
    level = headings[idx][1]
    for j in range(idx - 1, -1, -1):
        if headings[j][1] < level:
            return headings[j]
    return None


def is_numbered_substep_of_fix(headings, idx):
    """Numbered items like '1. Re-poll after DHCP...' under a '## Fix' heading
    are steps of ONE fix, not separate bugs - unlike top-level numbered bugs
    (GOLANG_MISSING_SYSCALLS.md's '1. Signal delivery...' etc, siblings of
    Root Cause/Fix, not children of Fix)."""
    parent = parent_heading(headings, idx)
    if parent is None:
        return False
    norm = re.sub(r"^[\d\.\)\s]+", "", parent[2])
    norm = re.sub(r"[:\-—(].*$", "", norm).strip().lower()
    return norm in STEP_PARENT_NAMES


def parse_headings(lines):
    out = []
    for i, line in enumerate(lines):
        m = HEADING_RE.match(line)
        if m:
            out.append((i, len(m.group(1)), m.group(2).strip()))
    return out


def section_end(headings, idx, total_lines):
    level = headings[idx][1]
    for j in range(idx + 1, len(headings)):
        if headings[j][1] <= level:
            return headings[j][0]
    return total_lines


def immediate_section_end(headings, idx, total_lines):
    """End of this heading's OWN text: up to the very next heading in
    document order (of any level), not the end of its whole subtree. A
    coarse top-level chapter (e.g. AKUMA_SELF_HOSTING.md's numbered
    sections) can span thousands of lines of unrelated sub-narrative;
    searching that whole span for the word 'fixed' would call the entire
    chapter 'fixed' just because some distant grandchild mentions it in
    passing."""
    if idx + 1 < len(headings):
        return headings[idx + 1][0]
    return total_lines


FIX_SUBHEADING_NAMES = {"fix", "fixes", "the fix", "fix applied", "fixes applied", "resolution"}


def has_fix_subheading(headings, idx):
    """A '### Fix' (or similar) DIRECT child heading under this candidate: many
    docs show the applied patch under a bare 'Fix' heading without ever
    writing the word 'fixed' in prose (e.g. FAR_0x5_AND_HEAP_CORRUPTION_FIX.md
    '## Bug 1' -> '### Fix'). Restricted to immediate children (level+1), not
    any descendant - a coarse chapter heading can contain a 'Fix' heading
    somewhere deep inside covering an unrelated micro-topic, which would
    otherwise wrongly mark the whole chapter as one fix."""
    level = headings[idx][1]
    for j in range(idx + 1, len(headings)):
        child_level, child_title = headings[j][1], headings[j][2]
        if child_level <= level:
            break
        if child_level != level + 1:
            continue
        norm = re.sub(r"^[\d\.\)\s]+", "", child_title)
        norm = re.sub(r"[:\-—(].*$", "", norm).strip().lower()
        if norm in FIX_SUBHEADING_NAMES:
            return True
    return False


BARE_FIXED_RE = re.compile(r"\bfixed\b|\bresolved\b", re.IGNORECASE)


def section_is_fixed(own_text, full_section_text, title_already_says_fixed):
    if title_already_says_fixed:
        return OVERRIDE_RE.search(full_section_text) is None
    resolution = STRONG_RESOLUTION_RE.search(own_text) or BARE_FIXED_RE.search(own_text)
    if resolution is None:
        return False
    override = OVERRIDE_RE.search(full_section_text)
    if not override:
        return True
    return resolution.start() < override.start()


def filename_signals_bugfix(filename):
    tokens = set(re.split(r"[^a-zA-Z]+", filename.lower()))
    if tokens & FILENAME_VETO_TOKENS:
        return False
    return bool(tokens & FILENAME_SIGNAL_TOKENS)


def classify_whole_file(text, filename):
    if STRONG_RESOLUTION_RE.search(text) and not OVERRIDE_RE.search(text):
        return True
    if filename_signals_bugfix(filename) and not OVERRIDE_RE.search(text):
        return True
    return False


def extract_fixes(f):
    lines = f.read_text(errors="replace").splitlines()
    text = "\n".join(lines)
    headings = parse_headings(lines)
    candidates = [(i, lvl, title) for i, lvl, title in headings if is_fix_candidate(title)]

    if not candidates:
        if classify_whole_file(text, f.name):
            return [f.name.rsplit(".", 1)[0].replace("_", " ")]
        return []

    doc_level_fixed = classify_whole_file(text, f.name)

    fixes = []
    for idx, (line_idx, level, title) in enumerate(headings):
        if not is_fix_candidate(title):
            continue
        if is_numbered_substep_of_fix(headings, idx):
            continue
        end = section_end(headings, idx, len(lines))
        section_text = "\n".join(lines[line_idx:end])
        own_end = immediate_section_end(headings, idx, len(lines))
        own_text = "\n".join(lines[line_idx:own_end])
        title_says_fixed = bool(FIXED_IN_HEADING_RE.search(title)) and not OVERRIDE_RE.search(title)
        title_says_fixed = title_says_fixed or has_fix_subheading(headings, idx)
        if section_is_fixed(own_text, section_text, title_says_fixed):
            fixes.append(title)
        elif (
            doc_level_fixed
            and ISSUE_NUMBER_RE.match(title.strip())
            and not OVERRIDE_RE.search(section_text)
        ):
            # Some docs describe several numbered "Bug N"/"Issue N" entries,
            # then a single trailing "## Fixes" section covering all of them
            # without repeating the word "fixed" per-bug (e.g.
            # VIRTIO_RECEIVE_FIX.md). The doc-level marker (checkmark/status/
            # filename) vouches for sub-bugs that don't say they're still
            # open. Restricted to explicit Bug/Issue/Problem/Regression N
            # headings (not bare "N. Some Section Title") - bare numbers are
            # usually just a long doc's table-of-contents numbering, not one
            # bug per number (AKUMA_SELF_HOSTING.md, BKL_*_CARVE_OUT.md).
            fixes.append(title)
    return fixes


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("-v", "--verbose", action="store_true")
    parser.add_argument("--out", help="write the full itemized list to this file")
    args = parser.parse_args()

    files = sorted(ARCHIVE_DIR.glob("*.md")) + sorted((REPO_ROOT / "userspace").glob("*/docs/*.md"))

    total = 0
    out_lines = []
    per_file_counts = []
    for f in files:
        fixes = extract_fixes(f)
        if not fixes:
            continue
        rel = f.relative_to(REPO_ROOT)
        total += len(fixes)
        per_file_counts.append((str(rel), len(fixes)))
        out_lines.append(f"\n### {rel} ({len(fixes)})\n")
        for title in fixes:
            out_lines.append(f"- {title}")

    if args.out:
        Path(args.out).write_text("\n".join(out_lines) + "\n")

    if args.verbose:
        for rel, n in per_file_counts:
            print(f"{n:4d}  {rel}")

    print(f"\nFiles scanned: {len(files)}")
    print(f"Files with >=1 counted fix: {len(per_file_counts)}")
    print(f"Total individual fixes counted: {total}")


if __name__ == "__main__":
    main()
