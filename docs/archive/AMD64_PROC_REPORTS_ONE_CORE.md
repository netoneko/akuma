# amd64 `/proc` reported one core on a four-core box — 2026-09-26

**Outcome: fixed and verified under QEMU `-smp 4`. Not yet deployed to the
trashcan.** The box always had four cores online and scheduling work; only the
reporting was wrong. Three independent defects combined to produce it.

## 1. Symptom

On the trashcan (HP box, bare-metal Akuma/amd64,
`0.0.8 2749bafb-release-smp-shared`), over ssh:

```
$ nproc
4
$ cat /proc/cores
core state role
0 online bsp
$ cat /proc/stat | head -2
cpu  47429 0 0 0 0 0 0 0 0 0
cpu0 0 0 0 12676 0 0 0 0 0 0
```

Four 4-way shell busy loops left both files unchanged. `top` and anything else
reading `/proc/stat` therefore showed one CPU.

The boot suite in `dmesg` showed that the hardware side was fine:

```
smp:  4 cpus online
smp: every secondary takes timer interrupts   [OK]
smp: cpu mask the workers ran on 15
smp: two workers executed simultaneously   [OK]
smp ring3: two processes ran on two cores   [OK]
```

So bring-up and scheduling worked, and only `/proc` was wrong. `nproc` gets
its answer from a different path than `/proc`, which is why the two disagreed.

The `cpu0` line gave two clues: there is only **one** row, and its busy column
is **0** while the aggregate `cpu` line holds 47429 jiffies. Those turned out to
be two separate bugs.

## 2. Defect 1: `akuma-vfs-glue/smp-shared` was never forwarded on amd64

`akuma-vfs-glue/src/proc.rs::active_core_count` sizes both the `cpuN` rows and
the per-core buckets:

```rust
#[cfg(kernel_smp_shared)]
fn active_core_count() -> usize { crate::probed_core_count().clamp(1, MAX_CORES) }
#[cfg(not(kernel_smp_shared))]
fn active_core_count() -> usize { 1 }
```

The crate's `build.rs` emits `kernel_smp_shared` from its own `smp-shared`
feature. That `build.rs` header already warns about this exact failure: without
the cfg, "`/proc` quietly divides by one core on a four-core machine — no build
error, no runtime error, just wrong numbers".

The AArch64 root `Cargo.toml` forwards `akuma-vfs-glue/smp-shared`. The amd64
`smp-shared` feature forwarded it to `akuma-exec` and `akuma-syscalls-glue` but
**not** to `akuma-vfs-glue`, so the amd64 kernel compiled the `1` fallback.
amd64 did register its `probed_core_count` hook correctly
(`crate::smp::online_cpus`, `amd64/src/boot.rs` and `amd64/src/fs.rs`), but
with the cfg off nothing ever called it.

**Fix:** `amd64/Cargo.toml`, add `"akuma-vfs-glue/smp-shared"` to `smp-shared`.
It was safe to enable: the only code that `kernel_smp_shared` gates in that
crate is `active_core_count` and `probed_core_count`.

## 3. Defect 2: the x86 switch arm never stamped `LAST_CORE`

`cpu_time_snapshot` adds each thread's CPU time to the row for
`threading::get_thread_last_core(tid)`, which reads `akuma-threading`'s
`LAST_CORE[tid]`. The generic `commit_switch` stamps it:

```rust
LAST_CORE[next_idx].store(bkl::current_core_id() as u8, Ordering::Relaxed);
```

