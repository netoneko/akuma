# B21: Taoxichuan on aarch64, and a smaller audit (proposal, 2026-11-04)

**Status: in progress** - [`docs/archive/TAOXICHUAN_B21_AARCH64.md`](../docs/archive/TAOXICHUAN_B21_AARCH64.md) is the record (A0, A1 done 2026-11-04). The step B21 of [`B15_TOKIO_KERNEL_KIT.md`](B15_TOKIO_KERNEL_KIT.md) reserved
("its own proposal when it starts"), plus a second track that reduces the image's audit lines. The two are one plan
because the largest audit cut available is the same work as the port: code that becomes arch-neutral is counted once
for two kernels instead of once per architecture.

## Where things are (measured 2026-11-04, `e80e6cd9`)

- **B15–B20 are done.** `taoxichuan/src/main.rs` is 18 lines: one `Kernel` chain. The machine is
  `crates/akuma-tokio-kernel-amd64` (7,013 lines: `lib.rs` 3,286, `child.rs` 1,322, `smp.rs` 560, `exec_twin.rs` 501,
  `lazy.rs` 434, `net.rs` 402, `shmpages.rs` 245, `sched.rs` 156, `console.rs` 69, `ring.rs` 38).
  `akuma-tokio-kernel` (44 lines, `forbid(unsafe_code)`) re-exports `Kernel` only under
  `cfg(all(target_arch = "x86_64", target_os = "none"))`. Shanghai (B20's stranger test) passed all six gates on the
  metal.
- **B12 is not started** ([`B12_AARCH64_SLOT_TABLE.md`](B12_AARCH64_SLOT_TABLE.md)). AArch64's scheduler is still
  `akuma-threading`'s default path, not behind `X86ArchHooks`.
- **`akuma-runtime-tokio` is x86_64-shaped.** `dispatch` decodes with `Syscall::from_x86_64` and falls through to
  `akuma-syscalls-glue-amd64`. The allow-list itself is `Syscall` values, which are already arch-neutral.
- **The AArch64 pieces exist as crates**, used by the full kernel today: `akuma-entry` (boot asm, secondary
  trampoline, linker symbols), `akuma-uart` (PL011), `akuma-gic`, `akuma-timer` (CNTV), `akuma-psci`,
  `akuma-trap-aarch64`, `akuma-el0-entry`, `akuma-mmu` (AArch64 tables), `akuma-fdt`, and the in-tree virtio
  (`akuma-virtqueue` + `akuma-virtio`), which already drives virtio-mmio on QEMU virt.
- **Audit** (`python3 scripts/cloc_akuma.py --audit taoxichuan`): **78,823** lines in the runtime image. Workspace
  56,949 in 76 crates, third-party 21,861 in 15 crates, the kernel 13. Largest: smoltcp 11,030, glue 10,770,
  akuma-exec 5,254, `akuma_tokio_kernel_amd64` 4,306, heapless 3,872, akuma-threading 3,146, akuma-ext2 2,906,
  akuma-net 2,448, vfs-glue 2,265, lock_api 2,061.

## Goal

1. `cargo build -p taoxichuan --release --target aarch64-unknown-none` builds from the **same** `main.rs`, `Cargo.toml`
   and `build.rs`, boots on QEMU `virt` (TCG, and HVF on the Mac), and serves ssh, busybox and tokio programs.
2. The machine half is split into what is arch-neutral and what is not, so the aarch64 half is the arch code only,
   not a second copy of the memory arms, the process model, signals or `dispatch_inner`.
3. Each kernel's image audit goes down, and the union over both arches is reported alongside it.

## Shape after

```text
   taoxichuan/src/main.rs   (unchanged, 18 lines)
        |
   akuma-tokio-kernel       forbid(unsafe). entry!, Kernel, the builder: unchanged API
        |
        +--> akuma-tokio-kernel-core      NEW. arch-neutral machine logic: task records, the mmap/brk/
        |                                 mprotect/munmap/msync/madvise arms, the dispatch order, fork/
        |                                 exec/exit/threads through akuma-exec, signal delivery decisions,
        |                                 the disk/net defaults, cmdline tokens. Reaches the CPU only
        |                                 through a `Machine` trait.
        +--> akuma-runtime-tokio           arch-neutral: takes a decoded `Syscall`, not an x86 number
        +--> akuma-tokio-kernel-amd64      impl Machine for amd64: GDT/IDT/TSS, LAPIC, PVH, ring3,
        |                                 x86 sigframe, x86 page-table effects, APs
        +--> akuma-tokio-kernel-aarch64    impl Machine for aarch64: akuma-entry, PL011, GIC, CNTV,
                                          PSCI, EL0 entry/trap, aarch64 sigframe, DTB virtio discovery
```

`akuma-tokio-kernel-core` is a separate crate rather than more code in `akuma-tokio-kernel` because it will hold
`unsafe` (the task table, page-table effects through the trait's callers) and the top crate keeps its `forbid`. If the
census in step A0 shows the neutral code holds none, it goes into `akuma-tokio-kernel` and the extra crate is not made.

The `Machine` trait is the seam. Its first draft, from reading `lib.rs` and `child.rs`, to be confirmed by A0:
page-table map/unmap/protect/walk for a user root, the user copies, the signal frame build and `rt_sigreturn`
restore, the user register file (`clone` child setup, `execve` entry), the switch hooks `akuma-threading` already
takes, wake IPI and TLB shootdown, the console byte sink, the clock and RNG defaults, and boot (memory map, command
line, device discovery). The pattern is the tree's existing one: the core decides, the machine performs.

## Track 1: the port

Each step is gated; the amd64 gates (TCG battery via `scripts/kvm/taoxichuan/tcg_rig.py`, `cow_leak_gate` delta 0,
tokioprobe at `smp=1` and `smp=4`, census 76/0 unserved) run on every step, because every step but A7 moves amd64
code.

| step | what | gate |
|---|---|---|
| A0 | **Census, move nothing.** Classify every item in `akuma-tokio-kernel-amd64` as neutral, arch, or neutral-after-a-hook, with line counts. Draft the `Machine` trait from the third column. Decide whether `-core` holds `unsafe` | a table in this doc |
| A1 | **`akuma-runtime-tokio` goes neutral.** `dispatch(call: Syscall, args)`; the x86 decode and the `akuma-syscalls-glue-amd64` fallthrough move to the amd64 half. The allow-list tests stay where they are and still pin the census | host tests; amd64 gates |
| A2 | **`akuma-tokio-kernel-core` and the `Machine` trait**, amd64 implementing it. Move the neutral items in the order A0 gives, smallest blast radius first: cmdline and identity, disk/net defaults, the memory arms, then `child.rs`/`exec_twin.rs` | amd64 gates; leak gate on the memory and process moves; goose PONG after `child.rs` |
| A3 | **`dispatch_inner` through glue** (the cleanup backlog's S6). It is 400+ lines of local arms in `lib.rs`; whatever glue already serves is deleted, the rest becomes core arms. This must happen before A5, or the aarch64 half gets a second copy | amd64 gates; census 76/0 |
| A4 | **The hooked scheduler path on aarch64** (revised by A0's census: B12 is not needed). The tokio kernel schedules through `akuma-threading`'s hooked path (`x86_claim_slot` .. `x86_finish_current`, `X86ArchHooks`), which is arch-neutral logic behind `cfg(target_arch = "x86_64")` plus a switch, a first-run frame and `Context`'s layout. It builds on aarch64 behind a feature with an aarch64 switch; the full AArch64 kernel's SGI scheduler is untouched | `akuma-threading` host tests; AArch64 full-kernel boot suite unchanged (feature off); the tokio half's own boot (A5) |
| A5 | **`akuma-tokio-kernel-aarch64`**, one core: boot through `akuma-entry`, PL011 console, GIC + CNTV tick, EL0 entry and traps, page tables through `akuma-mmu`, virtio-mmio found in the DTB (`akuma-fdt`), ext2 root, `init=`. `entry!` emits the aarch64 entry symbol under `cfg(target_arch)` (expanded in the kernel crate, which only ever builds for its target, so the host-test trap does not apply) | boots to `init` on QEMU virt TCG; busybox `sh -c` through ssh |
| A6 | **aarch64 signals, `net`, `smp`**: the aarch64 signal frame, virtio-net + DHCP, secondaries via PSCI `CPU_ON` | tokioprobe x2 at `smp=1` and `smp=4`; sigprobe rungs; census programs built for aarch64 musl, 0 unserved |
| A7 | **Fixed point on aarch64.** The aarch64 Taoxichuan builds itself in the guest (nightly musl aarch64 toolchain on the image), K1 == K2. Then HVF on the Mac | K1 == K2; HVF boot |

Rig work alongside: `tcg_rig.py` gets an aarch64 mode (QEMU `virt`, `-global virtio-mmio.force-legacy=false`, the same
battery), and the census programs (`tokioprobe`, `nettest-reqwest`, `kot`, goose) get aarch64 musl builds.

### Risks

- **HVF's `ISV=0` assertion.** Any MMIO store the optimizer turns into a writeback form kills QEMU under HVF. The aarch64
  half uses `akuma-gic`'s and `akuma-uart`'s accessors, never `write_volatile` on a device address.
- **AArch64 has no sigframe in the x86 kernels' shared code.** `akuma-syscalls-glue-amd64::kill` is x86-only. The
  delivery *decisions* (`SIG_IGN`, default-fatal, `SA_RESTART`, never-restart set) move to core in A2; only the frame
  layout is per arch. The full AArch64 kernel's frame code is the reference.
- **Feature unification.** A4's feature changes `akuma-threading` for every package in one `cargo` invocation that
  enables it. Taoxichuan is not in `default-members` and the full AArch64 kernel is built on its own, so the two
  never share a build; a check that the feature is off in the full kernel's `cargo tree` keeps it that way.
- **`akuma-exceptions` is welded to `akuma-exec`.** The aarch64 half does not use it; it takes `akuma-trap-aarch64`'s
  `FrameHandle` and writes its own fault paths, as amd64 did with `akuma-trap-amd64`. If A0 finds that duplicating the
  CoW break is the result, that code moves into core instead.

## Track 2: fewer audit lines

Runs in parallel with track 1; each row is independent and measured with `cloc_akuma.py --audit` before and after.
Numbers are today's audit lines for the crate; the saving is an upper bound until the row is done.

| row | candidate | audit lines | what it takes |
|---|---|---|---|
| R1 | **The neutral split itself** (A2, A3). Counted per kernel it moves lines; counted as the union over both kernels it avoids a second copy of about the neutral half of 7,013 | A0 gives it | track 1 |
| R2 | **`log`** | 799 | 13 files in akuma-exec, akuma-mmu, glue and akuma-syscalls-time use `log::` macros. Replace with `safe_print!` (the console rule already asks for it) or a five-line in-tree macro; drop the dependency |
| R3 | **`lock_api`** | 2,061 | Not a one-line cut: it arrives through talc's default features, through `spinning_top` (akuma-alloc) **and** directly from akuma-exec. Needs akuma-alloc and akuma-exec on `akuma-bkl`'s spinlocks, then talc with `default-features = false`. Measure the lock A/B (`scripts/benchmarks/locks_rw_ab.sh` pattern) before taking it |
| R4 | **The tty/pty stack without `shell`** | akuma-tty 1,189 + glue's pty arms | B11 kept `pty` because no kernel without a shell existed. Taoxichuan `--no-default-features` is that kernel now. Put `akuma-tty` and glue's pty arms behind `shell`; measure what the no-`shell` image loses |
| R5 | **AF_UNIX as its own tokio feature** | akuma-net-unix 710 | Cleanup analysis row 9, set aside earlier. Tokio's `net` includes `UnixStream`, so it is a separate `unix` feature in `akuma-runtime-tokio`, on in Taoxichuan's default |
| R6 | **Glue's unreached arms** | part of 10,770 | Dead-code elimination does not remove arms behind one runtime `match` (`AKUMA_SYSCALLS_TOKIO_DEP_ANALYSIS.md`). Run `cloc_akuma.py --by-file` over glue for the Taoxichuan build, map each arm family against B10's 76-call census, and gate the families no census program or busybox makes |
| R7 | **DHCP** | smoltcp's dhcpv4 feature + `net.rs` client | Cleanup analysis row 11, set aside earlier. QEMU user networking and Akuma's hypervisor both hand out a known address; `ip=` on the command line, DHCP behind a feature on by default. Small; do it only if R6 shows smoltcp's DHCP is a real share of the 11,030 |

Not on the list, and why: heapless (3,872), managed (777) and byteorder (698) come in with smoltcp unconditionally, so
only replacing smoltcp moves them; that is a separate decision, not a trim.

Report after each row: Taoxichuan amd64 image audit, Taoxichuan aarch64 image audit (from A5 on), and their union.
A target to aim at, not a promise: amd64 under 75,000 after R2–R6.

## Open questions

1. **Firecracker aarch64** in scope for A5–A7, or QEMU `virt` only? The full AArch64 kernel already boots under
   Firecracker, so the DTB path is known; it adds a rig, not code.
2. **`-core` crate name.** `akuma-tokio-kernel-core` follows the kit. If A0 shows it carries no `unsafe`, it folds
   into `akuma-tokio-kernel` and the question goes away.
3. **R3 (`lock_api`)** touches the allocator lock every allocation takes. Worth 2,061 lines, or leave it until a lock
   change is wanted for another reason?
4. **Is the full AArch64 kernel a consumer of the core?** Out of scope here. The `Machine` trait should not rule it out,
   but it should not be designed for it either.

## Background

[`B15_TOKIO_KERNEL_KIT.md`](B15_TOKIO_KERNEL_KIT.md), [`B12_AARCH64_SLOT_TABLE.md`](B12_AARCH64_SLOT_TABLE.md),
[`TAOXICHUAN_CLEANUP_PLAN.md`](../docs/archive/TAOXICHUAN_CLEANUP_PLAN.md) (status table, rows B15 and the dependency
analysis rows), [`TAOXICHUAN_B10_SYSCALL_CENSUS.md`](../docs/archive/TAOXICHUAN_B10_SYSCALL_CENSUS.md),
[`AUDIT_SURFACE_METHODOLOGY.md`](../docs/archive/AUDIT_SURFACE_METHODOLOGY.md),
[`AKUMA_VIRTIO_IN_TREE.md`](../docs/archive/AKUMA_VIRTIO_IN_TREE.md).
