fn main() {
    // Every `kernel_*` cfg this crate's source reads must be forwarded, or it
    // silently compiles the other arm. `src/syscall/` read six of them.
    // Exactly the cfgs this crate's source reads.
    //
    // **Grep for both spellings.** A first pass used
    // `grep -rohE 'cfg\(kernel_[a-z_]+'` and found three, missing the four
    // `no-bkl` gates because they are written `cfg!(all(kernel_smp_shared,
    // kernel_no_bkl_vfs))` — the macro form, not the attribute. The build then
    // failed with 199 `unexpected cfg condition name` errors. Use
    // `grep -rohE 'kernel_[a-z_]+'` and filter by hand.
    for c in [
        "kernel_smp_shared",
        "kernel_tests",
        "kernel_profile_extreme",
        "kernel_no_bkl_network",
        "kernel_no_bkl_vfs",
        "kernel_no_bkl_mm",
        "kernel_no_bkl_drivers",
    ] {
        println!("cargo::rustc-check-cfg=cfg({c})");
    }

    // The BKL phase toggles, forwarded from the bin's `no-bkl-*` features. Each
    // one is read as `cfg!(all(kernel_smp_shared, kernel_no_bkl_X))`, so a
    // missing forward silently reports the carve-out as absent.
    for (feat, cfg) in [
        ("CARGO_FEATURE_NO_BKL_NETWORK", "kernel_no_bkl_network"),
        ("CARGO_FEATURE_NO_BKL_VFS", "kernel_no_bkl_vfs"),
        ("CARGO_FEATURE_NO_BKL_MM", "kernel_no_bkl_mm"),
        ("CARGO_FEATURE_NO_BKL_DRIVERS", "kernel_no_bkl_drivers"),
    ] {
        println!("cargo:rerun-if-env-changed={feat}");
        if std::env::var(feat).is_ok() {
            println!("cargo:rustc-cfg={cfg}");
        }
    }
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SMP_SHARED");
    if std::env::var("CARGO_FEATURE_SMP_SHARED").is_ok() {
        println!("cargo:rustc-cfg=kernel_smp_shared");
    }
    // `no-tests` is an opt-*out*, so its absence is what enables the suite.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_NO_TESTS");
    if std::env::var("CARGO_FEATURE_NO_TESTS").is_err() {
        println!("cargo:rustc-cfg=kernel_tests");
    }
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EXTREME");
    if std::env::var("OPT_LEVEL").as_deref() == Ok("z")
        && std::env::var("CARGO_FEATURE_EXTREME").is_ok()
    {
        println!("cargo:rustc-cfg=kernel_profile_extreme");
    }

    // ---- build identity -------------------------------------------------
    //
    // `version.rs` and `proc.rs` (uname) read these. They used to come from the
    // **binary's** build.rs, but `cargo:rustc-env` does not propagate across
    // crates, so they stopped resolving the moment this became a crate.
    //
    // The derivation moved down here rather than the binary computing a value and
    // handing it back through a hook: that would have been the binary deriving
    // something purely so a crate could read it, and nothing else in the tree
    // reads either variable. There is still exactly one `git rev-parse` in the
    // build — the binary's emission was deleted in the same change.
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

    // Re-run when HEAD moves, so the SHA cannot go stale behind a cache hit.
    // Paths are relative to THIS crate's manifest dir, not the workspace root —
    // `.git/HEAD` here would name `crates/akuma-syscalls-glue/.git/HEAD`, which
    // does not exist, and the staleness would be silent.
    let git_dir = std::path::Path::new("../../.git");
    if git_dir.join("HEAD").exists() {
        println!("cargo:rerun-if-changed=../../.git/HEAD");
        if let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD"))
            && let Some(refname) = head.strip_prefix("ref: ").map(str::trim)
            && git_dir.join(refname).exists()
        {
            println!("cargo:rerun-if-changed=../../.git/{refname}");
        }
    }

    // Mirrors the binary's old three-way choice. The inputs are the same two
    // features this file already keys off above.
    let smp_shared = std::env::var("CARGO_FEATURE_SMP_SHARED").is_ok();
    let extreme = std::env::var("OPT_LEVEL").as_deref() == Ok("z")
        && std::env::var("CARGO_FEATURE_EXTREME").is_ok();
    let build_profile = if extreme {
        "extreme-size"
    } else if smp_shared {
        "release-smp-shared"
    } else {
        "release"
    };
    println!("cargo:rustc-env=AKUMA_BUILD_PROFILE={build_profile}");
}
