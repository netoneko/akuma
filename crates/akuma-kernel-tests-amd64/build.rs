//! Hand the suite the guest programs it embeds.
//!
//! `akuma-kernel-amd64`'s `build.rs` compiles them (`mkdisk.sh` also copies two of them onto
//! the disk) and exports each path as `links` metadata, which reaches this crate, a direct
//! dependent, as `DEP_AKUMA_KERNEL_AMD64_USER_<NAME>_ELF`. Re-emitted as `USER_<NAME>_ELF`,
//! the names `include_bytes!(env!(..))` read.

fn main() {
    // On the host (a `--workspace` build) the kernel is not a dependency and the suite is
    // empty; there is nothing to hand on.
    let target = std::env::var("TARGET").unwrap_or_default();
    if target != "x86_64-unknown-none" {
        return;
    }
    for name in ["hello", "threadprobe", "killprobe", "sigregs"] {
        let key = format!("DEP_AKUMA_KERNEL_AMD64_USER_{}_ELF", name.to_uppercase());
        let path = std::env::var(&key)
            .unwrap_or_else(|_| panic!("akuma-kernel-amd64 exports {key}"));
        println!("cargo:rustc-env=USER_{}_ELF={path}", name.to_uppercase());
        println!("cargo:rerun-if-env-changed={key}");
    }
}
