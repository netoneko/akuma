// Faithful minimal reproduction of proc-macro2's compiler-probing build script.
//
// The first repro (plain `rustc --version` / stdin `--emit=metadata`) did NOT
// deadlock on Akuma. proc-macro2's real probe (do_compile_probe in its build.rs)
// differs in ways that matter:
//   * it passes `--target <TARGET>` where TARGET = aarch64-unknown-none (the
//     bare-metal kernel target, NOT the host) -> pulls that target's sysroot
//     (libcore etc.) via file-backed mmap, a different demand-paging path;
//   * it appends CARGO_ENCODED_RUSTFLAGS (the `-Clink-arg=-T.../linker.ld`);
//   * it compiles a real file that does `extern crate proc_macro;`;
//   * it create_dir's a probe subdir and remove_dir_all's it afterward.
//
// This build.rs replays do_compile_probe step by step with explicit markers so
// the kernel console shows exactly which step walls.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::iter;
use std::path::Path;
use std::process::{Command, Stdio};

fn mark(s: &str) {
    let mut err = std::io::stderr();
    let _ = writeln!(err, "[REPRO] {}", s);
    let _ = err.flush();
}

fn cargo_env_var(key: &str) -> OsString {
    env::var_os(key).unwrap_or_else(|| panic!("env ${} not set", key))
}

const PROBE_SRC: &str = r#"#![cfg_attr(procmacro2_build_probe, feature(proc_macro_span))]
extern crate proc_macro;
use core::ops::Range;
use proc_macro::Span;
pub fn byte_range(this: &Span) -> Range<usize> { this.byte_range() }
"#;

fn do_compile_probe() -> bool {
    let rustc = cargo_env_var("RUSTC");
    let out_dir = cargo_env_var("OUT_DIR");
    let out_subdir = Path::new(&out_dir).join("probe");
    let probefile = Path::new(&out_dir).join("proc_macro_span.rs");

    mark(&format!("writing probe source to {}", probefile.display()));
    {
        let mut f = fs::File::create(&probefile).expect("create probe src");
        f.write_all(PROBE_SRC.as_bytes()).expect("write probe src");
    }

    mark(&format!("create_dir {}", out_subdir.display()));
    let _ = fs::create_dir(&out_subdir);

    // Mirror proc-macro2's RUSTC_WRAPPER chaining (usually empty).
    let rustc_wrapper = env::var_os("RUSTC_WRAPPER").filter(|w| !w.is_empty());
    let rustc_workspace_wrapper =
        env::var_os("RUSTC_WORKSPACE_WRAPPER").filter(|w| !w.is_empty());
    let mut rustc_chain = rustc_wrapper
        .into_iter()
        .chain(rustc_workspace_wrapper)
        .chain(iter::once(rustc));
    let mut cmd = Command::new(rustc_chain.next().unwrap());
    cmd.args(rustc_chain);

    cmd.stderr(Stdio::null())
        .arg("--cfg=procmacro2_build_probe")
        .arg("--edition=2021")
        .arg("--crate-name=proc_macro2")
        .arg("--crate-type=lib")
        .arg("--cap-lints=allow")
        .arg("--emit=dep-info,metadata")
        .arg("--out-dir")
        .arg(&out_subdir)
        .arg(&probefile);

    if let Some(target) = env::var_os("TARGET") {
        mark(&format!("probe target = {}", target.to_string_lossy()));
        cmd.arg("--target").arg(target);
    } else {
        mark("WARNING: TARGET env not set");
    }

    if let Ok(rustflags) = env::var("CARGO_ENCODED_RUSTFLAGS") {
        if !rustflags.is_empty() {
            for arg in rustflags.split('\x1f') {
                cmd.arg(arg);
            }
        }
    }

    mark("spawning probe rustc (cmd.status -> spawn+wait)");
    let success = match cmd.status() {
        Ok(status) => {
            mark(&format!("probe rustc returned, success={}", status.success()));
            status.success()
        }
        Err(e) => {
            mark(&format!("probe rustc spawn err: {}", e));
            false
        }
    };

    mark(&format!("remove_dir_all {}", out_subdir.display()));
    let _ = fs::remove_dir_all(&out_subdir);
    mark("remove_dir_all done");

    success
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    mark(&format!(
        "START rustc={} TARGET={:?}",
        cargo_env_var("RUSTC").to_string_lossy(),
        env::var_os("TARGET")
    ));

    // proc-macro2 runs the probe possibly twice (with/without RUSTC_BOOTSTRAP).
    // Replay that to match the "two orphaned rustc probes" observation.
    let n: usize = env::var("REPRO_PROBES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    for i in 0..n {
        mark(&format!("=== probe iteration {}/{} ===", i + 1, n));
        let ok = do_compile_probe();
        mark(&format!("iteration {} -> success={}", i + 1, ok));
    }
    mark("DONE — build.rs completed without deadlock");
}
