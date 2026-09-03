//! Two jobs: hand the kernel its linker script, and build the guest programs
//! the kernel's ELF loader consumes.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    kernel_link_args(&dir);
    build_user_program(&dir, "hello");
}

/// Pass the amd64 linker script to the bin only.
///
/// Deliberately NOT in `.cargo/config.toml`: a `[target.x86_64-unknown-none]`
/// rustflags entry there would apply to every crate built for that target,
/// including the per-crate `cargo build -p <crate> --target x86_64-unknown-none`
/// probes used to measure how much of the tree is arch-neutral. Those build
/// rlibs, which are archived rather than linked, but a bad `-T` still poisons
/// the flag set. `rustc-link-arg-bins` is scoped to this package's binary.
fn kernel_link_args(dir: &str) {
    println!("cargo:rustc-link-arg-bins=-T{dir}/linker.ld");
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=src/boot.s");
}

/// Compile and link `userspace/amd64/<name>/<name>.rs` into a static ELF64 the
/// kernel embeds with `include_bytes!`.
///
/// The sources live under `userspace/` because that is where this tree keeps
/// programs that run in ring 3, even though these are not members of that cargo
/// workspace and share nothing with its musl binaries yet. See
/// `userspace/amd64/README.md`.
///
/// # Why `rustc` directly and not a cargo package
///
/// A nested `cargo build` inside a build script shares the parent's target
/// directory and its package lock, which deadlocks; working around it means a
/// separate target dir and a second copy of every dependency. This program has
/// no dependencies — it is one file against `core` — so the thing cargo adds is
/// exactly the thing that breaks. `RUSTC` is handed to build scripts by cargo,
/// so the toolchain used is the one building the kernel.
///
/// # Why the flags differ from the kernel's
///
/// `code-model=small` rather than `kernel`: this image is linked at `0x40_0000`,
/// in the low 2 GiB, which is what the small model is for. Inheriting the
/// kernel's `code-model=kernel` (a `.cargo/config.toml` target setting, which
/// does *not* reach a hand-rolled `rustc` invocation) would emit sign-extended
/// references to the top -2 GiB from a program that is nowhere near it.
///
/// `panic=abort` because there is no unwinder and no `eh_personality`.
fn build_user_program(dir: &str, name: &str) {
    let user = format!("{dir}/../userspace/amd64");
    let src = format!("{user}/{name}/{name}.rs");
    let script = format!("{user}/user.ld");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join(format!("{name}.elf"));

    println!("cargo:rerun-if-changed=../userspace/amd64/{name}/{name}.rs");
    println!("cargo:rerun-if-changed=../userspace/amd64/user.ld");

    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let status = Command::new(rustc)
        .args(["--edition", "2024"])
        .args(["--target", "x86_64-unknown-none"])
        .args(["-C", "opt-level=2"])
        .args(["-C", "relocation-model=static"])
        .args(["-C", "code-model=small"])
        .args(["-C", "panic=abort"])
        // `--no-pie` as well as the static relocation model: the model decides
        // what relocations are emitted, the link flag decides what kind of
        // object comes out. Without it the result is ET_DYN, and the loader
        // rejects that — correctly, since nothing here relocates.
        .args(["-C", &format!("link-arg=-T{script}")])
        .args(["-C", "link-arg=--no-pie"])
        .args(["-C", "link-arg=-nostdlib"])
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .status()
        .expect("failed to run rustc for the guest program");

    assert!(
        status.success(),
        "guest program {name} failed to build; if this says the \
         x86_64-unknown-none target is missing, run \
         `rustup target add x86_64-unknown-none`"
    );

    println!("cargo:rustc-env=USER_{}_ELF={}", name.to_uppercase(), out.display());
}
