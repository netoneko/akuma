# Plan: `forbid(unsafe_code)` for akuma-exceptions, akuma-tokio-kernel-amd64 and akuma-threading (2026-11-06)

Goal from the owner: if these three crates can forbid `unsafe`, the whole kernel loses ~90 of 467 production
`unsafe` sites in one move (21 + 18 + 53 at the time of writing, after `3ec608e4`). This is the survey of what
actually blocks it, per crate, and the order to do it in. Nothing here is landed yet.

## What `forbid(unsafe_code)` actually rejects

Not only `unsafe { }`. The lint also fires on **`unsafe fn`, `unsafe impl`, `unsafe extern` blocks, and the
`#[unsafe(no_mangle)]` / `export_name` / `link_section` attributes**. `global_asm!` is *not* flagged (it needs no
`unsafe`), `asm!` is (it is only callable in an `unsafe` block). So "forbid" is a statement about the crate
containing **no entry shims, no extern declarations, no asm and no raw-pointer dereference** - the Rust bodies
can stay where they are.

## The shape that makes it possible: an entry crate *above* the logic crate

The vector table, `rust_sync_el0_handler` & co. are `no_mangle` symbols the assembly branches to. The logic they
run is safe Rust over `FrameHandle` / `IrqFrame`. So split by the claim each side makes:

```
akuma-exceptions        (forbid)   pub fn sync_el0(frame: FrameHandle), irq_el0(frame: IrqFrame), el1_sync(..), ...
akuma-exceptions-entry  (unsafe)   global_asm! vector table, #[no_mangle] shims that build the handle once
                                   (FrameHandle::new / IrqFrame::at), `msr vbar/elr/tpidr_el1`, extern blocks
```

The edge points **entry -> logic**, never back (the shim calls pub safe fns; the logic never names the shim's
symbols), so the logic crate keeps compiling for the host. `akuma-threading` and `akuma-tokio-kernel-amd64`
get the same split (`-switch` / `-boot` leaf crates). Every `unsafe` that moves keeps its `# Safety` text; the
budget moves with it, so **the total does not drop by 90 - it drops by whatever the entry crates do not need**
(the dereferences and `from_pa` claims, which are the real reduction). Be honest about that in the budget diff.

## Per crate

### akuma-exceptions (21 sites) - first, aarch64-testable under HVF

| sites | what | disposition |
|---|---|---|
| 5 `no_mangle` handlers + 4 `unsafe extern` blocks + `global_asm!` + `FrameHandle::new` + 4 `IrqFrame::at` | entry | move to `akuma-exceptions-entry` |
| `msr vbar_el1`, `msr elr_el1` x2, `msr tpidr_el1` | control-flow / re-pointing writes | move to the entry crate (stay `unsafe`, as `akuma-cpu` documents) |
| `__el1_gpr_transparency_probe` call + its extern | boot self-test | entry crate (`kernel_tests` only) |
| 4 crash-dump reads (`saved_regs.read()`, `*(elr as *const u32)`, two `*(l?_addr as *const u64)`) | display-only reads of kernel memory | a `peek`-style safe reader in the entry/forensics crate (`akuma-exceptions-forensics` already exists for this) |
| `from_raw_parts_mut` (readahead fill), `from_raw_parts` (FPC verify) | file read into a staged page; read of a cache frame | batch `FreshFrame` (`alloc_batch_zeroed` + `fill_with`, `UNSAFE_OWNERSHIP_TYPES_DESIGN.md` 2.1); a cache-frame reader in `akuma-fpcache` |
| `as unsafe fn(..) -> ! as usize` | function-pointer cast to an `unsafe fn` | give `enter_user` a safe wrapper in `akuma-exec`/`akuma-el0-entry` |

Already done on this path: the CoW break, trampoline, single-page fill (`da9a90a3`), the NEON locator
(`3ec608e4`). Gate: `mw_gate`, the CoW probes, `fpcprobe` (FP state across signals).

### akuma-tokio-kernel-amd64 (18 sites) - second, TCG-testable

Mostly *claims about frames*: nine `Frame::from_pa` (walker/`lazy.rs`), which is the duplicated 4-level walk the
ownership design already says to delete (item 7) in favor of `akuma_pte_amd64`. The rest are small leaves:
`load_cr3` x2 (a `ctlreg::activate`-style named function), `ap::install_trampoline`, the `smp.rs` `no_mangle`
AP entry + extern + `global_asm!`, two `asm!`, `enter_user`, one `MmioWindow`-shaped read. Plan: delete the
duplicate walker, add `Frame` cursors from `akuma-pte-amd64`, move the six leaves into
`akuma-tokio-kernel-amd64-boot`. Verify on TCG `SMP=4` + metal.

### akuma-threading (53 sites) - last

The hard one: it owns the context table. Sites cluster as
(a) switch asm + `no_mangle` + externs (aarch64 and amd64 `global_asm!`, `x86_switch_context`) -> `-switch` crate;
(b) `get_context_mut` / `stamp_slot` / `Context` slot dereferences (~20) -> `SlotClaim` + `peek` from
`UNSAFE_CONTEXT_TABLE_DESIGN.md` / `UNSAFE_OWNERSHIP_TYPES_DESIGN.md` 3 (needs the amd64 metal boot);
(c) `Waker::from_raw` / `Box::from_raw(closure_ptr)` x4 -> a safe `Arc`-based waker (`alloc::task::Wake`) and a
typed closure box (no `unsafe`; allocation story to be checked against the "analyze every allocation" rule);
(d) `unsafe impl Sync for SyncContext`, transmute of `entry` to `extern "C" fn`, `Location` read from a raw
pointer, `AtomicU64::from_ptr` (peek) -> each one a named leaf.
Do (a) and (c) first (mechanical), (b) needs the `SlotClaim` change and its own metal session.

## Order and gates

1. `akuma-exceptions-entry` split + batch `FreshFrame` + forensics readers -> forbid. `mw_gate`, CoW probes, `fpcprobe`.
2. `akuma-tokio-kernel-amd64`: delete the duplicate walker, boot leaf crate -> forbid. TCG `SMP=4`, then metal.
3. `akuma-threading`: (a) + (c), then `SlotClaim` (b) with a metal boot.

## Background

`UNSAFE_OWNERSHIP_TYPES_DESIGN.md`, `UNSAFE_CONTEXT_TABLE_DESIGN.md`, `COW_FRESHFRAME_STAGING.md`, `docs/reference/crate-safety.md`.
