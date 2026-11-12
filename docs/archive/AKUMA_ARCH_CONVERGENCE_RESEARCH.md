# Arch convergence: one source tree for the two full kernels

**Date:** 2026-11-09. **Branch:** `akuma-convergence-research`.
**Scope:** research only — no code moved, no build run. The question: the AArch64
kernel (`src/`) and the amd64 kernel (`amd64/`) have different bring-up and, despite
80+ shared crates, still each own a large private half. The Taoxichuan kernel
(`taoxichuan/` over `crates/akuma-tokio-kernel*`) builds one `main.rs` for both
`x86_64-unknown-none` and `aarch64-unknown-none`. This report reads how it managed
that, measures where the two full kernels actually stand, and classifies the remaining
work into **extract** (neutral logic into shared crates), **share** (already-shared
code both must consume), and **per-platform** (crates that stay arch-split). KVM is
the assumed test bench for the merge.

Related, read first: [`REDUCING_PLATFORM_DEPENDENCY.md`](REDUCING_PLATFORM_DEPENDENCY.md)
(the seam audit), [`AKUMA_AMD64_STREAMLINING.md`](AKUMA_AMD64_STREAMLINING.md) (the
amd64 bin's duplicate inventory), [`TAOXICHUAN_AMD64_EXTRACTION_PLAN_GLM.md`](TAOXICHUAN_AMD64_EXTRACTION_PLAN_GLM.md)
(the taoxichuan×amd64 extraction plan, slices 1–5 done),
[`docs/archive/B21_TOKIO_KERNEL_AARCH64.md`](B21_TOKIO_KERNEL_AARCH64.md)
+ [`TAOXICHUAN_B21_AARCH64.md`](TAOXICHUAN_B21_AARCH64.md) (the dual-arch machine-half
split, in flight).

---

## 1. How Taoxichuan actually did it

Worth being precise, because "taoxichuan merged the two arches" is not what happened.
It never merged two existing kernels — it **converged one minimal kernel onto the
shared crates**, then split what remained along an arch seam. Four moves, in order:

1. **Build on the shared stack, not beside it.** Taoxichuan's process model is
   `akuma-exec`, its syscall decisions are `akuma-syscalls-glue` + the
   `akuma-syscalls-*` decision crates, its scheduler is `akuma-threading`'s hooked
   path. There was never a second implementation of any of it to reconcile.
2. **Move the machine half into a crate** (B15): `taoxichuan/src/` (10,491 L) became
   `akuma-tokio-kernel-amd64` (7,013 L) plus an 18-line `main.rs`.
3. **Split the neutral half out** (B21 A0): a line-by-line census classified the
   machine half as **arch ~2,500 L / neutral ~4,500 L** — about two thirds of what a
   kernel bin "owns" is policy that never names the machine. The neutral part moved
   to `akuma-tokio-kernel-core` (`#![forbid(unsafe_code)]`).
4. **Make the seam a family of small traits, not one mega-trait.** The proposal's
   `Machine` trait became `BklCpu`, `UserPages`, `FreshPage`, `Mm`, `Loader`,
   `UserTree`, `Proc`, `Deliver`, `NetMachine` in `-core` — each the exact surface one
   neutral module needs, so an aarch64 half implements only what it can already do.
   The same philosophy already existed in the tree: `akuma-sched-amd64::Platform`
   (the shared switch bodies vs one kernel's facts) and `akuma-threading`'s
   `X86ArchHooks`.

Then the plumbing that makes one source build both targets:

- **`akuma-tokio-kernel`** (44 L, `forbid`) is the arch-neutral front: it re-exports
  `Kernel` from the amd64 or aarch64 machine half under
  `cfg(all(target_arch = "...", target_os = "none"))`, and defines `entry!` per arch
  (amd64: `kmain`/`kmain_mb2` for PVH/multiboot2; aarch64: `rust_start(dtb)` +
  `secondary_shared_start` for PSCI-started cores). A kernel root never writes a
  `#[cfg]`.
- **Features are devices.** `console`, `disk`, `net`, `smp`, `shell` exist with the
  same names in both machine halves, so the root's builder chain
  (`kernel.console().disk().net().smp().run_init()`) is byte-identical for both
  targets. The aarch64 half's Cargo.toml says it outright: "The same switches as the
  amd64 half, so one main.rs builds for both."
- **Boot is shared where it can be.** The aarch64 half boots through
  `akuma-boot-aarch64::_boot` — the *same instructions the full AArch64 kernel runs*
  (moved there in B21 A5.2). `akuma-boot-amd64` was extracted for the same reason and
  `akuma-amd64` itself now links it: "one boot block, two kernels, no drift."
- **Gates per move, candidate vs HEAD.** Every step ran the amd64 battery
  (`tcg_rig.py`), the leak gate, probes at smp=1/4 — under the *same accelerator* on
  both sides of the comparison.

**Where it stands (2026-11-09):** the amd64 side is finished to the fixed point
(self-hosting K1 == K2, battery green). The aarch64 machine half exists and boots
through the shared `_boot`, but only `console` does anything yet — disk, net, SMP,
EL0 and init are B21 A5.3→A7, still open. So the *pattern* is proven; the aarch64
half of the proof is mid-flight. Everything in this report leans on the pattern, not
on A5.3 being done.

**What taoxichuan does not prove:** it runs a narrow surface (no USB, no wifi, no
fbcon, no rump, no kvm0, no containers, `shell` optional). The full kernels are ~5×
the image audit and carry subsystems the tokio kit has never had to make neutral.
That residual surface is exactly what §3 classifies.

---

## 2. Where the two full kernels stand (measured today)

### 2.1 The AArch64 kernel (`src/`)

The bin is **281 lines**: `#[global_allocator]`, `#[alloc_error_handler]`,
`#[panic_handler]`, `rust_start`, boot-test hook registration — and re-exports.
Every byte of production logic lives in crates: the neutral spine in
`akuma-kernel-core` (console, config, fs, timer, irq, pmm, klog, the machine table)
and `akuma-kernel-glue` (`kernel_main`, boot ordering, halt, memory layout math —
both `#![forbid(unsafe_code)]`), the process model in `akuma-exec`, the syscall
decisions in `akuma-syscalls-glue` + the `akuma-syscalls-*` family, the machine bits
in per-arch crates (§3.3). 43 `akuma-*` dependencies.

Its one real machine table is `akuma-kernel-core::platform`: compile-time
machine modules (`qemu_virt` / `firecracker`, feature-switched) for the constants
needed before the FDT is readable, FDT-derived refinement after, and a
`bootstrap_device_map` → `install_fdt_device_map` flow. This is the shape an amd64
machine description should take (§3.2, M2).

### 2.2 The amd64 kernel (`amd64/`)

The bin is **53,787 lines** and still owns its kernel-proper. The big files, with
what they actually are:

| file | lines | what it holds | classification |
|---|---:|---|---|
| `usermode.rs` | 9,284 | ring-3 entry, the syscall dispatcher (two-table: legacy fold + glue), process slots (`PROCS[128]`), fork/exec/spawn/wait policy | **mostly extractable** — the `akuma-exec`-shaped half |
| `fd.rs` | 3,819 | fd effects over the *registered* `SharedFdTable` (`akuma-exec::process`) — already the same authority as AArch64 | **shared core, local effects** |
| `mm.rs` | 3,522 | region records, lazy fill, CoW glue, the mmap arms | **extractable** — decisions already in `akuma-mmap`/`akuma-user-tree`, amd64's tree walk deliberately not switched (GLM slice 3) |
| `sched.rs` | 2,631 | the x86 machine half of the scheduler over `akuma-threading` (`X86ArchHooks`, FxArea, CR3/TSS/FS-GS) | **per-arch by design** (its `Platform` seam is the model) |
| `xhci.rs` | 2,532 | USB wiring over `akuma-xhci`/`akuma-usb`/`akuma-usb-storage`/`akuma-dma` | neutral protocol already crate'd; wiring stays |
| `kvm.rs` + `kvm/*` | 8,649 | kvm0: the SVM hypervisor host (VMCB, NPT, MMIO decoder, `/dev/kvm0`, relay, tun, ctl, Linux/Akuma guests) | **amd64-only subsystem** (AMD-V is an ISA fact); `akuma-kvm-abi` is already the shared spec, and the relay/tun/ctl halves are candidates for later neutral extraction |
| `idt.rs` | 1,811 | trap dispatch over `akuma-trap-amd64` + `akuma-idt-amd64`, fault paths (CoW break, stack growth) | thin per-arch + one neutral fault-policy seam |
| `rtw89_sta.rs` / `rtw89.rs` | 2,379 | wifi station glue over `akuma-rtw89`/`akuma-ieee80211`/`akuma-wpa` | neutral stacks already crate'd |
| `multiboot2.rs` | 1,419 | the GRUB/bare-metal boot path (`kmain_mb2`) | **extractable** — see the divergence finding below |
| `signal.rs` | 1,399 | x86 signal *delivery* (rt_sigframe, FPU image) over `akuma-syscalls-glue` mask algebra + `akuma-syscalls-signal::procmask` | delivery is per-arch by design; decisions already shared |
| `net.rs` | 1,358 | NetRuntime hooks, TSC-vs-PIT clock, RDRAND, netpoll/DHCP glue over `akuma-net` (+ rtl8169 feature) | hook bodies neutral, clock/RNG arch |
| `hda.rs`, `uas.rs`, `nvme.rs`, `touchpad.rs`, `kbd.rs`, `serial.rs`, … | ~5,000 | device wiring over the matching crates | wiring stays per machine |
| `lapic.rs`, `smp.rs`, `boot.rs`, `loader.rs`, `fs.rs`, `sock.rs`, `futex.rs`, `uaccess.rs`, `thread.rs`, `exec_runtime.rs` | ~7,200 | SMP bring-up, ELF→address-space, mount policy, socket state machines, futex effects, uaccess, exec/thread runtime tables | mixed — the extract/share seam below |

It already consumes **82 `akuma-*` crates**, including the big shared ones the old
docs said it lacked: `akuma-exec` (205 call sites — the `PROCS[128]` array survives
*beside* it, not instead of it), `akuma-threading` (hooked path), `akuma-syscalls-glue`
(`smoltcp,exec,akuma-native` features) + `akuma-syscalls-glue-amd64`,
`akuma-syscalls-{sync,poll,mem,net,linux,proc}`, `akuma-user-access`, `akuma-tty`,
`akuma-fdtable`, `akuma-ext2`, `akuma-vfs(-glue)`. The divergence that remains is
**adoption depth, not existence**: `akuma-net-unix`, `akuma-kernel-core`,
`akuma-kernel-glue`, `akuma-exceptions` are not consumed.

### 2.3 What is genuinely still duplicated

Less than the line counts suggest, but the load-bearing pieces are the worst ones:

1. **The process lifecycle** — `usermode.rs`'s `PROCS[128]` fork/exec/wait/spawn
   policy vs `akuma-exec`'s slot table. `REDUCING_PLATFORM_DEPENDENCY` §9.2 flagged
   this in September; `akuma-exec` adoption since then (205 call sites, C2 step 4b's
   registered `SharedFdTable`, the wait4 thread-group fix routed through glue) has
   eaten into it from both ends, but the flat array is still the authority for
   process slots on amd64.
2. **The boot spine** — `kmain` (PVH) vs `kmain_mb2` (GRUB/metal) are no longer
   parallel: different memory init, different machine facts, different block/mount/
   net steps, and **two hand-maintained self-test registration lists** (`AKUMA_AMD64_STREAMLINING`
   §1). This is the machine that manufactures "works under QEMU, broken on the HP
   box", and it is the first thing the merge should fix because everything after it
   assumes one boot order exists.
3. **The scheduler's AArch64 half is not hooked** — B12 left the full AArch64 kernel
   on `akuma-threading`'s default path while amd64 runs the `X86ArchHooks` path.
   Neutral scheduler logic exists once; the *hooked* path's non-x86 arms are exactly
   what B21 A4 added behind a feature. Convergence here is mostly "switch the
   default", plus an aarch64 `Platform` impl.
4. **Syscall arms with local bodies** — the two-table dispatcher sends most calls to
   glue but keeps local arms (sockets against `fd.rs`'s socket kind, mmap against
   `mm.rs`, getdents cursor, pty gating). Each local arm is a place the two kernels
   can drift; the GLM plan's remaining items (sockaddr_un/setsockopt decode tables;
   amd64 adopting `akuma_syscalls_signal::check` for `rt_sigaction` — recorded as a
   pinned divergence because glue is looser) are this bucket.
5. **AF_UNIX** — amd64 has no `akuma-net-unix`; its unix-socket marshalling lives in
   `sock.rs`. The AArch64 kernel's `akuma-net-unix` (2,903 L) is the only state
   machine; B21 track 2 R5 already planned it as a feature.
6. **Fault paths** — `akuma-exceptions` (welded to `akuma-exec`) vs `akuma-trap-amd64`'s
   `Trap` handlers. B21's risk list says it plainly: if both halves end up duplicating
   the CoW break, that policy moves into a neutral core.

---

## 3. The classification

### 3.1 Extract — neutral logic out of `crates/akuma-kernel-amd64/src/` into the shared crates

Ordered by leverage; each item names the crate that already owns the decisions, so
extraction is *adoption + moving effects*, not design work.

| # | from | to | note |
|---|---|---|---|
| E1 | `boot.rs` + `multiboot2.rs` (two spines) | one `boot::sequence(&MachineFacts)` in `akuma-boot-amd64`; the two `kmain`s reduce to "collect facts, call it" | The `STREAMLINING` §1 fix shape. Self-test registration becomes one list gated by capability bits (`Caps: disk/net/fb/smp`), so KVM boots the same suite metal does |
| E2 | `usermode.rs` process policy (spawn/wait4/lifecycle over `PROCS[128]`) | `akuma-exec` (the slot table both kernels already trust elsewhere) | The big one, ~4–5 K L. Do it in slices the way `TAOXICHUAN_SWITCH_TO_CRATES` did (S1–S7), never whole. `wait4`-thread-group semantics are already fixed in glue — deleting the local arms deletes the class of bug |
| E3 | `mm.rs` region/hole/lazy policy | `akuma-mmap` (records, `HoleSet`), `akuma-user-tree` (`plan_clone`/`plan_free` — amd64's walk was deliberately left unswitched in GLM slice 3; switching it is the overdue half of that slice) | Walkers and PTE writes stay in `akuma-pagetable`/`akuma-pte-amd64` |
| E4 | `sock.rs` remaining marshalling + `fd.rs` socket kind | `akuma-syscalls-net` (sockaddr_un, setsockopt tables — the S4 leftovers), `akuma-net-unix` adoption for the AF_UNIX state machine | Differential host tests pin old-vs-new, as slice 4 did for `sockaddr_in` |
| E5 | `net.rs` DHCP/netpoll/NIC-pump policy | `akuma-net` / `akuma-net-nic` (the pump and queued ring are already there) | TSC calibration and RDRAND stay arch-side |
| E6 | `loader.rs` ELF→address-space | `akuma-elf-exec` | The "one loader" rule (`TAOXICHUAN_B8_ONE_ELF_LOADER.md`); AArch64's loader is the reference |
| E7 | `fs.rs` mount/root policy | `akuma-vfs-glue` | Mirrors what `kernel_main` does on AArch64 |
| E8 | `console.rs` serial-as-ProcessChannel | fold into the console/tty family (`akuma-comconsole` shape) | Both kernels then register the same `Stdin`/`DevTty` arms |
| E9 | `idt.rs` fault *policy* (CoW break, stack growth, UFFD-ish decisions) | neutral fault-policy module (into `akuma-kernel-core` or a new `akuma-faults`), with `akuma-trap-amd64`/`akuma-trap-aarch64` handing it decoded faults | Prevents the B21 "duplicated CoW break" risk; the two trap crates keep entry asm and frame types |

House rules apply unchanged (CLAUDE.md §Layout): new crates are
`akuma-<thing>`, `-amd64`/`-aarch64` suffix only for arch halves, `forbid(unsafe_code)`
from the first commit at unsafe-budget 0 (`scripts/check_unsafe_budget.py --update`
ratchets down only), host tests first, effects returned not performed, divergences
pinned as named enums with a test per value (`SharedSpans::{Copy, ShareFrames}` is
the precedent). amd64's production `unsafe` sits at 57 sites and must ratchet down,
not up — E1–E9 all move `unsafe` call sites into crates behind checked functions.

### 3.2 Share — already-shared code both kernels must consume

These exist; convergence is finishing adoption and keeping one authority:

- **`akuma-exec`** as the only process/fd/threading model. `SharedFdTable` is already
  the registered fd authority on *both* kernels (amd64 `fd.rs` step 4b). E2 finishes
  the process side.
- **`akuma-syscalls-glue`** as the only syscall decision layer, with the per-arch ABI
  decode in the per-arch halves (`akuma-syscalls-glue-amd64` today; B21 A1 moved the
  tokio kit to a decoded arch-neutral `Syscall`, which is the shape a glue-aarch64
  fold would take). The two-table dispatcher stays per kernel — it is the
  arch ABI fold — but every arm body it calls should be crate code.
- **`akuma-threading`** with the hooked path as the default scheduler shape on both
  arches (B21 A4 did the feature work; the full AArch64 kernel switching over is a
  separate, gated slice — its SGI scheduler is behaviour pinned by the boot suite).
- **`akuma-kernel-core` + `akuma-kernel-glue`** as the neutral kernel spine — console
  rules, config, timer policy, irq table, pmm wrappers, klog, and `kernel_main`'s
  boot ordering. amd64 consumes none of it today. This is the strategic one: it is
  where a merged kernel's *neutral full-kernel logic* lives, and it was extracted
  from the AArch64 bin, so its shape is already bin-agnostic. Adoption requires
  generalizing `platform.rs`'s machine table into the `MachineFacts` seam (M2 below)
  and adding the pieces `kernel_main` assumes but amd64 supplies differently
  (framebuffer policy, rump proxy — see open questions).
- **Everything that is already boringly shared**: `akuma-primitives`, `akuma-alloc`,
  `akuma-pmm`, `akuma-bkl`, `akuma-dma`, `akuma-mmap`, `akuma-ext2`, `akuma-vfs`,
  `akuma-tty`, `akuma-fbcon`, `akuma-net`, `akuma-elf`, `akuma-procfs`,
  `akuma-isolation`, the `akuma-syscalls-*` decision family, `akuma-dmesg`,
  `akuma-selftest`. No work — just the rule that neither bin grows private copies
  (`AMD64_CRATE_REUSE_AUDIT.md` is the failure mode to reread before writing any).

### 3.3 Per-platform — crates that stay arch-split

The pairs already exist and the naming convention holds; the table is the target
shape, with the gaps named:

| concern | amd64 | aarch64 | note |
|---|---|---|---|
| boot entry/asm | `akuma-boot-amd64` (`boot.s`, PVH+MB2 trampolines, physmap, kernel paging, mem bring-up) | `akuma-boot-aarch64` (`_boot`, early page tables, `secondary` trampoline, `platform-firecracker` map) | both linkable by any bin — the fixed-point trick |
| entry C ABI | `kmain`/`kmain_mb2` via `entry!` | `rust_start(dtb)` via `entry!` | neutral front crate emits |
| trap entry/frames | `akuma-trap-amd64` + `akuma-idt-amd64` | `akuma-trap-aarch64` (+ `akuma-exceptions` for `akuma-exec`'s fault contract) | E9 pulls the *policy* out; frames/asm stay |
| PTE encoding | `akuma-pte-amd64` | `akuma-pte-aarch64` | plus `akuma-pagetable`'s per-arch modules |
| MMU walkers | `akuma-pagetable::amd64` | `akuma-pagetable::aarch64` / `akuma-mmu` | `DevRegion`/`set_device_map` stays neutral (REDUCING §1/§2) |
| user entry (ring3/EL0) | `akuma-ring3-amd64` (`sysretq`) | `akuma-el0-entry` (`eret`) | syscall *numbers* fold via `akuma-syscalls-abi::legacy` on amd64 — the ABI difference lives exactly here |
| FPU state | `akuma-fpu-amd64` (policy) + `akuma-sched-amd64::fx` (asm) | FP/SIMD save in `akuma-trap-aarch64` frames | policy crate is shared-shape, asm is arch |
| interrupt controller | `akuma-lapic-amd64` (+ ioapic in the kvm0 guest model) | `akuma-gic` | |
| timer | LAPIC TSC-deadline + PIT calib | `akuma-timer` (CNTV) | `akuma-kernel-core::timer` is the neutral policy |
| SMP bring-up | INIT-SIPI + MADT (`akuma-ryzen-amd64::acpi`) | PSCI `CPU_ON` (`akuma-psci`) | |
| TLB shootdown | IPI + `invlpg` (`akuma-boot-amd64` stub) | SGI + `TLBI …is` | REDUCING §3's "who" vocabulary is the open seam — an `invalidate(who, range)` neutral call with two impls closes it |
| machine discovery | `akuma-multiboot2` + `akuma-ryzen-amd64` (ACPI/HVM start info) | `akuma-fdt` + `akuma-firecracker::describe` | both feed the neutral `MachineFacts` (M2) |
| console device | COM1 via `akuma-port` | PL011 via `akuma-uart` | `akuma_primitives::console` hook stays neutral |
| hypervisor host | `akuma-svm-amd64` + `crates/akuma-kernel-amd64/src/kvm*` | — (AMD-V is an ISA fact) | `akuma-kvm-abi` already shared; an eventual aarch64 VHE backend slots in without re-shape |
| drivers that exist on one side only | xHCI, NVMe, HDA, rtl8169, rtw89, i2c-hid, touchpad | rump stack, `akuma-net-unix` | protocol cores are already neutral crates; only machine wiring is local |

---

## 4. End-state shapes, and the recommendation

- **Option A — two thin bins, shared everything.** Keep `src/` and `amd64/` as
  separate bin crates; drive both bins' private halves into the shared + per-arch
  crates (§3) until each bin is a `rust_start`/`kmain` stub plus its boot-test suite.
  *This is exactly how the AArch64 bin already looks, and how taoxichuan's bin looks.*
- **Option B — one bin crate, `#[cfg(target_arch)]` modules.** Rejected: it re-opens
  the tree's two documented cfg wounds — `target_os = "none"` matching
  `x86_64-unknown-none` (REDUCING §0, the bug that took down three quarters of the
  tree) and the `akuma-el0-entry` lesson ("`#[cfg(target_os = "none")]` is no longer a
  safe spelling anywhere in the tree"). It also puts both arches in one compilation
  unit graph, which fights `default-members` (arch bins must stay out; the AArch64
  default build must never reach amd64 asm) and the in-guest self-host build (nightly
  cargo features are a risk there). The front-crate-with-cfg pattern (Option C)
  gets the same benefit with the bin crates isolated.
- **Option C — a full-kernel kit.** The tokio-kit shape grown up:
  `akuma-kernel` (front: `entry!`, cfg-selected re-export of the machine half),
  `akuma-kernel-core`-amplified as the neutral policy spine, `akuma-kernel-amd64` /
  `akuma-kernel-aarch64` machine halves, two ~100-line bins. The AArch64 side is
  80% of the way (its bins' logic already lives in core+glue); the amd64 side gets
  there by executing §3.1.

**Recommendation: A as the method, C as the destination.** Do Option A's extraction
slices; when the amd64 bin is thin, rename/hoist rather than merge — the kit front
(`entry!` + cfg re-export) is ~50 lines and can arrive last. Do not start by creating
the kit crate: B15/B21 moved code into crates first and made the kit a thin witness
after, and that order is why it worked.

---

## 5. Ordered slices, each gated

Every slice: host `cargo test` on touched crates, `check_unsafe_budget.py` green,
docs touched in the same commit, **and the KVM gate below** (§6) — this is the first
convergence effort that has a fast hardware-accelerated bench for *both* its
endpoints' metal-equivalent paths.

| slice | content | gate beyond the standard battery |
|---|---|---|
| M0 | Census like B21 A0: classify every `crates/akuma-kernel-amd64/src` file arch/neutral/neutral-after-hook with line counts; freeze baseline battery outputs under KVM (candidate-vs-HEAD discipline comes from taoxichuan's rig) | a table in this doc's follow-up; KVM baseline of `amd64/run.sh` + Firecracker boot recorded |
| M1 | **One boot spine** (E1): `boot::sequence(&MachineFacts)` in `akuma-boot-amd64`, both `kmain`s become fact-collectors, one capability-gated self-test list | PVH (KVM/Firecracker) *and* MB2 (TCG grub image) boot the same suite; amd64 metal entry unchanged |
| M2 | **`MachineFacts` seam**: generalize `akuma-kernel-core::platform`'s machine-table idea into the neutral machine description both discovery paths fill (FDT on aarch64, PVH info/ACPI/MADT on amd64): RAM, console, timer, IRQ controller, SMP count, RNG, reboot conduit, disk/net presence | aarch64 kernel boots QEMU virt + Firecracker unchanged; amd64 unchanged; the seam has host tests |
| M3 | mm adoption (E3): amd64's tree walk switches to `akuma-user-tree` plans (the GLM slice 3 remainder); `mm.rs` policy arms to `akuma-mmap` | amd64 fork/CoW battery + `cow_leak_gate`-equivalent (delta 0) under KVM; aarch64 untouched |
| M4 | process model (E2) in `akuma-exec` slices: spawn, then wait4/lifecycle, then exec tables; `PROCS[128]` deleted last | amd64 box suite + selfhost build per slice; fork/thread/ssh stress under KVM; the wait4 thread-group semantics pinned by a host test |
| M5 | syscalls/net/loader/fs/console adoption (E4–E8), `akuma-net-unix` behind a feature (R5) | per-crate host tests; KVM sshd + `curl` + AF_UNIX probe gates |
| M6 | scheduler hook unification: AArch64 full kernel onto the hooked path behind a feature, amd64's `Platform` as the reference impl | aarch64 boot suite byte-identical with feature off; feature-on boots devbox; SMP probes both arches |
| M7 | **The kit arrives**: `akuma-kernel` front + machine halves hoisted from what core/glue/boot crates now hold; two ~100-line bins | both kernels' full gates: AArch64 (QEMU boot suite, devbox, Firecracker) and amd64 (KVM battery, box suite, selfhost fixed point K1==K2) |

Interleaving rule from the GLM plan applies: slices touch disjoint files by
construction, but M3/M4 both reach into `mm.rs`/`usermode.rs` — serialize those.

---

## 6. Testing on KVM

What "test it here on KVM" buys, concretely, per endpoint:

**amd64 (the merge's main beneficiary).** The kernel is a PVH guest, which is the
native coin of KVM:

- `sh amd64/run.sh` boots QEMU `-M microvm` — add `-enable-kvm` (and `-cpu host`)
  for the accelerated loop; TCG remains the slow-but-correct cross-check because
  `microvm` keeps the device model identical to Firecracker's (virtio-MMIO, modern
  rings, the PVH note — `run.sh`'s header documents why each flag is load-bearing).
- Firecracker itself: `amd64/run-firecracker.sh` + `overlays/devbox-firecracker/`
  gives the sshd-in-guest gates; Firecracker is KVM-only, so this path *requires*
  the KVM host and is the closest stand-in for the ryzen metal.
- The regression ladder precedent: the `AMD64_KVMTEST_*` phase dmesgs in this
  directory were produced by booting phase-by-phase under KVM (longmode → two-level
  → LAPIC → SMP → net → …). Reuse that shape for the merge: each slice M1–M7 boots
  under KVM and appends its phase log next to the baseline.
- The amd64 behavioural gates that must stay green throughout:
  `scripts/box_amd64_suite.py` (live ssh regression) and
  `scripts/amd64_selfhost_build.py` (in-guest kernel build) — both run inside the
  KVM guest, which is exactly why KVM access changes the merge's economics: the
  gates that used to need the ryzen now run on the bench.
- kvm0 (the SVM hypervisor under `crates/akuma-kernel-amd64/src/kvm*`) is unaffected by the merge and
  needs metal/nested-SVM; it is *not* part of the KVM gate matrix. Linux-KVM and
  kvm0 are different machineries and must not be conflated in gates or docs.

**aarch64.** On an x86 KVM host there is no aarch64 accelerator: QEMU TCG under
`-M virt` (the `scripts/cargo_runner.sh` shape — virtio-MMIO legacy-off, pinned
buses) is correct and slow; that is what the boot suite and devbox gates run under
today and it stays the aarch64 gate. If an ARM KVM host is added later, `-M virt
-accel kvm` and Firecracker-aarch64 (the tree already has its DTB dumps and the
`platform-firecracker` map) light up with no code change — B21's open question 1
already anticipated that rig. HVF on the Mac remains the other accelerated option.

**Both kernels, one bench rule:** candidate and HEAD run under the *same
accelerator* in the same session (taoxichuan's rig discipline — TCG smp=4 has a
known flake, and a KVM-vs-TCG comparison would manufacture phantom regressions).

---

## 7. Risks, and the divergences that must be pinned, not unified

- **Feature unification.** Any feature flipped on `akuma-threading`/`glue` for one
  kernel changes every package in that `cargo` invocation. The full AArch64 kernel
  and the amd64 kernel are separate bin builds today and never share one; keep a
  `cargo tree` check that the merge's new features stay off in the other kernel's
  build (B21's mitigation, verbatim).
- **cfg spelling.** `all(target_os = "none", target_arch = "…")` or nothing;
  `target_os = "none"` alone is the documented trap. `#[cfg(target_arch)]` on a
  *crate* that only ever builds for its target (the machine halves, `akuma-boot-amd64`)
  is the safe shape — the host-test-empty-crate pattern the tokio halves use.
- **Pinned divergences already on the books — keep them pinned.** `SharedSpans::{Copy,
  ShareFrames}` (taoxichuan copies MAP_SHARED spans, amd64 shares frames);
  `fxsave` 512 B vs `xsave` 832 B; taoxichuan's futex group-exit purge; glue's
  looser `rt_sigaction` vs the crate's strict `check`. A convergence slice that
  "simplifies" one of these breaks a kernel silently — each has a host test; the
  rule is one test per value.
- **The unsafe budget is a ratchet.** `crates/akuma-kernel-amd64/src` is at 57 production sites; E1–E9
  must net *down*. New crates land at 0 or the gate fails by design.
- **The self-host gates are behaviour.** The amd64 kernel builds itself in the guest
  (`scripts/amd64_selfhost_build.py`, `AKUMA_SELF_HOSTING_AMD64.md`), and taoxichuan
  pins the stronger property — the rebuilt image is byte-identical, K1 == K2
  (`TAOXICHUAN_FIXED_POINT.md`). Any slice that changes the image layout, the linker
  script inputs, or the boot order must re-run the self-host gate, not just the battery.
- **`akuma-exceptions` vs `akuma-trap-*`.** AArch64's fault contract flows through
  `akuma-exceptions` (welded to `akuma-exec`); amd64's through `Trap` handlers. E9
  must not pick one — it lifts the *decisions* and leaves both entry paths.
- **The rump stack is AArch64-only.** `kernel_main` wires `rump_proxy` and the
  sysproxy machinery; amd64 has no counterpart. A merged neutral spine either
  features it (`rump` feature, off for amd64) or it stays an AArch64 bin concern.
  Decision needed before M2, because `kernel_main`'s shape depends on it.
- **The nightly toolchain split.** AArch64 self-hosts on its pinned toolchain; amd64
  builds with a gnu nightly in-guest (`RUST_TOOLCHAIN_AMD64.md`). The kit crate must
  build under both; feature-gated `build.rs` behaviour is the known-safe mechanism
  (taoxichuan's linker-script hand-over precedent).

---

## 8. Open questions

1. **`akuma-kernel-core` adoption depth for amd64.** Its `platform.rs` is
   AArch64-machine-table-shaped today. Does M2 generalize it in place (risking the
   AArch64 kernel mid-flight) or add a sibling `machine` module with the same
   `MachineFacts` API and migrate both? (Lean: sibling first, fold at M7.)
2. **Does the full AArch64 kernel switch to the hooked scheduler** (M6), or does it
   keep the SGI path as a pinned second shape? The boot suite pins today's
   behaviour; switching is a behaviour change that needs its own gate run.
3. **rump's fate in the merged spine** (feature vs bin-local). Blocks M2.
4. **AF_UNIX on amd64** (E4b): adopt `akuma-net-unix` whole, or keep the local
   `sock.rs` state machine and share only the marshalling? B21 R5 made it a tokio
   feature; the full kernel may want the same feature boundary.
5. **kvm0's `kvm/` modules** (relay, tun, ctl — 2.8 K L of guest-facing plumbing)
   are amd64-bin code with neutral-looking halves (the relay is socket algebra).
   Extract opportunistically after E1–E9, or leave until an aarch64 VHE backend
   justifies the `akuma-kvm-abi` growth?
6. **Sequencing vs B21 A5.3→A7.** The tokio kit's aarch64 half is mid-flight and
   consumes the same crates the merge touches (`akuma-boot-aarch64`, `akuma-tokio-kernel-*`).
   Serialize M-slices against B21's remaining steps per crate, or freeze the kit
   while M0–M4 run? (The GLM plan's interleave rule worked for two sessions; three
   needs the same file-disjointness check written down.)

---

## 9. The work list, and the road drawn

### 9.1 Checklist

**Decide first — each blocks the slice named:**

- [ ] **D1 — rump's fate**: `rump` feature on the neutral spine vs bin-local forever
  (`kernel_main`'s shape depends on it). Blocks **M2**.
- [ ] **D2 — AF_UNIX on amd64**: adopt `akuma-net-unix` behind a feature (B21 R5's
  boundary) vs keep the local `sock.rs` state machine and share marshalling only. Blocks **M5**.
- [ ] **D3 — AArch64 onto the hooked scheduler?** Behaviour change; needs its own
  gate run. Blocks **M6**.
- [ ] **D4 — sequencing vs B21 A5.3→A7**: write down the file-disjointness check so
  three sessions don't collide on `akuma-boot-aarch64`/`akuma-tokio-kernel-*`. Blocks **M0**.
- [ ] **D5 — kvm0's relay/tun/ctl**: extract opportunistically after E1–E9, or defer
  until an aarch64 VHE backend justifies it. Non-blocking.

**The slices** (each = host tests on touched crates + budget gate + docs in the same
commit + the KVM gate named below):

- [ ] **M0 — census + baseline.** Classify every `crates/akuma-kernel-amd64/src` file arch / neutral /
  neutral-after-hook (B21 A0's method); freeze battery outputs under KVM for
  candidate-vs-HEAD comparison.
- [ ] **M1 — one boot spine (E1).** `boot::sequence(&MachineFacts)` in
  `akuma-boot-amd64`; both `kmain`s become fact collectors; one capability-gated
  self-test list. Gate: PVH (KVM + Firecracker) *and* multiboot2 boot the same suite.
- [ ] **M2 — the MachineFacts seam.** FDT (aarch64) and PVH+ACPI (amd64) fill one
  neutral machine description. Gate: both kernels boot unchanged.
- [ ] **M3 — mm adoption (E3).** amd64's tree walk onto `akuma-user-tree` plans (the
  GLM slice-3 remainder); `mm.rs` policy arms onto `akuma-mmap`. Gate: fork/CoW
  delta = 0 under KVM.
- [ ] **M4 — process model (E2).** `akuma-exec` slices: spawn, then wait4/lifecycle,
  then exec tables; `PROCS[128]` deleted last. **Serialized with M3** (both reach
  `mm.rs`/`usermode.rs`). Gate: box suite + selfhost build per slice.
- [ ] **M5 — syscalls/net/loader/fs/console (E4–E8).** sockaddr_un + setsockopt
  tables → `akuma-syscalls-net`; `akuma-net-unix` per D2; one ELF loader; mount
  policy → `akuma-vfs-glue`; serial console → the console family. Gate: sshd +
  `curl` + AF_UNIX probes under KVM.
- [ ] **M6 — scheduler hooks unified.** AArch64 full kernel onto the hooked path
  behind a feature, per D3. Gate: AArch64 boot suite byte-identical with the
  feature off; SMP probes both arches.
- [ ] **M7 — the kit arrives.** `akuma-kernel` front (entry! + cfg re-export) and
  the machine halves hoisted from what core/glue/boot crates now hold; two ~100-line
  bins. Gate: *both* kernels' full gates, including the self-host build.

**Standing rules, every slice:** `check_unsafe_budget.py` ratchets down only; the
pinned divergences stay pinned (`SharedSpans`, fxsave/xsave, futex group-exit purge,
sigaction strict-vs-loose); cfg spells `all(target_os, target_arch)` or nothing; the
feature-unification `cargo tree` check; candidate vs HEAD under the **same
accelerator**; Linux-KVM gates never stand in for kvm0/nested-SVM gates.

### 9.2 The ladder

```text
                DECIDE FIRST (blocks the slice in parens)          every slice's standing gates
                D1 rump: feature vs bin-local            (M2)      - cargo test -p <touched crates>
                D2 AF_UNIX: adopt vs pin                 (M5)      - check_unsafe_budget.py (down only)
                D3 aarch64 hooked-sched switch?          (M6)      - docs in the same commit
                D4 sequencing vs B21 A5.3..A7            (M0)      - candidate vs HEAD under the
                D5 kvm0 relay/tun extract?               (none)      SAME accelerator
                                                                        Linux-KVM gates never
   THE LADDER                                                           stand in for kvm0/nested-SVM
   .........
      M0  census of crates/akuma-kernel-amd64/src + KVM baseline frozen
       |
      M1  ONE BOOT SPINE (E1)
       |    boot::sequence(&MachineFacts); one capability-gated
       |    self-test list; both kmain's become fact collectors
       |    gate: PVH (KVM + Firecracker) AND multiboot2 boot the same suite
       v
      M2  MachineFacts SEAM  (needs D1)
       |    FDT (aarch64) and PVH+ACPI (amd64) fill one neutral description
       |    gate: both kernels boot unchanged (QEMU virt + Firecracker + KVM)
       v
      +----------------------- SERIALIZED (share mm.rs/usermode.rs) -----------------------+
      |                                                                                    |
      M3  MM ADOPTION (E3)                M4  PROCESS MODEL (E2)
      |    amd64 walk -> user-tree plans; |    exec slices: spawn -> wait4/lifecycle
      |    mm.rs arms -> akuma-mmap       |    -> exec tables; PROCS[128] deleted last
      |    gate: fork/CoW delta = 0       |    gate: box suite + selfhost build, per slice
      |          under KVM                |
      +------------------------------------------------+-----------------------------------+
                                                       |
                                                      M5  SYSCALLS / NET / LOADER / FS / CONSOLE  (E4..E8)
                                                       |    sockaddr_un + setsockopt -> syscalls-net;
                                                       |    net-unix per D2; one loader; mount -> vfs-glue;
                                                       |    serial console -> the console family
                                                       |    gate: sshd + curl + AF_UNIX probes under KVM
                                                       v
                                                      M6  SCHEDULER HOOKS UNIFIED  (needs D3)
                                                       |    AArch64 full kernel onto the hooked path
                                                       |    gate: aarch64 suite byte-identical, feature off
                                                       v
                                                      M7  THE KIT ARRIVES
                                                            front crate + machine halves hoisted;
                                                            two ~100 L bins
                                                            gate: BOTH kernels' full gates incl. selfhost
```

### 9.3 The end state

```text
     src/ bin (281 L today)                       amd64/ bin (53,787 L today)
        |                                         |
        |                                         |
        |         M7: hoist, then rename          |
                     |                      |
                     v                      v
        +------------------------------------------------------------+
        | akuma-kernel        forbid(unsafe), ~50 L                  |
        |   entry! per arch, cfg(all(target_os,target_arch)) selects |
        |   the machine half; amd64: kmain/kmain_mb2;                |
        |   aarch64: rust_start + secondary_shared_start             |
        +-----------------------------+------------------------------+
                                      |
            +-------------------------+--------------------------------------+
            v                                                         v
   NEUTRAL - one copy for both arches                    PER-ARCH machine halves
   akuma-kernel-core(+glue): MachineFacts,               akuma-kernel-amd64: IDT/trap-amd64,
   boot order, fault policy, console/config/             lapic, ring3, x86 sigframe, fpu,
   timer/irq                                             SIPI+MADT, kvm0 (AMD-V: ISA fact)
   + everything already shared:                          akuma-kernel-aarch64: _boot, trap-
   akuma-exec, syscalls-glue + the decisions             aarch64/exceptions, gic, psci, PL011,
   family, threading, mmap, user-tree, ext2,             CNTV, el0-entry, aarch64 sigframe, fdt
   vfs, tty, net(+unix), fbcon, elf-exec ...
```

---

## Background

[`REDUCING_PLATFORM_DEPENDENCY.md`](REDUCING_PLATFORM_DEPENDENCY.md) ·
[`AKUMA_AMD64_STREAMLINING.md`](AKUMA_AMD64_STREAMLINING.md) ·
[`AMD64_CRATE_REUSE_AUDIT.md`](AMD64_CRATE_REUSE_AUDIT.md) ·
[`TAOXICHUAN_AMD64_EXTRACTION_PLAN_GLM.md`](TAOXICHUAN_AMD64_EXTRACTION_PLAN_GLM.md) ·
[`docs/archive/B21_TOKIO_KERNEL_AARCH64.md`](B21_TOKIO_KERNEL_AARCH64.md) ·
[`TAOXICHUAN_B21_AARCH64.md`](TAOXICHUAN_B21_AARCH64.md) ·
[`docs/archive/B15_TOKIO_KERNEL_KIT.md`](B15_TOKIO_KERNEL_KIT.md) ·
[`TAOXICHUAN_FIXED_POINT.md`](TAOXICHUAN_FIXED_POINT.md) ·
[`AKUMA_BOOT_EXTRACTION.md`](AKUMA_BOOT_EXTRACTION.md) ·
[`AKUMA_KVM.md`](AKUMA_KVM.md) · [`AKUMA_KVM_LINUX_GUEST.md`](AKUMA_KVM_LINUX_GUEST.md) ·
[`docs/runbooks/kvm-linux-guest.md`](../runbooks/kvm-linux-guest.md) ·
[`docs/runbooks/amd64-bare-metal-loop.md`](../runbooks/amd64-bare-metal-loop.md) ·
[`docs/reference/crate-safety.md`](../reference/crate-safety.md) ·
[`docs/reference/subsystems/kvm.md`](../reference/subsystems/kvm.md) ·
CLAUDE.md §Layout (crate naming, unsafe budget, typed addresses).
