# B15–B21: a whole kernel from one short file (proposal, 2026-11-02)

> **Archived 2026-11-11 from `proposals/`.** Done: `crates/akuma-tokio-kernel` and `taoxichuan/src/main.rs`
> (one `Kernel` chain) are this proposal built. The status line below is as last written.

**Status: proposal, not started.** Steps B15–B21 of
[`docs/archive/TAOXICHUAN_CLEANUP_PLAN.md`](TAOXICHUAN_CLEANUP_PLAN.md).

## Goal

Hand someone (or their agent) one README and they get a new kernel: a `main.rs` of 20–100 lines, a `Cargo.toml` that
picks features, and a three-line `build.rs`. The same `main.rs` builds for amd64 or aarch64. Whatever the README's
defaults do not cover, the kernel's author overrides in that file.

```rust
#![no_std]
#![no_main]

akuma_tokio_kernel::entry!(main); // the entry symbol, the panic handler, the global allocator

fn main(machine: akuma_tokio_kernel::Machine) -> ! {
    akuma_tokio_kernel::Kernel::new(machine)
        .console()   // feature `console`: COM1 / PL011, stdio of a process with no channel
        .disk()      // feature `disk`: virtio-blk, ext2 mounted at /
        .net()       // feature `net`: virtio-net, DHCP, smoltcp
        .smp()       // feature `smp`: `smp=N` application processors
        .run_init()  // `init=` from the command line, as pid 1
}
```

A kernel that wants its own clock writes `.clock(my_uptime_us)`; a kernel with no disk drops `.disk()` and the
feature, and so on.

## Where things are

`akuma-runtime-tokio` is the syscall **surface**: the allow-list, `install`, and `dispatch`. It has no machine, so a
kernel using it still writes the rest itself. Taoxichuan is that rest today, and all of it is in one kernel's
`src/` (5,924 lines):

