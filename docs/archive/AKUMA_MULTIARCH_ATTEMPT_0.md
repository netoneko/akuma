# Multiarch attempt 0 — running log

**Branch:** `akuma-multiarch-attempt-0` (from `0b78d0cb`). **Plan:**
[`AKUMA_ARCH_CONVERGENCE_RESEARCH.md`](AKUMA_ARCH_CONVERGENCE_RESEARCH.md) §9.
**Bench:** a Mac (Apple Silicon). amd64 runs under **QEMU TCG only** (HVF cannot run an x86
guest); aarch64 can use HVF or TCG. So every gate below is TCG, candidate vs baseline, same accelerator.

## M0 — baseline (2026-11-10)

`sh amd64/run.sh` (PVH, `-M microvm`, TCG, SMP=1, `INITARGS=uname,-a`), HEAD `0b78d0cb`:
**`Akuma/amd64 self-test: 826 passed, 0 failed`**, init ran, `uname -a` answered.
Builds clean for `--release` and `--features no-tests`; unsafe budget ok (419 sites).

- SMP=4 under TCG did **not** finish inside 300 s (stalled after "netpoll daemon spawned") — the
  known TCG smp=4 flake the research doc §1 names. Not a gate here; SMP gates need KVM.
- **Not runnable on this bench:** the multiboot2 path (QEMU `-kernel` speaks multiboot1/PVH only;
  needs a GRUB image) and Firecracker (KVM-only). Changes to `kmain_mb2` are build+clippy-checked
  only until a GRUB image exists; each such change is called out in its commit.

### Correction to the research doc (§2.3 item 2, E1)

"Two hand-maintained spines" is already half-true-no-longer: `boot::early_init`,
`install_shared_sinks`, `late_init`, `wire_console_and_syscalls` and `self_tests` (one list, 826
checks) are shared. What remains per entry point is the **fact collection** (`machine::describe` vs
`machine_from`), device bring-up order/gating (PCI, hda, kvm, power, wifi, watchdog, touchpad,
rtw89, root mount) and the **post-suite tail** (`boot_to_init` vs the tail of `kmain`). M1 is
therefore a sequence of small convergences of those, not a rewrite.

## M1 — one boot spine

- **M1a (done):** `boot::apply_runtime_flags(cmdline)` and `boot::dhcp_then_clock()`. The PVH path
  parsed 7 runtime flags (`fbverbose fbtrace strace strace_err strace_pid= strace_nr= nopidfd`),
  the multiboot2 path 2; now both call one function over a `&str`. Side effect, intended: `strace`,
  `nopidfd` etc. now work on the GRUB-booted metal. `SETTLE_BUDGET_MS` has one definition.
  `machine::{strace_pid,strace_nrs}` deleted (dead). Verified: PVH TCG SMP=1 826/0, `STRACE=1`
  prints `[sc]` lines through the shared function. multiboot2 side: compiles, not booted.
- **M1b (done):** `boot::bring_up_secondaries(machine, cmdline, keep_out) -> usize` replaces three
  copies of "start the other cores" (the self-test suite, the PVH `kmain` no-tests arm, the
  multiboot2 `start_secondaries` closure). The suite printed why it stayed single-core; the other
  two were silent — now all three print `nosmp` / `trampoline page is not free RAM`. Verified: PVH
  TCG **SMP=1 826/0, SMP=4 836/0** (the SMP=4 stall in M0 was the TCG flake, not a baseline
  property: this candidate completed). `no-tests` + default build and clippy clean. multiboot2
  side compiled only.

- **M1c (done):** `boot::apply_cmdline_policies(cmdline)` (the `smp=N` cap + the idle wake budget)
  called by both entry points right after the command line is known. The multiboot2 path used to set
  the idle policy only inside its `start_secondaries` closure, which the full-suite boot never runs,
  so `idlesleep` was silently ignored by a GRUB-booted tests kernel. Verified PVH TCG SMP=4 plain
  836/0. Note: `SMP=4 KCMDLINE="smp=2 idlesleep"` fails `smp ring3: two processes ran on two cores`
  on **both** M1b and M1c (control run) — a property of that command line, not of these commits.

### The multiboot2 gate (ryzen, 2026-11-10)

`kmain_mb2` can be **booted** without touching the metal: ryzen (Pop, `/dev/kvm`) runs
`overlays/ryzen/qemu.sh` — OVMF → the overlay's `grub.cfg` → multiboot2 kernel + `root.img` module,
the same chain as the real box, in QEMU/KVM. To keep the user's `/home/netoneko/akuma-metal` (and its
`out/` kernels) untouched, the rig runs from its own directory: copies of `build.sh` and `qemu.sh`
with `W=/home/netoneko/akuma-multiarch-rig`, `build.sh` cloning **this branch from GitHub** (so a
commit must be pushed before it is tested), `qemu.sh` patched to take `SMP=` and an extra
`grub.cfg` entry 31 (tests kernel, RAM root, `smp=4`, autoreboot) appended to the rig's own clone.
Run as root with `MEM=3072` (ryzen has ~6 GB available). Never install these kernels on the metal
(Mac-built or rig-built): they are for the rehearsal only.

| build | entry | result |
|---|---|---|
| M1a+M1b (`87ae39e6`) | 3: tests kernel, RAM root, `nosmp` | **816 passed, 0 failed**; prints the new `smp:  nosmp on the command line — single core` |
| M1a–M1c (`6453829a`) | 31: tests kernel, RAM root, `smp=4` | **826 passed, 0 failed**; 4 CPUs online, `online count agrees`; autoreboot reached, QEMU exited |
| W1b–W2c + W5c (`89dd26f7`) | 31: tests kernel, RAM root, `smp=4` | **827 passed, 0 failed**; `facts: multiboot2, 3066 MiB in 8 region(s), cpus 4`; 4 CPUs online; herd + sshd + autoreboot, QEMU exited. Covers the W1b/W2 rows below that said "multiboot2: not run" |

The multiboot2 side of M1a–M1c is therefore boot-verified under KVM (not on the metal itself).

## M2 — the `MachineFacts` seam

- **M2a (done): `akuma-machine-facts`** — a new leaf crate (no dependencies, `forbid(unsafe_code)`,
  unsafe budget 0, fixed arrays, `Copy`) holding what *both* discovery paths supply today:
  RAM regions, CPU count, virtio-MMIO windows + IRQs, the interrupt controller
  (`Gicv2`/`Gicv3`/`IoApic`/`Unknown`) and a `Provenance` (`CompileTime`/`Fdt`/`Pvh`/`Multiboot2`).
  `Facts::validate()` enforces the invariants every consumer assumes (RAM present and
  non-overlapping, virtio windows outside RAM and each other, CPU count ≠ 0). Console, timer, RNG
  and the reboot conduit are deliberately **not** in it: they are arch constants, not discovered.
  Per the research doc's open question 1, this is the *sibling* shape (own crate, own API), not an
  in-place generalisation of `akuma-kernel-core::platform` — that fold is M7.
  The protocol crates own the adapters and depend on the facts crate, never the reverse:
  `akuma_firecracker::MachineDescription::facts()` (tested against the four *measured* Firecracker
  FDTs, 1/2/4/8 vCPUs) and `akuma_ryzen_amd64::MachineDescription::facts(provenance)` (only
  type-1 memory becomes RAM). 12 + 2 + 2 host tests.
- **M2b (done): both kernels report it, neither behaves differently.** One line per boot:
  - amd64 PVH (TCG, SMP=4): `facts: pvh, 2047 MiB in 2 region(s), cpus 4, virtio 2, irq ioapic  [ok]`, suite **836/0**.
  - AArch64 (QEMU HVF, SMP=2): `[Platform] facts: fdt, 512 MiB in 1 region(s), cpus 2, virtio 8, irq gicv3  [ok]`, boot suite green.
  Report-only: a failing `validate()` prints `INVALID` and the boot continues exactly as before.
  - amd64 **multiboot2** (ryzen KVM rig, entry 31, `087da1e1`): `facts: multiboot2, 3066 MiB in 8 region(s), cpus 4, virtio 0, irq ioapic  [ok]`, suite **826/0**, 4 CPUs online.
    (`virtio 0` is right: a firmware boot has no virtio-MMIO command line; PCI devices are not in `Facts` yet.)
- **Not yet:** nothing *consumes* `Facts` — `mem::init`, `blk::init`, `smp::start_secondaries` and
  `detect_memory` still read their protocol-shaped descriptions. Switching them is the real M2
  payoff and the next step, one consumer at a time. The AArch64 compile-time map
  (`platform::{qemu_virt, firecracker}`) does not yet produce `Facts` either.

### The AArch64 gate (Lima, 2026-11-10)

