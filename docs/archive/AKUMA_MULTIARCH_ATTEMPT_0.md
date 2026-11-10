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
