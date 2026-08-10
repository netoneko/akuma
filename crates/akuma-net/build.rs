fn main() {
    println!("cargo::rustc-check-cfg=cfg(kernel_profile_extreme)");
    if std::env::var("OPT_LEVEL").as_deref() == Ok("z") {
        println!("cargo:rustc-cfg=kernel_profile_extreme");
    }
}
