# amd64 audit: trashcan control run (2026-10-18)

Hardware: trashcan (HP 500-502nj), SMP=4. Kernels: audit `d6ed6691`
(md5 `2aa83fa8…`) against the session-start baseline `c1fe3655`
(md5 `9b9ca29e…`), both Mac-built, installed with `kinstall`.

## Results

| Check | Audit `d6ed6691` | Baseline `c1fe3655` |
|---|---|---|
| Self-test | 777 passed, 1 FAILED: `physmap: reaches PHYSMAP_LIMIT` | **same** (777 / 1, same check) |
| Spawn stress, 4 loops x 150 `busybox true` | all done; 0 PANIC/CANARY/[Fault]/FREED-CR3/TLB stuck | not run |
| Incremental `kbuild -j 4` | `cargo build` spins (11:14 CPU in 13 min up), never spawns `rustc` | **same** (4:32 CPU in 5 min up, no `rustc`) |

Both failures are **pre-existing**: neither is caused by the unsafe audit.

## Open items (not root-caused)

1. `physmap: reaches PHYSMAP_LIMIT` (`amd64/src/mem.rs`): **bisected 2026-10-18 to
   `410117ce` (akuma-fbdev + akuma-fbcon)** — fails on every GRUB/multiboot2 boot
   with the fb console up (metal and the ryzen QEMU rehearsal), passes under PVH
   or `nofb`. The CPUID suspects below are ruled out (a software walk reads RAM),
   as is the 64 GiB commit itself; prime suspect is a boot-stack overflow into
   `__ap_pml4` + `__pd0`'s last PDE. See
   [`AMD64_PHYSMAP_SELFTEST_LAST_PDE.md`](AMD64_PHYSMAP_SELFTEST_LAST_PDE.md).
2. Incremental `kbuild -j 4` stalls in `cargo` after the
   `akuma-pipes (manifest)` warning, before any `rustc`. `cargo` is at ~100% CPU.
   Possibly the same getdents spin as `ls /root` (kernel `a1022dcd`); untested.
3. Boot-time `[SSHD] Accept error ... errno=88` flood fills the dmesg ring
   (~815 lines), so the `self-test` line is gone from `dmesg` shortly after boot.
   Not compared against the baseline.

## Same day: items 3-4 of the audit (600 -> 589 sites)

- `amd64/src/mm.rs`: 9 -> 0. The physmap views went onto the checked helpers
  `akuma-mmu` already had (`with_phys_bytes_mut`, `copy_to_phys`,
  `copy_from_phys`, bounds-checked by `akuma_pmm::contains`) and
  `alloc_page_zeroed`, so no new `unsafe` anywhere. Zeroing a live frame copies
  from a `static ZERO_PAGE`. The two write-back paths copy a page into a stack
  buffer instead of borrowing a view of a frame userspace may be storing into.
