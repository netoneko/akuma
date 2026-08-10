# Purging the multikernel (`smp`) feature

The `smp` feature — one whole kernel per core, `cfg(kernel_smp)`, branch
`smp-attempt-0` — is being removed. It served its purpose: it proved out
secondary-core bring-up, forwarded syscalls, and cross-core memory/NIC
questions that fed directly into the design of `smp-shared` (the real,
shared-kernel SMP that shipped as the default). See
`docs/archive/MULTIKERNEL.md`, `docs/archive/MULTIKERNEL_NETWORKING_EXPERIMENT.md`,
and `acceptance/archive/12_multikernel_demo.md` for the history.

## Why purge rather than keep both

- **Wrong shape for the goal.** Akuma's stability work targets a cluster of
  full VMs (LLM box + agent box), not a single VM split into per-core
  mini-kernels. `smp-shared` already covers "use more cores in one VM" better
  than multikernel ever did.
- **Cost/benefit is upside-down.** Running one kernel per core buys isolation
  between cores at the price of a forwarded-syscall bounce protocol, per-core
  NIC assignment, and a whole second memory-reclaim story
  (`docs/archive/MULTIKERNEL_MEMORY_PROTOCOL.md`-equivalent debt scheme) —
  for a return that `smp-shared`'s real locks already deliver more simply.
- **It's insecure by construction.** The forwarded-syscall bounce and
  cross-core doorbell wake are a second, less-audited privilege boundary
  running alongside the real one. Keeping two SMP models alive means every
  syscall-surface security review has to reason about both; multikernel's
  marginal value doesn't justify that surface.
- **Dead weight otherwise.** Nothing downstream (`akuma-exec`, `akuma-net`)
  depends on `kernel_smp` — it never spread past the root crate and
  `akuma-smp`, so the cut is clean.

## What gets deleted

Measured by grep/wc against the tree as of 2026-08-10, on branch
`better-sshd-and-networking`.

### Entirely multikernel-dedicated (delete outright)

| Path | Lines |
|---|---|
| `src/smp.rs` (behind `#[cfg(kernel_smp)] mod smp;` in `src/main.rs`) | 4,174 |
| `crates/akuma-smp/src/lib.rs` | 107 |
| `crates/akuma-smp/src/descriptor.rs` | 219 |
| `crates/akuma-smp/src/ring.rs` | 251 |
| `crates/akuma-smp/src/console_ring.rs` | 155 |
| `crates/akuma-smp/src/fwd_bounce.rs` | 245 |
| `crates/akuma-smp/src/state_machine.rs` | 498 |
| `crates/akuma-smp/src/init_program.rs` | 112 |
| `crates/akuma-smp/Cargo.toml` | 8 |
| **Subtotal** | **5,769** |

### `cfg(kernel_smp)` guards inside otherwise-shared files (delete the guarded blocks, keep the file)

| File | occurrences | approx gated lines |
|---|---|---|
| `src/syscall/net.rs` | 13 | ~186 |
| `src/syscall/fs.rs` | 13 | ~145 |
| `src/syscall/proc.rs` | 5 | ~48 |
| `src/pmm.rs` | 1 (`reserve_range`) | ~23 |
| `src/main.rs` | 8 (incl. `mod smp;`) | ~16 |
| `src/vfs/proc.rs` | 2 | ~11 |
| `src/console.rs` | 2 | ~8 |
| `src/irq.rs` | 1 (`register_handler_no_gic`) | ~8 |
| `src/fs.rs` | 1 (`mark_initialized`) | ~4 |
| **Subtotal** | | **~449** |

Not touched: `#[cfg(any(kernel_smp, kernel_smp_shared))]` in `gic.rs` /
`gic_v3.rs` (the SGI cross-core trigger) — that guard stays because
`smp-shared` needs it too; only the `kernel_smp`-only half of any such
condition would need rewriting, not deleting.

### Build plumbing

- `build.rs`: `cargo::rustc-check-cfg=cfg(kernel_smp)` line, the
  `CARGO_FEATURE_SMP` → `cfg(kernel_smp)` block, and the `"release-smp"`
  build-profile branch — **~15-20 lines**. The mutual-exclusion `assert!`
  against `smp-shared` is shared plumbing between the two features and needs
  a small rewrite (drop the `smp` half), not a deletion.
- Root `Cargo.toml`: `smp = ["dep:akuma-smp"]` feature line + the
  `akuma-smp` optional dependency entry.

### Grand total

**≈ 6,230-6,250 lines** of Rust + build config, cleanly separable — one
module, one optional crate, and a few hundred scattered `cfg` guards. No
bleed into `akuma-exec` or `akuma-net`: neither crate ever defined an `smp`
feature of its own.

### Not counted (already archived, left alone)

- `docs/archive/MULTIKERNEL.md` — 1,222 lines
- `docs/archive/MULTIKERNEL_NETWORKING_EXPERIMENT.md` — 470 lines
- `acceptance/archive/12_multikernel_demo.md` — 216 lines

These stay as historical record per the archive convention (linked from new
docs, never rewritten) — they document why multikernel was tried, which is
still useful even after the code is gone.

## Considered and rejected

