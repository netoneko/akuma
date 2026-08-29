# Updating `docs/archive/BUG_FIX_LIST.md` after landing a fix

You just fixed a bug and wrote (or updated) an archive doc about it. Do this
so `docs/archive/BUG_FIX_LIST.md` — the hand-curated running audit of every
fix in the codebase — doesn't silently go stale.

## When to do this

Any time you land a fix that gets its own `docs/archive/*.md` writeup (new
doc, or a `## Bug N` / `### Fix` section added to an existing one) and the
fix is dated/named enough to be its own line item — not a narrative aside, not
an open/unresolved issue, not a duplicate mention of a fix already listed
under a different doc.

Skip this for: pure refactors/removals with no bug attached, in-flight
`proposals/`, or a doc whose content is just table-of-contents narrative
pointing at fixes counted elsewhere.

The file has exactly three places that track counts — a per-category `##`
header, the top-of-file `## Statistics` block, and the `| Subsystem | Fixes |
% | Docs |` breakdown table right under it — and nothing else. There is no
running changelog of "Updated YYYY-MM-DD: ..." paragraphs; a prior version of
this file had one at the top, bunched together instead of organized by
subsystem like the rest of the doc, and it drifted out of sync with the
actual bullets more than once. `git log -p -- docs/archive/BUG_FIX_LIST.md`
is where the history of *how* a fix got added lives — don't recreate that
inside the file itself, and don't note which branch or commit a fix came
from; `git blame`/`git log` already answer that and a hand-copied branch name
just goes stale the moment the branch is deleted.

## Which docs need checking

A fix does **not** have to arrive in a new file, and this is the step that gets
skipped. Three shapes all need an entry, and only the first is obvious:

1. **A new `docs/archive/*.md`.** Easy to spot, usually remembered.
2. **A new `## Bug N` / `### Fix` / `## Root cause N` section in a doc that
   already has a `###` subsection here.** Its bullet count and its category
   header both need bumping. Nothing about the file's appearance changes, so
   nothing prompts you.
3. **A doc currently in `## Files scanned with zero counted fixes` that has since
   gained a fix.** It must be *removed from that section* and given a real `###`
   subsection under its category. This is the easiest one to miss entirely,
   because the doc *is* mentioned in the file — a grep for its name succeeds — it
   is just mentioned in the place that means "this one contributes nothing".

Before writing anything, list the archive docs your work touched — added **and**
modified — and check each against all three shapes:

```bash
git diff --name-only $(git merge-base main HEAD)..HEAD -- docs/archive/ | grep '\.md$'
git status --short -- docs/archive/ | awk '{print $NF}' | grep '\.md$'
```

For each one that is already listed, diff it and look for fix-shaped additions
rather than trusting your memory of what you changed:

```bash
git diff $(git merge-base main HEAD)..HEAD -- docs/archive/<DOC>.md \
  | grep '^+' | grep -iE '^\+#{2,4} |fixed|resolution|root cause'
```

**Beware the cross-reference.** A doc often notes that a bug it describes was
fixed *elsewhere* ("that one is now FIXED — see `OTHER_DOC.md`"). That is not a
fix belonging to this doc and must not get a bullet here, or the same fix is
counted twice under two subsections. Count a fix under the doc that *documents
the fix*, not every doc that mentions it.

## Steps

1. **Find the right category section.** `BUG_FIX_LIST.md` groups entries under
   `## <Subsystem> (N fixes, M docs)` headers — pick the one matching your
   fix's dominant subsystem (Scheduler & Process Management, SMP & Locking,
   Networking, etc. — see the file's own `## ` headings for the full list).
   Subsystem tags are assigned **per file**, not per bullet, even for docs
   that mix concerns.

