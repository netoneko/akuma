# akuma-kernel-amd64 unsafe + cleanup audit (Opus subagent, 2026-11-12, read-only)

37 production `unsafe` sites in `crates/akuma-kernel-amd64` at 5b596c57 (matches `docs/reference/unsafe-budget.json`). Paths are relative to `crates/akuma-kernel-amd64/src/` unless they name another crate. Not yet checked against the code: a plan, not findings.

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
