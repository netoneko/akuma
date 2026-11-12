# akuma-kernel-amd64 unsafe + cleanup audit (Opus subagent, 2026-11-12, read-only)

37 production `unsafe` sites in `crates/akuma-kernel-amd64` at 5b596c57 (matches `docs/reference/unsafe-budget.json`). Paths are relative to `crates/akuma-kernel-amd64/src/` unless they name another crate. Not yet checked against the code: a plan, not findings.

## Status (2026-11-12, late)

37 -> 18 -> 7.

First pass (37 -> 18): the CR3 chain (9, checked `activate`), the scheduler self-test counters (2,
atomics), the no-op waker (`Waker::noop`), `machine.rs` and the crash dump's memory peek
(`peek_ram`), `blk.rs` and `power.rs` (`MmioWindow` from `map_device_window`), the wake IPI stub
(`akuma_trap_amd64::Kind::Wake`), the reboot triple fault (`akuma_idt_amd64::reset_by_triple_fault`),
the RAM disk (`akuma_phys::RamSpan`, one claim at the module reservation in `multiboot2.rs`).

Second pass (18 -> 7):
- `gdt.rs` 2: the GDT and TSS are `akuma-percpu-gdt-amd64`'s (`init_cpu` claims the slot once,
  `set_rsp0` finds its core through `gs:[24]`).
- `idt.rs` stack word: `peek_ram` below the physmap check.
- `signal.rs` `deliver_pending`: scoped `read_current_uctx`/`with_current_uctx` borrows instead of a
  raw `&mut UserCtx` that aliased `mark_leave`'s borrow.
- `uaccess.rs` 4: `akuma_user_access::copy_{from,to}_user_upper_half` (range check + fault recovery,
  no page walk: the amd64 kernel lives in the upper half, so no user address aliases it), and
  `read_val<T: Copy + Default>` through `as_user_bytes_mut`.
- `multiboot2.rs` 2: the info block is copied out through `peek_ram` into a 64 KiB static; the EGA
  window is `akuma_boot_amd64::mem::ega_text_window` (below the PMM's 1 MiB floor).
- `smp.rs` trampoline: `akuma_boot_amd64::ap::TrampolinePage`, claimed in `boot.rs`, consumed by
  `install`.

Verified: amd64 boot suite (default features, SMP=2) 836/836, clippy `-D warnings` for default,
`headless` and `headless,no-tests`, host tests of the touched crates. Not yet booted: the multiboot2
paths (the suite boots PVH on virtio).

Left (7): `usermode.rs` 4 (ring-3 entry, unwind, LSTAR: the `akuma-usermode-amd64` crate),
`multiboot2.rs` 2 (framebuffer window, `RamSpan` claim), `smp.rs` `GsBase`. Still open from the
soundness list: `read_val`'s `T: Copy + Default` still admits `bool` and `char`, which have invalid
bit patterns; every caller passes an integer or an ABI struct, and a `UserPod` trait is the real
fix.

## Soundness first
1. uaccess::read_val<T: Copy> (uaccess.rs:310-322) builds a T from user bytes; T: Copy admits bool/char/enums/&'static T; exported to forbid(unsafe) drivers (drivers.rs:825). Same in akuma_user_access::read_user_into / as_user_bytes_mut. Fix: `unsafe trait UserPod` for ABI structs.
2. paging::map_device_window(pa,len) (akuma-boot-amd64/src/paging.rs:77) safe, exported (drivers.rs:819): MmioWindow over any PA incl. kernel RAM. pci::map_bar_window same (Bar::Memory constructible by anyone); paging::map_page same internally. Need claim types (Bar only from enumeration).
3. Theoretical aliasing: RamDisk::bytes_mut(&self) (ramdisk.rs:67); deliver_pending (signal.rs:492) raw &mut UserCtx across exit path (not UB today).

## (c) existing safe API, 21 sites
net.rs:131 noop RawWaker -> Waker::noop(); signal.rs:492 -> with_current_uctx; sched.rs:2208,2215 static mut test counters -> atomics; machine.rs:38, idt.rs:1001 -> akuma_phys::peek_ram; power.rs:154 -> map_device_window + MmioWindow; blk.rs:140 test-only -> gate; uaccess.rs:313,321 -> read_user_into (after UserPod); uaccess.rs:288,302 -> copy_{from,to}_user_with(Prefault::Yes); paging.rs:36,46,63,65,76 sched.rs:165,1265,1903 smp.rs:906 CR3 chain -> akuma_pagetable::amd64::activate::<Physmap> (tokio kernel uses it; A/B the cost).
## (a) existing leaf crates, 7 sites
reboot.rs:95 -> akuma_idt_amd64::triple_fault(); smp.rs:840 wake stub -> akuma-boot-amd64 (shootdown pattern); smp.rs:489 trampoline -> TrampolinePage token; multiboot2.rs:72 EGA, :183 mb2 info -> akuma-boot-amd64; ramdisk.rs:57,67 -> akuma_phys::RamSpan (one claim stays).
## (b) new crates, 6 sites
gdt.rs:92,119 -> akuma-tss-amd64; usermode.rs:807,868,945,1567 -> akuma-usermode-amd64 (owns syscall_entry/enter_user_mode/unwind/LSTAR install).
## (d) stays, ~4
smp.rs:204 GsBase write; idt.rs:151 stack_word; multiboot2.rs:125 framebuffer; RamSpan claim.
Net 37 -> ~4 here, ~+10 in leaf crates.

## Cleanup ranked (prod-line delta, risk)
1. drop crate-wide cfg_attr(no-tests, allow(dead_code, unused_imports)) (lib.rs:25); gate test scaffolding (sched ~2175-2238, smp ~955-1052, blk reg) : -80..-120, low
2. fd.rs forwards used only by fd::smoke_test (1912-2860) -> move to tests crate : -80, low
3. dup helpers (Physmap::read, power::map_window, noop_waker): -35, low
4. 38 redundant #[cfg(target_arch)] on mods in lib.rs: -38, low
5. stale comments: SPAWN (signal.rs:570,781,803; usermode/dispatch.rs:1125), proc_slot (mm.rs:104, sched.rs:1389), PhysMap claim sched.rs:158, dead_code allow on bkl_held smp.rs:372; history narratives usermode.rs:133,2354-2358,3294, thread.rs:60
6. demand paging implemented twice (mm.rs:313-1131 vs akuma-exec lazy_prefault): -400..-600, high
7. usermode lifecycle -> glue (run_process epilogue, after_reap hook, console write, memfd_create, sched affinity): -150..-300, med/high
8. extract signal frame -> akuma-sigframe-amd64 with host tests: ~250 moved, medium
Order: 1, cheap (c), comments/cfgs, fd forwards, UserPod, map claims, checked activate (A/B), leaf moves, new crates, big folds.

Background: [`AKUMA_SAFE_GUEST_ENTRY.md`](AKUMA_SAFE_GUEST_ENTRY.md) (the same treatment for `akuma-kvm-amd64`), [`../reference/crate-safety.md`](../reference/crate-safety.md).