amd64 never goes through `commit_switch`. Its switch path is the x86 arm
(`x86_yield_now`), which gained its own CPU-time accounting earlier (the fix
that made `ps`'s TIME column non-zero) but not the `LAST_CORE` stamp. Every
slot therefore stayed at the `0xFF` "never scheduled" initial value, and
`core < cores` dropped every thread from the per-core buckets. That explains
`cpu0`'s busy column of 0 next to a non-zero aggregate. It would have zeroed
every `cpuN` row even after defect 1 was fixed.

amd64's own `sched.rs` has a **separate** `LAST_CORE` array, stamped in
`hook_switch_to` and read only by its slot-table dump. That is why the
kernel's own diagnostics looked right while `/proc` did not: two arrays share
the name, and only one of them was being written.

**Fix:** `crates/akuma-threading/src/lib.rs`, in the x86 arm directly after
the CPU-time block, stamp `LAST_CORE[next]` with `bkl::current_core_id()`. On
x86_64 that is `akuma_cpu::percpu::core_id()`, i.e. `PerCpu::index` (0 = BSP,
dense), **not** a LAPIC id. That matters because the trashcan's LAPIC ids are
sparse (dmesg: `cpu 1 online (lapic id 2)`, `cpu 2 online (lapic id 4)`), and
using them would have put cores past the `core < cores` cut.

## 4. Defect 3: `/proc/cores` was a static string on both kernels

`/proc/cores` returned the literal `core state role\n0 online bsp\n`. It is a
leftover from the removed multikernel (`TRIM_FAT_MULTIKERNEL.md`), kept for
herd. This was wrong on AArch64 too, not just amd64.

**Fix:** render one row per `active_core_count()`, with core 0 as `bsp` and the
rest as `ap`. herd only streams the file and never parses its rows, and the
boot-suite test `test_procfs_virtual_files_are_readable` only requires a
non-empty read, so the format change affects no consumer.

## 5. Verification

- `cargo build -p akuma-amd64 --target x86_64-unknown-none --release`: builds.
  `cargo check --release` (AArch64): builds.
- `cargo test -p akuma-threading -p akuma-vfs-glue` on the host: green.
- Clippy: no new warnings. The remaining ones are in `amd64/src/mm.rs`,
  `amd64/src/usermode.rs` and `akuma-ext2`, none of them touched by this change.
- `SMP=4 sh amd64/run.sh` (microvm, TCG), a 4-way busy loop, then:

```
core state role
0 online bsp
1 online ap
2 online ap
3 online ap

cpu  2891 0 0 547 0 0 0 0 0 0
cpu0 690 0 0 169 0 0 0 0 0 0
cpu1 716 0 0 143 0 0 0 0 0 0
cpu2 669 0 0 190 0 0 0 0 0 0
cpu3 814 0 0 44 0 0 0 0 0 0
```

Four rows, each with real busy time. **Not yet verified on the trashcan
itself**; that needs a staged kernel and a reboot. After deploying, check that
`cat /proc/cores` shows four rows and that four busy loops raise every `cpuN`
row.

## 6. What is still missing

- `/proc/cpuinfo` and `/proc/cmdline` do not exist on amd64.
- `/proc/stat`'s `intr`/`ctxt`/`btime` are still literal zeros.

## 7. Lesson

Two failures here were silent, and two layers were both called `LAST_CORE`. A
`cfg` that selects a fallback raises no error when the feature is missing, so
**when a crate's behaviour depends on a forwarded feature, check that every
kernel's feature list forwards it.** The AArch64 root and amd64 lists are
maintained separately and drift apart. Likewise, when an arch has its own
switch path, go through everything `commit_switch` records and confirm the arch
arm records it as well. The CPU-time half had already been fixed that way
(`ps` TIME read 0), and the core half had not.

## Background

* [`../reference/subsystems/vfs.md`](../reference/subsystems/vfs.md): the
  `/proc` table and the `cpu_time_snapshot` notes, which are the current-state
  version of this.
* [`TRIM_FAT_MULTIKERNEL.md`](TRIM_FAT_MULTIKERNEL.md): where `/proc/cores`
  came from.
* [`AMD64_TRASHCAN_ISSUES.md`](AMD64_TRASHCAN_ISSUES.md): other bugs found on
  the same machine.
