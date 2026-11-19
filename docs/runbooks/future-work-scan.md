# Scanning the docs for future work

A tree this size accumulates promises: "still open", "deferred", "follow-up", a `TODO`. Most are in `docs/archive`, written
when an incident was fixed and one part was not. They are only useful if someone can find them and say whether they are
still true. This is the workflow; the collected result lives in [`proposals/FUTURE_WORK.md`](../../proposals/FUTURE_WORK.md).

## 1. Scan

```
scripts/future_work_scan.py           # rewrite the generated inventory
scripts/future_work_scan.py --check   # exit 1 if the checked-in inventory is stale
```

It greps every `.md` file and the `TODO`/`FIXME` comments in `.rs` files for the words authors use when they leave work
undone, and records `file:line`, the sentence, and whether the same line also says fixed/done/resolved (`maybe resolved`).
Add a phrase by editing `MARK` in the script. **What it cannot do:** find work described in other words, tell "deferred
drop" from "deferred work", or know whether a claim is still true. A second pass that *reads* a document (rather than
matching words) finds what the grep misses; it is slow, so do it for the documents a change touches, not the whole tree.

## 2. Triage a document

For each hit, in order:

1. **Is it still true?** Search the code for the thing the sentence says is missing (`git log -S'<symbol>'`, `grep`). The
   archive documents are dated; the code has moved since.
2. **Is it fixed?** Find the commit or the test. Record it in the entry you close.
3. **Is it still open?** Reproduce it if it is a bug, or confirm the missing piece is missing if it is a feature.
4. **Is it a decision, not a gap?** "Out of scope" and "not worth doing" are decisions; leave them recorded as such.

## 3. Close or promote

- Fixed or not reproducible: edit the source document's sentence (strike it through with the commit, or delete the claim),
  then re-run the scan; the hit disappears from the inventory by itself.
- Still open and worth doing: add a row to the **Curated** section of `FUTURE_WORK.md` with *what closes it*, so the next
  person has a stopping condition, and leave the source sentence alone or link to the row.

## 4. When to run it

- **On every merge into `main`, same as the bug-fix list** (`update-bug-fix-list.md`): before the merge, walk the curated
  section of `proposals/FUTURE_WORK.md` against the branch, delete each entry the branch closed (naming the commit or
  test in the commit message, not in the file), add the entries the branch opened, and regenerate the inventory
  (`scripts/future_work_scan.py`, then `--check` exits 0). A fixed item left in the list is worse than a missing one.

- After a large change, to see which documents it made stale (the scan is cheap).
- Before planning a sprint, to seed the list.
- Not in the pre-commit hook: a stale inventory is information, not an error, and failing every doc edit would train people
  to regenerate without reading.

## Known limits

The inventory is a map, not an audit. 371 of its 1,749 entries also say fixed/done, which is a hint about where to start
verifying and not a verdict. None has been verified.
