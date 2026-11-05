# Unsafe audit steps 1-2: the zero-cost removals and the `akuma-cpu` named functions

**Date:** 2026-11-05. **Plan:** [`UNSAFE_KERNEL_AUDIT.md`](UNSAFE_KERNEL_AUDIT.md) steps 1 and 2; precedent
[`AMD64_UNSAFE_REDUCTION_TRAP_MSR.md`](AMD64_UNSAFE_REDUCTION_TRAP_MSR.md). **Written by:** goose-glm (`glm-5.3`, on the
ryzen box under Pop OS) up to its quota, then reviewed, corrected in one place, verified and merged by the supervisor.
Steps 3-7 of the audit (PhysSlice, SVM, AP bring-up, ring3, per-CPU GDT, LAPIC, the PTE walker) are untouched.

## What moved

| step | old site | now |
|---|---|---|
| 1 | `tokio-kernel-amd64` `trap_top_for`; `amd64/src/gdt.rs` `TRAP_STACK` end | no `unsafe`: address arithmetic on a static (`wrapping_add`), no dereference |
| 2 | `amd64/src/smp.rs` `enable_sse`; `tokio-kernel-amd64` `enable_cpu_features` | `akuma_cpu::ctlreg::enable_fpu()` (CR0.EM=0, CR0.MP=1, CR4.OSFXSR, CR4.OSXMMEXCPT) |
| 2 | `smp.rs` `enable_xsave` | `ctlreg::enable_xsave(xcr0)` (CR4.OSXSAVE, then `xsetbv`; the CPUID facts are the caller's, stated in the doc) |
| 2 | `uaccess.rs` `init_smap`; `tokio-kernel-amd64`'s two CR0.WP writes | `ctlreg::enable_smep_smap(smep, smap)` and `ctlreg::set_cr0_wp()` |
| 2 | `sti`/`cli` in `lapic.rs`, `boot.rs`, `clock.rs`, `sched.rs` (selftests) | `interrupts::{on, off}` |
| 2 | `main.rs` and `tokio-kernel-amd64` halt loops | `park::halt() -> !` (`cli; hlt` per iteration, so an NMI wake re-halts) |
| 2 | byte-identical `rdrand` loops in `amd64/src/net.rs` and `tokio-kernel-amd64/src/net.rs` | `rng::rdrand() -> Option<u64>` (ten retries; the CPUID gate stays the caller's) |
| 2 | `multiboot2.rs` PAT write | `msr::set_pat_entry_wc(index)` (one entry becomes WC, the other seven written back as read) |

There is no raw `write_crN` anywhere in the public surface: each writer read-modify-writes exactly its own bits, the closed-set
design `msr` and `akuma-port` already use.

## What was reviewed and changed

Goose also added `msr::set_kernel_gs_base(base)`, a *safe* function behind a canonical check, for the one `IA32_GS_BASE` write in
`smp.rs::install_percpu`. **That was reversed.** `GS_BASE` is the per-CPU base that every per-CPU read dereferences, so a canonical
but wrong value hands safe code a wrong pointer; it is the same class as `TPIDR_EL1`/`TPIDRRO_EL0`, whose writes the crate
deliberately keeps `unsafe` ("writes to either re-point state the kernel *dereferences*"). The canonical check discharges the `#GP`
on the `wrmsr`, not the pointer. The write is `unsafe { msr::write(Msr::GsBase, addr) }` again, at its one call site, with the
reason in the SAFETY comment, and `crates/akuma-cpu` and `CLAUDE.md` say so. Cost: one site.

Judgement calls left as written, because they cannot hand safe code a pointer, and recorded here in case that changes:
`interrupts::on` (safe, as `daif::unmask_irq` was; enabling interrupts before the IDT is loaded is a caller boot-order fact),
`enable_xsave` and `rdrand` (a violated CPUID precondition faults ring 0; it does not corrupt memory), and `set_pat_entry_wc` (a
live mapping selecting the edited entry changes memory type: a coherency hazard).

## Evidence

* `unsafe` sites (`scripts/check_unsafe_budget.py`, production): `amd64/src` 80 -> 69, `akuma-tokio-kernel-amd64` 29 -> 24,
  `akuma-cpu` 65 -> 73 (the named functions). `docs/reference/unsafe-budget.json` carries the new numbers; the ratchet moved down
  by 16 in the kernels for 8 added inside the one crate whose job is to hold them.
* Builds: `akuma-amd64`, `akuma-tokio-kernel-amd64` (with and without `middleware`), `akuma` and
  `akuma-tokio-kernel-aarch64` check clean; clippy with `-D warnings` on `akuma-cpu` and `akuma-amd64`; `akuma-cpu` host tests (7).
* Boot, amd64 under QEMU TCG (no KVM on the Mac), `SMP=4 sh amd64/run.sh`, 200-240 s: 832 `[OK]` lines, 0 `[FAIL`, 4 CPUs
  online and every secondary taking timer interrupts, `smap`/`smep` follow CPUID, `wp: CR0.WP is set`. The boot was cut by
  `timeout` while the netpoll daemon was spawning, not by a failure. Not exercised: the PAT/write-combining path (needs a
  framebuffer).
* `sh shanghai/mw_gate.sh all` (both machines, SMP=1): 186 `^PASS`, 0 FAIL, the same as before the change. This boots the
  tokio-kernel amd64 and aarch64 kernels, the callers of `enable_fpu` and `park::halt`.

## Not done

* KVM boot of the real kernel, and the metal ryzen: the review boots are TCG.
* `akuma-tokio-kernel-core` fails `clippy -D warnings` on `loader.rs:97` (a long first doc paragraph, from the ETXTBSY change). It
  is not part of this change and was left alone.

## Part 2 (supervisor, 2026-11-06): AP bring-up, APIC, CR3 flush, stack bookkeeping, saved-frame reads, forensics

| what | now |
|---|---|
| AP trampoline page, mailbox, boot root, the three `boot.s` symbols (byte-identical copies in both amd64 kernels) | `akuma-boot-amd64::ap` (`ap_boot_root`, `mailbox_write` safe; `install_trampoline` stays `unsafe fn`, the page must be clear); `paging::boot_pd` |
| `IA32_APIC_BASE.EN` open-coded four times; two CR3 self-flushes | `akuma_cpu::msr::enable_apic_global`, `ctlreg::flush_non_global`; other `mov cr3` uses call `akuma_pagetable`'s `load_cr3` |
| `sys_rt_sigreturn` raw `UserCtx` pointer | three short borrows through `with_current_uctx` |
| stack sentinel painting, high-water scans (4 copies), base canary write/check in `akuma-threading` | new `akuma-stack::StackRegion` (range-checked, 5 host tests); `akuma-threading` vouches for a region in one place (`stack_region`) |
| `Context.sp + 240/248` IRQ-frame reads (5 sites) and four `&*CURRENT_TRAP_FRAME` derefs | `irq_frame_word` / `published_trap_frame`, one `unsafe` each |
| `interp_relr_forensics`, `print_page_forensics` (raw physmap reads) | `akuma-exceptions-forensics`, behind `akuma-exceptions`' `forensics` feature (default on; off = empty calls); reads go through `akuma_phys::Frame` |

`FrameHandle` was *not* used for the saved-frame reads: it names the frame the vector just pushed for the running handler, and
the IRQ-frame offsets (240/248) are not the `UserTrapFrame` layout, so the contract does not fit another thread's frame.

Production `unsafe` sites: kernels 93 -> 69 after the sweep + stack/frame/forensics work (`amd64/src` 69 -> 59, `tokio-kernel-amd64`
24 -> 18, `akuma-threading` 69 -> 59, `akuma-exceptions` 29 -> 27); new leaves `akuma-stack` 5, `akuma-exceptions-forensics` 2,
`akuma-boot-amd64` 2 -> 9, `akuma-cpu` 73 -> 75. Whole tree 492 (audit start) -> 472.

Next step (design for glm): `UNSAFE_CONTEXT_TABLE_DESIGN.md`.

Not done on purpose: the SVM run loop, per-CPU GDT, physmap slices, `deliver_pending`, and `akuma-threading`'s context table (~29)
and switch asm, which need a type that encodes the slot-ownership protocol (a design, not a move).

## Background

Brief given to goose: `scripts/kvm/goose/` session `20261105_1` ("Akuma unsafe-kernel reduction"); it ended mid-edit of
`CLAUDE.md`, which was recovered from its working tree.

## Part 3 (goose-glm, 2026-11-06, on the ryzen/akuma box): item 1 of UNSAFE_CONTEXT_TABLE_DESIGN.md — the IRQ frame, single-sourced

| what | now |
|---|---|
| 240/248 in `akuma-threading` (`IRQ_FRAME_ELR`/`IRQ_FRAME_SPSR`, `irq_frame_word`), `akuma-exceptions` (`IRQ_FRAME_SPSR_OFFSET`, hardcoded 240/248) and the `setup_fake_irq_frame` `frame.add(30/31)` writer | `akuma_trap_aarch64::irq_frame` (`ELR`/`SPSR`/`SIZE` — SIZE single-sourced from `akuma_exec_core::thread::IRQ_FRAME_SIZE`) + a `Copy` `IrqFrame` (`unsafe at` -> safe `elr`/`spsr`/`from_el0` -> safe `write_elr`/`write_spsr`); threading's writer and all 4 readers, and exceptions' `irq_eret_poison_check` + both BKL-reconcile SPSR reads + `interrupted_el0`, go through it |

Host tests: `akuma-trap-aarch64` grew 2 (`round_trips_through_a_synthetic_frame` — a `[u64; 104]` frame written and read back through `IrqFrame`; `offsets_are_not_user_trap_frame_offsets` — pins 240=`x30`/248=`sp_el0` in the `UserTrapFrame`, i.e. exactly the trap this type exists to prevent).

Budget (`scripts/check_unsafe_budget.py --update`, production): `akuma-threading` 59 -> 58, `akuma-exceptions` 27 -> 22, `akuma-trap-aarch64` 14 -> 19 (the five live in the leaf crate whose job is to hold them); whole tree 472 -> 471.

Verified on this box (x86_64 musl host, **no aarch64 target installed**): `cargo test` akuma-trap-aarch64 (7), akuma-phys, akuma-threading (30) incl. `ready_transition_publishes_context_writes_without_a_lock` **20/20 consecutive** (the design doc's bare `-- filter` invocation fails standalone on base too — the test needs its suite's ProcessHooks registrar; run the whole suite); clippy `-D warnings` on threading/trap-aarch64/phys (akuma-exceptions-forensics blocked by a **pre-existing** `akuma-exec` musl-host build failure: `allocator_api` partially stabilized vs this toolchain — unrelated to this change); `cargo check` akuma-amd64 + akuma-tokio-kernel-amd64 (x86_64-unknown-none); **KVM guest boot on akuma itself** (`kbuild --features no-tests`, md5-identical kernel via `herd vm-dev`, 4 vcpus): `smp: 4 cpus online`, sshd init up, `[SWITCH NO-BKL]` switches running, 0 EXCEPTION/panic/POISON.

**Not verified here, for the Mac: aarch64 `cargo check`, the `akuma-exceptions` build, `mw_gate.sh`, and every booted-aarch64 behaviour — items 1 and 3.3's SGI/IRQ-frame code is exactly what a unit test cannot exercise.**

**Not started (next session): items 2a/2b (exceptions page copies + EL1 crash dump move — aarch64-only crate, uncompilable on this box) and item 3 (the context table: `Initializing`/`SwitchPair`/`with_current_ctx`/`peek` in a new `src/ctx_table.rs`; 34 `get_context*` sites to classify per §3.1's table).** The design doc's note stands: `crates/akuma-slot-table` already exists with its own budget entry — read it before item 3's "extract a crate is out of scope" reasoning.

## Part 3 review and metal verification (supervisor, 2026-11-06)

* Glm's Part 3 (item 1) did not compile for aarch64: seven `IrqFrame::at` calls (three in `akuma-threading`, four in `akuma-exceptions`) lacked
  their `unsafe` blocks, because its box has no aarch64 target and the code is `cfg`-gated. Fixed in the merge; its "471 sites" was therefore an
  undercount - the real figure after the fix is **478** (`akuma-trap-aarch64` 14 -> 19, the callers keep one `unsafe` each). The aarch64 +
  amd64 gate (`SMP=4 sh shanghai/mw_gate.sh all`, 186 `^PASS`, 0 FAIL) is what boots the IRQ-frame change under HVF.
* **Metal (ryzen, GRUB entry 26, `d60ce6e4`, 8 cores, 300 fork/exec/pipe loops over ssh): no `EXCEPTION`/`panic`/`POISON`/`STACK-OVERFLOW`/`[FAIL`, `smp: 8 cpus online`.**
  This is the first real metal boot of Parts 2-3. The two earlier "booted on metal" notes for Part 2 were wrong: `arm.sh 27` is the *KVM test* entry
  and boots p3's primary kernel, not `.verify`. Confirm a verify boot by the commit hash in `uname -v` (see `overlays/ryzen/README.md`).
* That kernel (md5 `d81d8028cfa83722fdb8b78b49d9bdf6`) was promoted to p3's `/boot/akuma-amd64`; the previous one is `/boot/akuma-amd64.prev-fca1f9ed`.
