# Defects found by the type-system safety work (2026-11-17)

The sweep that introduced `Pages`/`PageRange`, `ThreadRef::with_live`, and `IrqSpinlock` (see
[`docs/reference/safety-types.md`](../reference/safety-types.md)) found six real defects. Each was fixed with the type that
closes its class, not by a point patch. Current rules live in the reference doc; this is the incident record.

## Memory

1. **`brk` near the top of the address space unmapped the process.** `Process::set_brk` rounded with
   `(new_brk + 0xFFF) & !0xFFF`, which wrapped to page 0, read as a shrink, and ran `drop_pages(0, old_top)`: the process's own
   image and heap. Now `brk_tops` via `page_ceil`; a request it cannot honour returns the old break, as Linux does.
2. **ELF `PT_LOAD` segments were not validated.** `vaddr + memsz` was unchecked in `segment_span`, the deferred path's
   `end_page`, relocation and boundary passes. `validate_load_segments` now runs once before any rounding: a segment must end
   inside the 48-bit space, must not wrap, and `p_filesz <= p_memsz` (`EINVAL`, as Linux). Applies to the interpreter too.
3. **`akuma-mmu` range checks wrapped.** Four sites computed `(va_start + len + PAGE_SIZE - 1) & ...` after `user_range_ok`;
   a length near `usize::MAX` wrapped to "empty". `page_span` makes a wrapping range "not mapped".
4. **`prefault_user_range` and `madvise(WILLNEED)` rounded unchecked.** Plain or saturating add followed by `+ 0xFFF`;
   now `checked_add` plus `page_ceil`.

## SMP & locking

5. **xHCI `KEYS` deadlocked a single core.** Readers took it with IRQs on; the producer (`kbd_event`) ran under the
   IRQ-masked controller lock. A tick during a read switched to the pump, which spun on `KEYS` masked while the holder could not
   run. `KEYS` is now an `IrqSpinlock`. Builds on amd64; not run (QEMU has no xHCI keyboard).

## Scheduler & process management

6. **`tkill` and the hard terminates could act on a recycled thread slot.** The tid was a bare number; a slot reborn between
   the lookup and the effect took the signal or the terminate. `tkill` now acts through the registering thread's recorded
   `ThreadRef` with `with_live`; the grace-expiry and single-core terminates do the same with the sibling's recorded reference.
   A stale row is refused, reported as success, as for a thread that just exited.
