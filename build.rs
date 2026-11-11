fn main() {
    println!("cargo::rustc-check-cfg=cfg(kernel_smp_shared)");
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
    // Default ON; `CONSOLE_LOCK=0` is an explicit opt-out.
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
    // Background on why the lock exists at all:
    // docs/archive/UART_SMP_INTERLEAVE_FIX.md.
    println!("cargo:rerun-if-env-changed=CONSOLE_LOCK");
    let console_lock = std::env::var("CONSOLE_LOCK").as_deref() != Ok("0");
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
    // the same argument docs/archive/FIRECRACKER_PORT.md §5.2 makes about the
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

    // Boot self-test suite present unless `no-tests`.
    let kernel_tests = std::env::var("CARGO_FEATURE_NO_TESTS").is_err();
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_NO_TESTS");
    if kernel_tests {
        println!("cargo:rustc-cfg=kernel_tests");
    }

    // linker.ld now derives the boot-stack reservation (STACK_BOTTOM / STACK_TOP /
    // IMAGE_RESERVE) from the actual linked image size, so there is no longer a
    // per-profile IMAGE_SIZE here nor a --defsym=STACK_BOTTOM to inject. Still
    // rerun if the linker script changes so the derivation can't go stale behind a
    // cache hit.
    println!("cargo:rerun-if-changed=linker.ld");

    // Boot-stack size, injected into linker.ld as the BOOT_STACK_SIZE symbol.
    // ALWAYS passed (linker.ld has no PROVIDE default — a PROVIDE would override
    // the defsym under LLD, the historical STACK_BOTTOM no-op bug). 1 MB: the boot
    // test suite runs deep on thread 0. config::KERNEL_STACK_SIZE is NOT used for
    // the boot stack bounds (main.rs derives them from STACK_TOP/BOTTOM).
    let boot_stack_size: usize = 1024 * 1024;
    println!("cargo:rustc-link-arg=--defsym=BOOT_STACK_SIZE={boot_stack_size}");
    // The build identity (`AKUMA_GIT_SHA`, `AKUMA_BUILD_PROFILE`) moved to
    // `crates/akuma-syscalls-glue/build.rs` on 2026-09-01, with `src/syscall/`.
    // Its `version.rs` and `uname` were the only readers in the tree, and
    // `cargo:rustc-env` does not cross crate boundaries — so emitting it here
    // would have meant a second `git rev-parse` producing a value nothing reads.


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
