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

1. `physmap: reaches PHYSMAP_LIMIT` (`amd64/src/mem.rs`): metal-only. QEMU is
   824/0. Suspects: MAXPHYADDR below 36 bits, 1 GiB page support, a reserved bit
   in a high entry. Needs CPUID values from the box.
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

## Background

[`AMD64_UNSAFE_REDUCTION_TRAP_MSR.md`](AMD64_UNSAFE_REDUCTION_TRAP_MSR.md)
