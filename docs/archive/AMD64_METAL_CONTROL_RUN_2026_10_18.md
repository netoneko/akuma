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

## Background

[`AMD64_UNSAFE_REDUCTION_TRAP_MSR.md`](AMD64_UNSAFE_REDUCTION_TRAP_MSR.md)
