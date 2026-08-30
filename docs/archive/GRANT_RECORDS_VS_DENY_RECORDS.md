# A record built to GRANT cannot be used to DENY

**Date: 2026-08-29.** One bug class, four independent instances, three failed
fixes, and a `rustc` that died the same way each time. Written up because the
mistake is not "someone was careless" — every individual writer of the record was
correct for the purpose it was built for.

## The class

> A field that is only ever read to **grant** a permission can be sloppy in ways
> nobody notices. Every sloppiness becomes a **false denial** the moment something
> reads it to refuse.

Sloppy-but-safe-for-granting has a specific signature: **the record is allowed to
under-state.** "I don't know" and "no permission" can share a representation,
because granting on either is a no-op. A record may also over-apply — say more
than it knows — as long as what it says is never *less* than the truth.

Invert the reader and both properties become bugs, and they are silent: nothing
in the granting path changes, no test that exercises granting fails, and the
failure surfaces as a `SIGSEGV` in an unrelated program.

## The instance

`MmapRegion::flags` records an eager mapping's protection. Its one consumer was
the EL0 write-permission-fault handler's **eager-region upgrade**: a page found
read-only inside a mapping the region says is writable gets its PTE repaired
instead of a `SIGSEGV`. Gated on `AP_RW_ALL`, so anything that is not positively
"writable" simply declines to repair — the historical behaviour.

Then a second reader was added: `mprotect(PROT_READ)` was not being enforced
across a `fork`, because the CoW-break arm fires on `cow_ref > 0` alone and hands
the writer a private writable copy. A CoW-demoted page and an `mprotect`-demoted
page are both read-only in the PTE; `flags` is what distinguishes them. So the
CoW break was gated on the record.

**Four writers, each fine for granting, each a false denial:**

| # | writer | what it did | why it was fine for granting | what it denied |
|---|---|---|---|---|
| 1 | `MmapRegion::owned()` | defaults `flags` to `NONE`, documented as "protection **unrecorded**" — chosen precisely because `NONE` grants nothing | an unknown protection must not grant a write | every region built without explicit flags |
| 2 | `update_eager_region_flags` | recorded a sub-range `mprotect` against the **whole** region; its comment says this is safe "because the fault handler only ever uses these flags to grant a write" | over-applying a *downgrade* only ever withholds a grant | a guard page's neighbours — `mprotect(PROT_NONE)` on one page marked the whole mapping |
| 3 | `sys_mremap` | `old_flags.unwrap_or(NONE)` when the source region was not found by exact `start_va` | "not found" → don't grant | **every `mremap` of a lazy or sub-range source** — and allocators `mremap` on every `realloc` |
| 4 | `fork`'s region copy | `owned_with_flags(va, frames, region.flags)` — carries the value, drops "was it recorded" | the value alone is enough to grant | a child of an unrecorded parent, which now states `NONE` |

Instance 2's comment is the one worth reading twice. It is *correct*, it states
its own precondition, and the precondition was silently invalidated by adding a
second reader. Nobody edited that function.

## How it presented

Identical every time, and legible once you know the shape:

```
[MPROTECT-DENY] pid=216 va=0x148dfb000 write refused by recorded protection
[WPF] pid=216 … va=0x148dfb000 cow_ref=1 lazy_self=0xffff… eager=0x60000000000080 ap_rw=false
[Fault] Process 216 (rustc) SIGSEGV
```

- `eager=0x60000000000080` is exactly `AP_RO_EL1 | UXN | PXN` = `user_flags::NONE`.
- `cow_ref=1` — a genuine CoW page, the write is legitimate.
- **zero `mprotect` traces in the whole boot** — nothing ever asked for this.

The `SIGSEGV` address equals the `[MPROTECT-DENY]` address. That correlation is
what turns "rustc is flaky again" into "this is ours": Akuma has a *known*
intermittent rustc `SIGSEGV` in self-host builds
([`instr_abort_relr_wedge`](../../README.md)), so a crash alone proves nothing.
The matching address proves it.

## Three fixes that were not fixes

Recorded because each was reasonable, and each failed for a different reason:

1. **"Treat `NONE` as unrecorded."** Filters instance 1. Did not help: the
   regions in the crash had `NONE` *recorded* — instances 2–4 put it there.
2. **Add `MmapRegion::prot_recorded`** to separate "unrecorded" from "explicit
   `PROT_NONE`". Correct, necessary, and still not sufficient: the record was
   genuinely recorded, just for pages it did not name (instance 2).
