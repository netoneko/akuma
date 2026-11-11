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
`usermode.rs` into a tests crate; (c) M6 done; (d) the dispatcher split by family. M7 is then a
rename/hoist.

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
