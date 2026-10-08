//! Stamp the commit this userspace tree was built from into `libakuma::GIT_REV`,
//! so a binary on a box can say what it is (`herd`'s startup line, `sshd`'s).
//!
//! `AKUMA_GIT_REV` in the environment wins (a build with no `.git`, e.g. the
//! in-VM self-host build, can be handed one); otherwise `git describe`-style
//! `<short sha>[-dirty]`; otherwise `unknown`. Never fails the build.
//!
//! "-dirty" is the tree's state when this script last ran: it reruns when HEAD
//! or the index moves, not on every edit, so treat it as "was dirty at the last
//! commit/stage", not as a live reading.
use std::path::PathBuf;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-env-changed=AKUMA_GIT_REV");
    let rev = std::env::var("AKUMA_GIT_REV").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| {
        if let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]) {
            let dir = PathBuf::from(dir);
            println!("cargo:rerun-if-changed={}", dir.join("HEAD").display());
            println!("cargo:rerun-if-changed={}", dir.join("index").display());
        }
        match git(&["rev-parse", "--short=10", "HEAD"]) {
            Some(sha) => {
                let dirty = git(&["status", "--porcelain", "--untracked-files=no", "--", "."])
                    .map(|s| !s.is_empty())
                    .unwrap_or(false);
                if dirty { format!("{sha}-dirty") } else { sha }
            }
            None => "unknown".to_string(),
        }
    });
    println!("cargo:rustc-env=AKUMA_GIT_REV={rev}");
}