3. **Make `mprotect` split the region** so the record is page-accurate
   (`mprotect_eager_regions_in_range`). Fixes instance 2. Still crashed —
   instances 3 and 4 write `NONE` with no `mprotect` involved at all.

The pattern in the failures: each fix addressed *the writer the evidence pointed
at*, and the evidence pointed at whichever writer happened to fire first. Only
enumerating **every** writer of the field ended it.

## What actually fixed it

All four, together. Removing any one brings the crash back:

- `MmapRegion::prot_recorded` + `recorded_prot()` — "did this region ever state a
  protection", separate from the value.
- `mprotect_eager_regions_in_range` — `mprotect` **splits**; a piece's flags
  describe that piece and nothing else. (The old objection, "splitting would have
  to split `frames` in step", was never true: `detach_eager_regions_in_range` had
  been doing exactly that all along.)
- `sys_mremap` — no `unwrap_or(NONE)`; an unknown source produces
  `MmapRegion::owned()`, which states nothing.
- `fork` — carries `prot_recorded`, not just `flags`.

## The rule

**Before reading a permission record to refuse anything, enumerate every writer
and ask each one: can this under-state?** Not "is the value right" — "is the value
a *statement*, and does it describe *exactly* the pages it is attached to". A
record that has only ever granted has never had to answer either question, and
its authors had no reason to make it.

Two structural tells that a record is grant-only, both present here:

- A **sentinel that means "unknown" shares a representation with a real value**
  (`NONE` for both "unrecorded" and `PROT_NONE`). Only a denying reader can tell
  them apart, so only a denying reader is broken by conflating them.
- A writer's comment **justifies imprecision by naming the reader**
  ("safe because the fault handler only ever uses these flags to grant"). That
  sentence is a precondition, and adding a reader invalidates it. Treat any such
  comment as a blocker on the change you are making.

## Verification

- `akuma-mmap`: 32 host tests, including `mprotect_middle_page_leaves_its_neighbours_writable`
  (instance 2 as an executable assertion) and
  `unrecorded_none_and_explicit_prot_none_are_distinguishable` (instance 1).
- `userspace/forktest/c_stress/eager_mprotect_probe` — both phases.
- The reproducer: an in-VM `cargo build --release` of Akuma itself in
  devbox-smoltcp, which killed `thiserror-impl` and `zerocopy-derive` on every
  run before the fix.

## What this did NOT fix: `cowstale`

Worth stating because the two are easy to conflate — both are CoW write faults,
and one was fixed in this work.

`userspace/forktest/c_stress/cowstale` still fails intermittently. Same-size
sweeps at `SMP=4`, 14 runs each, on the same host and boot configuration:

| tree | PASS | SEGV |
|---|---:|---:|
| `main` | 5 | 9 |
| after this work | 9 | 5 |

**Do not read that as an improvement.** At n=14 the difference is comfortably
inside binomial noise for a race, and nothing in these four changes touches the
mechanism `cowstale` exercises. `[MPROTECT-DENY]` fired **zero** times across
either sweep, which is the check that matters: the deny gate is not involved in
`cowstale`'s failure in either direction.

`cowstale` remains a known open flake that fails on `main`. It is tracked
separately and is not evidence for or against anything here.

> **Update 2026-08-30:** the residual got its second fix stage — the absorb now
> re-checks after the fault-slot wait and at SIGSEGV delivery
> ([`COWSTALE_FORK_THREAD_SEGV.md`](COWSTALE_FORK_THREAD_SEGV.md) header) —
> cutting the SMP=4 in-boot rate from ~30-60% to 1/15 (hammer storm) and 0/8
> (classic). The `[MPROTECT-DENY]`-gate verdict above is unaffected: the new
> checks read the live PTE only and never consult region records, so the deny
> gate still has no vote in this class. One hammer survivor with the old
> signature remains; see the archive header before treating `cowstale` as
> fully closed.

## Background

- [`reference/subsystems/syscalls/mem.md`](../reference/subsystems/syscalls/mem.md)
  — where the repair paths and their gates are documented.
- [`AKUMA_EXTRACT_MMAP.md`](AKUMA_EXTRACT_MMAP.md) §10.4 — the probe that found
  the original `mprotect`-across-fork hole.
- [`J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md`](J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md)
  §3 — why `MmapRegion::flags` exists, and why its default is `NONE`.