- **Fault isolation.** The strongest argument *for* keeping it: shared-kernel
  SMP means a bug on one core can corrupt state for all cores (one address
  space, one lock domain), where multikernel's forwarded-syscall boundary was
  a real, if crude, blast-radius wall between cores. Didn't outweigh the
  purge — nothing today calls for per-core isolation, and it's fully
  recoverable from git history if that ever changes, so there's no reason to
  keep ~6.2k lines of dormant code "just in case."
- **Reusing `RemoteFd`/the capability-forwarding plumbing for herd boxes.**
  Checked before deleting it: `FileDescriptor::RemoteFd` was constructed
  *only* on a secondary core whose VFS/Net were `Proxy`'d to the owner
  (`crates/akuma-exec/src/process/types.rs`, pre-removal) — it can't do
  anything without a second, separate kernel instance on another core to be
  the "owner." Herd's box networking already has its own, unrelated proxy
  mechanism (`FileDescriptor::RumpSocket` + sysproxy, for `stack=rump`
  boxes), untouched by this purge. Removed in full rather than left as dead
  code with no path to reuse.
- **Keeping `gic.rs` (GICv2) since `gic_v3.rs` is the default.** Out of
  scope — `gic-v2` is a separate, orthogonal feature for QEMU-TCG/non-Apple-
  Silicon hosts where only GICv2 exists (HVF on Apple Silicon needs GICv3),
  unrelated to the SMP model. Not touched.

## Deferred: herd's core-pinning feature

`sys_core_init`/`CORE_INIT` (syscall 327) is collapsed to a permanent
`ENOSYS` stub rather than removed — herd (`userspace/herd`) still calls it to
try pinning a service to a secondary core via the `core = N` service-config
field, and its own comment already treats `ENOSYS` as the expected result
under shared-kernel SMP (true on every build even before this removal, since
`smp` and `smp-shared` were mutually exclusive). Herd's userspace-side
core-pinning code and config schema were deliberately left alone — a
separate, already-inert binary-level feature with its own docs
(`userspace/herd/docs/CORE_AWARE_SCHEDULING.md`), out of scope for a
kernel-side cleanup. **Needs its own sweep later** if herd's core-pinning
feature itself is ever worth removing.

## Actual results (implemented 2026-08-10)

Landed on branch `better-sshd-and-networking`, commit `ebfb73f` ("remove
multikernel"). Final diff against the pre-removal tree: **37 files changed,
78 insertions(+), 6,695 deletions(-)**
(`git diff --stat` over `src/`, `crates/`, `build.rs`, `Cargo.toml`,
`scripts/cargo_runner.sh`) — within a few percent of the ≈6,230-6,250
estimate above; see `docs/archive/LINE_COUNT_ANALYSIS.md`'s matching
`cloc_akuma.py`-based re-measurement for the production/test-code split.

Beyond the estimate's scope, three more things turned out to be genuinely
dead once the estimate's cuts were made, and were removed too:
- `crates/akuma-exec`'s `prepare_user_address_space`/`remote_fd_close`
  runtime hooks (`ExecRuntime`) and the `FileDescriptor::RemoteFd`/
  `RemoteKind` enum variants — never constructed by anything except the
  deleted `src/smp.rs`.
- `spawn_process_from_image`/`spawn_process_from_image_with_args`
  (`crates/akuma-exec/src/process/spawn.rs`) — the in-memory-ELF spawn path
  used only to launch a pinned process on a secondary core.
- `set_boot_ttbr0_override`/`BOOT_TTBR0_OVERRIDE` (`crates/akuma-exec/src/mmu/mod.rs`)
  and `CORE2_NIC` (`scripts/cargo_runner.sh`, the third-NIC QEMU flag for a
  secondary's local rump stack) — both were plumbing with no caller once the
  multikernel side was gone.

Also removed the "SMP / multikernel" rows/sections from every
**`docs/reference/`** doc — that tree is current-state architecture, not a
history — and deleted `docs/reference/subsystems/smp.md` outright (it was
entirely about the removed feature). `docs/archive/MULTIKERNEL.md`,
`docs/archive/MULTIKERNEL_NETWORKING_EXPERIMENT.md`, and
`acceptance/archive/12_multikernel_demo.md` got a "removed, kept for
historical reference" header instead, per the archive/acceptance-archive
convention (see "Not counted" below — unchanged from the estimate).

Build+boot verified across every live target: `cargo check`/`clippy --release`
(default, `smp-shared`) clean; `extreme-size`, `devbox-smoltcp`, and `devbox`
(rump) all compile clean; `devbox-smoltcp` (SMP=2, shared kernel) and
`extreme-size` both boot to a working `sshd` with no panics on an isolated
disk copy (`devbox.img` was in use by another running VM at the time, so
verification used an APFS-cloned copy — see `docs/archive/DEVBOX_ISSUES.md`
Issues 3 and 4 for two unrelated pre-existing/newly-noticed bugs found along
the way).

## Background

- `docs/archive/MULTIKERNEL.md` — original design + milestone log.
- `docs/archive/MULTIKERNEL_NETWORKING_EXPERIMENT.md` — NIC/DNS experiment.
- `acceptance/archive/12_multikernel_demo.md` — the demo playbook, retired.
- `docs/archive/LINE_COUNT_ANALYSIS.md` — the matching before/after line-count
  re-measurement.
- `docs/archive/DEVBOX_ISSUES.md` — Issue 3 (UART cross-core interleaving,
  noticed while reading `console.rs` here) and Issue 4 (`/proc/cores`
  unreadable, pre-existing, found during boot verification).
