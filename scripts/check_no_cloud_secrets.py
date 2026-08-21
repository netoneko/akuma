#!/usr/bin/env python3
"""Refuse to commit AWS account ids or credentials in Markdown.

This repo is PUBLIC and its docs are written from real sessions, so a pasted
console line is the realistic leak path -- not code. The motivating case: an
archive doc about the AWS Firecracker host nearly shipped an account's IAM
posture (docs/archive/AKUMA_FIRECRACKER_TERRAFORM.md, scrubbed 2026-08-21).

Scans STAGED content by default, so a partially-staged file is judged on what is
actually being committed rather than on the working tree.

    scripts/check_no_cloud_secrets.py          # staged *.md -- what the hook runs
    scripts/check_no_cloud_secrets.py --all    # every tracked *.md, for an audit

Allowlist: scripts/cloud_secret_scan_allow.txt, one literal per line. Only the
12-digit account-id heuristic consults it -- a leaked key is never "expected".

Python rather than shell on purpose: the first cut was nested `for` loops in sh,
and `for spec in $PATTERNS` word-split on the spaces inside the descriptions,
silently running garbage regexes that matched every line of every file. A check
that cries wolf gets bypassed, which is worse than no check.
"""

import re
import subprocess
import sys
from pathlib import Path

ALLOWLIST = Path("scripts/cloud_secret_scan_allow.txt")

# (name, description, regex, may_be_allowlisted)
#
# Deliberately narrow. A 40-char base64 run -- the shape of a secret access key --
# is not matched: this tree is full of hashes and base64 blobs, and the false
# positives would train people to pass --no-verify. The assignment form is what
# a real paste looks like anyway.
PATTERNS = [
    (
        "account-id-in-arn",
        "AWS ARN carrying an account id",
        re.compile(r"arn:aws[a-z-]*:[a-z0-9-]*:[a-z0-9-]*:\d{12}:"),
        False,
    ),
    (
        "account-id-in-ecr-url",
        "ECR registry URL carrying an account id",
        re.compile(r"\d{12}\.dkr\.ecr\.[a-z0-9-]+\.amazonaws\.com"),
        False,
    ),
    (
        "access-key-id",
        "AWS access key id",
        re.compile(r"\b(?:A3T[A-Z0-9]|AKIA|ASIA|ABIA|ACCA)[A-Z0-9]{16}\b"),
        False,
    ),
    (
        "credential-assignment",
        "AWS secret or session token with a value",
        re.compile(
            r"(?:aws_secret_access_key|aws_session_token"
            r"|AWS_SECRET_ACCESS_KEY|AWS_SESSION_TOKEN)"
            r"\s*[=:]\s*[A-Za-z0-9+/]{8,}"
        ),
        False,
    ),
    (
        "bare-account-id",
        "12-digit run, the shape of an AWS account id",
        re.compile(r"(?<![0-9A-Za-z_])\d{12}(?![0-9A-Za-z_])"),
        True,
    ),
]


def git(*args: str) -> str:
    return subprocess.run(
        ["git", *args], capture_output=True, text=True, check=False
    ).stdout


def load_allowlist() -> set[str]:
    if not ALLOWLIST.exists():
        return set()
    out = set()
    for line in ALLOWLIST.read_text().splitlines():
        line = line.split("#", 1)[0].strip()
        if line:
            out.add(line)
    return out


def targets(scan_all: bool) -> list[str]:
    if scan_all:
        return [f for f in git("ls-files", "*.md").splitlines() if f]
    # ACM: added / copied / modified. A deletion cannot leak anything.
    return [
        f
        for f in git(
            "diff", "--cached", "--name-only", "--diff-filter=ACM", "--", "*.md"
        ).splitlines()
        if f
    ]


def content(path: str, scan_all: bool) -> str:
    if scan_all:
        try:
            return Path(path).read_text(errors="replace")
        except OSError:
            return ""
    # The staged blob, not the working tree: that is what is being committed.
    return git("show", f":{path}")


def main() -> int:
    scan_all = "--all" in sys.argv[1:]
    allow = load_allowlist()
    findings = []

    for path in targets(scan_all):
        text = content(path, scan_all)
        if not text:
            continue
        for lineno, line in enumerate(text.splitlines(), 1):
            for name, desc, pattern, allowable in PATTERNS:
                for m in pattern.finditer(line):
                    hit = m.group(0)
                    if allowable and hit in allow:
                        continue
                    findings.append((path, lineno, name, desc, hit, line.strip()))

    if not findings:
        return 0

    for path, lineno, name, desc, hit, line in findings:
        print(f"{path}:{lineno}: {desc} [{name}]")
        print(f"    match: {hit}")
        print(f"    line:  {line[:110]}")

    print(
        """
ERROR: possible AWS account id or credential in staged Markdown.

This repo is public. Fix it; do not pass --no-verify.
  account id    remove it, or derive it at run time -- the akuma-terraform repo
                reads it from aws_caller_identity rather than committing it
  ARN / ECR URL replace the account field with a placeholder:
                arn:aws:iam::<account>:role/...  or  <account>.dkr.ecr.<region>...
  access key    rotate it first, then remove it. Assume it is compromised.
  IAM posture   keep it in the private infra repo, not in docs/

If a 12-digit match is genuinely not an account id -- an Akuma box id, a
timestamp, a byte count -- add the literal to scripts/cloud_secret_scan_allow.txt
with a one-line reason. Only that heuristic can be allowlisted; a real key cannot.
""",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
