#!/usr/bin/env python3
"""Reject `?` on a positive-errno helper inside the syscall layer.

Two families of `Result<_, u64>` meet in `src/syscall/` and their `Err` values
have **opposite signs**:

  * this layer's own helpers (`copy_from_user_str`, `copy_from_user_byte`) carry
    the *negated* errno, because they exist to be returned from a syscall arm;
  * `akuma_exec::mmu::user_access`'s helpers (`copy_from_user`, `read_user_into`,
    `write_user_val`, and their `_with` variants) carry the *positive* errno,
    deliberately — that crate is used off the syscall path too, and its own
    comment says so: "`x0 = -errno` happens at the syscall boundary, not here".

A syscall arm returning `syscall::SysResult` can write `read_user_into(&mut v,
p)?` and it compiles. It is wrong: the arm returns `Err(14)`, and userspace
decodes a positive 14 as a syscall that *succeeded* and returned 14 — a silent
wrong answer, not a fault. The correct spelling is the one every existing call
site uses:

    if read_user_into(&mut v, p).is_err() {
        return Err(EFAULT);      // this module's EFAULT: negated
    }

This check existed from the day `SysResult` did. Before that there was no `?` in
these functions for the mistake to hide in; introducing one is what made the
trap reachable, so the guard ships with it rather than after the first bug.

Exit 0 when clean, 1 with the offending lines otherwise.
"""

import pathlib
import re
import sys

# The `user_access` helpers whose `Err` is a POSITIVE errno.
POSITIVE_ERRNO_HELPERS = (
    "copy_from_user",
    "copy_from_user_with",
    "copy_to_user",
    "copy_to_user_with",
    "read_user_into",
    "read_user_into_with",
    "write_user_val",
    "write_user_val_with",
    "as_user_bytes",
    "as_user_bytes_mut",
    "prefault_user_range",
)

# `name(...)?` — allowing nested parens one level deep, which covers the
# multi-line calls in net.rs and proc.rs once the file is joined.
CALL_THEN_QUESTION = re.compile(
    r"\b(" + "|".join(POSITIVE_ERRNO_HELPERS) + r")\s*\((?:[^()]|\([^()]*\))*\)\s*\?"
)

ROOTS = ("src/syscall",)

# `//` to end of line, including `///` doc comments. Blanked rather than removed
# so byte offsets — and therefore reported line numbers — stay correct. Without
# this the check flags its own explanation: `flat`'s doc comment in
# `src/syscall/mod.rs` spells the wrong form out on purpose.
LINE_COMMENT = re.compile(r"//[^\n]*")


def strip_comments(text: str) -> str:
    return LINE_COMMENT.sub(lambda m: " " * len(m.group(0)), text)


def main() -> int:
    repo = pathlib.Path(__file__).resolve().parent.parent
    violations = []

    for root in ROOTS:
        for path in sorted((repo / root).rglob("*.rs")):
            text = strip_comments(path.read_text())
            # Work on the whole file so a call split across lines is still seen,
            # then map the match offset back to a line number for the report.
            for m in CALL_THEN_QUESTION.finditer(text):
                line_no = text.count("\n", 0, m.start()) + 1
                rel = path.relative_to(repo)
                snippet = " ".join(m.group(0).split())
                violations.append(f"{rel}:{line_no}: {snippet}")

    if violations:
        print("ERROR: `?` applied to a positive-errno user_access helper:")
        for v in violations:
            print(f"  {v}")
        print()
        print("These return Err(<positive errno>), but a syscall arm must return")
        print("Err(<negated errno>) — userspace decodes a positive value as SUCCESS.")
        print("Use `.is_err()` and return this module's EFAULT explicitly instead.")
        print("See `flat` in src/syscall/mod.rs for the full explanation.")
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
