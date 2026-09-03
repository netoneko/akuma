//! Pass the amd64 linker script to the bin only.
//!
//! Deliberately NOT in `.cargo/config.toml`: a `[target.x86_64-unknown-none]`
//! rustflags entry there would apply to every crate built for that target,
//! including the per-crate `cargo build -p <crate> --target x86_64-unknown-none`
//! probes used to measure how much of the tree is arch-neutral. Those build
//! rlibs, which are archived rather than linked, but a bad `-T` still poisons
//! the flag set. `rustc-link-arg-bins` is scoped to this package's binary.
fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-arg-bins=-T{dir}/linker.ld");
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=src/boot.s");
}
