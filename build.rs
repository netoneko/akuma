fn main() {
    println!("cargo::rustc-check-cfg=cfg(kernel_profile_size)");
    println!("cargo::rustc-check-cfg=cfg(kernel_profile_extreme)");
    println!("cargo::rustc-check-cfg=cfg(kernel_smp)");
    println!("cargo::rustc-check-cfg=cfg(kernel_smp_shared)");
    println!("cargo::rustc-check-cfg=cfg(kernel_no_bkl_network)");
    println!("cargo::rustc-check-cfg=cfg(kernel_no_bkl_vfs)");
    println!("cargo::rustc-check-cfg=cfg(kernel_no_bkl_process)");
    println!("cargo::rustc-check-cfg=cfg(kernel_no_bkl_mm)");
    println!("cargo::rustc-check-cfg=cfg(kernel_no_bkl_drivers)");
    println!("cargo::rustc-check-cfg=cfg(kernel_no_bkl_irq)");
    println!("cargo::rustc-check-cfg=cfg(kernel_bkl_profile)");
    println!("cargo::rustc-check-cfg=cfg(kernel_builtin_ssh)");
    println!("cargo::rustc-check-cfg=cfg(kernel_tests)");

    // Multikernel (one-kernel-per-core) gate. ALL secondary-core code lives behind
    // `cfg(kernel_smp)`; with the feature off, none of it compiles and the default
    // build stays byte-for-byte single-core (docs/MULTIKERNEL.md §11). The `smp`
    // Cargo feature is the discriminator (the `release-smp` profile only sets
    // codegen; Cargo profiles cannot auto-enable features), exposed to build
    // scripts as CARGO_FEATURE_SMP. Selected together by scripts/build_smp.sh.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SMP");
    let smp = std::env::var("CARGO_FEATURE_SMP").is_ok();
    if smp {
        println!("cargo:rustc-cfg=kernel_smp");
    }

    // Real (shared-kernel) SMP gate. Distinct from the multikernel: ONE shared
    // kernel — one set of statics, one page-table set, one PMM/heap, one global run
    // queue — across all cores under real cross-core locking. All of it lives behind
    // `cfg(kernel_smp_shared)`, emitted only when the `smp-shared` feature is set
    // (exposed as CARGO_FEATURE_SMP_SHARED). Paired with the `release-smp-shared`
    // profile. See docs/reference/subsystems/smp-shared.md.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SMP_SHARED");
    let smp_shared = std::env::var("CARGO_FEATURE_SMP_SHARED").is_ok();
    if smp_shared {
        println!("cargo:rustc-cfg=kernel_smp_shared");
    }

    // The two SMP models are opposites (share-nothing vs. share-everything) and must
    // never compile together — the shared path assumes globals are NOT replicated,
    // the multikernel assumes they ARE.
    assert!(
        !(smp && smp_shared),
        "features `smp` (multikernel) and `smp-shared` (real SMP) are mutually exclusive"
    );

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
    // (`sys_getrandom`, `sys_read`/`sys_pread64` on `/dev/urandom`, `sys_write` on
    // `/dev/dsp`, and the `sys_fb_*` framebuffer syscalls) drop the BKL for their
    // duration, relying on each driver's own fine-grained Spinlock — `RNG_DEVICE`,
    // `SOUND_DEVICE`, `FB_STATE` — for cross-core mutual exclusion instead. Only
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

    // Boot self-test suite present. Mirrors the `not(any(feature = "no-tests",
    // kernel_profile_size))` condition main.rs already repeats a dozen times, so
    // kernel APIs whose only other caller was the in-kernel shell can say "keep me
    // where either the shell or the tests need me" in one cfg.
    let kernel_tests = std::env::var("CARGO_FEATURE_NO_TESTS").is_err()
        && std::env::var("OPT_LEVEL").as_deref() != Ok("z");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_NO_TESTS");
    if kernel_tests {
        println!("cargo:rustc-cfg=kernel_tests");
    }

    // Built-in (in-kernel) SSH server gate. Two conditions have to hold for the
    // server to be worth compiling: it is built on smoltcp sockets (so it needs
    // the native stack), and `userspace-sshd` must be OFF (with it on the image
    // serves SSH from the userspace /bin/sshd and the in-kernel copy would never
    // be started — `config::ENABLE_USERSPACE_SSHD` only stopped it at *runtime*,
    // leaving the whole SSH-2 implementation resident in the image).
    //
    // `cfg(kernel_builtin_ssh)` is what removes it from the build: `mod ssh`, the
    // `ssh_tests` suite, the shell's interactive SSH entry points and the `[SSH]`
    // stats report all hang off it, so with the cfg absent nothing references the
    // `akuma-ssh` crate and LTO drops it entirely. See
    // docs/archive/TRIM_FAT_SSHD.md § "The in-kernel SSH server is a candidate
    // for removal".
    // Policy: the built-in server survives ONLY in the `extreme` profile, where a
    // 4 MB box can be reachable with nothing on disk but a kernel. Every other
    // profile — default release, size, devbox, devbox-smoltcp — serves SSH from
    // the userspace /bin/sshd and compiles this out, together with everything
    // that exists only to serve it (the in-kernel shell and its command set).
    // `userspace-sshd` still opts extreme out on top of that.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SMOLTCP");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_USERSPACE_SSHD");
    let builtin_ssh = std::env::var("CARGO_FEATURE_SMOLTCP").is_ok()
        && std::env::var("CARGO_FEATURE_EXTREME").is_ok()
        && std::env::var("CARGO_FEATURE_USERSPACE_SSHD").is_err();
    if builtin_ssh {
        println!("cargo:rustc-cfg=kernel_builtin_ssh");
    }

    // OPT_LEVEL is "z" only for profile.size / profile.extreme-size (opt-level = "z").
    // PROFILE is always "release" for inherited profiles, so we can't use that.
    let size_profile = std::env::var("OPT_LEVEL").as_deref() == Ok("z");

    // `extreme-size` and `size` are indistinguishable via OPT_LEVEL (both "z"), so
    // the `extreme` Cargo feature (set only by build_extreme_size.sh) is the
    // discriminator. Cargo exposes it to build scripts as CARGO_FEATURE_EXTREME.
    let extreme_profile = size_profile && std::env::var("CARGO_FEATURE_EXTREME").is_ok();
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EXTREME");
    // linker.ld now derives the boot-stack reservation (STACK_BOTTOM / STACK_TOP /
    // IMAGE_RESERVE) from the actual linked image size, so there is no longer a
    // per-profile IMAGE_SIZE here nor a --defsym=STACK_BOTTOM to inject. Still
    // rerun if the linker script changes so the derivation can't go stale behind a
    // cache hit.
    println!("cargo:rerun-if-changed=linker.ld");

    if size_profile {
        println!("cargo:rustc-cfg=kernel_profile_size");
    }
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
    } else if size_profile {
        "size"
    } else if smp_shared {
        "release-smp-shared"
    } else if smp {
        "release-smp"
    } else {
        "release"
    };
    println!("cargo:rustc-env=AKUMA_BUILD_PROFILE={build_profile}");
}
