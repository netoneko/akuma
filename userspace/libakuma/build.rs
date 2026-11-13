//! Stamp the commit this userspace tree was built from into `libakuma::GIT_REV`,
//! so a binary on a box can say what it is (`herd`'s startup line, `sshd`'s).
//!
//! `AKUMA_GIT_REV` in the environment wins (a build with no `.git`, e.g. the
//! in-VM self-host build, can be handed one); otherwise `git describe`-style
//! `<short sha>[-dirty]`; otherwise `unknown`. Never fails the build.
//!
//! "-dirty" is the tree's state when this script last ran: it reruns when HEAD, its
//! branch ref, the reflog or the index moves (`track_git_state`), not on every edit, so
//! treat it as "was dirty at the last commit/stage", not as a live reading.
use std::path::PathBuf;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Ask cargo to rerun this script whenever the commit the tree is on can have moved.
///
/// `HEAD` alone is not enough: on a branch it holds `ref: refs/heads/<name>`, and a
/// commit moves that ref file, not `HEAD`. The index moves on most commits but not on
/// `reset --soft`, `commit --allow-empty` or a fast-forward of a ref another worktree
/// owns, and then the stamp names a day-old commit (the kernel's
/// `crates/akuma-syscalls-glue/build.rs` hit the same thing on 2026-10-02, and in a
/// `git worktree` on 2026-10-17, where `../../.git` is a file). The reflog is appended
/// to by every commit, checkout and reset, so it is the trigger that cannot go missing;
/// `packed-refs` covers `git gc` rewriting the ref; the common dir is where a worktree's
/// refs live while its own `HEAD` and reflog are in `<common>/worktrees/<name>/`.
fn track_git_state() {
    let path = |arg: &str| git(&["rev-parse", "--path-format=absolute", arg]).map(PathBuf::from);
    let Some(head_dir) = path("--absolute-git-dir").or_else(|| path("--git-dir")) else {
        return;
    };
    let common_dir = path("--git-common-dir").unwrap_or_else(|| head_dir.clone());
    let track = |p: PathBuf| {
        if p.exists() {
            println!("cargo:rerun-if-changed={}", p.display());
        }
    };
    track(head_dir.join("logs/HEAD"));
    track(head_dir.join("HEAD"));
    track(head_dir.join("index"));
    track(common_dir.join("packed-refs"));
    if let Ok(head) = std::fs::read_to_string(head_dir.join("HEAD")) {
        if let Some(refname) = head.strip_prefix("ref: ").map(str::trim) {
            track(common_dir.join(refname));
        }
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=AKUMA_GIT_REV");
    let rev = std::env::var("AKUMA_GIT_REV").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| {
        track_git_state();
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
