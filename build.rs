fn main() {
    println!("cargo::rustc-check-cfg=cfg(kernel_profile_extreme)");
    println!("cargo::rustc-check-cfg=cfg(kernel_smp_shared)");
    println!("cargo::rustc-check-cfg=cfg(kernel_no_bkl_network)");
    println!("cargo::rustc-check-cfg=cfg(kernel_no_bkl_vfs)");
    println!("cargo::rustc-check-cfg=cfg(kernel_no_bkl_process)");
    println!("cargo::rustc-check-cfg=cfg(kernel_no_bkl_mm)");
    println!("cargo::rustc-check-cfg=cfg(kernel_no_bkl_drivers)");
    println!("cargo::rustc-check-cfg=cfg(kernel_no_bkl_irq)");
    println!("cargo::rustc-check-cfg=cfg(kernel_bkl_profile)");
    println!("cargo::rustc-check-cfg=cfg(kernel_tests)");
    println!("cargo::rustc-check-cfg=cfg(kernel_console_lock)");
    println!("cargo::rustc-check-cfg=cfg(kernel_audio)");

    // Real (shared-kernel) SMP gate: ONE shared kernel — one set of statics, one
    // page-table set, one PMM/heap, one global run queue — across all cores under
    // real cross-core locking. All of it lives behind `cfg(kernel_smp_shared)`,
    // emitted only when the `smp-shared` feature is set (exposed as
    // CARGO_FEATURE_SMP_SHARED). Paired with the `release-smp-shared` profile. See
    // docs/reference/subsystems/smp-shared.md. The experimental one-kernel-per-core
    // multikernel (`smp`/`kernel_smp`) was removed 2026-08-10 —
    // docs/archive/TRIM_FAT_MULTIKERNEL.md.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SMP_SHARED");
    let smp_shared = std::env::var("CARGO_FEATURE_SMP_SHARED").is_ok();
    if smp_shared {
        println!("cargo:rustc-cfg=kernel_smp_shared");
    }

    // BKL-free network path (Phase 2 of docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md).
    // `cfg(kernel_no_bkl_network)` makes the smoltcp net syscalls drop the BKL for their
    // duration; only meaningful under shared-kernel SMP (nothing to drop otherwise). The
    // gate is emitted independently of `smp_shared` so the net.rs guard can compile-check
    // in either combination — its body is additionally `cfg(kernel_smp_shared)`-gated so
    // it stays a no-op without real SMP.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_NO_BKL_NETWORK");
    let no_bkl_network = std::env::var("CARGO_FEATURE_NO_BKL_NETWORK").is_ok();
    if no_bkl_network {
        println!("cargo:rustc-cfg=kernel_no_bkl_network");
    }

    // BKL-free VFS path (Phase 4 of docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md),
    // mirroring `kernel_no_bkl_network`. `cfg(kernel_no_bkl_vfs)` makes the fs
    // syscalls drop the BKL for their duration; only meaningful under shared-kernel
    // SMP. Emitted independently of `smp_shared` so the VFS guard body (gated on
    // `all(kernel_smp_shared, kernel_no_bkl_vfs)`) can compile-check in either
    // combination — its body is additionally `cfg(kernel_smp_shared)`-gated so it
    // stays a no-op without real SMP, exactly like the net guard.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_NO_BKL_VFS");
    let no_bkl_vfs = std::env::var("CARGO_FEATURE_NO_BKL_VFS").is_ok();
    if no_bkl_vfs {
        println!("cargo:rustc-cfg=kernel_no_bkl_vfs");
    }

    // BKL-free fork page-copy (Phase 3 of docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md),
    // mirroring `kernel_no_bkl_network` / `kernel_no_bkl_vfs`. `cfg(kernel_no_bkl_process)`
    // makes `fork_process` drop the BKL for its CoW share/demote pass, relying on the
    // address space's own `as_lock` (the same lock the CoW fault handler already takes
    // BKL-free) plus `COW_REFCOUNTS`. Only meaningful under shared-kernel SMP, and
    // included in the `smp-shared` feature set since 2026-07-31 (same as net and vfs).
    // Emitted independently of `smp_shared` so the guard body (gated on
    // `all(kernel_smp_shared, kernel_no_bkl_process)`) can compile-check in either
    // combination.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_NO_BKL_PROCESS");
    if std::env::var("CARGO_FEATURE_NO_BKL_PROCESS").is_ok() {
        println!("cargo:rustc-cfg=kernel_no_bkl_process");
    }

    // BKL-free memory-management syscalls (Phase 5 of
    // docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md), mirroring `kernel_no_bkl_vfs`.
    // `cfg(kernel_no_bkl_mm)` makes `sys_mprotect`/`sys_madvise`/`sys_munmap`/
    // `sys_mremap`/`sys_mmap` drop the BKL for their duration, relying on
    // `Process::as_lock` (page tables), `Process::vm_lock` (mmap_regions AND the
    // mmap free-list — extended to cover the latter as part of this carve, see
    // `Process::vm_alloc_mmap`/`vm_free_mmap`), `LAZY_REGION_TABLE`, PMM/
    // FRAME_TRACKER, and `SHARED_FILE_MAPPINGS` — all already independent of the
    // BKL. Only meaningful under shared-kernel SMP. Emitted independently of
    // `smp_shared` so the guard body (gated on `all(kernel_smp_shared,
    // kernel_no_bkl_mm)`) can compile-check in either combination.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_NO_BKL_MM");
    if std::env::var("CARGO_FEATURE_NO_BKL_MM").is_ok() {
        println!("cargo:rustc-cfg=kernel_no_bkl_mm");
    }

    // BKL-free device-driver syscalls (Phase 6 of
    // docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md), mirroring `kernel_no_bkl_mm`.
    // `cfg(kernel_no_bkl_drivers)` makes the device-driver syscall paths
    // (`sys_getrandom`, `sys_read`/`sys_pread64` on `/dev/urandom`, and
    // `sys_write` on `/dev/dsp`) drop the BKL for their duration, relying on each
    // driver's own fine-grained Spinlock — `RNG_DEVICE`, `SOUND_DEVICE` — for
    // cross-core mutual exclusion instead. Only
    // meaningful under shared-kernel SMP. Emitted independently of `smp_shared` so
    // the guard body (gated on `all(kernel_smp_shared, kernel_no_bkl_drivers)`)
    // can compile-check in either combination. The block device (`BLOCK_DEVICE`)
    // and network device (`NETWORK`) are already BKL-free via `no-bkl-vfs` and
    // `no-bkl-network` respectively; this phase covers the remaining drivers.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_NO_BKL_DRIVERS");
    if std::env::var("CARGO_FEATURE_NO_BKL_DRIVERS").is_ok() {
        println!("cargo:rustc-cfg=kernel_no_bkl_drivers");
    }

    // BKL-free timer-IRQ dispatch (Phase 7a of
    // docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md §7, docs/archive/BKL_PHASE7_AUDIT.md
    // §2.3/§5). `cfg(kernel_no_bkl_irq)` makes `rust_irq_handler_with_sp` dispatch the
    // timer IRQ (27, the only device IRQ registered) without ever calling
    // `enter_kernel`/`reconcile_for_spsr` — the handler's state (the alarm queue's own
    // Spinlock, per-thread preemption-watchdog atomics, raw GIC MMIO) no longer needs the
    // BKL. Only meaningful under shared-kernel SMP. Emitted independently of `smp_shared`
    // so the guard body (gated on `all(kernel_smp_shared, kernel_no_bkl_irq)`) can
    // compile-check in either combination, exactly like the other `no-bkl-*` gates.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_NO_BKL_IRQ");
    if std::env::var("CARGO_FEATURE_NO_BKL_IRQ").is_ok() {
        println!("cargo:rustc-cfg=kernel_no_bkl_irq");
    }

    // BKL-hold attribution build. Turns on the per-tag profiler in
    // `akuma_exec::sync` for the whole boot and has the async-main loop dump a
    // periodic delta histogram to the serial console. A MEASUREMENT build only:
    // the profiler writes a shared per-core tag line on every kernel entry, which
    // perturbs timing, so it must never be in a shipping feature set.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_BKL_PROFILE");
    if std::env::var("CARGO_FEATURE_BKL_PROFILE").is_ok() {
        println!("cargo:rustc-cfg=kernel_bkl_profile");
    }

    // Console cross-core serialization. Adds a `Spinlock<()>` + owner-core-ID
    // reentrancy guard around `console::emit`'s UART write loop so that under
    // `smp-shared` two cores cannot both be inside `emit()` at once and
    // byte-interleave each other's lines at the shared PL011 data register.
    // Verified safe under `SMP=4` + `cargo build -j4` self-host load on
    // 2026-08-11 (see docs/archive/UART_SMP_INTERLEAVE_FIX.md).
    //
    // Default ON for the `release` profile (anything with OPT_LEVEL != "z").
    // The size/extreme profiles are single-core targets where the lock is pure
    // overhead, so they stay off unless `CONSOLE_LOCK=1` forces it on for an
    // opt-in test. `CONSOLE_LOCK=0` is an explicit opt-out for `release`.
    //
    // `platform-firecracker` used to be excluded here, and that exclusion was a
    // correctness matter rather than an optimization: the lock is acquired inside
    // `with_irqs_disabled`, so a print issued from a section that already runs with
    // preemption disabled — `akuma_net::smoltcp_net::poll`'s `NETWORK` critical
    // section is the one that bit us — spins on a `Spinlock` whose holder may be a
    // thread that cannot be scheduled. On a multi-core guest another core drains
    // it; on a single-vCPU guest nothing can, and the kernel wedges with no output.
    // Firecracker was a single-vCPU-only target, so the interleave the lock
    // prevents could not occur while the deadlock it enables certainly could.
    //
    // Both halves of that argument are runtime facts, and Firecracker is no longer
    // single-vCPU-only (the redistributor base now comes from the FDT — see
    // `platform::install_fdt_device_map`). So the decision moved to run time:
    // the lock is compiled in here, and `console::set_multicore` decides whether
    // to *acquire* it, based on whether a second core has actually come online.
    // Single-vCPU keeps the deadlock-free behaviour, multi-vCPU gets serialized
    // output, and neither depends on a build-time guess about `vcpu_count`.
    //
    // Still off for the size/extreme profiles: those are single-core targets where
    // even the compiled-in atomic load is pure overhead. `CONSOLE_LOCK=1` forces it
    // on there, `CONSOLE_LOCK=0` opts out of `release`. Background on why the lock
    // exists at all: docs/archive/UART_SMP_INTERLEAVE_FIX.md.
    println!("cargo:rerun-if-env-changed=CONSOLE_LOCK");
    let size_opt_for_console = std::env::var("OPT_LEVEL").as_deref() == Ok("z");
    let console_lock_default_on = !size_opt_for_console;
    let console_lock = match std::env::var("CONSOLE_LOCK").as_deref() {
        Ok("0") => false,                       // explicit opt-out
        Ok("1") => true,                        // explicit opt-in (size/extreme)
        _      => console_lock_default_on,      // release default-on
    };
    if console_lock {
        println!("cargo:rustc-cfg=kernel_console_lock");
    }

    // ------------------------------------------------------------------
    // Devices this machine does not have.
    //
    // Firecracker's device tree, dumped from a live microVM and checked in at
    // docs/reference/firecracker/fdt/, lists exactly: cpus, memory, chosen,
    // intc, timer, apb-pclk, psci, rtc@40001000, uart@40002000, three
    // virtio_mmio nodes (net/block/rng), vmgenid and ptp. There is **no sound
    // device**, and Firecracker upstream does not implement one.
    //
    // So on `platform-firecracker` the virtio-sound driver is not merely unused,
    // it is undriveable. (The framebuffer was the other such driver and was
    // worse — `ramfb::init` faulted on the unmapped fw_cfg window; the whole
    // path is gone as of 2026-08-31, docs/archive/FRAMEBUFFER_REMOVED.md.)
    //
    // Expressed as cfgs rather than repeating `all(feature = "...", not(feature =
    // "platform-firecracker"))` at a dozen sites, because that compound is
    // exactly the kind of mirror invariant that rots when one site is missed —
    // the same argument proposals/FIRECRACKER_PORT.md §5.2 makes about the
    // duplicated device tables.
    //
    // Cargo features are additive and cannot be subtracted by a platform, so
    // `sound` stays in the default set and this cfg is what a Firecracker build
    // actually keys off.
    let firecracker = std::env::var("CARGO_FEATURE_PLATFORM_FIRECRACKER").is_ok();

    // `kernel_audio` no longer *selects* an implementation — since 2026-09-01
    // `akuma-virtio` owns that, via its own `platform-firecracker` feature which
    // swaps its real `imp` for its stub. This cfg has two narrower jobs left:
    // whether `kernel_main` probes at all (the real driver walks eight virtio
    // slots to report "not available", which is noise on a machine that has
    // none), and what `test_platform_device_gates` asserts.
    //
    // **It must stay equal to `akuma-virtio`'s gate** — both are
    // `sound && !platform-firecracker`. If they diverge, the boot test above
    // asserts against an implementation that is not the one compiled in.
    if std::env::var("CARGO_FEATURE_SOUND").is_ok() && !firecracker {
        println!("cargo:rustc-cfg=kernel_audio");
    }

    // Boot self-test suite present. Mirrors the `not(any(feature = "no-tests",
    // kernel_profile_extreme))` condition main.rs used to repeat a dozen times, so
    // kernel APIs whose only other caller was the in-kernel shell can say "keep me
    // where either the shell or the tests need me" in one cfg.
    let kernel_tests = std::env::var("CARGO_FEATURE_NO_TESTS").is_err()
        && std::env::var("OPT_LEVEL").as_deref() != Ok("z");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_NO_TESTS");
    if kernel_tests {
        println!("cargo:rustc-cfg=kernel_tests");
    }

    // OPT_LEVEL is "z" only for profile.extreme-size (the one size-optimised
    // profile). PROFILE is always "release" for inherited profiles, so we
    // cannot use that.
    let size_opt = std::env::var("OPT_LEVEL").as_deref() == Ok("z");

    // `extreme-size` and `size` are indistinguishable via OPT_LEVEL (both "z"), so
    // the `extreme` Cargo feature (set only by build_extreme_size.sh) is the
    // discriminator. Cargo exposes it to build scripts as CARGO_FEATURE_EXTREME.
    let extreme_profile = size_opt && std::env::var("CARGO_FEATURE_EXTREME").is_ok();
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EXTREME");
    // linker.ld now derives the boot-stack reservation (STACK_BOTTOM / STACK_TOP /
    // IMAGE_RESERVE) from the actual linked image size, so there is no longer a
    // per-profile IMAGE_SIZE here nor a --defsym=STACK_BOTTOM to inject. Still
    // rerun if the linker script changes so the derivation can't go stale behind a
    // cache hit.
    println!("cargo:rerun-if-changed=linker.ld");

    if extreme_profile {
        println!("cargo:rustc-cfg=kernel_profile_extreme");
    }

    // Boot-stack size, injected into linker.ld as the BOOT_STACK_SIZE symbol.
    // ALWAYS passed (linker.ld has no PROVIDE default — a PROVIDE would override
    // the defsym under LLD, the historical STACK_BOTTOM no-op bug).
    //   release/size: 1 MB — the boot test suite runs deep on thread 0.
    //   extreme:      32 KB — no test suite (no-tests); thread 0's measured stack
    //                 high-water is ~10 KB (docs/EXTREME_STACK_TRIMMING.md), and
    //                 its exception stack is a separate PMM allocation. Reclaims
    //                 ~992 KB to the user-page pool (≈17% of RAM at the 4.5 MB
    //                 floor). config::KERNEL_STACK_SIZE is NOT used for the boot
    //                 stack bounds (main.rs derives them from STACK_TOP/BOTTOM).
    let boot_stack_size: usize = if extreme_profile { 32 * 1024 } else { 1024 * 1024 };
    println!("cargo:rustc-link-arg=--defsym=BOOT_STACK_SIZE={boot_stack_size}");

    // Build identity for `uname -v` (docs/archive/UNAME.md). `sys_uname` used to
    // report a static "Akuma OS" that said nothing about which build was running;
    // it now reports `<git-sha>-<profile>`, e.g. `a1b2c3d-release-smp-shared`, so a
    // running kernel can be tied back to a commit and a build target. `release`
    // (`uname -r`) comes straight from CARGO_PKG_VERSION on the sys_uname side.
    //
    // The SHA is read here rather than in the kernel because the kernel is no_std and
    // cannot shell out. Not built inside a git checkout (source tarball, vendored
    // build) → "unknown"; that must stay non-fatal, the build has to work without git.
    let git_sha = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=AKUMA_GIT_SHA={git_sha}");

    // Rebuild when HEAD moves so the embedded SHA cannot go stale behind a cache hit.
    // Both files are needed: .git/HEAD changes on branch switch, the ref file changes
    // on commit. Emitted only when they exist — `rerun-if-changed` on a missing path
    // makes cargo rebuild unconditionally.
    if std::path::Path::new(".git/HEAD").exists() {
        println!("cargo:rerun-if-changed=.git/HEAD");
        if let Ok(head) = std::fs::read_to_string(".git/HEAD")
            && let Some(refname) = head.strip_prefix("ref: ").map(str::trim)
        {
            let ref_path = format!(".git/{refname}");
            if std::path::Path::new(&ref_path).exists() {
                println!("cargo:rerun-if-changed={ref_path}");
            }
        }
    }

    // Which build target produced this kernel. Cargo's own PROFILE is "release" for
    // every profile inheriting release (see the OPT_LEVEL note above), so the name is
    // reconstructed from the same discriminators used throughout this script.
    let build_profile = if extreme_profile {
        "extreme-size"
    } else if smp_shared {
        "release-smp-shared"
    } else {
        "release"
    };
    println!("cargo:rustc-env=AKUMA_BUILD_PROFILE={build_profile}");

    // Kernel load address, when the target machine is not QEMU virt.
    //
    // Firecracker's loader does `kernel_load = get_kernel_start() + text_offset`
    // (rust-vmm/linux-loader `pe::PE::load`), and Firecracker passes
    // get_kernel_start() = SYSTEM_MEM_START + SYSTEM_MEM_SIZE = 0x8020_0000.
    // boot.rs's Image header declares text_offset = 1 MiB, so the image lands at
    // 0x8030_0000 and the link address has to match exactly.
    //
    // Passed as a linker --defsym so `linker.ld` stays one file; it reads the
    // symbol through DEFINED(). Keep in lockstep with `config::KERNEL_PHYS_BASE`.
    if std::env::var_os("CARGO_FEATURE_PLATFORM_FIRECRACKER").is_some() {
        println!("cargo:rustc-link-arg=--defsym=KERNEL_PHYS_BASE_OVERRIDE=0x80300000");
    }
}
