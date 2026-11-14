# Copy and `mut` budgets (2026-11-01)

**Stability: B**: the gate and the counter are new; the counts are measured, the fixes the audit lists are not done.

## Why

Asked for after a read-only audit of unneeded `.clone()` in the two amd64 kernels and the 105 workspace crates they
link ([`CLONE_AUDIT_2026_11_01.md`](CLONE_AUDIT_2026_11_01.md)): give copies, and `mut`, the same gate `unsafe` has
(`scripts/check_unsafe_budget.py`), so they stop growing unreviewed.

## What the audit found that shaped the counter

- Clippy's clone lints (`redundant_clone`, `implicit_clone`, `unnecessary_to_owned`, ...) found **nothing** real.
  Every finding came from reading the code.
- The costliest copies do not contain the text `.clone()`. `Process::get_fd` (`akuma-exec`, 64 call sites, 60 in
  glue) and amd64's `table_get` copy a whole `FileDescriptor` with `.cloned()`: a path `String` plus an inode-pin round
  trip, and for a directory fd its whole cached `getdents` listing, with IRQs masked under the fd-table lock. amd64
  does it on every `write(2)` (`console_end`) and four times per `pread64`. The borrowing forms already exist
  (`with_fd`, `table_with`).
- So the copy measure counts `.clone()`, `.cloned()`, `.to_vec()`, `.to_owned()`, `.to_string()` **and** every call
  to `get_fd(`/`table_get(`; a `.clone()`-only budget would not move when the fd copies are fixed.

## The counter

`scripts/cloc_akuma.py` lexes both in its identifier branch, beside `unsafe`, so strings, comments, `#[cfg(test)]`
items and test files never count (`COPY_METHODS`, `FD_COPIERS`; JSON fields `copy_sites`/`mut_sites` and their
`test_` twins). A copy method counts when it is called after `.` or `::` (`Clone::clone(&x)` counts, a field named
`clone` does not); an fd copier counts when called, not at its `fn` definition. `--self-test` pins seven cases a grep
gets wrong.

## The gate

`scripts/check_budgets.py [copy|mut] [--update]`; budgets `docs/reference/copy-budget.json` and
`docs/reference/mut-budget.json`, per component (`src`, `crates/akuma-kernel-amd64/src`, `taoxichuan/src`, `crates/<name>`). Same rules
as the `unsafe` gate: no entry is a budget of 0, growth fails until `--update` puts it in the diff, a decrease is a
note that `--update` ratchets in.

First budgets (working tree after B7, 2026-11-01):

| measure | total | components | largest |
|---|---:|---:|---|
| copies | 275 | 22 | glue 90, `akuma-exec` 60, amd64 25, `akuma-vfs-glue` 16, `akuma-kernel-glue` 12, `akuma-locks-rw` 12 |
| `mut` | 5,547 | 103 | amd64 1,372, glue 466, `akuma-ext2` 282, `akuma-svm-amd64` 278, `akuma-exec` 211 |

The audit's own `.clone()`-only count over its 105 components was 125 (a plain grep says 235: 106 of those are test
code and 4 are comments). The copy budget is larger because it is wider (the other four methods and the fd copiers)
and covers the whole tree, the AArch64 kernel included.

## Open

- The fixes. The audit ranks them; the first four are the fd copies on read-only paths (`with_fd`/`table_with`),
  `execve`'s three or four argv copies, `translate_output`'s copy of every terminal write under the terminal lock
  (`Cow`), and `getdents`' double copy of each name.
- `clone_on_ref_ptr` (30 hits) is not enforced, so an `Arc` refcount bump still counts as a copy here. Enforcing it
  would let the measure drop those.
- `mut` has no audit behind it yet: the budget only stops growth.

## Background

- [`CLONE_AUDIT_2026_11_01.md`](CLONE_AUDIT_2026_11_01.md): the audit, findings ranked, and the copies that look
  removable but are not.
- [`TAOXICHUAN_CLEANUP_PLAN.md`](TAOXICHUAN_CLEANUP_PLAN.md): where this sits in the cleanup.
