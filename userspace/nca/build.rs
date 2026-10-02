use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// True when `tool` resolves on `PATH` (a cross toolchain on the Mac, absent on a native box).
fn on_path(tool: &str) -> bool {
    env::var_os("PATH")
        .map(|p| env::split_paths(&p).any(|d| d.join(tool).is_file()))
        .unwrap_or(false)
}

fn main() {
    // The arch nca is built FOR is the arch this wrapper is built for: aarch64 from
    // userspace/build.sh, x86_64 when cargo runs natively on the amd64 box.
    // NCA_ARCH overrides it.
    let arch = env::var("NCA_ARCH")
        .or_else(|_| env::var("CARGO_CFG_TARGET_ARCH"))
        .unwrap_or_else(|_| "aarch64".to_string());
    if arch != "aarch64" && arch != "x86_64" {
        panic!("nca: unsupported target arch {arch} (want aarch64 or x86_64)");
    }
    let triple = format!("{arch}-unknown-linux-musl");
    let triple_env = triple.replace('-', "_");
    let triple_env_upper = triple_env.to_uppercase();
    let prefix = format!("{arch}-linux-musl");
    // A cross toolchain (the Mac) if one is installed. On a native musl box there is none, and
    // the compiler/linker is whatever that machine's cargo config / `cc` says — the amd64 box
    // links with the toolchain's own ld.lld from $CARGO_HOME/config.toml, which a hardcoded
    // `gcc` here would silently override.
    let cross = |name: &str| {
        let prefixed = format!("{prefix}-{name}");
        on_path(&prefixed).then_some(prefixed)
    };

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let src_dir = manifest_dir.join("native-cli-ai");
    let target_bin_dir = manifest_dir.join("../../bootstrap/bin");

    if !src_dir.exists() {
        panic!(
            "native-cli-ai source not found at {}. Did you forget to initialize submodules?",
            src_dir.display()
        );
    }

    fs::create_dir_all(&target_bin_dir).expect("Failed to create bootstrap/bin");

    let num_jobs = std::thread::available_parallelism()
        .map(|n| n.get().to_string())
        .unwrap_or_else(|_| "4".to_string());

    // Override the upstream release profile with Akuma-tuned flags:
    //   - opt-level=3: speed over size (nca is I/O-bound but inference heavy)
    //   - lto=fat: full cross-crate inlining — the upstream uses "thin"
    //   - neon+fp16+dotprod: all SIMD extensions available on qemu-virt AArch64
    //   - static: link against musl statically, no dynamic loader on Akuma
    let mut flags = vec![
        "-C opt-level=3",
        "-C lto=fat",
        "-C codegen-units=1",
        "-C panic=abort",
        "-C overflow-checks=off",
    ];
    if arch == "aarch64" {
        flags.push("-C target-feature=+neon,+fp16,+dotprod");
    }
    flags.push("-C link-arg=-static");
    let rustflags = flags.join(" ");

    println!("cargo:warning=Building nca (native-cli-ai dev) for {triple}...");

    let mut cmd = Command::new("cargo");
    cmd.current_dir(&src_dir)
        .args([
            "build",
            "--release",
            // Disable clipboard (arboard/X11/Wayland): nca runs over SSH on Akuma.
            "--no-default-features",
            "--target",
            &triple,
            "-p",
            "nca-cli",
            "-j",
            &num_jobs,
        ])
        // musl cross-compilation toolchain
        // CARGO_ENCODED_RUSTFLAGS is set by the outer cargo process and takes priority
        // over RUSTFLAGS. Unset it so our RUSTFLAGS are actually used, and so the
        // outer workspace's linker flags (-Tlinker.ld, max-page-size) don't bleed in.
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env("RUSTFLAGS", &rustflags);
    if let Some(gcc) = cross("gcc") {
        cmd.env(format!("CARGO_TARGET_{triple_env_upper}_LINKER"), &gcc)
            .env(format!("CC_{triple_env}"), &gcc);
    }
    if let Some(gxx) = cross("g++") {
        cmd.env(format!("CXX_{triple_env}"), gxx);
    }
    if let Some(ar) = cross("ar") {
        cmd.env(format!("AR_{triple_env}"), ar);
    }
    let status = cmd
        .status()
        .expect("Failed to invoke cargo — is cargo installed?");

    if !status.success() {
        panic!("Failed to build nca");
    }

    let compiled = src_dir.join(format!("target/{triple}/release/nca"));
    if !compiled.exists() {
        panic!("nca binary not found at {}", compiled.display());
    }

    // bootstrap/bin/nca is the AArch64 image's copy; an amd64 build must not clobber it.
    let dest_name = if arch == "aarch64" { "nca".to_string() } else { format!("nca-{arch}") };
    let dest = target_bin_dir.join(&dest_name);
    fs::copy(&compiled, &dest).expect("Failed to copy nca binary");
    let _ = Command::new(cross("strip").unwrap_or_else(|| "strip".to_string())).arg(&dest).status();

    println!("cargo:warning=Installed nca to bootstrap/bin/{dest_name}");

    println!("cargo:rerun-if-env-changed=NCA_ARCH");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=native-cli-ai/Cargo.toml");
    println!("cargo:rerun-if-changed=native-cli-ai/crates/cli/src/main.rs");
    println!("cargo:rerun-if-changed=native-cli-ai/crates/core/src/lib.rs");
}
