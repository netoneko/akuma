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