2. **Add a `### docs/archive/<YOUR_DOC>.md` subsection** under that category,
   with one `- ` bullet per distinct fix in the doc (not one bullet for the
   whole doc, unless it really is a single fix). Match the terse,
   one-sentence-per-bullet style already used by neighboring entries — enough
   to identify the bug and its fix without opening the doc, not a full
   summary or a restatement of the archive doc's prose.

   **Before you write any number down, count the bullets you actually just
   typed** (`grep -c '^- '` over the lines you added, or just recount by eye)
   — don't carry forward a number from memory or from an earlier draft. Every
   drift this file has ever had traces back to a bullet count stated
   somewhere (a category header, a doc's own summary line) that didn't match
   the bullets actually present (e.g. "9 fixes" written next to a 10-bullet
   list, or a doc mentioned as added but no `###` subsection ever created for
   it). The bullets are the ground truth; every count in the file is derived
   from them, never the other way around.

3. **Bump that category's header count.** `(N fixes, M docs)` → add the
   number of bullets you just added (the one you just counted in step 2) to
   `N`, and `+1` to `M` if this is a new file (not an addition to an existing
   `### docs/archive/...md` subsection).

4. **Bump the top-of-file `## Statistics` block** by the same deltas:
   `Total distinct fixes counted` and `Docs contributing at least one fix`.
   `Subsystem categories` only changes if you added a brand-new `##` category
   header, which is rare — check the existing list first. `Docs contributing`
   counts doc-*subsections*, not unique files — if one doc's bullets are split
   across two categories (see the multi-category gotcha below), it adds 1 to
   this total for each category it lands in, same as the breakdown table.

5. **Bump the `| Subsystem | Fixes | % | Docs |` breakdown table**, directly
   under the `## Statistics` block. This is a separate place from the `##`
   category header in step 3 and the `## Statistics` numbers in step 4 — all
   three must move together or they silently disagree (this has happened
   before: a category header got bumped and the table didn't, and the two sat
   inconsistent for several updates before anyone noticed). Bump your
   category's row by the same delta as step 3, then recompute *every* row's
   `%` column as `fixes / new_total * 100` to one decimal (the percentages are
   relative to the grand total, so any change to any row shifts all of them),
   and update the `**Total**` row to the new grand total / `100.0%` / new doc
   total.

## Gotchas

- **One doc can span multiple categories.** If a single archive doc fixes both
  a scheduler bug and a networking bug (like the `PROCESS_PER_SESSION.md`
  precedent, split across SSH and Networking), split its bullets across two
  `### docs/archive/<doc>.md` subsections under their respective `##`
  categories, and bump both categories' headers, the table, and the
  Statistics `Docs contributing` count accordingly.
- **A removal/cleanup doc isn't automatically zero fixes.** A doc primarily
  about deleting dead code (a `TRIM_FAT_*.md`, say) can still surface and fix
  real bugs along the way — count those bullets normally under whichever
  category they belong to, even though the doc's main subject isn't "a bug."
- **The two counting scripts** (`scripts/count_individual_fixes.py`,
  `scripts/count_archive_bugfixes.py`) scan `docs/archive/` and
  `userspace/*/docs/` with their own heuristics and will **not** match this
  file's totals — that's expected, not a bug in either. They're a rough
  external signal ("did my new doc register as fix-shaped at all?" — check
  with `count_individual_fixes.py --verbose | grep <your-doc-name>`), not
  something to copy numbers from. The authoritative recount is always
  internal to `BUG_FIX_LIST.md` itself (see Verify below); it's the one that
  actually catches header/bullet drift, since the scripts don't parse this
  file at all.

## Verify

Recompute directly from the file — don't trust the running numbers, recount
them. `awk`'s portable across the team's shells but three-arg `match()` isn't
(gawk-only), so use `python3` instead — every category's stated `(N fixes, M
docs)` next to what's actually in its section:

```bash
python3 - <<'EOF'
import re
text = open("docs/archive/BUG_FIX_LIST.md").read()
body = text[text.index("---\n", text.index("## Statistics")):text.index("## Files scanned with zero counted fixes")]
for m in re.finditer(r'^## (.+?) \((\d+) fixes, (\d+) docs?\)\n(.*?)(?=\n## |\Z)', body, re.S | re.M):
    name, sf, sd, section = m.group(1), int(m.group(2)), int(m.group(3)), m.group(4)
    docs = len(re.findall(r'^### ', section, re.M))
    fixes = len(re.findall(r'^- ', section, re.M))
    if "GOLANG_MISSING_SYSCALLS.md" in section:
        fixes += 44  # only doc in the file that states its count in prose, no bullets
    status = "OK" if (fixes == sf and docs == sd) else "MISMATCH"
    print(f"{name:45s} stated={sf}/{sd} actual={fixes}/{docs} {status}")
EOF
```

- Every category must print `OK`. A `MISMATCH` means either the header wasn't
  bumped to match the bullets you added, or (rarer) a doc was mentioned as
  added somewhere but its `### docs/archive/<doc>.md` subsection was never
  actually created — grep the file for the doc's filename to check which.
- Sum all category header `N`/`M` values and confirm they equal the top
  `## Statistics` block's `Total distinct fixes counted` / `Docs contributing
  at least one fix`, and the breakdown table's `**Total**` row.
- **Coverage of what you touched.** The recount above proves the file is
  *self*-consistent; it cannot see a doc nobody added. This catches that. Note it
  is scoped to **your change**, not the whole archive: the zero-fixes section is a
  running narrative of scan passes, not an exhaustive index of all ~380 docs, so a
  whole-archive sweep produces ~160 false positives and teaches you to ignore it.

  ```bash
  BASE=$(git merge-base main HEAD)
  { git diff --name-only "$BASE"..HEAD -- docs/archive/ ;
    git status --short -- docs/archive/ | awk '{print $NF}' ; } \
    | grep '\.md$' | sort -u | while read -r f; do
      b=$(basename "$f"); [ "$b" = "BUG_FIX_LIST.md" ] && continue
      grep -q "^### docs/archive/$b\$" docs/archive/BUG_FIX_LIST.md && s=listed \
        || { grep -q "${b%.md}" docs/archive/BUG_FIX_LIST.md && s=mentioned || s=UNACCOUNTED; }
      printf '%-52s %s\n' "$b" "$s"
    done
  ```

  - `UNACCOUNTED` — nobody has judged it. Give it a `###` subsection or name it in
    the zero-fixes section. Never leave it silent.
  - `mentioned` — named in the zero-fixes narrative, or referenced from another
    doc's bullet. Re-read it: **a doc parked as zero-fixes can since have gained
    one**, and then it must be moved out into a real subsection.
  - `listed` — has a subsection already, so shape 1 is satisfied. It does **not**
    tell you whether new bullets are owed; only the per-doc diff above does.

  A sweep on 2026-08-30 found five docs `UNACCOUNTED` across a single branch, plus
  a `listed` doc that had silently gained a whole new `## Root cause` section.

- `git diff docs/archive/BUG_FIX_LIST.md` should show exactly: the new `###`
  subsection (or bullets added to an existing one), its category header's
  count bumped, the `## Statistics` numbers bumped, and the breakdown table's
  row + `**Total**` + every `%` column bumped. Nothing else moves — no
  changelog paragraph, no branch name, anywhere in the file.

## Background

- `scripts/count_individual_fixes.py`, `scripts/count_archive_bugfixes.py` —
  external signal scripts, self-documented with their exact heuristics in
  their module docstrings; they scan the archive docs themselves, not this
  file, and are not the source of truth for anything in it.
- `docs/archive/BUG_FIX_LIST.md` — the file itself states its counting rule
  at the top; read that before adding an entry if anything here is unclear.
