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

## Steps

1. **Run the two counting scripts** to see how the automated view of
   `docs/archive/` + `userspace/*/docs/` has shifted since the doc's numbers
   were last updated:

   ```bash
   python3 scripts/count_individual_fixes.py    # heading-granularity count
   python3 scripts/count_archive_bugfixes.py    # whole-file-granularity count
   ```

   **Their totals will not match `BUG_FIX_LIST.md`'s "Total distinct fixes
   counted" line, and that's expected** — the file's number is a hand-curated
   count following its own stated counting rule (one item per distinct,
   dated/named bug; duplicates and narrative TOCs excluded), not literally the
   live output of either script. Treat the scripts as a *signal* ("did my new
   doc register as fix-shaped at all?"), not the source of truth to copy in.
   Confirm your new doc/section shows up in `count_individual_fixes.py
   --verbose` output before moving on — if it doesn't, its heading/status
   wording probably doesn't match the script's `FIXED`/`RESOLVED` detection
   (see that script's own docstring for the exact heuristics), which is worth
   knowing even though you're not copying its number in.

2. **Find the right category section.** `BUG_FIX_LIST.md` groups entries under
   `## <Subsystem> (N fixes, M docs)` headers — pick the one matching your
   fix's dominant subsystem (Scheduler & Process Management, SMP & Locking,
   Networking, etc. — see the file's own `## ` headings for the full list).
   Subsystem tags are assigned **per file**, not per bullet, even for docs
   that mix concerns.

3. **Add a `### docs/archive/<YOUR_DOC>.md` subsection** under that category,
   with one `- ` bullet per distinct fix in the doc (not one bullet for the
   whole doc, unless it really is a single fix). Match the terse,
   one-sentence-per-bullet style already used by neighboring entries — enough
   to identify the bug and its fix without opening the doc, not a full
   summary.

4. **Bump that category's header count.** `(N fixes, M docs)` → add the
   number of bullets you just added to `N`, and `+1` to `M` if this is a new
   file (not an addition to an existing `### docs/archive/...md` subsection).

5. **Bump the top-of-file `## Statistics` block** by the same deltas:
   `Total distinct fixes counted` and `Docs contributing at least one fix`.
   `Subsystem categories` only changes if you added a brand-new `##` category
   header, which is rare — check the existing list first.

6. **Add a changelog paragraph** right after the `## Statistics` block,
   *above* the most recent existing entry (newest-first order). Follow the
   existing format exactly:

   ```markdown
   Updated YYYY-MM-DD (branch `<branch-name>`, <Nth> entry): +<fixes> fixes /
   +<docs> doc — `docs/archive/<YOUR_DOC>.md` (<which category/categories the
   fixes were counted under>).
   ```

   followed by 1-3 short paragraphs of prose explaining *what* was fixed and
   *why it's worth a standalone note* — the existing entries (e.g. the
   `PROCESS_PER_SESSION.md` one, or the cooperative-scheduling one) are the
   template: enough context that someone skimming just this changelog block
   (never opening the linked doc) understands what changed and why it
   mattered. `<Nth entry>` is the ordinal of this changelog paragraph among
   all the ones already in the file (count the existing "Updated ..." lines
   and add one) — it's a running counter, not tied to any external ID.

## Gotchas

- **The two counting scripts disagree with each other and with the file.**
  `count_individual_fixes.py` parses per-heading with a large boilerplate/
  override heuristic; `count_archive_bugfixes.py` is a coarser whole-file
  classifier. Neither is "the bug fix list" — `BUG_FIX_LIST.md` itself is the
  source of truth, maintained by hand, informed by but not generated from
  either script.
- **Don't recompute the total from scratch.** The file's `Total distinct
  fixes counted` is a running tally across many sessions; always add your
  delta to the existing number rather than trying to re-derive 500+ from the
  scripts (their differing methodology will not reproduce it, and you'll
  introduce a spurious diff against history).
- **One doc can span multiple categories.** If a single archive doc fixes both
  a scheduler bug and a networking bug (like the `PROCESS_PER_SESSION.md`
  precedent), split its bullets across two `### docs/archive/<doc>.md`
  subsections under their respective `##` categories, and say so explicitly
  in the changelog paragraph.
- **A removal/cleanup doc isn't automatically zero fixes.** A doc primarily
  about deleting dead code (a `TRIM_FAT_*.md`, say) can still surface and fix
  real bugs along the way — count those bullets normally under whichever
  category they belong to, even though the doc's main subject isn't "a bug."

## Verify

- `git diff docs/archive/BUG_FIX_LIST.md` shows: the new `###` subsection (or
  addition to an existing one), its category header's count bumped, the
  `## Statistics` numbers bumped by the same delta, and a new changelog
  paragraph inserted above the previous newest one (not replacing it).
- The category header's `(N fixes, M docs)` matches an actual count of `- `
  bullets and `### docs/archive/...md` subsections under it (spot-check with
  `awk`/manual count if the section is long).
- `python3 scripts/count_individual_fixes.py --verbose | grep <your-doc-name>`
  shows your new doc contributing at least one counted fix — if it shows zero,
  your heading/status wording likely needs a `FIXED`/`RESOLVED` signal for the
  automated view to agree with your manual entry (not blocking, but worth
  fixing so the two views don't diverge further).

## Background

- `scripts/count_individual_fixes.py`, `scripts/count_archive_bugfixes.py` —
  the two counting scripts, both self-documented with their exact heuristics
  in their module docstrings.
- `docs/archive/BUG_FIX_LIST.md` — the file itself states its counting rule
  at the top; read that before adding an entry if anything here is unclear.