- `amd64/src/uaccess.rs`: `clac` moved into a new safe `akuma_cpu::smap::clac()`
  (`akuma-cpu` +1, `amd64` -1: isolated, not removed); `write_val` uses
  `akuma_user_access::as_user_bytes`; three self-test frame fills use
  `copy_to_phys`. Kept as they were: the `cr0`/`cr4` writes (writes stay
  `unsafe`), the raw `copy_*_user_safe` calls (the range check is the caller's),
  `read_val`'s `MaybeUninit` and the volatile read-backs in the self-tests (a
  non-volatile read could be forwarded from the preceding store and make "the
  page is unchanged" pass vacuously).
- `amd64/src/net.rs`: the open-coded `rdtsc` in `weak_fill` -> `akuma_cpu::tsc`.

Verification: host tests for `akuma-cpu` and `akuma-mmu` pass. QEMU SMP=4: two
runs of 824 passed / 0 failed. A first run printed 3 failures
(`spawn: the child's registered table holds fd 0/1/2`) with the host at load
average ~5; the check asks a freshly spawned child for its fd table and fails
if the child has already exited, so it is a load-sensitive race, not
reproduced in either rerun. Trashcan (metal), kernel `6a776f03` (md5
`41d9eac0...`): boots, ssh up, self-test 777 passed / 1 FAILED (the same
`physmap` check as the baseline), 4 loops x 150 `busybox true` + `cat
/bin/busybox` finish, no PANIC/CANARY/[Fault]/FREED-CR3/SWITCH NO-BKL/TLB stuck.

Not done: the AArch64 half of `akuma-mmu` (needs the lima VM, stopped); the
amd64 drivers `i2c.rs`/`rtw89.rs`/`nvme.rs`/`watchdog.rs` (need ryzen running
Akuma - it is up as Pop!_OS); `net.rs`'s remaining MMIO sites go through
`akuma_net::ExternalDevice::probe_rtl8169(*mut u8)`, a cross-crate API change.

## Same day: ryzen metal, head against the baseline

Both kernels built **natively on ryzen** (a Mac-built one hangs there,
`AMD64_MAC_BUILT_KERNEL_METAL_HANG.md`), the **tests** build of each, installed as
p3's `/boot/akuma-amd64` and booted with GRUB entry 20 (`smp=8`, wifi, sshd, stays
up), read over wifi ssh (`~/.ssh/id_ed25519`; the Oct-6 per-build test key is
not in p3's `authorized_keys`).

| kernel | self-test | failures |
|---|---|---|
| head `0d604438` (twice) | 765 passed, 5 FAILED | `physmap`, `pci: every enumerated function has a real vendor id`, `spawn: ...fd 0 as Stdin`, `fd 1 as Stdout`, `fd 2 as Stderr` |
| baseline `c1fe3655` | 765 passed, 5 FAILED | **the same five** |

So all five predate the audit and my `mm.rs`/`uaccess.rs` changes. The `spawn`
trio is `smp=8`-specific on the machines tried: it passed on the trashcan (4
cores) and in two of three QEMU SMP=4 runs. The check asks the just-spawned child
for its fd table and `Process::free`/`close_all` sweeps it at exit, so a child that
finishes before the parent looks fails all three. Not proven: the baseline
fails it too, but no change to that test was made. The `pci` failure is one
or more of 42 enumerated functions with a bogus vendor id - ryzen hardware,
`pci.rs` untouched.

Head `no-tests` kernel (`0d60443`, md5 `7523e7bc...`, p3 `/boot/akuma-amd64`):
boots on entry 20; 8 loops x 100 `busybox true` + `cat /bin/busybox` finish with
no PANIC/CANARY/[Fault]/FREED-CR3/SWITCH NO-BKL/TLB stuck. The tests kernel
survived 8 x 150.

**`ls /root` does not spin on ryzen's p3** (49 entries, returns at once) on the
same kernel that spins on the trashcan's `sda1`. That makes the trashcan's
filesystem, not `getdents`, the first suspect for both the `ls /root` spin and
the incremental `kbuild` stall; the `e2fsck -fn` from Ubuntu is still wanted and
needs a keypress at GRUB (Akuma is the default entry, nothing remote returns to
Ubuntu).

p3 state left behind: `/boot/akuma-amd64` = head no-tests, `.prev` = the
kernel that was running before (`a028cec9`, md5 `4b9edb35...`); ryzen is back
on Pop.

## Same day: the four drivers (`b37eb82f`), 107 -> 81 amd64 sites, 589 -> 566 total

`watchdog.rs` and `i2c.rs` onto `MmioWindow`; `nvme.rs` and `rtw89.rs` onto
`DmaArray` statics plus `MmioWindow` BARs (`pci::map_bar_window`). `akuma-dma`
gained the two methods the rtw89 card needed: `read_range` (a shared slice over
one RX entry rather than the whole array the chip is still filling) and
`leak_mut` (a permanent `&'static mut` for a card that owns its TX memory; every
other access then panics, so the "called once" DMA contract is now enforced
rather than assumed). `akuma-dma` +2 sites, host-tested. `touchpad.rs` follows
`i2c::map_mmio`'s new return type; `watchdog` also drops its `cli` for
`akuma_cpu::daif::mask_irq`.

Verified: QEMU SMP=4 824/0; ryzen metal, tests kernel `b37eb82`, GRUB entry 20
(`smp=8`): `wdt: armed, 60 s` and the box stays up for 4+ minutes (the pet
works); `nvme:` init in 10 ms, GPT p3 found, root on it; `[rtw]` firmware
downloaded and the card kept as `/dev/wifi0`, with this ssh session running
over it; a 2.4 MB file read from NVMe and sent over wifi four times, md5 equal
each time (RX ring wraps); `[touchpad]` GXTP5100 found on `dw_i2c`, HID
descriptor read, `ptp report` parsed. Same 765/5 self-test as the baseline
(`physmap`, `pci`, `spawn` x3). `[bkls>]` spin lines appear on every ryzen boot
including the baseline control (49-68 per boot): not new.

Not exercised: `net.rs`'s rtl8169 MMIO (trashcan only, and untouched here),
`i2c`'s write path beyond the touchpad's descriptor read.

p3 state: `/boot/akuma-amd64` = head `no-tests` built natively at `b37eb82f`
(md5 `12d532a2...`), `.prev` = `a028cec9`; ryzen back on Pop.

## 2026-10-19: the AArch64 half (`akuma-mmu`, `akuma-exceptions`), 517 -> 486 sites

- `akuma-mmu` 80 -> 31 (`fb71bb62`): every AArch64 table walk reaches a frame
  through one helper (`table`/`table_phys`, `akuma_pagetable::Table::at`'s
  contract) instead of `phys_to_virt(pa) as *mut u64` + volatile access.
  `l3_slot_in` and the current-TTBR0 editors return a `Slot`; the acquire/CAS
  ordering of `get_or_create_table_atomic` and `map_user_page_inner` is kept
  through the new `Slot::get_acquire`. The public `unsafe fn`s
  (`map_user_page`, `shatter_block_to_pages`, `demote_range_to_ro`) keep their
  signatures and contracts - their callers in `src/` are untouched.
- New leaf crate `akuma-trap-aarch64` (`cbb6ca54`): `FrameHandle` over
  `UserTrapFrame` with volatile accessors, `snapshot` and `store`, made once at
  `rust_sync_el0_handler`. `akuma-exceptions` 83 -> 38; the new crate is 14.
  One behaviour fix: emulating a trapped `MRS` with `rt == 31` wrote word 31 of
  the frame (`sp_el0`); it is now discarded as XZR, as the read side already did.
- Verified: host tests, clippy on both targets, and the AArch64 boot suite under
  **KVM in lima**, before and after: the same two failures
  (`test_mmap_file_oom_survives`, `test_epoll_socket_waker`) and the same 307
  `PASSED` lines on the pre-change kernel and on each change. Both failures
  predate this work.
- **Lima setup that was missing** (`limactl start fc`, VM `fc`): `qemu-system-arm`
  was not installed and the user was not in the `kvm` group, so
  `scripts/lima_aarch64_run.sh` died with `qemu-system-aarch64: not found` after
  selecting TCG. `sudo apt-get install qemu-system-arm`, `sudo usermod -aG kvm
  $USER`, then `limactl stop fc && limactl start fc` so the group applies; the
  runner then reports `accelerator: KVM`. Stop your own instance with
  `ss -ltnp | grep :2622` inside the VM (`INSTANCE=4` -> ssh 2622), never a
  pattern kill.
- Left: `akuma-threading` 65 and `akuma-cpu` 65 (context switch and instruction
  wrappers - irreducible by design), `akuma-virtio` 38, `akuma-net-nic` 29,
  `akuma-alloc` 24, `akuma-user-access` 20, `amd64/src` 81 (mostly MSR/CR
  writes, extern symbols and `MmioWindow::new` claims).

## Background

[`AMD64_UNSAFE_REDUCTION_TRAP_MSR.md`](AMD64_UNSAFE_REDUCTION_TRAP_MSR.md)
