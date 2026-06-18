// Intentionally empty. The whole point of this crate is its build.rs, which
// reproduces proc-macro2's "spawn a child rustc to probe the compiler" step.
// If build.rs deadlocks, cargo never gets here.
pub const REPRO: &str = "buildrs-repro";