`limactl start fc` (nested virt, `/dev/kvm`), then `limactl shell fc env SMP=N DISK=<abs path>
MEMORY=512M [KERNEL=<abs path>] sh scripts/lima_aarch64_run.sh` — **`limactl shell` does not forward
the caller's environment**, so variables go through `env` inside the guest, and the disk is mounted
`snapshot=on` (the Mac's `disk.img` is never written). `timeout` kills the shell but not QEMU in the
guest: stop it with `limactl shell fc pkill -f 'hostfwd=tcp::2622-:22'` (instance 4's forward) before
the next run, or its ports collide. Baseline = a scratch worktree of `0b78d0cb` built to its own
target dir and copied under the repo (Lima mounts `$HOME`, not `/private/tmp`).

| kernel | SMP | `PASSED` | `FAILED` | note |
|---|---|---:|---:|---|
| baseline `0b78d0cb` (5,058,360 B) | 1 | 310 | 0 | |
| M2 candidate (5,063,608 B, +5,248) | 1 | 310 | 0 | prints `[Platform] facts: fdt, 512 MiB in 1 region(s), cpus 1, virtio 8, irq gicv3  [ok]` |
| baseline | 2 | 320 | 0 | |
| candidate | 2 | 320 | 0, 0, **1** | 1 of 3 runs: `test_epoll_socket_waker FAILED: latency too high (8292us)` — a latency threshold, not seen in the other two candidate runs; the same 320 pass |

The diff of test results at SMP=1 is timings and page counts only. The earlier Mac HVF run is not a
gate (HVF asserts mid-suite, `QEMU_HVF_ISV_BUG.md`).

## Ladder status, audited against the tree (2026-11-10)

The research doc was written from a 53 kLoC bin and a September-era seam audit; checking each
claim against the current tree found most of the extraction work **already done**. This section
replaces the doc's §3.1/§9 status. Method: for each E item, grep the consumer and read the comment
at the call site.

| item | research doc said | tree says | status |
|---|---|---|---|
| E1 boot spine | two hand-maintained `kmain`s | `early_init`/`install_shared_sinks`/`late_init`/`self_tests` already shared; M1a–c merged the tail, flag parsing, secondaries and policies | **done** (device bring-up *order* still differs by protocol — deliberate) |
| E2 process model | `PROCS[128]` in `usermode.rs`, "the big one, ~4–5 kL" | **`PROCS` was deleted in 5b slice 4** (`AKUMA_AMD64_STEP5B_SLICE4_PROCS.md`); the process is `akuma_exec::process::Process`; what is left in `usermode.rs` is the syscall dispatcher (1,346 L), `do_execve`/`register_exec_process`/`sys_spawn`, fork's page walk and 2,661 L of boot self-tests | **done** (nothing to delete last) |
| E3 mm | walk should use `akuma-user-tree` plans | `mm.rs` already consumes `akuma-mmap` and `akuma-syscalls-mem` (18 + several sites); the **fork walk is `share_parent_memory_into` over `rewrite_leaves_in_range`** — it sees decoded leaves (`va/pa/prot/attr`), not raw PTE bits, so `PteLayout::plan_clone` (which works on raw entries) does not fit it without a different walker | **open, and not a mechanical swap** — see below |
| E4 sockets / AF_UNIX | `sock.rs` holds the unix state machine | AF_UNIX is routed to glue's `unixsock` (on `akuma-net-unix`) before `sock.rs`; `socketpair` smoke test exists | **done** (D2 correction above) |
| E5 net | DHCP/netpoll policy local | consumes `akuma_net::smoltcp_net`/`dns`; local code is hook bodies + clock/RNG | **done** |
| E6 loader | local ELF→address-space | `loader.rs` uses `akuma_elf_exec` (`entry_to_cross` etc.), 713 L | **done** |
| E7 fs/mount | local mount policy | mount table *is* `akuma_vfs_glue` since C1 step 4a | **done** |
| E8 console | serial-as-channel | `ProcessChannel` over `akuma_tty`/`akuma_exec` | **done** |
| E9 fault policy | duplicated CoW break | the **decision** is `akuma_cow` on both kernels; the page-table write stays local on each | **done** (decision shared) |
| M2 MachineFacts | — | `akuma-machine-facts` + adapters + both kernels report it; **no consumer reads it yet** except `smp::trampoline_page_available` (M2c) | **half** |
| M6 scheduler | AArch64 full kernel onto hooked path | `akuma-threading/hooked` exists and `akuma-tokio-kernel-aarch64` uses it; the root kernel does not | **open** (D3 decided) |
| M7 kit | `akuma-kernel` front + machine halves | not started | **open** |
| amd64 on `akuma-kernel-core`/`-glue` | "consumes none" | still true (`amd64/Cargo.toml` has neither) | **open — the real remaining convergence** |

### What that means for the plan

1. **M4 and most of M5 are finished work.** Do not schedule them; correct the research doc if it is
   ever reused.
2. **The honest remaining distance** between the two kernels is (a) amd64 not consuming
   `akuma-kernel-core`/`-glue` (the neutral spine: console rules, config, timer policy, irq table,
   `kernel_main`'s boot order), (b) the scheduler shape (M6), (c) the fork page walk (M3), and (d) the
   kit front (M7). `MachineFacts` is the on-ramp for (a): every `kernel_main` step that today reads
   an AArch64 machine constant has to read `Facts` first.
3. **M3 is a walker question, not a plan swap.** `plan_clone`/`plan_free` decide per raw entry; amd64
   forks through a leaf iterator that hides the entries. Two honest options: (i) give
   `UserAddressSpace` a raw-entry walk for amd64 and feed `plan_clone` (changes the hottest,
   SMP-sensitive path in the kernel — needs the fork/CoW battery, `cow_leak_gate`-equivalent delta 0,
   and an SMP gate on KVM), or (ii) leave the leaf walker and pin its three leaf cases
   (device / shared-span / CoW) in a host test beside `plan_clone`'s, so the two cannot disagree.
   (ii) is cheap and removes the drift risk; (i) buys a second walker nobody asked for.
4. A GRUB-image-free multiboot2 gate now exists (ryzen KVM rig, above), and an AArch64 KVM gate
   (Lima). Both are what M6/M7 need.

## M3 — the fork walk's decision table (option ii)

- **M3a (done): `akuma_user_tree::fork_leaf(Backing, Span, writable, cow) -> ForkLeaf`** with
  `ForkLeaf::{Device, ShareWritable, DemoteBoth, ShareReadOnly}`, and `amd64/src/usermode.rs`'s
  `share_parent_memory_into` now classifies each leaf through it (the four arm bodies are unchanged;
  only their conditions moved). Two host tests: the truth table, and
  **`fork_leaf_agrees_with_plan_clone`** — for every `(shared, writable, cow)` it derives the raw x86
  entry and requires `PteLayout::plan_clone(.., SharedSpans::ShareWritable)` to leave the same
  parent/child entries. The two decision tables (decoded-leaf walker, raw-entry walker) can no longer
  drift apart unnoticed. This is option (ii) of the audit: no second walker.
- **Pinned discrepancy:** the amd64 bin treats a `MAP_SHARED` span as `SharedSpans::ShareWritable`
  (writable in both, no CoW marker — the 2026-10-03 shared-writable-pages fix), but the
  `SharedSpans::ShareFrames` doc comment still says "(amd64 ...)": that comment is stale. Left for a
  doc-only fix by whoever owns the enum.
- **Gate:** ryzen KVM multiboot2 SMP=4 (entry 31, working-tree overlay via `local.tar`): **826 passed, 0 failed**, 4 CPUs online — the suite's `fork`/`execve`/CoW checks run on this walk. PVH TCG SMP=1, baseline `0b78d0cb` vs candidate, interleaved, 4 runs each: **826 passed,
  0 failed in all eight**. One earlier candidate run (SMP=1, built from this tree) and one SMP=4 run
  (M2c, before this change) showed `thread: probe reported every check [FAIL] got 0x0 want 0x1ff`
  (ten checks, all the clone/futex thread probe) and passed on re-run — an intermittent TCG timing
  flake of the thread probe, **not** reproduced in 4/4 runs of the identical binary and not
  correlated with this change (it predates it). Worth its own look; not investigated here.

## M6 — the feature exists, default off; the machine half does not (2026-11-10)

**Done.** `hooked-sched` (root) -> `akuma-exec/hooked-sched` -> `akuma-threading/hooked` ->
`akuma-switch/hooked`; in no profile, not in `default`. **Gate: the AArch64 default build is
byte-identical** with and without the plumbing (`cmp` of the release ELF built from the stashed vs the
edited tree: identical; the guard in `src/main.rs` is `if cfg!(feature = "hooked-sched")`, const-folded).
With the feature **on**, the whole kernel compiles and clippy is clean (the `hooked_sched` cfg is
confirmed active in `akuma-threading`'s build-script output), and `rust_start` prints
`hooked-sched: the AArch64 full kernel has no machine half ... halting` and parks, instead of panicking
at the first timer tick.

**Why it stops there — measured, not assumed.** `akuma_threading::sgi_scheduler_handler_with_sp` is
`#[cfg(sgi_sched)]`; under the hooked path it is a stub that panics, and
`akuma-exceptions/src/lib.rs:2319` and `:2364` call it from the root kernel's IRQ front end. The root
kernel *is* the SGI design: preemption is a handler that returns a different SP to assembly, with
per-thread saved contexts (`akuma-switch`) and `TTBR0` in them. The hooked path is the opposite shape:
cooperative switches (`x86_yield`/`x86_pick_next`) with the machine's effects supplied through
`X86ArchHooks`, an EL0 trap frame at the top of each thread's own kernel stack, and an idle thread per
core that runs the picker. `akuma-tokio-kernel-aarch64` has that machine half (`sched.rs` 274 L,
`trap.rs` 195 L, `smp.rs` 202 L, built on `akuma-vectors-aarch64`), but it sits under a different trap
front end than `akuma-exceptions`.

So "flip the feature" needs, in order — none of it is a flag:
1. **A second IRQ/EL0-trap front end in `akuma-exceptions`** (or reuse `akuma-vectors-aarch64`) that
   saves the EL0 frame on the thread's own kernel stack and calls `x86_yield` from the tick, selected
   by the feature. This is where the risk is; the SGI front end has ~10 documented SMP races fixed in it.
2. **The AArch64 `X86ArchHooks`/`ThreadRuntime`/`ProcessHooks` registration** for the root kernel
   (`switch_to` = `TTBR0` via `akuma_boot_aarch64::user::switch_to`, `prepare_task_slot` = kernel stack,
   `transfer_lock_depth` = the BKL depth) — the tokio kernel's `hooks()` is the template.
3. **Boot ordering**: `kernel_glue::kernel_main` calls `threading::init()` (`:1138`) and brings
   secondaries up around it; under the hooked path core idle threads are seeded instead.
4. **The self-test suite**: tests that read SGI-path state (`get_saved_kernel_resume`,
   `any_saved_ctx_on_l0`, `update_thread_context`) have no hooked meaning and need cfg arms.
5. Gate per the research doc: boot suite byte-identical off (done), devbox boots on, SMP 1/2/4 probes
   on Lima/KVM.

Recommendation: do it as the **A5/A7 tail of B21** (the tokio kit's aarch64 half), where the front end
already exists, and let the root kernel adopt the *kit's* trap front end rather than grow a second one —
that is D4's sequencing question, still open.

## M7 — not started, on purpose

The research doc's own rule (§4): *"Do not start by creating the kit crate ... the kit front
(`entry!` + cfg re-export) is ~50 lines and can arrive last, when the amd64 bin is thin."* The bin is not
thin: `amd64/src` is 53 kL, `usermode.rs` alone 9.3 kL (1,346 L `syscall_dispatch`, 2,661 L of boot
self-tests), and amd64 still consumes neither `akuma-kernel-core` nor `akuma-kernel-glue`. A front crate
now would re-export two kernels that share a name and nothing else — the "thin witness" the doc warns
about. Preconditions, in order: (a) amd64 on `kernel-core`'s neutral spine (console rules, config,
timer policy, irq table), with `MachineFacts` as the input; (b) the boot self-tests moved out of
`usermode.rs` into a tests crate; (c) M6 done; (d) the dispatcher split by family; (e) the
device drivers out of the machine half into a drivers-wiring crate (below). M7 is then a
rename/hoist.

### Added to the plan 2026-11-11: the tests crate and the drivers-wiring crate

`amd64/src` (56.9 kL) does not split two ways (tests and drivers). It splits three ways, and
each destination gets its own crate:

| destination | what | crate |
|---|---|---|
| neutral, shared with AArch64 | syscall dispatch, `fd`, `mm`, most of `signal`, `sock`, `fs`, `pipe`, `futex`, `loader` | the existing `akuma-kernel-core`/`-glue`, `akuma-syscalls-glue`, `akuma-exec` |
| boot self-tests | `usermode/tests.rs` (3.0 kL) plus every file's inline `cfg(not(feature = "no-tests"))` suite | **`akuma-kernel-tests-amd64`**, the counterpart of `akuma-kernel-tests-aarch64`; a `no-tests` image does not link it |
| device drivers | `xhci`, `hda`, `rtw89`/`rtw89_sta`/`wifi`, `nvme`, `touchpad`/`i2c`/`input`/`kbd`, `fbdev`/`splash`, `ramdisk`, `blk`, `pci`, `kvm` | **`akuma-drivers-amd64`**: the wiring between the host-tested driver crates (`akuma-rtw89`, `akuma-net-rtl8169`, `akuma-dma`, `akuma-svm-amd64`, …) and the kernel's registries |
| machine half | IDT/trap entry, `lapic`, `smp`, `gdt`, `paging`, `multiboot2`, `boot`, ring 3 entry, x86 sigframe, fpu, `sched`'s `ArchHooks` side | `akuma-kernel-amd64` (what `amd64/` becomes) |

**The drivers-wiring crate is configurable.** Every amd64 image compiles every driver today and
probes for the hardware at boot. `akuma-drivers-amd64` gives each driver its own feature
(`xhci`, `hda`, `wifi-rtw89`, `nvme`, `touchpad`, `kvm`, …). An `all` feature is the
bare-metal default; a Firecracker image needs none of them. The machine half sees drivers only
through one registration call per class (block, input, net, framebuffer, sound), so turning a
feature off removes the driver's code and its probe, and leaves no `cfg` in the machine half.
The PCI enumeration the drivers bind through moves with them. The machine half keeps only
config-space access (`akuma-port`'s `PCI_CONFIG`).

The order is: tests crate first, because it is pure movement and touches no driver; then one
driver per commit, each one gated by an amd64 TCG self-test and, for drivers TCG does not
exercise (`rtw89`, `rtl8169`, `hda`, `touchpad`), a trashcan or ryzen boot.

## M2 tail, and the thread-probe flake (2026-11-10, afternoon)

- **AArch64 compile-time map -> `Facts`: not done, deliberately.** `akuma-kernel-core::platform` is
  `#[cfg(target_os = "none")]`, so a host test cannot reach the `machine::{RAM_BASE, VIRTIO_PA,
  VIRTIO_STRIDE, VIRTIO_INTID_BASE}` constants. The bootstrap-vs-FDT agreement is already pinned by
  literals in `akuma-firecracker`'s tests against the measured QEMU and Firecracker trees
  (`virtio_slots_are_dense_from_0x40003000_and_intid_32`, `qemu_virtio_intids_start_at_48_...`). A
  compile-time `Facts` would also have no RAM size (the map is a bootstrap guess), so `validate()`
  would reject it. If the constants are ever moved into a host-visible crate, the agreement test
  becomes one assert per constant.
- **Thread-probe flake, read not reproduced.** `thread_test` (`usermode.rs`) reads the global
  `EXIT_STATUS`, which any process on any core writes (`run_process` overwrites it with
  `enter_user`'s return; `exit_group` and fault paths store too). The failure value was `0x0`, not
  `u64::MAX`, so something stored 0 *after* the test reset it and the loop's
  `all_user_tasks_finished()` exit condition came true — a status from another task (a lingering
  earlier test process or a probe thread), or the probe's own early exit, not "never ran". The test is
  the racy part: `spawn_test` already moved to `waitpid` for the same reason (its comment says so).
  Suggested fix for the owner: read the status through `waitpid` like `spawn_test`, then the flake
  cannot hide a real failure either way. Not changed here because it edits the suite's meaning.

## Decisions (owner, 2026-11-10)

- **D1 — rump:** a universal, **optional** `rump` feature on the shared spine, not bin-local. Its
  testing is **deferred**; until then amd64 does not build it by default, and the feature must not
  leak into the other kernel's build (`cargo tree` check, research doc §7). On AArch64 the feature
  is already in `default` at compile time, but rump only runs under `rump-default` / `devbox`
  (`RUMP_NIC=1`); the default devbox is the smoltcp one. Unblocks **M2**.
- **D2 — AF_UNIX on amd64:** **adopt `akuma-net-unix`** (one state machine for both kernels),
  behind a feature, with **extensive testing** required before `sock.rs`'s local state machine is
  deleted: host differential tests old-vs-new first (the `sockaddr_in` slice-4 precedent), then
  an AF_UNIX probe under TCG, and sshd/herd paths that use unix sockets. Unblocks **M5**.
- **D2, correction (2026-11-10): already adopted.** The research doc's premise ("amd64 has no
  `akuma-net-unix`; its unix-socket marshalling lives in `sock.rs`") is stale. `akuma-syscalls-glue`
  depends on `akuma-net-unix` and serves AF_UNIX through `glue::unixsock`; amd64's dispatcher routes
  `socket`/`socketpair`/`sendto`/`recvfrom`/`sendmsg`/`recvmsg` for AF_UNIX to it *before*
  `crate::sock` (`usermode.rs`, "AF_UNIX first, then the native stack"), and `crate::sock` says so
  in its own comment. The suite has a `socketpair` smoke test. So there is no local unix state
  machine to delete and M5's "adopt net-unix" item shrinks to: confirm nothing in `sock.rs`/`fd.rs`
  still special-cases a unix fd beyond `fd::is_unix_socket`, and run the AF_UNIX battery D2 asked for. **That battery exists:**
  `userspace/forktest/c_stress/unixsock_amd64.c` (14 checks: STREAM/DGRAM, bind/listen/accept4/connect,
  epoll on the listener, S_IFSOCK path, connect-to-nothing) is in `scripts/utils/amd64_shmwrite_check.py`,
  and passed 14/14 on metal (`AKUMA_AMD64_AGENT_STAGING_AND_ACCOUNTING.md`). Nothing to write; re-run it
  after any change to the unix routing in `usermode.rs`.
- **D3 — scheduler:** **AArch64 adopts the hooked scheduler** (`akuma-threading`'s hooked path),
  behind a feature first. *Correction to the stated reason:* both paths live in one crate and share
  the same ~31 host tests (slot/wake/park state machine); the SGI handler and the hooked switch
  bodies are `target_os = "none"` code and neither is host-tested. The real argument for the hooked
  path is one scheduler shape across both kernels (and the `X86ArchHooks`/`Platform` seam), not
  better test coverage. It is a behaviour change, so M6 keeps the
  research doc's gate: the AArch64 boot suite must be byte-identical with the feature off, and
  feature-on needs its own run (QEMU HVF/TCG here; SMP probes). The SGI path stays until then.
  Unblocks **M6**.
- **D4, D5:** still open; neither blocks M1/M2.

## W5a — the thread-probe flake, fixed at the test (2026-11-10, evening)

**Cause, confirmed.** Five boot tests (`elf`, `fdprobe`, `thread`, `kill`, `sigregs`) reset a global
`EXIT_STATUS`, drove the scheduler until `all_user_tasks_finished()`, and read the global back. Every
process on every core stored it on its way out of ring 3 (`run_process`, `unwind_to_kernel`, the exit
arms), so a straggler or a forked child could overwrite the probe's status — the `got 0x0 want 0x1ff`
failure. **Fix:** `usermode::wait_test_exit(pid, spins)` waits on the test process's **own exit
channel** (`get_child_channel(pid)`, marked by `run_process` after `thread::drain` with the full `i32`)
and returns that status. Not `waitpid`: a wait status keeps 8 bits and the probes report 9- and 12-bit
masks. With no reader left, the global and its six stores are deleted; `exit_current_with_code(code)`
became `exit_current_group()` (the code was only ever stored to the global; the real carrier is `%rax`).

**Gate (PVH, TCG, interleaved, same session).**

| kernel | SMP=1 | SMP=2 | SMP=4 |
|---|---|---|---|
| baseline `445fe139` | 4/4 826/0 | — | 1/4 clean: **2× `thread: ... got 0x0 want 0x1ff`**, 1× `spawn` stdio checks fail then hang (727 s timeout) |
| candidate | **16/16 826/0** | **6/6 836/0** | 2/4 clean: 1× `net: the netpoll daemon completes laps`, 1× `spawn` stdio checks fail then stall after "netpoll daemon spawned" |

So the thread-probe failure is gone (0 in 26 candidate runs, 2 in 4 baseline SMP=4 runs) and **no real
threads failure was hiding behind it** — every candidate run reports `0x1ff`. The remaining SMP=4 TCG
failures appear on the baseline too and are different tests:

- **`spawn: the child's registered table holds fd 0 as Stdin` (+fd 1, fd 2):** the same family of test
  race. The test inspects the child's fd table right after `sys_spawn`, on the comment's assumption that
  this is "the one moment the table is guaranteed populated"; at SMP=4 `/bin/hello` can run to its exit
  on another core and `close_all` has emptied the table first. Open: the fix needs a child that blocks
  until told (e.g. reads stdin) so the inspection window is real; not changed here.
- **netpoll laps / stall after "netpoll daemon spawned":** the TCG SMP=4 stall M0 recorded. Not a gate.

## W5b — `check_addr_params.py` back to green (2026-11-10)

It failed on `main` too (+29 over budget in six crates). Cause: the 2026-11-06 `unsafe` sweep
(`15b40615`, `c2f58d79`, `fa01c1af`, `7c425e96`) moved raw-pointer operations into new leaf crates
(`akuma-switch`, `akuma-frame-raw`, `akuma-mmu-raw`) and the untyped `pa`/`addr`/`base` parameters
moved with them, the same day the address budget was recorded; `akuma-pagetable` (+3) and
`akuma-threading` (+2) gained the checked doors that replaced `Frame::from_pa` claims. Nothing caught
it because **the pre-commit hook does not run `check_addr_params.py`** (the hook lives in `.git/hooks`
and is not versioned, so this branch cannot change that; worth adding by the owner). This branch's
`akuma-machine-facts` (M2a) added 3. Resolved with a justified `--update` (per-crate reasons in
`docs/reference/typed-addresses.md`'s status table); the same run ratcheted `amd64` 87→85,
`akuma-exceptions` 16→15 and `akuma-pmm` 53→49 **down**. Total 704 in 55 components. Typing the raw
leaves is the bottom-up sweep that doc already orders, not this branch's work.

## W1 — amd64 onto the neutral spine

### The classification (2026-11-10, read against the tree)

`akuma-kernel-core` did **not build** for `x86_64-unknown-none` (first error: `akuma-gic`'s `{v:w}`
asm), so "amd64 consumes none of it" was a build fact, not a choice. Every module, with what amd64 does
for the same job today:

| module | class | amd64 today | action |
|---|---|---|---|
| core `klog` (`log` crate sink) | **neutral** | **no logger installed**: every `log::info!/warn!` from `akuma-exec`/`akuma-elf` was dropped (2 info+ sites today, e.g. `[ktg] grace expired`; the ~50 `debug!` are below klog's level anyway) | **adopted, W1a** |
| core `config`, `fs`, `file_page_cache` | neutral re-export shims | calls `akuma_config`, `akuma_vfs_glue::fs`, `akuma_fpcache` directly — the same crates | nothing to move (the shim exists for `crate::x` spellings in `src/`) |
| core `pmm` (`akuma_exec::pmm` + `dp_counters_line`) | neutral | re-export unused; the demand-paging counter dump is AArch64's crash path only | adopt opportunistically if amd64's fault path wants the line |
| core `irq` (line table + `akuma_gic::enable_irq`) | table neutral, enable AArch64 | **routes no device IRQ at all** — devices are polled; the only vectors are LAPIC timer, wake IPI, shootdown | `register_handler` gated `target_arch = "aarch64"`; no amd64 consumer exists to adopt the table |
| core `ntp_boot` | neutral-after-a-hook (uptime, yield) | `clock.rs` runs the **same** `akuma_sntp::boot` loop over the LAPIC clock — the protocol was already one crate | gated to AArch64; folding the ~40 lines of effect wiring is optional |
| core `console` (PL011, cross-core serialisation, line input, dmesg ring) | AArch64 device + neutral rules | serial `ProcessChannel` over `akuma_tty`; dmesg ring is the shared `akuma-dmesg` | gated; the printing *rules* are already `akuma_primitives::safe_print!` on both |
| core `timer` (CNTV ISR, SGI ring, governor) | AArch64 | LAPIC TSC-deadline; tick policy is `akuma_timer::policy` (shared crate) | gated; M6 changes its tail (`x86_yield` instead of the SGI) |
| core `platform` (compile-time map + FDT) | AArch64 | `akuma-ryzen-amd64` / `akuma-multiboot2`; both feed `Facts` (M2) | gated; the neutral output is `Facts` |
| core `bkl_profile`, `nic_profile` | neutral diagnostics, feature-gated | none | not adopted (no consumer asked) |
| glue `compute_heap_size`/`compute_thread_limit`/`reserve_calc_ram`/`compute_memory_layout` | neutral, pure, host-tested | `akuma_boot_amd64::mem::heap_size_for` — also pure and tested, **different policy on purpose** (512 MiB / 1 GiB at ≥ 8 GiB, raised after `apk add` exhausted 64 MiB; AArch64 is RAM/8 clamped 64–256 MiB) | **pinned divergence**, not unified: a merge would change one kernel's heap |
| glue `build_exec_runtime` vs amd64 `exec_runtime::runtime()` | same shape (`ExecRuntime`), values per machine | ~15 of ~45 fields name the same glue functions (`eventfd`/`unixsock`/`epoll`/`pidfd`/`pty`/`flock`/namespaces); amd64 differs on purpose in pipes (`crate::pipe`), sockets (`sock::close`), futex, BKL-free I/O | considered and **not** folded: struct-update from a shared base would turn a missing machine field from a compile error into a runtime panic |
| glue `kernel_main`, `run_async_main(_preemptive)`, `detect_memory`, `halt`, netpoll doorbell | AArch64 boot sequencing (GIC, PSCI, FDT, linker symbols) | `boot.rs` spine (M1) | per-machine by construction; M6/M7 territory |
| glue `vfs`, `syscall` | `src/` re-export shims | — | nothing |
| glue `rump_proxy` | D1: universal optional feature | not built | deferred (D1) |

So the neutral spine amd64 can actually *use* is small: most neutral logic already lives in leaf crates
both kernels consume directly, and `kernel-core`'s neutral modules are mostly re-export shims for the
AArch64 bin's `crate::` paths. What `kernel-core` uniquely owns and amd64 lacked was the logger.

### W1a — `akuma-kernel-core` builds for amd64; amd64 installs `klog` (2026-11-10)

- `akuma-kernel-core`: `akuma-gic`, `akuma-boot-aarch64` and the crates only its AArch64 modules name
  (`-dmesg`, `-fdt`, `-firecracker`, `-machine-facts`, `-syscalls-time`, `-timer`, `-uart`) moved to
  `[target.'cfg(target_arch = "aarch64")'.dependencies]`; `console`, `platform` and `timer` are
  `cfg(all(target_os = "none", target_arch = "aarch64"))`; `ntp_boot` and `irq::register_handler` are
  `cfg(target_arch = "aarch64")` (the host build and `akuma-kernel-glue`'s host check still reach them). New `klog::is_installed()`.
- amd64: depends on `akuma-kernel-core` (no features), calls `klog::init()` in
  `boot::install_shared_sinks` right after the print hook (both boot protocols), and the suite checks
  `klog: the log crate's sink is installed`. Its `smp-shared`, `no-tests` and `net-noalloc` now forward
  to `akuma-kernel-core/*` like the root does (inert on amd64 — they reach only target-gated deps — and
  required by `check_feature_parity.py`, which stays at the 3 known drifts).
- **Gates.** AArch64 default release: with only the manifest/cfg change, **ELF byte-identical** to the
  stashed-HEAD build (`cmp`; note a baseline from an older commit differs by the embedded git hash
  `0x280496b3` vs `0x445fe139` — compare against HEAD, not the branch point). With `is_installed`
  added, the **load image is identical** (`rust-objcopy -O binary` + `cmp`) and the ELF differs only in
  `.strtab` (+9 B, symbol names). `extreme-size` builds. amd64 `cargo tree -e features`: identical
  apart from the new crate. clippy `-D warnings` clean for amd64 (default, `no-tests`, `net-noalloc`)
  and `akuma-kernel-core` on both targets. amd64 PVH TCG SMP=1, interleaved: baseline (W5a) 4/4
  **826/0**, candidate 4/4 **827/0** (the one new check). Unsafe budget 419 unchanged.
- **Not run:** multiboot2 (ryzen rig: `ssh 192.168.1.126:22` timed out — the bench is off the LAN); the change is on
  the path both protocols share (`install_shared_sinks`), so the PVH run exercises it.

### W1b — amd64 memory bring-up, block driver and trampoline read `Facts` (2026-11-10)

The M2 consumers. Each entry point builds `Facts` **once** with its real provenance
(`machine.facts(Pvh)` in `kmain`, `machine.facts(Multiboot2)` in `kmain_mb2`) and everything that
needs RAM or virtio windows reads that; `MachineDescription` stays the protocol adapter and keeps only
what `Facts` does not hold (the MADT's LAPIC ids for `start_secondaries`, ACPI for `power`, the
command line).

- **`akuma-machine-facts`:** `VirtioGeometry` + `virtio_geometry(windows)` + `Facts::virtio_geometry()`
  — the evenly-spaced-slots rule that was a method on the PVH command-line type, now the one copy
  (overflow-checked; 5 new host tests). `akuma_ryzen_amd64::MmioDevices::geometry` delegates to it (the
  tokio kernel still calls that), and `facts_geometry_agrees_with_the_protocol_view` pins both views on
  the Firecracker fixture and three command lines.
- **`akuma-boot-amd64::mem::{init, init_reserving}` take `&Facts`** (were `&MachineDescription`):
  they only ever read the type-1 regions and their sum, which are `Facts::ram()`/`ram_bytes()`
  (same `is_ram` filter, both caps 16). The crate's dependency on the protocol crate
  (`akuma-ryzen-amd64`) is **gone**, replaced by the dependency-free facts crate. Callers updated:
  amd64 `mem.rs`, `multiboot2.rs`, and `akuma-tokio-kernel-amd64` (PVH, builds `Facts` at the call).
- **amd64 `blk::init(&Facts)`**, **`smp::trampoline_page_available(&Facts, ..)`** (it used to build a
  `Facts` stamped `Pvh` even on a multiboot2 boot), **`machine::report_facts(&Facts)`**,
  **`boot::bring_up_secondaries(madt, &facts, cmdline, keep_out)`**, and `SuiteCtx.facts`.
- Only visible difference by construction: the `mem:` region report no longer lists zero-size RAM
  entries (`Facts::push_ram` drops them; they were always `usable_of == None`).

**Gates.** Host: `akuma-machine-facts` 22/22, `akuma-ryzen-amd64` 22/22. clippy `-D warnings`: amd64
(default, `no-tests`), `akuma-boot-amd64` + `akuma-tokio-kernel-amd64` (x86_64), `-boot-amd64`,
`-ryzen-amd64`, `-machine-facts` (host). Builds: amd64, taoxichuan (x86_64 **and** aarch64, `no-tests`),
shanghai. amd64 PVH TCG, interleaved against the W1a build: SMP=1 **4/4 827/0** both; SMP=4 baseline
2/2 837/0, candidate 1/2 837/0 + 1 hang in `sigregs` (below). The `facts:`/`mem:`/`blk:` boot lines are
**identical** to the baseline's. AArch64 untouched by this slice. **Not run:** multiboot2 (ryzen rig
unreachable from this network) — `kmain_mb2` compiles and its three changed lines are the same
substitutions as `kmain`'s.

**One codegen surprise, explained.** The candidate's `.text` is 431 KB **smaller** (3,470,031 →
3,039,183 B) and its `pmm:` line shows 105 more free frames — deterministic, every run. Per-symbol diff:
nothing was removed; ThinLTO stopped inlining `serial::putb` / `serial::putb_tty` into every print site
(they now exist as standalone 618 B / 547 B functions; `rtw89_sta::daemon` −37 KB, `netpoll_daemon`
−19 KB, `machine::report` −18 KB ... are their inlined copies leaving). Behaviour is identical; a
serial byte costs one call more, which a port write dwarfs. The image end moved 0x69000 = 105 pages,
which is exactly the free-frame difference.

**Open finding (pre-existing, not this slice): `sigregs` can hang at SMP=4 under TCG.** Seen on the
`445fe139` baseline (`s4b4`, 727 s timeout) and on this candidate (1 of 2): the suite passes `kill`,
starts `sigregs`, the log shows ~8 `[sig!]` deliveries of its null-store `SIGSEGV`, then nothing. Not
the known post-netpoll stall (that one stops before the suite). Not investigated yet.

### W1c — the AArch64 kernel's memory bring-up reads `Facts` too (2026-11-10)

The last M2 `Facts` consumer on the AArch64 side. `akuma-kernel-glue::detect_memory` parsed the FDT's
`memory` node itself, a second reader beside `platform::fdt_facts` (which already built a `Facts` for
the `[Platform] facts:` line and threw it away). It now takes `Option<&Facts>` and uses the first RAM
region, and `kernel_main` builds the `Facts` once and hands it to both.

- **One pinned behaviour change, on a path no supported machine takes:** a DTB that
  `akuma_firecracker::describe_fdt` cannot describe (no `memory` node with a size, or no GIC node) used
  to have its memory node read anyway; now it takes QEMU virt's defaults and says
  `[Memory] DTB does not describe this machine, using defaults`. A DTB with a memory node but no
  `size` used to be read as 256 MB; `describe_fdt` refuses it, same defaults. QEMU virt, Lima's KVM
  machine and Firecracker all describe cleanly (the `facts:` line reads `ok` on each).
- Fixed in passing: `compute_heap_size`'s doc comment had drifted onto `struct MemoryLayout`, leaving
  the function undocumented; each has its own again.

**Gates.** clippy `-D warnings` (`akuma-kernel-glue`, aarch64). Lima KVM, SMP=1, interleaved
`280496b3` (stashed HEAD) vs candidate, 3 runs each: **313 passed, 1 failed in all six**, the failure
being `test_epoll_socket_waker`'s 10 ms latency threshold on both sides (baseline 9.2–10.4 ms,
candidate 8.8–20.7 ms) — a tick-granularity threshold under KVM, not this change. The
`[Memory] Detected from DTB: base=0x40000000, size=512 MB` line is identical in all six. The final
binary differs from the tested one only by doc-comment moves: `.text` identical, one panic-location
byte in `.rodata`.

W1c at **SMP=2** (one pair, Lima, after the commit): candidate **323/1** (the same latency threshold),
baseline 322/2 — it also failed `orphan reparented to init (shell did not exit in 10s)` and did not reach
herd. A baseline flake, recorded so the pair reads honestly; not investigated.

## W2 — `usermode.rs` shrinks

### W2a — the boot self-tests move to `usermode/tests.rs` (2026-11-10)

`amd64/src/usermode.rs` was 9,277 lines and a third of it was `#[cfg(not(feature = "no-tests"))]`
items. They are now the child module `amd64/src/usermode/tests.rs` (2,979 lines moved, unchanged): the
22 `pub fn *_test(t: &mut Suite)` entry points, their private helpers (`run_sh_capture`,
`for_each_pt_load`, `socketpair_test`, `argv_capacity_test`, `reject_test`, ...), the test-process
plumbing (`start_test_process`, `wait_test_exit`, `finish_test_process`), the hand-assembled program
(`build_user_program`, `Image::new`, `Image::from_elf` — a second `impl Image` block in the child), and
the embedded probe ELFs. One `cfg` on `mod tests;` replaces the per-item gates; `usermode.rs` re-exports
the 22 entry points by name, so `boot.rs`'s `usermode::*_test` calls did not change.

**A child module, not a tests crate.** The tests reach `usermode`'s private state (`Image`'s fields,
`PROCS`, `SPAWN`, `reap_exec_process`); a child sees it through `use super::*` without widening anything,
a crate would need all of it `pub`. The extraction was by script (each gated item with its leading
doc/attribute block, cut at the column-0 or column-4 closing brace), checked by the compiler and the
image below. Fixed in passing: W5a had put `wait_test_exit` between `finish_test_process` and its doc
comment, which the move would have carried; each has its own doc again.

**Gates.** clippy `-D warnings`, amd64 default and `no-tests`. The `no-tests` image's `.text` is
**byte-identical** to the stashed-HEAD build; `.rodata` differs in 5 words, each a panic-location line
number in `usermode.rs` moved up by the removed lines (415→386, 448→419, 4297→4039, ...). PVH TCG,
interleaved against the stashed HEAD, 2 rounds: SMP=1 **827/0** in 4/4; SMP=4 baseline 837/0 in 2/2,
candidate 837/0 in 1/2 and one run that stopped after `net: netpoll daemon spawned [OK]` until the 600 s
timeout — the known TCG SMP=4 post-netpoll stall (no failed check; the next round passed in 15 s).

### M3 — option (ii) is final

Recorded here because the handoff asked for the decision with W2: M3a (`akuma_user_tree::fork_leaf`
plus `fork_leaf_agrees_with_plan_clone`) already pins amd64's leaf walker to `plan_clone`'s decision
table. Option (i) would replace the hottest SMP-sensitive path in the kernel with a second, raw-entry
walker to buy a property the agreement test already guarantees. Not doing it.

### W2b — `syscall_dispatch` moves to `usermode/dispatch.rs`, split by family (2026-11-10)

The 1,338-line dispatcher (939 of them comments) and the C1 seam (`to_glue`, `to_glue_raw`) are now
`amd64/src/usermode/dispatch.rs`. Its four stages keep their order: Akuma-private numbers
(`akuma_private`, the old 230-line `return match`), the x86-only legacy folds and the shared-number
calls (inline, verbatim), then the decoded `Syscall` table — which was one ~900-line `match` and is now
eight family functions, `fs` (53 arms), `poll` (9), `mem` (9), `net` (25), `process` (23), `signal` (7),
`time` (9), `system` (10), each `Option<u64>`, tried in that order before the `has no dispatch arm`
fallback.

- **Behaviour-preserving by construction:** a variant's arms (a guarded `AF_UNIX` arm and the unguarded
  one after it) stay together in one family in their original order, and the families are disjoint —
  the split script asserts both — so the order the families are tried in cannot change which arm
  answers. A line-multiset diff of old dispatcher vs new module shows every original line present; the
  only additions are the scaffolding and three `return X` -> `return Some(X)` in `clone`'s arm.
- **Cost, from the codegen, not measured at runtime.** Left alone, LLVM kept the eight families out of
  line (each called once, but `fs` is 2 KB), and `Option::or_else` closures stayed out of line even with
  the families inlined into them. With `#[inline(always)]` on the families (per-item
  `allow(clippy::inline_always)` with a reason, the `akuma-fbcon` precedent) and `if let .. return`
  chaining, the hot path is one `syscall_dispatch` with **5** jump tables where the single `match` had
  **3**; a call pays at most a couple of extra range checks. `.text` +5.3 KB. No KVM syscall-rate
  measurement was possible from here (ryzen unreachable); TCG timings would not resolve it.
- `usermode.rs` is now **4,940** lines (from 9,277 before W2a).

**Gates.** clippy `-D warnings`, amd64 default and `no-tests`. PVH TCG, interleaved against the W2a
commit (`f19cc93b`), 2 rounds plus 4 extra SMP=4 pairs: SMP=1 **827/0** in 4/4; SMP=4 baseline **837/0** in 6/6, candidate **837/0** in 5/6 and one run that hung in `sigregs` (the pre-existing SMP=4 TCG hang, `docs/README.md` row) — the suite's
`dispatch_smoke_test` (the vocabulary hop, the disjoint tables, `symlink` read-back) and every probe
that makes syscalls run through the new dispatcher.

### W2c — one fresh-process constructor for three kernels (2026-11-10)

The neutral part of `register_exec_process`. Three kernels build a process that did not fork from one —
AArch64's `Process::from_elf` (`akuma-exec/src/process/image.rs`), the tokio kernels' `Process::adopted`,
amd64's `register_exec_process` — and each spelled the whole 45-field `Process` literal. A field-by-field
comparison found **28 initialisers textually identical in all three** (signal table, sigaltstack, robust
list, cwd/root, namespace, stdio buffers, counters, `pgid == tgid == pid`, `ctty` 0). They are now
`Process::fresh(FreshProcess)` in `akuma-exec/src/process/fresh.rs`; `FreshProcess` is the 17 inputs the
three actually differ in. All three call it; amd64's per-field comments (owning address space, `with_stdio`,
`thread_id`, the console's terminal state and channel) moved onto the seed fields unchanged. The clone
family (`Process::inherit_from`) keeps its own literal: nearly every field comes from the parent. No new
allocation (the same `String`s/`Arc`s, moved through the seed).

**What does not move, and why.** `do_execve`'s neutral steps already run through shared code
(`perm::check`, `text_pin`, `parse_shebang`/`shebang_hop`, `Process::install_image`,
`glue::execve::commit`); what is left is x86 image building (`Image`, `crate::loader`), amd64's thread
drain and arming its user context — machine half. `sys_spawn` is the same shape (amd64's `SPAWN` rows,
console attach, `Image`). Moving them into `akuma-exec` would mean hooks for each of those, which is
more seam than code. **Recorded as final for this attempt.**

**Gates.** Builds: AArch64 kernel, amd64 (default + `no-tests` clippy `-D warnings`), taoxichuan and
shanghai for x86_64 **and** aarch64 (the tokio kernels are `adopted`'s callers); clippy `akuma-exec`
(aarch64); host tests `akuma-exec` 106/106. Lima KVM, interleaved vs the stashed tree: SMP=1 **313/1** in
3/3 on both sides (the epoll latency threshold); SMP=2 candidate **323/1**, baseline 322/2 — the baseline
again failed `orphan reparented to init (shell did not exit in 10s)` (2 of 2 baseline SMP=2 pairs today,
0 of 2 candidate; a baseline flake, not investigated). amd64 PVH TCG vs W2b: SMP=1 827/0 2/2, SMP=4
837/0 2/2 on both sides.

## W5c — `crate-safety.md` regenerated; the unsafe counter stops counting a type (2026-11-10)

`python3 scripts/cloc_akuma.py src crates`: **101 of 135 crates** forbid `unsafe`, 98.9% of production
lines outside any `unsafe` block, 361 production sites under `crates/`. The run reported **1 site inside
an enforced crate** and flagged it as a counter bug: `akuma-exceptions` (forbid since 11-06) casts
`enter_user_mode as unsafe fn(&UserContext) -> !`, and the lexer counted every `unsafe` token. It now
retracts an `unsafe` (or `unsafe extern "ABI"`) followed by `fn(` — a pointer type has no name between
`fn` and its parameters; a declaration does. Three self-test cases pin it; nothing else in the tree moved
(362 -> 361), and `unsafe-budget.json` drops `akuma-exceptions`' 1 (ratchet down). The doc's opening
paragraph names the 15 crates that took the ban since 11-02 (the enforced sets of the two runs diff to 15
added, 0 removed). `docs/README.md`: the thread-probe row now says root-caused (W5a), and a row for the
open `sigregs` SMP=4 TCG hang. **Not done, deliberately:** the `CLAUDE.md` crate counts are stale (the
file says 79 of 108); `CLAUDE.md` is the owner's to edit.

## D4 — the root kernel's hooked-scheduler front end is the kit's (decided 2026-11-10)

**Decision: under `hooked-sched`, the AArch64 root kernel installs `akuma-vectors-aarch64`'s table, the
tokio kernels' front end, and does not grow a second one inside `akuma-exceptions`.** The handoff
recommended this; what settles it is a measurement, not a preference:

- **The two front ends already build the same frame.** The root kernel's EL0 sync frame is
  `UserTrapFrame` (x0–x30 at 0, `sp_el0` 248, `elr_el1` 256, `spsr_el1` 264, `tpidr_el0` 272, pad 280)
  with the NEON save area at `akuma_threading::SYNC_FRAME_NEON_OFFSET` = 304 (`SyncFrameNeon`: q0–q31,
  `fpcr` at +512 = 816, `fpsr` 824) inside an `IRQ_FRAME_SIZE` of 832. `akuma_vectors_aarch64::Frame` is
  `x` 0, `sp` 248, `elr` 256, `spsr` 264, `tpidr` 272, `esr` 280, `far` 288, `q` 304, `fpcr` 816, `fpsr` 824,
  `FRAME_BYTES` 832 — the same bytes, with the syndrome kept where the root kernel keeps padding. So the
  root's syscall/fault/signal logic (`akuma-exceptions::sync_el0(FrameHandle)`, `sigframe.rs`'s NEON
  accessors) can run over a kit frame with no translation layer: a `FrameHandle` over its first 288
  bytes and the NEON area at the offset both already use.
- **What a second front end would duplicate is the risky part.** `akuma-entry::exceptions`' vectors and
  `irq_with_sp` carry the SGI design's ~10 documented SMP fixes (ESR/FAR snapshot in the vector, the
  eret-window poison checks, BKL reconcile by the restored frame's SPSR). Under the hooked path most of
  those questions do not arise — the frame never moves, a switch is a call — and the kit's stubs are the
  ones the tokio kernel already boots SMP on.
- **What adoption costs** (the remaining W3 work, in order): (1) a `FrameHandle` constructor over a kit
  `Frame` (`akuma-trap-aarch64`, the one `unsafe` it needs is the same claim `FrameHandle::new` makes
  today); (2) kit handlers in the root kernel for `El0Sync` (-> `akuma_exceptions::sync_el0`), `El1Sync`
  (the user-copy fixup and lazy/CoW EL1 faults the SGI path handles in `sync_el1`), `El0Irq`/`El1Irq`
  (ack, timer re-arm, `x86_yield` on a tick from EL0, BKL enter/leave); (3) the `X86ArchHooks` /
  `ThreadRuntime` / `ProcessHooks` registration (`switch_to` = TTBR0 via
  `akuma_boot_aarch64::user::switch_to`, `prepare_task_slot` = kernel stacks, `transfer_lock_depth` = the
  BKL depth), with `sched.rs` of `akuma-tokio-kernel-aarch64` as the template; (4) boot order — seed
  idle threads where `kernel_main` calls `threading::init()`; (5) cfg arms for the SGI-only self-tests.

## W3a — the hooked scheduler boots the AArch64 root kernel, one core (2026-11-10)

`cargo build --release --features hooked-sched` now boots: vectors from `akuma-vectors-aarch64`,
threads switched by `akuma-threading`'s hooked path, the root kernel's own syscall/fault/IRQ logic
unchanged behind them. **Lima SMP=1: PASSED=313 FAILED=1** — the same count as the SGI baseline, the one
failure the known `test_epoll_socket_waker` 10 ms latency flake — then herd, httpd and sshd start. 29 s
to herd in the harness against 59 s for the SGI build (not investigated; the harness stops at herd).
Secondary cores are **not** started under the feature yet (`[hooked] secondary cores not started`):
SMP>1 is W3b. Default off; the default build is code-identical (below).

What it took, by D4's steps:

1. **Front end** — `akuma-entry::hooked` (new, behind `akuma-entry/hooked-sched`): registers the kit's
   five handlers. `El0Sync` names the kit frame as a `FrameHandle` (const assertions pin the 288-byte
   prefix and the NEON offset), opens interrupts as the SGI stub does, calls `akuma_exceptions::sync_el0`
   and writes x0. `El1Sync` hands `sync_el1` the six registers the SGI stub pushes and turns its
   `El1Resume` into `frame.elr`. IRQs call the new `akuma_exceptions::irq_hooked(from_el0)`: BKL unless
   already held, ack, dispatch, EOI, and `yield_now` only for the scheduler SGI that interrupted EL0.
   EL1 runs with interrupts open, as on the SGI path.
2. **Machine half** — `akuma-kernel-glue::hooked` (new): `switch_to` records the outgoing slot's live
   TTBR0 with its slot generation and installs the incoming one's (`akuma_switch::install_ttbr0_flush`,
   now compiled for both AArch64 paths, as is `set_thread_register`); a generation mismatch or a root in
   `akuma-mmu`'s freed-L0 ring installs the boot tables instead. That record is also the free gate's
   probe (`register_saved_root_probe`), filtered like amd64's to slots that can still be switched in.
   Kernel stacks are the pool's (`threading::ensure_pool_stack_top`, new). `read_user_context` is the
   trap frame `sync_el0` already publishes (`threading::published_user_context`, new);
   `write_user_context` is a no-op because fork/clone children start from their image context. EL0
   entry is `hooked::enter_user` (kit `enter_user` at the slot's stack top); under the feature
   `akuma_exec::process::enter_user_mode_checked` routes through the runtime hook, so `execve` needs no
   change. The idle thread reaps, yields when work is due, else `wfi` without the lock.
3. **The lock** — `akuma_bkl` stays with the core across a switch, so `transfer_lock_depth` is a no-op
   (as amd64). The root kernel, unlike amd64, yields from dropped-BKL windows (VFS/net carve-outs,
   `lock_bounded`'s backoff); `akuma-threading/yield-takes-bkl` (new, set only by this feature) takes the
   lock around such a switch and gives it back on resume.
4. **Boot order** — `hooked::init` after `threading::init`; `akuma_entry::hooked::init` replaces
   `exceptions::init`; the netpoll main loop, which on `smp-shared` only `wfi`s and relies on the tick
   to preempt it, yields every lap under the feature (it halts only when nothing else is runnable) —
   without that, herd never ran. The `src/main.rs` halt is gone.
5. **Self-tests** — three cfg arms. `yield_now_detects_masked_yield`, `neon_regs_across_yield` and
   `fpcr_fpsr_across_yield` assert SGI-path properties (a yield is a self-SGI through the full-frame IRQ
   entry; Q0-Q3/FPCR survive it); a hooked yield is a call and preserves what AAPCS64 says a call
   preserves. They print `[SKIP]`. `as_drop_defers_while_saved_ctx_on_l0` gets a hooked arm
   (`hooked::test_swap_saved_root` on its own slot). **Vacuous under the feature, still run:**
   `neon_regs_across_preemption` and `fp_arithmetic_across_preemption` pass because kernel threads are
   never preempted on this path, not because the save works.

**A real defect found on the way, in shared code:** the hooked `spawn_user_closure_initializing` was a
`&mut self` method called under `POOL`, and `prepare_task_slot` runs inside it. amd64's stacks are its
own, so nothing noticed; this kernel's stacks are the pool's, and its first `spawn_fn` deadlocked on the
non-reentrant spinlock. It is now an associated function called without the lock (the lock guarded
nothing it touches).

**Gates.** Default AArch64 build vs HEAD (built in a detached worktree): same 5466 symbols; `.text`
differs in 2 bytes — one `mov` immediate 5421 -> 5468, a `#[track_caller]` line in `akuma-threading`
moved by the lines added above it; `.rodata` 249 bytes of panic-location lines; `.symtab`/`.strtab` only
LLVM's per-module hash suffixes. amd64 (the threading change runs there): TCG candidate vs HEAD 2+2 runs
each, SMP=1 827/0 and SMP=4 837/0 on all eight, no `sigregs` hang this time. taoxichuan and shanghai
build both arches; `--features hooked-sched` and `hooked-sched,no-tests` clippy-clean; amd64 clippy
clean. Budgets: `akuma-entry` unsafe 14 -> 15 (the `FrameHandle::new` claim over the kit frame — the
same contract as the SGI shim's, D4 step 1 anticipated it); `akuma-kernel-glue` untyped address params
5 -> 6 (`saved_root_probe(l0_base: u64)`, whose shape `register_saved_root_probe` fixes for both kernels,
like amd64's `any_task_on_space_root`); amd64 ratchets 85 -> 84. **Not run yet:** devbox, the
epoll/futex suites, any SMP>1 — those wait for W3b's secondary bring-up.

## W3b — the hooked scheduler on 2 and 4 cores (2026-11-10)

`--features hooked-sched` now starts the secondaries. **Lima, boot suite to herd:**

| | SMP=1 | SMP=2 | SMP=4 |
|---|---|---|---|
| hooked | 314/0 (x2) | 322/0 (x2), 321/1 (the final build) | 319/1 (x3, and once on an earlier build: `test_epoll_socket_waker` every time, 8.1–14.2 ms) |
| SGI, same tree | 314/0 | 324/0 | 322/0 |

The count difference is the three SGI-only tests that print `[SKIP]` under the feature. **devbox-smoltcp**
(`devbox-smoltcp,no-tests,hooked-sched`, `devbox.img` snapshotted, ssh from the host): SMP=2 and 4 boot,
answer ssh in 3 s, report 2/4 CPUs, run `cargo --version` and 8 concurrent forks. Back-to-back ssh sessions
at SMP=4: **2 failures (exit 255) in 186 on hooked, 0 in 126 on SGI** — open; the reruns of 60 and 100 were
clean, and stderr was not captured for either failure.

How the cores come up: the SGI trampoline and `secondary_shared_start` are shared; under the feature the
core's adoption and its idle loop come from the kernel through a registered
`akuma_entry::smp_shared::HookedSecondary` (`akuma-entry` sits below glue), the core takes the kit's vector
table, and `threading::x86_adopt_core_idle` (new) claims a slot, records the boot stack and paints its
canary, as `adopt_current_as_core_idle` does for the SGI path. The cores start after `hooked::init` in both
images. Four defects stood between "core 1 online" and a green suite, each of them a property the SGI path
got from its tick:

1. **A recycled slot inherited a dead process's identity.** The hooked reaper
   (`x86_reap_terminated_slots`) and `x86_claim_slot`'s direct reuse of a `TERMINATED` slot skip the
   collector's cleanup callback — on this kernel `on_thread_cleanup`, which drops the tid -> pid row and
   unregisters the process. amd64 tears down on the dying thread and never needed it. Found as a lost futex
   wake: `test_futex_basic_wake`'s kernel-thread waiter, on a recycled slot on core 1, keyed its wait on
   `tgid=21` (a finished test process) while the waker used `tgid=0`; `futex_dump` showed both entries.
   Fix: `akuma-threading/collector-recycles` (set only by this feature) makes `x86_claim_slot` take `FREE`
   slots only, and the idle threads call the collector (`reclaim_terminated_slots`), not the reaper.
2. **A kernel thread that yields keeps the BKL across the switch.** The lock stays with the core, nothing
   preempts EL1, so threads that poll by yielding — to each other, or to nothing — kept it for good: two
   `blocking_relax` waiters on core 0 wedged core 1 (`[BKL] stuck`, `smp_shared_blocking_wait_peer_progress`).
   `x86_yield_now` (`yield-takes-bkl`) now releases and re-takes a held lock before switching; the ticket
   lock makes that a handoff. A yield already lets other kernel code run between the caller's halves, so
   no caller can rely on the lock across one.
3. **A socket wait that halts never gives the core up.** `idle_halt` (the socket wait's park) relies on
   the tick displacing a halted non-idle thread (the 2026-08-25 SMP=1 freeze fix); there is no such tick
   here, so httpd's `accept` held the only core at SMP=1 and herd's console drain never ran (lldb: the one
   vCPU in `idle_halt` under `socket_accept`). Under the feature it yields instead when anything else can
   run. (It surfaced only after 1 changed slot numbering, which changed who reached the core first.)
4. **Tests that assumed one core or the SGI idle.** `Thread spawn`/`Spawn and cleanup` and the three
   `ThreadWaker` tests read a slot a peer's idle thread may already have run and recycled; they accept that
   outcome under the feature (`slots_collected()`, a hooked-only counter). `core_idle_slots_survive_init`
   accepts `READY`: a hooked core switches its idle thread out whenever it has work.

**Gates.** Default AArch64 build vs `6144a970`: same 5466 symbols, one `.text` instruction differs (a
`#[track_caller]` line immediate, 5468 -> 5540), `.rodata` panic locations. Clippy clean (`hooked-sched`,
`hooked-sched,no-tests`, amd64). `check_addr_params` green — the two new stack-base parameters are
`VirtAddr`. Host tests for `akuma-threading` pass; note that `--features hooked` (with or without the new
features) does not build on a macOS host — `akuma-switch`'s ELF section syntax, pre-existing. amd64 (the
threading crate is shared; none of the new features is on there, so what reaches it is the
`cfg(hooked_sched)` additions with `collector-recycles` false): TCG candidate vs `6144a970`, two runs
each, SMP=1 827/0 on all four, SMP=4 837/0 on all four. One candidate SMP=4 run took 163 s instead of
7–9 s, all of it inside `sigregs` (pid 17's null-store `SIGSEGV` loop stalled ~155 s, then finished);
the rerun took 7 s. That is the open `sigregs` SMP=4 TCG finding above, this time recovering instead of
hanging. Note that all three sightings so far (here and the two above) were on a candidate build, each
with unrelated changes — not yet seen on a baseline, which with these sample sizes says little either way.

**`test_epoll_socket_waker` is not a hooked regression (corrected the same day).** The table above reads
as if it were: SGI passed in its three runs. An instrumented build (not committed) stamped when the waker
thread starts, when it calls `wake_all`, and when the waiter returns, on both schedulers:

| | waker starts | `wake_all` | waiter returns | cores (waker, waiter) |
|---|---|---|---|---|
| SGI SMP=1 | +631 µs | +6720 | +6780 (pass) | 0, 0 |
| SGI SMP=2 | +761 | +6030 | +11925 (**fail**) | 1, 0 |
| SGI SMP=4 | +934 | +6150 | +8965 (**fail**) | 2, 3 |
| hooked SMP=1 | not yet | not yet | +7321 (pass) | 0, 0 |
| hooked SMP=2 | +78 | +5081 | +7529 (pass) | 1, 0 |
| hooked SMP=4 | +1267 | not yet | +5944 (pass) | 1, 3 |

Two separate things. (1) **A cross-core wake is slow on both schedulers**: 2.4–5.9 ms from `wake_all`
on one core to the waiter running on another, against 60 µs on one core. The test's 8 ms bound leaves 3 ms
after the 5 ms spin, so it fails whenever that delay crosses it, under SGI as well. (2) **Under hooked the
waiter sometimes returns ready before anything woke it** (SMP=1 and SMP=4 rows: `EPOLLIN` at 6–7 ms with
`wake_all` not yet called). Then the test is not measuring the waker at all. Both are open; the next
measurement stamps the `wake_core` SGI's send, its arrival and the switch, and where the early readiness
comes from.

**Open:** the two epoll findings above; the epoll/futex suites under both schedulers; the 2-in-180 ssh
session failures; signal and deferred-kill delivery on the IRQ-return path (W3c).

### W3b follow-up — what the epoll test measures, the old kernels, and a hooked TLS bug (2026-11-10)

**Was the epoll failure introduced by the refactor?** No. Same `--features epoll-waker-trace` build
of the pre-refactor kernel (`main`, merge-base `3baa8e88`, trace applied in a scratch worktree, not
committed), two runs per cell:

| `test_epoll_socket_waker` | SMP=1 | SMP=2 | SMP=4 |
|---|---|---|---|
| `main` (pre-refactor) | pass, pass | pass; **fail** (8.2 ms after `wake_all`, same core) | **fail**, **fail** |
| this branch, SGI | pass, pass | pass, pass | **fail**, **fail** |
| this branch, hooked | pass, pass | **fail**; pass | **fail**, **fail** |

`orphan reparented to init` also failed once on `main` and once on SGI at SMP=4. That one predates
the refactor too.

**And the test does not measure a waker on any kernel.** The trace probes the fd before the wait and
times one non-blocking lap and one smoltcp drain:

- the fd is `EPOLLHUP` **before** the wait (`probe_before=0x10`). A never-connected smoltcp TCP socket
  is `Closed`, `!is_active()` makes it "dead", and a dead socket reports HUP whatever was asked
  (Linux also reports POLLHUP for a never-connected TCP socket). Every return, woken or not, was that
  HUP.
- one non-blocking `epoll_pwait` lap took **6.7–7.2 ms**, and one `smoltcp_net::poll()` called alone
  took **8.3 ms**. Once that drain had run, the real wait returned in 4 µs. The first drain after a
  gap clears a backlog, and the epoll lap does that drain before it probes.

So the test's elapsed time is one backlogged drain plus scheduling noise. Whether it crosses 8 ms
depends on how much network work piled up earlier in the suite, not on a scheduler. The "cross-core
wake is slow" reading in the correction above is withdrawn: those were drain times. **Open:** the test
needs rewriting to measure a wake (an fd that is not ready until the waker acts, the drain run before
timing). Why one drain costs ~8 ms at that point in the boot suite was not investigated.

**A userspace wake-latency probe for both kernels.** `userspace/epollprobe/c/epoll_wake_latency.c`
(new): one thread parks in `epoll_wait(-1)` on a pipe, another writes after 2 ms (spinning or
sleeping), 300 iterations, latency from the write to the wait's return. One source, static musl, both
ISAs. Akuma does not answer `getcpu`, so only the combined bucket fills. p50 / p99 in µs:

| AArch64, Lima devbox | spin SMP=2 | sleep SMP=2 | spin SMP=4 | sleep SMP=4 |
|---|---|---|---|---|
| `main` (pre-refactor) | 55 / 1434 | 137 / 555 | 173 / 814 | 283 / 590 |
| this branch, SGI | 53 / 1296 | 141 / 610 | 154 / 1080 | 267 / 648 |
| this branch, hooked (TLS fix below) | 139 / 290 | 331 / 992 | 142 / 529 | 181 / 458 |

| amd64, QEMU TCG | spin SMP=2 | sleep SMP=2 | spin SMP=4 | sleep SMP=4 |
|---|---|---|---|---|
| pre-refactor (`c2f58d79^`) | 24 / 83 | 234 / 535 | 53 / 92 | 305 / 659 |
| this branch | 26 / 75 | 235 / 495 | 52 / 100 | 322 / 825 |

The refactor did not move either kernel's wake path. The amd64 baseline is `c2f58d79^`, because
`3baa8e88`'s amd64 kernel does not link (`shootdown_entry` defined in both `amd64/src/shootdown.rs`
and `akuma-boot-amd64`, from `c2f58d79`). Hooked differs from SGI in shape, not by a clear margin
either way.

*Correction (same day): wrong baseline, and `main` does link.* This branch forked at **`0b78d0cb`**
(2026-11-09), not `3baa8e88`. The latter was `origin/main` as fetched when this section was started,
two days older. `main` fixed the duplicate symbol itself in `1e2fbdf0`, before the fork. The code
between `3baa8e88` and `0b78d0cb` is fbcon fonts, `wait4`/`waitid` routing and a `nopidfd` flag, with
no scheduler or epoll change, so the latency conclusions above stand. A rerun against `0b78d0cb`
would be the clean form. The metal A/B below is effectively against the fork point: its old kernel,
`c523598a`, is two docs-only commits before it.

**On real hardware (the trashcan, 2026-11-10).** The QEMU TCG amd64 numbers above are emulated (x86
on an ARM host); the Lima AArch64 ones are not (Lima `vz` plus nested KVM). With the owner's go-ahead
the branch kernel (`417485ca`, default features) is installed as the trashcan's main kernel. The old
one is kept as `/boot/akuma-amd64.c523598a`; `.good` is untouched. Boot self-test **789/0** (the
metal set, not the TCG 837), and herd brings `kot` and `llama-server` back. Probe, 300 iterations,
p50 / p99 µs:

| | rounds | spin p50 | spin p99 | sleep p50 | sleep p99 |
|---|---|---|---|---|---|
| `c523598a`, 19 h uptime | 2 | 9, 97 | 19, 1234 | 10, 10 | 22, 55 |
| `c523598a`, fresh boot + 60 s | 4 | 7–9 | 14–19 | 9–10 | 16–24 |
| `417485ca`, ~12 s after boot | 4 | 7–1015 | 23–5052 | 7–9 | 18–39 |
| `417485ca`, ~36 s after boot | 6 | 7–9 | 17–1036 | 8–13 | 17–1019 |

Real-hardware wake is **~7–10 µs**, against 26–320 µs under TCG. Both kernels have a second mode
where a wake waits about one 1 ms tick. The old kernel showed it once at 19 h uptime; the branch
kernel showed it in its first rounds after boot, while `llama-server` loads an 8 GB model and `kot`
starts, and not in its last four. That points at load. It is not a clean A/B: the old kernel's
fresh-boot rounds started ~2 min after boot and the branch kernel's within one, so it is not shown
the 1 ms mode is load-only. **Open:** rerun both kernels with the same post-boot wait, and find
which path waits for the tick (a halted peer core not sent a wake IPI is the first suspect).

**Line count, same script on all three trees** (`scripts/cloc_akuma.py`, no arguments): fork point
`0b78d0cb` 129,055 production / 70,590 test; `main` today 129,247 / 70,644; this branch 129,874 /
70,946. The branch is **+819 production lines**, all additions: `akuma-kernel-glue` +236 (hooked
machine half), `akuma-machine-facts` +178 (new), `amd64/src` +96, `akuma-entry` +94,
`akuma-threading` +79, the rest +136. Nothing is removed yet, because the hooked scheduler sits
beside the SGI one behind a feature. The reductions come with making hooked the AArch64 default and
deleting the SGI path, and with amd64 adopting `akuma-kernel-glue`. That size is not measured.

**The probe found a hooked-only correctness bug.** Before the fix, the probe SIGSEGVed in all four
hooked runs (SMP=2 and 4, both modes): the main thread came back from `pthread_join` with the exited
thread's TLS. The shared syscall epilogue copies the live `TPIDR_EL0` into the frame, and the hooked
switch did not preserve the register. Fixed in `akuma-kernel-glue::hooked::switch_to` (per-slot
`SAVED_TLS`); full write-up in `docs/archive/HOOKED_TPIDR_EL0_SWITCH.md`. After the fix: the probe
completes everywhere; hooked boot suite SMP=1 314/0, SMP=2 322/0, SMP=4 319/1 (the epoll test
above); devbox ssh session stress **0 failures in 100 at SMP=4 and 0 in 100 at SMP=2**, against 2 in
186 before. That makes this bug the likely cause of the earlier `exit 255`s, but 200 clean sessions
do not prove it. Under the same probe, hooked logs 15–21 `[bkls>]` contention lines against 4–5 on
`main` and SGI, and one `[BKL] stuck` appeared in a 100-session stress run. That is noted here, not
investigated.

**Open:** rewrite `test_epoll_socket_waker`; the epoll/futex suites under both schedulers; hooked BKL
contention; W3c.

## The epoll and futex suites under both schedulers (2026-11-10)

`scripts/epoll_suite.py` and `scripts/futex_suite.py`, Lima `fc` (KVM), devbox, one VM at a time,
`sgi-6a8-devbox` against `hooked-6a8-devbox` (both built at `6a817703`):

| kernel | SMP | epollops | futexops / futexkey / futextest / futexkill | `[BKL] stuck` |
|---|---|---|---|---|
| SGI | 2 | 17 PASS, 1 known DIVERGE | all pass | 1 |
| hooked | 2 | 17 PASS, 1 known DIVERGE | **futexops: no output** (0 PASS, 0 FAIL); rest pass | 5 |
| SGI | 4 | 17 PASS, 1 known DIVERGE | all pass | 2 |
| hooked | 4 | 17 PASS, 1 known DIVERGE | all pass | 30 |

The known DIVERGE is `epoll_ctl_add_twice`: a second ADD returns 0 where Linux returns EEXIST. Both
schedulers show it.

The empty futexops run could not be explained, because the harness kept only the failure lines.
The rerun booted hooked SMP=2 once and ran the whole futex suite 5 times: all four probes passed
every time, 20 of 20 probe runs. It is recorded as one silent run that did not reproduce.

**Where the `[BKL] stuck` lines come from.** All of them come from `futextest`'s thread
spawn-and-join churn (about 1,200 thread exits per run, each printing `[unregister] ... NO map
owner`). Run alone at SMP=4 on the hooked kernel:

- `futexops`, `futexkey` and `futexkill` give 0;
- `futextest` gives 28.

Seven full `futextest` runs per kernel, SMP=4, one boot each:

| per run | hooked (`hooked-w3c-devbox`) | SGI (`sgi-6a8-devbox`) |
|---|---|---|
| `[BKL] stuck` (8M-spin waits) | 28–35 | 0–3 |
| `[bkls>]` (shorter waits) | 156–176 | 497–602 |
| wall time | 11.3–13.9 s | 13.6–16.1 s |

So hooked has fewer contended waits in total and finishes the workload about 13% faster, but
more of its waits are long ones. The SGI tick that preempts kernel code is gone on hooked, so a
kernel stretch now runs to its end, and peers wait it out.

**One hypothesis tested and rejected:** the idle thread's collector reaps every eligible slot in
one BKL hold. Bounding it to 2 slots per hold made no difference, A/B/A/B:

- unbounded: 30 and 28 stuck lines;
- bounded: 25 and 26.

The change was reverted. Which stretch holds the lock is still open. The holds are tagged 511
(the profiler's "unknown" bucket), so the next measurement is a `bkl-profile` build that tags the
hooked path's holds.

## W3c — signal and deferred-kill delivery on the hooked IRQ return (2026-11-10)

Going in, the step was "mirror what the SGI path does on an IRQ return to EL0". **The SGI path does
nothing there.** On AArch64 the only places that take a pending signal are syscall return,
`rt_sigreturn` and the JIT retry. A thread in a pure compute loop therefore never received
SIGTERM, SIGINT or a handled signal under either scheduler; only SIGKILL worked, because it
hard-kills. The new probe `userspace/forktest/c_stress/sigspin.c` shows it: 0 of 3 rounds for
`term`, `handler` and `thread-term` on both schedulers, and 3 of 3 for the two SIGKILL cases.

Fix, on the hooked path only:

- `irq_hooked` takes the EL0 frame;
- before the BKL is released, `deliver_on_irq_return` consumes a deferred kill, then one
  pending signal.

The kit's IRQ frame is the full user frame, so `try_deliver_signal` writes into it unchanged. The
SGI path's IRQ frame is a smaller layout and is left as it is; that path is to be deleted. Full
write-up and timings: `docs/archive/HOOKED_IRQ_SIGNAL_DELIVERY.md`.

**Gates** (`hooked-w3c-*`):

- `sigspin 5`: 25 of 25 rounds pass at each of SMP=1, 2 and 4. SIGTERM reaches a spinner in
  5–15 ms, and a handler runs in 1–11 ms.
- Boot suite: SMP=1 314/0, SMP=2 322/0, SMP=4 319/1. The failure is the known
  `test_epoll_socket_waker`.
- epoll and futex suites: all pass at SMP=2 and 4, with `stuck` 1 and 38.
- clippy is clean with and without `hooked-sched`.

## Thread churn: the `[unregister]` print (2026-11-10)

The long BKL holds above were not the scheduler's. With `bkl-profile` and the hooked path tagged
(`irq_hooked` transient `irq`, idle `idle`, tag reload in `switch_to`), 50 of 64 stuck lines
were blamed on `idle`. A temporary timing probe in the hooked idle loop then showed about 6 ms
per reclaimed slot. The cost was one console line per thread exit,
`[unregister] ... has NO map owner`, which is normal on AArch64 and is a trap per byte under
KVM. Rate-limiting that line took `futextest` from about 13 s to 2.5–3.3 s on both schedulers,
with 0 stuck lines. Write-up and numbers: `docs/archive/UNREGISTER_PRINT_BKL_HOLDS.md`.

**Harness correction.** Two SGI SMP=2 boots looked wedged ("did not reach herd"). Both had
passed the suite and started herd. The console tore `[herd] Userspace supervisor starting` at
SMP>1, so my harness's marker grep missed it, the failure CLAUDE.md warns about. It now matches
`[Main] Starting herd supervisor`, which one core prints.

## Extreme-size under the hooked scheduler (2026-11-10)

`scripts/build_extreme_size.sh --features hooked-sched` builds; the image is 4 KB smaller than
SGI's. QEMU HVF, `SNAPSHOT=1`, 8 MB, the same commands on both kernels (ssh through `paws`, files
fetched with `busybox wget`):

| | SGI | hooked |
|---|---|---|
| boot to ssh | 3 s | 3 s |
| `tcc -o /tmp/h /tmp/h.c` | ok | ok |
| `sigspin 3` | 2 PASS, 3 FAIL (no delivery to spinners) | **5 PASS**, about 1 ms delivery |
| `MemFree` after boot | 3,692–3,752 kB | 3,292–3,400 kB |

**Open, before hooked becomes the default:**

- **hooked leaves about 350–400 kB less free at 8 MB.** One known part is core 0's idle loop,
  which is its own slot with a 96 KB zeroed system stack where SGI idles on the boot stack. The
  rest is not accounted for yet.
- **The 4 MB floor (acceptance 05) does not boot on this branch or on `main`.** QEMU stops with
  `Not enough space for DTB after kernel/initrd`. With .bss included, the image ends at about
  0x40235000 (main 0x40235040), past the 0x40200000 the 1 MB `text_offset` layout needs. It is
  not the scheduler, since the SGI build fails the same way.
- **`/tmp/h` from tcc logs a SIGSEGV on both kernels at 8 MB.** It exits 0 according to `paws`.
  Not investigated.

**Trashcan.** The fair A/B rebooted the box from c523598a into 6a817703 at 21:27, and it has not
come back: no ping, no ARP. The six c523598a rounds before that were clean, 7–10 µs p50 with no
1 ms mode. The box needs a power cycle. Its GRUB default boots `/boot/akuma-amd64`, now
6a817703, and the fallback boots the untouched `.good`.

## Hooked: the idle CPU burn (2026-11-10)

An idle hooked guest kept a whole host CPU busy: the async-main and netpoll threads yielded to each
other for ever, so the idle thread never ran. Cause and fix are in `docs/archive/HOOKED_IDLE_CPU_BURN.md`.
In short: `idle_halt` parks for 1 ms instead of yielding (only under `yield-takes-bkl`), the netpoll
loop's hooked branch also applies without `smp-shared`, and the hooked idle loop credits its `wfi`.

Extreme-size, 8 MB, HVF: idle host CPU 100 % -> 0.1 %; `/proc/stat` over 5 s idle +1 `user`, +533 `idle`.

Devbox, Lima `fc`. Idle CPU is the QEMU process's share. The wake probe is a pipe wake through a
`spin` or `sleep` waiter, measured in µs.

| build | SMP | idle CPU | ssh RTT p50 | wake spin p50 / p99 | wake sleep p50 / p99 |
|---|---|---|---|---|---|
| SGI | 1 | 44.0 % | 124 ms | 2883 / 4053 | 399 / 3252 |
| hooked, before | 1 | 100.8 % | 52 ms | 58 / 1102 | 53 / 129 |
| hooked, park | 1 | 36.6 % | 60 ms | 1263 / 1767 | 1372 / 1811 |
| SGI | 4 | 145.3 % | 67 ms | 173 / 1278 | 231 / 641 |
| hooked, before | 4 | 154.1 % | 74 ms | 142 / 487 | 165 / 552 |
| hooked, park | 4 | 153.1 % | 78 ms | 140 / 493 | 183 / 548 |

At SMP=1 the park gives back the µs wakes the burn had bought; wakes now arrive on the 1 ms
deadline or the tick, still about 2x better than SGI's spin case. At SMP=4 the park changes nothing.

Gates on the park build:

- Boot suite: 314/0, 322/0 and 319/1 at SMP=1/2/4. The one failure is the known
  `test_epoll_socket_waker` latency flake (15.9 ms).
- futex suite: all probes pass at SMP=2 and 4.
- sigspin at SMP=2: 5/5.
- No panics, BKL-stuck lines or SIGSEGVs.

**Open:**

- About 150 % idle CPU at SMP=4 on *both* schedulers. Not yet attributed.
- A waker-driven park in the wait loop, to get µs wakes back at SMP=1 without the burn.

## The hooked scheduler preempts EL1 (2026-11-10)

The hooked path switched only on a tick that interrupted EL0, so kernel threads were cooperative,
and four workarounds had grown around that. Now the tick preempts EL1 under the SGI path's
gates, and the workarounds are gone. Gating it found that the hooked switch never published its
TTBR0 to the free gate's registry, which is now fixed too. Details:
`docs/archive/HOOKED_EL1_PREEMPTION.md`.

Gates, Lima `fc`. Step 1 is preemption plus the publish fix; step 2 adds the workaround removal.

| | SMP=1 | SMP=2 | SMP=4 |
|---|---|---|---|
| boot suite, step 1 | 313/1 | 322/0 | 319/1 |
| boot suite, step 2 | 313/1 | 322/0 | 319/1 |

The only boot-suite failure in either step is `test_epoll_socket_waker`'s latency threshold,
8.2–8.5 ms. `pmm_conserved` at SMP=2 passed 8 of 8 after the publish fix; before it, it failed
1 run in 3.

Both steps also passed these:

- futex suite, all probes, at SMP=2 and 4;
- sigspin, 5/5 at SMP=2;
- no panics, BKL-stuck lines or SIGSEGVs;
- clippy clean on hooked, SGI and amd64.

At SMP=1 the NEON/FP `_across_preemption` tests now really interleave two busy kernel threads,
with 0 errors.

Idle and wake measurements, step 2. The wake probe is a pipe wake, in µs.

| build | SMP | idle CPU | ssh RTT p50 | wake spin p50 / p99 | wake sleep p50 / p99 |
|---|---|---|---|---|---|
| SGI | 1 | 44.0 % | 124 ms | 2883 / 4053 | 399 / 3252 |
| hooked, park (previous) | 1 | 36.6 % | 60 ms | 1263 / 1767 | 1372 / 1811 |
| hooked, preemptive | 1 | 42.5 % | 138 ms | 2929 / 3675 | 2953 / 3663 |
| SGI | 4 | 145.3 % | 67 ms | 173 / 1278 | 231 / 641 |
| hooked, preemptive | 4 | 175.0 % | 74 ms | 83 / 1234 | 209 / 911 |

Extreme-size, 8 MB, HVF: idle host CPU 0.4–0.5 %.

**Reading.** At SMP=1 hooked now behaves like SGI, including SGI's slow wake: the socket waiter
`wfi`s until the tick, where the park bounded it at 1 ms. That is the price of one `idle_halt`
for both paths. The µs wake belongs in the wait loop as a waker-driven park, for both
schedulers (open item above).

The SMP=4 idle figure is one run each; the open item "~150 % idle at SMP=4 on both schedulers"
stands.

**Open:**

- amd64 still does not preempt ring 0. The reason `preempt_if_needed` gives, the heap lock, no
  longer holds: `akuma-alloc` masks interrupts on x86 too.
- The hooked switch's `x86_*` names and "cooperative-only" comments: rename them when the SGI
  path goes.

## Idle cost at SMP=4 (2026-11-11)

An idle 4-core devbox burned about 1.5 host cores on both schedulers. The cost has three causes,
each fixed: sshd polled every millisecond; every core ticked at 1 kHz and sent itself an SGI on
each tick; and two kernel loops halted with `wfi` instead of parking. Details:
`docs/archive/HOOKED_IDLE_TICK.md`.

The fix also gives back the µs wakes that the EL1-preemption step had traded away at SMP=1.

The gates ran on the hooked build, with the new sshd on a copy of `devbox.img`.

| | SMP=1 | SMP=2 | SMP=4 |
|---|---|---|---|
| boot suite | 314/0 | 322/0 | 320/0 |

`test_epoll_socket_waker` passed at every core count.

Also:

- futex suite: all probes pass at SMP=2 and 4;
- epoll suite at SMP=4: 17 PASS, 0 FAIL, the 1 known DIVERGE;
- sigspin: 5/5;
- no panics, BKL-stuck lines or SIGSEGVs;
- clippy clean on hooked, SGI and amd64.

Idle and wake measurements. The wake probe is a pipe wake, in µs.

| build | SMP | idle CPU | ssh RTT p50 | wake spin p50 / p99 | wake sleep p50 / p99 |
|---|---|---|---|---|---|
| SGI | 1 | 44.0 % | 124 ms | 2883 / 4053 | 399 / 3252 |
| hooked, preemptive | 1 | 42.5 % | 138 ms | 2929 / 3675 | 2953 / 3663 |
| **hooked, tickless idle** | 1 | **5.4 %** | **78 ms** | **74 / 158** | **126 / 527** |
| SGI | 4 | 145.3 % | 67 ms | 173 / 1278 | 231 / 641 |
| hooked, preemptive | 4 | 175.0 % | 74 ms | 83 / 1234 | 209 / 911 |
| **hooked, tickless idle** | 4 | **22.9 %** | 94 ms | **54 / 301** | 216 / 2286 |

Extreme-size, 8 MB, HVF: idle host CPU 0.4 %.

**Open:**

- ssh RTT at SMP=4 is 94 ms, against 67 ms for SGI. One run each.
- The SMP=4 sleep-wake p99 is 2.3 ms.

Both are worth an A/B before the SGI path goes.

## amd64 ring 0 preempts outside the BKL (2026-11-11)

New feature `ring0-preempt`, off by default. Write-up: `docs/archive/AMD64_RING0_PREEMPTION.md`.

What it does:

- amd64 syscalls run with `IF` set once they hold the BKL.
- The timer switches kernel code under AArch64's `el1_preemptible` gate, plus one extra condition:
  this core does not hold the BKL.
- The shared `x86_yield_now` now masks interrupts on both architectures.

What it found: the old reason in `sched.rs`, a heap spinlock, was out of date. The real reason ring
0 was never preempted is that `IA32_FMASK` kept every syscall masked.

TCG results:

| build | SMP=1 | SMP=2 | SMP=4 |
|---|---|---|---|
| default | 829/0 | 839/0 | 839/0 |
| `ring0-preempt` | 831/0 | 841/0 | 841/0 |

The two extra checks in the `ring0-preempt` build are a new one-core boot check: a ring-0 spinner
that never yields waits for a flag only another thread can set. Its negative control (the gate
forced `false`) fails both checks.

**Open:**

- Preemption with the BKL held, as AArch64 does, needs an audit of amd64's own BKL sections.
- KVM gates are still to run.

## Hooked is the AArch64 default (2026-11-11)

`hooked-sched` is now in the root `default` and in `scripts/build_extreme_size.sh`. The SGI path is
still built by `--no-default-features` without it, until the next step deletes it.

Builds that stay on SGI:

- the rump devbox (`build_devbox.sh`, deferred): rump with hooked has never been tested;
- the taoxichuan kernel, which does not use this scheduler.

The new default builds match the `hooked-nohz` builds gated above to within about 100 bytes.

Gates, Lima `fc` and KVM:

- Boot suite SMP=1: 314/0. SMP=4: 320/0.
- Boot suite SMP=2, four serial runs: 322/0 each. A run alongside the SMP=1 and SMP=4 suites in the
  same Lima VM (7 vCPUs) failed `orphan reparented to init` and the epoll-waker latency check. It
  took 53 s against 32 s alone, so CPU contention.
- Devbox SMP=4, new sshd: epoll 17/0 with the 1 known DIVERGE, futex all pass. No panics, BKL-stuck
  lines or SIGSEGVs.

Extreme-size under HVF, guest script uname / `tcc` hello / `sigspin 3`:

| build | RAM | `tcc` + run | `sigspin` |
|---|---|---|---|
| SGI | 8 MB | builds; `/tmp/h` SIGSEGVs (known, both kernels) | 2 PASS, 3 FAIL |
| hooked | 8 MB | same | ssh session lost; no kernel message |
| hooked | 16 MB | same | 5/5 PASS |

**Open:**

- The 8 MB regime is under memory pressure on both kernels, and worse on hooked. This is the
  ~300 KB gap recorded earlier. 8 MB is not a floor any acceptance playbook gates on now; the
  4 MB floor boots on neither build.
- The trashcan (amd64 metal) is down since the 2026-11-10 reboot and needs a power cycle, so the
  `ring0-preempt` KVM gate has not run.

## The SGI scheduler is deleted (2026-11-11)

One scheduler is left for both architectures: the hooked path.

**Line count:** 27 files, −2,109 / +78.

**What went:**

- `akuma-threading`:
  - `sgi_scheduler_handler_with_sp` and the `ThreadPool` picker `impl` (proportional weights,
    network-thread boost, displacement bypass, idle fallback, wake-deadline preemption);
  - the SGI-path slot initialiser and saved-context readers, and `adopt_current_as_core_idle`;
  - the `sgi_sched`/`hooked_sched` cfgs and the `hooked` feature.
- `akuma-switch`: the SGI trampoline asm.
- `akuma-entry`: the SGI vector table, its four `#[no_mangle]` handlers, `install_vbar`, `init`,
  and the secondaries' `set_shared_vbar`.
- `akuma-exceptions`: `irq_with_sp` and its poison check.
- `akuma-kernel-core`: the self-SGI on every tick.
- The `hooked-sched` features of the root, glue, entry, exceptions, kernel-core and exec.

The AArch64 root kernel's two scheduler choices, `yield-takes-bkl` and `collector-recycles`, are
now enabled by `akuma-kernel-glue`'s direct `akuma-threading` dependency.

**Effects:**

- Production `unsafe` sites: 420 → 414. The budget was ratcheted down.
- The default AArch64 binaries are within ~100 bytes of the hooked builds gated above. The SGI
  code was already compiled out of them, so the deletion removes source and changes nothing that
  ran.
- The rump devbox (`build_devbox.sh`) and the stage-1 `taoxichuan` feature build now get the
  hooked scheduler too. Neither is booted here: rump is deferred, and the Firecracker platform
  build has 2 clippy errors that predate this change.

**Mechanics:** a cfg folder (`hooked_sched` = true, `sgi_sched` = false) resolved attributes and
deleted the governed items. One range ended early and was caught by the compiler. Two attributes
were left orphaned above deleted items; they were found by a diff scan and fixed by hand. The
asm-reachable SGI code was traced and removed by hand, since `dead_code` cannot see it.

`docs/reference/subsystems/scheduler.md` now describes the hooked scheduler. Its SGI-era sections
(the `POOL` gate, the proportional weights, the open `POOL` wedge) are gone with the code. `gic.md`,
`thread-lifecycle.md` and the `verify-trim-fat-change` runbook row are updated.

**Gates (post-deletion binaries, run serially on Lima/TCG):**

| Gate | Result |
|------|--------|
| Boot suite, SMP=1/2/4 | 314/0, 322/0, 320/0 |
| Devbox SMP=4: `epoll_suite.py` | 17 PASS, 0 FAIL, 1 known DIVERGE |
| Devbox SMP=4: `futex_suite.py` | all probes pass; panics=0, bkl_stuck=0, segv=0 |
| `sigspin`, SMP=2 | 5/5 |
| extreme at 16 MB | `sigspin` 5/5; panics=0 |
| amd64 TCG, base SMP=1/2 | 829/0, 839/0 |
| amd64 TCG, `ring0-preempt` SMP=2 | 841/0 |

The extreme run's single SEGV is the `/tmp/h` tcc-hello fault, which the pre-deletion hooked
baseline shows identically. It is not a regression.

## The scheduler surface loses its `x86_` prefix (2026-11-11)

The switch, picker and spawn surface in `akuma-threading` had been named for the target it was
first written for. Both kernels now run it, so the prefix said something false.

- `X86ArchHooks` → `ArchHooks`, `register_x86_arch_hooks` → `register_arch_hooks`.
- `x86_<name>` → `<name>` for the picker, wake pass, slot lifecycle and debug readers
  (`claim_slot`, `seed_entry`, `pick_next`, `wake_pass`, `runnable_exists`, `earliest_wake_time`, ...).
- `x86_publish` → `publish_slot` and `x86_abandon` → `abandon_slot`, to read beside `claim_slot`.
- `x86_yield` (public, `-> bool`) and the private `x86_yield_now` were the same function; they
  are now one `pub fn try_yield() -> bool`, and `yield_now()` calls it.
- Kept: `x86_check_incoming_frame` and the `akuma_threading_x86_switch_context` asm symbol,
  which are genuinely x86.

Deleted with it: `YIELD_WITH_IRQS_MASKED` and its two DAIF boot tests. Only the SGI path ever
incremented the counter, so one test was already skipped and the other asserted that a dead
counter stays zero.

Gates: clippy clean for default, devbox-smoltcp, amd64 (base and `ring0-preempt`), taoxichuan
(both arches) and shanghai. Host tests pass; unsafe 414, addr 700. Boot suite 322/0 at SMP=2 and
320/0 at SMP=4; amd64 TCG 839/0, and 841/0 with `ring0-preempt`.

## M7a — the amd64 kernel is a library behind a two-symbol bin (2026-11-11)

The first move of M7, done the way B15 did it for the tokio kit: code moves under a library
first, and the front crate arrives after. `amd64/src/main.rs` became `amd64/src/lib.rs` (a
`git mv`, so its history follows), and the package gained a `[lib]` named
`akuma_kernel_amd64`, after the machine half it is headed to be (research doc §9.3). The new
`main.rs` is 21 lines: the two `#[unsafe(no_mangle)] extern "C"` symbols `boot.s` calls (`kmain`,
`kmain_mb2`), each calling the library's plain `kmain`/`kmain_mb2`. These are exactly the two
symbols the tokio kit's `entry!` expands to on amd64.

What did not move: the package and the bin keep the name `akuma-amd64`, so `-p akuma-amd64`,
`amd64/run.sh`, the metal `build.sh` and every `target/x86_64-unknown-none/*/akuma-amd64` reader
are unchanged. `build.rs` is untouched: `rustc-link-arg-bins` still reaches the bin, and the
`kernel_smp_shared` cfg and the `OUT_DIR` probe ELFs reach the library because it is the same
package. `#[global_allocator]`, `#[alloc_error_handler]` and `#[panic_handler]` moved with the
library (one per linked image, which is still true).

Gates (TCG): default SMP=1 829/0, SMP=2 839/0; `ring0-preempt` SMP=2 841/0. All three are
identical to the pre-split build. The image file is +10.7 KB (+0.04%, unstripped). `nm` shows
`kmain` and `kmain_mb2` at adjacent addresses. **multiboot2: not run.** The KVM rig is on the
ryzen's Pop install, which is down (only the ryzen's own Akuma answers), and the trashcan is down.
The `kmain_mb2` change is the symbol moving to the bin, which wraps `%rdi` in a `PhysAddr` (so the address-parameter ratchet does not count it twice).

## M7b — the `akuma-kernel` front; both kernels enter through `entry!` (2026-11-11)

`crates/akuma-kernel` (`forbid(unsafe_code)`, 60 lines) is the full kernel's front, built the
same way as `akuma-tokio-kernel`. `entry!` expands to the target's entry symbols, and each one
calls the machine half that the target selects:

| target | half | `entry!` expands to |
|---|---|---|
| `x86_64-unknown-none` | `akuma-kernel-amd64` (`amd64/`) | `kmain` → `kmain`, `kmain_mb2` → `kmain_mb2(PhysAddr)` |
| `aarch64-unknown-none` | `akuma-kernel-glue` | `rust_start(dtb)` → `start(dtb, before_main)` |

`entry!(f)` takes an optional `before_main` hook. The AArch64 root kernel passes
`register_boot_tests`, which registers its boot self-tests, because the suite lives in the bin
crate and glue cannot depend back on it. Its `rust_start` body moved into
`akuma_kernel_glue::start` unchanged: the print hook, the DTB and x0 lines, then `kernel_main`.
`src/main.rs` now holds the allocator, the panic handler, the test-suite modules and
`akuma_kernel::entry!(register_boot_tests);`.

amd64 needed one more package split. The bin cannot use the front while it shares a package
with the library: `akuma-amd64 → akuma-kernel → akuma-amd64` is a cycle. So:

- `amd64/` is now package **`akuma-kernel-amd64`**, library only. All `amd64/src/...` paths
  are unchanged. Its `build.rs` exports `linker.ld` as `links` metadata instead of passing
  `-T` itself.
- **`amd64/bin/`** is package **`akuma-amd64`**. It holds the 11-line `main.rs`
  (`akuma_kernel::entry!()`), a `build.rs` that passes the linker script on, and all 22
  features plus `default`, forwarded by name. So `-p akuma-amd64 [--features ...]`, `amd64/run.sh`,
  the metal `build.sh`, and every reader of `target/x86_64-unknown-none/*/akuma-amd64` keep
  working.

**Still two bins.** The research doc's end state is two thin bins over one front (option C).
Option B, one bin crate for both targets, was rejected there. The AArch64 bin is not thin yet:
its 35 kL of boot tests are still in `src/`.

**Gates.** Clippy clean: default, devbox-smoltcp, extreme, amd64 (base and
`ring0-preempt,no-tests`), taoxichuan (both arches), and `akuma-kernel` on the host. Unsafe stays at
414 and addr at 700. AArch64 boot suite: 314/0, 322/0 and 320/0 at SMP=1, 2 and 4. Devbox at SMP=4:
the epoll suite has 0 FAIL (1 known DIVERGE) and the futex suite passes every probe; panics=0,
segv=0. Extreme at 16 MB: `sigspin` 5/5, panics=0; its single SEGV is the same tcc-hello `/tmp/h`
fault the baseline shows. amd64 under TCG: 829/0 at SMP=1, 839/0 at SMP=2, and 841/0 with
`ring0-preempt`. `nm` shows `kmain` and `kmain_mb2`, now defined by the macro in the bin, at the
same addresses as M7a. **multiboot2: not booted** (the ryzen Pop rig and the trashcan are down).

## The AArch64 boot tests leave the bin: `akuma-kernel-tests-aarch64` (2026-11-11)

The 35 kL of AArch64 boot self-tests (`src/tests.rs` and its eight siblings) moved with `git mv`
into a new crate, **`akuma-kernel-tests-aarch64`**. It is gated as a whole to
`all(target_arch = "aarch64", target_os = "none")`, so it is empty on the host and under amd64. Its
one entry point is `register()`, which installs the `BootTestHooks` and the rump-tests hook.
`src/main.rs` drops to the allocator, the panic handler and
`akuma_kernel::entry!(akuma_kernel_tests_aarch64::register);`. The AArch64 bin is now as thin
as amd64's, which is the precondition for one bin over both targets.

- **cfgs.** The suite read eight `kernel_*` cfgs that the root `build.rs` derived for the bin. The crate
  derives the same eight from its own features in its `build.rs`: `kernel_tests`,
  `kernel_profile_extreme`, `kernel_smp_shared`, `kernel_bkl_profile`, `kernel_no_bkl_{network,process}`
  and `kernel_audio`. The root forwards 18 features to it by name.
- **Dependencies** are copied from the root manifest spec for spec, `default-features = false`
  included. `cargo tree -e normal` with features is identical to M7b's for every package except
  the new crate.
- **Repointed** every live reference to the old `src/<test>.rs` paths (42 files; `docs/archive`
  is left as history), plus the pre-commit hook's exemption regex.
- **addr ratchet** `--update`, net 0 (700). M7b had committed `akuma-kernel` at +1 without running
  the check, and the move repays it.

**Gates.** Clippy is clean in default, devbox-smoltcp, extreme, devbox (rump) and on the host. Host
tests pass, and the unsafe budget is unchanged at 414. Both images print the same 419 test strings,
and the extreme image is the same size.

*Boot suite, QEMU HVF on the Mac, two runs per cell, M7b and this build:* identical. SMP=2 323/0,
SMP=4 321/0, and SMP=1 314/1 in **both**. The one failure is a real bug that predates the move
(next section).

*Boot suite, Lima KVM rig:* devbox at SMP=4 gives epoll 0 FAIL (1 known DIVERGE) and futex all
pass. Extreme 16 MB gives `sigspin` 5/5. The boot suite at first failed `test_epoll_socket_waker`
7 of 7 (9.4–11.8 ms against the 8 ms bound). **That came from the laptop running on battery, not from the move:** rerun
on battery, the unchanged M7b image failed it the same way (10.7 and 10.4 ms) where it had passed 3/3
an hour earlier on mains. Back on AC power, this build gave 314/0 and 322/0 at SMP=1 and 2, and at SMP=4
the known flake (8.14 ms; `main` fails it at SMP=4 too, see W3b). **Run latency-bound gates on mains power.**

## AArch64 fork/clone reclaim was skipped since hooked became the only scheduler (2026-11-11)

HVF at `SMP=1` exposed it in the test-move gate on both images. The shared
`spawn_user_closure_initializing` answered exhaustion with amd64's
`"No free x86 task slots"`, so `spawn_user_thread_initializing`'s reclaim-and-retry,
which is keyed on `"No free user thread slots"`, never ran on AArch64. That is wrong there:
under `collector-recycles` `claim_slot` takes only `FREE` slots. The fix returns the
retryable constant when that feature is on, and leaves the amd64 path unchanged. Before
the fix 4/4 runs failed, after it 4/4 passed. Record:
[`AARCH64_SPAWN_RECLAIM_SKIPPED.md`](AARCH64_SPAWN_RECLAIM_SKIPPED.md).

## Feature pruning F2: `no-bkl-*` folded into `smp-shared` (2026-11-11)

This is the first pruning step before the single-bin merge. Six features left both kernels'
manifests; they were on in every build of both kernels, and `no-bkl-irq` had gated nothing since
the SGI deletion. Section sizes are unchanged, and the boot suites match the pre-change runs (HVF
314/1, 323/0, 321/0; amd64 TCG 829/0, 839/0). Details:
[`AKUMA_FEATURE_AUDIT.md`](AKUMA_FEATURE_AUDIT.md) § "F2 applied".

## `net-waker-park` / `net-direct-waker` removed (2026-11-11)

Both targeted socket parks were deleted with their code; `wait_until` is promiscuous on every
build, as it already was by default (`AKUMA_FEATURE_AUDIT.md`). Gates on AC power: HVF
`SMP=2` 323/0 and `SMP=4` 321/0. At `SMP=1` the result was 314/1, the open
`thread_slot_reclaim_on_spawn` `respawn_ok=false` shape (`AARCH64_SPAWN_RECLAIM_SKIPPED.md`).
On the Lima devbox at `SMP=4`, epoll had 0 FAIL and 1 known DIVERGE, futex passed every probe,
and there were no panics.

## amd64 `console-notify` / `linux-file-io` / `reboot-trace` unconditional (2026-11-11)

These were default-on and could not be turned off (no amd64 build drops its defaults), so the cfgs went.
amd64 TCG gate: 829/0 at `SMP=1` and 839/0 at `SMP=2`, the same as before.

## `many-sessions` / `small-sockets` removed (2026-11-11)

The listener backlog is 8 and the smoltcp table is 256 (32 on extreme) on both kernels,
which leaves one less networking difference between them. Details and gates are in
`AKUMA_FEATURE_AUDIT.md`.

## F5: rump out of the default (2026-11-11)

`rump` is opt-in; `rump-tests` and the `devbox` bundle are folded away. `rump-default` stays until
the AArch64 kernel reads a boot command line, which would let box 0's stack be chosen at boot
(`AKUMA_FEATURE_AUDIT.md`). The default AArch64 image is 81 KB lighter in `.text`. On this
point it now matches amd64, which never had rump.

## F4: `linux-extra-syscalls` (2026-11-11)

The seven `sc-*` families are one group on both kernels, under the same name. Images are
byte-identical (`AKUMA_FEATURE_AUDIT.md`).

## `extreme`, `userspace-sshd`, `fs-cache` removed (2026-11-11)

The 4 MB `extreme` build and its `kernel_profile_extreme` cfg are gone; the `extreme-size` profile
stays as codegen only. `fs-cache` is folded in, so every build, both arches and Taoxichuan
included, has the ext2 clock cache. Default images unchanged; the rump devbox gains 10.7 KB for
the cache (`AKUMA_FEATURE_AUDIT.md`). One cfg dimension fewer for the unification.

## `smp-shared` removed, `bkl-smp` on the shared crates (2026-11-11)

The single-core AArch64 build is gone; both kernels are shared-kernel SMP unconditionally and
const-assert `akuma_bkl::ACTIVE`. The 16 crates Taoxichuan shares keep the switch as `bkl-smp` /
`kernel_bkl_smp` because Taoxichuan still serializes under its own lock. Merging the two locks is
`proposals/ONE_BIG_KERNEL_LOCK.md`, deferred to the big merge. Taoxichuan images are
byte-identical; the Akuma images move by codegen noise only (`AKUMA_FEATURE_AUDIT.md`).

## The pre-merge dead-code sweep (2026-11-11)

The cheap pass before the merge:
- unused dependencies made optional;
- blanket `dead_code` allows removed;
- items with no caller deleted, including `gic::trigger_sgi`, whose last caller was a
  single-core arm;
- the two SGI-only `_across_yield` boot tests deleted.

It surfaced one live defect: SysV message queues outlived their box. That is fixed, with a boot
test (`MSGQUEUE_BOX_TEARDOWN.md`). The "which kernel's copy wins" dead code is left for the
merge (`AKUMA_FEATURE_AUDIT.md`).

Gates:
- HVF default: SMP=1 has the known `thread_slot_reclaim_on_spawn` failure only; SMP=2 and SMP=4
  are clean. Every prior test line is present, plus the new one.
- amd64 TCG: SMP=1 829/0, SMP=2 839/0.

## `thread_slot_reclaim_on_spawn` fixed: the boot suite is green at every SMP (2026-11-11)

`spawn_user_thread_fn_internal`, the path every `spawn_process` thread takes, claimed a slot once
and returned the error. Its reclaim-and-retry was unreachable code below an early `return`, so
on AArch64 a pool of cooled-down `TERMINATED` slots refused the spawn. Both spawn entry points
now share `claim_user_slot_with_reclaim` (`AARCH64_SPAWN_RECLAIM_SKIPPED.md`).

Gates:
- HVF default: SMP=1 (two runs), SMP=2 and SMP=4 all have 0 `[Test]` failures, with
  `thread_slot_reclaim_on_spawn PASSED` in each.
- amd64 TCG: SMP=1 829/0, SMP=2 839/0.

## Second dead-code pass: the SGI scheduler's residue (2026-11-11)

A scan for `pub` items with no caller found ~1,100 lines the SGI scheduler's deletion had left
compiling: its picker, `commit_switch`, its per-core flags, and the run-next hint. Two fixes
lived in that hint (the nanosleep floor of 2026-08-18 and the signal-wake half of the
`pthread_kill` fix) and have done nothing since the call switch took over. Recorded as open,
not re-implemented: [`SGI_SCHEDULER_RESIDUE.md`](SGI_SCHEDULER_RESIDUE.md).

Gates:
- HVF default: SMP=1/2/4, 0 `[Test]` failures; the PASSED set equals the previous run's minus
  the deleted `borrow_tracker_disabled_no_serial_flood`.
- amd64 TCG: SMP=1 829/0, SMP=2 839/0.

## The lock's SGI residue (2026-11-11)

Same scan, the BKL's side: `reconcile_for_spsr` (both forms), `KernelLock`'s ticket-free barge
acquire, the `no-bkl-irq` and `sched_bklfree_el0` toggles and two unread scheduler config
fields are gone. Every BKL hold is now ticketed. Removing the config fields found a third
behaviour the SGI picker took with it, the network-thread boost the rump sysproxy latency fix
relied on; it is on the same open list (`SGI_SCHEDULER_RESIDUE.md`). The M5c step-2 section of
`debug-smp.md` described a path that no longer exists and moved to
`M5C_BKLFREE_EL0_SCHEDULER_DEADLOCK.md`.

Gates: HVF default SMP=1/2/4, 0 `[Test]` failures and the same PASSED set as the previous
commit; amd64 TCG SMP=1 829/0, SMP=2 839/0; clippy 9 targets, host tests.

## `akuma-kernel-tests-amd64` and `akuma-drivers-amd64` (2026-11-11)

The first two rows of the three-way split planned above ("Added to the plan 2026-11-11"). One
commit per step, on branch `worktree-agent-ab5da208db8b5f991`. The amd64 TCG gate for every
commit was SMP=1 **829 passed, 0 failed** and SMP=2 **839 passed, 0 failed**, the same as the
baseline, with the check *names* diffed against the baseline run, not just the count. Ratchets:
`unsafe` 407 and untyped address parameters 692 at every commit. Sites moved between
components; the totals did not change.

| commit | what moved | budgets |
|---|---|---|
| `02b1c1b2` | the boot suite: `boot::self_tests` -> `crates/akuma-kernel-tests-amd64/src/suite.rs` (291 lines), `amd64/src/usermode/tests.rs` -> `usermode_tests.rs` (3 008, `git mv`) | none |
| `1eb782f2` | `amd64/src/uas.rs` (the `akuma-mmu` walker suite, 675) -> `uas_tests.rs` | none |
| `ecabfbb7` | the driver crate, with HD Audio (`hda.rs`, 1 092) behind `hda`; the sound class | addr amd64 83 -> 82, drivers 0 -> 1 |
| `06a80f57` | the I2C-HID touchpad (`touchpad.rs` 784 + `i2c.rs` 252) behind `touchpad`; the pointer class | addr amd64 82 -> 80, drivers 1 -> 3 |
| `9c3de257` | the framebuffer console (~850 lines of `multiboot2.rs` -> `fbcon.rs`) behind `fbcon`; the display class; the `headless` image | unsafe amd64/src 57 -> 56, drivers 0 -> 1; addr amd64 80 -> 79, drivers 3 -> 4 |

**`02b1c1b2` does not build alone.** Its hook was still running when `uas.rs` was renamed with
`git mv`, and the rename went into that commit without the edits that went with it. `1eb782f2`
completes it. The history was not rewritten. Bisecting across the pair needs both commits.

### The tests crate

- **How the kernel reaches the suite.** It can't depend on the suite: the suite calls the
  kernel, so that would be a cycle. `boot::self_tests` is now
  `(SELF_TESTS.require())(cx)`, an `akuma_primitives::Registered<fn(&SuiteCtx) -> Verdict>`.
  The suite fills that slot in `akuma_kernel_tests_amd64::register()`. The bin calls `register()`
  from its `before_main` hook, which is the new argument of `akuma_kernel::entry!`; both
  `kmain` and `kmain_mb2` call `before_main` first.
- **`no-tests`.** The bin names the crate only under `cfg(not(feature = "no-tests"))`, and
  the crate's own `lib.rs` is `#![cfg(not(feature = "no-tests"))]`, so a `no-tests` image links
  nothing from it. `boot::register_self_tests` is cfg'd out of the kernel too.
- **The probe ELFs.** The ELFs the suite runs (`USER_{HELLO,THREADPROBE,KILLPROBE,SIGREGS}_ELF`)
  were `rustc-env` of `amd64/build.rs`, so only `amd64` could see them. `amd64/build.rs` now
  emits them as `links` metadata. The tests crate's `build.rs` reads them as
  `DEP_AKUMA_KERNEL_AMD64_USER_*_ELF` and re-emits them for itself.
- **Visibility.** The suite reaches the kernel only through `akuma_kernel_amd64::testing`.
  That module is `#[doc(hidden)] pub` and cfg'd out under `no-tests`, and it lists one
  re-export per item, so the list is the whole test-only surface. The items behind it were
  made `pub` inside their still-private modules, which is the narrowest visibility a
  re-export from another crate allows.
  - Per-file `smoke_test`s: `blk`, `fd`, `fs`, `idt`, `lapic`, `mem`, `mm`, `net`, `paging`,
    `pci`, `pipe`, `reboot`, `sched`, `shootdown`, `signal`, `smp`, `sock`, `uaccess`, `xhci`.
  - Boot plumbing: `boot::{bring_up_secondaries, wire_console_and_syscalls, suite_emit,
    SuiteCtx, Verdict}`.
  - `usermode`'s process and spawn surface (`Image` and its fields, `spawn_process_task`,
    `SPAWN`, `sys_spawn`/`sys_waitpid`/`sys_write`, `syscall_dispatch`, `to_glue`,
    `WRITE_SEQ*`, ...).
  - `loader::{load, read_user, LoadedImage}`.
  - The `fd` syscalls the tests drive, and seven errnos.
  - The `sched` counters.
  - `thread`'s group-exit setters.
- **New pub items needed new lints.** Once exported, items attracted clippy's
  `must_use_candidate` and `too_long_first_doc_paragraph`. They got `#[must_use]`, an
  `allow` with a reason where callers ignore the result, or a split first paragraph.
- **What did not move.** The per-file inline suites stay in `amd64/src` (92 `no-tests` cfgs
  remain there, some of them bring-up switches rather than tests; the largest suite is `fd.rs`'s
  ~1.1 kL). They read module-private state (`fd`'s table internals,
  `install`, `covered_by`, `console::channel`), so moving them means either widening that
  state or rewriting the tests against the public surface. The suite reaches them as
  `testing::<module>::smoke_test`. They are the next candidate once a module's state has a
  public face of its own.

### The drivers crate

- **The seam.** `amd64/src/drivers.rs` has one `Registered` slot per class, each holding a
  table of function pointers: `Sound` (`init`), `Pointer` (discover, init, poll, present,
  has_events, `/dev/input/mouse0` read and write, console bridge) and `Display` (20 entries,
  held as `&'static` because `mirror_byte` runs per printed byte). The machine half calls
  forwarding functions that read "absent" when nothing registered: `ENODEV`, `0`, `false`,
  `None`, or no-op. `akuma_drivers_amd64::register()` fills a slot for each driver its
  features compiled in, so the machine half has no driver `cfg` at all.
- **What a driver may use.** Drivers reach the kernel only through `drivers::services`, one
  re-export per item, the drivers' counterpart of `testing`.
- **Features.** One per driver: `fbcon`, `hda`, `touchpad`. `all` enables all three. On the
  bin, `all-drivers` is in `default`, so the default image is unchanged. Each driver is also a
  bin feature of its own.
- **`unsafe`.** The crate is `deny(unsafe_code)`. It arrived with one `allow`, on `fbcon` for
  its `MmioWindow::new`, and took 15 more with `kvm` (16). Now 7, all one kind; see "Shrinking
  the drivers crate's `unsafe`" below.
- **Verification.**
  - `hda` was booted under QEMU q35 with `intel-hda`: `[HDA] ready (/dev/dsp)`, and the
    self-test tone played.
  - `touchpad` and `fbcon` are compile-verified only. TCG boots the PVH path, which has no
    framebuffer and no I2C-HID, and their probes run on the multiboot2 path, which needs GRUB.
    **Their first boot is the metal.**
- **No longer reachable through `crate::multiboot2::`.** On the `fbcon` move, everything the
  console did went behind `drivers::` under the same names. The machine half keeps
  `ega_text`, the output of last resort, and the `fbtrace` stand-in.
  `drivers::mirror_byte` falls back to `multiboot2::fb_trace_byte` when no console drew the
  byte, which is what the old `else` arm did.
- **Not moved yet, in the machine half for these reasons:**
  - **`xhci`**: the USB keyboard *and* USB storage (`root=/dev/sda1`). The headless image
    needs it, so it was not first in line; it is a block-and-input class.
  - **`kvm`**: moved later, in `887d601f` (below).
  - **`nvme`, `ramdisk`, `blk`**: the boot disk. They need the block class.
  - **`rtw89`/`rtw89_sta`/`wifi`**: moved later, in `337cdb7b` (below).
  - **`fbdev`**: `/dev/fb0`'s mmap and `FBIOGET_*`, part of `fd`/`mm`. It is inert without a
    display driver, because nothing registers a geometry.
  - **`splash`**: inert without a console, because `splash::begin` is gated on
    `console.is_ok()`.
  - **`input`/`kbd`**: the i8042 and the console's input queue, used by every boot.
  - **`pci`**: enumeration is shared by the NIC, the watchdog and reboot, and the drivers
    reach it as a service.

### The `headless` image

`--no-default-features --features headless` on the bin (`NO_DEFAULT_FEATURES=1 FEATURES=headless
sh amd64/run.sh` under TCG) builds the machine half's defaults and the boot suite with no
optional driver. That means no framebuffer console, no splash, no `/dev/fb0`, no sound and no
touchpad, and `akuma-fbcon`, `-hda`, `-touchpad`, `-i2c-hid` and `-dwi2c` are absent from
`cargo tree`. The console is the serial port. On the multiboot2 path the banner says
`fb: none, headless (no display driver in this build ..)`, the same as on a machine without a
framebuffer tag, and GRUB's framebuffer is not touched.

- **What it keeps, and why.** It keeps what a test box needs. All of it is in the machine
  half rather than behind a feature: serial, the GRUB-module ramdisk, NVMe, xHCI (keyboard and
  USB storage) and the RTL8169. **xHCI stays deliberately**: on a box whose keyboard or root
  disk is USB, it is the only path to either.
- **Gates.**
  - clippy: clean for `headless` and for `headless,no-tests`.
  - TCG: SMP=1 **829/0** and SMP=2 **839/0**, check names identical to the default image's.
    **No test drops out**, because the suite never tested a driver that left: TCG has no
    framebuffer, HDA or touchpad, and none of their probes is a suite check.
- **Size.** `rust-size` text (Berkeley, so `.text` plus `.rodata`) is 25.38 MB for the
  default image and 4.54 MB for `headless` (`akuma-fbcon`'s baked fonts are most of the
  difference). Per-section numbers after the next round are below.
- **Not verified.** A headless image booting through GRUB on trashcan: the multiboot2 path
  cannot be booted here.

Docs: `docs/reference/build-profiles.md` § amd64 images; `config-flags.md` (`all-drivers`,
`fbcon`, `hda`, `touchpad`, `headless`, `NO_DEFAULT_FEATURES`).

### Second round: stripped guest programs, Wi-Fi and KVM (2026-11-11)

Prompted by measurements of the `headless` image (`llvm-nm --size-sort`, `llvm-size -A`). Same
gates per commit: TCG default and headless at SMP=1 **829/0** and SMP=2 **839/0** with identical
check names; clippy for default, `no-tests`, `ring0-preempt`, `headless` and
`headless,no-tests`, plus each new feature alone and in `headless`. The ratchet totals stayed
at unsafe 407 and addr 692.

| commit | what | budget moves |
|---|---|---|
| `a58becf2` | `amd64/build.rs` builds the guest test programs with `-C strip=debuginfo` | none |
| `337cdb7b` | `wifi.rs`, `rtw89.rs`, `rtw89_sta.rs` (2,630 lines) behind `wifi`; the wireless class | none (the three files hold neither) |
| `887d601f` | `kvm.rs` + `kvm/` (8 files, 8,674 lines) behind `kvm`; the hypervisor class | unsafe amd64/src 56 -> 41, drivers 1 -> 16; addr amd64 79 -> 62, drivers 4 -> 21 |

**The guest programs.** `sigregs.elf` was 742,760 bytes, about 707 KB of it `.debug_*`, and
`fdprobe.elf` was 737,808. They are now 20,160 and 15,200. `hello` (13,192), `threadprobe`
(9,544) and `killprobe` (9,240) did not change; they had no DWARF to begin with. Nothing reads
the debug info: not the loader, not the suite, not `mkdisk.sh`. The symbol table stays (a few
KB) so `llvm-nm` can still name a faulting guest `rip`. The headless image's `.rodata` fell
722,624 bytes.

**The wireless class** (`drivers::Wireless`, 10 entries) is the edge between a radio and the
stack, not a general net-device registry. The RTL8169 stays `net`'s own wired NIC through
`akuma-net`'s `ExternalDevice`. A wireless driver hands `net` its `FrameQueues` (`link`), which
`net` makes the interface when `is_link`. The other entries:
- **Probe steps.** `init` registers `/dev/wifi0`'s backend; `card_init` runs after the root
  mount, because the firmware is a file; `spawn` starts the station daemon.
- **Reset.** `shutdown_for_reset` stops the card's DMA before a warm reset.
- **The device node.** `/dev/wifi0`'s `present`, `dev_read`, `dev_write` and `release`.

`KCMDLINE=wifisim` under TCG shows the default image registering `/dev/wifi0` through the class
and the headless one having none. The card itself is compile-verified only; it exists on ryzen.

**The hypervisor class** (`drivers::Hypervisor`, 21 entries) is the char-device edge for the
three nodes it serves (`/dev/kvm0`, `/dev/kvm`, `/dev/net/tun`). It carries `fd`'s open, read
and ioctl arms, `usermode`'s write, writev, readv and close arms, `fork`'s and `mmap`'s notes,
the boot hooks and the poll family's `tun_has_frame`. The descriptor kinds stay in `fd`, because
they are the fd table's vocabulary.

Without the driver, nothing opens. `/dev/net/tun` used to open unconditionally and is now gated
on `hypervisor_present`. The `mmap` note's forwarder takes a `VirtAddr`, so the seam adds no
untyped address parameter.

Moving `kvm/ctl.rs` into `crates/` exposed a hand-rolled `fmt::Write` buffer. The pre-commit
console-print rule scans `crates/*/src` but not `amd64/src`. It is now
`akuma_primitives::console::StackWriter<192>`, with the same truncation. The SVM path is
compile-verified only, because TCG has no SVM.

**Image sizes** (`llvm-size -A`; the first row is the coordinator's measurement before this
round):

| image | step | `.text` | `.rodata` | `.bss` |
|---|---|---:|---:|---:|
| headless | before | 2,910,991 | 1,632,480 | 5,728,408 |
| headless | stripped ELFs | 2,910,991 | 909,856 | 5,728,408 |
| headless | + `wifi` off | 2,794,047 | 328,600 | 1,193,368 |
| headless | + `kvm` off | **2,452,479** | **249,752** | **1,149,848** |
| headless + no-tests | before | 2,698,575 | 810,264 | (5,696,920) |
| headless + no-tests | + `wifi` off | 2,606,367 | 237,072 | 1,161,624 |
| headless + no-tests | + `kvm` off | **2,255,855** | **154,576** | **1,118,360** |
| default | after all three | 3,053,311 | 21,598,896 | 5,811,096 |

The headless + no-tests `.bss` "before" figure is from my own build at the start of the round.
That image embeds no guest programs, so stripping them does not change it. Headless, end to end:
`.text` -16%, `.rodata` -85%, `.bss` -80%.

**Still in the machine half:** `xhci` (keyboard and USB storage), `nvme`/`ramdisk`/`blk` (the
block class), `fbdev`, `splash`, `input`/`kbd`, `pci`. The reasons are in the first-round list
above.

## Shrinking the drivers crate's `unsafe` (2026-11-11)

`akuma-drivers-amd64` went from 16 production `unsafe` sites to 7. The question was whether it could
carry `forbid(unsafe_code)`; the answer is "not yet", and the 7 are the reason.

- **`kvm/api.rs`, the vCPU box (1 -> 0).** `boxed` built a `Box<[T; 1]>` with `Box::from_raw`. It now
  goes through `Vec::into_boxed_slice().try_into()`, which is the same allocation, safely.
- **`fbcon`, the framebuffer window (1 -> 0 here, +1 in `amd64/src`).** The `MmioWindow::new` over
  GRUB's framebuffer moved into the machine half: `multiboot2::framebuffer_window()` records the
  boot framebuffer's address and length from the boot info, checks it is an RGB framebuffer inside
  the physmap, and hands the window out **once per boot**. The claim "these bytes are the
  framebuffer and nobody else maps them" is a fact about the boot, so the boot code makes it. fbcon
  now has no `allow`.
- **SVM (13 -> 7): `akuma-svm-switch-amd64`.** A new leaf crate, the only one that executes `VMRUN`,
  `VMSAVE`, `VMLOAD` or writes `EFER.SVME`/`VM_HSAVE_PA` (6 sites, budget 0 -> 6).
  - `enable(HostSaveArea)` is safe. A `HostSaveArea` is made only by giving up a `FreshFrame` and is
    consumed by `enable`, so one page is never two cores' save area. `enable` checks CPUID and
    `VM_CR.SVMDIS` before touching either MSR. The per-core reserve became
    `[Spinlock<Option<HostSaveArea>>; MAX_CPUS]`.
  - `HostState::save` is safe: `VMSAVE` into a page the value owns alone.
  - `run`/`run_with_regs` stay `unsafe fn`, and their 7 call sites in the wiring are what is left.
    The obligation is that the guest reaches only what it owns. That is a property of the VMCB's
    nested page table and permission maps. The wiring's guest RAM is physical ranges carved from the
    `kvmmem=` reserve (or a contiguous PMM run) and tracked by arena offset, not by an owning type,
    so no type can state it. Closing it means a guest-memory type that owns the carved range, an
    `Npt` built only from such types inside the leaf, and a check at `run` that the VMCB has nested
    paging on with `nCR3` at that `Npt`. That is a rewrite of `Ram`, the memslot table and both
    `build_npt`s, and it can only be verified on SVM hardware.
- **Budget.** Drivers 16 -> 7, leaf 0 -> 6, `amd64/src` 41 -> 42: 405 in total (from 407).
- **Verification.** Clippy is clean on the default, `no-tests`, `ring0-preempt`, `headless` and
  `headless,no-tests` images; the amd64 TCG boot suite passes: default and headless, SMP=1 829/0 and SMP=2 839/0. **The SVM path is
  compile-verified only**: TCG boots PVH with no guest, the trashcan has no SVM, and rig kernels do
  not go on the ryzen metal. The asm is byte-for-byte the old asm with the operands renamed.

### `akuma-kvm-amd64`: the drivers crate forbids `unsafe` (2026-11-11)

Typing guest memory is deferred (it needs the ryzen). In the meantime the hypervisor moved out of
`akuma-drivers-amd64` into its own crate, `akuma-kvm-amd64`, with `git mv` (the module tree is
unchanged; `crate::kvm::` became `crate::`). The drivers crate's `kvm` feature now just pulls it in,
and `register` installs `akuma_kvm_amd64::HYPERVISOR`.

- `akuma-drivers-amd64` is `#![forbid(unsafe_code)]`: 103 of 141 crates forbid.
- `akuma-kvm-amd64` is `deny(unsafe_code)` with an `allow` on each of the seven guest-entry
  statements, not on the crate or a module, so new `unsafe` anywhere else in it does not compile.
- The dependencies only the hypervisor used moved with it, with the same specs and default
  features, so feature unification turned nothing on.
- Moving the module to a crate root surfaced pedantic lints a private module hid
  (`redundant_pub_crate`, `must_use_candidate`, long first doc paragraphs); fixed, no behaviour change.
- Gates: clippy clean on six feature sets (incl. `headless,kvm`); amd64 TCG default and headless SMP=1 829/0, SMP=2 839/0.
- Budget: drivers 7 -> 0, `akuma-kvm-amd64` 0 -> 7, total unchanged at 405. The 13 untyped address
  parameters moved with the code (`addr-params-budget.json`).
- Stale `amd64/src/kvm/` paths in live docs and code comments (stale since `887d601f`) now point
  at the crate.

### Future work: host tests for `akuma-kvm-amd64`

The crate is gated to `cfg(all(target_arch = "x86_64", target_os = "none"))` like the drivers crate,
so `cargo test` never compiles it. Its pure parts would be tested the way `akuma-svm-amd64`'s are, by
moving them down into that crate (or a sibling with no kernel dependency) and leaving the wiring here.
Candidates, by how little they touch the kernel (counted 2026-11-11):

- **`api/platform.rs`** (238 lines, no `services::`): the in-kernel PIT/PIC/LAPIC aggregate behind
  `CREATE_IRQCHIP` and its `offer`/`accept`/`next_event_us`. It already depends only on
  `akuma-svm-amd64`'s models and two `linux.rs` items.
- **`relay.rs`** (703 lines, no `services::`): the gateway's outbound TCP/DNS/forward state against host
  sockets. Its decisions (slot lifecycle, `OutPhase` transitions) can be split from the `akuma_net::socket`
  calls behind a small trait, the `akuma-net-yarn` "callers supply only the effects" shape.
- **`linux.rs` pure functions:** `get_reg`/`set_reg` (the ModRM register-number map over
  `GuestRegs` + VMCB), `init_vmcb`, `set_pvh_state`/`set_startup_state` (checkable against
  `akuma-svm-amd64`'s §15.5.1 consistency checks), `spec_cmdline`/`cmdline_with_tsc`, and `mmio_emulate`
  over a fake `MmioDev`.
- **ioctl argument decoding in `api.rs` / `api/vcpu.rs`** (memslot checks, `KVM_SET_REGS` and
  `KVM_SET_SREGS` layouts) where it is not already in `akuma-kvm-abi`.

None of this needs SVM hardware, which is the point: today every change to these paths is
compile-only until it reaches the ryzen.

### Future work: a sweep of crates without host tests

`akuma-kvm-amd64` is not the only crate `cargo test` never exercises. A survey of all 141 crates
(2026-11-11: `#[test]` count per crate, its line count, and whether `lib.rs` or the manifest gates it
to `target_os = "none"`) finds 31 with **no** host tests and several large ones with very few. They
split into crates that cannot be host-tested, where the boot suites are the coverage, and crates
that could be and are not. This is a first pass, from the survey and each crate's stated purpose,
not from reading every crate; the sweep itself should confirm each classification.

**Cannot be host-tested, by design** (the boot suites cover them):

- *Kernel-only by `cfg`:* `akuma-drivers-amd64`, `akuma-kvm-amd64`, `akuma-svm-switch-amd64`,
  `akuma-tokio-kernel-aarch64`, `akuma-tokio-kernel-amd64`, and the kernel fronts `akuma-kernel` and
  `akuma-tokio-kernel`. `akuma-kernel-tests-aarch64` and `akuma-kernel-tests-amd64` *are* the boot
  suites (about 38k lines of in-kernel tests).
- *Instruction and MMIO leaves*, whose whole job is one `asm!` or one register window:
  `akuma-boot-aarch64`, `akuma-boot-amd64`, `akuma-entry`, `akuma-el0-entry`,
  `akuma-vectors-aarch64`, `akuma-switch`, `akuma-sched-amd64`, `akuma-percpu-gdt-amd64`,
  `akuma-mmu-raw`, `akuma-frame-raw`, `akuma-gic`, `akuma-psci`, `akuma-uart`, `akuma-comconsole`.
  `akuma-cpu` (7 tests) and `akuma-idt-amd64` (4) test the little that is not an instruction, which is
  the right amount.
- *Physmap access:* `akuma-phys` dereferences the kernel's physmap; its contracts are exercised by
  every boot-suite allocation.
- *Compile-time constants:* `akuma-config` is consts with nothing to execute. A test would only
  restate the values.

**Lacking coverage, and testable** (the sweep's real work):

- `akuma-fpcache` (558 lines, `forbid`): a keyed, refcounted page cache. Its insert, lookup, evict
  and cap decisions are pure apart from the frame source, which a fake can supply.
- `akuma-syscalls-ipc` (497): System V message queues. Queue state and the
  `msgget`/`msgctl`/`msgsnd`/`msgrcv` errno rules are pure; only the user copies are not.
- `akuma-syscalls-time` (613): its tests left with the SNTP code for `akuma-sntp` (`2fb18cb7`), so the
  itimer and `adjtimex` logic still here is untested. `docs/reference/subsystems/syscalls/time.md`
  still says the `sntp`/`boot` submodules are host-tested "in the same crate"; fix that in the sweep.
- `akuma-syscalls-log` (3 tests, 272 lines): the per-pid trace rings' wrap and retain-past-exit rules.
- `akuma-alloc` (1.4k): the growth, backoff and reclaim **policy** around Talc. Several fixed bugs
  (`heap_growth_backoff_fix`, the heap->PMM reclaim) were policy bugs. The policy can be driven
  against a host-memory arena; only the Talc hookup needs the kernel.
- `akuma-exceptions` (4.5k, `forbid`, 0 tests): the largest untested crate. The fault classification
  (ESR decode to action) and the CoW-break bookkeeping are decisions, and `akuma-cow` shows the
  pattern for pulling one out. The ~500-line break path is deliberately welded to `akuma-exec`
  (see `akuma-cow`), so expect a partial extraction, not a whole-crate test suite.
- `akuma-exceptions-forensics` (163): feature-gated diagnostics. Low priority; test only if it is
  kept.
- `akuma-virtio` (2 tests, 2.5k): split-ring and descriptor bookkeeping can run over host memory,
  like `akuma-dma`'s fake. Today most of it is covered only by booting.
- `akuma-net-nic` (8 tests, 3.4k): the frame arenas and lease rules are host-testable. The DMA
  doorbells are not.

**Glue, where the rule is "extract, don't test in place":** `akuma-kernel-glue` (4.6k, 0 tests),
`akuma-vfs-glue` (4k, 2), `akuma-syscalls-glue` (20.9k, 5), `akuma-kernel-core` (2k, 0) and
`akuma-tokio-kernel-core` (4.1k, 18). By the naming rule a `-glue` crate is the part that cannot be
host-tested. The sweep's task here is to find pure logic that has collected in them anyway (decision
tables, wire formats, errno rules) and move it down into a family crate, as was done for
`akuma-syscalls-{time,sync,poll,mem,net}`. It is not to add tests to the glue.

How to run the sweep: regenerate the counts (a short script over `crates/*/` counting `#[test]`
against lines; keep it next to `scripts/cloc_akuma.py` so the numbers are reproducible). Then work
through the "testable" list largest-risk-first, taking `akuma-exceptions`, `akuma-alloc` and
`akuma-fpcache` before the small ones, since those three have the most bug history in
`BUG_FIX_LIST.md`.

## The storage and keyboard drivers join the drivers crate (2026-11-11)

xHCI (2.5 kL), NVMe (0.6 kL) and the i8042 keyboard (0.5 kL) moved from `amd64/src` into
`akuma-drivers-amd64` with `git mv`, each behind its own feature (`xhci`, `nvme`, `i8042`; all three
in `all`, and in the new `storage-and-input`, which the bin's `headless` now enables). Three new class
slots in `amd64/src/drivers.rs`, in the existing shape (a `Registered` table of function pointers,
forwarders that read "absent"):

- **`Usb`**: init, shutdown, the `/dev/sda` byte I/O and flush, the boot keyboard, and the boot
  suite's `smoke_test` (an `Option`, `None` under the drivers crate's new `no-tests`, which the bin's
  `no-tests` forwards).
- **`Nvme`**: init and `open_partition` for the root, byte I/O, flush, shutdown, and the guest
  partition the hypervisor uses. `services::nvme` now re-exports the forwarders under the old names,
  so `akuma-kvm-amd64` did not change.
- **`Keyboard`**: the i8042's init, `getb`, `has_byte`. `input`'s three-source poll asks the two classes.

What did **not** move, and why:

- **`quiesce_all` became the machine half's `pci::quiesce_xhci_controllers`.** It stops any xHCI
  controller the *firmware* left bus-mastering into this kernel's memory (the warm-reboot crash loop).
  A build without the USB driver needs it most, so it cannot live behind the driver's feature. The
  driver's `shutdown` still calls it as its belt-and-braces last step.
- **`SDA1_OFFSET` / `mbr_looks_right`** (the USB root's partition layout) went to `fs.rs`, beside
  `UsbDisk`. The root search owns that knowledge; the driver's smoke test reads it through `services::fs`.
- **`blk.rs`** (virtio-blk) stays. It is not a driver: it points the shared `akuma-virtio` driver at
  the transport window `MachineFacts` reports, which is machine-half work (AArch64 does it in its own glue).
- **`pci`, `ramdisk`, `input`, `fbdev`, `splash`** stay. `pci` is the enumeration every driver binds
  through (a service already), `ramdisk` is a GRUB module in memory, and the other three are registries
  that the console, `fd`, `mm` and the scheduler call on every path, inert without a driver behind them.

**`unsafe` went down, not sideways.** `kbd.rs` carried a hand-rolled bounded lock (`UnsafeCell`, an
`unsafe impl Sync`, a `&mut *` in the guard), two sites. It is now `spinning_top::Spinlock::try_lock`
in the same bounded loop (give up after `1 << 22` attempts and report "no key"), so the
drivers crate stays `forbid`. `amd64/src` 42 -> 40; total 405 -> 403. NVMe and xHCI held no `unsafe`
(BAR windows from `pci::map_bar_window`, `DmaArray` for DMA). Four untyped address parameters moved
with the code.

One function lost its only caller: xHCI's `capacity_sectors` was used only by its smoke test (as
`pub` in the machine half nothing flagged it); it is now `cfg(not(feature = "no-tests"))`.

Gates: clippy clean on seven amd64 feature sets (default, `no-tests`, `ring0-preempt`, `headless`,
`headless,no-tests`, `headless,kvm`, and `--no-default-features` with no driver at all); TCG default
and headless SMP=1 829/0, SMP=2 839/0; AArch64 `cargo check`. **Compile-only:** none of the three drivers
runs under TCG's PVH boot (no PCI scan, no i8042 probe on that path), so their first boot is the
ryzen rig or the metal.

## The convergence, re-evaluated against the tree (2026-11-11, evening)

"The big merge" is one kernel build for both architectures: the neutral half shared, two machine
halves. With the drivers out, `amd64/src` is 33 kL. What is still a second copy of something AArch64
gets from a shared crate turns out to be much narrower than the 9.3 plan assumed. The checks behind
that:

- **Already shared:** the process model (`akuma_exec::process::Process`), the fd table (both kernels'
  `SharedFdTable`; amd64 adds a `KERNEL_TABLE` fallback for kernel tasks with no process, which the
  boot suite runs as), the address-space type (`akuma_mmu::UserAddressSpace`, which grew an x86_64 walker
  in B3), the scheduler (`akuma-threading`'s hooked path) and the whole of `akuma-syscalls-glue`, which
  amd64 builds with the same `akuma-native` feature set as AArch64. Glue's families no longer name the
  AArch64 walker (`akuma_mmu`: 0 references in `fs`/`mem`/`net`/`sync`/`signal`/`proc`).
- **The dispatcher is mostly glue already:** 95 of the 136 decoded arms in
  `amd64/src/usermode/dispatch.rs` hand the call to glue (`to_glue`, which converts the number to
  asm-generic). The x86-only legacy spellings (`open`, `stat`, `fork`, ...) stay amd64's by rule, since
  they have no asm-generic number.
- **What is still local**, and is the convergence:

| family | amd64 copy | glue's | why it stayed local | plan |
|---|---|---|---|---|
| sockets | `sock.rs` (818 L, wiring over `akuma_net::socket`) | `net.rs` | both wire the same crate; never folded | **done** (Slice 1, below) |
| futex | `futex.rs` (450 L, effects over `akuma-syscalls-sync`) | `sync.rs` | glue's used `akuma_exec::threading` when amd64 had its own scheduler (the `amd64/Cargo.toml` note is from then); both are on `akuma-threading` now | **done** (Slice 2, below) |
| signals | `kill`/`tkill`/`tgkill` via `akuma-syscalls-glue-amd64::kill`; `rt_sigreturn` and delivery | `signal.rs`/`proc.rs` | the x86 `kill -9` fix went into the x86 copy | diff the two, keep the stronger, one copy; sigframe and `rt_sigreturn` stay per-arch |
| fd I/O | `fd.rs` (3.8 kL): read/write/open/stat/dup/fcntl/ioctl/poll/select/getdents | `fs.rs`/`poll.rs` | device kinds glue does not know (`DevKvm`, `DevTun`, `DevWifi`, `DevFb`, `DevMouse`, `DevPower`) and x86 `struct stat`/open-flag translation | device kinds through `SyscallHooks` (it already carries `dev_mouse_has_events`/`dev_tun_has_frame`); x86 ABI translation from `akuma-syscalls-glue-amd64`, which the tokio kernel already uses for exactly this; then route |
| mm | `mm.rs` (3.5 kL) | `mem.rs` | shared-writable file pages (`shmpages`), `/dev/fb0` and `/dev/kvm` mappings, the fork walk (M3) | last: diff per call, hooks for the device mappings |
| threads | `thread.rs` `clone(CLONE_VM)` | `proc.rs` clone | the per-arch TLS/stack register setup | glue's decision, arch hook for the register file |

Order: sockets, futex, signals, fd, mm, threads. Small to large, and each slice is independently
revertible. Per slice: route the arms to glue, delete the local body, then gate with clippy on every
amd64 feature set, the amd64 TCG suite at SMP=1/2, the `c_stress` probes for that family through the
amd64 harness (`scripts/utils/amd64_*.py`), the AArch64 HVF suite where glue itself changed, and the
ryzen Firecracker lane before the slice counts as done. Every divergence found between the two copies
gets pinned in the slice's commit (research doc §7), not silently resolved.

### Slice 1: sockets (2026-11-11)

Every AF_INET socket arm in `amd64/src/usermode/dispatch.rs` now calls `to_glue`. That covers
`socket`, `bind`, `listen`, `accept`, `accept4`, `connect`, `sendto`, `recvfrom`, `sendmsg`, `recvmsg`
and `setsockopt`. The AF_UNIX guards went with them, because glue's `net::dispatch_*` already decide the
family from the domain at `socket` and from the descriptor after it.

`amd64/src/sock.rs` went from 818 lines to its kernel-only parts:
- the `recvfrom` busy-loop tripwire, which reads this kernel's task slots;
- the `close` hook that `exec_runtime` hands `akuma-exec`;
- the boot self-test, which now calls `usermode::syscall_dispatch` by x86_64 number, so each check
  also proves its arm routes to glue.

`fd::alloc_socket_fd`, `fd::copy_out` and `errno::EDESTADDRREQ` lost their last callers and were
deleted.

Both copies wrapped the same `akuma_net::socket` table, so they differed only at the argument
boundary. The divergences found:

| | amd64's copy | glue's (AArch64) | now (both) |
|---|---|---|---|
| `socket(AF_INET, 99)` | `EINVAL` | `EAFNOSUPPORT` | `EINVAL`, as Linux gives |
| unknown flag bit in `socket`'s type | ignored | ignored (masked `& 0xff`) | `EINVAL`, as Linux gives |
| AF_INET call on an open non-socket fd | `ENOTSOCK` | `EBADF` | `ENOTSOCK` (`net::not_a_socket`); `EBADF` stays for an unopened fd |
| `accept4` with an unknown flag | `EINVAL` | accepted | `EINVAL` |
| `accept`/`accept4` `*addrlen` | written | never written | written (16) |
| `accept` `EAGAIN` | re-arms the listener's `EPOLLET` edge | did not | re-arms |
| `listen(fd, 0)` | clamped to 1 | 0 handles, so the listener never accepted | clamped to 1; a negative backlog means the maximum |
| `accept` with no process after dequeue | (n/a) | leaked the connection | the connection is removed |

Each fix went into glue, so the AArch64 kernel gets it too.

Glue was the stronger copy in one respect: `setsockopt` honours `SO_RCVTIMEO`/`SO_SNDTIMEO`, which the
amd64 copy accepted and dropped.

A live probe found three more gaps, all on both kernels. It runs 25 AF_INET checks over loopback,
pushed in over ssh to an amd64 guest booted with herd under TCG (`SMP=2`).

| | was | now |
|---|---|---|
| AF_INET `sendmsg` | sent `iovs[0]` only | coalesces every iovec (`gather_iovecs`); the amd64 copy did, up to 8 |
| AF_INET `recvmsg` | filled `iovs[0]` only | scatters across every iovec (`scatter_iovecs`) |
| `msg_iovlen` | unbounded, into an infallible `vec!` (up to 64 GiB on its `u32`) | over `UIO_MAXIOV` (1024) is `EMSGSIZE`, as Linux |
| UDP blocking `recv` | ignored `SO_RCVTIMEO`; a fixed 10 s, then `ETIMEDOUT` | honours it, and its expiry is `EAGAIN` (TCP send/recv too, which honoured the timeout but said `ETIMEDOUT`) |

The 10 s cap stays when no `SO_RCVTIMEO` is set. That is a pinned divergence: Linux blocks forever.

Pinned, not fixed:
- **An `accept` of a `127.0.0.1` connect reports the peer as `10.0.2.15`**, the interface address.
  It is the stack's source-address choice for loopback, which both deleted copies passed through
  unchanged.
- **An unanswered `busybox nslookup` wedges the amd64 kernel** under TCG at `SMP=2`. The serial
  console stops, the vCPU sits at 99 % and ssh never answers again. The pre-change kernel (the
  previous code commit's ELF, same disk, same boot) wedges identically, so it is not this slice. A
  refused `connect`, a 200 ms `SO_RCVTIMEO` wait and 40 s of idle all survive. Not yet run down.
  The suspect is amd64's in-kernel net wait (`net_blocking_relax` halts without switching tasks
  unless a sleeper is due, and ring-0 code is not preempted). Measured on battery, off the home
  network, so the DNS packet had nowhere to go. The ryzen and trashcan lanes were not run.

Gates:
- clippy on the seven amd64 sets, the AArch64 default / `no-tests` / `rump` / rump-only sets, and
  Taoxichuan on both architectures;
- amd64 TCG 836/0 (`SMP=1`) and 846/0 (`SMP=2`), default and `headless`, against a 829/839 baseline;
  the difference is the new checks;
- AArch64 HVF 315 (`SMP=1`) and 321 (`SMP=4`) `PASSED`, with the same pre-existing `[threads] SPAWN
  FAILED` line the earlier runs on this branch print;
- `akuma-net` host tests;
- the live probe 24/25, the one being the pinned peer address;
- ten fresh ssh sessions;
- repeated refused connects with a liveness check after each.

`sock.rs` had also been answering every non-socket fd `ENOTSOCK`. The one boot check that depended on
it, `dispatch: accept4 (288) reaches the socket layer`, passes an unopened fd 0 and now expects `EBADF`.

### Slice 2: futex (2026-11-11)

x86_64 202 now goes `to_glue`, and `amd64/src/futex.rs` (450 lines) is deleted. Its callers were
rewired to glue:

| caller | now |
|---|---|
| `thread::teardown`'s `clear_child_tid` wake | `futex_wake` |
| the slot-purge callback and `teardown`'s purge | `futex_purge_tid` |
| `akuma-exec`'s `futex_wake` hook | `futex_wake` itself |
| the `DUMP_FUTEX_WAITERS` diagnostic | `futex_dump` |

`amd64/Cargo.toml` drops `akuma-syscalls-sync`.

**Why it could fold now.** The Cargo note said glue's `sync` could not build for amd64 and parked
on a scheduler the targets did not share. Both stopped being true with B3 and the `akuma-threading`
hook. Glue's waiter id is `akuma_threading::current_thread_id()`, which on amd64 is the arch hook
reading the per-CPU task slot, so purge and wake name the same thing the old table did. Its park
(`schedule_blocking`) and wake (`wake_by_handle`) are the ones glue's pipe waits already use on this
target. The two copies' wait loops differed in what ends a wait, and that is the one change made to
glue:

| input | amd64's loop | glue's loop | now |
|---|---|---|---|
| pending signal | masked by the thread's signal mask | **unmasked** (`peek_pending_signal`) | masked: a blocked signal ended every wait with `EINTR` and musl's retry spun, on AArch64 too |
| deferred kill | via `should_interrupt_blocking_syscall` | `has_pending_kill` | kept; amd64's `drain` posts it with `request_thread_kill` |
| interrupt flag | via `should_interrupt_blocking_syscall` (the leader's route out, the `-j4` fix) | absent | `is_current_interrupted()` added |
| group exit flag | `thread::should_leave_now()` | absent | not needed: `drain` posts the deferred kill to the same threads |

amd64's key namespace pinned `shared_file_mapping = false`; glue's asks the memory map. That is a
change for the better, unexercised by the probes.

Gates:
- clippy across the slice-1 matrix;
- amd64 TCG 836 (`SMP=1`) and 846 (`SMP=2`), default and `headless`, unchanged;
- AArch64 HVF 315 (`SMP=1`) and 321 (`SMP=4`), unchanged;
- `scripts/futex_suite.py`'s four probes, built for x86_64 musl and run in an amd64 herd guest under
  TCG (`SMP=2`), on the slice-1 kernel and on this one. All PASS on both: `futexops` 5/5, `futexkey`
  3/3, `futextest` 7/7 phases, `futexkill` worst `exit_group` -> reaped 0 ms over 5 rounds. A
  second full pass also passed;
- `futexops` 3.2 s per run on an idle host (baseline 3.8 s). The first run read 15 s while the TCG
  gate was building on the same host.

**The amd64 freeze is not DNS.** Every amd64 herd guest booted this session eventually stopped:
console silent, vCPU at 100 %, ssh gone. The pre-sockets kernel stopped between 525 and 613 s of
uptime after an unanswered `nslookup`. The slice-1 kernel stopped between about 30 and 54 s, idle,
right after a passing probe run. This slice's kernel stopped between 386 and 417 s, idle after two
probe passes. A slice-1 guest that only did refused connects survived 70 s. The times look random and
follow no workload. All of it ran under TCG on a laptop on battery, so host throttling is not ruled
out. It is open, and it predates both slices. The next step is the same freeze under
`ring0-preempt`, with `GDB=1` attached when it stops to read where the vCPU is.

## Open issue: heap auto-sizing does not scale down for the unified build's smaller footprints (2026-11-11)

Found while boot-gating the config flag sweep (`config-flag-sweep` branch) on amd64 `headless`
under local Firecracker: at `mem_size_mib=512` (511 MiB usable, one RAM region past the 1 MiB
floor), boot failed with

```
[FATAL] heap ends 0x0000000020703000 but the region holding it ends 0x0000000020000000
```

**Root cause** (`crates/akuma-boot-amd64/src/mem.rs::heap_size_for`): the heap-size policy is a
two-step function with no floor that respects actual available RAM —

```rust
pub const HEAP_SIZE: usize = 512 * 1024 * 1024;        // every machine under the threshold
pub const HEAP_SIZE_LARGE: usize = 1024 * 1024 * 1024; // >= LARGE_HEAP_RAM_THRESHOLD
pub const LARGE_HEAP_RAM_THRESHOLD: u64 = 8 * 1024 * 1024 * 1024; // 8 GiB
```

Below 8 GiB, every machine gets a flat 512 MiB heap request regardless of how much RAM it
actually has. At `mem_size_mib=512` the single usable region is itself only 511 MiB, so the flat
request cannot fit *by construction* — before even subtracting the kernel image, PMM bookkeeping
or boot stack. `pick()` falls back to the same oversized `HEAP_SIZE` a second time
(`.or_else(|| pick(HEAP_SIZE))`) rather than to anything RAM-proportional, so there is no smaller
value to fall back to. Booting at `mem_size_mib=2048` (the project's usual default) worked and
passed the full suite (802/0); the bug only shows at a genuinely small machine.

**Why this matters for the unified build specifically**: the drivers-crate split and the
`headless`/`storage-and-input` feature set (2026-11-11, this same day) exist to make a small,
test-box-sized amd64 image — the explicit goal in "docs: stripped guest programs, the wifi and kvm
features, headless sizes" (`2062fc32`). A smaller binary and a smaller feature set do not help if
the heap floor alone already needs 512 MiB+ to boot at all; `KERNEL_HEAP_SIZE_MB` (manual override,
`config.rs`) is a workaround per-boot, not a fix to the default policy. AArch64 is unaudited here —
unknown whether its own heap-sizing path (`akuma-kernel-core`) has the same flat-floor shape; worth
checking as part of the same pass, since convergence is supposed to make the two agree.

**Next step**: make `heap_size_for` (or its AArch64 equivalent, once compared) scale from actual
usable RAM rather than stepping between two fixed sizes — e.g. a fraction of `usable_ram` with a
small absolute floor — and gate it with a boot test at a RAM size below today's 512 MiB floor
(Firecracker `mem_size_mib` in the 128-384 MiB range) alongside the existing 2048 MiB default, on
both `headless` and the full driver set. Not scheduled against a slice yet; filed here because the
unified build's own stated smaller-footprint goal is what exposed it.
**Correction (2026-11-11, later the same day): it was QEMU, not the guest.** The guest kept running
throughout: its clock, scheduler, netpoll daemon and sshd's accept loop were all live. QEMU's main loop
was stuck in its timer list holding the BQL, with both vCPUs blocked on it mid-MMIO store. The trigger
was `akuma_lapic_amd64::start_timer` writing the periodic LVT before the new count, leaving a 32 ns
periodic timer for one store. Unrelated to either slice. Fixed by zeroing the count first:
`docs/archive/AMD64_LAPIC_PERIODIC_SWITCH_QEMU_LIVELOCK.md`.

### Slice 3: signals (2026-11-11)

`kill`, `tkill` and `tgkill` are one copy, glue's `signal::{sys_kill, sys_tkill, sys_tgkill}`, on
every kernel. x86_64 62/200/234 go `to_glue`; the Taoxichuan arms call glue's functions by name;
`crates/akuma-syscalls-glue-amd64/src/kill.rs` (237 lines) and the three amd64 wrappers in
`amd64/src/signal.rs` are deleted, and so is glue's `proc::sys_kill`. The x86 body was the stronger
one and became glue's: every signal, SIGKILL included, is pended on the target group and taken by
each thread at its own return to user mode. AArch64 used to hand SIGKILL to `deliver_signal`, whose
SIGKILL arm marks the victims' slots `TERMINATED` from the killer's core. That arm now serves only
console Ctrl-C (`kill_foreground_group`).

The decision `akuma_syscalls_signal::tkill_action` is now `{Drop, Pend, PendInterrupt}`. The
pinned divergences, each a change for both kernels unless noted:

| case | before | now |
|---|---|---|
| fatal default to the caller's own group (`ExitOwnGroup`) | AArch64 called `sys_exit_group` inside `tkill`; wrong exit on amd64 | pend + interrupt; the caller dies at the return it is about to make |
| blocked fatal signal | amd64 interrupted the target's wait | pend only (a blocked signal never ends a wait) |
| blocked default-ignore signal, or `SIG_IGN` + blocked | glue dropped it | pended, as on Linux (the disposition can change before unblock) |
| SIGKILL to a tid with a stale table entry | depended on the disposition read | always pend + interrupt |
| `kill(pid, SIGKILL)` on AArch64 | hard kill from the caller's core, 2 s grace for a victim stuck in the kernel | pend; a victim that never returns to user mode is no longer forced out |
| amd64 Akuma 302 and `pidfd_send_signal(SIGKILL)` | reached `deliver_signal`'s hard kill, which amd64 cannot survive | pend (`pidfd_send_signal` now `EINVAL`s `sig > 64`) |
| `tkill` of an unknown in-range tid | 0 | still 0 (Linux: `ESRCH`); pinned |
| `kill(-pgid)` on AArch64 | `ESRCH` | works |

**Two defects found by the gates and fixed in the slice:**

1. **Every group SIGKILL cost 2 s on AArch64.** With SIGKILL pended on every thread, each thread
   takes the fatal default and calls `sys_exit_group`; each one's `kill_thread_group` grace-waits for
   its siblings, which are in their own grace waits. killtree read 2006-2016 ms on every round, with
   25 `[ktg] grace expired` lines. Fix: Linux's `SIGNAL_GROUP_EXIT` as
   `SharedSignalTable::claim_group_exit` (the table is shared by exactly the `CLONE_THREAD` group).
   The first caller runs the teardown; a later one marks its own slot `TERMINATED` and parks, as a
   posted deferred kill does at the EL0 boundary. amd64 does not use glue's `sys_exit_group`.
2. **amd64 `kill` could fail with `EINTR` and send nothing.** Glue's dispatch prologue answers `EINTR`,
   without running the call, while the caller's interrupt bit is up. amd64's own `sys_kill` was called
   directly and never saw that gate. Once routed, `sigregs`'s `kill(child, SIGKILL)` raced a SIGUSR1
   from the child landing on the other core, returned `EINTR`, and the child was orphaned: the boot
   suite at `SMP=2` failed `sigregs: teardown leaks nothing` and `wait4: no children -> ECHILD` in 3
   of 5 runs (the pre-slice kernel 0 of 7). Found with a temporary print in `sys_kill`: the failing
   run had no SIGKILL from the parent at all. Fix: the signal senders join the signal-state calls the
   gate exempts. Linux never fails `kill` with `EINTR`.

Gates:

- clippy clean on amd64 default, `no-tests`, `ring0-preempt`, `headless`, `headless,no-tests`,
  `headless,kvm`, `--no-default-features`; AArch64 default, `no-tests`, the rump devbox set;
  Taoxichuan x86_64 and aarch64. Host tests (`akuma-syscalls-signal` 17, glue, glue-amd64), unsafe
  budget and address-param ratchets: ok.
- AArch64 HVF boot suite 315 (`SMP=1`), 323 (`SMP=2`), 321 (`SMP=4`); the one `FAILED` line at 1 and
  4 is `thread_slot_reclaim`'s own `SPAWN FAILED` print and is in the baseline log too.
- amd64 TCG 836 (`SMP=1`), 846 (`SMP=2`, 8 of 8 runs, and `headless` 2 of 2), 846 (`SMP=4`).
- killtree, 8 rounds of modes 1-4, AArch64 HVF `SMP=4`:

  | | baseline (HEAD `63652251`) | this slice |
  |---|---|---|
  | SIGKILL browser, tree | 28-32 ms | 14-15 ms |
  | SIGKILL group | `ESRCH`, per-pid fallback | 11-12 ms |
  | SIGTERM group, 300 ms, SIGKILL | 2012-2015 ms | 312 ms (the 300 ms is the probe's pause) |
  | worst tree | 2015 ms | 312 ms |
  | `[ktg] grace expired` lines | 7 | 0 |

- killtree on amd64 TCG `SMP=2`: PASS, worst 308 ms, SIGKILL rounds 34-110 ms, no grace expiry. Not
  measured on the pre-slice amd64 kernel.
- `abortsig`, `pthread_kill_eintr`, `sigsusprobe`: PASS on both. `sigprobe`: all 12 rungs on amd64.

**Future work** (found by this slice's gates, not fixed in it):

- **amd64 process groups outside a pty session.** `setpgid`, `getpgid` and `getsid` are real only for
  a process whose session a pty controls (`pty::sys_{setpgid,getpgid,getsid}_gated`, since
  `5ceb3bb0`, 2026-10-05). Everywhere else `setpgid` returns 0 and changes nothing, and `getpgid`
  reports 1. That was the stub's behaviour from the first amd64 checkpoint, and it was kept
  deliberately because nothing on the console side had been tested against job control. `setsid`
  is real everywhere. Consequences:
  - `kill(-pgid)` for a group made with `setpgid` in a non-pty process (`ssh host cmd`, a service
    under herd) is `ESRCH`. killtree passes on amd64 only through its per-pid fallback. A minimal
    repro (fork, child `setpgid(0, 0)`, parent `kill(-child, 9)`) read `setpgid=0 pgid=1` and
    `ESRCH` on amd64, and worked on AArch64.
  - **Suspected, not tested:** if every non-pty process reads group 1, `kill(0, sig)` from one of
    them signals every leader whose group reads 1, possibly herd and sshd, not just the caller's
    group. Check in a VM before relying on either answer.
  - Fix: make the three calls real everywhere, as on AArch64, with the console Ctrl-C path
    (`kill_foreground_group`) re-tested, since that is the job-control reader the gate protected.
- **AArch64 `sigprobe` rung 9.** A write to a present read-only page reports `si_code`
  `SEGV_MAPERR` (1) where Linux reports `SEGV_ACCERR` (2); rungs 10-12 are never reached. This is in
  the EL0 fault path, which this slice did not touch, and it was not run on the baseline kernel.
  Next: run it on the baseline, then find where `akuma-exceptions` chooses the code for a
  permission fault.
- **AArch64 SIGKILL of a thread that never returns to user mode.** `kill(pid, 9)` used to force it
  out after a 2 s grace. Now only the teardown's deferred kill does, and only once some thread of the
  group has started `exit_group`. A group whose every thread is stuck in the kernel is not reaped by
  `kill -9`. Linux does not force such a thread out either; a stuck kernel thread is a bug of its
  own.
- **`tkill` of an unknown in-range tid returns 0** (Linux: `ESRCH`). Pinned, not fixed: it was the
  pre-slice behaviour on both kernels.

### Slice 4: fd (2026-11-11)

Three steps, each its own commit:

1. amd64's fd layer becomes `akuma-syscalls-glue-amd64`, the x86 copy Taoxichuan already runs.
2. amd64's devices move behind glue hooks.
3. The Linux-correct refusals move into glue itself, for both kernels.

**Step 1: amd64 runs glue-amd64's fd layer.** `amd64/src/fd.rs` was a second copy of glue-amd64:
`openat`'s four refusals (`O_NOFOLLOW`, `O_CREAT` on a directory, `O_DIRECTORY` on a file,
`O_EXCL`), chunked `read`/`pread` past 64 KiB, the `O_RDONLY` write refusal, the `struct stat`
conversion, the `poll`/`select` timeouts, `dup2`'s `old == new` rule and the `fcntl` seal commands.
Those bodies are gone. Each amd64 arm keeps only what is its own, then calls glue-amd64:

| arm | kept on amd64 | then |
|---|---|---|
| `openat` | fb, wifi, power, kvm, tun, mouse opens; block node `ENODEV`; the `MAX_FDS` ceiling | `openat_resolved` (new: the path is resolved once) |
| `read` | the six device reads | `read` |
| `pread64` | `ESPIPE` on a pipe, socket or console | `pread64` |
| `write` | the device writes, the serial console | `write` |
| `readv`/`writev` | the tun calls | `vectored` (now public), with amd64's own `read`/`write` per entry |
| `fstat` | the device stats, the boot task's unbound console | `fstat` |
| `newfstatat` | `AT_EMPTY_PATH` with an empty path goes to amd64's `fstat` | `newfstatat` |
| `dup2` / `fcntl` / `poll` / `select` | `dup2`'s `MAX_FDS` bound | the same names |

No behaviour change was intended, and none was seen. Gates: clippy (amd64, Taoxichuan x86_64 and
aarch64); glue-amd64 host tests; amd64 TCG 836/846/846 at `SMP=1/2/4`, `headless` 836/846; an ssh
shell smoke covering redirects, `>>`, pipes, `stat` of a file, a directory and `/dev/null`, a 1.1 MB
`dd` copy compared with `cmp`, `set -C` refusing an existing file, `cd`, and `/dev/zero` and
`/dev/urandom` reads.

**Found on the way, pre-existing:** on amd64 an `ssh host cmd` session returns nothing on the
client's stderr. `echo x >&2` and `ls /missing` print nothing, and `2>&1` works. The kernel before
this step does the same. This is an sshd/session item, logged here for the fd follow-up.

**Step 2: amd64's devices are glue's `chardev` hooks.** Step 1 left one reason for an amd64 fd arm to
exist: `/dev/fb0`, `/dev/wifi0`, `/dev/power`, `/dev/kvm0` and `/dev/kvm`, `/dev/net/tun` and
`/dev/input/mouse0` are this kernel's drivers, and each arm asked "is this descriptor one of mine?"
before calling glue. That question is now asked by glue. `akuma_syscalls_glue::chardev` holds one
`CharDevOps` table (`open`, `read`, `write`, `vectored`, `ioctl`, `fstat`, `close`), which a kernel
registers at boot. `amd64/src/chardev.rs` is amd64's table, made of the code that left `fd.rs`,
`usermode.rs` and the dispatcher. Glue calls it from:

| glue arm | hook |
|---|---|
| `fs::openat_path`, after the path is resolved | `open` |
| `fs::sys_read`, `fs::sys_write`, before the pty match | `read`, `write` |
| `readv`/`writev` (amd64's `usermode`, glue-amd64's `dispatch`) | `vectored`: only the tun takes the call whole |
| `term::sys_ioctl`, before the pty lookup | `ioctl` |
| `fs::fstat_fill` | `fstat` |
| `fs::sys_close`, `fs::sys_close_range`, after the entry left the table | `close` |

Every hook answers `Option`, and `None` is glue's own arm: the hooks only see the six `Dev*` variants
(`chardev::is_chardev`), a device returns `None` where amd64 never answered for it, and a kernel that
registers nothing (AArch64, Taoxichuan) gets `None` throughout. amd64's `close` and `close_range` arms
are now plain `to_glue`; `close_range` no longer collects the range into a `Vec` to run the releases
first. `dup3` replacing a device descriptor still does not release it, as before; this is pinned for
step 3 rather than changed here.

Gates: clippy (amd64 default, `headless`, `no-tests`; AArch64; Taoxichuan); glue-amd64 host tests;
the unsafe budget; amd64 TCG 836/846/846 at `SMP=1/2/4`, `headless` 846 at `SMP=2`; AArch64 HVF
315/323/321 at `SMP=1/2/4`, the same as the signals slice. On a live amd64 herd VM a static probe
opened `/dev/power` with `O_CLOEXEC` and checked the results:

- `fstat`: `S_IFCHR`, mode 0600, rdev 10:243, ino 20;
- `F_GETFD`: `FD_CLOEXEC` set;
- `readv` over two 8-byte `iovec`s: 16 bytes;
- `TCGETS`: `ENOTTY`;
- `/dev/fb0` and `/dev/wifi0` on microvm: `ENOENT`;
- `close`, then `fstat`: `EBADF`;
- reopen without `O_CLOEXEC`: the flag clear.

A write of `dim=50` came back in the next read, and the shell smoke passed.

**Pre-existing, unchanged:** `/dev/power` renders a fresh snapshot on every read and never returns
0, so `cat /dev/power` does not end. `powerd` reads once. Path `stat`s of these nodes
(`ls -l /dev/power`) fail with `ENOENT` because the VFS has no entries for them; only the
descriptor answers.

**Step 3: the refusals are glue's, for both kernels.** Steps 1 and 2 left the Linux-correct
refusals where they were: in glue-amd64 (`openat`'s four, the `O_RDONLY` write) and in amd64's own
preambles (`pread64`'s `ESPIPE`, the `/dev` `lseek`). Each had a comment saying it stayed out of glue
because the change would land on AArch64 too, and AArch64 was not verified from this machine. Both
kernels are verified here now (HVF and TCG), and Linux gives an oracle: `userspace/forktest/c_stress/fdrefuse.c`,
22 rows, run as the same static musl binary on Linux 7.0 (Lima `fc`) and on each kernel.

Before the step, against Linux:

| row | Linux | AArch64 | amd64 |
|---|---|---|---|
| `O_CREAT\|O_EXCL` on a present file | `EEXIST` | opened | `EEXIST` |
| `O_DIRECTORY` on a file | `ENOTDIR` | opened | `ENOTDIR` |
| `O_CREAT` on a directory | `EISDIR` | opened | `EISDIR` |
| `O_NOFOLLOW` on a symlink | `ELOOP` | opened | `ELOOP` |
| `write` on `O_RDONLY` | `EBADF` | wrote | `EBADF` |
| `pwrite` on `O_RDONLY` | `EBADF` | wrote | wrote |
| `read`/`pread` on `O_WRONLY` | `EBADF` | read | read |
| `pread`/`pwrite` on a pipe | `ESPIPE` | `EBADF` | `ESPIPE`/`EBADF` |
| `pread`/`pwrite` on a socketpair | `ESPIPE` | `EBADF` | `EBADF` |
| `lseek(/dev/null\|zero, 100)` | 0 | 0 | 100 |
| `lseek(/dev/urandom, 100)` | 0 | `ESPIPE` | 100 |
| `lseek` with `whence` 7 | `EINVAL` | 0 | `EINVAL` |

AArch64 was wrong on the most rows, and `O_EXCL` was the worst of them: exclusive create, the
primitive lock files and `mkstemp` races rely on, silently succeeded on a file that was already
there. amd64's two `lseek` answers came from a preamble whose comment said Linux seeks a `/dev` node
to the offset asked for. Linux does not (`null_lseek` returns 0, `/dev/urandom` is `noop_llseek`);
glue's `0` was right and amd64 had overridden it. amd64's `pread` on a socketpair was `EBADF` despite
its `ESPIPE` preamble, because `socket_index` knows only this kernel's own socket table, not AF_UNIX
pairs.

What moved:

- `akuma_syscalls_linux::flags::open::{may_read, may_write}`: the access-mode rule, with `O_PATH`
  and mode 3 granting neither; host-tested.
- `ELOOP` (40) joins `akuma_primitives::errno`; glue-amd64's private copies of `EEXIST`, `ENOTDIR`,
  `EISDIR` and `ELOOP` are gone.
- glue `fs::open_refusal`: the four, in Linux's order (`EEXIST` before `EISDIR` for
  `O_CREAT|O_EXCL` on a directory, where glue-amd64 had them the other way round). It runs before
  any device or `/proc/self/fd` arm, does no lookup unless one of the three flags is set, and takes
  the VFS BKL drop for the lookups it does make.
- glue `sys_read`/`sys_write`/`sys_pread64`/`sys_pwrite64`: `EBADF` by access mode; the `_` arm of
  the two positioned calls is `ESPIPE`, with `BlockDev` kept `EBADF`.
- glue `sys_lseek`: `EBADF`, then `whence > SEEK_HOLE` `EINVAL`, then `/dev/urandom` joins the 0 arm.
- Removed: glue-amd64's `openat_resolved` (amd64 calls `fs::openat_path` directly) and its `write`
  check; amd64's `pread64` preamble down to the unbound console fds glue cannot see; amd64's
  `lseek` arm; the `pipe_write_id` helper that was left unused.

Still amd64's: the `MAX_FDS` ceiling on `openat`/`dup2`/`dup3` (it is this kernel's lookup bound
too, so it is not only a policy), and the block-node `ENODEV`.

Gates:

- clippy: amd64 default, `headless`, `no-tests`; AArch64; Taoxichuan.
- Host tests for `akuma-syscalls-linux`.
- Boot suites: amd64 TCG 836/846/846, `headless` 846; AArch64 HVF 315/323/321. Both are unchanged
  from step 2.
- `fdrefuse`: identical to Linux on both kernels.
- An ssh shell smoke on both kernels: `>>`, `set -C`, `sed -i`, `cp -r`, `find`, a 1.2 MB
  `dd`/`cp`/`cmp`, `mv`, a symlink, `sort -o`, and a busybox `tar` round trip.

**Not changed, pinned:**

- `dup3` replacing a device descriptor still does not release the device (step 2).
- `pread` on a `TimerFd` answers `EAGAIN` and on an `EpollFd` `EINVAL`, where Linux's `ESPIPE`
  check comes first.
- `pread` on a `BlockDev` is `EBADF`.

### Slice 5: mm (deferred, 2026-11-11)

Not per call. amd64 keeps every mapping, lazy ones included, in `Process::mmap_regions` with a neutral
`akuma_mmap::Prot`, filled by its own `mm::fault_in`. AArch64 splits them: eager regions in
`mmap_regions`, lazy ones in `Process::lazy_regions`, whose entries carry **AArch64 PTE bits**
(`page_flags: u64`) and which `akuma-exceptions`' fault handler reads. Glue's `mem.rs` writes the
second shape, so routing amd64's `mmap` to it would hand its fault path regions it cannot read; even
`brk` depends on which fault path knows the heap. Converging is one region model and one fault path.
That touches AArch64's self-host and CoW fault path, so it gets a design doc first; the threads
slice went ahead of it.

### Slice 6: threads (2026-11-11)

Thread creation was already one copy: both kernels call `akuma_exec::process::clone_thread`. What
differed was the arm around it. `userspace/forktest/c_stress/cloneflags.c` is the oracle: raw
`clone` syscalls, because musl's `clone()` refuses `CLONE_VM` without `CLONE_VFORK` before the kernel
sees it. Before the slice:

| case | Linux 7.0 | AArch64 | amd64 |
|---|---|---|---|
| Go's flag word, no `CLONE_SETTLS` | ran | ran | `EINVAL` |
| `pthread_create`'s flag word | ran | ran | ran |
| `CLONE_VM\|CLONE_THREAD` without `CLONE_SIGHAND` | `EINVAL` | ran | `EINVAL` |
| `CLONE_SIGHAND` without `CLONE_VM` | `EINVAL` | `ENOSYS` | `EINVAL` |

amd64 refused every Go OS thread. It required `CLONE_SETTLS` on the reasoning that a thread without it
"runs on the parent's `%fs`". On Linux it does, and Go's runtime asks for exactly that, then sets
its TLS itself in the child.

Shared code had the matching bug. `clone_thread` installed the `tls` argument unconditionally, so
a thread made without `CLONE_SETTLS` got whatever the register held. It now keeps the parent's TLS
base unless `CLONE_SETTLS` is set.

Now:

- glue's `sys_clone` asks `akuma_syscalls_proc::clone::decode` for the two `EINVAL` combinations,
  the rule the x86 kernels already used.
- glue `proc::clone_thread` is every kernel's thread arm. It drains retired process rows first:
  amd64 did that to stop thread churn from filling the process table, and it is one copy now.
  It maps a thread-slot or table limit to `EAGAIN`, a zero stack to `EINVAL` and memory to
  `ENOMEM`.
- Removed:
  - glue's second, post-ready seed of the signal mask, since `clone_thread` seeds it before ready;
  - amd64's `CLONE_THREAD`, stack and `CLONE_SETTLS` refusals and its errno mapping;
  - `usermode::clone_flags`.
- amd64 keeps one check, a caller with no process slot.

Gates: `cloneflags` matches Linux on both kernels. `thrprobe` (8 × 12 threads) and
`pthread_kill_eintr` pass on both, and `futextest` passes on amd64. Clippy is clean on amd64
(default, `headless`, `no-tests`), AArch64 and Taoxichuan. The boot suites hold: amd64 TCG
836/846/846, `headless` 846; AArch64 HVF 315/323/321.

**Found on the way, pre-existing:** on AArch64, `futextest` step 6 (500 thread create/join rounds)
fails with `[asid] EXHAUSTED` in `UserAddressSpace::new_shared`. Repeated runs leave the box unable
to spawn a process, and ssh stops answering. The kernel from before any slice (`mrg`, 16:43) behaves
the same, and the drain at clone does not cure it, so ASIDs are leaked, not slow to come back.
Next item.
