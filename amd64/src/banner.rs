//! The sign-on banner: Akuma's mark, then the version line.
//!
//! Printed once, right before `run_init` hands the machine to `sshd` — so on the
//! HP box, whose console is a television, the last thing on screen before the
//! login service comes up says what is running. The art is the signature; the
//! version is the fact.

use crate::serial;

/// `uname -r` — the kernel release. Shared with `usermode::UTSNAME` so the
/// banner and `uname(2)` cannot disagree.
pub const RELEASE: &str = "0.1.0-amd64";

/// `uname -v` — the longer description, same source as above.
pub const VERSION_DESC: &str = "Akuma/amd64 (x86_64 bring-up)";

/// Akuma's mark, the 40-column cut — a local copy of `src/akuma_40.txt`, the
/// same art `userspace/sshd` prints on an interactive login (its own
/// `akuma_40.txt`). Kept here rather than reaching across the source tree, the
/// way sshd's copy is.
const ART: &str = include_str!("akuma_40.txt");

/// Print [`ART`] then the version line. `run_init` calls this on both boot
/// paths just before the init program starts.
pub fn print() {
    serial::puts("\n");
    for line in ART.lines() {
        serial::puts(line);
        serial::puts("\n");
    }
    serial::puts("\n  ");
    serial::puts(VERSION_DESC);
    serial::puts("  ");
    serial::puts(RELEASE);
    serial::puts("\n\n");
}