| what | where now | becomes feature |
|---|---|---|
| COM1, `console_stdio_write` | `main.rs` (`akuma-comconsole` + 50 lines) | `console` |
| virtio-blk probe, `Vda`, ext2 mount, `mount_vfs_glue`, `ROOT_FS`, `read_image` | `main.rs` | `disk` |
| virtio-net, DHCP, the NIC pump | `net.rs` | `net` (grows from glue's socket table to the device) |
| task records, `brk`/`mmap`/`mprotect`/`munmap`/`msync`/`madvise`/`mremap`, stack growth, copy-on-write faults, page-table clone and free | `main.rs` | `kernel` (memory) |
| init thread, fork/threads/exec/exit through `akuma-exec`, the slot table, address-space lifetime | `child.rs`, `exec_twin.rs`, `sched.rs`, `main.rs` (`load_task`, `spawn`, `run_init`) | `kernel` (procs) |
| delivery at return, the signal frame, `rt_sigreturn`, `kill` (glue-amd64's) | `child.rs` | `kernel` (signals) |
| GDT/TSS/IDT, traps, LAPIC timer, `arch_prctl`, `sched_yield` | `main.rs` | `kernel` (cpu) |
| APs, BKL, TLB shootdown, wake IPI | `smp.rs` | `smp` |
| TSC clock, RDRAND, `epoch=` | `net.rs`, `main.rs` | defaults, below |
| `trace-err` rings, `mmdbg=`, `wx=` | `ring.rs`, `main.rs` | `trace-err` |

## Shape

Three crates, layered:

```text
   your kernel's main.rs  (20-100 lines)
        |
        v
   akuma-tokio-kernel            arch-neutral: the builder, the features, the defaults, the
        |                        dispatch order, cmdline tokens. forbid(unsafe_code), host-tested.
        +--> akuma-runtime-tokio          the syscall surface (unchanged)
        +--> akuma-tokio-kernel-amd64     Taoxichuan's machine, moved      (x86_64-unknown-none)
        +--> akuma-tokio-kernel-aarch64   the same contract on QEMU virt   (aarch64-unknown-none, B21)
```

- **Why a crate on top and not more features on `akuma-runtime-tokio`.** The surface stays what it is: host-tested,
  `forbid(unsafe_code)`, no machine. The machine halves hold `unsafe` (CR0, TSS, the trap stacks, MMIO) and build only
  for `*-unknown-none`.
- **Why arch halves are separate crates.** `cfg(target_arch)` resolves to the *host* under `cargo test`, so an arch
  module inside a host-tested crate would compile the wrong half in tests. The convention for an arch pair
  (`akuma-trap-aarch64`, `akuma-pte-amd64`) applies, with both halves suffixed because the top crate is neutral.
- **The top crate picks the half by target**, through target-specific optional dependencies; the root never names an
  arch.

## Defaults via features

Every hook `akuma-runtime-tokio::Kernel` and the boot path need has a default behind a feature, so a root that takes
the defaults writes none of them:

| hook | default | feature | amd64 | aarch64 |
|---|---|---|---|---|
| uptime / `utc_time_us` | calibrated counter, `epoch=` anchor | `clock` (in `default`) | PIT-calibrated TSC | `CNTVCT_EL0` |
| `rng_fill` | hardware RNG, SplitMix64 fallback | `rng` (in `default`) | RDRAND | RNDR, else virtio-rng |
| `online_cpus` | the APs that came up | `smp` | LAPIC INIT-SIPI | PSCI `CPU_ON` |
| `after_reap` | free reaped address spaces | `kernel` | `child::collect` | same contract |
| console | the platform UART | `console` | COM1 | PL011 |
| root fs | ext2 on the first virtio-blk | `disk` | virtio-mmio | virtio-mmio |
| NIC | the first virtio-net, DHCP | `net` | virtio-mmio | virtio-mmio |

Without a feature the builder method is absent, and the matching override takes the root's own function. The
`process`, `shell` and `net` features forward to `akuma-runtime-tokio`'s.

## Steps

Moves first, amd64 only, Taoxichuan being the root that shrinks. Each step passes the TCG battery
(`scripts/kvm/taoxichuan/tcg_rig.py`); the steps that touch exec, fork or memory also pass `cow_leak_gate` on Pop.

| step | what | gate |
|---|---|---|
| B15 | the two crates, empty builder; `console` moves (COM1 bring-up, print hook, `console_stdio_write`) | TCG battery |
| B16 | `disk` moves (`blk_init`, `Vda`, the mount, `ROOT_FS`, `read_image`, `fd_inode`) | TCG battery |
| B17 | `kernel`, memory: task records, the memory arms, fault paths, copy-on-write, page-table clone/free, user copies | TCG battery, leak gate |
| B18 | `kernel`, procs and signals: `child.rs`, `exec_twin.rs`, `sched.rs`, `load_task`/`spawn`/`run_init` | TCG battery, leak gate, goose PONG |
| B19 | `kernel` cpu, `smp`, `net`, the clock/RNG defaults, `entry!`; `kmain` becomes the builder. **Taoxichuan's `src/` is one file of at most 100 lines** | all of the above, fixed point K1 == K2 |
| B20 | `akuma-tokio-kernel`'s README is the brief: the file above, `Cargo.toml`, `build.rs`, build and boot commands, overriding a default, the census loop. Tested by a fresh agent session given only the README, building a second root that boots on TCG | a stranger's root boots |
| B21 | `akuma-tokio-kernel-aarch64`, after B12 (AArch64's slot table behind the interface the x86 kernels use, which is what lets `kernel` drive both). The same `main.rs` builds for `aarch64-unknown-none` on QEMU virt. Its own proposal when it starts | the census programs run on aarch64 |

## What moves stays the same code

B15–B19 relocate code; they do not rewrite it. About 5,800 lines leave `taoxichuan/src` and arrive in
`akuma-tokio-kernel-amd64`, so the tree's line count barely changes. What the move buys is the boundary: a second root
can use it, and B20 and B21 test exactly that. Taoxichuan's `unsafe` budget (33 sites in `taoxichuan/src`) moves
with the code by `scripts/check_unsafe_budget.py --update`, net zero.

The `unsafe` pass that the cleanup plan put after B14 moves to after B19. The reason the plan gave for waiting (the
steps move state) applies again.

## Mechanics to settle in B15

- **Linker script.** A dependency's `rustc-link-arg` does not reach the binary, so the arch half ships `linker.ld`
  and exports its path through `links` metadata; the root's `build.rs` is the three lines that pass it on.
- **`#[global_allocator]` and `#[panic_handler]`** expand in the root through `entry!`, so neither arch crate defines
  one and both stay buildable in a test harness.
- **Sizes** (`NSLOTS`, `MAX_CPUS`, the mmap slab, the 256 MiB `brk` window) stay constants in the arch half for now.
- **Workspace.** Both arch halves stay out of `default-members`, as Taoxichuan is.

## Background

[`TAOXICHUAN_CLEANUP_PLAN.md`](TAOXICHUAN_CLEANUP_PLAN.md) (phase 3, B11),
[`B12_AARCH64_SLOT_TABLE.md`](B12_AARCH64_SLOT_TABLE.md),
[`crates/akuma-runtime-tokio/README.md`](../../crates/akuma-runtime-tokio/README.md).
