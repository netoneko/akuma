//! Taoxichuan's one build step that is not cargo's: hand the kernel its linker
//! script.
//!
//! Deliberately NOT in `.cargo/config.toml` (same reasoning as
//! `akuma-amd64/build.rs`): a `[target.x86_64-unknown-none]` rustflags `-T`
//! there would apply to every crate built for that target, including the
//! per-crate probes that measure how much of the tree is arch-neutral. Those
//! build rlibs — archived, not linked — but a stray link script in their flag
//! set is noise at best. `rustc-link-arg-bins` is scoped to this package's
//! binary.

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-arg-bins=-T{dir}/linker.ld");
    println!("cargo:rerun-if-changed=linker.ld");
    // The boot assembly lives in the dependency crate; its *output* feeds this
    // binary's link, so a change to it must rebuild here too.
    println!("cargo:rerun-if-changed=../crates/akuma-boot-amd64/src/boot.s");
}
