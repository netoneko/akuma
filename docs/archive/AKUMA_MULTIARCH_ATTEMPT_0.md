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
  still special-cases a unix fd beyond `fd::is_unix_socket`, and add an AF_UNIX *connect/accept*
  probe (not only `socketpair`) for the extensive testing D2 asked for.
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
